//! Sending, receiving and delivering messages.
//!
//! Delivery is at least once: a message stays unread until whoever showed it to the agent confirms
//! with `Delivered`. Messages handed to a push channel or a hook are "in flight" meanwhile, so the inbox and hooks
//! don't show them a second time.

use super::{Daemon, Inner, directory::Target, log};
use crate::{
    agent, claude,
    config::Policy,
    files,
    proto::*,
    render,
    store::{Counted, Remembered},
    team::Signed,
    util,
};
use anyhow::{Result, anyhow, bail};
use iroh::EndpointId;
use std::{
    sync::Arc,
    time::{Duration, Instant},
};
use ulid::Ulid;

const MAX_BODY: usize = 64 * 1024;
/// Messages per minute accepted from one teammate's machine; more are deferred, not dropped.
const RATE_LIMIT: usize = 60;
/// Agent-to-agent messages allowed between one pair in `LOOP_WINDOW`; past that it is probably a loop.
const LOOP_LIMIT: usize = 12;
const LOOP_WINDOW: Duration = Duration::from_secs(600);
/// A push or hook not confirmed within this long is shown through the inbox and hooks again.
const IN_FLIGHT_TTL: Duration = Duration::from_secs(90);

/// How a session on this machine gets a message into its conversation.
enum Channel {
    /// Its `reason network mcp` posts into Claude Code's session socket.
    ClaudeSocket(u64),
    /// The agent's own CLI: `codex queue --thread <id> --message`, `reason session send <id> --message`.
    Command(&'static str, Vec<String>),
    /// Straight into a Claude Code session's messaging socket (one without Reason Network's tools: it replies with the CLI).
    ClaudeDirect(agent::ClaudeSocket),
    /// Hooks inject it at the next tool call or turn end.
    Hook,
    /// Only `check_inbox`.
    Pull,
}

impl Inner {
    /// Unread mail for an address that isn't already on its way through a push or a hook.
    pub(super) fn pending(&mut self, address: &str) -> Result<Vec<Message>> {
        self.in_flight.retain(|_, at| at.elapsed() < IN_FLIGHT_TTL);
        let msgs = self.store.unread(address)?;
        Ok(msgs.into_iter().filter(|m| !self.in_flight.contains_key(&m.id)).collect())
    }

