use super::Daemon;
use crate::util;
use anyhow::Result;
use serde_json::{Value, json};

impl Daemon {
    pub(super) fn diagnostics(&self) -> Result<Value> {
        let mut g = self.inner.lock().unwrap();
        self.prune(&mut g);
        let now = util::now_ms();
        let mut sessions = Vec::new();
        for l in g.live.values().take(64) {
            let (unread, oldest) = g.store.queue_health(&self.addr(&l.name))?;
            sessions.push(json!({
                "agent": crate::usage::family(&l.reg.agent), "tier": l.reg.tier,
                "status": l.status.as_ref().map_or("live", |(s, _)| s.as_str()),
                "hookAgeMs": l.last_hook.map(|at| now.saturating_sub(at).max(0)),
                "sessionLinked": l.reg.agent_session_id.as_ref().is_some_and(|id| l.reg.key == format!("{}:{id}", l.reg.agent)),
                "adapterConnected": l.mcp_push && l.conn.is_some_and(|id| g.conns.contains_key(&id)),
                "unread": unread, "queueAgeMs": oldest.map(|at| now.saturating_sub(at).max(0)),
            }));
        }
        // These snapshots leave the machine: no names, paths, conversation ids, or message contents.
        Ok(json!({"observedAt": now, "sessions": sessions, "truncated": g.live.len() > 64}))
    }
}
