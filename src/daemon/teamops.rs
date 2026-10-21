//! Team membership and the network: create, join, invite, remove, leave; and what the network layer asks of us.

use super::{Daemon, Inner, log};
use crate::{
    CLI,
    config::Config,
    invite,
    net::{self, JoinRequest, Net, Remote, Wire},
    proof,
    proto::*,
    team::{self, TeamState, Ticket},
    util,
};
use anyhow::{Result, anyhow, bail};
use iroh::{EndpointAddr, EndpointId};
use std::{collections::HashMap, collections::HashSet, sync::Arc, time::Duration};

fn is_private(ip: std::net::IpAddr) -> bool {
    match ip {
        std::net::IpAddr::V4(v4) => v4.is_private() || v4.is_link_local(),
        std::net::IpAddr::V6(v6) => (v6.segments()[0] & 0xfe00) == 0xfc00,
    }
}

impl Daemon {
    pub fn team_state(&self) -> Option<TeamState> {
        self.inner.lock().unwrap().team.clone()
    }

    pub fn team_id(&self) -> Option<String> {
        self.inner.lock().unwrap().team.as_ref().map(|t| t.id.clone())
    }

    pub fn is_member(&self, id: &EndpointId) -> bool {
        self.inner.lock().unwrap().team.as_ref().is_some_and(|t| t.is_active(id))
    }

    /// Teammates' machines to stay connected to.
    pub fn peer_ids(&self) -> Vec<EndpointId> {
        let me = self.key.public();
        let g = self.inner.lock().unwrap();
        g.team.as_ref().map(|t| t.active().map(|m| m.endpoint).filter(|e| *e != me).collect()).unwrap_or_default()
    }

    fn save_team(&self, g: &Inner) -> Result<()> {
        match &g.team {
            Some(t) => g.store.set("team", &serde_json::to_string(t)?),
            None => g.store.delete_prefix("team"),
        }
    }

    /// The network again, on the way to the relays that gets through now (see `net::route_loop`).
    pub(crate) async fn restart_net(self: &Arc<Self>) {
        let old = self.net.write().unwrap().take();
        if let Some(net) = old {
            net.shutdown().await;
        }
        if let Err(e) = self.start_net().await {
            log(&format!("network not started: {e:#}"));
        }
    }

    pub(super) async fn start_net(self: &Arc<Self>) -> Result<()> {
        let team = self.team_state().ok_or_else(|| anyhow!("not in a team"))?;
        let relay = team.meta.body.relay.as_deref();
        let endpoint = net::bind(self.key.clone(), relay, self.cfg.lan()).await?;
        // Last known presence of each machine, so `who --all` shows offline teammates after a restart.
        let mut remote = HashMap::new();
        {
            let g = self.inner.lock().unwrap();
            for m in team.active() {
                if let Some(rem) = g
                    .store
                    .get(&format!("presence:{}", m.endpoint))?
                    .and_then(|s| serde_json::from_str::<Remote>(&s).ok())
                {
                    remote.insert(m.endpoint, rem);
                }
            }
        }
        let net = Net::start(self, endpoint, relay, remote);
        log(&format!("team {} · machine {}", team.shown_name(), net.id().fmt_short()));
        *self.net.write().unwrap() = Some(net.clone());
        // Invite codes made here that are still good: listen for them again.
        let codes: Vec<(String, i64)> = {
            let g = self.inner.lock().unwrap();
            g.store
                .prefixed("code:")?
                .into_iter()
                .filter_map(|(k, v)| Some((k.strip_prefix("code:")?.to_string(), v.parse().ok()?)))
                .collect()
        };
        for (code, expires) in codes {
            if team.invite_unused(&invite::secret(&code)) {
                net.listen_invite(invite::key(&code), expires, relay, self.cfg.lan()).await?;
            } else {
                self.inner.lock().unwrap().store.delete_prefix(&format!("code:{code}"))?;
            }
        }
        Ok(())
    }

