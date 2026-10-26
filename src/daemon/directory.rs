//! Who is where: this machine's sessions, teammates' sessions (from their presence), and resolving an address
//! like `alice/api-cursor`, `repo:api` or `*` to recipients.

use super::Daemon;
use crate::{
    CLI, agent,
    claude::{self, ClaudeSession},
    doing,
    git::GitInfo,
    net::{Presence, Remote},
    proto::*,
    store::Remembered,
    util,
};
use anyhow::{Result, bail};
use iroh::EndpointId;
use std::{
    collections::{HashMap, HashSet},
    sync::Arc,
};

/// How many offline names (`known`, and `kept`) presence carries: what teammates' machines take.
const PRESENCE_NAMES: usize = 100;

/// A resolved recipient.
pub(super) enum Target {
    /// A session on this machine; `key` is set when it is running.
    Session { address: String, key: Option<String> },
    /// A session or person on a teammate's machine.
    Remote { address: String, machine: EndpointId, machine_name: String, online: bool },
}

impl Target {
    pub(super) fn address(&self) -> String {
        match self {
            Target::Session { address, .. } | Target::Remote { address, .. } => address.clone(),
        }
    }

    /// Its address as a message from it shows here, which a receipt names: a teammate's terminal with its machine.
    pub(super) fn shown_address(&self) -> String {
        match self {
            Target::Session { address, .. } => address.clone(),
            Target::Remote { address, machine_name, .. } => crate::render::shown_from(address, machine_name),
        }
    }
}

/// Something an address can match.
struct Cand {
    person: String,
    name: String,
    machine: String,
    live: bool,
    repo: Option<String>,
    place: Place,
}

enum Place {
    Local(Option<String>),
    Remote(EndpointId, bool),
}

impl Daemon {
    /// This machine's sessions: running ones, plus (if `unmanaged`) Claude Code sessions without Reason Network.
    pub(super) async fn local_rows(self: &Arc<Self>, me: Option<&str>, unmanaged: bool) -> Result<Vec<SessionInfo>> {
        let claude = self.shared(tokio::task::spawn_blocking(claude::scan).await?);
        let mut cwds: Vec<String> = self.inner.lock().unwrap().live.values().map(|l| l.reg.cwd.clone()).collect();
        cwds.extend(claude.iter().filter_map(|c| c.cwd.clone()));
        let mut gits: HashMap<String, GitInfo> = HashMap::new();
        for cwd in cwds {
            let info = self.git(&cwd).await;
            gits.insert(cwd, info);
        }

        // With Claude Code on Reason Network, every open Claude session is on it: sessions started before Reason Network was set
        // up have no Reason Network tools, but take messages through their own socket and reply with the CLI.
        if claude::wired() {
            let now = util::now_ms();
            for c in &claude {
                let git = c.cwd.as_ref().and_then(|cwd| gits.get(cwd)).cloned().unwrap_or_default();
                self.claude_seen(c, git, now);
            }
        }
        // A Claude Code session someone renamed goes by that name here too.
        let renames: Vec<(String, String)> = {
            let g = self.inner.lock().unwrap();
            g.live
                .iter()
                .filter_map(|(k, l)| {
                    let name = util::slug(claude.iter().find(|c| claude_match(c, &l.reg))?.chosen_name()?);
                    (!name.is_empty() && name != l.name).then(|| (k.clone(), name))
                })
                .collect()
        };
        for (key, name) in renames {
            let _ = self.set_status(&key, None, Some(name));
        }
        // What each session is doing, from what its agent records anyway (title, task in progress).
        let sources: Vec<(String, doing::Source)> = {
            let g = self.inner.lock().unwrap();
            let live = g.live.iter().map(|(k, l)| {
                let session = claude.iter().find(|c| claude_match(c, &l.reg)).and_then(|c| c.session_id.clone());
                let session = session.or_else(|| l.reg.agent_session_id.clone());
                (k.clone(), doing::Source { agent: l.reg.agent.clone(), session, transcript: l.transcript.clone() })
            });
            let unmanaged = claude.iter().map(|c| {
                let source =
                    doing::Source { agent: agent::CLAUDE_CODE.into(), session: c.session_id.clone(), transcript: None };
                (format!("pid:{}", c.pid), source)
            });
            live.chain(unmanaged).collect()
        };
        let doings: HashMap<String, String> = tokio::task::spawn_blocking(move || {
            sources.into_iter().filter_map(|(k, s)| Some((k, doing::of(&s)?))).collect()
        })
        .await?;

        let mut g = self.inner.lock().unwrap();
        self.prune(&mut g);
        for l in g.live.values_mut() {
            if let Some(info) = gits.get(&l.reg.cwd) {
                l.git = info.clone();
            }
        }
        let mut rows = Vec::new();
        let mut matched = HashSet::new();
        for l in g.live.values() {
            let mut row = self.row(&l.name, &l.reg.agent, &l.reg.cwd, Some(&l.git));
            row.since = Some(l.since);
            if let Some((st, at)) = &l.status {
                row.status = st.clone();
                row.since = Some(*at);
            }
            // Claude Code's registry knows whether the session is busy or idle.
            if let Some(c) = claude.iter().find(|c| claude_match(c, &l.reg)) {
                matched.insert(c.pid);
                if let Some(st) = &c.status {
                    row.status = st.clone();
                    row.since = c.status_updated_at.or(row.since);
                }
            }
            row.doing = l.doing.clone().or_else(|| doings.get(&l.reg.key).cloned());
            row.tier = l.reg.tier;
            row.unread = g.store.unread_count(&row.address)?;
            row.is_me = me == Some(l.reg.key.as_str());
            rows.push(row);
        }
        if unmanaged {
            for c in claude.iter().filter(|c| !matched.contains(&c.pid)) {
                let cwd = c.cwd.clone().unwrap_or_default();
                let name = c.label().unwrap_or_else(|| format!("claude-{}", c.pid));
                let mut row = self.row(&name, agent::CLAUDE_CODE, &cwd, gits.get(&cwd));
                row.status = c.status.clone().unwrap_or_else(|| "live".into());
                row.since = c.status_updated_at.or(c.started_at);
                row.doing = doings.get(&format!("pid:{}", c.pid)).cloned();
                rows.push(row);
            }
        }
        Ok(rows)
    }

