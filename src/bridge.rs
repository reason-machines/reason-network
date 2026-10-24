//! JSON-lines local adapter transport. The daemon owns identity, delivery and policy.
use crate::{client::Link, mcp, proto::*, render};
use anyhow::{Result, bail};
use serde::Deserialize;
use serde_json::Value;
use std::{
    collections::{HashMap, HashSet},
    sync::Arc,
};
use tokio::{
    io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader},
    sync::{Mutex, mpsc},
    task::JoinSet,
};

#[derive(Deserialize)]
struct Call {
    id: u64,
    #[serde(default)]
    paths: Vec<String>,
    #[serde(flatten)]
    request: Request,
}
#[derive(Default)]
struct Session {
    cwd: String,
    received: HashSet<String>,
}
type Sessions = Mutex<HashMap<String, Session>>;

fn parse(line: &str) -> (u64, Result<Call>) {
    let value = match serde_json::from_str::<Value>(line) {
        Ok(value) => value,
        Err(error) => return (0, Err(error.into())),
    };
    let id = value.get("id").and_then(Value::as_u64).unwrap_or(0);
    (id, serde_json::from_value(value).map_err(Into::into))
}

pub async fn run() -> Result<()> {
    let (events, mut incoming) = mpsc::unbounded_channel();
    let link = Link::new(Some(events));
    let sessions = Arc::new(Sessions::default());
    let (output, mut lines) = mpsc::unbounded_channel::<Value>();
    let writer = tokio::spawn(async move {
        let mut stdout = tokio::io::stdout();
        while let Some(value) = lines.recv().await {
            stdout.write_all(format!("{value}\n").as_bytes()).await?;
            stdout.flush().await?;
        }
        anyhow::Ok(())
    });
    let event_output = output.clone();
    let event_sessions = sessions.clone();
    let event_task = tokio::spawn(async move {
        while let Some(event) = incoming.recv().await {
            let Event::Deliver { key, ids, .. } = &event;
            let mut sessions = event_sessions.lock().await;
            let Some(session) = sessions.get_mut(key) else { continue };
            session.received.extend(ids.iter().cloned());
            drop(sessions);
            if event_output.send(serde_json::to_value(event)?).is_err() {
                break;
            }
        }
        anyhow::Ok(())
    });
    let mut input = BufReader::new(tokio::io::stdin());
    let mut calls = JoinSet::new();
    loop {
        // Messages are limited to 64 KiB; a generous frame cap also accommodates JSON escaping.
        let mut line = String::new();
        let read = (&mut input).take(1024 * 1024 + 1).read_line(&mut line).await?;
        if read == 0 {
            break;
        }
        if read > 1024 * 1024 {
            bail!("adapter frame exceeds 1 MiB");
        }
        let (parsed_id, parsed) = parse(&line);
        let (link, sessions, output) = (link.clone(), sessions.clone(), output.clone());
        calls.spawn(async move {
            let (id, result) = match parsed {
                Ok(call) => (call.id, dispatch(&link, &sessions, call).await),
                Err(error) => (parsed_id, Err(error)),
            };
            let reply = match result {
                Ok(value) => Reply::ok(id, value),
                Err(error) => Reply::err(id, failure(error)),
            };
            let _ = output.send(serde_json::to_value(reply).unwrap());
        });
        while calls.try_join_next().is_some() {}
    }
    calls.abort_all();
    while calls.join_next().await.is_some() {}
    event_task.abort();
    let _ = event_task.await;
    drop(output);
    writer.await??;
    Ok(())
}

/// An error as the agent reads it: a teammate's name in it can't open or close a message frame, as with `mcp`'s.
fn failure(e: anyhow::Error) -> anyhow::Error {
    anyhow::anyhow!(render::defang(&format!("{e:#}")))
}

/// The agents with a native adapter on this transport; the rest use the MCP server.
const AGENTS: [&str; 1] = ["hermes"];