    pub fn save_presence(&self, id: &EndpointId, rem: &Remote) {
        if let Ok(s) = serde_json::to_string(rem) {
            let _ = self.inner.lock().unwrap().store.set(&format!("presence:{id}"), &s);
        }
    }

    pub fn outbox_for(&self, id: &EndpointId) -> Vec<Message> {
        self.inner.lock().unwrap().store.outbox_for(&id.to_string(), util::now_ms()).unwrap_or_default()
    }

    pub fn outbox_done(&self, id: &str, taken: bool) {
        let _ = self.inner.lock().unwrap().store.outbox_done(id, taken);
    }

    /// Takes in a teammate's copy of the team state, passing it on if it taught us something.
    pub fn merge_state(self: &Arc<Self>, other: &TeamState) {
        let me = self.key.public();
        let (state, removed, gone, relay_changed) = {
            let mut g = self.inner.lock().unwrap();
            let Some(team) = g.team.as_mut() else { return };
            let relay = team.meta.body.relay.clone();
            let before: HashSet<EndpointId> = team.active().map(|m| m.endpoint).collect();
            match team.merge(other) {
                Ok(true) => {}
                Ok(false) => return,
                Err(e) => return log(&format!("ignoring team state: {e:#}")),
            }
            let after: HashSet<EndpointId> = team.active().map(|m| m.endpoint).collect();
            let state = team.clone();
            let relay_changed = state.meta.body.relay != relay;
            let gone = !after.contains(&me);
            // Removed: what came with the news (a team link that replaced the one this machine knew, from a teammate
            // on an older build) is never written here, not even to the database's log.
            if !gone {
                let _ = self.save_team(&g);
            }
            (state, before.difference(&after).copied().collect::<Vec<_>>(), gone, relay_changed)
        };
        if let Some(net) = self.net() {
            net.broadcast(&Wire::State { state: Box::new(state) });
            for id in &removed {
                net.disconnect(id);
            }
            net.dial_now();
        }
        if gone {
            log("this machine was removed from the team");
            let mut g = self.inner.lock().unwrap();
            g.team = None;
            let _ = self.save_team(&g);
            let _ = g.store.delete_prefix("presence:");
            let _ = g.store.scrub();
            drop(g);
            self.restart_soon();
        } else if relay_changed {
            log("the team's relay changed; restarting to use it");
            self.restart_soon();
        }
    }

    /// Someone presented an invite (on the join protocol).
    pub fn redeem(self: &Arc<Self>, joiner: EndpointId, req: &JoinRequest) -> Result<TeamState> {
        // Signed in to Reason: the name is the one Reason gave. (Expiry doesn't decide who joins: peers don't check it.)
        let proof = req.proof.as_deref().map(|p| proof::verify_ignoring_expiry(p, &joiner)).transpose()?;
        let person = proof.as_ref().map_or_else(|| util::slug(&req.person), |p| p.name.clone());
        let machine = util::slug(&req.machine);
        if person.is_empty() || machine.is_empty() {
            bail!("a name needs letters or digits");
        }
        let state = {
            let mut g = self.inner.lock().unwrap();
            let team = g.team.as_mut().ok_or_else(|| anyhow!("this machine is not in a team any more"))?;
            if !req.team.is_empty() && req.team != team.id {
                bail!("this invite is for a different team");
            }
            team.redeem(&self.key, &req.secret, &person, joiner, proof.as_ref(), req.owner_only)?;
            if let Some(p) = &req.proof {
                team.attest(&joiner, p);
            }
            team.put_member(&self.key, joiner, &person, &machine, true);
            let state = team.clone();
            self.save_team(&g)?;
            state
        };
        if let Some(net) = self.net() {
            net.broadcast(&Wire::State { state: Box::new(state.clone()) });
            net.dial_now();
        }
        Ok(state)
    }