    /// A live Claude Code session in Claude Code's own registry: on the team, and the conversation its process is in.
    fn claude_seen(self: &Arc<Self>, c: &ClaudeSession, git: GitInfo, now: i64) {
        let Some(id) = &c.session_id else { return };
        let key = format!("{}:{id}", agent::CLAUDE_CODE);
        // Its process is in this conversation now: after `/clear`, the one it was in has ended, though its MCP server
        // (which the registration names by this process) hasn't said so; until then mail for it would go to this one.
        {
            let mut g = self.inner.lock().unwrap();
            let before: Vec<String> = g
                .live
                .iter()
                .filter(|(k, l)| l.reg.agent == agent::CLAUDE_CODE && l.reg.pid == Some(c.pid) && **k != key)
                .map(|(k, _)| k.clone())
                .collect();
            self.moved_on(&mut g, before, now);
        }
        if !self.inner.lock().unwrap().live.contains_key(&key) {
            let named = matches!(c.name_source.as_deref(), Some("user" | "peer" | "hook" | "auto"));
            let reg = SessionReg {
                key: key.clone(),
                agent: agent::CLAUDE_CODE.into(),
                cwd: c.cwd.clone().unwrap_or_else(|| "/".into()),
                agent_session_id: Some(id.clone()),
                pid: Some(c.pid),
                tier: Tier::Push,
                name: if named { c.label() } else { None },
            };
            if let Err(e) = self.register(None, reg, git) {
                super::log(&format!("adding the Claude session of process {}: {e:#}", c.pid));
            }
        }
        // Seen alive just now (it goes when its process does), and its inbox listed: it can be pushed into.
        if let Some(l) = self.inner.lock().unwrap().live.get_mut(&key) {
            l.last_hook = Some(now);
            if c.messaging_socket_path.is_some() && !l.push_failed {
                l.reg.tier = Tier::Push;
            }
        }
    }

