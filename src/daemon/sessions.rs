//! Sessions running on this machine: registered by `reason network mcp` (a connection), or by hooks (no connection,
//! kept alive by hook events and the agent's process).

use super::{Daemon, Inner, Live, log};
use crate::{
    agent,
    config::{BUSY_OFF, OFF_HERE},
    git::GitInfo,
    proto::*,
    render,
    store::Remembered,
    util,
};
use crate::{net::Remote, team::TeamState};
use anyhow::{Result, anyhow, bail};
use iroh::EndpointId;
use std::{
    collections::{HashMap, HashSet},
    sync::Arc,
    time::Instant,
};

/// A hook-only session that hasn't fired a hook for this long is considered gone.
const HOOK_TTL_MS: i64 = 6 * 3_600_000;
/// A session gone this long isn't coming back: it is forgotten, with the mail it never read.
pub(super) const FORGET_MS: i64 = 24 * 3_600_000;
/// A session in a folder that is off counts as mid-turn this long after its last prompt or tool call.
const OFF_TURN_MS: i64 = 10 * 60_000;

impl Daemon {
    /// How fast hooks let us reach an agent's session (`push_failed`: its push channel didn't work).
    fn hook_tier(&self, agent: &str, push_failed: bool) -> Tier {
        match agent {
            "codex" if !push_failed && agent::find_bin("codex").is_some() => Tier::Push,
            // Their plugin or extension delivers into idle sessions; Reason's CLI sends into its sessions.
            "opencode" | "pi" | "reason" => Tier::Push,
            _ => Tier::Turn,
        }
    }

    /// Registers (or refreshes) a session. Returns its mail that is waiting, for the caller to deliver.
    pub(super) fn register(self: &Arc<Self>, conn: Option<u64>, reg: SessionReg, git: GitInfo) -> Result<Registered> {
        let (registered, pending) = self.register_quiet(conn, reg, git)?;
        let key = pending.0;
        if !pending.1.is_empty() {
            let mut g = self.inner.lock().unwrap();
            self.deliver(&mut g, &key, pending.1, true);
        }
        Ok(registered)
    }

    fn register_quiet(
        &self,
        conn: Option<u64>,
        mut reg: SessionReg,
        git: GitInfo,
    ) -> Result<(Registered, (String, Vec<Message>))> {
        // Every session comes in here: one in a folder that is off never does.
        if self.cfg.off_in(&reg.cwd) {
            bail!(OFF_HERE);
        }
        let now = util::now_ms();
        let others = self.my_names_elsewhere(now);
        let mut g = self.inner.lock().unwrap();
        let g = &mut *g;
        let key = reg.key.clone();
        // Its hooks said its folder is off (an MCP server several sessions share can't know its folder).
        if g.off.contains_key(&key) {
            bail!(OFF_HERE);
        }
        // An agent with an MCP server per session is in one conversation at a time: the one its server registers now
        // ends any other it served (Claude Code's `/clear`, which runs no new server). Also when the new one is known
        // already, from its hooks or Claude Code's registry.
        if conn.is_some() && !agent::multiplexed(&reg.agent) {
            let before: Vec<String> = g
                .live
                .iter()
                .filter(|(k, l)| l.conn == conn && l.reg.agent == reg.agent && **k != key)
                .map(|(k, _)| k.clone())
                .collect();
            self.moved_on(g, before, now);
        }
        if let Some(l) = g.live.get_mut(&key) {
            // Known already (e.g. from a hook, now the MCP server connects, or a reconnect).
            if conn.is_some() {
                l.conn = conn;
                l.attached = true;
            }
            l.reg.tier = if l.push_failed { l.reg.tier.max(Tier::Turn) } else { l.reg.tier.min(reg.tier) };
            l.attached |= l.reg.tier == Tier::Push;
            if l.last_hook.is_none() {
                l.reg.cwd = reg.cwd;
                l.git = git;
            }
            l.reg.pid = reg.pid.or(l.reg.pid);
        } else {
            if conn.is_none() {
                reg.tier = reg.tier.min(self.hook_tier(&reg.agent, false));
            }
            let name = pick_name(g, &reg, &git, &others, &self.person, |r| self.holds(r, now))?;
            self.take_over(g, &name, &key)?;
            let attached = conn.is_some() || reg.tier == Tier::Push || reg.agent == "cli";
            g.live.insert(
                key.clone(),
                Live {
                    reg,
                    name,
                    conn,
                    git,
                    doing: None,
                    since: now,
                    status: None,
                    last_hook: None,
                    push_failed: false,
                    transcript: None,
                    retry: None,
                    stop_gen: 0,
                    attached,
                },
            );
            self.presence_changed();
        }
        let l = &g.live[&key];
        g.store.upsert_session(&remembered(l, now), &self.person)?;
        let name = l.name.clone();
        let address = self.addr(&name);
        let pending: Vec<Message> = g.pending(&address)?;
        let wake: Vec<Message> = pending.iter().filter(|m| m.wake).cloned().collect();
        log(&format!("session {address} registered"));
        Ok((Registered { name, address, unread: pending.len() as u32 }, (key, wake)))
    }

    /// My session names on my other machines, so my addresses stay unique across them (see [`names_elsewhere`]).
    fn my_names_elsewhere(&self, now: i64) -> Elsewhere {
        let (Some(net), Some(team)) = (self.net(), self.team_state()) else { return Elsewhere::default() };
        names_elsewhere(&net.remotes(), &team, &self.person, &self.key.public(), now)
    }

    /// This machine's terminal took a name another of my machines' terminal has too, while every engine in the team
    /// could tell two apart in a reply. Once one that can't is in the team, it takes the next free name, as it would if
    /// it registered now. (Checked as the terminal is used: each command from it comes through here.) A rename that
    /// fails (another session took the name meanwhile) is logged, and the command goes on: it is tried again next time.
    fn recheck_terminal(&self, others: &Elsewhere) {
        self.recheck_terminal_with(others, |name| self.set_status(TERMINAL, None, Some(name)).map(drop));
    }

    /// `recheck_terminal`, renaming with `rename`.
    fn recheck_terminal_with(&self, others: &Elsewhere, rename_to: impl FnOnce(String) -> Result<()>) {
        let now = util::now_ms();
        let rename = || -> Result<()> {
            let name = {
                let g = self.inner.lock().unwrap();
                let Some(l) = g.live.get(TERMINAL) else { return Ok(()) };
                if others.terminals || !others.live.contains(&l.name) {
                    return Ok(());
                }
                pick_name(&g, &l.reg, &l.git, others, &self.person, |r| self.holds(r, now))?
            };
            log(&format!(
                "{} is on another machine too, and an older engine is in the team: now {name}",
                self.addr("terminal")
            ));
            rename_to(name)
        };
        if let Err(e) = rename() {
            log(&format!("renaming {}: {e:#}", self.addr("terminal")));
        }
    }

    /// Whether a session remembered here still keeps its name from every other session: one that can come back to it
    /// (see [`Remembered::kept`]), until it would be forgotten. The person's own mailboxes are never forgotten.
    pub(super) fn holds(&self, r: &Remembered, now: i64) -> bool {
        r.kept() && (r.agent == "cli" || now - r.last_seen <= FORGET_MS || !self.forgetting(now))
    }

    /// Whether sessions gone a day are forgotten yet: not just after a start, while sessions that are still up haven't
    /// registered again (each gets the time a hook-only session has to show it is there).
    fn forgetting(&self, now: i64) -> bool {
        now - self.started > HOOK_TTL_MS
    }