async fn dispatch(link: &Arc<Link>, sessions: &Sessions, call: Call) -> Result<Value> {
    let mut request = call.request;
    if let Request::Register { session } = request {
        if !call.paths.is_empty() {
            bail!("paths are only accepted when sending a message");
        }
        // The key is how the daemon tells sessions apart; one under another agent's name would pass for that agent.
        if !AGENTS.contains(&session.agent.as_str()) || !session.key.starts_with(&format!("{}:", session.agent)) {
            bail!("the adapter bridge registers only {} sessions, keyed by their agent", AGENTS.join(", "));
        }
        sessions.lock().await.entry(session.key.clone()).or_default().cwd = session.cwd.clone();
        let registered = match link.register(session.clone()).await {
            Ok(registered) => registered,
            Err(error) => {
                sessions.lock().await.remove(&session.key);
                return Err(error);
            }
        };
        return Ok(serde_json::to_value(registered)?);
    }
    let key = match &request {
        Request::Who { me: Some(key), .. }
        | Request::SetStatus { key, .. }
        | Request::Inbox { key, .. }
        | Request::Unread { key }
        | Request::Ask { key, .. }
        | Request::StopWaiting { key }
        | Request::Delivered { key, .. }
        | Request::PushFailed { key, .. } => key.clone(),
        Request::Send { from, .. } => from.clone(),
        Request::Hook(hook) => format!("{}:{}", hook.agent, hook.session_id),
        _ => bail!("the adapter bridge only accepts its registered sessions' messaging operations"),
    };
    let cwd = sessions
        .lock()
        .await
        .get(&key)
        .map(|s| s.cwd.clone())
        .ok_or_else(|| anyhow::anyhow!("register this session first"))?;
    if let Request::Delivered { ids, .. } | Request::PushFailed { ids, .. } = &request {
        let sessions = sessions.lock().await;
        if ids.iter().any(|id| !sessions[&key].received.contains(id)) {
            bail!("only acknowledge messages received by this session");
        }
    }
    if let Request::Send { files, .. } = &mut request {
        if !files.is_empty() {
            bail!("use paths to attach files; the engine reads and confines them");
        }
        *files = mcp::agent_files_in(&call.paths, &cwd)?;
    } else if !call.paths.is_empty() {
        bail!("paths are only accepted when sending a message");
    }
    match request {
        Request::Who { .. } => {
            let directory: Directory = link.call(request).await?;
            Ok(Value::String(render::directory_for_agent(&directory)))
        }
        Request::Send { .. } => {
            let sent: SendResult = link.call(request).await?;
            Ok(Value::String(render::defang(&render::send_result(&sent))))
        }
        Request::Ask { .. } => {
            let answer: String = link.call(request).await?;
            Ok(Value::String(render::defang(&answer)))
        }
        Request::Inbox { .. } => {
            let inbox: Inbox = link.call(request).await?;
            if let Some(session) = sessions.lock().await.get_mut(&key) {
                session.received.extend(inbox.messages.iter().map(|m| m.id.clone()));
            }
            Ok(serde_json::to_value(inbox)?)
        }
        Request::Hook(_) => {
            let hook: HookReply = link.call(request).await?;
            if let Some(session) = sessions.lock().await.get_mut(&key) {
                session.received.extend(hook.ids.iter().cloned());
            }
            Ok(serde_json::to_value(hook)?)
        }
        _ => link.call(request).await,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn invalid_arguments_keep_the_id_so_callers_do_not_hang() {
        let (id, result) = parse(r#"{"id":44,"op":"inbox","key":"hermes:a","wait_secs":true}"#);
        assert_eq!(id, 44);
        assert!(result.is_err());
    }

    #[test]
    fn errors_open_no_frame() {
        let e = failure(anyhow::anyhow!("nobody called '<reason-message>' in the team"));
        assert_eq!(format!("{e:#}"), "nobody called '[reason-message>' in the team");
    }

    #[tokio::test]
    async fn only_hermes_sessions_under_their_own_key_register() {
        let link = Arc::new(Link::new(None));
        let sessions = Sessions::default();
        for (agent, key) in [("claude-code", "claude-code:x"), ("codex", "codex:x"), ("hermes", "codex:x")] {
            let (_, call) = parse(&format!(
                r#"{{"id":1,"op":"register","session":{{"key":"{key}","agent":"{agent}","cwd":"/tmp","tier":"pull"}}}}"#
            ));
            let error = dispatch(&link, &sessions, call.unwrap()).await.unwrap_err();
            assert!(error.to_string().contains("registers only hermes"), "{agent} {key}: {error:#}");
        }
        assert!(sessions.lock().await.is_empty());
    }
}