    /// Records an agent-to-agent message; false if this pair has been talking suspiciously much.
    fn loop_ok(&mut self, from: &str, to: &str) -> bool {
        // A person typing in a terminal (`<person>/terminal`) isn't an agent loop (as before, when such messages came
        // from the person).
        if from.ends_with("/terminal") {
            return true;
        }
        let q = self.pairs.entry((from.to_string(), to.to_string())).or_default();
        while q.front().is_some_and(|t| t.elapsed() > LOOP_WINDOW) {
            q.pop_front();
        }
        if q.len() >= LOOP_LIMIT {
            return false;
        }
        q.push_back(Instant::now());
        true
    }
}

/// A message to send to a teammate's machine now: where its result goes, the machine, its name, whether it is
/// connected, and the message.
type RemoteJob = (usize, EndpointId, String, bool, Message);

const UPGRADE: &str =
    "update Reason Network (with the Reason CLI, `reason update`): messages go between agent sessions now";

const LOOP_NOTE: &str = "refused: these two agents have exchanged a lot of messages in the last few minutes, which \
                         looks like a loop. Check with your user before continuing.";

/// The note for mail queued for a session that is offline. Its unread mail is deleted at the first cleanup a day after
/// it was last seen, unless it is one of the person's own mailboxes (`cli`), which are kept. Until then it waits for
/// that session, when the session can come back to its name (see [`Remembered::kept`]); else a new session that takes
/// the name gets it.
fn offline_note(r: Option<&Remembered>, now: i64) -> String {
    const NAME: &str = "a new session that takes this name gets the mail";
    const BACK: &str = "it reaches that session when it's back";
    let Some(r) = r else { return format!("queued: the session is offline; {NAME}") };
    let who = if r.kept() { BACK } else { NAME };
    let seen = util::ago(r.last_seen, now);
    let forgotten = r.last_seen.saturating_add(super::sessions::FORGET_MS);
    if r.agent == "cli" {
        format!("queued: last seen {seen} ago; {who}")
    } else if forgotten <= now {
        format!("queued: last seen {seen} ago; unread mail may be dropped any time; {who}")
    } else {
        let left = util::ago(now, forgotten);
        format!("queued: last seen {seen} ago; unread mail is dropped after about {left} if it stays offline; {who}")
    }
}

impl Daemon {
    #[allow(clippy::too_many_arguments)]
    pub(super) async fn send(
        self: &Arc<Self>,
        from: String,
        to: &str,
        body: &str,
        reply_to: Option<String>,
        wake: bool,
        notify: bool,
        approval: Option<Box<Signed<Approval>>>,
        files: Vec<Attachment>,
    ) -> Result<SendResult> {
        let (body, to) = (body.trim(), to.trim());
        if body.is_empty() {
            bail!("empty message");
        }
        if notify && is_group(to) {
            bail!("notify is for one session or one person, not a group");
        }
        if body.len() > MAX_BODY {
            // A path means nothing on the recipient's computer.
            bail!(
                "message is {} KB; the limit is {} KB (send a small text file with `files`, or a link)",
                body.len().div_ceil(1024),
                MAX_BODY / 1024
            );
        }
        let files = files::check(files)?;
        let now = util::now_ms();
        let net = self.net();
        let remotes = net.as_ref().map(|n| n.remotes()).unwrap_or_default();

        let mut res = SendResult { ids: Vec::new(), recipients: Vec::new() };
        let mut remote_jobs = Vec::new();
        {
            let mut g = self.inner.lock().unwrap();
            self.prune(&mut g);
            // Messages come from agent sessions: a session key, or (from agents that reply through the CLI, like
            // Reason's) its address or name here.
            let k = from.as_str();
            let sender = g
                .live
                .get(k)
                .or_else(|| g.live.values().find(|l| self.addr(&l.name) == k || l.name == k))
                .ok_or_else(unregistered)?;
            let (from_addr, from_agent, from_repo) =
                (self.addr(&sender.name), Some(sender.reg.agent.clone()), sender.git.repo.clone());
            let exclude = sender.reg.key.clone();
            // A person at a terminal may message a group; an agent picks the one session it needs.
            if is_group(to) && !from_addr.ends_with("/terminal") {
                bail!("agents message one session, not a group: pick it with list_team, or ask your user");
            }
            // Your own user, not a session: just the pop-up, on each of their machines that is on now. (To get a
            // teammate's attention, message one of their sessions with notify.)
            let own = notify && to == self.person;
            if own && !files.is_empty() {
                bail!("files go to a session: send them to one of {to}'s sessions");
            }
            if own {
                let msg = Message {
                    id: Ulid::generate().to_string(),
                    from: from_addr.clone(),
                    from_agent: from_agent.clone(),
                    from_repo: from_repo.clone(),
                    to: to.to_string(),
                    via: None,
                    reply_to: reply_to.clone(),
                    body: body.to_string(),
                    wake: false,
                    notify: true,
                    sent_at: now,
                    approval: None,
                    files: Vec::new(),
                };
                let (sent, note) = self.notify(&msg);
                let state = if sent { "notified" } else { "not notified" };
                res.recipients.push(Recipient { address: format!("{to}@{}", self.machine), state: state.into(), note });
                for m in g.team.iter().flat_map(|t| t.active()).filter(|m| m.person == to) {
                    // Machines we've never heard from (and this one) aren't in `remotes`.
                    let Some(online) = remotes.get(&m.endpoint).map(|r| r.online) else { continue };
                    let address = format!("{to}@{}", m.machine);
                    let note = if online {
                        format!("no answer from {}", m.machine)
                    } else {
                        format!("{} is offline", m.machine)
                    };
                    if online {
                        remote_jobs.push((res.recipients.len(), m.endpoint, m.machine.clone(), true, msg.clone()));
                    }
                    res.recipients.push(Recipient { address, state: "not notified".into(), note });
                }
            }
            let targets = if own { Vec::new() } else { self.resolve(&g, &remotes, to, Some(&exclude))? };
            for t in targets {
                let mut msg = Message {
                    id: Ulid::generate().to_string(),
                    from: from_addr.clone(),
                    from_agent: from_agent.clone(),
                    from_repo: from_repo.clone(),
                    to: t.address(),
                    via: is_group(to).then(|| to.to_string()),
                    reply_to: reply_to.clone(),
                    body: body.to_string(),
                    wake,
                    notify,
                    sent_at: now,
                    approval: approval.clone(),
                    files: files.clone(),
                };
                res.ids.push(msg.id.clone());
                let here = !matches!(t, Target::Remote { .. });
                let (state, mut note): (String, String) = match t {
                    _ if !matches!(t, Target::Remote { .. }) && !g.loop_ok(&msg.from, &msg.to) => {
                        ("refused".into(), LOOP_NOTE.into())
                    }
                    Target::Session { key: Some(key), .. } => {
                        g.store.insert(&mut msg, false)?;
                        self.deliver(&mut g, &key, vec![msg.clone()], wake)
                    }
                    Target::Session { key: None, address } => {
                        let name = address.split_once('/').map_or("", |(_, n)| n);
                        let note = offline_note(g.store.session_by_name(name)?.as_ref(), now);
                        g.store.insert(&mut msg, false)?;
                        ("queued".into(), note)
                    }
                    Target::Remote { machine, .. } => {
                        let takes_files = remotes.get(&machine).is_some_and(|r| r.presence.has(files::FEATURE));
                        match for_machine(msg.clone(), takes_files) {
                            Ok(m) => {
                                msg = m;
                                self.queue_remote(&mut g, t, &msg, &mut remote_jobs, res.recipients.len())?
                            }
                            Err(note) => ("refused".into(), note),
                        }
                    }
                };
                // Counting never stops a message. One to another machine counts once that machine takes it.
                if here && state != "refused" {
                    let _ = g.store.count(now, Counted::Sent);
                    let _ = g.store.count(now, Counted::Received);
                }
                if notify && here && state != "refused" {
                    note = format!("{note}; {}", self.notify(&msg).1);
                }
                res.recipients.push(Recipient { address: msg.to.clone(), state, note });
            }
        }
        self.arrived.notify_waiters();

        if let Some(net) = net {
            let sends = remote_jobs.into_iter().filter(|j| j.3).map(|(i, machine, name, _, msg)| {
                let net = net.clone();
                async move {
                    let verdict = net.send_msg(&machine, &msg).await;
                    (i, name, verdict)
                }
            });
            for (i, name, verdict) in futures_join(sends).await {
                let r = &mut res.recipients[i];
                match verdict {
                    // A pop-up only (see above): nothing kept to retry.
                    Some((state, _)) if r.state == "not notified" && state == "retry" => {
                        r.note = format!("{name} is busy")
                    }
                    Some((state, note)) => {
                        r.state = state;
                        r.note = note;
                    }
                    None if r.state == "not notified" => r.note = format!("no answer from {name}"),
                    None => r.note = format!("queued: no answer from {name} yet; it will be retried"),
                }
            }
        }
        Ok(res)
    }

