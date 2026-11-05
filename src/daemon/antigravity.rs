use super::Daemon;
use crate::{
    git::GitInfo,
    proto::{SessionReg, Tier},
    util,
};
use anyhow::Result;
use std::{sync::Arc, time::Instant};

impl Daemon {
    pub(super) fn attach_antigravity(self: &Arc<Self>, conn: u64) -> Result<usize> {
        // Native hooks mark canonical session rows as App conversations. Restore those bindings after restarts
        // without asking an idle agent to run again; arbitrary MCP registrations cannot create an App binding.
        let bindings = {
            let mut g = self.inner.lock().unwrap();
            self.prune(&mut g);
            let candidates = g.store.native_app_sessions(util::now_ms() - super::sessions::FORGET_MS)?;
            let mut allowed = Vec::new();
            for candidate in candidates {
                if g.store
                    .native_app_roots(&candidate.0.key)?
                    .is_some_and(|roots| roots.iter().all(|root| !self.cfg.off_in(root)))
                {
                    allowed.push(candidate);
                }
            }
            allowed
        };
        for (known, hook_at) in &bindings {
            if self.inner.lock().unwrap().live.contains_key(&known.key) {
                continue;
            }
            let Some(id) = known.key.strip_prefix("antigravity:") else { continue };
            let reg = SessionReg {
                key: known.key.clone(),
                agent: "antigravity".into(),
                cwd: known.cwd.clone(),
                agent_session_id: Some(id.into()),
                pid: None,
                tier: Tier::Push,
                name: Some(known.name.clone()),
            };
            let git = GitInfo { root: None, repo: known.repo.clone(), branch: known.branch.clone() };
            if self.register(Some(conn), reg, git).is_err() {
                continue;
            }
            if let Some(l) = self.inner.lock().unwrap().live.get_mut(&known.key) {
                l.last_hook = Some(*hook_at);
            }
        }
        let mut g = self.inner.lock().unwrap();
        self.prune(&mut g);
        g.antigravity_conn = Some(conn);
        let keys: Vec<String> = g
            .live
            .iter()
            .filter(|(_, l)| {
                // CLI conversations have no verified App binding and retain hook delivery.
                l.reg.agent == "antigravity"
                    && l.last_hook.is_some()
                    && bindings.iter().any(|(known, _)| known.key == l.reg.key)
                    && l.retry.is_none_or(|(at, _)| Instant::now() >= at)
            })
            .map(|(key, _)| key.clone())
            .collect();
        let mut changed = false;
        for key in &keys {
            let l = g.live.get_mut(key).expect("selected above");
            changed |= l.conn != Some(conn) || !l.mcp_push || l.reg.tier != Tier::Push || !l.attached;
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
        if changed {
            self.presence_changed();
        }
        Ok(keys.len())
    }
}