    /// Starts a team, with its own relay from the first moment if `relay` is given.
    pub(super) async fn create_team(
        self: &Arc<Self>,
        name: &str,
        person: &str,
        relay: Option<String>,
        proof: Option<String>,
    ) -> Result<TeamInfo> {
        if let Some(t) = self.team_state() {
            bail!("already in team '{}'; run `{CLI} leave` first", t.shown_name());
        }
        let checked = proof.as_deref().map(|p| proof::verify(p, &self.key.public())).transpose()?;
        let name = name.trim();
        let person = checked.map_or_else(|| util::slug(person), |p| p.name);
        if name.is_empty() || person.is_empty() {
            bail!("the team and your name need letters or digits");
        }
        let mut state = TeamState::found(name, &person, &self.machine, &self.key);
        if let Some(p) = &proof {
            state.attest(&self.key.public(), p);
        }
        if let Some(u) = &relay {
            net::check_relay(self.key.clone(), u).await?;
            state.set_relay(&self.key, relay.clone());
        }
        {
            let mut g = self.inner.lock().unwrap();
            g.team = Some(state);
            self.save_team(&g)?;
        }
        self.become_person(&person)?;
        self.restart_soon();
        Ok(TeamInfo {
            id: self.team_id().unwrap_or_default(),
            name: name.into(),
            team_name: name.into(),
            relay,
            relay_connected: None,
            person,
            machine: self.machine.clone(),
            machines: Vec::new(),
            features: vec![crate::files::FEATURE.into()],
            messages: Vec::new(),
        })
    }

    /// Joins with an invite code (`plum-tiger-lake-orbit`) or a team link (`rn1-…`, or an older `tincan1…`).
    pub(super) async fn join_team(
        self: &Arc<Self>,
        invite_text: &str,
        person: &str,
        proof: Option<String>,
    ) -> Result<TeamInfo> {
        // A proof that doesn't verify here won't anywhere: say so before leaving anything.
        let checked = proof.as_deref().map(|p| proof::verify(p, &self.key.public())).transpose()?;
        sign_in_needs_a_link(invite_text, proof.as_deref())?;
        let person = checked.map_or_else(|| person.to_string(), |p| p.name);
        let mut solo = false;
        if let Some(t) = self.team_state() {
            let me = self.key.public();
            let mut others: Vec<String> = t.active().filter(|m| m.endpoint != me).map(|m| m.person.clone()).collect();
            others.dedup();
            if !others.is_empty() {
                bail!(
                    "already in team '{}' (with {}). To switch, run `{CLI} leave`, then join again",
                    t.shown_name(),
                    others.join(", ")
                );
            }
            // A team of one that nobody joined: switching loses nothing. It stays until the join succeeds.
            solo = true;
            let old = self.net.write().unwrap().take();
            if let Some(net) = old {
                net.shutdown().await;
            }
        }
        let res = self.join_with(invite_text, &person, proof).await;
        if let Err(e) = &res {
            log(&format!("couldn't join: {e:#}"));
            if solo {
                let _ = self.start_net().await;
            }
        }
        res
    }