    /// A live session turned out reachable (mail went to it or came from it): from now on its name is kept for it.
    pub(super) fn reached(&self, g: &mut Inner, key: &str) {
        if let Some(l) = g.live.get_mut(key)
            && !l.attached
        {
            l.attached = true;
            let r = remembered(l, util::now_ms());
            if let Err(e) = g.store.upsert_session(&r, &self.person) {
                log(&format!("remembering {}: {e:#}", self.addr(&r.name)));
            }
        }
    }

    /// Ends sessions whose agent is in another conversation now: they are offline from here on, their names kept for
    /// them. (The log names them by address: agents' session ids stay out of it.)
    pub(super) fn moved_on(&self, g: &mut Inner, keys: Vec<String>, now: i64) {
        for k in keys {
            if let Some(l) = g.live.remove(&k) {
                let _ = g.store.touch_session(&l.name, now);
                log(&format!("session {} ended: its agent went on to another conversation", self.addr(&l.name)));
                self.presence_changed();
            }
        }
    }

    /// Before `key` takes `name`: a session that kept the name until it lapsed is forgotten, with the mail it never
    /// read, which is never another session's. A session known only by its process leaves its mail to the name.
    fn take_over(&self, g: &mut Inner, name: &str, key: &str) -> Result<()> {
        if let Some(r) = g.store.session_by_name(name)?
            && r.key != key
            && r.kept()
        {
            log(&format!("{} lapsed: a new session takes the name", self.addr(name)));
            g.store.forget_session(&r, &self.person)?;
        }
        Ok(())
    }

    pub(super) fn set_status(&self, key: &str, doing: Option<String>, name: Option<String>) -> Result<Registered> {
        let now = util::now_ms();
        let others = self.my_names_elsewhere(now);
        let mut g = self.inner.lock().unwrap();
        let g = &mut *g;
        let new_name = name.as_deref().map(util::slug);
        if let Some(n) = new_name.as_deref() {
            if n.is_empty() {
                bail!("a name needs letters or digits");
            }
            if g.live.iter().any(|(k, l)| l.name == n && k != key) || others.live.contains(n) {
                bail!("'{n}' is taken by a live session");
            }
            if n == self.person {
                bail!("'{n}' is your person name; pick another session name");
            }
            // A name kept for a session that is offline stays its own, with its mail: never renamed into.
            if let Some(r) = g.store.session_by_name(n)?.filter(|r| r.key != key && self.holds(r, now)) {
                bail!(
                    "'{n}' belongs to a session that is offline (last seen {} ago); pick another name",
                    util::ago(r.last_seen, now)
                );
            }
            if others.kept.contains(n) {
                bail!("'{n}' belongs to a session on another of your machines that is offline; pick another name");
            }
        }
        if let Some(n) = new_name.as_deref()
            && g.live.get(key).is_some_and(|l| l.name != n)
        {
            self.take_over(g, n, key)?;
        }
        let live = g.live.get_mut(key).ok_or_else(|| anyhow!("this session is not registered"))?;
        if let Some(d) = doing {
            let d = d.trim();
            live.doing = (!d.is_empty()).then(|| d.chars().take(200).collect());
        }
        if let Some(n) = new_name
            && n != live.name
        {
            g.store.rename(
                &live.name,
                &n,
                &format!("{}/{}", self.person, live.name),
                &format!("{}/{n}", self.person),
            )?;
            live.name = n;
        }
        g.store.upsert_session(&remembered(live, now), &self.person)?;
        let address = self.addr(&live.name);
        Ok(Registered { name: live.name.clone(), unread: g.store.unread_count(&address)?, address })
    }

    pub(super) fn unread(&self, key: &str) -> Result<Unread> {
        let mut g = self.inner.lock().unwrap();
        let Some(l) = g.live.get(key) else { return Ok(Unread { count: 0, from: Vec::new() }) };
        let address = self.addr(&l.name);
        let msgs = g.pending(&address)?;
        let mut from: Vec<String> = Vec::new();
        for m in &msgs {
            if !from.contains(&m.from) {
                from.push(m.from.clone());
            }
        }
        Ok(Unread { count: msgs.len() as u32, from })
    }

    /// A hook fired in an agent: keep its session alive, track busy/idle, hand back mail to inject.
    pub(super) async fn hook(self: &Arc<Self>, h: HookCall) -> Result<HookReply> {
        let HookCall { agent, event, session_id, cwd, pid, mail, transcript, doing, name, dirs } = h;
        let (agent, event) = (agent.as_str(), event.as_str());
        let key = format!("{agent}:{session_id}");
        let now = util::now_ms();
        // In a folder that is off, by any folder the agent names: never a session here, only noted as off.
        if cwd.iter().chain(&dirs).any(|d| self.cfg.off_in(d)) {
            let mut g = self.inner.lock().unwrap();
            if let Some(l) = g.live.remove(&key) {
                let _ = g.store.touch_session(&l.name, now);
                self.presence_changed();
            }
            match event {
                "end" => {
                    g.off.remove(&key);
                }
                "doing" => {}
                _ => {
                    g.off.insert(key, (matches!(event, "prompt" | "tool"), now));
                }
            }
            // Hooks print nothing either way; `listen --as` says why.
            bail!(OFF_HERE);
        }
        // Only in folders that are on (again): it left the one that is off.
        self.inner.lock().unwrap().off.remove(&key);
        if event == "end" {
            let mut g = self.inner.lock().unwrap();
            if g.live.get(&key).is_some_and(|l| l.conn.is_none())
                && let Some(l) = g.live.remove(&key)
            {
                let _ = g.store.touch_session(&l.name, now);
                log(&format!("session {} ended", self.addr(&l.name)));
                self.presence_changed();
            }
            return Ok(HookReply::default());
        }
        let known = self.inner.lock().unwrap().live.contains_key(&key);
        if known && key == TERMINAL {
            self.recheck_terminal(&self.my_names_elsewhere(now));
        }
        if !known {
            let remembered_cwd = self.inner.lock().unwrap().store.session_by_key(&key)?.map(|r| r.cwd);
            let cwd = cwd.or(remembered_cwd).unwrap_or_else(|| "/".into());
            let git = self.git(&cwd).await;
            let reg = SessionReg {
                key: key.clone(),
                agent: agent.to_string(),
                cwd,
                agent_session_id: Some(session_id.clone()),
                pid,
                tier: Tier::Pull,
                name,
            };
            self.register_quiet(None, reg, git)?;
        }

        let mut g = self.inner.lock().unwrap();
        let g = &mut *g;
        let Some(l) = g.live.get_mut(&key) else { return Ok(HookReply::default()) };
        if let Some(t) = transcript {
            l.transcript = Some(t.into());
        }
        // Only a report: it says nothing about the session's activity or how to reach it.
        if event == "doing" {
            if let Some(d) = doing {
                l.doing = Some(d.trim().chars().take(200).collect::<String>()).filter(|d| !d.is_empty());
            }
            return Ok(HookReply::default());
        }
        l.last_hook = Some(now);
        l.reg.tier = l.reg.tier.min(self.hook_tier(agent, l.push_failed));
        if pid.is_some() {
            l.reg.pid = pid;
        }
        let status = if matches!(event, "prompt" | "tool") { "busy" } else { "idle" };
        if l.status.as_ref().is_none_or(|(s, _)| s != status) {
            l.status = Some((status.to_string(), now));
        }
        let address = self.addr(&l.name);
        let agent_kind = l.reg.agent.clone();
        // Mid-turn, only what should interrupt; at a pause, everything.
        let msgs: Vec<Message> = if mail {
            g.pending(&address)?.into_iter().filter(|m| event != "tool" || m.wake).collect()
        } else {
            Vec::new()
        };
        // Not marked read here: the hook confirms with `Delivered` once it has printed them. Until then they are in
        // flight, so a hook asking at the same moment (opencode 2.x can run two plugin instances) doesn't get them too.
        let ids: Vec<String> = msgs.iter().map(|m| m.id.clone()).collect();
        for id in &ids {
            g.in_flight.insert(id.clone(), Instant::now());
        }
        // Its hooks hand it mail: it can be reached, and its name stays its own.
        if !ids.is_empty() {
            self.reached(g, &key);
        }
        let text = (!msgs.is_empty()).then(|| render::for_agent(&msgs, Some(&agent_kind), false));
        Ok(HookReply { address: Some(address), text, ids })
    }

