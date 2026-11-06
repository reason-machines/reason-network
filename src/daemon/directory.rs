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
    /// The default name a live session here went by before its title named it: it reaches that session by name only.
    former: bool,
}

enum Place {
    Local(Option<String>),
    Remote(EndpointId, bool),
}

impl Daemon {
    /// This machine's sessions: running ones, plus (if `unmanaged`) Claude Code sessions without Reason Network.
    pub(super) async fn local_rows(self: &Arc<Self>, me: Option<&str>, unmanaged: bool) -> Result<Vec<SessionInfo>> {
        // Sessions of Claude Code run with another `CLAUDE_CONFIG_DIR` than ours are listed in that directory.
        let dirs: Vec<std::path::PathBuf> = {
            let g = self.inner.lock().unwrap();
            g.live.values().filter_map(|l| l.reg.claude_dir.as_ref().map(std::path::PathBuf::from)).collect()
        };
        let claude = self.shared(tokio::task::spawn_blocking(move || claude::scan_in(&dirs)).await?);
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
            self.claude_scanned(&claude, &gits, util::now_ms());
        }
        // A Claude Code session someone renamed goes by that name here too, and a title never renames it.
        let renames: Vec<(String, String)> = {
            let g = self.inner.lock().unwrap();
            let chosen: Vec<(String, String)> = g
                .live
                .iter()
                .filter_map(|(k, l)| {
                    let name = util::slug(claude.iter().find(|c| claude_match(c, &l.reg))?.chosen_name()?);
                    (!name.is_empty()).then(|| (k.clone(), name))
                })
                .collect();
            for (key, name) in &chosen {
                if g.live[key].name == *name {
                    let _ = g.store.settle_name(key);
                }
            }
            chosen.into_iter().filter(|(k, n)| g.live[k].name != *n).collect()
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
                let source = doing::Source {
                    agent: l.reg.agent.clone(),
                    session,
                    transcript: l.transcript.clone(),
                    codex_home: l.codex_home.clone().map(Into::into),
                };
                (k.clone(), source)
            });
            let unmanaged = claude.iter().map(|c| {
                let source = doing::Source {
                    agent: agent::CLAUDE_CODE.into(),
                    session: c.session_id.clone(),
                    transcript: None,
                    codex_home: None,
                };
                (format!("pid:{}", c.pid), source)
            });
            live.chain(unmanaged).collect()
        };
        let (doings, titles): (HashMap<String, String>, HashMap<String, String>) =
            tokio::task::spawn_blocking(move || {
                let mut doings = HashMap::new();
                let mut titles = HashMap::new();
                for (k, s) in sources {
                    if let Some(t) = doing::title(&s) {
                        titles.insert(k.clone(), t);
                    }
                    if let Some(d) = doing::of(&s) {
                        doings.insert(k, d);
                    }
                }
                (doings, titles)
            })
            .await?;
        self.name_after_titles(&titles);

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
            for c in claude.iter().filter(|c| !c.elsewhere && !matched.contains(&c.pid)) {
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

    /// A session still on the name it got for asking none (`api-claude`, `api-claude-3`) goes by its folder and what
    /// its conversation is about once its agent has titled it (`api-fix-the-login-bug`), so the chats in one folder can
    /// be told apart, by the person and by the agents they ask to message one. Claude Code's only, which titles each
    /// conversation. Once: a name from a title, or one someone chose (`/rename`, `set_name`), stays. Its mail comes
    /// along, as for any rename, and while it is live the default name stays its own too: mail to the name it was told
    /// at its start still reaches it, and its next hook tells it the new one (see `Live::former`). `-2`, `-3`, … when the
    /// name is taken, and the default name is kept when none of those is free. Not one that mail went to or from: a
    /// reply goes to the name the mail had (see `title_may_name`, checked again under the rename's lock).
    fn name_after_titles(&self, titles: &HashMap<String, String>) {
        let renames: Vec<(String, String)> = {
            let g = self.inner.lock().unwrap();
            g.live
                .iter()
                .filter(|(k, l)| super::sessions::title_may_name(&g, k, l).unwrap_or(false))
                .filter_map(|(k, l)| Some((k.clone(), agent::titled_name(&l.reg.cwd, &l.git, titles.get(k)?)?)))
                .collect()
        };
        for (key, base) in renames {
            let names = std::iter::once(base.clone()).chain((2..10).map(|i| util::joined(&base, &i.to_string())));
            for name in names {
                // Renamed, or no longer to be (named meanwhile): either way, done with it.
                if self.name_from_title(&key, name).is_ok() {
                    break;
                }
            }
        }
    }

    /// Every session of a scan of Claude Code's registries, each `claude_seen`.
    fn claude_scanned(self: &Arc<Self>, claude: &[ClaudeSession], gits: &HashMap<String, GitInfo>, now: i64) {
        // Only our own profile's: the daemon can't post into another's sessions, so it neither lists one there that
        // has no MCP server of its own nor ends one its server registered (`/clear`); those just go by their names
        // there.
        for c in claude.iter().filter(|c| !c.elsewhere) {
            let git = c.cwd.as_ref().and_then(|cwd| gits.get(cwd)).cloned().unwrap_or_default();
            self.claude_seen(c, git, now);
        }
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
                claude_dir: None,
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
        let live: HashSet<&str> =
            g.live.values().flat_map(|l| std::iter::once(l.name.as_str()).chain(l.former.as_deref())).collect();
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
        // A live session's former default name (see `Live::former`) reaches it from teammates' machines too, and stays its
        // own on the person's other machines.
        let formers: Vec<String> = g.live.values().filter_map(|l| l.former.clone()).collect();
        let kept: Vec<String> = formers
            .iter()
            .cloned()
            .chain(known.iter().filter(|r| r.agent != "cli" && self.holds(r, now)).map(|r| r.name.clone()))
            .collect();
        // Teammates' machines take 100 of each, so only the most recent go; the rest are named only here.
        if kept.len() > PRESENCE_NAMES && !self.capped.swap(true, std::sync::atomic::Ordering::Relaxed) {
            super::log(&format!(
                "{} names are kept for offline sessions here: other machines hear of the {PRESENCE_NAMES} most recent",
                kept.len()
            ));
        }
        Presence {
            sessions,
            known: formers.into_iter().chain(known.into_iter().map(|r| r.name)).take(PRESENCE_NAMES).collect(),
            kept: kept.into_iter().take(PRESENCE_NAMES).collect(),
            features: vec![crate::files::FEATURE.into(), super::sessions::TERMINALS.into()],
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
            for (name, former) in std::iter::once((&l.name, false)).chain(l.former.iter().map(|f| (f, true))) {
                out.push(Cand {
                    person: self.person.clone(),
                    name: name.clone(),
                    machine: self.machine.clone(),
                    live: true,
                    repo: l.git.repo.clone(),
                    place: Place::Local(Some(l.reg.key.clone())),
                    former,
                });
            }
        }
        for r in self.remembered(g) {
            out.push(Cand {
                person: self.person.clone(),
                name: r.name,
                machine: self.machine.clone(),
                live: false,
                repo: r.repo,
                place: Place::Local(None),
                former: false,
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
                    former: false,
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
                    former: false,
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
            // A live session at the name it goes by now, as its mail is kept.
            Place::Local(key) => Target::Session {
                address: self.addr(key.as_ref().and_then(|k| g.live.get(k)).map_or(&c.name, |l| &l.name)),
                key: key.clone(),
            },
            Place::Remote(id, online) => Target::Remote {
                address: format!("{}/{}", c.person, c.name),
                machine: *id,
                machine_name: c.machine.clone(),
                online: *online,
            },
        };
        let live_matching = |f: &dyn Fn(&Cand) -> bool| -> Vec<Target> {
            cands.iter().filter(|c| c.live && !c.former && f(c)).map(|c| target(c)).collect()
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
                claude_dir: None,
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
    async fn a_claude_session_in_another_claude_config_dir_goes_by_the_name_given_there() {
        let d = Arc::new(super::super::test_daemon(crate::config::Config::default()));
        // `CLAUDE_CONFIG_DIR=~/.claude-b claude --name my-session`: its registry is in that profile, not the daemon's.
        let profile = std::env::temp_dir().join(format!("reason-network-claude-b-{}", std::process::id()));
        std::fs::create_dir_all(profile.join("sessions")).unwrap();
        let pid = std::process::id();
        let entry = serde_json::json!({
            "pid": pid, "sessionId": "b", "cwd": "/work/api", "name": "my-session", "nameSource": "user"
        });
        std::fs::write(profile.join("sessions").join(format!("{pid}.json")), entry.to_string()).unwrap();
        let reg = SessionReg {
            key: "claude-code:b".into(),
            agent: agent::CLAUDE_CODE.into(),
            cwd: "/work/api".into(),
            agent_session_id: Some("b".into()),
            pid: Some(pid),
            tier: Tier::Push,
            name: None,
            claude_dir: Some(profile.to_string_lossy().into_owned()),
        };
        assert_eq!(d.register(Some(1), reg, GitInfo::default()).unwrap().name, "api-claude");
        let rows = d.local_rows(None, false).await.unwrap();
        let _ = std::fs::remove_dir_all(&profile);
        let names: Vec<&str> = rows.iter().map(|r| r.address.as_str()).collect();
        assert!(names.iter().any(|a| a.ends_with("/my-session")), "{names:?}");
    }

    /// An entry of another profile's registry (`CLAUDE_CONFIG_DIR=~/.claude-b`), as `claude::scan_in` reads it.
    fn elsewhere(entry: serde_json::Value) -> ClaudeSession {
        ClaudeSession { elsewhere: true, ..serde_json::from_value(entry).unwrap() }
    }

    #[tokio::test]
    async fn a_clear_in_another_claude_config_dir_keeps_its_session_pushed() {
        let d = Arc::new(super::super::test_daemon(crate::config::Config::default()));
        let pid = std::process::id();
        // Its MCP server registered it, with its connection, from the other profile.
        let reg = SessionReg {
            key: "claude-code:before".into(),
            agent: agent::CLAUDE_CODE.into(),
            cwd: "/work/api".into(),
            agent_session_id: Some("before".into()),
            pid: Some(pid),
            tier: Tier::Push,
            name: None,
            claude_dir: Some("/home/u/.claude-b".into()),
        };
        assert_eq!(d.register(Some(1), reg, GitInfo::default()).unwrap().name, "api-claude");
        // `/clear` there: that profile's registry names the new conversation; the daemon can't post into it.
        let entry = elsewhere(serde_json::json!({
            "pid": pid, "sessionId": "after", "cwd": "/work/api", "messagingSocketPath": "/tmp/b.sock"
        }));
        d.claude_scanned(&[entry], &HashMap::new(), util::now_ms());
        let g = d.inner.lock().unwrap();
        let live: Vec<&String> = g.live.keys().collect();
        assert_eq!(live, ["claude-code:before"]);
        assert_eq!(g.live["claude-code:before"].reg.tier, Tier::Push);
        assert!(g.live["claude-code:before"].conn.is_some());
    }

    #[tokio::test]
    async fn a_claude_session_in_another_claude_config_dir_without_reason_network_is_not_listed() {
        let d = Arc::new(super::super::test_daemon(crate::config::Config::default()));
        let codex = SessionReg {
            key: "codex:1".into(),
            agent: "codex".into(),
            cwd: "/work/web".into(),
            agent_session_id: None,
            pid: None,
            tier: Tier::Pull,
            name: None,
            claude_dir: None,
        };
        d.register(Some(1), codex, GitInfo::default()).unwrap();
        // A session in the other profile with no MCP server of ours: only its registry entry knows of it.
        let entry = elsewhere(serde_json::json!({
            "pid": std::process::id(), "sessionId": "b", "cwd": "/work/api", "name": "unmanaged-b",
            "nameSource": "user", "messagingSocketPath": "/tmp/b.sock"
        }));
        d.claude_scanned(&[entry], &HashMap::new(), util::now_ms());
        let live: Vec<String> = d.inner.lock().unwrap().live.keys().cloned().collect();
        assert_eq!(live, ["codex:1"]);
        let err = d.send("codex:1".into(), "unmanaged-b", "hi", None, true, false, None, Vec::new()).await.unwrap_err();
        assert!(err.to_string().starts_with("no session called 'unmanaged-b'"), "{err:#}");
    }

    #[tokio::test]
    async fn the_directory_lists_no_session_of_another_claude_config_dir_that_no_mcp_server_registered() {
        let d = Arc::new(super::super::test_daemon(crate::config::Config::default()));
        let profile = std::env::temp_dir().join(format!("reason-network-claude-b-unmanaged-{}", std::process::id()));
        std::fs::create_dir_all(profile.join("sessions")).unwrap();
        let pid = std::process::id();
        // Another session of that profile registered it (no process of its own, so nothing matches it by pid).
        let reg = SessionReg {
            key: "claude-code:managed".into(),
            agent: agent::CLAUDE_CODE.into(),
            cwd: "/work/web".into(),
            agent_session_id: Some("managed".into()),
            pid: None,
            tier: Tier::Push,
            name: None,
            claude_dir: Some(profile.to_string_lossy().into_owned()),
        };
        d.register(Some(1), reg, GitInfo::default()).unwrap();
        // A session there with no MCP server of ours: only that profile's registry knows of it.
        let entry = serde_json::json!({
            "pid": pid, "sessionId": "unmanaged", "cwd": "/work/api", "name": "unmanaged-b", "nameSource": "user"
        });
        std::fs::write(profile.join("sessions").join(format!("{pid}.json")), entry.to_string()).unwrap();
        let rows = d.local_rows(None, true).await.unwrap();
        let _ = std::fs::remove_dir_all(&profile);
        let names: Vec<&str> = rows.iter().map(|r| r.address.as_str()).collect();
        assert!(!names.iter().any(|a| a.ends_with("/unmanaged-b")), "{names:?}");
        assert!(names.iter().any(|a| a.ends_with("/web-claude")), "{names:?}");
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
            claude_dir: None,
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
    async fn chats_in_one_folder_go_by_what_they_are_about_once_titled() {
        let d = Arc::new(super::super::test_daemon(crate::config::Config::default()));
        let dir = std::env::temp_dir().join(format!("reason-network-titles-{}", ulid::Ulid::generate()));
        std::fs::create_dir_all(&dir).unwrap();
        let reg = |id: &str, conn: u64| {
            let reg = SessionReg {
                key: format!("claude-code:{id}"),
                agent: agent::CLAUDE_CODE.into(),
                cwd: "/work/api".into(),
                agent_session_id: Some(id.into()),
                pid: None,
                tier: Tier::Pull,
                name: None,
                claude_dir: None,
            };
            d.register(Some(conn), reg, GitInfo::default()).unwrap().name
        };
        let titled = |id: &str, title: &str| {
            let path = dir.join(format!("{id}.jsonl"));
            let line = serde_json::json!({"type": "ai-title", "aiTitle": title, "sessionId": id});
            std::fs::write(&path, format!("{line}\n")).unwrap();
            d.inner.lock().unwrap().live.get_mut(&format!("claude-code:{id}")).unwrap().transcript = Some(path);
        };
        // Four chats in one folder: what they are called before Claude Code titles them.
        assert_eq!(reg("a", 1), "api-claude");
        assert_eq!(reg("b", 2), "api-claude-2");
        assert_eq!(reg("c", 3), "api-claude-3");
        assert_eq!(reg("d", 4), "api-claude-4");
        // The person named one themselves; two get the same title.
        d.set_status("claude-code:c", None, Some("review".into())).unwrap();
        titled("a", "Fix the login bug on Safari");
        titled("b", "Add CSV export to reports");
        titled("c", "Something else entirely");
        titled("d", "Fix the login bug on Safari");
        let rows = d.local_rows(None, false).await.unwrap();
        let name = |id: &str| d.inner.lock().unwrap().live[&format!("claude-code:{id}")].name.clone();
        let mut same = [name("a"), name("d")];
        same.sort();
        assert_eq!(same, ["api-fix-the-login-bug", "api-fix-the-login-bug-2"]);
        assert_eq!(name("b"), "api-add-csv-export-to");
        assert_eq!(name("c"), "review");
        // The directory (list_team, `who`) shows the new names.
        let mut shown: Vec<&str> = rows.iter().map(|r| r.name.as_str()).collect();
        shown.sort();
        assert_eq!(shown, ["api-add-csv-export-to", "api-fix-the-login-bug", "api-fix-the-login-bug-2", "review"]);
        // A later title leaves a titled name alone.
        titled("b", "Something new");
        d.local_rows(None, false).await.unwrap();
        assert_eq!(name("b"), "api-add-csv-export-to");
        let is_default = |n: &str| agent::is_default_name(n, agent::CLAUDE_CODE, "/work/api", &GitInfo::default());
        // A title that reads like a default name (`Claude 7`) is still a title: the next one leaves it alone.
        assert!(is_default(&reg("e", 5)));
        titled("e", "Claude 7");
        d.local_rows(None, false).await.unwrap();
        assert_eq!(name("e"), "api-claude-7");
        titled("e", "Something new");
        d.local_rows(None, false).await.unwrap();
        assert_eq!(name("e"), "api-claude-7");
        // A name someone chose stays, even one that reads like a default. (Not `api-claude-3`: the chat titled
        // `Claude 7` went by that, and keeps it while it is live.)
        reg("f", 6);
        assert!(d.set_status("claude-code:f", None, Some("api-claude-3".into())).is_err());
        d.set_status("claude-code:f", None, Some("api-claude-8".into())).unwrap();
        titled("f", "Rename me please");
        d.local_rows(None, false).await.unwrap();
        assert_eq!(name("f"), "api-claude-8");
        // So does one its MCP server asks for, after its hooks registered it under that name.
        let first = reg("i", 9);
        let asked = SessionReg {
            key: "claude-code:i".into(),
            agent: agent::CLAUDE_CODE.into(),
            cwd: "/work/api".into(),
            agent_session_id: Some("i".into()),
            pid: None,
            tier: Tier::Pull,
            name: Some(first.clone()),
            claude_dir: None,
        };
        d.register(Some(10), asked, GitInfo::default()).unwrap();
        titled("i", "Rename me too");
        d.local_rows(None, false).await.unwrap();
        assert_eq!(name("i"), first);
        // And the name of one that mail went from or to: a reply goes to the name the mail had.
        let sender = reg("g", 7);
        d.send("claude-code:g".into(), "review", "done?", None, true, false, None, Vec::new()).await.unwrap();
        let receiver = reg("j", 11);
        d.send("claude-code:c".into(), &receiver, "how is it going?", None, true, false, None, Vec::new())
            .await
            .unwrap();
        titled("g", "Ship the release notes");
        titled("j", "Write the changelog");
        d.local_rows(None, false).await.unwrap();
        assert_eq!(name("g"), sender);
        assert_eq!(name("j"), receiver);
        assert_eq!(d.inner.lock().unwrap().store.unread_count(&d.addr(&receiver)).unwrap(), 1);
        // A title decided before someone named the session never overrides that name (checked under the rename's lock).
        reg("h", 8);
        d.set_status("claude-code:h", None, Some("mine".into())).unwrap();
        assert!(d.name_from_title("claude-code:h", "api-stale-title".into()).unwrap().is_none());
        assert_eq!(name("h"), "mine");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn a_chat_its_title_named_keeps_the_address_it_was_told() {
        let d = Arc::new(super::super::test_daemon(crate::config::Config::default()));
        let dir = std::env::temp_dir().join(format!("reason-network-former-{}", ulid::Ulid::generate()));
        std::fs::create_dir_all(&dir).unwrap();
        let reg = |agent: &str, id: &str, conn: u64, name: Option<&str>| {
            let reg = SessionReg {
                key: format!("{agent}:{id}"),
                agent: agent.into(),
                cwd: "/work/api".into(),
                agent_session_id: Some(id.into()),
                pid: None,
                tier: Tier::Pull,
                name: name.map(str::to_string),
                claude_dir: None,
            };
            d.register(Some(conn), reg, GitInfo::default()).unwrap().name
        };
        let titled = |key: &str, title: &str| {
            let path = dir.join(format!("{}.jsonl", key.replace(':', "-")));
            let line = serde_json::json!({"type": "ai-title", "aiTitle": title});
            std::fs::write(&path, format!("{line}\n")).unwrap();
            d.inner.lock().unwrap().live.get_mut(key).unwrap().transcript = Some(path);
        };
        let name = |key: &str| d.inner.lock().unwrap().live[key].name.clone();
        let hook = |event: &str| crate::proto::HookCall {
            through_reason: Some(false),
            agent: agent::CLAUDE_CODE.into(),
            event: event.into(),
            session_id: "a".into(),
            cwd: Some("/work/api".into()),
            pid: None,
            mail: false,
            transcript: None,
            doing: None,
            name: None,
            dirs: Vec::new(),
            keep: false,
            codex_home: None,
        };
        reg("claude-code", "sender", 1, Some("sender"));
        // Told at its start who it is: `api-claude`.
        assert_eq!(reg(agent::CLAUDE_CODE, "a", 2, None), "api-claude");
        assert_eq!(d.hook(hook("start")).await.unwrap().address.as_deref(), Some(d.addr("api-claude").as_str()));
        titled("claude-code:a", "Fix the login bug on Safari");
        d.local_rows(None, false).await.unwrap();
        let now = d.addr("api-fix-the-login-bug");
        assert_eq!(name("claude-code:a"), "api-fix-the-login-bug");
        // The name it was told stays its own while it is live: a new chat in the folder gets another, nobody can take
        // it, and the person's other machines hear it is taken.
        assert_eq!(reg(agent::CLAUDE_CODE, "z", 3, None), "api-claude-2");
        assert!(d.set_status("claude-code:z", None, Some("api-claude".into())).is_err());
        let presence = d.presence_of(Vec::new());
        assert!(
            presence.known.contains(&"api-claude".to_string()) && presence.kept.contains(&"api-claude".to_string())
        );
        // Mail to the address it was told reaches it, at its name now, not the new chat.
        let sent =
            d.send("claude-code:sender".into(), "api-claude", "still there?", None, true, false, None, Vec::new());
        let sent = sent.await.unwrap();
        assert_eq!(sent.recipients[0].address, now);
        {
            let g = d.inner.lock().unwrap();
            assert_eq!(g.store.unread_count(&now).unwrap(), 1);
            assert_eq!(g.store.unread_count(&d.addr("api-claude-2")).unwrap(), 0);
            assert_eq!(g.store.unread_count(&d.addr("api-claude")).unwrap(), 0);
        }
        // A list of everyone (`*`) reaches it once, by its name now.
        {
            let g = d.inner.lock().unwrap();
            let all = d.resolve(&g, &HashMap::new(), "*", Some("claude-code:sender")).unwrap();
            let mut all: Vec<String> = all.iter().map(|t| t.address()).collect();
            all.sort();
            assert_eq!(all, [d.addr("api-claude-2"), now.clone()]);
        }
        // Its next turn's hook tells it its new address, once.
        let told = d.hook(hook("prompt")).await.unwrap();
        let text = told.text.unwrap();
        assert!(text.contains(&format!("you are {now} now")) && text.contains(&d.addr("api-claude")), "{text}");
        assert_eq!(told.address.as_deref(), Some(now.as_str()));
        assert_eq!(d.hook(hook("prompt")).await.unwrap().text, None);
        // Once it is gone, the name is free again.
        d.inner.lock().unwrap().live.remove("claude-code:a");
        assert_eq!(reg(agent::CLAUDE_CODE, "y", 4, None), "api-claude");
        // Only Claude Code's chats go by their titles.
        let codex = reg("codex", "x", 5, None);
        titled("codex:x", "Fix the login bug on Safari");
        d.local_rows(None, false).await.unwrap();
        assert_eq!(name("codex:x"), codex);
        let _ = std::fs::remove_dir_all(&dir);
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
            claude_dir: None,
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