    async fn join_with(self: &Arc<Self>, invite_text: &str, person: &str, proof: Option<String>) -> Result<TeamInfo> {
        let person = util::slug(person);
        if person.is_empty() {
            bail!("your name needs letters or digits");
        }
        // A code points at the key the inviting machine listens on for it; a ticket lists machines.
        let (team, secret, relay, peers) = match invite::normalize(invite_text) {
            Some(code) => {
                (String::new(), invite::secret(&code), None, vec![EndpointAddr::new(invite::key(&code).public())])
            }
            None if Ticket::is_ticket(invite_text) => {
                let t = Ticket::decode(invite_text)?;
                let addrs = t.addrs();
                (t.team, t.secret, t.relay, addrs)
            }
            None => bail!("that's not an invite code (four words, like plum-tiger-lake-orbit)"),
        };
        let (endpoint, relays) = net::bind_settled(self.key.clone(), relay.as_deref(), self.cfg.lan()).await?;
        let req = JoinRequest {
            team: team.clone(),
            secret,
            person: person.clone(),
            machine: self.machine.clone(),
            proof: proof.clone(),
            owner_only: proof.is_some(),
        };
        // Another computer of an account that holds a name is best let in by one of that person's own machines, which
        // every build takes: ask for that first, and once more shortly if a machine on the link couldn't be reached (it
        // may be theirs, not found yet). Else any machine of this build lets it in on its proof, which builds before
        // proofs don't take.
        let same_account = |r: &Result<_>| matches!(r, Err(e) if e.to_string().contains(team::SAME_ACCOUNT));
        let mut tried = net::join(&endpoint, &relays, &peers, &req).await;
        if same_account(&tried.result) && tried.unreachable {
            tokio::time::sleep(Duration::from_secs(5)).await;
            let again = net::join(&endpoint, &relays, &peers, &req).await;
            if again.result.is_ok() {
                tried = again;
            }
        }
        let res = match tried.result {
            Err(e) if e.to_string().contains(team::SAME_ACCOUNT) => {
                let any = JoinRequest { owner_only: false, ..req };
                net::join(&endpoint, &relays, &peers, &any)
                    .await
                    .result
                    .map_err(|again| if taken(&again) { e } else { again })
            }
            r => r,
        };
        endpoint.close().await;
        let (state, by) = res?;
        let mut state = TeamState::verified(&state)?;
        let who =
            state.member(&by).map_or_else(|| by.fmt_short().to_string(), |m| format!("{} on {}", m.person, m.machine));
        log(&format!("joined team {} as {person}: let in by {who}", state.shown_name()));
        let me = self.key.public();
        // A ticket names the team; a code can't (whoever answers on the code's key knows the code, so could make up a
        // team), which is why a sign-in joins only with a link.
        if (!team.is_empty() && state.id != team) || !state.is_active(&me) {
            bail!("the team did not add this machine");
        }
        // An inviting machine from before sign-in proofs passes ours over: it goes out with this machine's state.
        if let Some(p) = &proof {
            state.attest(&me, p);
        }
        let info = TeamInfo {
            id: state.id.clone(),
            name: state.shown_name(),
            team_name: state.shown_name(),
            relay: state.meta.body.relay.clone(),
            relay_connected: None,
            person: person.clone(),
            machine: self.machine.clone(),
            machines: Vec::new(),
            features: vec![crate::files::FEATURE.into()],
            messages: Vec::new(),
        };
        {
            let mut g = self.inner.lock().unwrap();
            g.team = Some(state);
            self.save_team(&g)?;
        }
        self.become_person(&person)?;
        self.restart_soon();
        Ok(info)
    }

    /// Attaches (or refreshes) this machine's Reason sign-in proof, for its teammates to check.
    pub(super) fn attach_proof(self: &Arc<Self>, jws: &str) -> Result<String> {
        let me = self.key.public();
        let p = proof::verify(jws, &me)?;
        let state = {
            let mut g = self.inner.lock().unwrap();
            let team = g.team.as_mut().ok_or_else(|| anyhow!("not in a team yet: run `{CLI}`"))?;
            let person = team.member(&me).map(|m| m.person.clone()).unwrap_or_default();
            if p.name != person {
                bail!("this proof is for {}, but this machine is {person} in the team", p.name);
            }
            team.attest(&me, jws);
            let state = team.clone();
            self.save_team(&g)?;
            state
        };
        if let Some(net) = self.net() {
            net.broadcast(&Wire::State { state: Box::new(state) });
        }
        let days = (p.exp - util::now_ms() / 1000) / 86_400;
        Ok(format!("{} on {} is verified, for {days} more days.", p.name, self.machine))
    }

    fn become_person(&self, person: &str) -> Result<()> {
        let mut cfg =
            Config::load_strict().map_err(|e| anyhow!("fix {} first: {e}", crate::paths::config().display()))?;
        cfg.person = Some(person.to_string());
        cfg.save()
    }