    /// The hook-known session of `agent` that is calling a tool right now, when that is unambiguous: the only
    /// busy one in `cwd` (the caller is mid-turn), else the only busy one at all. None while a session of `agent` in a
    /// folder that is off is mid-turn: the caller may be that one, which must never get another session's key.
    pub(super) fn attach(&self, agent: &str, cwd: &str) -> Result<Option<String>> {
        let mut g = self.inner.lock().unwrap();
        self.prune(&mut g);
        let now = util::now_ms();
        let prefix = format!("{agent}:");
        if g.off.iter().any(|(k, (busy, at))| *busy && now - at < OFF_TURN_MS && k.starts_with(&prefix)) {
            bail!(BUSY_OFF);
        }
        let busy: Vec<(&String, &Live)> = g
            .live
            .iter()
            .filter(|(_, l)| {
                l.reg.agent == agent && l.last_hook.is_some() && l.status.as_ref().is_some_and(|(s, _)| s == "busy")
            })
            .collect();
        let here: Vec<&(&String, &Live)> = busy.iter().filter(|(_, l)| l.reg.cwd == cwd).collect();
        let key = match (here.as_slice(), busy.as_slice()) {
            ([(k, _)], _) => Some((*k).clone()),
            ([], [(k, _)]) => Some((*k).clone()),
            _ => None,
        };
        // Its agent's MCP server answers for it now.
        if let Some(l) = key.as_ref().and_then(|k| g.live.get_mut(k))
            && !l.attached
        {
            l.attached = true;
            let r = remembered(l, now);
            g.store.upsert_session(&r, &self.person)?;
        }
        Ok(key)
    }

    pub(super) fn drop_conn(&self, conn: u64) {
        let now = util::now_ms();
        let mut g = self.inner.lock().unwrap();
        g.conns.remove(&conn);
        let mine: Vec<String> = g.live.iter().filter(|(_, l)| l.conn == Some(conn)).map(|(k, _)| k.clone()).collect();
        for key in mine {
            let l = g.live.get_mut(&key).expect("listed above");
            // Hooks still vouch for it: keep it, reachable through them.
            if l.last_hook.is_some() && l.reg.pid.is_some_and(util::pid_alive) {
                l.conn = None;
                continue;
            }
            if let Some(l) = g.live.remove(&key) {
                let _ = g.store.touch_session(&l.name, now);
                log(&format!("session {} left", self.addr(&l.name)));
                self.presence_changed();
            }
        }
    }

    /// Forgets hook-only sessions whose agent is gone or has been silent too long.
    pub(super) fn prune(&self, g: &mut Inner) {
        let now = util::now_ms();
        let dead: Vec<String> = g
            .live
            .iter()
            .filter(|(_, l)| {
                l.conn.is_none()
                    && (l.reg.pid.is_some_and(|p| !util::pid_alive(p))
                        || l.last_hook.is_none_or(|t| now - t > HOOK_TTL_MS))
            })
            .map(|(k, _)| k.clone())
            .collect();
        for key in dead {
            if let Some(l) = g.live.remove(&key) {
                let _ = g.store.touch_session(&l.name, now);
            }
        }
        g.off.retain(|_, (_, at)| now - *at <= HOOK_TTL_MS);
        if self.forgetting(now) {
            let live: HashSet<&str> = g.live.values().map(|l| l.name.as_str()).collect();
            let _ = g.store.forget_sessions(now - FORGET_MS, &live, &self.person);
        }
        // Files of messages that are gone, or were read a day ago.
        if now - g.swept > 10 * 60_000 {
            g.swept = now;
            // Live sessions are seen now: if this daemon stops without seeing them go, their names are kept from then.
            for l in g.live.values() {
                let _ = g.store.touch_session(&l.name, now);
            }
            if let Err(e) = g.store.sweep_files(now) {
                log(&format!("sweeping the inbox folder: {e:#}"));
            }
        }
    }
}

fn remembered(l: &Live, now: i64) -> Remembered {
    Remembered {
        name: l.name.clone(),
        key: l.reg.key.clone(),
        agent: l.reg.agent.clone(),
        cwd: l.reg.cwd.clone(),
        repo: l.git.repo.clone(),
        branch: l.git.branch.clone(),
        doing: l.doing.clone(),
        tier: l.reg.tier,
        last_seen: now,
        attached: l.attached,
    }
}

/// The key of a machine's own terminal mailbox (`<you>/terminal`).
const TERMINAL: &str = "cli:terminal";

/// What a machine's presence says when its engine shows a message from another machine's terminal as from
/// `<person>/terminal@<machine>`, so its agents' replies reach the one it came from.
pub(super) const TERMINALS: &str = "terminals";

/// My session names on my other machines.
#[derive(Default, Debug, PartialEq)]
struct Elsewhere {
    live: HashSet<String>,
    kept: HashSet<String>,
    /// Every teammate machine heard from routes replies to a terminal by machine (`TERMINALS`).
    terminals: bool,
}

/// The session names on `person`'s other machines, from their presence: live sessions', and those kept there for
/// sessions that are offline (older engines don't say which). Kept names count while that machine is online or was
/// in the last day: one off longer can't still be keeping them, and its last presence, loaded from disk, would
/// otherwise hold them for good.
fn names_elsewhere(
    remotes: &HashMap<EndpointId, Remote>,
    team: &TeamState,
    person: &str,
    me: &EndpointId,
    now: i64,
) -> Elsewhere {
    let mut out = Elsewhere { terminals: true, ..Default::default() };
    for m in team.active().filter(|m| m.endpoint != *me) {
        // A machine not heard from yet (offline since before this one joined) may be of an older engine.
        let r = remotes.get(&m.endpoint);
        out.terminals &= r.is_some_and(|r| r.presence.has(TERMINALS));
        let Some(r) = r else { continue };
        if m.person != person {
            continue;
        }
        out.live.extend(r.presence.sessions.iter().map(|s| s.name.clone()));
        if r.online || now - r.last_seen <= FORGET_MS {
            out.kept.extend(r.presence.kept.iter().cloned());
        }
    }
    out
}

