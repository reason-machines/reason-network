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
pub(crate) const RECEIPTS: &str = "anchor-receipts";
/// Not a session slug: only this machine's terminal can read its custody notices, even after a rename.
pub(super) const NOTICE_INBOX: &str = "@anchor";

#[cfg(test)]
mod tests;

impl Daemon {
    pub fn selected_anchor(&self) -> Option<EndpointId> {
        self.team_state()?.selected_anchor(&self.key.public())
    }

    pub(super) fn set_anchor(&self, machine: &str, trust: bool) -> Result<String> {
        let state = {
            let mut g = self.inner.lock().unwrap();
            let mut team = g.team.clone().ok_or_else(|| anyhow!("not in a team"))?;
            let endpoint = if machine == "none" {
                None
            } else {
                let matches: Vec<_> = team
                    .active()
                    .filter(|m| m.person == self.person && (m.machine == machine || m.endpoint.to_string() == machine))
                    .map(|m| m.endpoint)
                    .collect();
                if matches.len() != 1 {
                    bail!("name one of your own computers from `reason-network team`, or `none`");
                }
                Some(matches[0])
            };
            if trust {
                team.trust_anchor(&self.key, endpoint.ok_or_else(|| anyhow!("name a trusted own computer"))?)?;
            } else {
                team.select_anchor(&self.key, endpoint)?;
            }
            let state = team.clone();
            g.store.set("team", &serde_json::to_string(&state)?)?;
            g.team = Some(team);
            state
        };
        let note = if trust {
            let selected = state
                .selected_anchor(&self.key.public())
                .and_then(|id| state.member(&id))
                .map_or("none", |m| m.machine.as_str());
            format!("Following {machine}'s signed anchor choice on {} (currently {selected}).", self.machine)
        } else if machine == "none" {
            format!("Anchor handoffs disabled on {}.", self.machine)
        } else {
            format!("Anchor choice signed on {}: {machine}. Enable `anchor = true` on that computer.", self.machine)
        };
        if let Some(net) = self.net() {
            net.broadcast(&Wire::State { state: Box::new(state) });
            net.presence_changed();
        }
        Ok(note)
    }

