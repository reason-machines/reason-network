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
    time::{Duration, Instant},
};

/// A hook-only session that hasn't fired a hook for this long is considered gone.
pub(super) const HOOK_TTL: Duration = Duration::from_secs(6 * 3600);
const HOOK_TTL_MS: i64 = HOOK_TTL.as_millis() as i64;
/// A session gone this long isn't coming back: it is forgotten, with the mail it never read.
pub(super) const FORGET_MS: i64 = 24 * 3_600_000;
/// A session in a folder that is off counts as mid-turn this long after its last prompt or tool call.
const OFF_TURN_MS: i64 = 10 * 60_000;

#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum Rename {
    Explicit,
    Automatic,
}

impl Daemon {
    /// How fast hooks let us reach an agent's session (`push_failed`: its push channel didn't work).
    fn hook_tier(&self, agent: &str, push_failed: bool) -> Tier {
        match agent {
            // Native Hermes queues push events, otherwise its explicit inbox tool is the reader.
            "hermes" => Tier::Pull,
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
        reg: SessionReg,
        git: GitInfo,
    ) -> Result<(Registered, (String, Vec<Message>))> {
        self.register_among(conn, reg, git, self.my_names_elsewhere(util::now_ms()))
    }

    /// `register_quiet`, with `others`, my session names on my other machines.
    fn register_among(
        &self,
        conn: Option<u64>,
        mut reg: SessionReg,
        git: GitInfo,
        others: Elsewhere,
    ) -> Result<(Registered, (String, Vec<Message>))> {
        // Every session comes in here: one in a folder that is off never does.
        if self.cfg.off_in(&reg.cwd) {
            bail!(OFF_HERE);
        }
        let now = util::now_ms();
        let mut g = self.inner.lock().unwrap();
        let g = &mut *g;
        let key = reg.key.clone();
        if reg.agent == "antigravity" && conn.is_some() && reg.tier == Tier::Push {
            let allowed =
                g.store.native_app_roots(&key)?.is_some_and(|roots| roots.iter().all(|root| !self.cfg.off_in(root)));
            if !allowed {
                bail!("Antigravity App binding is unavailable or includes a disabled workspace");
            }
        }
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
        // A Gemini window is in one conversation at a time: its hooks starting another (`/clear`, `/new`) end the one
        // before, which its MCP server, outliving it, may still have. Never one registered by process (an older MCP
        // server's).
        if conn.is_none() && agent::session_from_hooks(&reg.agent) && reg.pid.is_some() && !g.live.contains_key(&key) {
            let before: Vec<String> = g
                .live
                .iter()
                .filter(|(k, l)| l.reg.agent == reg.agent && l.reg.pid == reg.pid && !k.starts_with("proc:"))
                .map(|(k, _)| k.clone())
                .collect();
            self.moved_on(g, before, now);
        }
        if let Some(l) = g.live.get_mut(&key) {
            // Known already (e.g. from a hook, now the MCP server connects, or a reconnect). Asking for the name it
            // has settles it, as asking for any name does.
            if reg.name.as_deref().map(util::slug).is_some_and(|n| n == l.name) {
                g.store.settle_name(&key)?;
            }
            if conn.is_some() {
                l.conn = conn;
                l.attached = true;
                l.mcp_push = reg.tier == Tier::Push;
            }
            l.reg.tier = if l.push_failed { l.reg.tier.max(Tier::Turn) } else { l.reg.tier.min(reg.tier) };
            l.attached |= l.reg.tier == Tier::Push;
            if l.last_hook.is_none() {
                l.reg.cwd = reg.cwd;
                l.git = git;
            }
            l.reg.pid = reg.pid.or(l.reg.pid);
            l.reg.claude_dir = reg.claude_dir.or(l.reg.claude_dir.take());
        } else {
            if conn.is_none() {
                reg.tier = reg.tier.min(self.hook_tier(&reg.agent, false));
            }
            let name = pick_name(g, &reg, &git, &others, &self.person, |r| self.holds(r, now))?;
            // Its title named it before this daemon started (an update restarts it), or before it went offline: the
            // default name it went by is still its own (see `Live::former`), unless another session has taken it since.
            // By its key: also when it comes back under another name (its own was taken on another of my machines).
            let former = match g.store.former(&key)? {
                Some((f, told))
                    if f != name
                        && g.live_called(&f).is_none()
                        && g.store.session_by_name(&f)?.is_some_and(|o| o.key == key) =>
                {
                    Some((f, told))
                }
                _ => None,
            };
            if reg.name.is_some() {
                g.store.settle_name(&key)?;
            }
            self.take_over(g, &name, &key)?;
            let attached = conn.is_some() || reg.tier == Tier::Push || reg.agent == "cli";
            let mcp_push = conn.is_some() && reg.tier == Tier::Push;
            self.usage.count(
                "registered",
                crate::usage::Dimensions { source: crate::usage::family(&reg.agent), ..Default::default() },
                1,
            );
            if !self.forgetting() && !key.starts_with("proc:") {
                g.seen_since_start.insert(key.clone());
            }
            g.live.insert(
                key.clone(),
                Live {
                    through_reason: Some(conn.is_none() && reg.agent == "reason"),
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
                    mcp_push,
                    mailbox: key == TERMINAL,
                    codex_home: None,
                    introduced: false,
                    told_renamed: former.as_ref().is_some_and(|(_, told)| *told),
                    former: former.map(|(f, _)| f),
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
        self.recheck_terminal_with(others, |name| {
            self.set_status(TERMINAL, None, Some(name), Rename::Automatic).map(drop)
        });
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
        r.kept() && (r.mailbox || now - r.last_seen <= FORGET_MS || !self.forgetting())
    }

    /// Whether sessions gone a day are forgotten yet: not just after a start, while sessions that are still up haven't
    /// registered again (each gets the time a hook-only session has to show it is there).
    fn forgetting(&self) -> bool {
        Instant::now() >= self.startup_grace_ends
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

    /// Marks a live session as one of the person's own mailboxes, kept for good.
    fn mailbox(&self, key: &str, now: i64) -> Result<()> {
        let mut g = self.inner.lock().unwrap();
        let g = &mut *g;
        if let Some(l) = g.live.get_mut(key)
            && !l.mailbox
        {
            l.mailbox = true;
            g.store.upsert_session(&remembered(l, now), &self.person)?;
        }
        Ok(())
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
        g.store.give_up_former(name, key)
    }

    pub(super) fn set_status(
        &self,
        key: &str,
        doing: Option<String>,
        name: Option<String>,
        rename: Rename,
    ) -> Result<Registered> {
        self.set_status_quiet(key, doing, name, rename).map(|(r, _)| r)
    }

    /// A session still on its default name goes by `name`, from its conversation's title, unless it was named since
    /// (checked under the same lock as the rename, so a name someone gives it, or an earlier title's, wins). None when
    /// it is not renamed for that reason.
    pub(super) fn name_from_title(&self, key: &str, name: String) -> Result<Option<Registered>> {
        Ok(self.set_status_as(key, None, Some(name), Rename::Automatic, true)?.map(|(r, _)| r))
    }

    /// Also tells the caller to deliver pending mail after a reclaim, as registering does.
    /// A push session may never check an inbox or run another hook.
    pub(super) fn set_status_quiet(
        &self,
        key: &str,
        doing: Option<String>,
        name: Option<String>,
        rename: Rename,
    ) -> Result<(Registered, bool)> {
        self.set_status_as(key, doing, name, rename, false)?.ok_or_else(|| anyhow!("this session is not registered"))
    }

    fn set_status_as(
        &self,
        key: &str,
        doing: Option<String>,
        name: Option<String>,
        rename: Rename,
        from_title: bool,
    ) -> Result<Option<(Registered, bool)>> {
        let now = util::now_ms();
        let others = self.my_names_elsewhere(now);
        let mut g = self.inner.lock().unwrap();
        let g = &mut *g;
        if from_title && !g.live.get(key).is_some_and(|l| title_may_name(g, key, l).unwrap_or(false)) {
            return Ok(None);
        }
        let new_name = name.as_deref().map(util::slug);
        let mut reclaim = false;
        if let Some(n) = new_name.as_deref() {
            if n.is_empty() {
                bail!("a name needs letters or digits");
            }
            if g.live.iter().any(|(k, l)| l.goes_by(n) && k != key) || others.live.contains(n) {
                bail!("'{n}' is taken by a live session");
            }
            if n == self.person {
                bail!("'{n}' is your person name; pick another session name");
            }
            if others.kept.contains(n) {
                bail!("'{n}' belongs to a session on another of your machines that is offline; pick another name");
            }
            if let Some(r) = g.store.session_by_name(n)?.filter(|r| r.key != key && self.holds(r, now)) {
                if rename == Rename::Automatic || r.mailbox || key.starts_with("proc:") || key.starts_with("cli:") {
                    bail!(
                        "'{n}' belongs to a session that is offline (last seen {} ago); pick another name",
                        util::ago(r.last_seen, now)
                    );
                }
                let remaining = self.startup_grace_ends.saturating_duration_since(Instant::now());
                if !remaining.is_zero() && !g.seen_since_start.contains(&r.key) {
                    let minutes = remaining.as_nanos().div_ceil(Duration::from_secs(60).as_nanos());
                    let unit = if minutes == 1 { "minute" } else { "minutes" };
                    bail!(
                        "'{n}' has not reconnected since the daemon restart. Resume the original conversation \
                         so it reconnects, then end it and retry. Otherwise, try again in about {minutes} {unit}."
                    );
                }
                reclaim = true;
            }
        }
        if let Some(n) = new_name.as_deref()
            && !reclaim
            && g.live.get(key).is_some_and(|l| l.name != n)
        {
            self.take_over(g, n, key)?;
        }
        if new_name.is_some() {
            g.store.settle_name(key)?;
        }
        if let Some(n) = new_name.as_deref()
            && reclaim
        {
            // A name reclaimed from a titled session's old one is no longer that session's.
            g.store.give_up_former(n, key)?;
        }
        let live = g.live.get_mut(key).ok_or_else(|| anyhow!("this session is not registered"))?;
        if let Some(n) = new_name
            && n != live.name
        {
            g.store.rename(&self.person, &live.name, &n, reclaim.then_some(key))?;
            live.attached |= reclaim;
            let old = std::mem::replace(&mut live.name, n);
            // Named by its title: the name it was told at its start still reaches it, and its hooks tell it the new one.
            if from_title {
                live.former = Some(old);
                live.told_renamed = false;
            } else if live.former.as_deref() == Some(live.name.as_str()) {
                live.former = None;
            }
        }
        if let Some(d) = doing {
            let d = crate::team::without_tickets(d.trim());
            live.doing = (!d.is_empty()).then(|| d.chars().take(200).collect());
        }
        g.store.upsert_session(&remembered(live, now), &self.person)?;
        g.store.set_former(&live.name, live.former.as_deref(), live.told_renamed)?;
        let address = self.addr(&live.name);
        let name = live.name.clone();
        let unread = if reclaim {
            self.arrived.notify_waiters();
            self.presence_changed();
            g.pending(&address)?.len() as u32
        } else {
            g.store.unread_count(&address)?
        };
        Ok(Some((Registered { name, unread, address }, reclaim)))
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
        let HookCall {
            agent,
            event,
            session_id,
            cwd,
            pid,
            mail,
            transcript,
            doing,
            name,
            dirs,
            keep,
            through_reason,
            codex_home,
        } = h;
        let (agent, event) = (agent.as_str(), event.as_str());
        let key = format!("{agent}:{session_id}");
        let now = util::now_ms();
        let mut native_dirs: Vec<String> = cwd.iter().chain(&dirs).cloned().collect();
        // In a folder that is off, by any folder the agent names: never a session here, only noted as off.
        if cwd.iter().chain(&dirs).any(|d| self.cfg.off_in(d)) {
            let mut g = self.inner.lock().unwrap();
            if agent == "antigravity" {
                g.store.remember_native_app(&key, None, &[])?;
            }
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
            if agent == "antigravity" {
                g.store.remember_native_app(&key, None, &[])?;
            }
            // Gemini's MCP server outlives a conversation (`/clear`): its hooks say when one ends.
            if g.live
                .get(&key)
                .is_some_and(|l| l.conn.is_none() || agent::session_from_hooks(agent) || agent == "antigravity")
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
                claude_dir: None,
            };
            self.register_quiet(None, reg, git)?;
        }
        // A name the person gave with `listen --as`: one of their own mailboxes from now on.
        if keep && agent == "cli" {
            self.mailbox(&key, now)?;
        }

        // Read a Codex rollout that is new here now, not later while holding the lock (see `deliver`).
        if agent == "codex"
            && let Some(t) = &transcript
        {
            crate::codex::turn_running(std::path::Path::new(t));
        }
        let mut g = self.inner.lock().unwrap();
        let g = &mut *g;
        // A Codex thread that starts again gets another try at `codex queue` once it is offline again.
        if event == "start" {
            g.codex_failed.remove(&key);
        }
        let Some(l) = g.live.get_mut(&key) else { return Ok(HookReply::default()) };
        if let Some(t) = transcript {
            l.transcript = Some(t.into());
        }
        // Kept with the session too: mail reaches it through that home while it is offline.
        if agent == "codex" && l.codex_home != codex_home {
            l.codex_home = codex_home;
            g.store.remember_codex_home(&key, l.codex_home.as_deref())?;
        }
        // Only a report: it says nothing about the session's activity or how to reach it.
        if event == "doing" {
            if let Some(d) = doing {
                let d = crate::team::without_tickets(d.trim());
                l.doing = Some(d.chars().take(200).collect::<String>()).filter(|d| !d.is_empty());
            }
            return Ok(HookReply::default());
        }
        l.through_reason = through_reason;
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
        let native_app = agent == "antigravity"
            && l.transcript.as_ref().is_some_and(|p| crate::antigravity::app_transcript(&p.to_string_lossy()));
        if native_app {
            l.reg.pid = None;
            if !native_dirs.contains(&l.reg.cwd) {
                native_dirs.push(l.reg.cwd.clone());
            }
            g.store.remember_native_app(&key, Some(now), &native_dirs)?;
        }
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
        let mut text = (!msgs.is_empty()).then(|| render::for_agent(&msgs, Some(&agent_kind), false));
        // Its title named it since it was told who it is: say the new name once, as it starts a turn (a stop's or a
        // tool's output may not reach the model, or would keep it going).
        if matches!(event, "start" | "prompt")
            && let Some(l) = g.live.get_mut(&key)
            && let Some(former) = l.former.clone()
            && !std::mem::replace(&mut l.told_renamed, true)
        {
            // Told once, after a restart too.
            let name = l.name.clone();
            g.store.set_former(&name, Some(&former), true)?;
            let note = format!(
                "Reason Network: you are {address} now, named after what this chat is about. Mail to {} still reaches \
                 you; give teammates your new address.",
                self.addr(&former)
            );
            text = Some(match text {
                Some(t) => format!("{note}\n\n{t}"),
                None => note,
            });
        }
        // Say who it is once: at session start, or for Antigravity (whose start may say nothing) its first invocation.
        let intro = text.is_none()
            && if agent == "antigravity" {
                matches!(event, "start" | "prompt")
                    && g.live.get_mut(&key).is_some_and(|l| !std::mem::replace(&mut l.introduced, true))
            } else {
                event == "start"
            };
        Ok(HookReply { address: Some(address), text, ids, intro: Some(intro) })
    }

    /// Refuses as registering `key` would for a folder that is off, without registering it (`mcp --no-register`). With
    /// no key, an agent with one MCP server for all its sessions may be calling from one in an off folder: refused
    /// while one is mid-turn, as `attach` is.
    pub(super) fn off_for(&self, agent: &str, key: Option<&str>) -> Result<()> {
        let mut g = self.inner.lock().unwrap();
        self.prune(&mut g);
        match key {
            Some(key) => {
                let cwd_off = g.live.get(key).is_some_and(|l| self.cfg.off_in(&l.reg.cwd));
                if cwd_off || g.off.contains_key(key) {
                    bail!(OFF_HERE);
                }
            }
            None if agent::multiplexed(agent) => {
                let (now, prefix) = (util::now_ms(), format!("{agent}:"));
                if g.off.iter().any(|(k, (busy, at))| *busy && now - at < OFF_TURN_MS && k.starts_with(&prefix)) {
                    bail!(BUSY_OFF);
                }
            }
            None => {}
        }
        Ok(())
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

    /// The session id `agent`'s live process `pid` is in, as its hooks registered it: for its MCP server, which the
    /// agent gives no session id (Gemini). None unless exactly one session is its process's.
    pub(super) fn linked(&self, agent: &str, pid: u32) -> Option<String> {
        if !agent::session_from_hooks(agent) || !util::pid_alive(pid) {
            return None;
        }
        let mut g = self.inner.lock().unwrap();
        self.prune(&mut g);
        let mut ids = g
            .live
            .values()
            .filter(|l| l.reg.agent == agent && l.reg.pid == Some(pid) && !l.reg.key.starts_with("proc:"))
            .map(|l| l.reg.agent_session_id.clone());
        match (ids.next(), ids.next()) {
            (Some(id), None) => id,
            _ => None,
        }
    }

    pub(super) fn drop_conn(&self, conn: u64) {
        let now = util::now_ms();
        let mut g = self.inner.lock().unwrap();
        g.conns.remove(&conn);
        if g.antigravity_conn == Some(conn) {
            g.antigravity_conn = None;
        }
        let mine: Vec<String> = g.live.iter().filter(|(_, l)| l.conn == Some(conn)).map(|(k, _)| k.clone()).collect();
        for key in mine {
            let l = g.live.get_mut(&key).expect("listed above");
            // Hooks still vouch for it: keep it, reachable through them.
            if l.last_hook.is_some()
                && (l.reg.pid.is_some_and(util::pid_alive)
                    || l.reg.agent == "antigravity"
                        && l.reg.pid.is_none()
                        && l.last_hook.is_some_and(|at| now - at < HOOK_TTL_MS))
            {
                l.conn = None;
                if l.reg.agent == "antigravity" {
                    l.reg.tier = Tier::Turn;
                    l.mcp_push = false;
                }
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
        if self.forgetting() {
            g.seen_since_start.clear();
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

impl Live {
    /// Whether `name` reaches this session: its name, or the default one it went by before its title named it.
    pub(super) fn goes_by(&self, name: &str) -> bool {
        self.name == name || self.former.as_deref() == Some(name)
    }
}

impl Inner {
    /// The live session `name` reaches (see [`Live::goes_by`]), its own name first.
    pub(super) fn live_called(&self, name: &str) -> Option<String> {
        let (named, former): (Vec<_>, Vec<_>) =
            self.live.iter().filter(|(_, l)| l.goes_by(name)).partition(|(_, l)| l.name == name);
        named.into_iter().chain(former).next().map(|(k, _)| k.clone())
    }
}

/// Whether a title may still name this session: a Claude Code conversation (whose title Claude Code writes; not one of
/// the person's mailboxes or a `cli:` one), on the name it got for asking none, and its name is not settled: never named (by someone or by an
/// earlier title), and no mail went to or from it, since a reply goes to the name the mail had.
pub(super) fn title_may_name(g: &Inner, key: &str, l: &Live) -> Result<bool> {
    Ok(l.reg.agent == agent::CLAUDE_CODE
        && !l.mailbox
        && !key.starts_with("cli:")
        && agent::is_default_name(&l.name, &l.reg.agent, &l.reg.cwd, &l.git)
        && !g.store.name_settled(key)?)
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
        mailbox: l.mailbox,
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
    let live = |n: &str| {
        n == person || g.live.iter().any(|(k, l)| l.goes_by(n) && *k != reg.key) || elsewhere.live.contains(n)
    };
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
            claude_dir: None,
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
            claude_dir: None,
        }
    }

    #[test]
    fn registering_again_without_a_claude_dir_keeps_the_one_it_had() {
        let d = super::super::test_daemon(Default::default());
        let b = SessionReg { claude_dir: Some("/home/u/.claude-b".into()), ..claude("b") };
        d.register_quiet(Some(1), b, GitInfo::default()).unwrap();
        // A hook or the scan registers it again, knowing nothing of its Claude directory.
        d.register_quiet(None, claude("b"), GitInfo::default()).unwrap();
        let g = d.inner.lock().unwrap();
        assert_eq!(g.live["claude-code:b"].reg.claude_dir.as_deref(), Some("/home/u/.claude-b"));
    }

    /// Registers a session through an MCP server of its own, as Claude Code runs one per session; a `cli` one as
    /// `listen --as` does, as one of the person's own mailboxes.
    fn join(d: &Daemon, reg: SessionReg) -> Registered {
        let (conn, key, cli) = (conn_of(&reg.key), reg.key.clone(), reg.agent == "cli");
        let r = d.register_quiet(Some(conn), reg, GitInfo::default()).unwrap().0;
        if cli {
            d.mailbox(&key, util::now_ms()).unwrap();
        }
        r
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
        let mut g = d.inner.lock().unwrap();
        g.live.clear();
        g.seen_since_start.clear();
    }

    /// Long enough after a start that sessions gone a day are forgotten.
    fn settled(d: &mut Daemon) {
        d.startup_grace_ends = Instant::now();
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
    async fn a_pasted_invite_line_never_reaches_teammates_through_what_a_session_is_doing() {
        let cfg = crate::config::Config { person: Some("alex".into()), ..Default::default() };
        let d = Arc::new(super::super::test_daemon(cfg));
        let mut team = crate::team::TeamState::found("acme", &d.person, &d.machine, &d.key);
        team.new_link(&d.key);
        let secret = team.link_secret().expect("a team link").to_string();
        d.inner.lock().unwrap().team = Some(team);
        let l = d.link(false).await.unwrap();
        let line = crate::invite_line(&l.url);
        // `status <doing>` from an agent's shell; a `doing` hook; a session kept by a build that stored the line.
        join(&d, claude("a"));
        d.set_status("claude-code:a", Some(format!("{line} then fix CI")), None, Rename::Explicit).unwrap();
        join(&d, claude("b"));
        let doing = HookCall {
            through_reason: Some(false),
            agent: "claude-code".into(),
            event: "doing".into(),
            session_id: "b".into(),
            cwd: Some("/work/api".into()),
            pid: None,
            mail: false,
            transcript: None,
            doing: Some(line.clone()),
            name: None,
            dirs: Vec::new(),
            keep: false,
            codex_home: None,
        };
        d.hook(doing).await.unwrap();
        let old = Remembered {
            name: "old".into(),
            key: "claude-code:old".into(),
            agent: "claude-code".into(),
            cwd: "/work/api".into(),
            repo: None,
            branch: None,
            doing: Some(line.clone()),
            tier: Tier::Push,
            last_seen: util::now_ms() - 60_000,
            attached: true,
            mailbox: false,
        };
        d.inner.lock().unwrap().store.upsert_session(&old, &d.person).unwrap();

        let dir = d.who(None, None, true).await.unwrap();
        let presence = serde_json::to_string(&d.presence().await).unwrap();
        let stored = format!("{:?}", d.inner.lock().unwrap().store.session_by_name("api-claude").unwrap());
        let agent = crate::render::directory_for_agent(&dir);
        for out in [&serde_json::to_string(&dir).unwrap(), &agent, &presence, &stored] {
            assert!(!out.contains("rn1-") && !out.contains(&l.ticket) && !out.contains(&secret), "{out}");
        }
        let doings: Vec<_> = dir.sessions.iter().filter_map(|s| s.doing.as_deref()).collect();
        assert_eq!(
            doings.iter().filter(|d| d.starts_with("Join the Reason Network: (team link)")).count(),
            3,
            "{doings:?}"
        );
        assert!(agent.contains("doing: Join the Reason Network: (team link) then fix CI"), "{agent}");
    }

    #[tokio::test]
    async fn a_conversation_coming_back_through_its_hooks_gets_its_name_too() {
        let d = Arc::new(super::super::test_daemon(Default::default()));
        let call = |id: &str| HookCall {
            through_reason: Some(false),
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
            keep: false,
            codex_home: None,
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

    #[tokio::test]
    async fn an_antigravity_conversation_hears_who_it_is_once_not_before_every_invocation() {
        let d = Arc::new(super::super::test_daemon(Default::default()));
        let call = |event: &str| HookCall {
            through_reason: Some(false),
            agent: "antigravity".into(),
            event: event.into(),
            session_id: "c1".into(),
            cwd: Some("/work/api".into()),
            pid: None,
            mail: true,
            transcript: None,
            doing: None,
            name: None,
            codex_home: None,
            dirs: Vec::new(),
            keep: false,
        };
        let mut seen = Vec::new();
        for event in ["start", "prompt", "tool", "prompt", "stop", "prompt", "start"] {
            seen.push(d.hook(call(event)).await.unwrap().intro);
        }
        assert_eq!(seen, [Some(true), Some(false), Some(false), Some(false), Some(false), Some(false), Some(false)]);
        // Mail it is handed is not the introduction, so that doesn't use it up.
        let call = |event: &str| HookCall { session_id: "c2".into(), ..call(event) };
        d.hook(call("tool")).await.unwrap();
        let name = d.inner.lock().unwrap().live["antigravity:c2"].name.clone();
        mail(&d, "m1", &name);
        let handed = d.hook(call("prompt")).await.unwrap();
        assert!(handed.text.is_some());
        assert_eq!(handed.intro, Some(false));
        let next = d.hook(call("prompt")).await.unwrap();
        assert!(next.text.is_none());
        assert_eq!(next.intro, Some(true));
        assert_eq!(d.hook(call("prompt")).await.unwrap().intro, Some(false));
        // Other agents are told at session start, as before.
        let claude = |event: &str| HookCall { agent: "claude-code".into(), session_id: "c3".into(), ..call(event) };
        let mut seen = Vec::new();
        for event in ["start", "prompt", "tool", "stop", "start"] {
            seen.push(d.hook(claude(event)).await.unwrap().intro == Some(true));
        }
        assert_eq!(seen, [true, false, false, false, true]);
    }

    #[tokio::test]
    async fn a_hermes_conversations_hooks_never_promise_delivery_at_its_next_turn() {
        let d = Arc::new(super::super::test_daemon(Default::default()));
        // Its plugin registers it pull (wake off), and calls hooks only for activity: they never carry mail.
        join(&d, named("hermes:p:1", "hermes", None));
        for event in ["prompt", "tool", "stop"] {
            d.hook(HookCall {
                through_reason: Some(false),
                agent: "hermes".into(),
                event: event.into(),
                session_id: "p:1".into(),
                cwd: Some("/work/api".into()),
                pid: None,
                mail: false,
                transcript: None,
                doing: None,
                name: None,
                dirs: Vec::new(),
                keep: false,
                codex_home: None,
            })
            .await
            .unwrap();
            assert_eq!(d.inner.lock().unwrap().live["hermes:p:1"].reg.tier, Tier::Pull, "after {event}");
        }
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
    fn explicit_reclaim_keeps_both_inboxes_for_the_new_conversation() {
        let d = super::super::test_daemon(Default::default());
        join(&d, claude("a"));
        leave(&d, "claude-code:a", 0);
        mail(&d, "before", "api-claude");
        // A hook-only conversation with a durable id, not previously reachable.
        let caller = named("gemini:b", "gemini", None);
        let target = "api-claude";
        let own = d.register_quiet(None, caller.clone(), GitInfo::default()).unwrap().0;
        mail(&d, "own", &own.name);
        let r = d.set_status(&caller.key, None, Some(target.into()), Rename::Explicit).unwrap();
        assert_eq!((r.name.as_str(), r.unread), (target, 2));
        leave(&d, &caller.key, 0);
        let back = join(&d, claude("a"));
        assert_ne!(back.name, target);
        assert_eq!(back.unread, 0);
        let back = join(&d, caller);
        assert_eq!((back.name.as_str(), back.unread), (target, 2));
        let g = d.inner.lock().unwrap();
        assert!(g.store.session_by_name(target).unwrap().unwrap().kept());
        let msgs = g.store.unread(&r.address).unwrap();
        assert_eq!(msgs.iter().map(|m| m.id.as_str()).collect::<Vec<_>>(), ["before", "own"]);
    }

    #[test]
    fn reclaim_waits_for_an_unseen_owner_after_restart() {
        let mut d = super::super::test_daemon(Default::default());
        join(&d, claude("a"));
        restart(&d);
        join(&d, claude("b"));
        let rename = |d: &Daemon| d.set_status("claude-code:b", None, Some("api-claude".into()), Rename::Explicit);
        d.startup_grace_ends = Instant::now() + Duration::from_secs(17 * 60 + 30);
        let error = rename(&d).unwrap_err().to_string();
        assert!(error.contains("try again in about 18 minutes"), "{error}");
        join(&d, claude("a"));
        assert!(rename(&d).unwrap_err().to_string().contains("live"));
        leave(&d, "claude-code:a", 0);
        restart(&d);
        join(&d, claude("b"));
        d.startup_grace_ends = Instant::now() + Duration::from_secs(30);
        let error = rename(&d).unwrap_err().to_string();
        assert!(error.ends_with("try again in about 1 minute."), "{error}");
        settled(&mut d);
        assert_eq!(rename(&d).unwrap().name, "api-claude");
    }

    #[test]
    fn only_a_stable_session_can_reclaim_and_permanent_mailboxes_cannot_be_taken() {
        let d = super::super::test_daemon(Default::default());
        join(&d, claude("a"));
        leave(&d, "claude-code:a", 0);
        join(&d, named("proc:7", "zed", None));
        assert!(d.set_status("proc:7", None, Some("api-claude".into()), Rename::Explicit).is_err());
        join(&d, named(TERMINAL, "cli", Some("terminal")));
        assert!(d.set_status(TERMINAL, None, Some("api-claude".into()), Rename::Explicit).is_err());
        join(&d, claude("b"));
        for name in ["terminal", "review"] {
            let key = format!("cli:{name}");
            join(&d, named(&key, "cli", Some(name)));
            leave(&d, &key, 0);
            assert!(d.set_status("claude-code:b", None, Some(name.into()), Rename::Explicit).is_err());
        }
        assert!(d.set_status("claude-code:b", None, Some(d.person.clone()), Rename::Explicit).is_err());
    }

    #[test]
    fn automatic_renames_cannot_take_a_held_name() {
        let mut d = super::super::test_daemon(Default::default());
        join(&d, claude("a"));
        leave(&d, "claude-code:a", 3 * 3_600_000);
        mail(&d, "m1", "api-claude");
        join(&d, claude("b"));
        let e = d.set_status("claude-code:b", None, Some("API Claude".into()), Rename::Automatic).unwrap_err();
        assert_eq!(
            e.to_string(),
            "'api-claude' belongs to a session that is offline (last seen 3h ago); pick another name"
        );
        // A name of its own is fine, and its own mail comes along.
        mail(&d, "m2", "api-claude-2");
        let r = d.set_status("claude-code:b", None, Some("auth".into()), Rename::Explicit).unwrap();
        assert_eq!((r.name.as_str(), r.unread), ("auth", 1));
        // Once the offline one has lapsed, the name can be taken, without that session's mail.
        leave(&d, "claude-code:b", 0);
        join(&d, claude("b"));
        d.inner.lock().unwrap().store.touch_session("api-claude", util::now_ms() - 2 * DAY).unwrap();
        settled(&mut d);
        let r = d.set_status("claude-code:b", None, Some("api-claude".into()), Rename::Explicit).unwrap();
        assert_eq!(r.name, "api-claude");
        let unread = d.inner.lock().unwrap().store.unread(&r.address).unwrap();
        assert_eq!(unread.iter().map(|m| m.id.as_str()).collect::<Vec<_>>(), ["m2"]);
    }

    #[tokio::test]
    async fn a_relay_rename_without_reclaim_leaves_the_offline_owners_mail_alone() {
        let d = Arc::new(super::super::test_daemon(Default::default()));
        let owner = SessionReg { name: Some("cursor".into()), ..claude("a") };
        join(&d, owner.clone());
        leave(&d, &owner.key, 0);
        mail(&d, "queued", "cursor");

        let key = "reason:hosted";
        assert_eq!(join(&d, named(key, "reason", Some("cursor"))).name, "cursor-2");
        // The Reason relay periodically retries its preferred name with this request.
        let req =
            serde_json::from_value(serde_json::json!({"op": "set_status", "key": key, "name": "cursor"})).unwrap();
        let error = d.handle(conn_of(key), req).await.unwrap_err();
        assert!(error.to_string().contains("belongs to a session that is offline"), "{error}");
        assert!(d.inbox(key.into(), 0, true).await.unwrap().messages.is_empty());
        let back = join(&d, owner);
        assert_eq!((back.name.as_str(), back.unread), ("cursor", 1));
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

    #[tokio::test]
    async fn the_reason_clis_one_off_ask_sessions_are_sessions_like_any_other() {
        let mut d = super::super::test_daemon(Default::default());
        settled(&mut d);
        let d = Arc::new(d);
        let cli = |id: &str, keep: bool| HookCall {
            through_reason: Some(false),
            agent: "cli".into(),
            event: "stop".into(),
            session_id: id.into(),
            cwd: Some("/work/api".into()),
            pid: None,
            mail: false,
            transcript: None,
            doing: None,
            name: Some(id.into()),
            dirs: Vec::new(),
            keep,
            codex_home: None,
        };
        // `reason network <who> "…"` waits for its answer at a session of its own; `listen --as review` is a mailbox.
        d.hook(cli("ask-1a2b3c", false)).await.unwrap();
        d.hook(cli("review", true)).await.unwrap();
        for key in ["cli:ask-1a2b3c", "cli:review"] {
            leave(&d, key, 2 * DAY);
        }
        mail(&d, "m1", "ask-1a2b3c");
        mail(&d, "m2", "review");
        // Neither is named to the person's other machines (each machine has its own).
        assert!(d.presence_of(Vec::new()).kept.is_empty());
        // A day on, the ask session is forgotten with its mail, as any session is; the mailbox stays, mail and all.
        {
            let mut g = d.inner.lock().unwrap();
            d.prune(&mut g);
            assert!(g.store.session_by_key("cli:ask-1a2b3c").unwrap().is_none());
            assert_eq!(g.store.unread_count(&d.addr("ask-1a2b3c")).unwrap(), 0);
            assert!(g.store.session_by_key("cli:review").unwrap().is_some_and(|r| r.mailbox));
            assert_eq!(g.store.unread_count(&d.addr("review")).unwrap(), 1);
        }
        // The terminal is always one of the person's own mailboxes, whoever registers it.
        d.hook(cli("terminal", false)).await.unwrap();
        assert!(d.inner.lock().unwrap().store.session_by_key(TERMINAL).unwrap().is_some_and(|r| r.mailbox));
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
            mailbox: false,
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
            mailbox: false,
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
            through_reason: Some(false),
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
            keep: false,
            codex_home: None,
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
            through_reason: Some(false),
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
            keep: false,
            codex_home: None,
        }
    }

    /// A Gemini hook from process `pid`, and its MCP server registering `key` over its connection `conn`.
    fn gemini_hook(id: &str, event: &str, pid: u32) -> HookCall {
        HookCall { pid: Some(pid), ..gemini(id, event) }
    }

    fn gemini_mcp(d: &Daemon, conn: u64, key: &str, pid: u32) {
        let id = key.strip_prefix("gemini:").map(str::to_string);
        let reg = SessionReg { agent_session_id: id, pid: Some(pid), ..named(key, "gemini", None) };
        d.register_quiet(Some(conn), reg, GitInfo::default()).unwrap();
    }

    fn live_sessions(d: &Daemon) -> Vec<(String, String, Tier)> {
        let g = d.inner.lock().unwrap();
        let mut v: Vec<_> = g.live.iter().map(|(k, l)| (k.clone(), l.name.clone(), l.reg.tier)).collect();
        v.sort();
        v
    }

    /// A process that has exited.
    fn gone_pid() -> u32 {
        let mut child = std::process::Command::new(std::env::current_exe().unwrap())
            .arg("--list")
            .stdout(std::process::Stdio::null())
            .spawn()
            .unwrap();
        child.wait().unwrap();
        child.id()
    }

    #[tokio::test]
    async fn geminis_mcp_server_goes_by_the_session_its_hooks_registered() {
        // Gemini gives its MCP servers no session id, only its hooks: its MCP server asks for the session the hooks of
        // its own process registered, and registers that, which keeps its hooks' tier.
        let d = Arc::new(super::super::test_daemon(Default::default()));
        let (me, other) = (std::process::id(), util::parent_pid());
        d.hook(gemini_hook("g1", "start", me)).await.unwrap();
        assert_eq!(d.linked("gemini", me).as_deref(), Some("g1"));
        assert_eq!(d.linked("gemini", other), None);
        gemini_mcp(&d, 1, "gemini:g1", me);
        assert_eq!(live_sessions(&d), [("gemini:g1".into(), "api-gemini".into(), Tier::Turn)]);
        // Only Gemini's: agents whose MCP calls name the session, or that share one MCP server, never ask.
        let mut cursor = gemini_hook("c1", "start", me);
        cursor.agent = "cursor".into();
        d.hook(cursor).await.unwrap();
        assert_eq!((d.linked("cursor", me), d.linked("claude-code", me), d.linked("codex", me)), (None, None, None));
    }

    #[tokio::test]
    async fn an_older_gemini_mcp_server_keeps_the_session_it_registered_by_process() {
        // An MCP server from before (or one whose hooks were late) registered by its process: its hooks coming never
        // take that key away, so it keeps working across an upgrade.
        let d = Arc::new(super::super::test_daemon(Default::default()));
        let me = std::process::id();
        gemini_mcp(&d, 7, "proc:7", me);
        d.hook(gemini_hook("g2", "start", me)).await.unwrap();
        let keys: Vec<String> = live_sessions(&d).into_iter().map(|(k, ..)| k).collect();
        assert_eq!(keys, ["gemini:g2", "proc:7"]);
        // A current one goes by its hooks' session from then on: over the same connection, that ends its own.
        gemini_mcp(&d, 7, "gemini:g2", me);
        assert_eq!(live_sessions(&d), [("gemini:g2".into(), "api-gemini-2".into(), Tier::Turn)]);
    }

    #[tokio::test]
    async fn a_new_conversation_in_a_gemini_window_ends_the_one_before() {
        let d = Arc::new(super::super::test_daemon(Default::default()));
        let me = std::process::id();
        d.hook(gemini_hook("g1", "start", me)).await.unwrap();
        gemini_mcp(&d, 3, "gemini:g1", me);
        // `/clear`: SessionEnd, then SessionStart with a new id, the same MCP server throughout.
        d.hook(gemini_hook("g1", "end", me)).await.unwrap();
        assert_eq!(live_sessions(&d), []);
        d.hook(gemini_hook("g2", "start", me)).await.unwrap();
        assert_eq!(d.linked("gemini", me).as_deref(), Some("g2"));
        gemini_mcp(&d, 3, "gemini:g2", me);
        assert_eq!(live_sessions(&d).len(), 1);
        // A SessionEnd that never came: the next conversation's start ends this one all the same. An older MCP
        // server's session, registered by process, stays.
        gemini_mcp(&d, 9, "proc:9", me);
        d.hook(gemini_hook("g3", "start", me)).await.unwrap();
        let keys: Vec<String> = live_sessions(&d).into_iter().map(|(k, ..)| k).collect();
        assert_eq!(keys, ["gemini:g3", "proc:9"]);
    }

    #[tokio::test]
    async fn no_session_is_linked_when_it_is_unclear_or_its_process_is_gone() {
        let d = Arc::new(super::super::test_daemon(Default::default()));
        let me = std::process::id();
        // Two of its process's sessions: neither is taken for the other.
        d.hook(gemini_hook("g1", "start", me)).await.unwrap();
        gemini_mcp(&d, 5, "gemini:g9", me);
        assert_eq!(d.linked("gemini", me), None);
        // A process that has exited (its id may be some other process's soon).
        let gone = gone_pid();
        d.hook(gemini_hook("g4", "start", gone)).await.unwrap();
        assert_eq!(d.linked("gemini", gone), None);
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

    #[test]
    fn paused_sessions_and_held_mail_keep_the_normal_expiry() {
        let mut d =
            super::super::test_daemon(crate::config::Config { person: Some("alex".into()), ..Default::default() });
        join(&d, claude("a"));
        leave(&d, "claude-code:a", 0);
        let bob = iroh::SecretKey::generate();
        let mut team = TeamState::found("acme", &d.person, &d.machine, &d.key);
        team.put_member(&d.key, bob.public(), "bob", "xps", true);
        d.inner.lock().unwrap().team = Some(team);
        d.cfg.unreadable = Some("bad TOML".into());
        d.cfg.all_folders_off = true;
        d.cfg.inbound.default = crate::config::Policy::Hold;
        settled(&mut d);
        let d = Arc::new(d);
        let msg = Message {
            id: ulid::Ulid::generate().to_string(),
            from: "bob/web".into(),
            from_agent: None,
            from_repo: None,
            to: d.addr("api-claude"),
            via: None,
            reply_to: None,
            body: "kept while paused".into(),
            wake: true,
            notify: false,
            sent_at: 0,
            approval: None,
            files: Vec::new(),
        };
        assert_eq!(d.receive(bob.public(), msg).unwrap().0, "held");
        let g = d.inner.lock().unwrap();
        assert!(g.store.unread(&d.addr("api-claude")).unwrap().is_empty());
        drop(g);
        d.inner.lock().unwrap().store.touch_session("api-claude", util::now_ms() - 2 * DAY).unwrap();
        {
            let mut g = d.inner.lock().unwrap();
            d.prune(&mut g);
        }
        assert!(d.inner.lock().unwrap().store.session_by_name("api-claude").unwrap().is_none());
        assert!(d.inner.lock().unwrap().store.held().unwrap().is_empty());
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

    /// Alex's daemon, with Bob's machine in the team and mail from teammates held when `hold`.
    fn with_bob(store: crate::store::Store, hold: bool) -> (Daemon, iroh::SecretKey) {
        let inbound = if hold {
            crate::config::Inbound { default: crate::config::Policy::Hold, ..Default::default() }
        } else {
            Default::default()
        };
        let cfg = crate::config::Config { person: Some("alex".into()), inbound, ..Default::default() };
        let d = Daemon::with(cfg, store, iroh::SecretKey::generate()).unwrap();
        let bob = iroh::SecretKey::generate();
        let mut team = TeamState::found("acme", &d.person, &d.machine, &d.key);
        team.put_member(&d.key, bob.public(), "bob", "xps", true);
        d.inner.lock().unwrap().team = Some(team);
        (d, bob)
    }

    fn from_bob(d: &Daemon, to: &str) -> Message {
        Message {
            id: ulid::Ulid::generate().to_string(),
            from: "bob/web".into(),
            from_agent: None,
            from_repo: None,
            to: d.addr(to),
            via: None,
            reply_to: None,
            body: "still on the login bug?".into(),
            wake: true,
            notify: false,
            sent_at: 0,
            approval: None,
            files: Vec::new(),
        }
    }

    fn memory_store() -> crate::store::Store {
        let inbox = std::env::temp_dir().join(format!("reason-network-inbox-{}", ulid::Ulid::generate()));
        crate::store::Store::open(std::path::Path::new(":memory:"), inbox).unwrap()
    }

    const TITLED: &str = "api-fix-the-login-bug";

    /// Claude Code's hook for the conversation `id`.
    fn claude_hook(id: &str, event: &str) -> HookCall {
        HookCall {
            through_reason: Some(false),
            agent: agent::CLAUDE_CODE.into(),
            event: event.into(),
            session_id: id.into(),
            cwd: Some("/work/api".into()),
            pid: None,
            mail: true,
            transcript: None,
            doing: None,
            keep: false,
            name: None,
            dirs: Vec::new(),
            codex_home: None,
        }
    }

    /// Whether the hook for `event` tells the conversation `id` its name now.
    async fn told(d: &Arc<Daemon>, id: &str, event: &str) -> bool {
        let reply = d.hook(claude_hook(id, event)).await.unwrap();
        reply.text.is_some_and(|t| t.contains(&format!("you are {} now", d.addr(TITLED))))
    }

    #[tokio::test]
    async fn a_titled_chats_old_name_stays_its_own_after_the_daemon_restarts() {
        // Restarted before its next hook told it its new name, and after.
        for told_before in [false, true] {
            let home = std::env::temp_dir().join(format!("reason-network-restart-{}", ulid::Ulid::generate()));
            std::fs::create_dir_all(&home).unwrap();
            let open = || crate::store::Store::open(&home.join("network.db"), home.join("inbox")).unwrap();
            {
                let (d, _) = with_bob(open(), false);
                let d = Arc::new(d);
                assert_eq!(join(&d, claude("x")).name, "api-claude");
                assert_eq!(join(&d, claude("a")).name, "api-claude-2");
                assert!(d.name_from_title("claude-code:a", TITLED.into()).unwrap().is_some());
                if told_before {
                    assert!(told(&d, "a", "prompt").await);
                }
            }
            // An update restarts the daemon, on the same database: a new chat in the folder registers before `a` does.
            let (d, bob) = with_bob(open(), false);
            let d = Arc::new(d);
            assert_eq!(join(&d, claude("x")).name, "api-claude");
            assert_eq!(join(&d, claude("z")).name, "api-claude-3");
            assert_eq!(join(&d, claude("a")).name, TITLED);
            assert_eq!(d.inner.lock().unwrap().live["claude-code:a"].former.as_deref(), Some("api-claude-2"));
            // Told its new name once: at its next prompt, unless it was told before the restart.
            assert_eq!(told(&d, "a", "prompt").await, !told_before, "told before: {told_before}");
            assert!(!told(&d, "a", "prompt").await);
            // Bob's mail to the name it was told reaches it, not the new chat.
            let (state, _) = d.receive(bob.public(), from_bob(&d, "api-claude-2")).unwrap();
            assert_ne!(state, "refused");
            let g = d.inner.lock().unwrap();
            assert_eq!(g.store.unread_count(&d.addr(TITLED)).unwrap(), 1);
            assert_eq!(g.store.unread_count(&d.addr("api-claude-2")).unwrap(), 0);
            assert_eq!(g.store.unread_count(&d.addr("api-claude-3")).unwrap(), 0);
            drop(g);
            let _ = std::fs::remove_dir_all(&home);
        }
    }

    #[test]
    fn an_old_name_another_chat_took_never_comes_back_to_the_titled_one_after_a_restart() {
        let (mut d, bob) = with_bob(memory_store(), false);
        settled(&mut d);
        let d = Arc::new(d);
        // Known only from its hooks, so its name isn't kept for it once it ends.
        let hook_only = || SessionReg { tier: Tier::Turn, ..claude("a") };
        assert_eq!(d.register_quiet(None, hook_only(), GitInfo::default()).unwrap().0.name, "api-claude");
        assert!(d.name_from_title("claude-code:a", TITLED.into()).unwrap().is_some());
        leave(&d, "claude-code:a", 0);
        // Its old name is free, and another chat takes it: for good.
        assert_eq!(join(&d, claude("z")).name, "api-claude");
        assert_eq!(d.inner.lock().unwrap().store.former("claude-code:a").unwrap(), None);
        // A restart, and the titled chat comes back first.
        restart(&d);
        assert_eq!(d.register_quiet(None, hook_only(), GitInfo::default()).unwrap().0.name, TITLED);
        assert_eq!(d.inner.lock().unwrap().live["claude-code:a"].former, None);
        // Mail to the name waits for the chat that has it, not the titled one.
        let (state, _) = d.receive(bob.public(), from_bob(&d, "api-claude")).unwrap();
        assert_ne!(state, "refused");
        let g = d.inner.lock().unwrap();
        assert_eq!(g.store.unread_count(&d.addr("api-claude")).unwrap(), 1);
        assert_eq!(g.store.unread_count(&d.addr(TITLED)).unwrap(), 0);
    }

    #[tokio::test]
    async fn a_reply_from_a_titled_chat_to_mail_sent_to_its_old_name_counts_as_the_reply() {
        let (d, _) = with_bob(memory_store(), false);
        let d = Arc::new(d);
        assert_eq!(join(&d, claude("a")).name, "api-claude");
        assert!(d.name_from_title("claude-code:a", TITLED.into()).unwrap().is_some());
        assert_eq!(join(&d, claude("z")).name, "api-claude-2");
        // Sent here to its old name, while it is live and once it has ended.
        for live in [true, false] {
            if !live {
                leave(&d, "claude-code:a", 0);
            }
            let sent = d.send("claude-code:z".into(), "api-claude", "q", None, true, false, None, Vec::new()).await;
            let id = sent.unwrap().ids[0].clone();
            if !live {
                join(&d, claude("a"));
            }
            let reply =
                d.send("claude-code:a".into(), "api-claude-2", "a", Some(id.clone()), true, false, None, Vec::new());
            reply.await.unwrap();
            let status = d.inner.lock().unwrap().store.delivery_status("claude-code:z", &[id]).unwrap();
            assert_eq!(status[0]["stage"], "replied", "live: {live}: {status:?}");
            assert_eq!(status[0]["recipient"], d.addr(TITLED));
        }
    }

    #[test]
    fn a_teammate_says_where_mail_to_an_old_name_went_and_its_reply_from_there_counts() {
        let (d, bob) = with_bob(memory_store(), false);
        let d = Arc::new(d);
        join(&d, claude("z"));
        let sent = |id: &str| Message {
            id: id.into(),
            from: d.addr("api-claude"),
            to: "bob/web-claude".into(),
            sent_at: util::now_ms(),
            ..from_bob(&d, "x")
        };
        let reply = |id: &str, from: &str| Message {
            from: from.into(),
            to: d.addr("api-claude"),
            reply_to: Some(id.into()),
            ..from_bob(&d, "x")
        };
        let stage = |id: &str| {
            let g = d.inner.lock().unwrap();
            g.store.delivery_status("claude-code:z", &[id.into()]).unwrap()[0]["stage"].as_str().unwrap().to_string()
        };
        let (one, other) = ("01JA0000000000000000000001", "01JA0000000000000000000002");
        for id in [one, other] {
            d.inner.lock().unwrap().store.record_sent("claude-code:z", &sent(id), "bob/web-claude", "queued").unwrap();
        }
        // Bob's machine took it for the chat at its name now; nobody else can say so, nor name someone else's session.
        let carol = iroh::SecretKey::generate();
        d.readdressed(&carol.public(), one, "bob/web-fix");
        d.readdressed(&bob.public(), one, "carol/web");
        d.readdressed(&bob.public(), one, "bob/web-fix");
        d.receive(bob.public(), reply(one, "bob/web-fix")).unwrap();
        assert_eq!(stage(one), "replied");
        // Without word of where it went, a reply from another of Bob's chats is not the one it waits for.
        d.receive(bob.public(), reply(other, "bob/web-fix")).unwrap();
        assert_ne!(stage(other), "replied");
        // The word comes after the reply (Bob's machine flushed its outbox first on a reconnect): the reply counts then.
        d.readdressed(&bob.public(), other, "bob/web-fix");
        assert_eq!(stage(other), "replied");
        // A reply from elsewhere doesn't, whenever the word comes.
        let late = "01JA0000000000000000000003";
        d.inner.lock().unwrap().store.record_sent("claude-code:z", &sent(late), "bob/web-claude", "queued").unwrap();
        d.receive(bob.public(), reply(late, "bob/web-other")).unwrap();
        d.readdressed(&bob.public(), late, "bob/web-fix");
        assert_ne!(stage(late), "replied");
    }

    #[test]
    fn mail_to_an_old_name_that_is_to_be_tried_again_says_nothing_of_where_it_went() {
        let (d, bob) = with_bob(memory_store(), false);
        let d = Arc::new(d);
        assert_eq!(join(&d, claude("a")).name, "api-claude");
        assert!(d.name_from_title("claude-code:a", TITLED.into()).unwrap().is_some());
        // Bob has sent all the files this machine takes from him in an hour.
        let hour = crate::files::QUOTA_SENDER_HOUR;
        d.inner.lock().unwrap().quota.admit("bob", hour, 0, 0, std::time::Instant::now()).unwrap();
        let file = crate::proto::Attachment { name: "log.txt".into(), content: "x".into(), path: None };
        let msg = Message { files: vec![file], ..from_bob(&d, "api-claude") };
        let (state, note, to) = d.receive_readdressed(bob.public(), msg).unwrap();
        // Tried again later, when the name may be another chat's: Bob's receipt keeps the name it was sent to.
        assert_eq!((state.as_str(), to), ("retry", None), "{note}");
    }

    #[test]
    fn a_titled_chat_back_under_another_name_keeps_its_old_one() {
        let (d, bob) = with_bob(memory_store(), false);
        let d = Arc::new(d);
        assert_eq!(join(&d, claude("a")).name, "api-claude");
        assert!(d.name_from_title("claude-code:a", TITLED.into()).unwrap().is_some());
        restart(&d);
        // Its name now is live on another of my machines: it comes back with a suffix, asking for that name.
        let others = Elsewhere { live: HashSet::from([TITLED.to_string()]), ..Default::default() };
        let reg = SessionReg { name: Some(TITLED.into()), ..claude("a") };
        let suffixed = format!("{TITLED}-2");
        let name = d.register_among(Some(conn_of(&reg.key)), reg, GitInfo::default(), others).unwrap().0.name;
        assert_eq!(name, suffixed);
        // Its old name is still its own, live as remembered.
        assert_eq!(d.inner.lock().unwrap().live["claude-code:a"].former.as_deref(), Some("api-claude"));
        assert_eq!(
            d.inner.lock().unwrap().store.former("claude-code:a").unwrap().map(|f| f.0).as_deref(),
            Some("api-claude")
        );
        // Bob's mail to it is taken as mail to the live chat is.
        let (state, note, to) = d.receive_readdressed(bob.public(), from_bob(&d, "api-claude")).unwrap();
        assert_eq!(to, Some(d.addr(&suffixed)), "{note}");
        assert_eq!(d.receive(bob.public(), from_bob(&d, &suffixed)).unwrap().0, state);
        assert_eq!(d.inner.lock().unwrap().store.unread_count(&d.addr(&suffixed)).unwrap(), 2);
    }

    #[test]
    fn a_title_never_takes_an_offline_chats_name_or_its_mail() {
        let mut d = super::super::test_daemon(Default::default());
        settled(&mut d);
        // Someone named this chat by hand, as a title would; it has gone offline with mail waiting.
        let by_hand = SessionReg { name: Some(TITLED.into()), ..claude("h") };
        assert_eq!(join(&d, by_hand).name, TITLED);
        mail(&d, "waiting", TITLED);
        leave(&d, "claude-code:h", 0);
        // A new chat titled "Fix the login bug" may not take it, though someone asking for it by name could.
        assert_eq!(join(&d, claude("n")).name, "api-claude");
        assert!(d.name_from_title("claude-code:n", TITLED.into()).is_err());
        let g = d.inner.lock().unwrap();
        assert_eq!(g.store.session_by_name(TITLED).unwrap().unwrap().key, "claude-code:h");
        assert_eq!(g.store.unread_count(&d.addr(TITLED)).unwrap(), 1);
        assert_eq!(g.live["claude-code:n"].name, "api-claude");
        assert_eq!(g.store.unread_count(&d.addr("api-claude")).unwrap(), 0);
    }

    #[test]
    fn a_teammates_ack_moves_the_receipt_to_where_mail_went_only_once_it_was_taken() {
        let (d, bob) = with_bob(memory_store(), false);
        join(&d, claude("z"));
        let sent = |id: &str| Message {
            id: id.into(),
            from: d.addr("api-claude"),
            to: "bob/web-claude".into(),
            sent_at: util::now_ms(),
            ..from_bob(&d, "x")
        };
        let recipient = |id: &str| d.inner.lock().unwrap().store.receipt_recipient(id).unwrap().unwrap();
        let (busy, refused) = ("01JA0000000000000000000011", "01JA0000000000000000000012");
        for id in [busy, refused] {
            d.inner.lock().unwrap().store.record_sent("claude-code:z", &sent(id), "bob/web-claude", "queued").unwrap();
        }
        // Turned away for now, or for good: the receipt keeps the address it was sent to.
        d.acked(&bob.public(), busy, "retry", Some("bob/web-fix".into()));
        d.acked(&bob.public(), refused, "refused", Some("bob/web-fix".into()));
        assert_eq!((recipient(busy), recipient(refused)), ("bob/web-claude".into(), "bob/web-claude".into()));
        // Taken, at the name its chat goes by now: the receipt waits for a reply from there.
        d.acked(&bob.public(), busy, "queued", Some("bob/web-fix".into()));
        assert_eq!(recipient(busy), "bob/web-fix");
    }

    #[test]
    fn an_explicit_reclaim_of_an_ended_titled_chats_old_name_takes_it_for_good() {
        let mut d = super::super::test_daemon(Default::default());
        settled(&mut d);
        assert_eq!(join(&d, claude("a")).name, "api-claude");
        assert!(d.name_from_title("claude-code:a", TITLED.into()).unwrap().is_some());
        mail(&d, "titled", TITLED);
        leave(&d, "claude-code:a", 0);
        assert_eq!(join(&d, claude("z")).name, "api-claude-2");
        // Asked for by name, as `set_name` does: the ended chat's old name is taken, its own mail stays with it.
        let r = d.set_status("claude-code:z", None, Some("api-claude".into()), Rename::Explicit).unwrap();
        assert_eq!((r.name.as_str(), r.unread), ("api-claude", 0));
        {
            let g = d.inner.lock().unwrap();
            assert_eq!(g.store.session_by_name("api-claude").unwrap().unwrap().key, "claude-code:z");
            assert_eq!(g.store.former("claude-code:a").unwrap(), None);
            assert_eq!(g.store.unread_count(&d.addr(TITLED)).unwrap(), 1);
        }
        // The titled chat comes back to its name now, without the old one.
        leave(&d, "claude-code:z", 0);
        restart(&d);
        assert_eq!(join(&d, claude("a")).name, TITLED);
        assert_eq!(d.inner.lock().unwrap().live["claude-code:a"].former, None);
    }

    #[test]
    fn mail_sent_again_to_an_old_name_says_where_it_went_then_not_who_has_the_name_now() {
        let (mut d, bob) = with_bob(memory_store(), false);
        settled(&mut d);
        let d = Arc::new(d);
        assert_eq!(join(&d, claude("a")).name, "api-claude");
        assert!(d.name_from_title("claude-code:a", TITLED.into()).unwrap().is_some());
        // Bob's mail to the old name reaches the titled chat; word of where it went gets lost on the way back.
        let first = from_bob(&d, "api-claude");
        assert_eq!(d.receive_readdressed(bob.public(), first.clone()).unwrap().2, Some(d.addr(TITLED)));
        // The titled chat ends, and another asks for the old name by name and gets it.
        leave(&d, "claude-code:a", 0);
        assert_eq!(join(&d, claude("z")).name, "api-claude-2");
        d.set_status("claude-code:z", None, Some("api-claude".into()), Rename::Explicit).unwrap();
        // Bob's machine sends it again: not delivered twice, and it went to the titled chat, where its reply comes from.
        let (state, note, to) = d.receive_readdressed(bob.public(), first.clone()).unwrap();
        assert_eq!((state.as_str(), note.as_str(), to), ("queued", "already delivered", Some(d.addr(TITLED))));
        {
            let g = d.inner.lock().unwrap();
            assert_eq!(g.store.unread_count(&d.addr(TITLED)).unwrap(), 1);
            assert_eq!(g.store.unread_count(&d.addr("api-claude")).unwrap(), 0);
        }
        // The same id from another of Bob's chats is not Bob's mail sent again: it says nothing of where that went.
        let other = Message { from: "bob/other".into(), ..first };
        assert_eq!(d.receive_readdressed(bob.public(), other).unwrap().2, None);
    }

    #[tokio::test]
    async fn a_titled_chats_old_name_is_kept_for_it_once_it_ends_until_it_is_forgotten() {
        let (mut d, bob) = with_bob(memory_store(), false);
        // Long after the daemon started: sessions gone a day are forgotten.
        settled(&mut d);
        let d = Arc::new(d);
        assert_eq!(join(&d, claude("a")).name, "api-claude");
        assert!(d.name_from_title("claude-code:a", TITLED.into()).unwrap().is_some());
        leave(&d, "claude-code:a", 0);
        // A new chat in the folder doesn't get the name the ended one was told, nor can take it.
        assert_eq!(join(&d, claude("z")).name, "api-claude-2");
        assert!(d.set_status("claude-code:z", None, Some("api-claude".into()), Rename::Automatic).is_err());
        let presence = d.presence_of(Vec::new());
        assert!(presence.kept.contains(&"api-claude".to_string()), "{:?}", presence.kept);
        // Bob's mail to it (say from his outbox, while Alex was offline) waits for the ended chat, at its name now.
        let (state, _) = d.receive(bob.public(), from_bob(&d, "api-claude")).unwrap();
        assert_ne!(state, "refused");
        {
            let g = d.inner.lock().unwrap();
            assert_eq!(g.store.unread_count(&d.addr(TITLED)).unwrap(), 1);
            assert_eq!(g.store.unread_count(&d.addr("api-claude")).unwrap(), 0);
            assert_eq!(g.store.unread_count(&d.addr("api-claude-2")).unwrap(), 0);
        }
        // Mail sent here to it goes the same way.
        let sent = d.send("claude-code:z".into(), "api-claude", "me too", None, true, false, None, Vec::new()).await;
        assert_eq!(sent.unwrap().recipients[0].address, d.addr(TITLED));
        // It comes back to its name and the old one, with that mail.
        let back = join(&d, claude("a"));
        assert_eq!((back.name.as_str(), back.unread), (TITLED, 2));
        assert_eq!(d.inner.lock().unwrap().live["claude-code:a"].former.as_deref(), Some("api-claude"));
        // Once it is forgotten, both names are free again.
        leave(&d, "claude-code:a", 2 * DAY);
        assert_eq!(join(&d, claude("y")).name, "api-claude");
        assert!(d.inner.lock().unwrap().store.session_by_name(TITLED).unwrap().is_none());
    }

    #[test]
    fn a_teammates_mail_to_a_titled_chats_old_name_reaches_it_held_or_not() {
        for hold in [false, true] {
            let (d, bob) = with_bob(memory_store(), hold);
            let d = Arc::new(d);
            assert_eq!(join(&d, claude("a")).name, "api-claude");
            assert!(d.name_from_title("claude-code:a", TITLED.into()).unwrap().is_some());
            assert_eq!(join(&d, claude("z")).name, "api-claude-2");
            let msg = from_bob(&d, "api-claude");
            let id = msg.id.clone();
            let (state, note, to) = d.receive_readdressed(bob.public(), msg).unwrap();
            assert_ne!(state, "refused", "{note}");
            // Bob's machine hears where it went, which the reply comes from.
            assert_eq!(to, Some(d.addr(TITLED)), "hold: {hold}");
            let mut ids = vec![id.clone()];
            if hold {
                assert_eq!(state, "held");
                assert_eq!(d.settle(std::slice::from_ref(&id), true).unwrap(), 1);
            } else {
                // Taken as mail to the live chat at its name now is (not left waiting as for an offline one).
                let to_now = from_bob(&d, TITLED);
                ids.push(to_now.id.clone());
                assert_eq!(d.receive(bob.public(), to_now).unwrap(), (state, note));
            }
            let g = d.inner.lock().unwrap();
            let unread = g.store.unread(&d.addr(TITLED)).unwrap();
            assert_eq!(unread.iter().map(|m| m.id.clone()).collect::<Vec<_>>(), ids, "hold: {hold}");
            assert!(unread.iter().all(|m| m.to == d.addr(TITLED)));
            assert_eq!(g.store.unread_count(&d.addr(TITLED)).unwrap(), ids.len() as u32);
            assert_eq!(g.store.unread_count(&d.addr("api-claude-2")).unwrap(), 0);
            assert_eq!(g.store.unread_count(&d.addr("api-claude")).unwrap(), 0);
        }
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
            through_reason: Some(false),
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
            keep: false,
            codex_home: None,
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
            mailbox: false,
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
                    mailbox: false,
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
            claude_dir: None,
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
            claude_dir: None,
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
            through_reason: Some(false),
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
            keep: false,
            codex_home: None,
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
            claude_dir: None,
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

    /// MCP servers of earlier releases drop a session refused on reconnect only when the refusal is exactly `OFF_HERE`.
    /// Any other text fails their whole reconnect, and they keep retrying it, so it stays exactly that, whatever part
    /// of the config file can't be read.
    #[tokio::test]
    async fn a_daemon_refuses_a_folder_that_is_off_with_exactly_the_old_text_even_when_the_file_is_unreadable() {
        let off: std::collections::BTreeMap<_, _> = [("/work/private".to_string(), "off".to_string())].into();
        for cfg in [
            // The whole file: every folder is off.
            crate::config::Config { unreadable: Some("bad TOML".into()), all_folders_off: true, ..Default::default() },
            // Another setting.
            crate::config::Config {
                folders: off.clone(),
                unreadable: Some("port: invalid type".into()),
                ..Default::default()
            },
            // The folder's own entry.
            crate::config::Config {
                folders: off.clone(),
                unreadable_folders: vec!["/work/private".into()],
                unreadable: Some("folders./work/private: expected \"off\"".into()),
                ..Default::default()
            },
        ] {
            let d = Arc::new(super::super::test_daemon(cfg));
            let private = SessionReg { cwd: "/work/private".into(), ..named("cursor:c1", "cursor", None) };
            let in_private = HookCall { dirs: vec!["/work/private".into()], ..gemini("c2", "prompt") };
            let said = [
                d.register_quiet(Some(1), private, GitInfo::default()).unwrap_err(),
                d.hook(in_private).await.unwrap_err(),
                // Its hooks said it is in a folder that is off; its MCP server doesn't know.
                d.register_quiet(Some(2), named("gemini:c2", "gemini", None), GitInfo::default()).unwrap_err(),
            ];
            for e in said {
                assert_eq!(e.to_string(), OFF_HERE);
            }
        }
    }
}