    /// Makes an invite: a code (this machine listens for it until it is used or expires) and the equivalent
    /// long ticket (which any teammate's machine can redeem).
    pub(super) async fn invite(&self, me: bool, hours: u64) -> Result<InviteInfo> {
        let net =
            self.net().ok_or_else(|| anyhow!("not connected to a team yet (run `{CLI} init` or `{CLI} join`)"))?;
        net.online(Duration::from_secs(5)).await;
        let hours = hours.clamp(1, 24 * 30);
        let code = invite::generate();
        let secret = invite::secret(&code);
        let expires = util::now_ms() + hours as i64 * 3_600_000;
        let (state, ticket) = {
            let mut g = self.inner.lock().unwrap();
            let team = g.team.as_mut().ok_or_else(|| anyhow!("not in a team"))?;
            team.prune_invites();
            team.invite(&self.key, &secret, hours, me.then(|| self.person.clone()));
            let ticket = self.ticket(team, secret);
            let state = team.clone();
            self.save_team(&g)?;
            if state.meta.body.relay.is_none() {
                g.store.set(&format!("code:{code}"), &expires.to_string())?;
            }
            (state, ticket)
        };
        let relay = state.meta.body.relay.clone();
        net.broadcast(&Wire::State { state: Box::new(state) });
        // Finding the machine behind a code takes n0's address lookup: a team with its own relay gets the long form.
        if relay.is_some() {
            let ticket = ticket.encode();
            return Ok(InviteInfo { code: ticket.clone(), ticket, hours });
        }
        net.listen_invite(invite::key(&code), expires, relay.as_deref(), self.cfg.lan()).await?;
        Ok(InviteInfo { code, ticket: ticket.encode(), hours })
    }

    /// A ticket for `secret`: this machine first, then a few teammates, so whoever is online can let them in.
    fn ticket(&self, team: &TeamState, secret: String) -> Ticket {
        let me = self.key.public();
        let online = |id: &EndpointId| self.net().is_some_and(|n| n.is_online(id));
        let mut others: Vec<EndpointId> = team.active().map(|m| m.endpoint).filter(|e| *e != me).collect();
        others.sort_by_key(|id| !online(id));
        let peers = std::iter::once(me).chain(others.into_iter().take(2)).collect();
        let here = self.net().map(|n| n.addr());
        Ticket {
            team: team.id.clone(),
            name: team.name().into(),
            secret,
            relay: team.meta.body.relay.clone(),
            hint_relay: here.as_ref().and_then(|a| a.relay_urls().next().map(|u| u.to_string())),
            // Private addresses are what helps (same LAN, no internet); public ones are found anyway.
            hint_ips: here.map(|a| a.ip_addrs().filter(|s| is_private(s.ip())).copied().collect()).unwrap_or_default(),
            peers,
        }
    }

    /// The team link (`new`: replace it first, so the old one stops working).
    pub(super) async fn link(self: &Arc<Self>, new: bool) -> Result<LinkInfo> {
        if let Some(net) = self.net() {
            net.online(Duration::from_secs(5)).await;
        }
        let state = {
            let mut g = self.inner.lock().unwrap();
            let team = g.team.as_mut().ok_or_else(|| anyhow!("not in a team yet: run `{CLI}`"))?;
            if new || team.link_secret().is_none() {
                team.new_link(&self.key);
            }
            let state = team.clone();
            self.save_team(&g)?;
            state
        };
        if new && let Some(net) = self.net() {
            net.broadcast(&Wire::State { state: Box::new(state.clone()) });
        }
        let secret = state.link_secret().expect("just made").to_string();
        let ticket = self.ticket(&state, secret).encode();
        Ok(LinkInfo { team: state.shown_name(), url: crate::join_url(&ticket), ticket })
    }

    /// The team link as it is now, without making one (for `list_team`).
    pub(super) fn current_link(&self) -> Option<String> {
        let state = self.team_state()?;
        let secret = state.link_secret()?.to_string();
        Some(crate::join_url(&self.ticket(&state, secret).encode()))
    }

    pub(super) fn team_info(&self) -> Result<TeamInfo> {
        let team = self
            .team_state()
            .ok_or_else(|| anyhow!("not in a team yet: `{CLI} init <team>` or `{CLI} join <invite>`"))?;
        Ok(TeamInfo {
            id: team.id.clone(),
            name: team.shown_name(),
            team_name: team.shown_name(),
            relay: team.meta.body.relay.clone(),
            relay_connected: self.net().map(|n| n.relay_connected()),
            person: self.person.clone(),
            machine: self.machine.clone(),
            machines: self.machines(),
            features: vec![crate::files::FEATURE.into()],
            messages: self.inner.lock().unwrap().store.counts(util::now_ms()).unwrap_or_default(),
        })
    }