    pub(super) fn row(&self, name: &str, agent: &str, cwd: &str, git: Option<&GitInfo>) -> SessionInfo {
        SessionInfo {
            address: self.addr(name),
            name: name.to_string(),
            person: self.person.clone(),
            machine: self.machine.clone(),
            agent: agent.to_string(),
            cwd: util::tilde(cwd),
            repo: git.and_then(|g| g.repo.clone()),
            branch: git.and_then(|g| g.branch.clone()),
            status: "live".into(),
            since: None,
            doing: None,
            tier: Tier::None,
            unread: 0,
            is_me: false,
        }
    }

    /// Claude Code sessions outside the folders that are off.
    fn shared(&self, mut claude: Vec<ClaudeSession>) -> Vec<ClaudeSession> {
        claude.retain(|c| !c.cwd.as_deref().is_some_and(|d| self.cfg.off_in(d)));
        claude
    }

    /// Sessions this machine remembers (gone for now), but none from a folder that is off.
    fn remembered(&self, g: &super::Inner) -> Vec<Remembered> {
        let live: HashSet<&str> = g.live.values().map(|l| l.name.as_str()).collect();
        let mut known = g.store.sessions().unwrap_or_default();
        known.retain(|r| !live.contains(r.name.as_str()) && !self.cfg.off_in(&r.cwd));
        known
    }

    /// What this machine tells teammates about itself.
    pub async fn presence(self: &Arc<Self>) -> Presence {
        let sessions = self.local_rows(None, self.cfg.share_unmanaged).await.unwrap_or_default();
        self.presence_of(sessions)
    }

    pub(super) fn presence_of(&self, mut sessions: Vec<SessionInfo>) -> Presence {
        if !self.cfg.share_paths {
            for s in &mut sessions {
                s.cwd =
                    std::path::Path::new(&s.cwd).file_name().map_or("~".into(), |n| n.to_string_lossy().into_owned());
            }
        }
        let g = self.inner.lock().unwrap();
        let mut known = self.remembered(&g);
        known.sort_by_key(|r| -r.last_seen);
        // Not the person's own mailboxes: each machine has its own (`terminal`).
        let now = util::now_ms();
        let kept: Vec<String> =
            known.iter().filter(|r| r.agent != "cli" && self.holds(r, now)).map(|r| r.name.clone()).collect();
        // Teammates' machines take 100 of each, so only the most recent go; the rest are named only here.
        if kept.len() > PRESENCE_NAMES && !self.capped.swap(true, std::sync::atomic::Ordering::Relaxed) {
            super::log(&format!(
                "{} names are kept for offline sessions here: other machines hear of the {PRESENCE_NAMES} most recent",
                kept.len()
            ));
        }
        let mailboxes = g
            .store
            .sessions()
            .unwrap_or_default()
            .into_iter()
            .filter(|r| r.mailbox && !self.cfg.off_in(&r.cwd))
            .map(|r| r.name)
            .collect();
        let mut features =
            vec![crate::files::FEATURE.into(), super::sessions::TERMINALS.into(), super::anchor::FEATURE.into()];
        if self.cfg.anchor {
            features.push("anchor".into());
        }
        Presence {
            sessions,
            known: known.into_iter().take(PRESENCE_NAMES).map(|r| r.name).collect(),
            kept: kept.into_iter().take(PRESENCE_NAMES).collect(),
            features,
            mailboxes,
            anchor_routes: self.saved_mailboxes(&g),
        }
    }

