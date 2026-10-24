//! Connection to the local daemon, starting it if needed. Long-lived clients (`reason-network mcp`) reconnect on their
//! own and re-register their sessions, so a daemon restart is invisible to the agent.

use crate::{ipc, paths, proto::*};
use anyhow::{Result, anyhow, bail};
use serde::de::DeserializeOwned;
use serde_json::Value;
use std::{
    collections::HashMap,
    process::{Command, Stdio},
    sync::{
        Arc, Mutex, Weak,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
    time::Duration,
};
use tokio::{
    io::{AsyncBufReadExt, AsyncWriteExt, BufReader},
    sync::{mpsc, oneshot},
};

type Pending = Arc<Mutex<HashMap<u64, oneshot::Sender<Reply>>>>;

#[derive(Clone)]
struct Conn {
    tx: mpsc::UnboundedSender<String>,
    pending: Pending,
    alive: Arc<AtomicBool>,
    /// The last failure the daemon answered with no request's id: an older daemon can't read a request it doesn't
    /// know, and says so this way. Told to a call that then gets no answer.
    unmatched: Arc<Mutex<Option<String>>>,
}

/// How long a call waits for the daemon's answer, at most: an older daemon never answers a request it doesn't know
/// (it fails it under no id), and the caller mustn't wait for good. Calls that wait on purpose get that long, and more.
fn patience(req: &Request) -> Duration {
    match req {
        // Up to 10 minutes each, for a message or the person's answer.
        Request::Inbox { .. } | Request::Ask { .. } => Duration::from_secs(660),
        // Over the network.
        Request::CreateTeam { .. }
        | Request::Join { .. }
        | Request::Invite { .. }
        | Request::Link { .. }
        | Request::Remove { .. }
        | Request::Leave { .. }
        | Request::SetRelay { .. }
        | Request::Proof { .. } => Duration::from_secs(600),
        _ => Duration::from_secs(120),
    }
}

pub struct Link {
    conn: tokio::sync::Mutex<Option<Conn>>,
    regs: Mutex<HashMap<String, SessionReg>>,
    /// Where pushed events go. `None` for one-shot clients, which also don't reconnect.
    events: Option<mpsc::UnboundedSender<Event>>,
    next_id: AtomicU64,
    /// Start the daemon when it isn't running.
    start: bool,
}

impl Link {
    pub fn new(events: Option<mpsc::UnboundedSender<Event>>) -> Arc<Self> {
        Self::with(events, true)
    }

    /// A one-shot client that never starts the daemon: it fails while none is running.
    pub fn existing() -> Arc<Self> {
        Self::with(None, false)
    }

    fn with(events: Option<mpsc::UnboundedSender<Event>>, start: bool) -> Arc<Self> {
        Arc::new(Link { conn: Default::default(), regs: Default::default(), events, next_id: AtomicU64::new(1), start })
    }

    pub async fn call<T: DeserializeOwned>(self: &Arc<Self>, req: Request) -> Result<T> {
        let conn = self.connect().await?;
        let v = self.raw(&conn, &req).await?;
        Ok(serde_json::from_value(v)?)
    }

    pub async fn register(self: &Arc<Self>, reg: SessionReg) -> Result<Registered> {
        let r = self.call(Request::Register { session: reg.clone() }).await?;
        remember(&mut self.regs.lock().unwrap(), reg);
        Ok(r)
    }

    /// How a registered session was said to be reached.
    pub fn registered_tier(&self, key: &str) -> Option<Tier> {
        self.regs.lock().unwrap().get(key).map(|r| r.tier)
    }

    async fn raw(&self, conn: &Conn, req: &Request) -> Result<Value> {
        self.raw_within(conn, req, patience(req)).await
    }

    async fn raw_within(&self, conn: &Conn, req: &Request, patience: Duration) -> Result<Value> {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let (tx, rx) = oneshot::channel();
        conn.pending.lock().unwrap().insert(id, tx);
        let line = serde_json::to_string(&Frame { id, body: req })?;
        conn.tx.send(line).map_err(|_| anyhow!("network daemon connection closed"))?;
        let reply = match tokio::time::timeout(patience, rx).await {
            Ok(reply) => reply.map_err(|_| anyhow!("network daemon connection closed"))?,
            Err(_) => {
                conn.pending.lock().unwrap().remove(&id);
                let said =
                    conn.unmatched.lock().unwrap().take().map(|e| format!(" (it said: {e})")).unwrap_or_default();
                bail!("no answer from the network daemon in {}s{said}; is it an older version?", patience.as_secs())
            }
        };
        if reply.ok {
            Ok(reply.result.unwrap_or(Value::Null))
        } else {
            bail!(reply.error.unwrap_or_else(|| "unknown error".into()))
        }
    }

    async fn connect(self: &Arc<Self>) -> Result<Conn> {
        let mut slot = self.conn.lock().await;
        if let Some(c) = slot.as_ref()
            && c.alive.load(Ordering::Relaxed)
        {
            return Ok(c.clone());
        }
        let c = self.spawn_io(connect_or_start(self.start).await?);
        let regs: Vec<SessionReg> = self.regs.lock().unwrap().values().cloned().collect();
        for reg in regs {
            if let Err(e) = self.raw(&c, &Request::Register { session: reg.clone() }).await {
                // Its folder was turned off meanwhile: it stays off, and the rest reconnect.
                if e.to_string() != crate::config::OFF_HERE {
                    return Err(e);
                }
                self.regs.lock().unwrap().remove(&reg.key);
            }
        }
        *slot = Some(c.clone());
        Ok(c)
    }

    fn spawn_io(self: &Arc<Self>, stream: ipc::Stream) -> Conn {
        let (rd, mut wr) = tokio::io::split(stream);
        let (tx, mut rx) = mpsc::unbounded_channel::<String>();
        let conn = Conn {
            tx,
            pending: Pending::default(),
            alive: Arc::new(AtomicBool::new(true)),
            unmatched: Default::default(),
        };
        tokio::spawn(async move {
            while let Some(mut line) = rx.recv().await {
                line.push('\n');
                if wr.write_all(line.as_bytes()).await.is_err() {
                    break;
                }
            }
        });
        let (reader, alive, events, link) =
            (conn.clone(), conn.alive.clone(), self.events.clone(), Arc::downgrade(self));
        let pending = conn.pending.clone();
        tokio::spawn(async move {
            let mut lines = BufReader::new(rd).lines();
            while let Ok(Some(line)) = lines.next_line().await {
                reader.dispatch(&line, events.as_ref());
            }
            alive.store(false, Ordering::Relaxed);
            pending.lock().unwrap().clear();
            if events.is_some() {
                tokio::spawn(reconnect(link));
            }
        });
        conn
    }
}

impl Conn {
    /// A line from the daemon: an event, or the reply to a call, which it hands over.
    fn dispatch(&self, line: &str, events: Option<&mpsc::UnboundedSender<Event>>) {
        let Ok(v) = serde_json::from_str::<Value>(line) else { return };
        if v.get("event").is_some() {
            if let (Some(events), Ok(ev)) = (events, serde_json::from_value::<Event>(v)) {
                let _ = events.send(ev);
            }
            return;
        }
        let Ok(reply) = serde_json::from_value::<Reply>(v) else { return };
        match self.pending.lock().unwrap().remove(&reply.id) {
            Some(tx) => {
                let _ = tx.send(reply);
            }
            None if !reply.ok => *self.unmatched.lock().unwrap() = reply.error,
            None => {}
        }
    }
}

async fn reconnect(link: Weak<Link>) {
    // Backs off: every agent session runs one of these, and they must not storm a daemon that can't start.
    let mut wait = Duration::from_secs(1);
    loop {
        tokio::time::sleep(wait).await;
        let Some(link) = link.upgrade() else { return };
        match link.connect().await {
            Ok(_) => return,
            Err(e) => eprintln!("reason-network: reconnecting to daemon: {e:#}"),
        }
        wait = (wait * 2).min(Duration::from_secs(30));
    }
}

/// The daemon that holds the lock, if it is alive (it may still be starting).
pub fn running_daemon() -> Option<u32> {
    holder(&paths::lock())
}

/// Who holds the lock at `path`. Asked of the lock, not the pid: a daemon that exited holds none, even when a
/// parent that never reaps (a container's `sleep` as PID 1) leaves it a zombie whose pid still "exists".
fn holder(path: &std::path::Path) -> Option<u32> {
    if !crate::util::locked(path) {
        return None;
    }
    std::fs::read_to_string(path).ok()?.trim().parse().ok()
}

async fn connect_or_start(start: bool) -> Result<ipc::Stream> {
    if let Ok(s) = ipc::connect().await {
        return Ok(s);
    }
    if !start {
        bail!("the network daemon isn't running");
    }
    // One is running (it may be starting or restarting): wait for it rather than start another.
    let mut child = None;
    if running_daemon().is_none() {
        // With the background service installed, the service starts it, so there is only ever its daemon.
        let by_service = crate::setup::service_installed()
            && crate::paths::env_var_os("NO_SERVICE").is_none()
            && crate::setup::start_service();
        if !by_service {
            child = Some(start_daemon(false)?);
        }
    }
    let failed = || anyhow!("could not start the network daemon; see {}", paths::log().display());
    let mut connected = None;
    for _ in 0..100 {
        tokio::time::sleep(Duration::from_millis(50)).await;
        if let Ok(s) = ipc::connect().await {
            connected = Some(s);
            break;
        }
        // It exited: either another daemon got there first (keep waiting for it) or it can't start (give up now).
        if let Some(c) = child.as_mut()
            && c.try_wait().ok().flatten().is_some()
        {
            child = None;
            if running_daemon().is_none() {
                return Err(failed());
            }
        }
    }
    // Reaped when it exits, so a long-lived client (`reason-network mcp`) never collects zombies.
    if let Some(mut c) = child {
        std::thread::spawn(move || c.wait());
    }
    connected.ok_or_else(failed)
}

/// Starts `reason-network daemon` (`--service` with `service`) detached from this process group, logging to
/// `~/.reason/network/daemon.log`.
pub fn start_daemon(service: bool) -> Result<std::process::Child> {
    paths::ensure_home()?;
    let log = std::fs::OpenOptions::new().create(true).append(true).open(paths::log())?;
    let mut cmd = Command::new(crate::util::current_exe()?);
    cmd.arg("daemon").args(service.then_some("--service")).stdin(Stdio::null()).stdout(log.try_clone()?).stderr(log);
    #[cfg(unix)]
    let child = std::os::unix::process::CommandExt::process_group(&mut cmd, 0).spawn()?;
    #[cfg(windows)]
    let child = detach(&mut cmd)?;
    Ok(child)
}

/// On Windows: no window, out of reach of this console's Ctrl-C, and without our own standard handles, which
/// Windows would hand it otherwise (an agent reading a hook's output to the end would wait for the daemon too).
/// Nor any other handle we inherited: Windows PowerShell capturing our output hands us its own standard handles
/// as well. Also out of the starter's job when the job allows it: an SSH session's job ends everything at logout.
#[cfg(windows)]
fn detach(cmd: &mut Command) -> std::io::Result<std::process::Child> {
    use std::os::windows::{io::AsRawHandle, process::CommandExt};
    use windows_sys::Win32::{Foundation::*, System::Threading::*};
    let ours = [std::io::stdin().as_raw_handle(), std::io::stdout().as_raw_handle(), std::io::stderr().as_raw_handle()];
    // Handles are multiples of 4; what we inherited sits where it sat in our parent. (`cmd`'s own are made
    // inheritable again when it spawns.)
    for h in ours.into_iter().chain((4..1usize << 16).step_by(4).map(|h| h as HANDLE)) {
        // SAFETY: only clears inherit flags; a value that isn't an open handle just fails.
        unsafe { SetHandleInformation(h, HANDLE_FLAG_INHERIT, 0) };
    }
    let flags = CREATE_NO_WINDOW | CREATE_NEW_PROCESS_GROUP;
    cmd.creation_flags(flags | CREATE_BREAKAWAY_FROM_JOB).spawn().or_else(|_| cmd.creation_flags(flags).spawn())
}

/// Remembers a registration, to make again after a reconnect. An agent with an MCP server per session is in one
/// conversation at a time: a new one (Claude Code's `/clear`) replaces the one before, which must not come back.
fn remember(regs: &mut HashMap<String, SessionReg>, reg: SessionReg) {
    if !crate::agent::multiplexed(&reg.agent) {
        regs.retain(|_, r| r.agent != reg.agent);
    }
    regs.insert(reg.key.clone(), reg);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn a_request_an_older_daemon_cant_read_fails_instead_of_waiting_for_good() {
        let link = Link::existing();
        let (tx, mut sent) = mpsc::unbounded_channel();
        let conn = Conn {
            tx,
            pending: Pending::default(),
            alive: Arc::new(AtomicBool::new(true)),
            unmatched: Default::default(),
        };
        let req = Request::PushFailed { key: "claude-code:a".into(), ids: vec!["01M".into()] };
        let call = {
            let (link, conn) = (link.clone(), conn.clone());
            tokio::spawn(async move { link.raw_within(&conn, &req, Duration::from_millis(300)).await })
        };
        // What 0.12.6 answers: a failure under no request's id.
        let line = sent.recv().await.unwrap();
        assert!(line.contains("push_failed"), "{line}");
        conn.dispatch(r#"{"id":0,"ok":false,"error":"bad request: unknown variant `push_failed`"}"#, None);
        let e = call.await.unwrap().unwrap_err().to_string();
        assert!(
            e.starts_with("no answer from the network daemon") && e.contains("unknown variant `push_failed`"),
            "{e}"
        );
        assert!(conn.pending.lock().unwrap().is_empty());
        // A reply to its own id still comes through.
        let call = {
            let (link, conn) = (link.clone(), conn.clone());
            tokio::spawn(async move { link.raw_within(&conn, &Request::Team, Duration::from_secs(5)).await })
        };
        let line = sent.recv().await.unwrap();
        let id = serde_json::from_str::<Value>(&line).unwrap()["id"].as_u64().unwrap();
        conn.dispatch(&serde_json::to_string(&Reply::ok(id, serde_json::json!(7))).unwrap(), None);
        assert_eq!(call.await.unwrap().unwrap(), serde_json::json!(7));
    }

    #[test]
    fn a_reconnect_registers_only_the_conversation_a_server_is_in_now() {
        let reg = |key: &str, agent: &str| crate::proto::SessionReg {
            key: key.into(),
            agent: agent.into(),
            cwd: "/".into(),
            agent_session_id: None,
            pid: None,
            tier: crate::proto::Tier::Pull,
            name: None,
            route: None,
        };
        let mut regs = super::HashMap::new();
        super::remember(&mut regs, reg("claude-code:before-clear", "claude-code"));
        super::remember(&mut regs, reg("claude-code:after-clear", "claude-code"));
        // A shared server (Codex's) serves several at once.
        super::remember(&mut regs, reg("codex:1", "codex"));
        super::remember(&mut regs, reg("codex:2", "codex"));
        let mut keys: Vec<&String> = regs.keys().collect();
        keys.sort();
        assert_eq!(keys, ["claude-code:after-clear", "codex:1", "codex:2"]);
    }

    #[test]
    fn a_daemon_runs_only_while_it_holds_the_lock() {
        let dir = std::env::temp_dir().join(format!("reason-network-lock-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("daemon.lock");
        // A pid left behind by a daemon that exited (or a zombie): the lock is free.
        std::fs::write(&path, std::process::id().to_string()).unwrap();
        assert_eq!(super::holder(&path), None);
        let held = crate::util::lock(&path, false).unwrap().expect("free lock");
        assert_eq!(super::holder(&path), Some(std::process::id()));
        drop(held);
        assert_eq!(super::holder(&path), None);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