/// The name of a session registering here. Its own from before (and the mail waiting for it), unless a live session
/// has that now; else the name it asks for or `<repo>-<agent>`, or the first `-2`, `-3`, … that is free: that no live
/// session has, here or on my other machines, and that isn't kept for another session that is offline (`holds`). One of
/// the person's own mailboxes (`listen --as`) takes the name it asks for whenever that is free.
fn pick_name(
    g: &Inner,
    reg: &SessionReg,
    git: &GitInfo,
    elsewhere: &Elsewhere,
    person: &str,
    holds: impl Fn(&Remembered) -> bool,
) -> Result<String> {
    // A machine's own `terminal` is its own: another machine's never makes it take a suffix, once every engine in the
    // team shows which machine a terminal's message came from. (An older one couldn't tell two apart in a reply.)
    let none = Elsewhere::default();
    let elsewhere = if reg.key == TERMINAL && elsewhere.terminals { &none } else { elsewhere };
    // The person's own name is never a session's (`<person>/<person>` would read as the person), as for a rename.
    let live =
        |n: &str| n == person || g.live.iter().any(|(k, l)| l.name == n && *k != reg.key) || elsewhere.live.contains(n);
    let free = |n: &str| -> Result<bool> {
        let kept_here = g.store.session_by_name(n)?.is_some_and(|r| r.key != reg.key && holds(&r));
        Ok(!live(n) && !kept_here && !elsewhere.kept.contains(n))
    };
    let asked = reg.name.as_deref().map(util::slug).filter(|s| !s.is_empty());
    // One of the person's own mailboxes is the name it asks for: once that is free again, it goes back to it (its mail
    // from the suffixed name comes along).
    if reg.key.starts_with("cli:")
        && let Some(n) = asked.as_deref()
        && free(n)?
    {
        return Ok(n.to_string());
    }
    if let Some(r) = g.store.session_by_key(&reg.key)?
        && !live(&r.name)
    {
        return Ok(r.name);
    }
    let base = asked.unwrap_or_else(|| agent::default_name(&reg.agent, &reg.cwd, git));
    if free(&base)? {
        return Ok(base);
    }
    for i in 2.. {
        let n = util::joined(&base, &i.to_string());
        if free(&n)? {
            return Ok(n);
        }
    }
    unreachable!("some suffix is free")
}

#[cfg(test)]
mod tests {
    use super::*;

    const DAY: i64 = 24 * 3_600_000;

    fn claude(id: &str) -> SessionReg {
        SessionReg {
            key: format!("claude-code:{id}"),
            agent: agent::CLAUDE_CODE.into(),
            cwd: "/work/api".into(),
            agent_session_id: Some(id.into()),
            pid: None,
            tier: Tier::Push,
            name: None,
        }
    }

    fn named(key: &str, agent: &str, name: Option<&str>) -> SessionReg {
        SessionReg {
            key: key.into(),
            agent: agent.into(),
            cwd: "/work/api".into(),
            agent_session_id: None,
            pid: None,
            tier: Tier::Pull,
            name: name.map(str::to_string),
        }
    }

    /// Registers a session through an MCP server of its own, as Claude Code runs one per session.
    fn join(d: &Daemon, reg: SessionReg) -> Registered {
        let conn = conn_of(&reg.key);
        d.register_quiet(Some(conn), reg, GitInfo::default()).unwrap().0
    }

    /// The connection of the MCP server `join` registers a session through.
    fn conn_of(key: &str) -> u64 {
        key.bytes().fold(7, |h: u64, b| h.wrapping_mul(31).wrapping_add(b.into()))
    }

    /// Takes a session here offline, last seen `ago` before now.
    fn leave(d: &Daemon, key: &str, ago: i64) {
        let mut g = d.inner.lock().unwrap();
        let name = g.live.remove(key).unwrap().name;
        g.store.touch_session(&name, util::now_ms() - ago).unwrap();
    }

    /// The daemon restarts: nothing is live until sessions register again.
    fn restart(d: &Daemon) {
        d.inner.lock().unwrap().live.clear();
    }

    /// Long enough after a start that sessions gone a day are forgotten.
    fn settled(d: &mut Daemon) {
        d.started -= HOOK_TTL_MS + 1;
    }

    fn mail(d: &Daemon, id: &str, name: &str) {
        let mut m = Message {
            id: id.into(),
            from: "sven/web-codex".into(),
            from_agent: None,
            from_repo: None,
            to: d.addr(name),
            via: None,
            reply_to: None,
            body: format!("for {name}"),
            wake: true,
            notify: false,
            sent_at: 0,
            approval: None,
            files: Vec::new(),
        };
        d.inner.lock().unwrap().store.insert(&mut m, false).unwrap();
    }

    #[test]
    fn a_conversation_gets_its_name_and_mail_back_after_a_restart_whoever_registers_first() {
        let d = super::super::test_daemon(Default::default());
        assert_eq!(join(&d, claude("a")).name, "api-claude");
        assert_eq!(join(&d, claude("b")).name, "api-claude-2");
        restart(&d);
        mail(&d, "m1", "api-claude");
        mail(&d, "m2", "api-claude-2");
        // A new conversation in the same folder comes back first: both names are kept, and it gets none of their mail.
        let new = join(&d, claude("c"));
        assert_eq!((new.name.as_str(), new.unread), ("api-claude-3", 0));
        // Then the two others, in the other order.
        let b = join(&d, claude("b"));
        assert_eq!((b.name.as_str(), b.unread), ("api-claude-2", 1));
        let a = join(&d, claude("a"));
        assert_eq!((a.name.as_str(), a.unread), ("api-claude", 1));
    }

    #[test]
    fn a_new_conversation_never_takes_the_name_or_mail_of_one_that_is_offline() {
        let d = super::super::test_daemon(Default::default());
        join(&d, claude("a"));
        leave(&d, "claude-code:a", 3 * 3_600_000);
        mail(&d, "m1", "api-claude");
        // Asking for the name doesn't get it either.
        let new = join(&d, named("cli:api-claude", "cli", Some("api-claude")));
        assert_eq!((new.name.as_str(), new.unread), ("api-claude-2", 0));
        let other = join(&d, claude("c"));
        assert_eq!((other.name.as_str(), other.unread), ("api-claude-3", 0));
        // Resumed hours later, the conversation is still api-claude, with its mail.
        let a = join(&d, claude("a"));
        assert_eq!((a.name.as_str(), a.unread), ("api-claude", 1));
    }

    #[tokio::test]
    async fn a_conversation_coming_back_through_its_hooks_gets_its_name_too() {
        let d = Arc::new(super::super::test_daemon(Default::default()));
        let call = |id: &str| HookCall {
            agent: "claude-code".into(),
            event: "start".into(),
            session_id: id.into(),
            cwd: Some("/work/api".into()),
            pid: None,
            mail: true,
            transcript: None,
            doing: None,
            name: None,
            dirs: Vec::new(),
        };
        // Its MCP server registered it; then it ends.
        assert_eq!(join(&d, claude("a")).name, "api-claude");
        d.hook(call("a")).await.unwrap();
        d.drop_conn(conn_of("claude-code:a"));
        d.hook(HookCall { event: "end".into(), ..call("a") }).await.unwrap();
        mail(&d, "m1", "api-claude");
        assert_eq!(d.hook(call("b")).await.unwrap().address, Some(d.addr("api-claude-2")));
        let back = d.hook(call("a")).await.unwrap();
        assert_eq!(back.address, Some(d.addr("api-claude")));
        assert!(back.text.is_some_and(|t| t.contains("for api-claude")));
    }

    #[test]
    fn a_name_is_kept_until_its_conversation_would_be_forgotten_and_then_comes_without_its_mail() {
        let mut d = super::super::test_daemon(Default::default());
        join(&d, claude("a"));
        leave(&d, "claude-code:a", DAY + 60_000);
        mail(&d, "m1", "api-claude");
        // Just after a start, sessions still up may not have registered again: nothing has lapsed yet.
        assert_eq!(join(&d, claude("b")).name, "api-claude-2");
        settled(&mut d);
        // A day on, the name is free again, but the mail stays the forgotten conversation's: it is dropped.
        let c = join(&d, claude("c"));
        assert_eq!((c.name.as_str(), c.unread), ("api-claude", 0));
        assert_eq!(d.inner.lock().unwrap().store.unread_count(&d.addr("api-claude")).unwrap(), 0);
        // Back after that, the conversation is a new session.
        assert_eq!(join(&d, claude("a")).name, "api-claude-3");
    }