    /// Puts a message for a teammate's machine in the outbox, and on the list to send now if it is connected.
    fn queue_remote(
        &self,
        g: &mut Inner,
        t: Target,
        msg: &Message,
        jobs: &mut Vec<RemoteJob>,
        at: usize,
    ) -> Result<(String, String)> {
        let Target::Remote { machine, machine_name, online, .. } = t else { unreachable!("only remote targets") };
        g.store.outbox_add(&machine.to_string(), msg)?;
        let note = format!("queued: not connected to {machine_name} right now; it goes out as soon as it is");
        jobs.push((at, machine, machine_name, online, msg.clone()));
        Ok(("queued".into(), note))
    }

    /// Hands messages to a session here by the fastest way it has. Returns the state and a note for the sender.
    pub(super) fn deliver(
        self: &Arc<Self>,
        g: &mut Inner,
        key: &str,
        msgs: Vec<Message>,
        wake: bool,
    ) -> (String, String) {
        let Some(l) = g.live.get(key) else {
            let note = offline_note(g.store.session_by_key(key).ok().flatten().as_ref(), util::now_ms());
            return ("queued".into(), note);
        };
        if !wake {
            return ("queued".into(), "queued as FYI: it reads it at its next natural pause".into());
        }
        if g.polling.get(key).is_some_and(|n| *n > 0) {
            return ("delivered".into(), "delivered: the session was waiting for messages".into());
        }
        // Reason didn't take the last push: wait out its backoff (its sync pushes again when it allows).
        if l.reg.agent == "reason" && l.retry.is_some_and(|(at, _)| Instant::now() < at) {
            return ("queued".into(), "queued: Reason didn't take the last delivery; it is retried shortly".into());
        }
        let has = |bin: &str| !l.push_failed && agent::find_bin(bin).is_some();
        let channel = match (l.reg.tier, l.conn, &l.reg.agent_session_id) {
            (Tier::Push, Some(conn), _) if l.reg.agent == agent::CLAUDE_CODE => Channel::ClaudeSocket(conn),
            (Tier::Push, _, Some(id)) if l.reg.agent == "codex" && has("codex") => {
                Channel::Command("codex", vec!["queue".into(), "--thread".into(), id.clone(), "--message".into()])
            }
            (Tier::Push, _, Some(id)) if l.reg.agent == "reason" && has("reason") => {
                Channel::Command("reason", vec!["session".into(), "send".into(), id.clone(), "--message".into()])
            }
            (Tier::Push, None, id) if l.reg.agent == agent::CLAUDE_CODE && !l.push_failed => {
                match claude::socket_for(id.as_deref(), l.reg.pid) {
                    Some(sock) => Channel::ClaudeDirect(sock),
                    None => Channel::Pull,
                }
            }
            _ if l.last_hook.is_some() => Channel::Hook,
            _ => Channel::Pull,
        };
        let ids: Vec<String> = msgs.iter().map(|m| m.id.clone()).collect();
        let cli = matches!(channel, Channel::ClaudeDirect(_));
        let text = render::for_agent(&msgs, Some(&l.reg.agent), cli);
        let claude_session = l.reg.agent_session_id.clone();
        let hook_note = if matches!(l.reg.agent.as_str(), "opencode" | "pi") {
            "queued: delivered within seconds once the session is idle"
        } else if l.reg.agent == "cli" {
            "queued: it gets it the next time it listens"
        } else {
            "queued: it sees it at its next tool call or when its turn ends"
        };
        match channel {
            Channel::ClaudeSocket(conn) => {
                let Some(tx) = g.conns.get(&conn) else {
                    return ("queued".into(), "queued: this agent only sees it when it checks its inbox".into());
                };
                let ev = Event::Deliver { key: key.to_string(), ids: ids.clone(), text };
                let _ = tx.send(serde_json::to_string(&ev).expect("event serializes"));
                for id in ids {
                    g.in_flight.insert(id, Instant::now());
                }
                (
                    "pushed".into(),
                    "delivered into the session: it reads it between tool calls, or wakes up if idle".into(),
                )
            }
            Channel::Command(bin, args) => {
                for id in &ids {
                    g.in_flight.insert(id.clone(), Instant::now());
                }
                self.send_with(bin, args, key.to_string(), ids, text);
                (
                    "pushed".into(),
                    format!(
                        "queued into the {} session: it runs after the current turn, or now if idle",
                        agent::label(bin)
                    ),
                )
            }
            Channel::ClaudeDirect(sock) => {
                for id in &ids {
                    g.in_flight.insert(id.clone(), Instant::now());
                }
                self.post_direct(key.to_string(), sock, ids, text);
                let note = claude::held_note(claude_session.as_deref())
                    .unwrap_or("delivered into the session: it reads it between tool calls, or wakes up if idle");
                ("pushed".into(), note.into())
            }
            Channel::Hook => ("queued".into(), hook_note.into()),
            Channel::Pull => ("queued".into(), "queued: this agent only sees it when it checks its inbox".into()),
        }
    }