    pub(super) async fn who(
        self: &Arc<Self>,
        me: Option<String>,
        filter: Option<String>,
        all: bool,
    ) -> Result<Directory> {
        let mut rows = self.local_rows(me.as_deref(), true).await?;
        if all {
            let g = self.inner.lock().unwrap();
            for r in self.remembered(&g) {
                let git = GitInfo { root: None, repo: r.repo.clone(), branch: r.branch.clone() };
                let mut row = self.row(&r.name, &r.agent, &r.cwd, Some(&git));
                row.status = "offline".into();
                row.since = Some(r.last_seen);
                // Kept by a build that left team links in.
                row.doing = r.doing.as_deref().map(crate::team::without_tickets);
                row.tier = r.tier;
                row.unread = g.store.unread_count(&row.address)?;
                rows.push(row);
            }
        }
        let machines = self.machines();
        if let (Some(net), Some(team)) = (self.net(), self.team_state()) {
            for (id, rem) in net.remotes() {
                let Some(m) = team.member(&id).filter(|m| m.active) else { continue };
                if !rem.online && !all {
                    continue;
                }
                for mut s in rem.presence.sessions {
                    // Trust the roster, not the peer, for who and where.
                    s.person = m.person.clone();
                    s.machine = m.machine.clone();
                    s.address = format!("{}/{}", m.person, s.name);
                    s.is_me = false;
                    if !rem.online {
                        s.status = "offline".into();
                        s.since = Some(rem.last_seen);
                    }
                    rows.push(s);
                }
            }
            let remotes = net.remotes();
            let routes = {
                let g = self.inner.lock().unwrap();
                self.learned_routes(&g, &remotes)
            };
            for route in routes {
                let Some(m) = team.member(&route.machine).filter(|m| m.active) else { continue };
                for name in route.names {
                    if rows.iter().any(|r| r.person == m.person && r.machine == m.machine && r.name == name) {
                        continue;
                    }
                    let mut row = self.row(&name, "cli", "", None);
                    row.person = m.person.clone();
                    row.address = format!("{}/{name}", m.person);
                    row.machine = m.machine.clone();
                    row.status = "offline".into();
                    row.tier = Tier::Pull;
                    rows.push(row);
                }
            }
        }
        if let Some(f) = filter.map(|f| f.to_lowercase()).filter(|f| !f.is_empty()) {
            rows.retain(|r| {
                [&r.address, &r.agent, &r.cwd, &r.machine]
                    .into_iter()
                    .chain([&r.repo, &r.branch, &r.doing].into_iter().flatten())
                    .any(|s| s.to_lowercase().contains(&f))
            });
        }
        // Me first, then teammates; on Reason Network before not; live before offline.
        let rank = |r: &SessionInfo| {
            (
                r.person != self.person,
                r.person.clone(),
                r.machine.clone(),
                r.tier == Tier::None,
                r.status == "offline",
                r.name.clone(),
            )
        };
        rows.sort_by_key(rank);
        Ok(Directory {
            team: self.team_state().map(|t| t.shown_name()),
            person: self.person.clone(),
            machine: self.machine.clone(),
            machines,
            sessions: rows,
        })
    }

    /// Every address target we know about, here and on teammates' machines.
    fn candidates(&self, g: &super::Inner, remotes: &HashMap<EndpointId, Remote>) -> Vec<Cand> {
        let mut out = Vec::new();
        for l in g.live.values() {
            out.push(Cand {
                person: self.person.clone(),
                name: l.name.clone(),
                machine: self.machine.clone(),
                live: true,
                repo: l.git.repo.clone(),
                place: Place::Local(Some(l.reg.key.clone())),
            });
        }
        for r in self.remembered(g) {
            out.push(Cand {
                person: self.person.clone(),
                name: r.name,
                machine: self.machine.clone(),
                live: false,
                repo: r.repo,
                place: Place::Local(None),
            });
        }
        let Some(team) = &g.team else { return out };
        for (id, rem) in remotes {
            let Some(m) = team.member(id).filter(|m| m.active) else { continue };
            let names: HashSet<&str> = rem.presence.sessions.iter().map(|s| s.name.as_str()).collect();
            for s in &rem.presence.sessions {
                out.push(Cand {
                    person: m.person.clone(),
                    name: s.name.clone(),
                    machine: m.machine.clone(),
                    live: rem.online && s.tier != Tier::None,
                    repo: s.repo.clone(),
                    place: Place::Remote(*id, rem.online),
                });
            }
            for n in rem.presence.known.iter().filter(|n| !names.contains(n.as_str())) {
                out.push(Cand {
                    person: m.person.clone(),
                    name: n.clone(),
                    machine: m.machine.clone(),
                    live: false,
                    repo: None,
                    place: Place::Remote(*id, rem.online),
                });
            }
        }
        for route in self.learned_routes(g, remotes) {
            let Some(m) = team.member(&route.machine).filter(|m| m.active) else { continue };
            for name in route.names {
                if out.iter().any(|c| c.name == name && matches!(c.place, Place::Remote(id, _) if id == route.machine))
                {
                    continue;
                }
                out.push(Cand {
                    person: m.person.clone(),
                    name,
                    machine: m.machine.clone(),
                    live: false,
                    repo: None,
                    place: Place::Remote(route.machine, false),
                });
            }
        }
        out
    }