    #[test]
    fn live_sessions_are_seen_as_they_run_so_a_restart_after_days_up_keeps_their_names() {
        let mut d = super::super::test_daemon(Default::default());
        settled(&mut d);
        join(&d, claude("a"));
        // Registered days ago, and still up.
        d.inner.lock().unwrap().store.touch_session("api-claude", util::now_ms() - 3 * DAY).unwrap();
        let mut g = d.inner.lock().unwrap();
        d.prune(&mut g);
        let r = g.store.session_by_name("api-claude").unwrap().unwrap();
        assert!(util::now_ms() - r.last_seen < 60_000);
    }

    #[test]
    fn renaming_into_a_name_kept_for_an_offline_session_is_refused() {
        let mut d = super::super::test_daemon(Default::default());
        join(&d, claude("a"));
        leave(&d, "claude-code:a", 3 * 3_600_000);
        mail(&d, "m1", "api-claude");
        join(&d, claude("b"));
        let e = d.set_status("claude-code:b", None, Some("API Claude".into())).unwrap_err();
        assert_eq!(
            e.to_string(),
            "'api-claude' belongs to a session that is offline (last seen 3h ago); pick another name"
        );
        // A name of its own is fine, and its own mail comes along.
        mail(&d, "m2", "api-claude-2");
        let r = d.set_status("claude-code:b", None, Some("auth".into())).unwrap();
        assert_eq!((r.name.as_str(), r.unread), ("auth", 1));
        // Once the offline one has lapsed, the name can be taken, without that session's mail.
        leave(&d, "claude-code:b", 0);
        join(&d, claude("b"));
        d.inner.lock().unwrap().store.touch_session("api-claude", util::now_ms() - 2 * DAY).unwrap();
        settled(&mut d);
        let r = d.set_status("claude-code:b", None, Some("api-claude".into())).unwrap();
        assert_eq!(r.name, "api-claude");
        let unread = d.inner.lock().unwrap().store.unread(&r.address).unwrap();
        assert_eq!(unread.iter().map(|m| m.id.as_str()).collect::<Vec<_>>(), ["m2"]);
    }

    #[test]
    fn the_persons_own_mailboxes_keep_their_names_for_good() {
        let mut d = super::super::test_daemon(Default::default());
        settled(&mut d);
        join(&d, named("cli:review", "cli", Some("review")));
        leave(&d, "cli:review", 30 * DAY);
        mail(&d, "m1", "review");
        // A Claude Code session someone named "review" can't have that mailbox.
        let c = join(&d, SessionReg { name: Some("Review".into()), ..claude("c") });
        assert_eq!((c.name.as_str(), c.unread), ("review-2", 0));
        let r = join(&d, named("cli:review", "cli", Some("review")));
        assert_eq!((r.name.as_str(), r.unread), ("review", 1));
    }

    #[test]
    fn a_session_known_only_by_its_process_leaves_its_name_and_mail_to_the_next_in_its_place() {
        let d = super::super::test_daemon(Default::default());
        assert_eq!(join(&d, named("proc:11", "zed", None)).name, "api-zed");
        leave(&d, "proc:11", 3 * 3_600_000);
        mail(&d, "m1", "api-zed");
        // As before: nothing could come back for it, so "tell the api agent" reaches the one there now.
        let next = join(&d, named("proc:12", "zed", None));
        assert_eq!((next.name.as_str(), next.unread), ("api-zed", 1));
        // But it can't take a name kept for a conversation.
        join(&d, claude("a"));
        leave(&d, "claude-code:a", 0);
        assert_eq!(join(&d, named("proc:13", "zed", Some("api-claude"))).name, "api-claude-2");
    }

    #[test]
    fn rows_an_older_engine_left_are_kept_for_their_sessions_and_others_work_as_before() {
        let d = super::super::test_daemon(Default::default());
        let row = |name: &str, key: &str| Remembered {
            name: name.into(),
            key: key.into(),
            agent: agent::CLAUDE_CODE.into(),
            cwd: "/work/api".into(),
            repo: None,
            branch: None,
            doing: None,
            tier: Tier::Push,
            last_seen: util::now_ms() - 3_600_000,
            attached: true,
        };
        {
            let g = d.inner.lock().unwrap();
            for (name, key) in
                [("api-claude", "claude-code:a"), ("api-claude-2", "claude-code:b"), ("api-claude-3", "proc:5")]
            {
                g.store.upsert_session(&row(name, key), &d.person).unwrap();
            }
        }
        for (id, name) in [("m1", "api-claude"), ("m2", "api-claude-2"), ("m3", "api-claude-3")] {
            mail(&d, id, name);
        }
        // Rows with an agent's id are kept for it from the start; one known by its process is taken, mail and all.
        let new = join(&d, claude("d"));
        assert_eq!((new.name.as_str(), new.unread), ("api-claude-3", 1));
        let b = join(&d, claude("b"));
        assert_eq!((b.name.as_str(), b.unread), ("api-claude-2", 1));
        let a = join(&d, claude("a"));
        assert_eq!((a.name.as_str(), a.unread), ("api-claude", 1));
    }

    #[test]
    fn a_session_whose_name_went_elsewhere_takes_its_mail_to_its_new_one() {
        let d = super::super::test_daemon(Default::default());
        join(&d, claude("a"));
        leave(&d, "claude-code:a", 3_600_000);
        mail(&d, "m1", "api-claude");
        // An older engine on another of my machines gave the name to a live session there.
        let elsewhere =
            Elsewhere { live: HashSet::from(["api-claude".to_string()]), kept: HashSet::new(), terminals: true };
        let mut g = d.inner.lock().unwrap();
        let g = &mut *g;
        let name = pick_name(g, &claude("a"), &GitInfo::default(), &elsewhere, &d.person, |_| true).unwrap();
        assert_eq!(name, "api-claude-2");
        let r = Remembered {
            name: name.clone(),
            key: "claude-code:a".into(),
            agent: agent::CLAUDE_CODE.into(),
            cwd: "/work/api".into(),
            repo: None,
            branch: None,
            doing: None,
            tier: Tier::Push,
            last_seen: util::now_ms(),
            attached: true,
        };
        g.store.upsert_session(&r, &d.person).unwrap();
        assert!(g.store.session_by_name("api-claude").unwrap().is_none());
        assert_eq!(g.store.unread_count(&d.addr("api-claude-2")).unwrap(), 1);
    }

    #[test]
    fn names_kept_on_my_other_machines_go_to_no_new_session_here() {
        let d = super::super::test_daemon(Default::default());
        let elsewhere = Elsewhere {
            live: HashSet::from(["api-claude".to_string()]),
            kept: HashSet::from(["api-claude-2".to_string()]),
            terminals: true,
        };
        let g = d.inner.lock().unwrap();
        assert_eq!(
            pick_name(&g, &claude("c"), &GitInfo::default(), &elsewhere, &d.person, |_| true).unwrap(),
            "api-claude-3"
        );
    }

