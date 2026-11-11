//! Sending, receiving and delivering messages.
//!
//! Delivery is at least once: a message stays unread until whoever showed it to the agent confirms
//! with `Delivered`. Messages handed to a push channel or a hook are "in flight" meanwhile, so the inbox and hooks
//! don't show them a second time.

use super::{Daemon, Inner, directory::Target, log};
use crate::{
    agent, claude, codex,
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

/// The most text a message carries, in bytes. A teammate's machine is held to it too (see [`clean`]).
const MAX_BODY: usize = 64 * 1024;
/// Messages per minute accepted from one teammate's machine; more are deferred, not dropped.
const RATE_LIMIT: usize = 60;
/// Agent-to-agent messages allowed between one pair in `LOOP_WINDOW`; past that it is probably a loop.
const LOOP_LIMIT: usize = 12;
const LOOP_WINDOW: Duration = Duration::from_secs(600);
/// A push or hook not confirmed within this long is shown through the inbox and hooks again.
const IN_FLIGHT_TTL: Duration = Duration::from_secs(90);
/// How often a Codex session's rollout is checked while mail waits for a hook of its turn.
const CODEX_TURN_POLL: Duration = Duration::from_secs(2);
/// An offline Codex thread seen within this long gets its wake mail through `codex queue`, so the first message starts
/// a turn when the thread is resumed (Codex runs no hook before then). One gone longer waits for its next turn.
const CODEX_RESUME_MS: i64 = 24 * 3_600_000;

/// How a session on this machine gets a message into its conversation.
enum Channel {
    /// Its `reason network mcp` posts into Claude Code's session socket.
    ClaudeSocket(u64),
    /// The agent's own CLI: `codex queue --thread <id> --message`, `reason session send <id> --message`. With the
    /// `CODEX_HOME` the session's hooks run with, if not the default.
    Command(&'static str, Vec<String>, Option<String>),
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

    /// The Codex thread an offline session's wake mail is queued into, with the `CODEX_HOME` its hooks named: only a
    /// Codex thread seen within [`CODEX_RESUME_MS`], checked when the mail is sent.
    fn resumable_codex(&self, r: &Remembered, wake: bool, now: i64) -> Option<(String, Option<String>)> {
        let thread = r.key.strip_prefix("codex:").filter(|t| !t.is_empty())?;
        if !wake || r.agent != "codex" || now - r.last_seen > CODEX_RESUME_MS {
            return None;
        }
        Some((thread.to_string(), self.store.codex_home(&r.key).ok().flatten()))
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

/// The command that hands `text` to `program` (`args` end with its message flag). With `codex_home`, it talks to that
/// home's Codex app-server, not the one of the daemon's own `CODEX_HOME`.
fn handoff_command(
    program: std::process::Command,
    args: &[String],
    text: &str,
    codex_home: Option<&str>,
) -> tokio::process::Command {
    let mut command = crate::util::no_window(tokio::process::Command::from(program));
    command.args(args).arg(text).stdin(std::process::Stdio::null()).kill_on_drop(true);
    if let Some(home) = codex_home {
        command.env("CODEX_HOME", home);
    }
    command
}

/// A message to send to a teammate's machine now: where its result goes, the machine, its name, whether it is
/// connected, and the message.
type RemoteJob = (usize, EndpointId, String, bool, Message);

const UPGRADE: &str =
    "update Reason Network (with the Reason CLI, `reason update`): messages go between agent sessions now";

const LOOP_NOTE: &str = "refused: these two agents have exchanged a lot of messages in the last few minutes, which \
                         looks like a loop. Check with your user before continuing.";

/// `codex queue`'s arguments for `thread`, up to its message flag. With `resume`, `--resume`: once the message is
/// queued, Codex loads that same thread again if it isn't loaded (`thread/resume`, a no-op for a loaded one), so mail
/// reaches a thread Codex unloaded while idle (its app-server unsubscribes and shuts an inactive thread down, firing
/// SessionEnd) without a person opening it. Never for a thread the person archived.
fn codex_queue(thread: &str, resume: bool) -> Vec<String> {
    let mut args: Vec<String> = vec!["queue".into(), "--thread".into(), thread.into()];
    if resume {
        args.push(RESUME.into());
    }
    args.push("--message".into());
    args
}

const RESUME: &str = "--resume";

/// What `codex queue --resume` prints, exiting 0, when it queued the message but could not load the thread again (no
/// shared app-server it can reach, say): the mail is Codex's then, and starts a turn only when the thread is opened.
const NOT_RESUMED: &str = "could not resume thread";

/// Whether the person archived the thread `args` queue into, looked up off the daemon's lock (the folder may be large
/// or on a slow disk). One that can't be looked up in time counts as archived: never bring a thread back unsure.
async fn archived_target(args: &[String], codex_home: Option<&str>) -> bool {
    let Some(thread) = args.iter().position(|a| a == "--thread").and_then(|i| args.get(i + 1)).cloned() else {
        return false;
    };
    let home = codex_home.map(std::path::PathBuf::from);
    let look = tokio::task::spawn_blocking(move || codex::archived(&thread, home.as_deref()));
    !matches!(tokio::time::timeout(Duration::from_secs(5), look).await, Ok(Ok(false)))
}

/// A `codex queue` older than `--resume` refuses the flag (clap: "unexpected argument '--resume' found") before it
/// queues anything: then it is run again without it, as before.
fn refused_resume(args: &[String], stderr: &str) -> bool {
    args.iter().any(|a| a == RESUME) && stderr.contains(RESUME) && stderr.contains("unexpected argument")
}

/// The note for mail queued for a session that is offline. Its unread mail is deleted at the first cleanup a day after
/// it was last seen, unless it is one of the person's own mailboxes (`terminal`, `listen --as`), which are kept. Until then it waits for
/// that session or one that explicitly reclaims its name. For a session without a durable id it waits for the name.
fn offline_note(r: Option<&Remembered>, now: i64) -> String {
    const NAME: &str = "a new session that takes this name gets the mail";
    let Some(r) = r else { return format!("queued: the session is offline; {NAME}") };
    let who = if r.mailbox {
        "it reaches that session when it's back"
    } else if r.kept() {
        "it reaches that session when it's back, unless another session here explicitly takes its name"
    } else {
        NAME
    };
    let seen = util::ago(r.last_seen, now);
    let forgotten = r.last_seen.saturating_add(super::sessions::FORGET_MS);
    if r.mailbox {
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
        self.send_idempotent(from, to, body, reply_to, wake, notify, approval, files, None).await
    }

    #[allow(clippy::too_many_arguments)]
    pub(super) async fn send_idempotent(
        self: &Arc<Self>,
        from: String,
        to: &str,
        body: &str,
        reply_to: Option<String>,
        wake: bool,
        notify: bool,
        approval: Option<Box<Signed<Approval>>>,
        files: Vec<Attachment>,
        idempotency_key: Option<String>,
    ) -> Result<SendResult> {
        // A key may name an earlier send, even if this attempt fails before looking up its receipt.
        // Only a successful begin_send that inserted a fresh key can prove otherwise: neither a
        // missing receipt for an existing key nor a failed lookup proves that it never reached a recipient.
        let mut may_have_dispatched = idempotency_key.is_some();
        self.send_inner(
            from,
            to,
            body,
            reply_to,
            wake,
            notify,
            approval,
            files,
            idempotency_key,
            &mut may_have_dispatched,
        )
        .await
        .map_err(|cause| SendError { cause, not_dispatched: !may_have_dispatched }.into())
    }

    #[allow(clippy::too_many_arguments)]
    async fn send_inner(
        self: &Arc<Self>,
        from: String,
        to: &str,
        body: &str,
        reply_to: Option<String>,
        wake: bool,
        notify: bool,
        approval: Option<Box<Signed<Approval>>>,
        files: Vec<Attachment>,
        idempotency_key: Option<String>,
        may_have_dispatched: &mut bool,
    ) -> Result<SendResult> {
        if idempotency_key.as_ref().is_some_and(|key| key.is_empty() || key.len() > 200) {
            bail!("idempotency key must be 1 to 200 bytes");
        }
        if idempotency_key.is_some() && (is_group(to.trim()) || notify && to.trim() == self.person) {
            bail!("idempotency keys are for one agent session");
        }
        let (body, to) = (body.trim(), to.trim());
        if body.is_empty() {
            bail!("empty message");
        }
        if notify && is_group(to) {
            bail!("notify is for one session or one person, not a group");
        }
        fits(body)?;
        let files = files::check(files)?;
        let now = util::now_ms();
        let net = self.net();
        let remotes = net.as_ref().map(|n| n.remotes()).unwrap_or_default();

        let mut res = SendResult { ids: Vec::new(), recipients: Vec::new() };
        let mut remote_jobs = Vec::new();
        let mut send_key = None;
        let mut send_nonce = String::new();
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
            let sender_origin = match sender.through_reason {
                Some(true) => "hosted",
                Some(false) => "local",
                None => "unknown",
            };
            let exclude = sender.reg.key.clone();
            if let Some(key) = &idempotency_key {
                let key = blake3::hash(serde_json::to_string(&(&self.person, &exclude, key))?.as_bytes())
                    .to_hex()
                    .to_string();
                let fingerprint =
                    blake3::hash(serde_json::to_string(&(to, body, &reply_to, wake, notify, &files))?.as_bytes())
                        .to_hex()
                        .to_string();
                let (nonce, receipt, fresh) = g.store.begin_send(&key, &fingerprint, now)?;
                if let Some(receipt) = receipt {
                    return Ok(receipt);
                }
                *may_have_dispatched = !fresh;
                send_nonce = nonce;
                send_key = Some(key);
            }
            // It talks to teammates: its name stays its own.
            self.reached(&mut g, &exclude);
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
                *may_have_dispatched = true;
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
                // A crash between queuing and saving the receipt retries the same peer id, never a new message.
                let id = if let Some(key) = &send_key {
                    let generation = if send_nonce.is_empty() { key.clone() } else { format!("{key}:{send_nonce}") };
                    let hash = blake3::hash(format!("{generation}:{}", t.shown_address()).as_bytes());
                    Ulid::from(u128::from_be_bytes(hash.as_bytes()[..16].try_into().expect("16 bytes"))).to_string()
                } else {
                    Ulid::generate().to_string()
                };
                if let Some(old) = g.store.message(&id)? {
                    *may_have_dispatched = true;
                    res.ids.push(id);
                    res.recipients.push(Recipient {
                        address: t.shown_address(),
                        state: "accepted".into(),
                        note: format!(
                            "already accepted for {}; the original message is retained without resending",
                            old.to
                        ),
                    });
                    continue;
                }
                let mut msg = Message {
                    id,
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
                // The receipt names it as its answer will show here: a teammate's terminal with its machine.
                let shown = t.shown_address();
                let here = !matches!(t, Target::Remote { .. });
                let target_agent = match &t {
                    Target::Session { key: Some(key), .. } => g.live.get(key).map(|l| l.reg.agent.as_str()),
                    Target::Session { key: None, .. } => None,
                    Target::Remote { address, machine, .. } => remotes
                        .get(machine)
                        .and_then(|r| r.presence.sessions.iter().find(|s| &s.address == address))
                        .map(|s| s.agent.as_str()),
                };
                let mut dims = crate::usage::Dimensions {
                    source: crate::usage::family(from_agent.as_deref().unwrap_or("unknown")),
                    target: target_agent.map(crate::usage::family).unwrap_or_else(|| "unknown".into()),
                    locality: if here { "local" } else { "remote" }.into(),
                    origin: sender_origin.into(),
                    relationship: if here && g.team.as_ref().is_some_and(|t| t.has_proof(&self.key.public())) {
                        "same"
                    } else {
                        "unknown"
                    }
                    .into(),
                    transport: if here { "local" } else { "unknown" }.into(),
                    ..Default::default()
                };
                if let Target::Remote { machine, .. } = &t {
                    if let Some(link) = net.as_ref().and_then(|n| n.link_info(machine)) {
                        dims.transport = if link.relay { "relay" } else { "direct" }.into();
                        let mut latency = dims.clone();
                        latency.bucket = crate::usage::duration(link.rtt_ms).into();
                        self.usage.count("rtt_ms", latency, 1);
                    }
                    if let Some(team) = &g.team
                        && let (Some(ours), Some(theirs)) = (team.proof(&self.key.public()), team.proof(machine))
                    {
                        dims.relationship = if ours.sub == theirs.sub { "same" } else { "different" }.into();
                    }
                }
                self.usage.start(&msg.id, dims.clone(), msg.reply_to.as_deref(), false);
                self.usage.count("attempted", dims.clone(), 1);
                let mut sized = dims.clone();
                sized.bucket = crate::usage::size(body.len()).into();
                self.usage.count("body_bytes", sized, 1);
                let mut attached = dims.clone();
                attached.bucket = crate::usage::number(files.len()).into();
                self.usage.count("attachments", attached, 1);
                let mut attachment_size = dims.clone();
                attachment_size.bucket = crate::usage::size(files.iter().map(|f| f.content.len()).sum()).into();
                self.usage.count("attachment_bytes", attachment_size, 1);
                // Set before the first durable write: any later failure must not authorize a reroute.
                *may_have_dispatched = true;
                g.store.record_sent(&exclude, &msg, &shown, "queued")?;
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
                        let r = g.store.session_by_name(name)?;
                        // To the default name an offline session went by before its title named it: at its name now.
                        if let Some(r) = r.as_ref().filter(|r| r.name != name) {
                            msg.to = self.addr(&r.name);
                        }
                        g.store.insert(&mut msg, false)?;
                        self.deliver_offline(&mut g, r.as_ref(), vec![msg.clone()], wake)
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
                if state == "refused" {
                    self.usage.stage(&msg.id, "rejected");
                } else if here {
                    self.usage.stage(&msg.id, "accepted");
                    self.usage.count("received", dims.clone(), 1);
                }
                if state == "queued" {
                    self.usage.count("queued", dims, 1);
                }
                // Counting never stops a message. One to another machine counts once that machine takes it.
                if here && state != "refused" {
                    let _ = g.store.count(now, Counted::Sent);
                    let _ = g.store.count(now, Counted::Received);
                }
                if notify && here && state != "refused" {
                    note = format!("{note}; {}", self.notify(&msg).1);
                }
                res.recipients.push(Recipient { address: shown, state, note });
                let _ = g.store.send_outcome(&msg.id, &res.recipients.last().expect("just added").state);
            }
            // The durable outbox now owns remote retries. A lost API response returns this same receipt.
            if let Some(key) = &send_key {
                g.store.finish_send(key, &res)?;
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
        if let Some(key) = &send_key {
            self.inner.lock().unwrap().store.finish_send(key, &res)?;
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
        if !g.live.contains_key(key) {
            let r = g.store.session_by_key(key).ok().flatten();
            return self.deliver_offline(g, r.as_ref(), msgs, wake);
        }
        // Mail reached it: it is a session teammates talk to, whose name stays its own.
        self.reached(g, key);
        let l = &g.live[key];
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
        let has = |bin: &str| !l.push_failed && self.agent_bin(bin).is_some();
        // A Codex turn is running (and `codex queue` is there for when it ends).
        let codex_turn = l.reg.agent == "codex"
            && has("codex")
            && l.transcript.as_deref().and_then(codex::turn_running) == Some(true);
        let channel = match (l.reg.tier, l.conn, &l.reg.agent_session_id) {
            // These adapters take Deliver events on this connection. Codex and Reason still
            // use their native channels below, even when their MCP client registers as push.
            (Tier::Push, Some(conn), _)
                if l.mcp_push && matches!(l.reg.agent.as_str(), agent::CLAUDE_CODE | "hermes" | "antigravity") =>
            {
                Channel::ClaudeSocket(conn)
            }
            // In a turn, its next PostToolUse (or Stop) shows it; `codex queue` would hold it until the turn ends.
            (Tier::Push, _, Some(_)) if codex_turn => Channel::Hook,
            (Tier::Push, _, Some(id)) if l.reg.agent == "codex" && has("codex") => {
                Channel::Command("codex", codex_queue(id, true), l.codex_home.clone())
            }
            (Tier::Push, _, Some(id)) if l.reg.agent == "reason" && has("reason") => {
                Channel::Command("reason", vec!["session".into(), "send".into(), id.clone(), "--message".into()], None)
            }
            (Tier::Push, _, id) if l.reg.agent == agent::CLAUDE_CODE && !l.push_failed => {
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
                ("pushed".into(), "accepted by the delivery adapter; waiting for its handoff acknowledgment".into())
            }
            Channel::Command(bin, args, codex_home) => {
                for id in &ids {
                    g.in_flight.insert(id.clone(), Instant::now());
                }
                self.send_with(bin, args, codex_home, key.to_string(), ids, text);
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
            Channel::Hook => {
                if codex_turn {
                    self.watch_codex_turn(g, key);
                }
                ("queued".into(), hook_note.into())
            }
            Channel::Pull => ("queued".into(), "queued: this agent only sees it when it checks its inbox".into()),
        }
    }

    /// Mail for a session here that is offline waits for it, except wake mail for a Codex thread seen within the last
    /// day: that goes to `codex queue --resume`, which queues it and loads that same thread again (by its id: never
    /// another chat), and the thread starts a turn with it. Offline is not over: Codex unloads an idle thread (its
    /// SessionEnd fires and its MCP server exits) though the conversation is there to come back to. A thread the
    /// person archived is not brought back: its mail is queued without `--resume`, for when they open it. In
    /// flight meanwhile, so the hooks of that turn don't show it again; if `codex queue` fails, they show it instead,
    /// and that thread's mail waits for them until its next SessionStart or a `codex queue` that works. Never for a
    /// thread in a folder that is off.
    fn deliver_offline(
        self: &Arc<Self>,
        g: &mut Inner,
        r: Option<&Remembered>,
        msgs: Vec<Message>,
        wake: bool,
    ) -> (String, String) {
        let now = util::now_ms();
        let resumable = r
            .filter(|r| !g.codex_failed.contains(&r.key))
            .filter(|r| !self.cfg.off_in(&r.cwd) && !g.off.contains_key(&r.key))
            .and_then(|r| Some((r, g.resumable_codex(r, wake, now)?)));
        let Some((r, (thread, codex_home))) = resumable.filter(|_| self.agent_bin("codex").is_some()) else {
            return ("queued".into(), offline_note(r, now));
        };
        let ids: Vec<String> = msgs.iter().map(|m| m.id.clone()).collect();
        for id in &ids {
            g.in_flight.insert(id.clone(), Instant::now());
        }
        let text = render::for_agent(&msgs, Some(&r.agent), false);
        let unresumed = g.codex_unresumed.contains(&r.key);
        self.send_with("codex", codex_queue(&thread, true), codex_home, r.key.clone(), ids, text);
        let seen = util::ago(r.last_seen, now);
        let then = if unresumed {
            "Codex could not load it again last time, so it may start a turn only when the person opens the thread"
        } else {
            "Codex is asked to load that same thread again, which starts a turn with it (unless the person archived it: \
             then when they open it)"
        };
        ("pushed".into(), format!("queued into the Codex thread, which is offline (last seen {seen} ago): {then}"))
    }

    /// Codex runs no Stop hook when a turn is interrupted or fails: until the turn `key` is in ends, checks its
    /// rollout, and once it has ended hands the wake mail no hook took to `codex queue`. One watch per session.
    /// The watch is not kept across a daemon restart: if the daemon restarts while mail waits for a running turn, and
    /// that turn then ends without Stop before any of its hooks runs, the mail waits until the next prompt.
    fn watch_codex_turn(self: &Arc<Self>, g: &mut Inner, key: &str) {
        if !g.codex_watch.insert(key.to_string()) {
            return;
        }
        let (this, key) = (self.clone(), key.to_string());
        tokio::spawn(async move {
            loop {
                tokio::time::sleep(CODEX_TURN_POLL).await;
                let mut g = this.inner.lock().unwrap();
                let Some(l) = g.live.get(&key) else {
                    g.codex_watch.remove(&key);
                    return;
                };
                let (address, transcript) = (this.addr(&l.name), l.transcript.clone());
                if !g.store.unread_wakes(&address).unwrap_or_default() {
                    g.codex_watch.remove(&key);
                    return;
                }
                if transcript.as_deref().and_then(codex::turn_running) == Some(true) {
                    continue;
                }
                // What a hook showed and hasn't confirmed yet is left to it until its confirmation is overdue.
                let waiting: Vec<Message> =
                    g.pending(&address).unwrap_or_default().into_iter().filter(|m| m.wake).collect();
                if waiting.is_empty() {
                    continue;
                }
                g.codex_watch.remove(&key);
                this.deliver(&mut g, &key, waiting, true);
                return;
            }
        });
    }

    /// A session's MCP server had no inbox to push `ids` into: they are unread again, for the session to read when it
    /// checks, and nothing is pushed through it until it registers knowing one (or Claude Code lists one to post into).
    pub(super) fn push_failed(&self, key: &str, ids: &[String]) {
        for id in ids {
            self.usage.stage(id, "push_failed");
        }
        let mut g = self.inner.lock().unwrap();
        let _ = g.store.queue_again(ids);
        for id in ids {
            g.in_flight.remove(id);
        }
        if let Some(l) = g.live.get_mut(key) {
            l.mcp_push = false;
            l.reg.tier = l.reg.tier.max(Tier::Pull);
            if l.reg.agent == "antigravity" {
                let wait = l.retry.map_or(Duration::from_secs(10), |(_, wait)| (wait * 2).min(Duration::from_secs(60)));
                l.retry = Some((Instant::now() + wait, wait));
                if l.last_hook.is_some() {
                    l.reg.tier = Tier::Turn;
                }
            }
        }
        log(&format!("pushing to {}: its MCP server knows no inbox; left unread", self.logged(&g, key)));
        drop(g);
        self.arrived.notify_waiters();
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
            if !matches!(res, Ok(Ok(()))) {
                this.arrived.notify_waiters();
            }
            match res {
                Ok(Ok(())) => {
                    let changed = g.store.mark_read(&ids, util::now_ms()).unwrap_or_default();
                    let _ = g.store.handoff(&changed, util::now_ms());
                    for id in &changed {
                        this.usage.stage(id, "delivered");
                    }
                }
                Ok(Err(e)) => log(&format!("delivering to Claude session {}: {e}", this.logged(&g, &key))),
                Err(e) => log(&format!("delivering to Claude session {}: {e}", this.logged(&g, &key))),
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

    /// Hands `text` to the agent's own CLI (`args` end with its message flag), against `codex_home` if set.
    fn send_with(
        self: &Arc<Self>,
        bin: &'static str,
        args: Vec<String>,
        codex_home: Option<String>,
        key: String,
        ids: Vec<String>,
        text: String,
    ) {
        let this = self.clone();
        tokio::spawn(async move {
            let run = |args: Vec<String>| {
                let (this, text, codex_home) = (&this, &text, &codex_home);
                async move {
                    let program =
                        this.agent_bin(bin).map(|b| agent::command_at(&b)).ok_or_else(|| format!("{bin} not found"))?;
                    let out = handoff_command(program, &args, text, codex_home.as_deref()).output();
                    match tokio::time::timeout(Duration::from_secs(30), out).await {
                        Ok(Ok(o)) if o.status.success() => Ok(String::from_utf8_lossy(&o.stderr).trim().to_string()),
                        Ok(Ok(o)) => Err(String::from_utf8_lossy(&o.stderr).trim().to_string()),
                        Ok(Err(e)) => Err(e.to_string()),
                        Err(_) => Err("timed out".into()),
                    }
                }
            };
            let mut args = args;
            // A thread the person archived is only queued into, for when they open it: never brought back.
            if args.iter().any(|a| a == RESUME) && archived_target(&args, codex_home.as_deref()).await {
                args.retain(|a| a != RESUME);
            }
            let mut res = run(args.clone()).await;
            if let Err(why) = &res
                && refused_resume(&args, why)
            {
                res = run(args.iter().filter(|a| *a != RESUME).cloned().collect()).await;
            }
            // Queued, but not loaded again: the mail is Codex's, not to be queued twice; it says so from now on.
            let unresumed = match &res {
                Ok(warn) if warn.contains(NOT_RESUMED) => Some(warn.clone()),
                _ => None,
            };
            this.handed(bin, &key, &ids, res.map(drop), unresumed);
        });
    }

    /// What came of handing `ids` to `bin` for `key`: read once it took them, else unread again for its hooks to show.
    fn handed(
        &self,
        bin: &str,
        key: &str,
        ids: &[String],
        res: std::result::Result<(), String>,
        unresumed: Option<String>,
    ) {
        let mut g = self.inner.lock().unwrap();
        for id in ids {
            g.in_flight.remove(id);
        }
        if let Some(why) = unresumed {
            if g.codex_unresumed.insert(key.to_string()) {
                log(&format!(
                    "codex queued mail for {} but could not load the thread again ({why}); it starts a turn when the \
                     person opens it",
                    self.logged(&g, key)
                ));
            }
        } else if res.is_ok() {
            g.codex_unresumed.remove(key);
        }
        if bin == "codex" && res.is_ok() {
            g.codex_failed.remove(key);
        } else if bin == "codex" {
            g.codex_failed.insert(key.to_string());
        }
        if res.is_err() {
            self.arrived.notify_waiters();
        }
        match res {
            Ok(()) => {
                let changed = g.store.mark_read(ids, util::now_ms()).unwrap_or_default();
                // A local native queue is an agent input handoff; the cloud command only acknowledges its server.
                if bin != "reason" {
                    let _ = g.store.handoff(&changed, util::now_ms());
                    for id in &changed {
                        self.usage.stage(id, "delivered");
                    }
                }
                if let Some(l) = g.live.get_mut(key) {
                    l.retry = None;
                }
            }
            // Reason has no hooks to fall back on: its sync pushes again, backing off (15 s doubling to 30 min).
            Err(why) if bin == "reason" => {
                let who = self.logged(&g, key);
                if let Some(l) = g.live.get_mut(key) {
                    if l.retry.is_none() {
                        log(&format!("{bin} delivery to {who} failed ({why}); retrying with backoff"));
                    }
                    let wait = l.retry.map_or(Duration::from_secs(15), |(_, w)| (w * 2).min(Duration::from_secs(1800)));
                    l.retry = Some((Instant::now() + wait, wait));
                }
            }
            Err(why) => {
                log(&format!("{bin} delivery to {} failed ({why}); falling back to hooks", self.logged(&g, key)));
                // Hooks (or check_inbox) show it instead; tell senders the truth from now on.
                if let Some(l) = g.live.get_mut(key) {
                    l.push_failed = true;
                    l.reg.tier = Tier::Turn;
                }
            }
        }
    }

    #[cfg(test)]
    pub fn receive(self: &Arc<Self>, peer: EndpointId, msg: Message) -> Result<(String, String)> {
        self.receive_readdressed(peer, msg).map(|(state, note, _)| (state, note))
    }

    /// A message from a teammate's machine. Returns the state and a note for the sender. A `retry` state
    /// means "not now": the sender keeps it in its outbox. With the address it went to, when that is not the one it
    /// was sent to: the name the session it was sent to (at the default name it went by before its title named it)
    /// goes by now, which its replies come from.
    pub fn receive_readdressed(
        self: &Arc<Self>,
        peer: EndpointId,
        msg: Message,
    ) -> Result<(String, String, Option<String>)> {
        let mut to = None;
        let (state, note) = self.receive_to(peer, msg, &mut to)?;
        Ok((state, note, to))
    }

    fn receive_to(
        self: &Arc<Self>,
        peer: EndpointId,
        msg: Message,
        to: &mut Option<String>,
    ) -> Result<(String, String)> {
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
        let name = name.to_string();
        let asked = msg.to.clone();

        let res = {
            let mut g = self.inner.lock().unwrap();
            self.prune(&mut g);
            // Sent again (word that it was taken got lost on the way): where it went then, not where its name leads now.
            if let Some(had) = g.store.message(&msg.id)? {
                if render::without_machine(&had.from) == msg.from && had.to != asked {
                    *to = Some(had.to);
                }
                return Ok(("queued".into(), "already delivered".into()));
            }
            let key = g.live_called(&name);
            // To the default name a live session went by before its title named it: that session's, at its name now.
            let name = match key.as_ref().map(|k| &g.live[k]) {
                Some(l) if l.name != name => {
                    msg.to = self.addr(&l.name);
                    l.name.clone()
                }
                _ => name,
            };
            let remembered = if key.is_none() { g.store.session_by_name(&name)? } else { None };
            // To the default name an offline session went by before its title named it: at its name now.
            let name = match remembered.as_ref() {
                Some(r) if r.name != name => {
                    msg.to = self.addr(&r.name);
                    r.name.clone()
                }
                _ => name,
            };
            let name = name.as_str();
            // Where it went, said only once it is taken: not for a refusal or a "retry".
            let went = (msg.to != asked).then(|| msg.to.clone());
            if key.is_none()
                && remembered
                    .as_ref()
                    .is_none_or(|r| self.cfg.off_in(&r.cwd) && !(self.cfg.all_folders_off && policy == Policy::Hold))
            {
                return Ok(("refused".into(), format!("no session called {} on {}", msg.to, self.machine)));
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
            *to = went;
            // Each machine has its own terminal: replies go back to the one this came from.
            msg.from = render::shown_from(&msg.from, &machine);
            // A held message's files are saved only once the person approves it.
            let held = policy == Policy::Hold;
            // Held for a session whose name is kept, it is that session's: it goes when the session does.
            let kept = held && g.store.session_by_name(name)?.is_some_and(|r| r.kept());
            // Counted once, when it is new here; a held one once approved.
            if g.store.insert(&mut msg, held)? && !held {
                self.record_remote_received(&msg);
                let _ = g.store.count(util::now_ms(), Counted::Received);
            }
            let (state, note) = match key {
                _ if kept => (
                    "held".into(),
                    format!("held until {} approves it; dropped if that session is gone for a day first", self.person),
                ),
                _ if held => ("held".into(), format!("held until {} approves it", self.person)),
                Some(key) => self.deliver(&mut g, &key, vec![msg.clone()], msg.wake),
                None => self.deliver_offline(&mut g, remembered.as_ref(), vec![msg.clone()], msg.wake),
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

    /// `peer` answered message `id` from our outbox with `state`, and where it went when not to the address it was sent
    /// to.
    pub fn acked(&self, peer: &EndpointId, id: &str, state: &str, to: Option<String>) {
        // Only once it was taken: a "retry" may go elsewhere next time, or nowhere.
        if let Some(to) = to.filter(|_| state != "refused" && state != "retry") {
            self.readdressed(peer, id, &to);
        }
        // "retry": the other side is busy; keep it in the outbox for the next flush.
        if state != "retry" {
            self.outbox_done(id, state != "refused");
        }
    }

    /// `peer` delivered message `id` to `to`, the name the session it was sent to goes by now (see
    /// `receive_readdressed`): its receipt waits for a reply from there. Only for a session of the person it was sent
    /// to, on that person's machine.
    pub fn readdressed(&self, peer: &EndpointId, id: &str, to: &str) {
        let Some((person, machine)) =
            self.team_state().and_then(|t| t.member(peer).map(|m| (m.person.clone(), m.machine.clone())))
        else {
            return;
        };
        let theirs =
            |a: &str| render::without_machine(a).split_once('/').is_some_and(|(p, n)| p == person && !n.is_empty());
        if !theirs(to) || to.contains('@') {
            return;
        }
        let g = self.inner.lock().unwrap();
        if let Ok(Some(old)) = g.store.receipt_recipient(id)
            && theirs(&old)
        {
            let _ = g.store.readdress_receipt(id, &old, &render::shown_from(to, &machine));
        }
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
        let (agent, stop) = {
            let g = self.inner.lock().unwrap();
            let l = g.live.get(&key).ok_or_else(unregistered)?;
            (Some(l.reg.agent.clone()), l.stop_gen)
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
                let Some(l) = g.live.get(&key).filter(|l| l.stop_gen == stop) else {
                    return Ok(Inbox { messages: Vec::new(), text: String::new() });
                };
                // By its current name: a rename while waiting frees the old one for another session.
                let address = self.addr(&l.name);
                let messages = g.pending(&address)?;
                if !messages.is_empty() && !peek {
                    let ids: Vec<String> = messages.iter().map(|m| m.id.clone()).collect();
                    let changed = g.store.mark_read(&ids, util::now_ms())?;
                    if g.live.get(&key).is_some_and(|l| l.through_reason == Some(false)) {
                        g.store.handoff(&changed, util::now_ms())?;
                        for id in &changed {
                            self.usage.stage(id, "delivered");
                        }
                    }
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

    // Both immediate receipt and an approved hold enter the eligible inbox once.
    // Keep their observation together; neither telemetry failure can hold up delivery.
    fn record_remote_received(&self, msg: &Message) {
        let dims = crate::usage::Dimensions {
            source: msg.from_agent.as_deref().map(crate::usage::family).unwrap_or_else(|| "unknown".into()),
            locality: "remote".into(),
            ..Default::default()
        };
        self.usage.start(&msg.id, dims.clone(), msg.reply_to.as_deref(), true);
        self.usage.count("received", dims, 1);
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
                self.record_remote_received(m);
                let name = m.to.split_once('/').map_or("", |(_, n)| n);
                if let Some(key) = g.live_called(name) {
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
pub(super) fn approval_holds(a: &Signed<Approval>, team: &str, peer: &EndpointId, sender: &str, to: &str) -> bool {
    let b = &a.body;
    let size: usize = [&b.about, &b.asker, &b.question, &b.request, &b.note].iter().map(|s| s.chars().count()).sum();
    size <= 2000 && a.by == *peer && b.approver == sender && b.asker == to && a.verify(team, "approval")
}

/// A message's text within [`MAX_BODY`].
fn fits(body: &str) -> Result<()> {
    if body.len() > MAX_BODY {
        // A path means nothing on the recipient's computer.
        bail!(
            "message is {} KB; the limit is {} KB (send a small text file with `files`, or a link)",
            body.len().div_ceil(1024),
            MAX_BODY / 1024
        );
    }
    Ok(())
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
    // Held to what its sender's machine checks (as are its files, below): past that, it is refused and nothing kept.
    fits(&m.body)?;
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
    use crate::daemon::sessions::Rename;
    use crate::team::TeamState;
    use iroh::SecretKey;
    use serde::Deserialize;
    use std::collections::HashMap;

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
            claude_dir: None,
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
            claude_dir: None,
        }
    }

    /// Not Codex: mail for it while it is offline only waits (see `deliver_offline`).
    fn gemini(key: &str) -> SessionReg {
        SessionReg { agent: "gemini".into(), ..codex(key) }
    }

    fn disk_daemon() -> (Arc<Daemon>, rusqlite::Connection, std::path::PathBuf) {
        let home = std::env::temp_dir().join(format!("reason-network-send-{}", Ulid::generate()));
        std::fs::create_dir_all(&home).unwrap();
        let path = home.join("network.db");
        let store = crate::store::Store::open(&path, home.join("inbox")).unwrap();
        let d = Arc::new(Daemon::with(crate::config::Config::default(), store, SecretKey::generate()).unwrap());
        (d, rusqlite::Connection::open(path).unwrap(), home)
    }

    #[tokio::test]
    async fn a_fresh_send_refused_before_dispatch_has_a_typed_code() {
        let d = Arc::new(super::super::test_daemon(crate::config::Config::default()));
        let error = d.send("missing".into(), "alex/api", "hello", None, true, false, None, vec![]).await.unwrap_err();
        let reply = serde_json::to_value(Reply::err(7, error)).unwrap();
        assert_eq!(reply["error"], unregistered().to_string());
        assert_eq!(reply["code"], "send_not_dispatched");
        let error = d.send("missing".into(), "alex/api", "", None, true, false, None, vec![]).await.unwrap_err();
        let reply = serde_json::to_value(Reply::err(8, error)).unwrap();
        assert_eq!(reply["error"], "empty message");
        assert_eq!(reply["code"], "send_not_dispatched");
    }

    #[tokio::test]
    async fn a_fresh_key_can_report_a_recipient_refusal_but_a_pending_key_or_failed_lookup_cannot() {
        let (d, db, home) = disk_daemon();
        d.register(Some(1), codex("codex:1"), crate::git::GitInfo::default()).unwrap();
        let send = |key: &str| {
            d.send_idempotent(
                "codex:1".into(),
                "no-such-recipient",
                "hello",
                None,
                true,
                false,
                None,
                vec![],
                Some(key.into()),
            )
        };
        let fresh = Reply::err(7, send("job-1").await.unwrap_err());
        assert!(fresh.error.as_deref().unwrap().starts_with("no session called 'no-such-recipient'"));
        assert_eq!(fresh.code.as_deref(), Some("send_not_dispatched"));
        // The failed resolution left a pending key. A retry cannot assume what happened on that attempt.
        let pending = Reply::err(8, send("job-1").await.unwrap_err());
        assert_eq!(pending.error, fresh.error);
        assert_eq!(pending.code, None);
        assert_eq!(
            db.query_row("SELECT count(*) FROM send_keys WHERE receipt IS NULL", [], |r| r.get::<_, i64>(0)).unwrap(),
            1
        );
        // A different job is independently proven fresh.
        assert_eq!(Reply::err(9, send("job-2").await.unwrap_err()).code.as_deref(), Some("send_not_dispatched"));
        // Even an actual insertion is not enough if the following receipt read cannot complete.
        db.execute_batch(
            "CREATE TRIGGER corrupt_receipt AFTER INSERT ON send_keys
             BEGIN UPDATE send_keys SET receipt='invalid JSON' WHERE key=NEW.key; END;",
        )
        .unwrap();
        let failed_lookup = Reply::err(10, send("job-3").await.unwrap_err());
        assert!(failed_lookup.error.unwrap().contains("expected value"));
        assert_eq!(failed_lookup.code, None);
        for table in ["messages", "outbox", "sent_receipts"] {
            assert_eq!(db.query_row(&format!("SELECT count(*) FROM {table}"), [], |r| r.get::<_, i64>(0)).unwrap(), 0);
        }
        drop(db);
        drop(d);
        std::fs::remove_dir_all(home).unwrap();
    }

    #[tokio::test]
    async fn a_receipt_failure_after_push_never_says_not_dispatched_and_retries_do_not_push_again() {
        // Fail either finish_send: the first saves the queued result, the second the peer result.
        for condition in ["1", "OLD.receipt IS NOT NULL"] {
            let (d, db, home) = disk_daemon();
            d.register(Some(1), codex("codex:1"), crate::git::GitInfo::default()).unwrap();
            let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
            d.inner.lock().unwrap().conns.insert(7, tx);
            let to = d
                .register(
                    Some(7),
                    SessionReg { agent: agent::CLAUDE_CODE.into(), tier: Tier::Push, ..codex("claude-code:2") },
                    crate::git::GitInfo::default(),
                )
                .unwrap()
                .address;
            db.execute_batch(&format!(
                "CREATE TRIGGER fail_receipt BEFORE UPDATE OF receipt ON send_keys WHEN {condition}
                 BEGIN SELECT RAISE(FAIL, 'receipt persistence failed'); END;"
            ))
            .unwrap();
            let send = || {
                d.send_idempotent("codex:1".into(), &to, "hello", None, true, false, None, vec![], Some("job-1".into()))
            };
            let reply = serde_json::to_value(Reply::err(7, send().await.unwrap_err())).unwrap();
            assert!(reply["error"].as_str().unwrap().contains("receipt persistence failed"));
            assert_eq!(reply.get("code"), None);
            // The push happened, not merely an attempted database write.
            let Event::Deliver { ids, .. } = serde_json::from_str(&rx.try_recv().unwrap()).unwrap();
            assert_eq!(ids.len(), 1);
            assert_eq!(d.inner.lock().unwrap().store.unread(&to).unwrap().len(), 1);
            db.execute_batch("DROP TRIGGER fail_receipt;").unwrap();
            let receipt = send().await.unwrap();
            assert_eq!(receipt.ids, ids);
            let cached = send().await.unwrap();
            assert_eq!(serde_json::to_value(cached).unwrap(), serde_json::to_value(receipt).unwrap());
            assert!(rx.try_recv().is_err());
            assert_eq!(d.inner.lock().unwrap().store.unread(&to).unwrap().len(), 1);
            drop(db);
            drop(d);
            std::fs::remove_dir_all(home).unwrap();
        }
    }

    #[tokio::test]
    async fn a_keyed_retry_cannot_claim_no_dispatch_when_its_receipt_or_sender_is_unavailable() {
        let (d, db, home) = disk_daemon();
        d.register(Some(1), codex("codex:1"), crate::git::GitInfo::default()).unwrap();
        let to = d.register(Some(2), codex("codex:2"), crate::git::GitInfo::default()).unwrap().address;
        let send =
            || d.send_idempotent("codex:1".into(), &to, "hello", None, true, false, None, vec![], Some("job-1".into()));
        let receipt = send().await.unwrap();
        db.execute_batch("UPDATE send_keys SET receipt='invalid JSON';").unwrap();
        let reply = Reply::err(7, send().await.unwrap_err());
        assert!(reply.error.unwrap().contains("expected value"));
        assert_eq!(reply.code, None);
        // SQLite lookup itself can fail too, with the earlier message still present.
        db.execute_batch("ALTER TABLE send_keys RENAME TO saved_send_keys;").unwrap();
        let reply = Reply::err(8, send().await.unwrap_err());
        assert!(reply.error.unwrap().contains("no such table"));
        assert_eq!(reply.code, None);
        d.inner.lock().unwrap().live.remove("codex:1");
        let reply = Reply::err(9, send().await.unwrap_err());
        assert_eq!(reply.error.as_deref(), Some(unregistered().to_string().as_str()));
        assert_eq!(reply.code, None);
        let messages = d.inner.lock().unwrap().store.unread(&to).unwrap();
        assert_eq!(messages.len(), 1);
        assert_eq!(messages[0].id, receipt.ids[0]);
        drop(db);
        drop(d);
        std::fs::remove_dir_all(home).unwrap();
    }

    #[tokio::test]
    async fn an_unkeyed_send_stops_classifying_errors_before_its_first_durable_attempt() {
        for table in ["sent_receipts", "messages"] {
            let (d, db, home) = disk_daemon();
            d.register(Some(1), codex("codex:1"), crate::git::GitInfo::default()).unwrap();
            let to = d.register(Some(2), codex("codex:2"), crate::git::GitInfo::default()).unwrap().address;
            db.execute_batch(&format!(
                "CREATE TRIGGER fail_write BEFORE INSERT ON {table}
                 BEGIN SELECT RAISE(FAIL, 'durable write failed'); END;"
            ))
            .unwrap();
            let error = d.send("codex:1".into(), &to, "hello", None, true, false, None, vec![]).await.unwrap_err();
            let reply = Reply::err(7, error);
            assert!(reply.error.unwrap().contains("durable write failed"));
            assert_eq!(reply.code, None);
            assert!(d.inner.lock().unwrap().store.unread(&to).unwrap().is_empty());
            drop(db);
            drop(d);
            std::fs::remove_dir_all(home).unwrap();
        }
    }

    /// Takes a session here offline, last seen `ago` before now. Returns its name.
    fn gone(d: &Daemon, key: &str, ago: i64) -> String {
        let mut g = d.inner.lock().unwrap();
        let name = g.live.remove(key).unwrap().name;
        g.store.touch_session(&name, util::now_ms() - ago).unwrap();
        name
    }

    /// A Codex thread's hook, run with `CODEX_HOME` set to `home`.
    fn codex_hook(thread: &str, event: &str, home: Option<&str>) -> HookCall {
        HookCall {
            through_reason: Some(false),
            agent: "codex".into(),
            event: event.into(),
            session_id: thread.into(),
            cwd: Some("/work/api".into()),
            pid: None,
            mail: true,
            transcript: None,
            doing: None,
            keep: false,
            name: None,
            dirs: Vec::new(),
            codex_home: home.map(str::to_string),
        }
    }

    #[test]
    fn the_codex_queue_command_runs_against_the_home_the_sessions_hooks_named() {
        let args = |c: &tokio::process::Command| c.as_std().get_args().map(|a| a.to_owned()).collect::<Vec<_>>();
        let env = |c: &tokio::process::Command| {
            c.as_std().get_envs().find(|(k, _)| *k == "CODEX_HOME").and_then(|(_, v)| v.map(|v| v.to_owned()))
        };
        let program = || std::process::Command::new("codex");
        let theirs = handoff_command(program(), &codex_queue("t1", true), "hi", Some("/work/.codex-b"));
        assert_eq!(args(&theirs), ["queue", "--thread", "t1", "--resume", "--message", "hi"]);
        assert_eq!(env(&theirs).as_deref(), Some(std::ffi::OsStr::new("/work/.codex-b")));
        // Unset: the command gets the daemon's own environment, as before.
        let default = handoff_command(program(), &codex_queue("t1", true), "hi", None);
        assert_eq!(args(&default), args(&theirs));
        assert_eq!(default.as_std().get_envs().count(), 0);
        // An archived thread is only queued into, not loaded again.
        let archived = handoff_command(program(), &codex_queue("t1", false), "hi", None);
        assert_eq!(args(&archived), ["queue", "--thread", "t1", "--message", "hi"]);
    }

    #[test]
    fn only_a_codex_that_refuses_the_resume_flag_is_asked_again_without_it() {
        let with = codex_queue("t1", true);
        let clap =
            "error: unexpected argument '--resume' found\n\nUsage: codex queue --thread <THREAD> --message <TEXT>";
        assert!(refused_resume(&with, clap));
        // Any other failure is a failure: its hooks show the mail.
        assert!(!refused_resume(&with, "the local app-server daemon does not support thread/queue/add"));
        assert!(!refused_resume(&with, "Warning: message was queued, but could not resume thread t1: --resume"));
        // Already without it: nothing to drop.
        assert!(!refused_resume(&codex_queue("t1", false), clap));
    }

    #[tokio::test]
    async fn an_offline_codex_thread_seen_within_a_day_takes_wake_mail_through_codex_queue_in_its_home() {
        let d = Arc::new(super::super::test_daemon(crate::config::Config::default()));
        let home = "/work/.codex-b";
        d.hook(codex_hook("t1", "start", Some(home))).await.unwrap();
        // A thread from the default home: `codex queue` runs as before.
        d.hook(codex_hook("t2", "start", None)).await.unwrap();
        assert_eq!(d.inner.lock().unwrap().live["codex:t1"].codex_home.as_deref(), Some(home));
        let (name, default) = (gone(&d, "codex:t1", 3 * HOUR), gone(&d, "codex:t2", HOUR));
        let g = d.inner.lock().unwrap();
        let r = g.store.session_by_name(&name).unwrap().unwrap();
        let now = util::now_ms();
        assert_eq!(g.resumable_codex(&r, true, now), Some(("t1".into(), Some(home.into()))));
        let r2 = g.store.session_by_name(&default).unwrap().unwrap();
        assert_eq!(g.resumable_codex(&r2, true, now), Some(("t2".into(), None)));
        // An FYI waits for the thread's next turn: it starts none.
        assert_eq!(g.resumable_codex(&r, false, now), None);
        // Seen longer ago than a day, at the time the mail is sent: it waits.
        assert_eq!(g.resumable_codex(&r, true, r.last_seen + CODEX_RESUME_MS + 1), None);
        // Only a Codex thread its agent named.
        let by_process = Remembered { key: "proc:7".into(), ..r.clone() };
        assert_eq!(g.resumable_codex(&by_process, true, now), None);
        let other = Remembered { agent: "gemini".into(), key: "gemini:t1".into(), ..r.clone() };
        assert_eq!(g.resumable_codex(&other, true, now), None);
    }

    #[tokio::test]
    async fn mail_for_a_codex_thread_offline_for_over_a_day_waits_with_the_offline_note() {
        let d = Arc::new(super::super::test_daemon(crate::config::Config::default()));
        d.register(Some(1), gemini("gemini:1"), crate::git::GitInfo::default()).unwrap();
        d.hook(codex_hook("t1", "start", Some("/work/.codex-b"))).await.unwrap();
        let name = gone(&d, "codex:t1", 25 * HOUR);
        let res = d.send("gemini:1".into(), &name, "hi", None, true, false, None, Vec::new()).await.unwrap();
        assert_eq!(res.recipients[0].state, "queued");
        let note = &res.recipients[0].note;
        assert!(note.starts_with("queued: last seen 1d ago; unread mail may be dropped any time"), "{note}");
        let to = d.addr(&name);
        assert!(d.inner.lock().unwrap().in_flight.is_empty());
        assert_eq!(d.inner.lock().unwrap().store.unread(&to).unwrap().len(), 1);
        // Back at its next turn, its hook shows it.
        let shown = d.hook(codex_hook("t1", "start", Some("/work/.codex-b"))).await.unwrap();
        assert!(shown.text.unwrap().contains("hi"));
    }

    /// An offline Codex thread with mail handed to `codex queue` and not answered yet, as `deliver_offline` leaves it.
    async fn codex_queue_in_flight() -> (Arc<Daemon>, String, Vec<String>) {
        let d = Arc::new(super::super::test_daemon(crate::config::Config::default()));
        d.hook(codex_hook("t1", "start", None)).await.unwrap();
        // The MCP connection makes this a reachable conversation even on runners without the Codex CLI.
        d.register(Some(1), codex("codex:t1"), crate::git::GitInfo::default()).unwrap();
        let name = gone(&d, "codex:t1", HOUR);
        let to = d.addr(&name);
        let mut msg = Message {
            id: Ulid::generate().to_string(),
            from: format!("{}/web", d.person),
            from_agent: Some("claude-code".into()),
            from_repo: None,
            to: to.clone(),
            via: None,
            reply_to: None,
            body: "please look at the failing test".into(),
            wake: true,
            notify: false,
            sent_at: util::now_ms(),
            approval: None,
            files: Vec::new(),
        };
        let mut g = d.inner.lock().unwrap();
        assert!(g.store.session_by_name(&name).unwrap().unwrap().kept());
        g.store.insert(&mut msg, false).unwrap();
        g.in_flight.insert(msg.id.clone(), Instant::now());
        drop(g);
        (d, to, vec![msg.id])
    }

    #[tokio::test]
    async fn reclaim_skips_an_old_codex_handoff_and_only_a_failed_handoff_becomes_available() {
        for accepted in [true, false] {
            let (d, address, ids) = codex_queue_in_flight().await;
            d.register(Some(2), gemini("gemini:new"), crate::git::GitInfo::default()).unwrap();
            let name = address.split_once('/').unwrap().1;
            let req =
                Request::SetStatus { key: "gemini:new".into(), doing: None, name: Some(name.into()), reclaim: true };
            let new = d.handle(2, req).await.unwrap();
            assert_eq!(new["address"], address);
            assert_eq!(new["unread"], 0);
            assert!(d.inbox("gemini:new".into(), 0, false).await.unwrap().messages.is_empty());
            let mut waiting = Box::pin(d.inbox("gemini:new".into(), 10, false));
            assert!(futures_lite::future::poll_once(&mut waiting).await.is_none());
            d.handed("codex", "codex:t1", &ids, if accepted { Ok(()) } else { Err("offline".into()) }, None);
            let inbox = if accepted {
                d.inbox("gemini:new".into(), 0, false).await.unwrap()
            } else {
                tokio::time::timeout(Duration::from_secs(1), waiting).await.expect("retry wakes the inbox").unwrap()
            };
            let got: Vec<_> = inbox.messages.into_iter().map(|msg| msg.id).collect();
            assert_eq!(got, if accepted { vec![] } else { ids });
            assert!(d.inbox("gemini:new".into(), 0, false).await.unwrap().messages.is_empty());
        }
    }

    #[tokio::test]
    async fn mail_codex_queue_took_is_never_shown_again_by_the_hooks_of_the_turn_it_starts() {
        let (d, to, ids) = codex_queue_in_flight().await;
        // Resumed while `codex queue` still runs: SessionStart doesn't show it too.
        let start = d.hook(codex_hook("t1", "start", None)).await.unwrap();
        assert_eq!((start.text, start.ids), (None, Vec::<String>::new()));
        d.handed("codex", "codex:t1", &ids, Ok(()), None);
        assert!(d.inner.lock().unwrap().store.unread(&to).unwrap().is_empty());
        for event in ["prompt", "tool", "stop"] {
            assert_eq!(d.hook(codex_hook("t1", event, None)).await.unwrap().text, None, "{event}");
        }
    }

    #[tokio::test]
    async fn mail_codex_queue_failed_to_take_stays_unread_and_its_hooks_show_it_once() {
        let (d, to, ids) = codex_queue_in_flight().await;
        d.handed("codex", "codex:t1", &ids, Err("no app-server".into()), None);
        assert_eq!(d.inner.lock().unwrap().store.unread(&to).unwrap().len(), 1);
        let start = d.hook(codex_hook("t1", "start", None)).await.unwrap();
        assert!(start.text.unwrap().contains("please look at the failing test"));
        assert_eq!(start.ids, ids);
        // Shown, and waiting for the hook to confirm it: no other hook shows it meanwhile.
        assert_eq!(d.hook(codex_hook("t1", "stop", None)).await.unwrap().text, None);
    }

    #[tokio::test]
    async fn a_codex_threads_session_start_clears_its_failed_codex_queue() {
        let (d, _, ids) = codex_queue_in_flight().await;
        d.handed("codex", "codex:t1", &ids, Err("no app-server".into()), None);
        let failed = || d.inner.lock().unwrap().codex_failed.contains("codex:t1");
        assert!(failed());
        // Its other hooks leave the mark: its offline mail keeps waiting for its hooks.
        for event in ["prompt", "tool", "stop"] {
            d.hook(codex_hook("t1", event, None)).await.unwrap();
            assert!(failed(), "{event}");
        }
        // Started again: its offline wake mail gets another try at `codex queue`.
        d.hook(codex_hook("t1", "start", None)).await.unwrap();
        assert!(!failed());
    }

    /// A fake `codex` for `d` to run: it records each command, as `CODEX_HOME|arguments`, in a file of its own in the
    /// folder returned, and fails if `fails`, as `codex queue` does when it can't reach an app-server.
    #[cfg(unix)]
    fn fake_codex(d: &Daemon, fails: bool) -> std::path::PathBuf {
        fake_codex_ending(d, if fails { "echo 'no app-server' >&2; exit 1" } else { "exit 0" })
    }

    /// [`fake_codex`] whose commands end with the shell `end`, after recording them.
    #[cfg(unix)]
    fn fake_codex_ending(d: &Daemon, end: &str) -> std::path::PathBuf {
        use std::os::unix::fs::PermissionsExt;
        let dir = std::env::temp_dir().join(format!("reason-network-fake-codex-{}", Ulid::generate()));
        let log = dir.join("commands");
        std::fs::create_dir_all(&log).unwrap();
        let bin = dir.join("codex");
        let script =
            format!("#!/bin/sh\nprintf '%s|%s' \"${{CODEX_HOME:-default}}\" \"$*\" > '{}/'$$\n{end}\n", log.display());
        std::fs::write(&bin, script).unwrap();
        std::fs::set_permissions(&bin, std::fs::Permissions::from_mode(0o755)).unwrap();
        d.fake_bins.lock().unwrap().insert("codex", bin);
        log
    }

    /// The `codex queue` commands for `thread` the fake recorded in `log`, each as `CODEX_HOME|message`, once every one
    /// started has finished.
    #[cfg(unix)]
    async fn codex_queued(d: &Daemon, log: &std::path::Path, thread: &str) -> Vec<String> {
        for _ in 0..1000 {
            if d.inner.lock().unwrap().in_flight.is_empty() {
                let start = format!("|queue --thread {thread} --message ");
                let commands =
                    codex_commands(log).into_iter().map(|c| c.replacen(" --resume --message ", " --message ", 1));
                return commands.filter_map(|c| c.split_once(&start).map(|(home, m)| format!("{home}|{m}"))).collect();
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        panic!("codex queue never finished");
    }

    /// Every command the fake recorded in `log`, as `CODEX_HOME|arguments`.
    #[cfg(unix)]
    fn codex_commands(log: &std::path::Path) -> Vec<String> {
        std::fs::read_dir(log).unwrap().map(|f| std::fs::read_to_string(f.unwrap().path()).unwrap()).collect()
    }

    /// Feedback (sline, 2026-11-11): a Codex Desktop thread idle for 3 h was unsubscribed by Codex's app-server, which
    /// shut it down (SessionEnd); the daemon took it offline and its mail sat unread until the person woke that same
    /// thread by hand. Its mail now asks Codex to resume that same thread; a thread the person archived stays put.
    #[cfg(unix)]
    #[tokio::test]
    async fn mail_resumes_a_codex_thread_codex_unloaded_but_never_one_the_person_archived() {
        let d = Arc::new(super::super::test_daemon(crate::config::Config::default()));
        let log = fake_codex(&d, false);
        let home = log.parent().unwrap().join("codex-home");
        std::fs::create_dir_all(home.join("sessions/2026/11/11")).unwrap();
        std::fs::create_dir_all(home.join("archived_sessions")).unwrap();
        std::fs::write(home.join("archived_sessions/rollout-2026-11-10T09-00-00-t2.jsonl"), "{}\n").unwrap();
        let home_s = home.to_str().unwrap();
        d.register(Some(1), gemini("gemini:1"), crate::git::GitInfo::default()).unwrap();
        let mut names = Vec::new();
        for (conn, thread) in [(2, "t1"), (3, "t2")] {
            let key = format!("codex:{thread}");
            d.hook(codex_hook(thread, "start", Some(home_s))).await.unwrap();
            d.register(Some(conn), codex(&key), crate::git::GitInfo::default()).unwrap();
            // Codex's app-server unloads it: SessionEnd, then its MCP server exits. Offline, not forgotten: its name
            // and route are kept.
            d.hook(codex_hook(thread, "end", Some(home_s))).await.unwrap();
            d.drop_conn(conn);
            let g = d.inner.lock().unwrap();
            assert!(!g.live.contains_key(&format!("codex:{thread}")));
            let r = g.store.session_by_key(&format!("codex:{thread}")).unwrap().unwrap();
            assert!(r.kept(), "{thread}");
            names.push(r.name);
        }
        let send = |name: String| {
            let d = d.clone();
            async move {
                let res = d.send("gemini:1".into(), &name, "the build is green", None, true, false, None, Vec::new());
                res.await.unwrap().recipients.remove(0)
            }
        };
        let unloaded = send(names[0].clone()).await;
        assert_eq!(unloaded.state, "pushed");
        assert!(
            unloaded.note.contains("Codex is asked to load that same thread again, which starts a turn with it"),
            "{}",
            unloaded.note
        );
        let archived = send(names[1].clone()).await;
        assert_eq!(archived.state, "pushed");
        assert!(archived.note.contains("unless the person archived it"), "{}", archived.note);
        assert_eq!(codex_queued(&d, &log, "t1").await.len(), 1);
        let commands = codex_commands(&log);
        let ran = |args: &str| commands.iter().filter(|c| c.contains(&format!("|queue {args} --message "))).count();
        // The same thread, by its id: loaded again by Codex, never a new chat.
        assert_eq!((ran("--thread t1 --resume"), ran("--thread t1")), (1, 0), "{commands:?}");
        assert_eq!((ran("--thread t2 --resume"), ran("--thread t2")), (0, 1), "{commands:?}");
        assert!(commands.iter().all(|c| c.starts_with(&format!("{home_s}|"))), "{commands:?}");
        let _ = std::fs::remove_dir_all(log.parent().unwrap());
    }

    /// Codex exits 0 when it queued the message but could not load the thread again (no shared app-server it can reach,
    /// say), and says so on stderr. The mail is Codex's then: not queued twice, and senders hear it may wait for the
    /// person until a later `--resume` works or the thread starts again.
    #[cfg(unix)]
    #[tokio::test]
    async fn mail_codex_queued_but_could_not_resume_is_not_claimed_resumed() {
        let d = Arc::new(super::super::test_daemon(crate::config::Config::default()));
        let log = fake_codex_ending(
            &d,
            "case \"$*\" in *--resume*) echo 'Warning: message was queued, but could not resume thread t1: no \
             app-server daemon' >&2;; esac; exit 0",
        );
        d.register(Some(1), gemini("gemini:1"), crate::git::GitInfo::default()).unwrap();
        d.hook(codex_hook("t1", "start", None)).await.unwrap();
        let name = gone(&d, "codex:t1", HOUR);
        let to = d.addr(&name);
        let send = |text: &'static str| {
            let (d, name) = (d.clone(), name.clone());
            async move {
                let res = d.send("gemini:1".into(), &name, text, None, true, false, None, Vec::new());
                res.await.unwrap().recipients.remove(0)
            }
        };
        let first = send("first").await;
        assert!(first.note.contains("Codex is asked to load that same thread again"), "{}", first.note);
        // Taken once, by Codex's own queue: read, not queued again, not left to the hooks.
        assert_eq!(codex_queued(&d, &log, "t1").await.len(), 1);
        assert!(d.inner.lock().unwrap().store.unread(&to).unwrap().is_empty());
        assert!(!d.inner.lock().unwrap().codex_failed.contains("codex:t1"));
        assert!(d.inner.lock().unwrap().codex_unresumed.contains("codex:t1"));
        // The next sender hears the truth.
        let second = send("second").await;
        assert_eq!(second.state, "pushed");
        assert!(second.note.contains("could not load it again last time"), "{}", second.note);
        assert_eq!(codex_queued(&d, &log, "t1").await.len(), 2);
        // The thread starts again: a fresh try.
        d.hook(codex_hook("t1", "start", None)).await.unwrap();
        assert!(!d.inner.lock().unwrap().codex_unresumed.contains("codex:t1"));
        let _ = std::fs::remove_dir_all(log.parent().unwrap());
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn a_codex_without_queue_resume_still_takes_an_offline_threads_mail() {
        let d = Arc::new(super::super::test_daemon(crate::config::Config::default()));
        let log = fake_codex_ending(
            &d,
            "case \"$*\" in *--resume*) echo \"error: unexpected argument '--resume' found\" >&2; exit 2;; esac; exit 0",
        );
        d.register(Some(1), gemini("gemini:1"), crate::git::GitInfo::default()).unwrap();
        d.hook(codex_hook("t1", "start", None)).await.unwrap();
        let name = gone(&d, "codex:t1", HOUR);
        let to = d.addr(&name);
        let res = d.send("gemini:1".into(), &name, "hi", None, true, false, None, Vec::new()).await.unwrap();
        assert_eq!(res.recipients[0].state, "pushed");
        // Asked with `--resume`, refused, asked again as before: taken.
        assert_eq!(codex_queued(&d, &log, "t1").await.len(), 2);
        assert!(d.inner.lock().unwrap().store.unread(&to).unwrap().is_empty());
        assert!(!d.inner.lock().unwrap().codex_failed.contains("codex:t1"));
        let _ = std::fs::remove_dir_all(log.parent().unwrap());
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn wake_mail_from_here_and_from_a_teammate_goes_into_a_codex_thread_offline_since_today() {
        let (d, bob, _) = with_teammate();
        let log = fake_codex(&d, false);
        d.hook(codex_hook("t1", "start", Some("/work/.codex-b"))).await.unwrap();
        let name = gone(&d, "codex:t1", HOUR);
        let to = d.addr(&name);
        let res = d.send("codex:1".into(), &name, "from a session here", None, true, false, None, Vec::new());
        let sent = res.await.unwrap().recipients.remove(0);
        let mut msg = from_bob(&to, "from bob's agent".into(), Vec::new());
        msg.wake = true;
        let received = d.receive(bob.public(), msg).unwrap();
        for (state, note) in [(sent.state, sent.note), received] {
            assert_eq!(state, "pushed");
            assert!(note.starts_with("queued into the Codex thread, which is offline (last seen 1h ago)"), "{note}");
        }
        let queued = codex_queued(&d, &log, "t1").await;
        assert_eq!(queued.len(), 2, "{queued:?}");
        assert!(queued.iter().any(|q| q.contains("from a session here")) && queued.iter().any(|q| q.contains("bob")));
        // Against the home its hooks named, and with the reply tool named as Codex has it.
        assert!(queued.iter().all(|q| q.starts_with("/work/.codex-b|<reason-message ")), "{queued:?}");
        assert!(queued.iter().all(|q| q.contains("mcp__reason_network__send_message")), "{queued:?}");
        // Taken: read, and the turn it starts doesn't show it again.
        assert!(d.inner.lock().unwrap().store.unread(&to).unwrap().is_empty());
        assert_eq!(d.hook(codex_hook("t1", "start", Some("/work/.codex-b"))).await.unwrap().text, None);
        let _ = std::fs::remove_dir_all(log.parent().unwrap());
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn once_codex_queue_fails_for_an_offline_codex_thread_its_mail_waits_for_its_hooks() {
        let (d, bob, _) = with_teammate();
        let log = fake_codex(&d, true);
        d.hook(codex_hook("t1", "start", None)).await.unwrap();
        let name = gone(&d, "codex:t1", HOUR);
        let to = d.addr(&name);
        let send = |body: &'static str| {
            let (d, name) = (d.clone(), name.clone());
            async move {
                let res = d.send("codex:1".into(), &name, body, None, true, false, None, Vec::new()).await;
                res.unwrap().recipients.remove(0)
            }
        };
        assert_eq!(send("first try").await.state, "pushed");
        assert_eq!(codex_queued(&d, &log, "t1").await.len(), 1);
        // No second process for it, and the sender hears the truth: it waits.
        let second = send("second try").await;
        assert_eq!(second.state, "queued");
        assert!(second.note.starts_with("queued: last seen 1h ago"), "{}", second.note);
        let mut msg = from_bob(&to, "from bob's agent".into(), Vec::new());
        msg.wake = true;
        let (state, note) = d.receive(bob.public(), msg).unwrap();
        assert!(state == "queued" && note.starts_with("queued: last seen 1h ago"), "{state}: {note}");
        assert_eq!(codex_queued(&d, &log, "t1").await.len(), 1);
        // Its hooks show all of it when it is back.
        let shown = d.hook(codex_hook("t1", "start", None)).await.unwrap().text.unwrap();
        assert!(["first try", "second try", "from bob's agent"].iter().all(|b| shown.contains(b)), "{shown}");
        let _ = std::fs::remove_dir_all(log.parent().unwrap());
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn once_codex_queue_takes_a_threads_mail_again_its_offline_mail_goes_through_it_again() {
        let (d, _, _) = with_teammate();
        let failing = fake_codex(&d, true);
        d.hook(codex_hook("t1", "start", None)).await.unwrap();
        let name = gone(&d, "codex:t1", HOUR);
        let to = d.addr(&name);
        let send = |body: &'static str| {
            let (d, name) = (d.clone(), name.clone());
            async move {
                let res = d.send("codex:1".into(), &name, body, None, true, false, None, Vec::new()).await;
                res.unwrap().recipients.remove(0)
            }
        };
        assert_eq!(send("first try").await.state, "pushed");
        assert_eq!(codex_queued(&d, &failing, "t1").await.len(), 1);
        assert!(d.inner.lock().unwrap().codex_failed.contains("codex:t1"));
        // Back at a prompt (no SessionStart, which clears the mark too), with `codex queue` working again: its hook
        // shows what failed, and it takes mail through `codex queue`, as with `codex` on PATH.
        let working = fake_codex(&d, false);
        let shown = d.hook(codex_hook("t1", "prompt", None)).await.unwrap();
        assert!(shown.text.unwrap().contains("first try"));
        d.handle(0, Request::Delivered { key: "codex:t1".into(), ids: shown.ids }).await.unwrap();
        d.inner.lock().unwrap().live.get_mut("codex:t1").unwrap().reg.tier = Tier::Push;
        let live = send("while it is here").await;
        assert!(live.state == "pushed" && live.note.starts_with("queued into the Codex session"), "{}", live.note);
        assert_eq!(codex_queued(&d, &working, "t1").await.len(), 1);
        assert!(!d.inner.lock().unwrap().codex_failed.contains("codex:t1"));
        // Offline again: its next wake mail goes through `codex queue` again, which takes it once.
        gone(&d, "codex:t1", HOUR);
        let later = send("once it is gone again").await;
        assert!(later.state == "pushed" && later.note.starts_with("queued into the Codex thread"), "{}", later.note);
        let queued = codex_queued(&d, &working, "t1").await;
        assert_eq!(queued.len(), 2, "{queued:?}");
        assert_eq!(queued.iter().filter(|q| q.contains("once it is gone again")).count(), 1, "{queued:?}");
        assert!(d.inner.lock().unwrap().store.unread(&to).unwrap().is_empty());
        // Read: the turn it starts doesn't show it again.
        assert_eq!(d.hook(codex_hook("t1", "start", None)).await.unwrap().text, None);
        for log in [failing, working] {
            let _ = std::fs::remove_dir_all(log.parent().unwrap());
        }
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn no_codex_queue_for_an_offline_codex_thread_in_a_folder_that_is_off() {
        let folders = std::collections::BTreeMap::from([("/work/personal".to_string(), "off".to_string())]);
        let d = Arc::new(super::super::test_daemon(crate::config::Config { folders, ..Default::default() }));
        let log = fake_codex(&d, false);
        d.register(Some(1), gemini("gemini:1"), crate::git::GitInfo::default()).unwrap();
        // Its hooks last said it is in a folder that is off: it was in one that is on when it was here.
        d.hook(codex_hook("t1", "start", None)).await.unwrap();
        let name = gone(&d, "codex:t1", HOUR);
        let off = HookCall { cwd: Some("/work/personal".into()), ..codex_hook("t1", "prompt", None) };
        assert!(d.hook(off).await.is_err());
        let res = d.send("gemini:1".into(), &name, "hi", None, true, false, None, Vec::new()).await.unwrap();
        assert_eq!(res.recipients[0].state, "queued", "{}", res.recipients[0].note);
        // Remembered in a folder that is off (turned off since). In a folder of its own: never the first one's name.
        d.hook(HookCall { cwd: Some("/work/web".into()), ..codex_hook("t2", "start", None) }).await.unwrap();
        let other = gone(&d, "codex:t2", HOUR);
        {
            let mut g = d.inner.lock().unwrap();
            let mut r = g.store.session_by_name(&other).unwrap().unwrap();
            r.cwd = "/work/personal/notes".into();
            let mut msg = from_bob(&d.addr(&other), "hi".into(), Vec::new());
            msg.wake = true;
            g.store.insert(&mut msg, false).unwrap();
            assert_eq!(d.deliver_offline(&mut g, Some(&r), vec![msg], true).0, "queued");
        }
        assert!(codex_queued(&d, &log, "t2").await.is_empty());
        assert_eq!(std::fs::read_dir(&log).unwrap().count(), 0);
        let _ = std::fs::remove_dir_all(log.parent().unwrap());
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
            mailbox: agent == "cli",
        };
        let back = "it reaches that session when it's back, unless another session here explicitly takes its name";
        assert_eq!(
            offline_note(Some(&seen("codex:1", "codex")), 3 * HOUR),
            format!("queued: last seen 3h ago; unread mail is dropped after about 21h if it stays offline; {back}")
        );
        assert_eq!(
            offline_note(Some(&seen("codex:1", "codex")), 24 * HOUR),
            format!("queued: last seen 1d ago; unread mail may be dropped any time; {back}")
        );
        // The person's own mailboxes are never forgotten.
        assert_eq!(
            offline_note(Some(&seen("cli:t", "cli")), 30 * HOUR),
            "queued: last seen 1d ago; it reaches that session when it's back"
        );
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
        let to = d.register(Some(2), gemini("gemini:2"), crate::git::GitInfo::default()).unwrap().address;
        gone(&d, "gemini:2", 3 * HOUR);
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
        d.register(Some(1), gemini("gemini:1"), crate::git::GitInfo::default()).unwrap();
        let name = gone(&d, "gemini:1", 3 * HOUR);
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
        assert!(note.ends_with("unless another session here explicitly takes its name"), "{note}");
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
            text.contains("From bob, typed in a terminal.") && text.contains("send_message) to \"bob/terminal@xps\""),
            "{text}"
        );
        // Asked the other way: a message to `bob/terminal` names it in its receipt as Bob's answer will show here, so a
        // reply with no `reply_to` matches it (the Reason CLI and server match answers that way).
        let mut terminal = d.row("terminal", "cli", "/", None);
        terminal.tier = Tier::Pull;
        let remotes = HashMap::from([(
            bob.public(),
            crate::net::Remote {
                presence: crate::net::Presence { sessions: vec![terminal], ..Default::default() },
                last_seen: util::now_ms(),
                online: true,
            },
        )]);
        let g = d.inner.lock().unwrap();
        let targets = d.resolve(&g, &remotes, "bob/terminal", None).unwrap();
        assert_eq!(targets.iter().map(|t| t.shown_address()).collect::<Vec<_>>(), [got[0].from.clone()]);
        // A teammate's session goes by its name alone, there and here.
        let mut web = d.row("web-codex", "codex", "/", None);
        web.tier = Tier::Pull;
        let remotes = HashMap::from([(
            bob.public(),
            crate::net::Remote {
                presence: crate::net::Presence { sessions: vec![web], ..Default::default() },
                last_seen: util::now_ms(),
                online: true,
            },
        )]);
        let targets = d.resolve(&g, &remotes, "bob/web-codex", None).unwrap();
        assert_eq!(targets.iter().map(|t| t.shown_address()).collect::<Vec<_>>(), ["bob/web-codex"]);
    }

    #[tokio::test]
    async fn a_waiting_inbox_follows_its_session_through_a_rename() {
        let d = Arc::new(super::super::test_daemon(crate::config::Config::default()));
        let named = |key: &str, name: &str| SessionReg { name: Some(name.into()), ..codex(key) };
        let old = d.register(Some(1), named("codex:a", "old"), crate::git::GitInfo::default()).unwrap().address;
        d.register(Some(2), codex("codex:s"), crate::git::GitInfo::default()).unwrap();
        let waiting = tokio::spawn({
            let d = d.clone();
            async move { d.inbox("codex:a".into(), 5, true).await.unwrap() }
        });
        tokio::time::sleep(Duration::from_millis(50)).await;
        let new = d.set_status("codex:a", None, Some("new".into()), Rename::Explicit).unwrap().address;
        // Another session takes the name it gave up; its mail is its own.
        d.register(Some(3), named("codex:b", "old"), crate::git::GitInfo::default()).unwrap();
        for (to, body) in [(&old, "for b"), (&new, "for a")] {
            d.send("codex:s".into(), to, body, None, true, false, None, Vec::new()).await.unwrap();
        }
        let got: Vec<String> = waiting.await.unwrap().messages.into_iter().map(|m| m.body).collect();
        assert_eq!(got, ["for a"]);
        let b: Vec<String> = d.inner.lock().unwrap().pending(&old).unwrap().into_iter().map(|m| m.body).collect();
        assert_eq!(b, ["for b"]);
    }

    #[tokio::test]
    async fn a_reading_wait_never_marks_the_new_owners_mail_read() {
        let d = Arc::new(super::super::test_daemon(crate::config::Config::default()));
        let named = |key: &str, name: &str| SessionReg { name: Some(name.into()), ..codex(key) };
        let old = d.register(Some(1), named("codex:a", "old"), crate::git::GitInfo::default()).unwrap().address;
        d.register(Some(2), codex("codex:s"), crate::git::GitInfo::default()).unwrap();
        let waiting = tokio::spawn({
            let d = d.clone();
            async move { d.inbox("codex:a".into(), 5, false).await.unwrap() }
        });
        tokio::time::sleep(Duration::from_millis(50)).await;
        let new = d.set_status("codex:a", None, Some("new".into()), Rename::Explicit).unwrap().address;
        d.register(Some(3), named("codex:b", "old"), crate::git::GitInfo::default()).unwrap();
        for (to, body) in [(&old, "for b"), (&new, "for a")] {
            d.send("codex:s".into(), to, body, None, true, false, None, Vec::new()).await.unwrap();
        }
        let got: Vec<String> = waiting.await.unwrap().messages.into_iter().map(|m| m.body).collect();
        assert_eq!(got, ["for a"]);
        let b: Vec<String> = d.inner.lock().unwrap().store.unread(&old).unwrap().into_iter().map(|m| m.body).collect();
        assert_eq!(b, ["for b"]);
        assert!(d.inner.lock().unwrap().store.unread(&new).unwrap().is_empty());
    }

    #[tokio::test]
    async fn a_wait_whose_session_leaves_ends_empty_and_leaves_the_mail() {
        let d = Arc::new(super::super::test_daemon(crate::config::Config::default()));
        let named = |key: &str, name: &str| SessionReg { name: Some(name.into()), ..gemini(key) };
        let old = d.register(Some(1), named("gemini:a", "old"), crate::git::GitInfo::default()).unwrap().address;
        d.register(Some(2), codex("codex:s"), crate::git::GitInfo::default()).unwrap();
        let waiting = tokio::spawn({
            let d = d.clone();
            async move { d.inbox("gemini:a".into(), 5, false).await.unwrap() }
        });
        tokio::time::sleep(Duration::from_millis(50)).await;
        d.drop_conn(1);
        d.register(Some(3), named("gemini:b", "old"), crate::git::GitInfo::default()).unwrap();
        let t0 = std::time::Instant::now();
        d.send("codex:s".into(), &old, "for b", None, true, false, None, Vec::new()).await.unwrap();
        let got = waiting.await.unwrap().messages;
        assert!(got.is_empty());
        assert!(t0.elapsed() < Duration::from_secs(2));
        assert_eq!(d.inner.lock().unwrap().store.unread(&old).unwrap().len(), 1);
    }

    #[tokio::test]
    async fn reclaim_pushes_waiting_mail_to_the_replacements_connection() {
        let d = Arc::new(super::super::test_daemon(crate::config::Config::default()));
        d.register(Some(1), codex("codex:sender"), crate::git::GitInfo::default()).unwrap();
        let old = SessionReg { name: Some("handoff".into()), ..gemini("gemini:old") };
        let address = d.register(Some(2), old, crate::git::GitInfo::default()).unwrap().address;
        d.drop_conn(2);
        let sent =
            d.send("codex:sender".into(), &address, "waiting message", None, true, false, None, vec![]).await.unwrap();
        assert_eq!(sent.recipients[0].state, "queued");
        let fyi = d
            .send("codex:sender".into(), &address, "for a natural pause", None, false, false, None, vec![])
            .await
            .unwrap();

        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        d.inner.lock().unwrap().conns.insert(7, tx);
        let key = "claude-code:new";
        let reg = SessionReg { agent: agent::CLAUDE_CODE.into(), tier: Tier::Push, ..codex(key) };
        d.register(Some(7), reg, crate::git::GitInfo::default()).unwrap();
        let rename =
            || Request::SetStatus { key: key.into(), doing: None, name: Some("handoff".into()), reclaim: true };
        // The CLI uses a different connection from the session's MCP server.
        let reply = d.handle(9, rename()).await.unwrap();
        assert_eq!(reply["address"], address);
        assert_eq!(reply["unread"], 2);
        let Event::Deliver { key: recipient, ids, text } = serde_json::from_str(&rx.try_recv().unwrap()).unwrap();
        assert_eq!(recipient, key);
        assert_eq!(ids, sent.ids);
        assert!(text.contains("waiting message") && !text.contains("for a natural pause"));
        d.handle(9, rename()).await.unwrap();
        assert!(rx.try_recv().is_err());
        d.handle(7, Request::Delivered { key: key.into(), ids }).await.unwrap();
        let inbox = d.inbox(key.into(), 0, false).await.unwrap();
        assert_eq!(inbox.messages.into_iter().map(|m| m.id).collect::<Vec<_>>(), fyi.ids);
    }

    #[tokio::test]
    async fn a_claude_session_is_told_delivered_only_through_a_server_that_knows_its_inbox() {
        let d = Arc::new(super::super::test_daemon(crate::config::Config::default()));
        d.register(Some(1), codex("codex:1"), crate::git::GitInfo::default()).unwrap();
        // Claude Code's MCP server, connected, before Claude Code has listed its inbox.
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        d.inner.lock().unwrap().conns.insert(7, tx);
        let claude = |tier| SessionReg {
            key: "claude-code:a".into(),
            agent: agent::CLAUDE_CODE.into(),
            cwd: "/work/web".into(),
            agent_session_id: None,
            pid: None,
            tier,
            name: None,
            claude_dir: None,
        };
        let to = d.register(Some(7), claude(Tier::Pull), crate::git::GitInfo::default()).unwrap().address;
        let send = |body: &'static str| d.send("codex:1".into(), &to, body, None, true, false, None, Vec::new());
        let r = send("one").await.unwrap();
        assert_eq!((r.recipients[0].state.as_str(), rx.try_recv().is_err()), ("queued", true));
        // Its inbox has turned up: the server registers again, as push; what waited, and the next, go through it.
        d.register(Some(7), claude(Tier::Push), crate::git::GitInfo::default()).unwrap();
        let r = send("two").await.unwrap();
        assert_eq!(r.recipients[0].state, "pushed");
        let mut pushed = Vec::new();
        while let Ok(line) = rx.try_recv() {
            let Event::Deliver { ids, .. } = serde_json::from_str(&line).unwrap();
            pushed.extend(ids);
        }
        assert_eq!(pushed.len(), 2);
        // The server finds it can't after all: unread again, and not pushed through it any more.
        let key = "claude-code:a".to_string();
        d.push_failed(&key, &pushed);
        {
            let g = d.inner.lock().unwrap();
            assert!(g.in_flight.is_empty());
            assert!(!g.live[&key].mcp_push);
        }
        let unread: Vec<String> = d.inner.lock().unwrap().pending(&to).unwrap().into_iter().map(|m| m.body).collect();
        assert_eq!(unread, ["one", "two"]);
        let r = send("three").await.unwrap();
        assert_eq!((r.recipients[0].state.as_str(), rx.try_recv().is_err()), ("queued", true));
    }

    #[tokio::test]
    async fn a_codex_session_has_one_turn_watch_and_it_ends_once_its_wake_mail_is_read() {
        let d = Arc::new(super::super::test_daemon(crate::config::Config::default()));
        d.register(Some(1), codex("codex:1"), crate::git::GitInfo::default()).unwrap();
        let reg = SessionReg { cwd: "/work/web".into(), ..codex("codex:2") };
        let to = d.register(Some(2), reg, crate::git::GitInfo::default()).unwrap().address;
        let send = |body: &'static str, wake| d.send("codex:1".into(), &to, body, None, wake, false, None, Vec::new());
        send("fyi", false).await.unwrap();
        send("wake", true).await.unwrap();
        // Its turn runs.
        let rollout = std::env::temp_dir().join(format!("reason-network-watch-{}.jsonl", Ulid::generate()));
        std::fs::write(&rollout, "{\"type\":\"event_msg\",\"payload\":{\"type\":\"task_started\"}}\n").unwrap();
        d.inner.lock().unwrap().live.get_mut("codex:2").unwrap().transcript = Some(rollout.clone());

        let tasks = || tokio::runtime::Handle::current().metrics().num_alive_tasks();
        let watching = || d.inner.lock().unwrap().codex_watch.contains("codex:2");
        let before = tasks();
        {
            let mut g = d.inner.lock().unwrap();
            d.watch_codex_turn(&mut g, "codex:2");
            d.watch_codex_turn(&mut g, "codex:2");
        }
        assert_eq!(tasks(), before + 1, "more mail for the turn starts no second watch");
        tokio::time::sleep(CODEX_TURN_POLL + Duration::from_millis(500)).await;
        assert!(watching(), "watching while wake mail waits and the turn runs");
        // A hook took the wake mail; the FYI mail is still unread, and keeps no watch going.
        let wake: Vec<String> = {
            let g = d.inner.lock().unwrap();
            g.store.unread(&to).unwrap().into_iter().filter(|m| m.wake).map(|m| m.id).collect()
        };
        d.inner.lock().unwrap().store.mark_read(&wake, util::now_ms()).unwrap();
        tokio::time::sleep(CODEX_TURN_POLL + Duration::from_millis(500)).await;
        assert!(!watching(), "the watch ended");
        assert!(tasks() <= before, "…and its task with it");
        let _ = std::fs::remove_file(&rollout);
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

    /// A daemon for alex, with bob's machine on the team and a session here. Returns it, bob's key and the session.
    fn with_teammate() -> (Arc<Daemon>, SecretKey, String) {
        with_inbound_policy(Policy::Accept)
    }

    fn with_inbound_policy(policy: Policy) -> (Arc<Daemon>, SecretKey, String) {
        let cfg = crate::config::Config {
            person: Some("alex".into()),
            inbound: crate::config::Inbound { default: policy, ..Default::default() },
            ..Default::default()
        };
        let d = Arc::new(super::super::test_daemon(cfg));
        let bob = SecretKey::generate();
        let mut team = TeamState::found("acme", &d.person, &d.machine, &d.key);
        team.put_member(&d.key, bob.public(), "bob", "xps", true);
        d.inner.lock().unwrap().team = Some(team);
        let to = d.register(Some(1), codex("codex:1"), crate::git::GitInfo::default()).unwrap().address;
        (d, bob, to)
    }

    fn from_bob(to: &str, body: String, files: Vec<Attachment>) -> Message {
        Message {
            id: Ulid::generate().to_string(),
            from: "bob/web".into(),
            from_agent: None,
            from_repo: None,
            to: to.into(),
            via: None,
            reply_to: None,
            body,
            wake: false,
            notify: false,
            sent_at: 0,
            approval: None,
            files,
        }
    }

    #[tokio::test]
    async fn held_mail_reaches_the_replacement_only_after_approval() {
        let (d, bob, address) = with_inbound_policy(Policy::Hold);
        let name = gone(&d, "codex:1", 0);
        assert!(d.inner.lock().unwrap().store.session_by_name(&name).unwrap().unwrap().kept());
        let msg = from_bob(&address, "for review".into(), vec![]);
        let id = msg.id.clone();
        assert_eq!(d.receive(bob.public(), msg).unwrap().0, "held");

        let key = "gemini:new";
        d.register(Some(2), gemini(key), crate::git::GitInfo::default()).unwrap();
        let req = Request::SetStatus { key: key.into(), doing: None, name: Some(name), reclaim: true };
        let reply = d.handle(2, req).await.unwrap();
        assert_eq!(reply["address"], address);
        assert_eq!(reply["unread"], 0);
        assert!(d.inbox(key.into(), 0, false).await.unwrap().messages.is_empty());
        assert_eq!(d.settle(std::slice::from_ref(&id), true).unwrap(), 1);
        let inbox = d.inbox(key.into(), 0, false).await.unwrap();
        assert_eq!(inbox.messages.into_iter().map(|m| m.id).collect::<Vec<_>>(), [id]);
    }

    /// Takes `msg` from bob's machine: its error, if refused, after checking nothing of it was kept.
    fn refused(d: &Arc<Daemon>, bob: &SecretKey, to: &str, msg: Message) -> String {
        let id = msg.id.clone();
        let e = d.receive(bob.public(), msg).expect_err("refused").to_string();
        let g = d.inner.lock().unwrap();
        assert!(!g.store.has(&id).unwrap() && g.store.unread(to).unwrap().is_empty(), "kept: {e}");
        e
    }

    #[test]
    fn a_teammates_message_is_held_to_the_size_limit_it_was_sent_under() {
        let (d, bob, to) = with_teammate();
        // Past the limit (an engine changed to skip its own check): refused, and none of it kept or shown.
        let e = refused(&d, &bob, &to, from_bob(&to, "x".repeat(MAX_BODY + 1), Vec::new()));
        assert_eq!(e, "message is 65 KB; the limit is 64 KB (send a small text file with `files`, or a link)");
        let e = refused(&d, &bob, &to, from_bob(&to, "x".repeat(4 << 20), Vec::new()));
        assert!(e.starts_with("message is 4096 KB; the limit is 64 KB"), "{e}");
        // At the limit, as an honest engine may send it: taken as before.
        let (state, _) = d.receive(bob.public(), from_bob(&to, "x".repeat(MAX_BODY), Vec::new())).unwrap();
        assert_eq!(state, "queued");
        let got = d.inner.lock().unwrap().store.unread(&to).unwrap();
        assert_eq!(got.iter().map(|m| m.body.len()).collect::<Vec<_>>(), [MAX_BODY]);
    }

    #[test]
    fn a_teammates_files_are_held_to_the_limits_they_were_sent_under() {
        use files::{MAX_FILE, MAX_FILES, MAX_TOTAL};
        let (d, bob, to) = with_teammate();
        let file =
            |i: usize, bytes: usize| Attachment { name: format!("{i}.txt"), content: "x".repeat(bytes), path: None };
        let msg = |files: Vec<Attachment>| from_bob(&to, "the logs".into(), files);
        let e = refused(&d, &bob, &to, msg((0..=MAX_FILES).map(|i| file(i, 1)).collect()));
        assert_eq!(e, format!("{} files; a message carries at most {MAX_FILES}", MAX_FILES + 1));
        let e = refused(&d, &bob, &to, msg(vec![file(0, MAX_FILE + 1)]));
        assert!(e.starts_with("0.txt is 257 KB; a file can be at most 256 KB"), "{e}");
        let e = refused(&d, &bob, &to, msg(vec![file(0, MAX_FILE), file(1, MAX_FILE), file(2, 1)]));
        assert!(e.starts_with("the files come to more than 512 KB"), "{e}");
        // The most an honest engine sends in one message: the most text, the most files, as many bytes as files carry.
        let mut most: Vec<Attachment> = (0..MAX_FILES - 2).map(|i| file(i, 0)).collect();
        most.extend([file(8, MAX_FILE), file(9, MAX_TOTAL - MAX_FILE)]);
        let (state, _) = d.receive(bob.public(), from_bob(&to, "x".repeat(MAX_BODY), most)).unwrap();
        assert_eq!(state, "queued");
        let got = d.inner.lock().unwrap().store.unread(&to).unwrap();
        assert_eq!((got.len(), got[0].body.len(), got[0].files.len()), (1, MAX_BODY, MAX_FILES));
    }
}