    /// Every team machine with how it is reached; just this one when not in a team.
    pub(super) fn machines(&self) -> Vec<MachineInfo> {
        let me = self.key.public();
        let here = MachineInfo {
            person: self.person.clone(),
            machine: self.machine.clone(),
            endpoint: me.to_string(),
            is_me: true,
            online: true,
            link: None,
            rtt_ms: None,
            last_seen: None,
            verified: false,
            has_proof: false,
        };
        let (Some(team), net) = (self.team_state(), self.net()) else { return vec![here] };
        let verified = team.verified_people();
        let remotes = net.as_ref().map(|n| n.remotes()).unwrap_or_default();
        let mut out =
            vec![MachineInfo { verified: verified.contains_key(&here.person), has_proof: team.has_proof(&me), ..here }];
        for m in team.active().filter(|m| m.endpoint != me) {
            let link = net.as_ref().and_then(|n| n.link_info(&m.endpoint));
            out.push(MachineInfo {
                person: m.person.clone(),
                machine: m.machine.clone(),
                endpoint: m.endpoint.to_string(),
                is_me: false,
                online: net.as_ref().is_some_and(|n| n.is_online(&m.endpoint)),
                link: link.as_ref().map(|l| if l.relay { "relay" } else { "direct" }.to_string()),
                rtt_ms: link.map(|l| l.rtt_ms),
                last_seen: remotes.get(&m.endpoint).map(|r| r.last_seen).filter(|t| *t > 0),
                verified: verified.contains_key(&m.person),
                has_proof: team.has_proof(&m.endpoint),
            });
        }
        out[1..].sort_by(|a, b| (&a.person, &a.machine).cmp(&(&b.person, &b.machine)));
        out
    }

    pub(super) fn remove(self: &Arc<Self>, who: &str) -> Result<String> {
        let me = self.key.public();
        let (state, removed) = {
            let mut g = self.inner.lock().unwrap();
            let team = g.team.as_mut().ok_or_else(|| anyhow!("not in a team"))?;
            // `person`, `person/machine`, a machine name, or the start of a machine's key.
            let hits: Vec<(EndpointId, String, String)> = team
                .active()
                .filter(|m| match who.split_once('/') {
                    Some((p, mach)) => m.person == p && m.machine == mach,
                    None => {
                        m.person == who
                            || m.machine == who
                            || (who.len() >= 6 && m.endpoint.to_string().starts_with(who))
                    }
                })
                .map(|m| (m.endpoint, m.person.clone(), m.machine.clone()))
                .collect();
            if hits.is_empty() {
                bail!("no person or machine called '{who}' in the team");
            }
            let people: HashSet<&String> = hits.iter().map(|h| &h.1).collect();
            if people.len() > 1 && !team.people().iter().any(|p| p == who) {
                let opts: Vec<String> = hits.iter().map(|h| format!("{}/{}", h.1, h.2)).collect();
                bail!("'{who}' matches machines of several people; say which: {}", opts.join(", "));
            }
            if hits.iter().any(|h| h.0 == me) {
                bail!("that includes this machine; use `{CLI} leave` instead");
            }
            for (id, person, machine) in &hits {
                team.put_member(&self.key, *id, person, machine, false);
            }
            // They knew the team link; make a new one.
            team.new_link(&self.key);
            let state = team.clone();
            self.save_team(&g)?;
            (state, hits)
        };
        if let Some(net) = self.net() {
            net.broadcast(&Wire::State { state: Box::new(state) });
            for (id, ..) in &removed {
                net.disconnect(id);
            }
        }
        let names: Vec<String> = removed.iter().map(|(_, p, m)| format!("{p} on {m}")).collect();
        Ok(format!("Removed {}. The team link changed too (`{CLI} link` shows the new one).", names.join(", ")))
    }

