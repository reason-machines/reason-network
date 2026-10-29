use super::Daemon;
use crate::{proto::Tier, util};
use anyhow::Result;
use std::{sync::Arc, time::Instant};

impl Daemon {
    pub(super) fn attach_antigravity(self: &Arc<Self>, conn: u64) -> Result<usize> {
        let mut g = self.inner.lock().unwrap();
        self.prune(&mut g);
        g.antigravity_conn = Some(conn);
        let keys: Vec<String> = g
            .live
            .iter()
            .filter(|(_, l)| {
                // App transcripts identify conversations agentapi can address. CLI conversations retain hook delivery.
                l.reg.agent == "antigravity"
                    && l.last_hook.is_some()
                    && l.reg.pid.is_none_or(util::pid_alive)
                    && l.transcript
                        .as_ref()
                        .is_some_and(|p| p.to_string_lossy().replace('\\', "/").contains("/.gemini/antigravity/brain/"))
                    && l.retry.is_none_or(|(at, _)| Instant::now() >= at)
            })
            .map(|(key, _)| key.clone())
            .collect();
        for key in &keys {
            let l = g.live.get_mut(key).expect("selected above");
            l.conn = Some(conn);
            l.mcp_push = true;
            l.reg.tier = Tier::Push;
            l.attached = true;
            let address = self.addr(&l.name);
            let waiting = g.pending(&address)?.into_iter().filter(|m| m.wake).collect::<Vec<_>>();
            if !waiting.is_empty() {
                self.deliver(&mut g, key, waiting, true);
            }
        }
        self.presence_changed();
        Ok(keys.len())
    }
}
