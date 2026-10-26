//! Trusted, same-person custody of mail for permanent mailboxes on offline computers.

use super::{Daemon, Inner, log, mail};
use crate::{
    net::{MailboxRoute, Net, Presence, Remote, Wire},
    proto::Message,
    store::ANCHOR_TTL_MS,
    util,
};
use anyhow::{Result, anyhow, bail};
use iroh::EndpointId;
use std::{collections::HashMap, sync::Arc};

pub(crate) const FEATURE: &str = "anchor-mail";
/// Not a session slug: only this machine's terminal can read its custody notices, even after a rename.
pub(super) const NOTICE_INBOX: &str = "@anchor";

#[cfg(test)]
mod tests;

impl Daemon {
    pub fn anchor_local(&self) -> bool {
        self.cfg.anchor
    }

    pub fn same_owner(&self, peer: &EndpointId) -> bool {
        self.inner
            .lock()
            .unwrap()
            .team
            .as_ref()
            .is_some_and(|t| t.member(peer).is_some_and(|m| m.active && m.person == self.person))
    }

    pub(super) fn saved_mailboxes(&self, g: &Inner) -> Vec<MailboxRoute> {
        if !self.cfg.anchor {
            return Vec::new();
        }
        g.store
            .prefixed("anchor-route:")
            .unwrap_or_default()
            .into_iter()
            .filter_map(|(_, s)| serde_json::from_str::<MailboxRoute>(&s).ok())
            .filter(|r| {
                g.team
                    .as_ref()
                    .is_some_and(|t| t.member(&r.machine).is_some_and(|m| m.active && m.person == self.person))
            })
            .collect()
    }

    /// Only direct announcements can replace saved routes; advertisements from another anchor are never re-exported.
    pub(super) fn learn_mailboxes(&self, peer: &EndpointId, p: &Presence) -> Result<()> {
        if !self.cfg.anchor || !self.same_owner(peer) {
            return Ok(());
        }
        let g = self.inner.lock().unwrap();
        let names = if p.has(FEATURE) { p.mailboxes.clone() } else { Vec::new() };
        let key = format!("anchor-route:{peer}");
        let route = if names.is_empty() {
            None
        } else {
            Some(serde_json::to_string(&MailboxRoute { machine: *peer, names: names.clone() })?)
        };
        let mut drops = Vec::new();
        for held in g.store.anchor_pending()?.into_iter().filter(|h| h.target == peer.to_string()) {
            if !held.msg.to.split_once('/').is_some_and(|(_, n)| names.iter().any(|known| known == n)) {
                let notice = self
                    .anchor_notice(&held.msg, "the destination mailbox was removed or no longer supports the anchor");
                drops.push((held.msg.id, notice));
            }
        }
        g.store.anchor_update_route(&key, route.as_deref(), &drops)?;
        // The next presence contains the changed routes, even when no local session changed.
        drop(g);
        if let Some(net) = self.net() {
            net.presence_changed();
        }
        Ok(())
    }

    pub(super) fn learned_routes(&self, g: &Inner, remotes: &HashMap<EndpointId, Remote>) -> Vec<MailboxRoute> {
        let mut routes = self.saved_mailboxes(g);
        routes.retain(|route| !remotes.get(&route.machine).is_some_and(|r| r.online));
        for (id, r) in remotes {
            if r.online
                && r.presence.has("anchor")
                && g.team.as_ref().is_some_and(|t| t.member(id).is_some_and(|m| m.active && m.person == self.person))
            {
                routes.extend(r.presence.anchor_routes.iter().filter(|route| {
                    g.team.as_ref().is_some_and(|t| t.member(&route.machine).is_some_and(|m| m.active && m.person == self.person))
                    // Fresh direct presence, including an empty mailbox list, wins over a saved route.
                    && !remotes.get(&route.machine).is_some_and(|r| r.online)
                }).cloned());
            }
        }
        routes
    }

    pub fn anchor_peer(&self, remotes: &HashMap<EndpointId, Remote>, target: &EndpointId) -> Option<EndpointId> {
        if !self.same_owner(target) {
            return None;
        }
        let mut peers: Vec<_> = remotes
            .iter()
            .filter(|(id, r)| {
                *id != target
                    && r.online
                    && r.presence.has("anchor")
                    && self.same_owner(id)
                    && r.presence.anchor_routes.iter().any(|route| route.machine == *target)
            })
            .map(|(id, _)| *id)
            .collect();
        peers.sort();
        peers.into_iter().next()
    }

    pub fn accept_anchor(&self, source: EndpointId, target: EndpointId, msg: Message) -> Result<()> {
        if !self.cfg.anchor || !self.same_owner(&source) || !self.same_owner(&target) || target == self.key.public() {
            bail!("an anchor only accepts mail between its owner's other machines");
        }
        let msg = mail::clean(msg)?;
        let Some((person, name)) = msg.to.split_once('/') else {
            bail!("an anchor needs a permanent mailbox");
        };
        if person != self.person || !msg.from.starts_with(&format!("{person}/")) {
            bail!("an anchor only accepts its owner's mail");
        }
        let g = self.inner.lock().unwrap();
        let known = self.saved_mailboxes(&g).iter().any(|r| r.machine == target && r.names.iter().any(|n| n == name));
        if !known {
            bail!("unknown permanent mailbox {} on {target}; connect that machine once to learn it", msg.to);
        }
        g.store.anchor_accept(&source.to_string(), &target.to_string(), &msg, util::now_ms())?;
        Ok(())
    }