    pub(super) async fn leave(self: &Arc<Self>, force: bool) -> Result<String> {
        let me = self.key.public();
        let anyone = self.net().is_some_and(|n| self.peer_ids().iter().any(|p| n.is_online(p)));
        if !anyone && !self.peer_ids().is_empty() && !force {
            bail!(
                "no teammate is online to hear that this machine left. Try again when one is, or use --force (they'll \
                 need to `{CLI} remove` it then)"
            );
        }
        let state = {
            let mut g = self.inner.lock().unwrap();
            let team = g.team.as_mut().ok_or_else(|| anyhow!("not in a team"))?;
            team.put_member(&self.key, me, &self.person, &self.machine, false);
            team.clone()
        };
        if let Some(net) = self.net() {
            net.broadcast(&Wire::State { state: Box::new(state.clone()) });
            // Give the news a moment to reach teammates.
            tokio::time::sleep(Duration::from_millis(1500)).await;
        }
        {
            let mut g = self.inner.lock().unwrap();
            g.team = None;
            self.save_team(&g)?;
            g.store.delete_prefix("presence:")?;
        }
        self.restart_soon();
        Ok(format!("Left team {}.", state.shown_name()))
    }

    pub(super) async fn set_relay(self: &Arc<Self>, url: Option<String>) -> Result<String> {
        if let Some(u) = &url {
            net::check_relay(self.key.clone(), u).await?;
        }
        let state = {
            let mut g = self.inner.lock().unwrap();
            let team = g.team.as_mut().ok_or_else(|| anyhow!("not in a team"))?;
            team.set_relay(&self.key, url.clone());
            let state = team.clone();
            self.save_team(&g)?;
            state
        };
        let me = self.key.public();
        let online = |id: &EndpointId| self.net().is_some_and(|n| n.is_online(id));
        let away: Vec<String> = state
            .active()
            .filter(|m| m.endpoint != me && !online(&m.endpoint))
            .map(|m| format!("{}/{}", m.person, m.machine))
            .collect();
        if let Some(net) = self.net() {
            net.broadcast(&Wire::State { state: Box::new(state) });
            tokio::time::sleep(Duration::from_millis(800)).await;
        }
        self.restart_soon();
        let (now, cmd) = match &url {
            Some(u) => (format!("The team now uses relay {u}, and nothing of n0's."), format!("{CLI} relay {u}")),
            None => (
                format!("The team now uses relay {}.", crate::render::default_relays()),
                format!("{CLI} relay default"),
            ),
        };
        let mut out = format!("{now} Team links from before stop working: `{CLI} link` shows the new one.");
        // A machine that is off can't hear about it, and won't find the team where it looks when it's back.
        if !away.is_empty() {
            out += &format!(
                "\nOffline now: {}. When back, each needs `{cmd}` too, unless it already uses that relay.",
                away.join(", ")
            );
        }
        Ok(out)
    }
}

/// The name is someone else's in the team (as every build says it).
pub fn taken(e: &anyhow::Error) -> bool {
    e.to_string().contains("is already in this team")
}

/// A sign-in proof goes with a team link, never a four-word code: a code can't show the joiner which team answers it.
fn sign_in_needs_a_link(invite_text: &str, proof: Option<&str>) -> Result<()> {
    if proof.is_some() && invite::normalize(invite_text).is_some() {
        bail!("signing in joins a team with its link, not a four-word code: ask for the team link");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_sign_in_never_joins_with_a_code() {
        let code = invite::generate();
        assert!(sign_in_needs_a_link(&code, Some("jws")).unwrap_err().to_string().contains("not a four-word code"));
        assert!(sign_in_needs_a_link(&code.replace('-', " "), Some("jws")).is_err());
        assert!(sign_in_needs_a_link(&code, None).is_ok(), "codes still work without signing in");
        assert!(sign_in_needs_a_link("https://reasonmachines.com/network/join/#rn1-AAAA", Some("jws")).is_ok());
    }
}