    /// Posts into a Claude Code session's messaging socket; read once it took it.
    fn post_direct(self: &Arc<Self>, key: String, sock: agent::ClaudeSocket, ids: Vec<String>, text: String) {
        let this = self.clone();
        tokio::spawn(async move {
            let res = tokio::task::spawn_blocking(move || agent::post_to_claude(&sock, &text)).await;
            let mut g = this.inner.lock().unwrap();
            for id in &ids {
                g.in_flight.remove(id);
            }
            match res {
                Ok(Ok(())) => {
                    let _ = g.store.mark_read(&ids, util::now_ms());
                }
                Ok(Err(e)) => log(&format!("delivering to Claude session {key}: {e}")),
                Err(e) => log(&format!("delivering to Claude session {key}: {e}")),
            }
        });
    }

    /// Ends `key`'s `check_inbox` wait (its client gave up on it), then pushes what arrived meanwhile the usual way:
    /// while it was waiting, pushes were skipped.
    pub(super) async fn stop_waiting(self: &Arc<Self>, key: &str) {
        if let Some(l) = self.inner.lock().unwrap().live.get_mut(key) {
            l.stop_gen += 1;
        }
        self.arrived.notify_waiters();
        for _ in 0..40 {
            if !self.inner.lock().unwrap().polling.contains_key(key) {
                break;
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
        let mut g = self.inner.lock().unwrap();
        let Some(address) = g.live.get(key).map(|l| self.addr(&l.name)) else { return };
        let waiting: Vec<Message> = g.pending(&address).unwrap_or_default().into_iter().filter(|m| m.wake).collect();
        if !waiting.is_empty() {
            self.deliver(&mut g, key, waiting, true);
        }
    }

    /// Hands `text` to the agent's own CLI (`args` end with its message flag).
    fn send_with(self: &Arc<Self>, bin: &'static str, args: Vec<String>, key: String, ids: Vec<String>, text: String) {
        let this = self.clone();
        tokio::spawn(async move {
            let run = async {
                let path = agent::find_bin(bin).ok_or_else(|| format!("{bin} not found"))?;
                let out = tokio::process::Command::new(path)
                    .args(&args)
                    .arg(&text)
                    .stdin(std::process::Stdio::null())
                    .kill_on_drop(true)
                    .output();
                match tokio::time::timeout(Duration::from_secs(30), out).await {
                    Ok(Ok(o)) if o.status.success() => Ok(()),
                    Ok(Ok(o)) => Err(String::from_utf8_lossy(&o.stderr).trim().to_string()),
                    Ok(Err(e)) => Err(e.to_string()),
                    Err(_) => Err("timed out".into()),
                }
            };
            let res = run.await;
            let mut g = this.inner.lock().unwrap();
            for id in &ids {
                g.in_flight.remove(id);
            }
            match res {
                Ok(()) => {
                    let _ = g.store.mark_read(&ids, util::now_ms());
                    if let Some(l) = g.live.get_mut(&key) {
                        l.retry = None;
                    }
                }
                // Reason has no hooks to fall back on: its sync pushes again, backing off (15 s doubling to 30 min).
                Err(why) if bin == "reason" => {
                    if let Some(l) = g.live.get_mut(&key) {
                        if l.retry.is_none() {
                            log(&format!("{bin} delivery to {key} failed ({why}); retrying with backoff"));
                        }
                        let wait =
                            l.retry.map_or(Duration::from_secs(15), |(_, w)| (w * 2).min(Duration::from_secs(1800)));
                        l.retry = Some((Instant::now() + wait, wait));
                    }
                }
                Err(why) => {
                    log(&format!("{bin} delivery to {key} failed ({why}); falling back to hooks"));
                    // Hooks (or check_inbox) show it instead; tell senders the truth from now on.
                    if let Some(l) = g.live.get_mut(&key) {
                        l.push_failed = true;
                        l.reg.tier = Tier::Turn;
                    }
                }
            }
        });
    }

    /// A message from a teammate's machine. Returns the state and a note for the sender. A `retry` state
    /// means "not now": the sender keeps it in its outbox.
    pub fn receive(self: &Arc<Self>, peer: EndpointId, msg: Message) -> Result<(String, String)> {
        let (sender, machine) = self
            .team_state()
            .and_then(|t| t.member(&peer).map(|m| (m.person.clone(), m.machine.clone())))
            .ok_or_else(|| anyhow!("not a team member"))?;
        let mut msg = clean(msg)?;
        // Messages go between agent sessions (or, with notify, to a person's screen); an older Reason Network may still send
        // to or from a person.
        if !msg.from.contains('/') || (!msg.to.contains('/') && !msg.notify) {
            bail!(UPGRADE);
        }
        // A machine speaks only for its own agents.
        if !msg.from.starts_with(&format!("{sender}/")) {
            bail!("{} cannot send as {}", sender, msg.from);
        }
        if let Some(a) = &msg.approval
            && !self.team_state().is_some_and(|t| approval_holds(a, t.name(), &peer, &sender, &msg.to))
        {
            msg.approval = None;
            msg.body.push_str("\n\n(It came with an approval that didn't verify.)");
        }
        let policy = self.cfg.policy_for(&sender);
        if policy == Policy::Refuse {
            return Ok(("refused".into(), format!("{} doesn't accept messages from {sender}", self.person)));
        }
        if !self.allow(&peer) {
            return Ok((
                "retry".into(),
                format!("queued: {} is receiving a lot right now; retrying shortly", self.machine),
            ));
        }
        let Some((person, name)) = msg.to.split_once('/') else {
            // Only your own machines pop up for you without a session.
            if msg.to != self.person || sender != self.person {
                bail!("a pop-up without a session is only for your own user");
            }
            let (sent, note) = self.notify(&msg);
            return Ok((if sent { "notified" } else { "not notified" }.into(), note));
        };
        if person != self.person {
            bail!("{} is not on this machine", msg.to);
        }

        let res = {
            let mut g = self.inner.lock().unwrap();
            self.prune(&mut g);
            let key = g.live.iter().find(|(_, l)| l.name == name).map(|(k, _)| k.clone());
            let remembered = if key.is_none() { g.store.session_by_name(name)? } else { None };
            if key.is_none() && remembered.as_ref().is_none_or(|r| self.cfg.off_in(&r.cwd)) {
                return Ok(("refused".into(), format!("no session called {} on {}", msg.to, self.machine)));
            }
            if g.store.has(&msg.id)? {
                return Ok(("queued".into(), "already delivered".into()));
            }
            // Files take room here: past the quota, the sender's machine keeps the message and tries again later.
            let bytes = files::size(&msg.files);
            if bytes > 0 {
                let (kept, from_sender) = (g.store.files_kept()?, g.store.files_kept_from(&sender)?);
                if let Err(why) = g.quota.admit(&sender, bytes, kept, from_sender, Instant::now()) {
                    return Ok((
                        "retry".into(),
                        format!("queued: {} can't take these files now ({why}); retrying later", self.machine),
                    ));
                }
            }
            if !g.loop_ok(&msg.from, &msg.to) {
                return Ok(("refused".into(), LOOP_NOTE.into()));
            }
            // Only an older Reason Network still sends an agent's group message; a teammate's isn't taken.
            let sender = msg.from.split_once('/').map_or(msg.from.as_str(), |(p, _)| p);
            if msg.via.is_some() && !msg.from.ends_with("/terminal") && sender != self.person {
                let note = format!("refused: agents message one of {}'s sessions, not a group", self.person);
                return Ok(("refused".into(), note));
            }
            // Each machine has its own terminal: replies go back to the one this came from.
            if render::from_terminal(&msg.from) {
                msg.from = format!("{}@{machine}", msg.from);
            }
            // A held message's files are saved only once the person approves it.
            let held = policy == Policy::Hold;
            // Held for a session whose name is kept, it is that session's: it goes when the session does.
            let kept = held && g.store.session_by_name(name)?.is_some_and(|r| r.kept());
            // Counted once, when it is new here; a held one once approved.
            if g.store.insert(&mut msg, held)? && !held {
                let _ = g.store.count(util::now_ms(), Counted::Received);
            }
            let (state, note) = match key {
                _ if kept => (
                    "held".into(),
                    format!("held until {} approves it; dropped if that session is gone for a day first", self.person),
                ),
                _ if held => ("held".into(), format!("held until {} approves it", self.person)),
                Some(key) => self.deliver(&mut g, &key, vec![msg.clone()], msg.wake),
                None => ("queued".into(), offline_note(remembered.as_ref(), util::now_ms())),
            };
            if msg.notify { (state, format!("{note}; {}", self.notify(&msg).1)) } else { (state, note) }
        };
        self.arrived.notify_waiters();
        // The sender's session can be answered right away, before its machine's next presence names it.
        if let (Some(net), Some((_, from))) = (self.net(), msg.from.split_once('/')) {
            net.note_session(&peer, from.split_once('@').map_or(from, |(n, _)| n));
        }
        Ok(res)
    }

    fn allow(&self, peer: &EndpointId) -> bool {
        let mut rate = self.rate.lock().unwrap();
        let q = rate.entry(peer.to_string()).or_default();
        while q.front().is_some_and(|t| t.elapsed() > Duration::from_secs(60)) {
            q.pop_front();
        }
        if q.len() >= RATE_LIMIT {
            return false;
        }
        q.push_back(Instant::now());
        true
    }

    /// A session's unread messages. Waits up to `wait_secs` for one. Marks
    /// them read unless `peek`, in which case the client confirms with `Delivered`.
    pub(super) async fn inbox(&self, key: String, wait_secs: u64, peek: bool) -> Result<Inbox> {
        let (address, agent, stop) = {
            let g = self.inner.lock().unwrap();
            let l = g.live.get(&key).ok_or_else(unregistered)?;
            (self.addr(&l.name), Some(l.reg.agent.clone()), l.stop_gen)
        };
        let deadline = tokio::time::Instant::now() + Duration::from_secs(wait_secs);
        let _polling = (wait_secs > 0).then(|| Polling::start(self, &key));
        loop {
            // Register interest before checking, so a message landing in between still wakes us.
            let arrived = self.arrived.notified();
            tokio::pin!(arrived);
            arrived.as_mut().enable();
            let messages = {
                let mut g = self.inner.lock().unwrap();
                // The client gave up on this wait (or the session is gone): end it with nothing.
                if g.live.get(&key).is_none_or(|l| l.stop_gen != stop) {
                    return Ok(Inbox { messages: Vec::new(), text: String::new() });
                }
                let messages = g.pending(&address)?;
                if !messages.is_empty() && !peek {
                    let ids: Vec<String> = messages.iter().map(|m| m.id.clone()).collect();
                    g.store.mark_read(&ids, util::now_ms())?;
                }
                messages
            };
            if !messages.is_empty() || tokio::time::Instant::now() >= deadline {
                // A `reason network listen` session replies with the CLI.
                let text = render::for_agent(&messages, agent.as_deref(), agent.as_deref() == Some("cli"));
                return Ok(Inbox { messages, text });
            }
            let _ = tokio::time::timeout_at(deadline, arrived).await;
        }
    }

    /// Approves or drops held messages. Approved ones go to their session now.
    pub(super) fn settle(self: &Arc<Self>, ids: &[String], release: bool) -> Result<usize> {
        let mut g = self.inner.lock().unwrap();
        let (msgs, unsaved) = g.store.settle_held(ids, release)?;
        for (id, why) in &unsaved {
            log(&format!("held message {id} stays held: its files couldn't be saved ({why})"));
        }
        if release {
            for m in &msgs {
                let name = m.to.split_once('/').map_or("", |(_, n)| n);
                if let Some(key) = g.live.iter().find(|(_, l)| l.name == name).map(|(k, _)| k.clone()) {
                    self.deliver(&mut g, &key, vec![m.clone()], m.wake);
                }
            }
        }
        drop(g);
        self.arrived.notify_waiters();
        Ok(msgs.len())
    }
}

/// Marks a session as waiting in `check_inbox` for as long as it lives.
struct Polling<'a> {
    daemon: &'a Daemon,
    key: String,
}

impl<'a> Polling<'a> {
    fn start(daemon: &'a Daemon, key: &str) -> Self {
        *daemon.inner.lock().unwrap().polling.entry(key.to_string()).or_default() += 1;
        Polling { daemon, key: key.to_string() }
    }
}

impl Drop for Polling<'_> {
    fn drop(&mut self) {
        let mut g = self.daemon.inner.lock().unwrap();
        if let Some(n) = g.polling.get_mut(&self.key) {
            *n -= 1;
            if *n == 0 {
                g.polling.remove(&self.key);
            }
        }
    }
}

pub(super) fn unregistered() -> anyhow::Error {
    anyhow!(
        "this session isn't registered with Reason Network; restart the agent session, or use the reason-network tools"
    )
}

/// Keeps a field to plain characters, so nothing a peer sends can reshape what agents are shown.
fn tidy(s: &str, extra: &str, max: usize) -> Option<String> {
    let t: String = s.chars().filter(|c| c.is_ascii_alphanumeric() || extra.contains(*c)).take(max).collect();
    (!t.is_empty()).then_some(t)
}

/// Whether an approval that came from `peer` (a machine of `sender`'s) to the session `to` stands: signed by that machine
/// for this team, as its own person's answer, for that session, and of a sane size (in characters, as the sender cuts
/// it). Which request it answers is the approver's machine's word: the frame shows `about`, for the agent to check.
fn approval_holds(a: &Signed<Approval>, team: &str, peer: &EndpointId, sender: &str, to: &str) -> bool {
    let b = &a.body;
    let size: usize = [&b.about, &b.asker, &b.question, &b.request, &b.note].iter().map(|s| s.chars().count()).sum();
    size <= 2000 && a.by == *peer && b.approver == sender && b.asker == to && a.verify(team, "approval")
}

/// Validates a message from a peer and strips its metadata down to safe characters.
fn clean(m: Message) -> Result<Message> {
    let ok = |s: &str, extra: &str| {
        !s.is_empty() && s.len() <= 120 && s.chars().all(|c| c.is_ascii_alphanumeric() || extra.contains(c))
    };
    // Every engine names a message by a ULID, as written: one id, in one case.
    let canonical = Ulid::from_string(&m.id).is_ok_and(|u| u.to_string() == m.id);
    if !canonical || !ok(&m.from, "-/") || !ok(&m.to, "-/") {
        bail!("malformed message");
    }
    Ok(Message {
        from_agent: m.from_agent.as_deref().and_then(|s| tidy(s, "-", 30)),
        from_repo: m.from_repo.as_deref().and_then(|s| tidy(s, "-_./", 100)),
        via: m.via.as_deref().and_then(|s| tidy(s, "-_./:*@", 100)),
        reply_to: m.reply_to.as_deref().and_then(|s| tidy(s, "", 40)),
        files: files::check(m.files)?,
        ..m
    })
}

/// A message as it goes to a teammate's machine. One whose engine doesn't take files (its presence doesn't say so)
/// would drop them without a word: they are written out in the text instead, if that fits, and otherwise it isn't sent.
fn for_machine(mut msg: Message, takes_files: bool) -> Result<Message, String> {
    if msg.files.is_empty() || takes_files {
        return Ok(msg);
    }
    match files::inline(&msg.body, &msg.files, &msg.to, MAX_BODY) {
        Some(body) => {
            (msg.body, msg.files) = (body, Vec::new());
            Ok(msg)
        }
        None => Err(format!("{} needs to update Reason Network to receive files", msg.to)),
    }
}

/// Runs futures concurrently and collects their outputs in order.
async fn futures_join<F>(futs: impl Iterator<Item = F>) -> Vec<F::Output>
where
    F: std::future::Future + Send + 'static,
    F::Output: Send + 'static,
{
    let handles: Vec<_> = futs.map(tokio::spawn).collect();
    let mut out = Vec::with_capacity(handles.len());
    for h in handles {
        if let Ok(v) = h.await {
            out.push(v);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::team::TeamState;
    use iroh::SecretKey;
    use serde::Deserialize;

    #[tokio::test]
    async fn a_message_between_sessions_here_counts_as_sent_and_received_once() {
        let d = Arc::new(super::super::test_daemon(crate::config::Config::default()));
        let reg = |key: &str, cwd: &str| SessionReg {
            key: key.into(),
            agent: "codex".into(),
            cwd: cwd.into(),
            agent_session_id: None,
            pid: None,
            tier: Tier::Pull,
            name: None,
        };
        d.register(Some(1), reg("codex:1", "/work/api"), crate::git::GitInfo::default()).unwrap();
        let to = d.register(Some(2), reg("codex:2", "/work/web"), crate::git::GitInfo::default()).unwrap().address;
        let res = d.send("codex:1".into(), &to, "hi", None, true, false, None, Vec::new()).await.unwrap();
        assert_eq!(res.recipients[0].state, "queued");
        let days = d.inner.lock().unwrap().store.counts(util::now_ms()).unwrap();
        assert_eq!(days.iter().map(|d| (d.sent, d.received)).collect::<Vec<_>>(), [(1, 1)]);
    }

    const HOUR: i64 = 3_600_000;

    fn codex(key: &str) -> SessionReg {
        SessionReg {
            key: key.into(),
            agent: "codex".into(),
            cwd: "/work/api".into(),
            agent_session_id: None,
            pid: None,
            tier: Tier::Pull,
            name: None,
        }
    }

    /// Takes a session here offline, last seen `ago` before now. Returns its name.
    fn gone(d: &Daemon, key: &str, ago: i64) -> String {
        let mut g = d.inner.lock().unwrap();
        let name = g.live.remove(key).unwrap().name;
        g.store.touch_session(&name, util::now_ms() - ago).unwrap();
        name
    }

    #[test]
    fn queued_mail_says_when_it_may_be_dropped_and_who_gets_it() {
        let seen = |key: &str, agent: &str| Remembered {
            name: "api-codex".into(),
            key: key.into(),
            agent: agent.into(),
            cwd: "/work/api".into(),
            repo: None,
            branch: None,
            doing: None,
            tier: Tier::Pull,
            last_seen: 0,
            attached: true,
        };
        // A conversation its agent gave an id waits for it, whoever takes the name meanwhile.
        let back = "it reaches that session when it's back";
        assert_eq!(
            offline_note(Some(&seen("codex:1", "codex")), 3 * HOUR),
            format!("queued: last seen 3h ago; unread mail is dropped after about 21h if it stays offline; {back}")
        );
        assert_eq!(
            offline_note(Some(&seen("codex:1", "codex")), 24 * HOUR),
            format!("queued: last seen 1d ago; unread mail may be dropped any time; {back}")
        );
        // The person's own mailboxes are never forgotten.
        assert_eq!(offline_note(Some(&seen("cli:t", "cli")), 30 * HOUR), format!("queued: last seen 1d ago; {back}"));
        // A session known only by its process can't come back: a new one in its place gets the mail.
        let name = "a new session that takes this name gets the mail";
        assert_eq!(
            offline_note(Some(&seen("proc:7", "zed")), 3 * HOUR),
            format!("queued: last seen 3h ago; unread mail is dropped after about 21h if it stays offline; {name}")
        );
        assert_eq!(offline_note(None, 0), format!("queued: the session is offline; {name}"));
    }

    #[tokio::test]
    async fn sending_to_an_offline_session_here_says_when_its_mail_may_be_dropped() {
        let d = Arc::new(super::super::test_daemon(crate::config::Config::default()));
        d.register(Some(1), codex("codex:1"), crate::git::GitInfo::default()).unwrap();
        let to = d.register(Some(2), codex("codex:2"), crate::git::GitInfo::default()).unwrap().address;
        gone(&d, "codex:2", 3 * HOUR);
        let res = d.send("codex:1".into(), &to, "hi", None, true, false, None, Vec::new()).await.unwrap();
        assert_eq!(res.recipients[0].state, "queued");
        let note = &res.recipients[0].note;
        assert!(note.starts_with("queued: last seen 3h ago; unread mail is dropped after about 2"), "{note}");
    }

    #[test]
    fn a_teammate_sending_to_an_offline_session_is_told_when_its_mail_may_be_dropped() {
        let cfg = crate::config::Config { person: Some("alex".into()), ..Default::default() };
        let d = Arc::new(super::super::test_daemon(cfg));
        let bob = SecretKey::generate();
        let mut team = TeamState::found("acme", &d.person, &d.machine, &d.key);
        team.put_member(&d.key, bob.public(), "bob", "xps", true);
        d.inner.lock().unwrap().team = Some(team);
        d.register(Some(1), codex("codex:1"), crate::git::GitInfo::default()).unwrap();
        let name = gone(&d, "codex:1", 3 * HOUR);
        let msg = Message {
            id: Ulid::generate().to_string(),
            from: "bob/web".into(),
            from_agent: None,
            from_repo: None,
            to: format!("alex/{name}"),
            via: None,
            reply_to: None,
            body: "hi".into(),
            wake: true,
            notify: false,
            sent_at: 0,
            approval: None,
            files: Vec::new(),
        };
        let (state, note) = d.receive(bob.public(), msg).unwrap();
        assert_eq!(state, "queued");
        assert!(note.starts_with("queued: last seen 3h ago; unread mail is dropped after about 2"), "{note}");
        assert!(note.ends_with("; it reaches that session when it's back"), "{note}");
    }

    #[test]
    fn a_message_from_a_teammates_terminal_is_answered_on_that_machine() {
        let cfg = crate::config::Config { person: Some("alex".into()), ..Default::default() };
        let d = Arc::new(super::super::test_daemon(cfg));
        let bob = SecretKey::generate();
        let mut team = TeamState::found("acme", &d.person, &d.machine, &d.key);
        team.put_member(&d.key, bob.public(), "bob", "xps", true);
        d.inner.lock().unwrap().team = Some(team);
        let to = d.register(Some(1), codex("codex:1"), crate::git::GitInfo::default()).unwrap().address;
        let msg = Message {
            id: Ulid::generate().to_string(),
            from: "bob/terminal".into(),
            from_agent: Some("cli".into()),
            from_repo: None,
            to: to.clone(),
            via: None,
            reply_to: None,
            body: "hi".into(),
            wake: true,
            notify: false,
            sent_at: 0,
            approval: None,
            files: Vec::new(),
        };
        d.receive(bob.public(), msg).unwrap();
        let got = d.inner.lock().unwrap().store.unread(&to).unwrap();
        assert_eq!(got[0].from, "bob/terminal@xps");
        let text = render::for_agent(&got, Some("codex"), false);
        assert!(
            text.contains("From bob, typed in a terminal.") && text.contains("send_message to \"bob/terminal@xps\""),
            "{text}"
        );
    }

    #[test]
    fn a_peers_message_id_is_a_ulid_as_written() {
        let msg = |id: &str| Message {
            id: id.into(),
            from: "bob/api".into(),
            from_agent: None,
            from_repo: None,
            to: "alex/web".into(),
            via: None,
            reply_to: None,
            body: "hi".into(),
            wake: true,
            notify: false,
            sent_at: 0,
            approval: None,
            files: Vec::new(),
        };
        let id = Ulid::generate().to_string();
        assert!(clean(msg(&id)).is_ok());
        for bad in [id.to_lowercase(), "CON".into(), "01ALICEMSG".into(), format!("{id}0"), String::new()] {
            assert!(clean(msg(&bad)).is_err(), "{bad}");
        }
    }

    fn approval(asker: &str, approver: &str) -> Approval {
        Approval {
            about: "01REQ".into(),
            asker: asker.into(),
            approver: approver.into(),
            approved: true,
            question: "Rename it?".into(),
            request: "please rename billing".into(),
            note: String::new(),
            at: 1,
        }
    }

    #[test]
    fn an_approval_stands_only_from_its_approvers_machine_to_the_session_that_asked() {
        let (bob, mallory) = (SecretKey::generate(), SecretKey::generate());
        let real = Signed::new(approval("alice/api", "bob"), &bob, "acme", "approval");
        assert!(approval_holds(&real, "acme", &bob.public(), "bob", "alice/api"));
        // Passed on by another machine; claimed for bob by another person's machine; to another session; another team.
        assert!(!approval_holds(&real, "acme", &mallory.public(), "mallory", "alice/api"));
        let claimed = Signed::new(approval("alice/api", "bob"), &mallory, "acme", "approval");
        assert!(!approval_holds(&claimed, "acme", &mallory.public(), "mallory", "alice/api"));
        assert!(!approval_holds(&real, "acme", &bob.public(), "bob", "alice/other"));
        assert!(!approval_holds(&real, "other", &bob.public(), "bob", "alice/api"));
        // Changed after it was signed, or too big.
        let mut changed = real.clone();
        changed.body.approved = false;
        assert!(!approval_holds(&changed, "acme", &bob.public(), "bob", "alice/api"));
        // A full-size answer in any script stands; one bigger than the sender would make doesn't.
        let mut cjk = approval("alice/api", "bob");
        (cjk.request, cjk.question, cjk.note) = ("请".repeat(501), "吗".repeat(401), "好".repeat(500));
        assert!(approval_holds(&Signed::new(cjk, &bob, "acme", "approval"), "acme", &bob.public(), "bob", "alice/api"));
        let mut big = approval("alice/api", "bob");
        big.note = "x".repeat(3000);
        let big = Signed::new(big, &bob, "acme", "approval");
        assert!(!approval_holds(&big, "acme", &bob.public(), "bob", "alice/api"));
    }

    #[test]
    fn an_older_engine_gets_the_files_in_the_text_or_nothing() {
        let msg = |content: &str| Message {
            id: "01M".into(),
            from: "alex/web".into(),
            from_agent: None,
            from_repo: None,
            to: "bob/api".into(),
            via: None,
            reply_to: None,
            body: "the config".into(),
            wake: true,
            notify: false,
            sent_at: 0,
            approval: None,
            files: vec![Attachment { name: "app.toml".into(), content: content.into(), path: None }],
        };
        // An engine that takes files gets them as they are.
        assert_eq!(for_machine(msg("port = 1"), true).unwrap().files.len(), 1);
        // One that doesn't: written out in the text, as nothing it doesn't know would be dropped.
        let sent = for_machine(msg("port = 1"), false).unwrap();
        assert!(sent.files.is_empty());
        assert!(sent.body.starts_with("the config\n\n(bob/api runs an older Reason Network"), "{}", sent.body);
        assert!(sent.body.ends_with("app.toml:\n```\nport = 1\n```"), "{}", sent.body);
        // Too much to write out: not sent, and the sender is told why.
        let e = for_machine(msg(&"x".repeat(MAX_BODY)), false).unwrap_err();
        assert_eq!(e, "bob/api needs to update Reason Network to receive files");
        // Nothing to carry: as it is.
        let mut plain = msg("");
        plain.files.clear();
        assert_eq!(for_machine(plain, false).unwrap().body, "the config");
        // What an older engine reads of a message with files: the message, without them (hence the above).
        #[derive(Deserialize)]
        struct Older {
            body: String,
        }
        let wire = serde_json::to_string(&msg("port = 1")).unwrap();
        assert_eq!(serde_json::from_str::<Older>(&wire).unwrap().body, "the config");
    }
}