    pub fn receive_forward(
        self: &Arc<Self>,
        anchor: EndpointId,
        source: EndpointId,
        msg: Message,
    ) -> Result<(String, String)> {
        if !self.same_owner(&anchor) || !self.same_owner(&source) {
            bail!("a forwarded message must come from this person's machines");
        }
        let now = util::now_ms();
        if msg.sent_at <= 0 || msg.sent_at > now.saturating_add(5 * 60_000) {
            bail!("invalid original send time");
        }
        if msg.sent_at <= now.saturating_sub(ANCHOR_TTL_MS) {
            bail!("the forwarded message expired");
        }
        {
            let g = self.inner.lock().unwrap();
            let name = msg
                .to
                .strip_prefix(&format!("{}/", self.person))
                .ok_or_else(|| anyhow!("not this person's mailbox"))?;
            if !g.store.session_by_name(name)?.is_some_and(|r| r.mailbox && !self.cfg.off_in(&r.cwd)) {
                return Ok(("refused".into(), "the permanent mailbox was removed".into()));
            }
        }
        self.receive(source, msg)
    }

    fn anchor_notice(&self, original: &Message, why: &str) -> Message {
        Message {
            id: ulid::Ulid::generate().to_string(),
            from: self.addr("terminal"),
            from_agent: Some("cli".into()),
            from_repo: None,
            to: self.addr(NOTICE_INBOX),
            via: None,
            reply_to: Some(original.id.clone()),
            body: format!("Anchor on {} dropped message {} for {}: {why}.", self.machine, original.id, original.to),
            wake: false,
            notify: false,
            sent_at: util::now_ms(),
            approval: None,
            files: Vec::new(),
        }
    }

    /// A final recipient ACK releases only mail addressed to that transport peer, never another leg of the handoff.
    pub fn forward_done(&self, peer: &EndpointId, id: &str, state: &str, note: &str) -> Result<()> {
        if !matches!(state, "refused" | "queued" | "delivered" | "pushed" | "held") {
            return Ok(());
        }
        let g = self.inner.lock().unwrap();
        if let Some(held) = g.store.anchor_for_ack(id, &peer.to_string())? {
            if held.cancelled {
                return Ok(());
            }
            let notice = (state == "refused").then(|| self.anchor_notice(&held.msg, note));
            g.store.anchor_finish(id, notice.as_ref())?;
        }
        Ok(())
    }

    /// Runs on presence/retry ticks, never holding the database mutex while calling the network.
    pub fn anchor_tick(self: &Arc<Self>, net: &Net) {
        if let Err(e) = self.anchor_flush(net) {
            log(&format!("anchor mail: {e:#}"));
        }
    }

    fn anchor_flush(self: &Arc<Self>, net: &Net) -> Result<()> {
        let remotes = net.remotes();
        let now = util::now_ms();
        let pending = self.inner.lock().unwrap().store.anchor_pending()?;
        for held in pending {
            let source: EndpointId = held.source.parse()?;
            let target: EndpointId = held.target.parse()?;
            let reason = if held.cancelled {
                Some("this handoff was cancelled before delivery")
            } else if !self.same_owner(&source) || !self.same_owner(&target) {
                Some("a machine was removed or revoked")
            } else if held.msg.sent_at <= now.saturating_sub(ANCHOR_TTL_MS) {
                Some("seven days elapsed since the original send")
            } else {
                None
            };
            if let Some(reason) = reason {
                let notice = self.anchor_notice(&held.msg, reason);
                let g = self.inner.lock().unwrap();
                g.store.anchor_cancel(&held.msg.id)?;
                g.store.anchor_finish(&held.msg.id, Some(&notice))?;
            } else if remotes.get(&target).is_some_and(|r| r.online && r.presence.has(FEATURE)) {
                net.anchor_send(target, &held.msg.id, Wire::Forward { source, msg: held.msg.clone() });
            }
        }
        self.inner.lock().unwrap().store.anchor_prune(now)?;
        for target in self.peer_ids().into_iter().filter(|p| self.same_owner(p) && !net.is_online(p)) {
            let routes = {
                let g = self.inner.lock().unwrap();
                self.learned_routes(&g, &remotes)
            };
            for msg in self.outbox_for(&target) {
                if !routes.iter().any(|r| {
                    r.machine == target
                        && msg.to.split_once('/').is_some_and(|(_, name)| r.names.iter().any(|n| n == name))
                }) {
                    continue;
                }
                if self.cfg.anchor {
                    if self.accept_anchor(self.key.public(), target, msg.clone()).is_ok() {
                        self.outbox_done_from(&target, &msg.id, true);
                    }
                } else if let Some(anchor) = self.anchor_peer(&remotes, &target) {
                    net.anchor_send(anchor, &msg.id.clone(), Wire::Anchor { target, msg });
                }
            }
        }
        Ok(())
    }

    pub(super) fn remove_anchor_machine(&self, g: &Inner, id: &EndpointId) -> Result<()> {
        let machine = id.to_string();
        g.store.anchor_cancel_machine((*id != self.key.public()).then_some(machine.as_str()))?;
        g.store.delete_prefix(&format!("anchor-route:{id}"))?;
        for held in g
            .store
            .anchor_pending()?
            .into_iter()
            .filter(|h| *id == self.key.public() || h.source == id.to_string() || h.target == id.to_string())
        {
            let notice = self.anchor_notice(&held.msg, "a machine left or was removed from the team");
            g.store.anchor_finish(&held.msg.id, Some(&notice))?;
        }
        if *id == self.key.public() {
            g.store.delete_prefix("anchor-route:")?;
        }
        Ok(())
    }
}