    pub(super) fn anchor_status(&self) -> Vec<String> {
        let Some(team) = self.team_state() else { return Vec::new() };
        let selected = team.selected_anchor(&self.key.public());
        let mut lines = vec![match selected.and_then(|id| team.member(&id)) {
            Some(m) => format!("anchor: {} ({})", m.machine, m.endpoint),
            None => "anchor: none selected; use `reason-network anchor <machine>` on a trusted own computer".into(),
        }];
        let mut claims: Vec<_> = self
            .net()
            .map(|n| {
                n.remotes()
                    .into_iter()
                    .filter(|(id, r)| {
                        r.online
                            && r.presence.has("anchor")
                            && team.member(id).is_some_and(|m| m.active && m.person == self.person)
                    })
                    .map(|(id, _)| id)
                    .collect()
            })
            .unwrap_or_default();
        if self.cfg.anchor {
            claims.push(self.key.public());
        }
        claims.sort();
        claims.dedup();
        if claims.is_empty() && team.anchor_choice(&self.key.public()).is_none() {
            return Vec::new();
        }
        if claims.len() > 1 || claims.iter().any(|id| Some(*id) != selected) {
            lines.push(format!(
                "! unselected/multiple anchor claims: {}; only the signed choice is used",
                claims
                    .iter()
                    .filter_map(|id| team.member(id))
                    .map(|m| m.machine.as_str())
                    .collect::<Vec<_>>()
                    .join(", ")
            ));
        }
        lines
    }
    pub fn anchor_local(&self) -> bool {
        self.cfg.anchor && self.selected_anchor() == Some(self.key.public())
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
                g.team.as_ref().is_some_and(|t| {
                    t.member(&r.machine).is_some_and(|m| m.active && m.person == self.person)
                        && t.selected_anchor(&r.machine) == Some(self.key.public())
                })
            })
            .collect()
    }

    /// Only direct announcements can replace saved routes; advertisements from another anchor are never re-exported.
    pub(super) fn learn_mailboxes(&self, peer: &EndpointId, p: &Presence) -> Result<()> {
        if !self.cfg.anchor || !self.same_owner(peer) {
            return Ok(());
        }
        let g = self.inner.lock().unwrap();
        let consent = g.team.as_ref().and_then(|t| t.selected_anchor(peer)) == Some(self.key.public());
        let names = if p.has(FEATURE) && consent { p.mailboxes.clone() } else { Vec::new() };
        let key = format!("anchor-route:{peer}");
        let route = if names.is_empty() {
            None
        } else {
            Some(serde_json::to_string(&MailboxRoute { machine: *peer, names: names.clone() })?)
        };
        // Presence is discovery, not a cancellation: an old engine or failed mailbox read may omit these names.
        g.store.anchor_update_route(&key, route.as_deref(), &[])?;
        // The next presence contains the changed routes, even when no local session changed.
        drop(g);
        if let Some(net) = self.net() {
            net.presence_changed();
        }
        Ok(())
    }

    pub(super) fn learned_routes(&self, g: &Inner, remotes: &HashMap<EndpointId, Remote>) -> Vec<MailboxRoute> {
        let selected = g.team.as_ref().and_then(|t| t.selected_anchor(&self.key.public()));
        let mut routes = if selected == Some(self.key.public()) { self.saved_mailboxes(g) } else { Vec::new() };
        routes.retain(|route| !remotes.get(&route.machine).is_some_and(|r| r.online));
        for (id, r) in remotes {
            if r.online
                && r.presence.has("anchor")
                && Some(*id) == selected
                && g.team.as_ref().is_some_and(|t| t.member(id).is_some_and(|m| m.active && m.person == self.person))
            {
                routes.extend(r.presence.anchor_routes.iter().filter(|route| {
                    g.team.as_ref().is_some_and(|t| t.member(&route.machine).is_some_and(|m| m.active && m.person == self.person)
                        && t.selected_anchor(&route.machine) == Some(*id))
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
        let selected = self.selected_anchor()?;
        if self.team_state()?.selected_anchor(target) != Some(selected) {
            return None;
        }
        let mut peers: Vec<_> = remotes
            .iter()
            .filter(|(id, r)| {
                *id != target
                    && **id == selected
                    && r.online
                    && r.presence.has("anchor")
                    && r.presence.has(RECEIPTS)
                    && self.same_owner(id)
                    && r.presence.anchor_routes.iter().any(|route| route.machine == *target)
            })
            .map(|(id, _)| *id)
            .collect();
        peers.sort();
        peers.into_iter().next()
    }

    pub fn accept_anchor(&self, source: EndpointId, target: EndpointId, msg: Message) -> Result<()> {
        let g = self.inner.lock().unwrap();
        if !self.cfg.anchor
            || target == self.key.public()
            || !g.team.as_ref().is_some_and(|t| {
                [source, target].iter().all(|id| {
                    t.member(id).is_some_and(|m| m.active && m.person == self.person)
                        && t.selected_anchor(id) == Some(self.key.public())
                })
            })
        {
            bail!("both own machines must select this anchor before it accepts their mail");
        }
        let msg = mail::clean(msg)?;
        let Some((person, name)) = msg.to.split_once('/') else {
            bail!("an anchor needs a permanent mailbox");
        };
        if person != self.person || !msg.from.starts_with(&format!("{person}/")) {
            bail!("an anchor only accepts its owner's mail");
        }
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
        if !self.same_owner(&anchor) || !self.same_owner(&source) || self.selected_anchor() != Some(anchor) {
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

    pub fn anchor_stored(&self, anchor: &EndpointId, target: &EndpointId, id: &str) -> Result<bool> {
        self.inner.lock().unwrap().store.outbox_anchored(&anchor.to_string(), &target.to_string(), id)
    }

    pub fn anchor_result(&self, source: &EndpointId, target: &EndpointId, id: &str) -> Result<Option<String>> {
        self.inner.lock().unwrap().store.anchor_result(&source.to_string(), &target.to_string(), id, util::now_ms())
    }

    pub fn anchor_record_result(
        &self,
        anchor: &EndpointId,
        target: &EndpointId,
        id: &str,
        stage: &str,
    ) -> Result<bool> {
        self.inner.lock().unwrap().store.anchor_record_result(&anchor.to_string(), &target.to_string(), id, stage)
    }

    fn anchor_flush(self: &Arc<Self>, net: &Net) -> Result<()> {
        {
            let g = self.inner.lock().unwrap();
            if !self.cfg.anchor
                && g.team.as_ref().and_then(|t| t.selected_anchor(&self.key.public())).is_none()
                && !g.store.anchor_has_work()?
            {
                return Ok(());
            }
        }
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
                let stage = if held.msg.sent_at <= now.saturating_sub(ANCHOR_TTL_MS) { "expired" } else { "rejected" };
                g.store.anchor_finish_as(&held.msg.id, Some(&notice), stage)?;
            } else if remotes.get(&target).is_some_and(|r| r.online && r.presence.has(FEATURE)) {
                net.anchor_send(target, &held.msg.id, Wire::Forward { source, msg: held.msg.clone() });
            }
        }
        self.inner.lock().unwrap().store.anchor_prune(now)?;
        let queries = self.inner.lock().unwrap().store.anchor_queries(now)?;
        for (id, anchor, target) in queries {
            let (anchor, target): (EndpointId, EndpointId) = (anchor.parse()?, target.parse()?);
            if anchor == self.key.public() {
                if let Some(stage) = self.anchor_result(&self.key.public(), &target, &id)? {
                    self.anchor_record_result(&anchor, &target, &id, &stage)?;
                }
            } else {
                net.anchor_send(anchor, &id, Wire::AnchorQuery { target, id: id.clone() });
            }
        }
        let routes = self.learned_routes(&self.inner.lock().unwrap(), &remotes);
        if routes.is_empty() {
            return Ok(());
        }
        for target in self.peer_ids().into_iter().filter(|p| self.same_owner(p) && !net.is_online(p)) {
            if !routes.iter().any(|r| r.machine == target) {
                continue;
            }
            for msg in self.outbox_for(&target) {
                if !routes.iter().any(|r| {
                    r.machine == target
                        && msg.to.split_once('/').is_some_and(|(_, name)| r.names.iter().any(|n| n == name))
                }) {
                    continue;
                }
                if self.anchor_local() {
                    if net.anchor_attempt(self.key.public(), &msg.id, Some(target))
                        && self.accept_anchor(self.key.public(), target, msg.clone()).is_ok()
                    {
                        self.anchor_stored(&self.key.public(), &target, &msg.id)?;
                        net.anchor_attempt_done(self.key.public(), &msg.id);
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