    #[test]
    fn presence_tells_my_other_machines_which_names_are_kept() {
        let mut d = super::super::test_daemon(Default::default());
        settled(&mut d);
        join(&d, claude("a"));
        join(&d, named("cli:terminal", "cli", Some("terminal")));
        join(&d, named("proc:9", "zed", None));
        join(&d, claude("old"));
        join(&d, claude("up"));
        leave(&d, "claude-code:a", 3_600_000);
        leave(&d, "cli:terminal", 3 * DAY);
        leave(&d, "proc:9", 3_600_000);
        leave(&d, "claude-code:old", 2 * DAY);
        let p = d.presence_of(Vec::new());
        let mut known = p.known.clone();
        known.sort();
        assert_eq!(known, ["api-claude", "api-claude-2", "api-zed", "terminal"]);
        // Not each machine's own `terminal`, nor a session that can't come back, nor one that has lapsed.
        assert_eq!(p.kept, ["api-claude"]);
    }
    #[tokio::test]
    async fn a_run_only_its_hooks_ever_saw_keeps_no_name() {
        let d = Arc::new(super::super::test_daemon(Default::default()));
        let call = |id: &str, event: &str| HookCall {
            agent: "claude-code".into(),
            event: event.into(),
            session_id: id.into(),
            cwd: Some("/work/api".into()),
            pid: None,
            mail: true,
            transcript: None,
            doing: None,
            name: None,
            dirs: Vec::new(),
        };
        // A headless run: hooks only, never an MCP server, a push channel or a listen.
        assert_eq!(d.hook(call("p", "start")).await.unwrap().address, Some(d.addr("api-claude")));
        d.hook(call("p", "end")).await.unwrap();
        mail(&d, "m1", "api-claude");
        let r = d.inner.lock().unwrap().store.session_by_name("api-claude").unwrap().unwrap();
        assert!(!r.kept());
        // As for a session without an id: the next one in its place takes the name, mail and all.
        let next = join(&d, claude("q"));
        assert_eq!((next.name.as_str(), next.unread), ("api-claude", 1));
        // A run whose MCP server answers for it is reachable, so its name is kept from then on.
        assert_eq!(d.hook(call("r", "prompt")).await.unwrap().address, Some(d.addr("api-claude-2")));
        let mut cursor = call("r", "prompt");
        cursor.agent = "cursor".into();
        d.hook(cursor).await.unwrap();
        assert_eq!(d.attach("cursor", "/work/api").unwrap().as_deref(), Some("cursor:r"));
        assert!(d.inner.lock().unwrap().store.session_by_key("cursor:r").unwrap().unwrap().kept());
    }

    fn gemini(id: &str, event: &str) -> HookCall {
        HookCall {
            agent: "gemini".into(),
            event: event.into(),
            session_id: id.into(),
            cwd: Some("/work/api".into()),
            pid: None,
            mail: true,
            transcript: None,
            doing: None,
            name: None,
            dirs: Vec::new(),
        }
    }

    #[tokio::test]
    async fn a_session_its_hooks_hand_mail_to_keeps_its_name() {
        // Gemini is reached through its hooks only: no MCP connection or push channel ever.
        let d = Arc::new(super::super::test_daemon(Default::default()));
        assert_eq!(d.hook(gemini("g1", "start")).await.unwrap().address, Some(d.addr("api-gemini")));
        mail(&d, "m1", "api-gemini");
        let got = d.hook(gemini("g1", "stop")).await.unwrap();
        assert!(got.text.is_some_and(|t| t.contains("for api-gemini")));
        d.hook(gemini("g1", "end")).await.unwrap();
        mail(&d, "m2", "api-gemini");
        // A new conversation gets the next name, and none of g1's mail.
        let g2 = d.hook(gemini("g2", "start")).await.unwrap();
        assert_eq!((g2.address, g2.text.is_some()), (Some(d.addr("api-gemini-2")), false));
        // g1, resumed, is api-gemini again, with what came meanwhile.
        let back = d.hook(gemini("g1", "start")).await.unwrap();
        assert_eq!(back.address, Some(d.addr("api-gemini")));
        assert!(back.text.is_some_and(|t| t.contains("for api-gemini")));
    }

    #[tokio::test]
    async fn a_session_that_sends_or_is_sent_mail_keeps_its_name() {
        let d = Arc::new(super::super::test_daemon(Default::default()));
        d.hook(gemini("s", "start")).await.unwrap();
        let mut copilot = gemini("r", "start");
        copilot.agent = "copilot".into();
        d.hook(copilot.clone()).await.unwrap();
        // s writes to r, whose hooks haven't shown it yet.
        d.send("gemini:s".into(), &d.addr("api-copilot"), "hi", None, true, false, None, Vec::new()).await.unwrap();
        let g = d.inner.lock().unwrap();
        for key in ["gemini:s", "copilot:r"] {
            assert!(g.store.session_by_key(key).unwrap().unwrap().kept(), "{key}");
        }
    }

    #[tokio::test]
    async fn a_session_stays_reachable_once_it_has_been() {
        let d = Arc::new(super::super::test_daemon(Default::default()));
        d.hook(gemini("g1", "start")).await.unwrap();
        mail(&d, "m1", "api-gemini");
        d.hook(gemini("g1", "stop")).await.unwrap();
        d.hook(gemini("g1", "end")).await.unwrap();
        // Back through its hooks, with no mail this time, then gone again: still kept.
        d.hook(gemini("g1", "start")).await.unwrap();
        d.hook(gemini("g1", "end")).await.unwrap();
        assert!(d.inner.lock().unwrap().store.session_by_key("gemini:g1").unwrap().unwrap().kept());
        assert_eq!(d.hook(gemini("g2", "start")).await.unwrap().address, Some(d.addr("api-gemini-2")));
    }

    #[test]
    fn a_listen_mailbox_goes_back_to_the_name_it_asks_for_once_free() {
        let mut d = super::super::test_daemon(Default::default());
        join(&d, named("cli:review-x", "cli", Some("x")));
        join(&d, SessionReg { name: Some("review".into()), ..claude("a") });
        leave(&d, "claude-code:a", 3_600_000);
        // `listen --as review` while the name is kept for the conversation: the next free one.
        assert_eq!(join(&d, named("cli:review", "cli", Some("review"))).name, "review-2");
        leave(&d, "cli:review", 0);
        mail(&d, "m1", "review-2");
        assert_eq!(join(&d, named("cli:review", "cli", Some("review"))).name, "review-2");
        leave(&d, "cli:review", 0);
        // Once the conversation's name lapses, the mailbox takes it, and its mail comes along.
        d.inner.lock().unwrap().store.touch_session("review", util::now_ms() - 2 * DAY).unwrap();
        settled(&mut d);
        let r = join(&d, named("cli:review", "cli", Some("review")));
        assert_eq!((r.name.as_str(), r.unread), ("review", 1));
        assert!(d.inner.lock().unwrap().store.session_by_name("review-2").unwrap().is_none());
    }

    #[tokio::test]
    async fn mail_held_for_a_conversation_goes_with_it_and_never_to_the_next_session() {
        let cfg = crate::config::Config {
            person: Some("alex".into()),
            inbound: crate::config::Inbound { default: crate::config::Policy::Hold, ..Default::default() },
            ..Default::default()
        };
        let mut d = super::super::test_daemon(cfg);
        let bob = iroh::SecretKey::generate();
        let mut team = TeamState::found("acme", &d.person, &d.machine, &d.key);
        team.put_member(&d.key, bob.public(), "bob", "xps", true);
        d.inner.lock().unwrap().team = Some(team);
        let d = {
            join(&d, claude("a"));
            join(&d, named("proc:5", "zed", None));
            leave(&d, "claude-code:a", 0);
            leave(&d, "proc:5", 0);
            settled(&mut d);
            Arc::new(d)
        };
        let (h1, h2) = (ulid::Ulid::generate().to_string(), ulid::Ulid::generate().to_string());
        let from_bob = |id: &str, to: &str| Message {
            id: id.into(),
            from: "bob/web".into(),
            from_agent: None,
            from_repo: None,
            to: d.addr(to),
            via: None,
            reply_to: None,
            body: "hi".into(),
            wake: true,
            notify: false,
            sent_at: 0,
            approval: None,
            files: Vec::new(),
        };
        let (state, note) = d.receive(bob.public(), from_bob(&h1, "api-claude")).unwrap();
        assert_eq!(state, "held");
        assert_eq!(note, "held until alex approves it; dropped if that session is gone for a day first");
        // Held for a session without an id, it stays the person's to settle, for whoever has the name, as before.
        assert_eq!(d.receive(bob.public(), from_bob(&h2, "api-zed")).unwrap().1, "held until alex approves it");
        // A day on, both names lapse: the conversation's held mail goes with it.
        {
            let g = d.inner.lock().unwrap();
            g.store.touch_session("api-claude", util::now_ms() - 2 * DAY).unwrap();
            g.store.touch_session("api-zed", util::now_ms() - 2 * DAY).unwrap();
        }
        let next = join(&d, claude("b"));
        assert_eq!(next.name, "api-claude");
        let held: Vec<String> = d.inner.lock().unwrap().store.held().unwrap().into_iter().map(|m| m.id).collect();
        assert_eq!(held, [h2]);
        assert_eq!(d.settle(&[], true).unwrap(), 1);
        assert_eq!(d.inner.lock().unwrap().store.unread_count(&d.addr("api-claude")).unwrap(), 0);
    }

