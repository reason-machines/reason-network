//! The App launches this sidecar with agentapi on PATH. Mail arrives over the existing daemon event stream.
use crate::{agent, client::Link, proto::*, util};
use anyhow::{Context, Result};
use serde_json::Value;
use std::{process::Stdio, time::Duration};
use tokio::{sync::mpsc, task::JoinSet};

pub async fn run() -> Result<()> {
    agent::command_for("agentapi").context("Antigravity must launch this sidecar with agentapi on PATH")?;
    let (tx, mut events) = mpsc::unbounded_channel();
    let link = Link::new(Some(tx));
    let mut heartbeat = tokio::time::interval(Duration::from_secs(5));
    let mut deliveries = JoinSet::new();
    loop {
        tokio::select! {
            _ = heartbeat.tick() => {
                // Reattaches after daemon restart, discovers newly opened conversations, and retries failed handoffs.
                if link.call::<Value>(Request::AntigravityAttach).await.is_err() {
                    eprintln!("Reason Network: waiting for the local delivery service");
                }
            }
            Some(Event::Deliver { key, ids, text }) = events.recv() => {
                let link = link.clone();
                deliveries.spawn(async move {
                    let Some(id) = key.strip_prefix("antigravity:") else { return };
                    let Some(command) = agent::command_for("agentapi") else {
                        let _ = link.call::<Value>(Request::PushFailed {key,ids}).await;
                        return;
                    };
                    let result = tokio::time::timeout(Duration::from_secs(30),
                        util::no_window(tokio::process::Command::from(command))
                            .args(["send-message", id, &text]).stdin(Stdio::null())
                            .stdout(Stdio::null()).stderr(Stdio::null()).kill_on_drop(true).status()).await;
                    let request = if matches!(result, Ok(Ok(status)) if status.success()) {
                        Request::Delivered { key, ids }
                    } else {
                        eprintln!("Reason Network: Antigravity handoff failed; the same messages remain queued");
                        Request::PushFailed { key, ids }
                    };
                    let _ = link.call::<Value>(request).await;
                });
            }
            Some(_) = deliveries.join_next(), if !deliveries.is_empty() => {}
        }
    }
}