    /// Resolves an address. `exclude` is the sender's session key (not messaged by its own broadcasts).
    pub(super) fn resolve(
        &self,
        g: &super::Inner,
        remotes: &HashMap<EndpointId, Remote>,
        to: &str,
        exclude: Option<&str>,
    ) -> Result<Vec<Target>> {
        let all = self.candidates(g, remotes);
        let (spec, at) = match to.rsplit_once('@') {
            Some((s, m)) if !s.is_empty() && !m.is_empty() => (s, Some(m)),
            _ => (to, None),
        };
        let cands: Vec<&Cand> = all
            .iter()
            .filter(|c| at.is_none_or(|m| c.machine == m))
            .filter(|c| !matches!(&c.place, Place::Local(Some(k)) if Some(k.as_str()) == exclude))
            .filter(|c| !(c.person == self.person && c.name == "*"))
            .collect();
        let mut people: Vec<String> = g.team.as_ref().map(|t| t.people()).unwrap_or_default();
        if !people.contains(&self.person) {
            people.push(self.person.clone());
        }
        let target = |c: &Cand| match &c.place {
            Place::Local(key) => Target::Session { address: self.addr(&c.name), key: key.clone() },
            Place::Remote(id, online) => Target::Remote {
                address: format!("{}/{}", c.person, c.name),
                machine: *id,
                machine_name: c.machine.clone(),
                online: *online,
            },
        };
        let live_matching = |f: &dyn Fn(&Cand) -> bool| -> Vec<Target> {
            cands.iter().filter(|c| c.live && f(c)).map(|c| target(c)).collect()
        };

        let targets = if spec == "*" {
            live_matching(&|_| true)
        } else if let Some(repo) = spec.strip_prefix("repo:") {
            let repo = repo.to_lowercase();
            live_matching(&|c| {
                c.repo.as_deref().is_some_and(|r| {
                    let r = r.to_lowercase();
                    r == repo || r.ends_with(&format!("/{repo}"))
                })
            })
        } else if let Some((person, name)) = spec.split_once('/') {
            if !people.iter().any(|p| p == person) {
                bail!("nobody called '{person}' in the team (people: {})", people.join(", "));
            }
            if name == "*" {
                live_matching(&|c| c.person == person)
            } else {
                self.pick(
                    cands.iter().copied().filter(|c| c.person == person && c.name == name).collect(),
                    spec,
                    &target,
                )?
            }
        } else if people.iter().any(|p| p == spec) && !cands.iter().any(|c| c.name == spec) {
            // Messages go to agents, never to a person.
            let mut theirs: Vec<String> = cands
                .iter()
                .filter(|c| c.person == spec && c.live)
                .map(|c| format!("{}/{}", c.person, c.name))
                .collect();
            theirs.sort();
            theirs.dedup();
            if theirs.is_empty() {
                bail!("messages go to agent sessions, and {spec} has none on Reason Network right now");
            }
            bail!("messages go to agent sessions; {spec}'s are: {}", theirs.join(", "));
        } else {
            self.pick(cands.iter().copied().filter(|c| c.name == spec).collect(), spec, &target)?
        };
        if targets.is_empty() {
            bail!("nobody matches '{to}' right now; run list_team (or `{CLI} who`) to see who is around");
        }
        Ok(targets)
    }