    #[test]
    fn names_elsewhere_are_my_other_machines_and_kept_ones_only_while_they_could_still_be_kept() {
        let d = super::super::test_daemon(crate::config::Config { person: Some("alex".into()), ..Default::default() });
        let (desk, old, bob) = (iroh::SecretKey::generate(), iroh::SecretKey::generate(), iroh::SecretKey::generate());
        let mut team = TeamState::found("acme", &d.person, &d.machine, &d.key);
        team.put_member(&d.key, desk.public(), "alex", "desk", true);
        team.put_member(&d.key, old.public(), "alex", "old", true);
        team.put_member(&d.key, bob.public(), "bob", "xps", true);
        let now = util::now_ms();
        let remote = |live: &str, kept: &str, online: bool, last_seen: i64| Remote {
            presence: crate::net::Presence {
                sessions: vec![SessionInfo {
                    address: String::new(),
                    name: live.into(),
                    person: String::new(),
                    machine: String::new(),
                    agent: "codex".into(),
                    cwd: String::new(),
                    repo: None,
                    branch: None,
                    status: "idle".into(),
                    since: None,
                    doing: None,
                    tier: Tier::Pull,
                    unread: 0,
                    is_me: false,
                }],
                known: vec![kept.into()],
                kept: vec![kept.into()],
                ..Default::default()
            },
            last_seen,
            online,
        };
        let remotes = HashMap::from([
            (desk.public(), remote("desk-live", "desk-kept", false, now - 3_600_000)),
            // Off for two days: what it kept then has lapsed by now.
            (old.public(), remote("old-live", "old-kept", false, now - 2 * DAY)),
            // Another person's names are theirs.
            (bob.public(), remote("bob-live", "bob-kept", true, now)),
        ]);
        let e = names_elsewhere(&remotes, &team, "alex", &d.key.public(), now);
        let set = |v: &[&str]| v.iter().map(|s| s.to_string()).collect::<HashSet<_>>();
        assert_eq!(e.live, set(&["desk-live", "old-live"]));
        assert_eq!(e.kept, set(&["desk-kept"]));
        // None of those engines said it tells terminals apart by machine.
        assert!(!e.terminals);
    }

    #[test]
    fn terminals_go_by_machine_only_once_every_machine_in_the_team_says_it_can() {
        let d = super::super::test_daemon(crate::config::Config { person: Some("alex".into()), ..Default::default() });
        let (desk, olga) = (iroh::SecretKey::generate(), iroh::SecretKey::generate());
        let mut team = TeamState::found("acme", &d.person, &d.machine, &d.key);
        team.put_member(&d.key, desk.public(), "alex", "desk", true);
        team.put_member(&d.key, olga.public(), "olga", "oldbox", true);
        let now = util::now_ms();
        let new = Remote {
            presence: crate::net::Presence { features: vec![TERMINALS.into()], ..Default::default() },
            last_seen: now,
            online: true,
        };
        let me = d.key.public();
        // olga's machine hasn't been heard from (it was off when this one joined): it may be an older engine.
        let remotes = HashMap::from([(desk.public(), new.clone())]);
        assert!(!names_elsewhere(&remotes, &team, "alex", &me, now).terminals);
        // Heard from, and an older engine.
        let old = Remote { presence: Default::default(), ..new.clone() };
        let remotes = HashMap::from([(desk.public(), new.clone()), (olga.public(), old)]);
        assert!(!names_elsewhere(&remotes, &team, "alex", &me, now).terminals);
        // Every other machine says it can (this one isn't asked).
        let remotes = HashMap::from([(desk.public(), new.clone()), (olga.public(), new)]);
        assert!(names_elsewhere(&remotes, &team, "alex", &me, now).terminals);
    }

    #[tokio::test]
    async fn a_terminal_takes_a_suffix_once_an_older_engine_turns_up() {
        let d = Arc::new(super::super::test_daemon(Default::default()));
        assert_eq!(join(&d, named(TERMINAL, "cli", Some("terminal"))).name, "terminal");
        mail(&d, "m1", "terminal");
        let mut others =
            Elsewhere { live: HashSet::from(["terminal".to_string()]), terminals: true, ..Default::default() };
        // Every engine tells two apart: it stays `terminal`.
        d.recheck_terminal(&others);
        assert_eq!(d.inner.lock().unwrap().live[TERMINAL].name, "terminal");
        // An older one is in the team now: the next free name, with its mail.
        others.terminals = false;
        d.recheck_terminal(&others);
        let g = d.inner.lock().unwrap();
        assert_eq!(g.live[TERMINAL].name, "terminal-2");
        assert_eq!(g.store.unread_count(&d.addr("terminal-2")).unwrap(), 1);
    }

    #[tokio::test]
    async fn a_terminal_rename_that_fails_never_fails_the_command() {
        let d = Arc::new(super::super::test_daemon(Default::default()));
        assert_eq!(join(&d, named(TERMINAL, "cli", Some("terminal"))).name, "terminal");
        let others =
            Elsewhere { live: HashSet::from(["terminal".to_string()]), terminals: false, ..Default::default() };
        // The rename loses a race: another session took the name it picked meanwhile.
        d.recheck_terminal_with(&others, |name| bail!("'{name}' is taken by a live session"));
        assert_eq!(d.inner.lock().unwrap().live[TERMINAL].name, "terminal");
        // The terminal's commands go on as before.
        let call = HookCall {
            agent: "cli".into(),
            event: "stop".into(),
            session_id: "terminal".into(),
            cwd: Some("/".into()),
            pid: None,
            mail: false,
            transcript: None,
            doing: None,
            name: Some("terminal".into()),
            dirs: Vec::new(),
        };
        assert_eq!(d.hook(call).await.unwrap().address, Some(d.addr("terminal")));
    }

    #[test]
    fn no_session_is_named_after_its_person() {
        let d = super::super::test_daemon(crate::config::Config { person: Some("lars".into()), ..Default::default() });
        // `listen --as lars`, and a Claude Code session someone named Lars.
        assert_eq!(join(&d, named("cli:lars", "cli", Some("lars"))).name, "lars-2");
        assert_eq!(join(&d, SessionReg { name: Some("Lars".into()), ..claude("a") }).name, "lars-3");
        // Nor a session coming back to such a name an older engine gave it.
        let r = Remembered {
            name: "lars".into(),
            key: "claude-code:old".into(),
            agent: agent::CLAUDE_CODE.into(),
            cwd: "/work/api".into(),
            repo: None,
            branch: None,
            doing: None,
            tier: Tier::Push,
            last_seen: util::now_ms(),
            attached: true,
        };
        d.inner.lock().unwrap().store.upsert_session(&r, &d.person).unwrap();
        assert_eq!(join(&d, claude("old")).name, "api-claude");
    }

    #[test]
    fn each_machine_has_its_own_terminal() {
        let d = super::super::test_daemon(Default::default());
        let mut elsewhere = Elsewhere {
            live: HashSet::from(["terminal".to_string()]),
            kept: HashSet::from(["review".to_string()]),
            terminals: false,
        };
        let g = d.inner.lock().unwrap();
        let terminal = named(TERMINAL, "cli", Some("terminal"));
        // While an older engine in the team couldn't tell two apart in a reply, the second one on is `terminal-2`.
        assert_eq!(
            pick_name(&g, &terminal, &GitInfo::default(), &elsewhere, &d.person, |_| true).unwrap(),
            "terminal-2"
        );
        elsewhere.terminals = true;
        assert_eq!(pick_name(&g, &terminal, &GitInfo::default(), &elsewhere, &d.person, |_| true).unwrap(), "terminal");
        // Any other session asking for it, or for a name kept elsewhere, still gets the next free one.
        let other = named("cli:t2", "cli", Some("terminal"));
        assert_eq!(pick_name(&g, &other, &GitInfo::default(), &elsewhere, &d.person, |_| true).unwrap(), "terminal-2");
        let review = named("cli:review", "cli", Some("review"));
        assert_eq!(pick_name(&g, &review, &GitInfo::default(), &elsewhere, &d.person, |_| true).unwrap(), "review-2");
    }

    #[test]
    fn presence_names_the_most_recent_kept_names_when_there_are_too_many() {
        let d = super::super::test_daemon(Default::default());
        let now = util::now_ms();
        {
            let g = d.inner.lock().unwrap();
            for i in 0..120 {
                let r = Remembered {
                    name: format!("s{i}"),
                    key: format!("codex:{i}"),
                    agent: "codex".into(),
                    cwd: "/work/api".into(),
                    repo: None,
                    branch: None,
                    doing: None,
                    tier: Tier::Pull,
                    last_seen: now - 1_000 * (120 - i),
                    attached: true,
                };
                g.store.upsert_session(&r, &d.person).unwrap();
            }
        }
        let p = d.presence_of(Vec::new());
        assert_eq!(p.kept.len(), 100);
        assert_eq!(
            (p.kept.first().map(String::as_str), p.kept.last().map(String::as_str)),
            (Some("s119"), Some("s20"))
        );
        assert_eq!(p.known, p.kept);
    }

    #[test]
    fn a_session_in_a_folder_that_is_off_never_registers() {
        let cfg = crate::config::Config {
            folders: [("/work/private".to_string(), "off".to_string())].into(),
            ..Default::default()
        };
        let d = super::super::test_daemon(cfg);
        let reg = |key: &str, cwd: &str| SessionReg {
            key: key.into(),
            agent: "codex".into(),
            cwd: cwd.into(),
            agent_session_id: None,
            pid: None,
            tier: Tier::Pull,
            name: None,
        };
        let err = d.register_quiet(None, reg("codex:1", "/work/private/notes"), GitInfo::default()).unwrap_err();
        assert_eq!(err.to_string(), OFF_HERE);
        let (r, _) = d.register_quiet(Some(1), reg("codex:2", "/work/private-api"), GitInfo::default()).unwrap();
        assert_eq!(r.name, "private-api-codex");
        let g = d.inner.lock().unwrap();
        assert_eq!(g.live.keys().collect::<Vec<_>>(), ["codex:2"]);
        assert!(g.store.session_by_key("codex:1").unwrap().is_none());
    }

    #[test]
    fn a_new_conversation_from_the_same_mcp_server_ends_the_one_before() {
        let d = super::super::test_daemon(Default::default());
        let reg = |key: &str, agent: &str| SessionReg {
            key: key.into(),
            agent: agent.into(),
            cwd: "/work/api".into(),
            agent_session_id: None,
            pid: None,
            tier: Tier::Pull,
            name: None,
        };
        // Claude Code's `/clear`: its MCP server now registers the new conversation.
        assert_eq!(
            d.register_quiet(Some(1), reg("claude-code:a", "claude-code"), GitInfo::default()).unwrap().0.name,
            "api-claude"
        );
        d.register_quiet(Some(2), reg("claude-code:other", "claude-code"), GitInfo::default()).unwrap();
        let (r, _) = d.register_quiet(Some(1), reg("claude-code:b", "claude-code"), GitInfo::default()).unwrap();
        // The cleared conversation keeps its name: `/resume` can bring it back.
        assert_eq!(r.name, "api-claude-3");
        assert!(d.inner.lock().unwrap().store.session_by_name("api-claude").unwrap().unwrap().kept());
        let live = |d: &Daemon| {
            let mut k: Vec<String> = d.inner.lock().unwrap().live.keys().cloned().collect();
            k.sort();
            k
        };
        assert_eq!(live(&d), ["claude-code:b", "claude-code:other"]);
        // The registry scan can register the new conversation before the server's next call does: its call then still
        // ends the one before.
        d.register_quiet(
            None,
            SessionReg { tier: Tier::Push, ..reg("claude-code:c", "claude-code") },
            GitInfo::default(),
        )
        .unwrap();
        d.register_quiet(Some(1), reg("claude-code:c", "claude-code"), GitInfo::default()).unwrap();
        assert_eq!(live(&d), ["claude-code:c", "claude-code:other"]);
        let g = d.inner.lock().unwrap();
        assert_eq!(g.live["claude-code:c"].conn, Some(1));
        drop(g);
        // One shared MCP server serves many sessions at once.
        d.register_quiet(Some(3), reg("codex:1", "codex"), GitInfo::default()).unwrap();
        d.register_quiet(Some(3), reg("codex:2", "codex"), GitInfo::default()).unwrap();
        assert_eq!(live(&d), ["claude-code:c", "claude-code:other", "codex:1", "codex:2"]);
    }

    #[tokio::test]
    async fn a_session_its_hooks_put_in_a_folder_that_is_off_never_gets_a_key() {
        let cfg = crate::config::Config {
            folders: [("/work/private".to_string(), "off".to_string())].into(),
            ..Default::default()
        };
        let d = Arc::new(super::super::test_daemon(cfg));
        let call = |id: &str, event: &str, dirs: &[&str]| HookCall {
            agent: "cursor".into(),
            event: event.into(),
            session_id: id.into(),
            cwd: Some("/work/api".into()),
            pid: None,
            mail: true,
            transcript: None,
            doing: None,
            name: None,
            dirs: dirs.iter().map(|d| d.to_string()).collect(),
        };
        // Working in /work/api, with /work/private open too.
        let e = d.hook(call("c1", "prompt", &["/work/private"])).await.unwrap_err();
        assert_eq!(e.to_string(), OFF_HERE);
        assert!(!d.inner.lock().unwrap().live.contains_key("cursor:c1"));
        // Its MCP server (shared, in another folder) can't register it either.
        let reg = SessionReg {
            key: "cursor:c1".into(),
            agent: "cursor".into(),
            cwd: "/work/api".into(),
            agent_session_id: None,
            pid: None,
            tier: Tier::Pull,
            name: None,
        };
        assert_eq!(d.register_quiet(Some(1), reg, GitInfo::default()).unwrap_err().to_string(), OFF_HERE);
        // Another session, mid-turn: while c1 is mid-turn too, a call naming no session can't be handed its key.
        assert!(d.hook(call("c2", "prompt", &[])).await.unwrap().address.is_some());
        assert_eq!(d.attach("cursor", "/work/api").unwrap_err().to_string(), BUSY_OFF);
        assert!(d.hook(call("c1", "stop", &["/work/private"])).await.is_err());
        assert_eq!(d.attach("cursor", "/work/api").unwrap().as_deref(), Some("cursor:c2"));
        // Once its folders are all on again, it is a session like any other.
        assert!(d.hook(call("c1", "prompt", &[])).await.unwrap().address.is_some());
        assert!(d.inner.lock().unwrap().live.contains_key("cursor:c1"));
    }
}