    /// One session among same-named candidates: the live one, else an offline one to queue for.
    fn pick(&self, found: Vec<&Cand>, spec: &str, target: &dyn Fn(&Cand) -> Target) -> Result<Vec<Target>> {
        let live: Vec<&&Cand> = found.iter().filter(|c| c.live).collect();
        let mut pool: Vec<&&Cand> = if live.is_empty() { found.iter().collect() } else { live };
        // Each machine has its own `<you>/terminal`: unnamed on one that has its own, it is that one (another is
        // `…@<machine>`). Anywhere else, two are ambiguous, as two sessions of any one name are.
        let local = |c: &&&Cand| matches!(c.place, Place::Local(_));
        if pool.len() > 1
            && pool.iter().all(|c| c.name == "terminal" && c.person == pool[0].person)
            && pool.iter().any(local)
        {
            pool.retain(local);
        }
        match pool.as_slice() {
            [] => bail!(
                "no session called '{spec}'. Run list_team (or `{CLI} who`) to see who is around (sessions without Reason Network can't \
                 receive)."
            ),
            [one] => Ok(vec![target(one)]),
            many => {
                let opts: Vec<String> = many.iter().map(|c| format!("{}/{}@{}", c.person, c.name, c.machine)).collect();
                bail!("'{spec}' is ambiguous: {}", opts.join(", "))
            }
        }
    }
}

fn claude_match(c: &ClaudeSession, reg: &SessionReg) -> bool {
    (reg.agent_session_id.is_some() && c.session_id == reg.agent_session_id) || reg.pid == Some(c.pid)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn after_clear_the_registry_ends_the_cleared_conversation_before_any_tool_call() {
        let d = Arc::new(super::super::test_daemon(crate::config::Config::default()));
        // A live process, standing in for Claude Code's.
        let pid = std::process::id();
        let reg = |key: &str, agent: &str, cwd: &str, conn: u64| {
            let reg = SessionReg {
                key: key.into(),
                agent: agent.into(),
                cwd: cwd.into(),
                agent_session_id: None,
                pid: Some(pid),
                tier: Tier::Push,
                name: None,
            };
            d.register(Some(conn), reg, GitInfo::default()).unwrap().name
        };
        // A Claude Code session, registered by its MCP server under its process; and another agent run from it.
        assert_eq!(reg("claude-code:before", agent::CLAUDE_CODE, "/work/api", 1), "api-claude");
        reg("codex:1", "codex", "/work/web", 2);
        // `/clear`: Claude Code's registry names the new conversation; its MCP server hasn't made a call since.
        let entry: ClaudeSession = serde_json::from_value(serde_json::json!({
            "pid": pid, "sessionId": "after", "cwd": "/work/api", "kind": "interactive"
        }))
        .unwrap();
        d.claude_seen(&entry, GitInfo::default(), util::now_ms());
        let mut live: Vec<String> = d.inner.lock().unwrap().live.keys().cloned().collect();
        live.sort();
        // Another agent's session from that process stays.
        assert_eq!(live, ["claude-code:after", "codex:1"]);
        // Mail for the cleared conversation waits for it; the new one, api-claude-2, gets none of it.
        let res = d
            .send("codex:1".into(), "api-claude", "for the old one", None, true, false, None, Vec::new())
            .await
            .unwrap();
        assert_eq!(res.recipients[0].state, "queued");
        assert!(
            res.recipients[0].note.ends_with("it reaches that session when it's back"),
            "{}",
            res.recipients[0].note
        );
        let g = d.inner.lock().unwrap();
        assert_eq!(g.live["claude-code:after"].name, "api-claude-2");
        assert_eq!(g.store.unread_count(&d.addr("api-claude-2")).unwrap(), 0);
    }

    #[tokio::test]
    async fn a_headless_claude_run_from_an_interactive_ones_shell_is_a_session_of_its_own() {
        let d = Arc::new(super::super::test_daemon(crate::config::Config::default()));
        // The interactive Claude, in `outer`, in Claude Code's registry.
        let pid = std::process::id();
        let entry: ClaudeSession = serde_json::from_value(serde_json::json!({
            "pid": pid, "sessionId": "outer", "cwd": "/work/api", "kind": "interactive"
        }))
        .unwrap();
        d.claude_seen(&entry, GitInfo::default(), util::now_ms());
        // A headless Claude it ran, in `inner`, with no entry: its MCP server registers it with no process.
        let reg = SessionReg {
            key: "claude-code:inner".into(),
            agent: agent::CLAUDE_CODE.into(),
            cwd: "/work/api".into(),
            agent_session_id: Some("inner".into()),
            pid: None,
            tier: Tier::Pull,
            name: None,
        };
        assert_eq!(d.register(Some(1), reg, GitInfo::default()).unwrap().name, "api-claude-2");
        // The scan, again and again, leaves it alone.
        for _ in 0..3 {
            d.claude_seen(&entry, GitInfo::default(), util::now_ms());
        }
        let mut live: Vec<String> = d.inner.lock().unwrap().live.keys().cloned().collect();
        live.sort();
        assert_eq!(live, ["claude-code:inner", "claude-code:outer"]);
    }

    #[tokio::test]
    async fn the_directory_never_carries_the_team_link() {
        let cfg = crate::config::Config { person: Some("alex".into()), ..Default::default() };
        let d = Arc::new(super::super::test_daemon(cfg));
        let mut team = crate::team::TeamState::found("acme", &d.person, &d.machine, &d.key);
        team.new_link(&d.key);
        let secret = team.link_secret().expect("a team link").to_string();
        d.inner.lock().unwrap().team = Some(team);
        let ticket = d.link(false).await.unwrap().ticket;
        let dir = d.who(None, None, false).await.unwrap();
        // `list_team`; `who --json` (an agent's shell, and the Reason CLI's relay for hosted assistants); `who`.
        let json = serde_json::to_string(&dir).unwrap();
        let agent = crate::render::directory_for_agent(&dir);
        let person = crate::render::directory(&dir, false, crate::render::Paint::PLAIN);
        for out in [&json, &agent, &person] {
            assert!(!out.contains("rn1-") && !out.contains(&ticket) && !out.contains(&secret), "{out}");
        }
        assert!(agent.contains("your user runs `reason-network invite`"), "{agent}");
    }

    #[tokio::test]
    async fn a_long_team_name_is_shown_cut_and_given_whole_to_the_reason_cli() {
        let cfg = crate::config::Config { person: Some("alex".into()), ..Default::default() };
        let d = Arc::new(super::super::test_daemon(cfg));
        let long = format!("acme {}", "y".repeat(200));
        let mut team = crate::team::TeamState::found(&long, &d.person, &d.machine, &d.key);
        team.new_link(&d.key);
        d.inner.lock().unwrap().team = Some(team);
        let dir = d.who(None, None, false).await.unwrap();
        let shown = dir.team.clone().unwrap();
        assert!(shown.chars().count() == 60 && shown.starts_with("acme yyy") && shown.ends_with('…'), "{shown}");
        let agent = crate::render::directory_for_agent(&dir);
        let t = d.team_info().unwrap();
        let person = crate::render::team(&t);
        for out in [&agent, &person] {
            assert!(out.contains(&shown) && !out.contains(&"y".repeat(60)), "{out}");
        }
        // The Reason CLI compares the name with the ones it kept: it gets it whole, as before.
        assert_eq!((t.name.as_str(), t.team_name.as_str()), (long.as_str(), long.as_str()));
        assert_eq!(d.link(false).await.unwrap().team, long);
    }

    #[test]
    fn each_machine_answers_for_its_own_terminal() {
        let cfg = crate::config::Config { person: Some("alex".into()), ..Default::default() };
        let d = Arc::new(super::super::test_daemon(cfg));
        let desk = iroh::SecretKey::generate();
        let mut team = crate::team::TeamState::found("acme", &d.person, &d.machine, &d.key);
        team.put_member(&d.key, desk.public(), "alex", "desk", true);
        d.inner.lock().unwrap().team = Some(team);
        let reg = SessionReg {
            key: "cli:terminal".into(),
            agent: "cli".into(),
            cwd: "/".into(),
            agent_session_id: None,
            pid: None,
            tier: Tier::Pull,
            name: Some("terminal".into()),
        };
        assert_eq!(d.register(Some(1), reg, GitInfo::default()).unwrap().name, "terminal");
        let mut theirs = d.row("terminal", "cli", "/", None);
        theirs.tier = Tier::Pull;
        let remotes = HashMap::from([(
            desk.public(),
            Remote {
                presence: Presence { sessions: vec![theirs], ..Default::default() },
                last_seen: util::now_ms(),
                online: true,
            },
        )]);
        let g = d.inner.lock().unwrap();
        let to = |spec: &str| {
            d.resolve(&g, &remotes, spec, None).unwrap().into_iter().map(|t| match t {
                Target::Session { .. } => "here".to_string(),
                Target::Remote { machine_name, .. } => machine_name,
            })
        };
        assert_eq!(to("alex/terminal").collect::<Vec<_>>(), ["here"]);
        assert_eq!(to("alex/terminal@desk").collect::<Vec<_>>(), ["desk"]);
    }

    #[test]
    fn on_a_teammates_machine_two_terminals_of_one_person_are_ambiguous_and_one_is_found() {
        let cfg = crate::config::Config { person: Some("bob".into()), ..Default::default() };
        let d = super::super::test_daemon(cfg);
        let (laptop, desk) = (iroh::SecretKey::generate(), iroh::SecretKey::generate());
        let mut team = crate::team::TeamState::found("acme", &d.person, &d.machine, &d.key);
        team.put_member(&d.key, laptop.public(), "alex", "laptop", true);
        team.put_member(&d.key, desk.public(), "alex", "desk", true);
        d.inner.lock().unwrap().team = Some(team);
        let mut terminal = d.row("terminal", "cli", "/", None);
        terminal.tier = Tier::Pull;
        let remote = |sessions: Vec<SessionInfo>| Remote {
            presence: Presence { sessions, ..Default::default() },
            last_seen: util::now_ms(),
            online: true,
        };
        let g = d.inner.lock().unwrap();
        // Only one of alex's machines has a terminal on: it is the one, as before.
        let remotes =
            HashMap::from([(laptop.public(), remote(vec![terminal.clone()])), (desk.public(), remote(vec![]))]);
        let found = d.resolve(&g, &remotes, "alex/terminal", None).unwrap();
        assert!(matches!(&found[..], [Target::Remote { machine_name, .. }] if machine_name == "laptop"));
        // Both: which one, by machine (a reply to a message from one names it).
        let remotes =
            HashMap::from([(laptop.public(), remote(vec![terminal.clone()])), (desk.public(), remote(vec![terminal]))]);
        let e = d.resolve(&g, &remotes, "alex/terminal", None).err().unwrap().to_string();
        assert!(
            e.contains("is ambiguous") && e.contains("alex/terminal@laptop") && e.contains("alex/terminal@desk"),
            "{e}"
        );
        assert_eq!(d.resolve(&g, &remotes, "alex/terminal@desk", None).unwrap().len(), 1);
    }

    #[test]
    fn presence_leaves_out_sessions_in_a_folder_that_is_off() {
        let cfg = crate::config::Config {
            folders: [("/work/private".to_string(), "off".to_string())].into(),
            share_unmanaged: true,
            ..Default::default()
        };
        let d = super::super::test_daemon(cfg);
        // Remembered from before the folder was off, and one that wasn't in it.
        for (name, cwd) in [("taxes-claude", "/work/private/taxes"), ("api-claude", "/work/api")] {
            let r = Remembered {
                name: name.into(),
                key: format!("claude-code:{name}"),
                agent: agent::CLAUDE_CODE.into(),
                cwd: cwd.into(),
                repo: None,
                branch: None,
                doing: None,
                tier: Tier::Push,
                last_seen: util::now_ms(),
                attached: true,
                mailbox: false,
            };
            d.inner.lock().unwrap().store.upsert_session(&r, &d.person).unwrap();
        }
        assert_eq!(d.presence_of(Vec::new()).known, ["api-claude"]);
        // Claude Code's own list: neither registered nor shown without Reason Network.
        let claude = |pid: u32, cwd: &str| -> ClaudeSession {
            serde_json::from_value(serde_json::json!({ "pid": pid, "cwd": cwd })).unwrap()
        };
        let shown = d.shared(vec![claude(1, "/work/private"), claude(2, "/work/api"), claude(3, "/work/private/x")]);
        assert_eq!(shown.iter().map(|c| c.pid).collect::<Vec<_>>(), [2]);
        // Nor can a message reach the remembered one.
        let g = d.inner.lock().unwrap();
        let names: Vec<String> = d.candidates(&g, &HashMap::new()).into_iter().map(|c| c.name).collect();
        assert_eq!(names, ["api-claude"]);
    }
}
