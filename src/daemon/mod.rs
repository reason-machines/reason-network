//! The per-machine daemon: knows the live sessions here, talks to teammates' daemons, routes messages, keeps
//! mailboxes. Split by concern: `sessions` (who is running here), `directory` (who is where, and resolving
//! addresses), `mail` (sending, receiving, delivering), `teamops` (membership and the network).

mod antigravity;
mod ask;
mod diagnostics;
mod directory;
mod mail;
mod notify;
mod reason;
mod sessions;
mod teamops;
mod update;

use crate::{
    config::Config,
    git::{self, GitInfo},
    ipc,
    net::Net,
    paths,
    proto::*,
    store::Store,
    team::TeamState,
    util,
};
use anyhow::{Result, anyhow};
use iroh::SecretKey;
use serde_json::{Value, json};
use std::{
    collections::{HashMap, HashSet, VecDeque},
    sync::{Arc, Mutex, RwLock},
    time::{Duration, Instant},
};
use tokio::{
    io::{AsyncBufReadExt, AsyncWriteExt, BufReader},
    sync::{Notify, mpsc},
};

const GIT_TTL: Duration = Duration::from_secs(30);
const MAX_WAIT: u64 = 600;

/// Runs the daemon. Only one runs per state folder (`REASON_NETWORK_HOME`): a second one exits, or with `service` waits its turn.
pub async fn run(service: bool) -> Result<()> {
    // Windows runs what starts at logon in a console window of its own: carry on in a hidden process instead.
    #[cfg(windows)]
    if service && std::io::IsTerminal::is_terminal(&std::io::stderr()) {
        crate::client::start_daemon(true)?;
        return Ok(());
    }
    paths::ensure_home()?;
    let lock = tokio::task::spawn_blocking(move || util::lock(&paths::lock(), service)).await??;
    let Some(_lock) = lock else {
        log("another daemon is already running");
        return Ok(());
    };
    let mut listener = ipc::Listener::bind()?;
    let daemon = Arc::new(Daemon::open()?);
    if daemon.cfg.debug_log {
        daemon.known_to_debug_log(daemon.team_state().as_ref());
        match crate::debuglog::start() {
            Ok(()) => log(&format!("debug log on: {}", crate::debuglog::path().display())),
            Err(e) => log(&format!("debug log not started: {e}")),
        }
    }
    log(&format!("listening on {} as {}@{}", paths::socket().display(), daemon.person, daemon.machine));
    if daemon.team_state().is_some()
        && let Err(e) = daemon.start_net().await
    {
        log(&format!("network not started: {e:#}"));
    }
    // Normally the service's daemon; a daemon started some other way stands in for it while it runs.
    if daemon.cfg.tray() && (service || crate::setup::service_installed()) {
        tokio::spawn(tray());
    }
    if daemon.cfg.reason {
        tokio::spawn(daemon.clone().reason_loop());
    }
    if crate::managed() {
        tokio::spawn(daemon.clone().relay_loop());
    }
    tokio::spawn(daemon.clone().notices_loop());
    tokio::spawn(crate::migrate::run(log));
    tokio::spawn(daemon.clone().relays_loop());
    if update::allowed(daemon.cfg.auto_update) {
        tokio::spawn(daemon.clone().update_loop());
    }
    loop {
        let stream = listener.accept().await?;
        tokio::spawn(daemon.clone().serve(stream));
    }
}

/// Keeps the menu bar icon up while we run. It exits 0 when the user hides it, and on its own when we exit.
async fn tray() {
    let Ok(exe) = util::current_exe() else { return };
    for _ in 0..5 {
        match util::no_window(tokio::process::Command::new(&exe))
            .arg("tray")
            .stdin(std::process::Stdio::null())
            .status()
            .await
        {
            Ok(st) if st.success() => return,
            Ok(st) => log(&format!("menu bar icon exited ({st}); restarting it")),
            Err(e) => return log(&format!("menu bar icon not started: {e}")),
        }
        tokio::time::sleep(Duration::from_secs(5)).await;
    }
}

/// The answer to a request this daemon can't read (from a newer client, say): under its id when that much reads, so
/// the caller gets it rather than waiting.
fn bad_request(line: &str, e: serde_json::Error) -> Reply {
    let id = serde_json::from_str::<Value>(line).ok().and_then(|v| v.get("id")?.as_u64()).unwrap_or(0);
    Reply::err(id, anyhow!("bad request: {e}"))
}

fn log(msg: &str) {
    eprintln!("[{}] {msg}", util::now_ms());
}

pub struct Daemon {
    usage: crate::usage::Usage,
    person: String,
    machine: String,
    cfg: Config,
    key: SecretKey,
    inner: Mutex<Inner>,
    /// Woken on every new message, for `Inbox` long-polls.
    arrived: Notify,
    git_cache: Mutex<HashMap<String, (Instant, GitInfo)>>,
    net: RwLock<Option<Arc<Net>>>,
    /// Held while the network starts, stops or is swapped, and while an invite is made: one at a time, so two networks
    /// never run on this machine's key, and an invite isn't made on one that is going away.
    net_lock: tokio::sync::Mutex<()>,
    /// Recent message times per sender, for the rate limit.
    rate: Mutex<HashMap<String, VecDeque<Instant>>>,
    /// When a notification from each sender was last shown here.
    shown: Mutex<HashMap<String, Instant>>,
    /// Each session's `ask_user` question that is up, with its answer once there is one.
    asks: Mutex<HashMap<String, ask::Asking>>,
    /// When this daemon started (ms): sessions live before it register again in the while after.
    started: i64,
    /// Whether the log says presence names only some of the names kept here.
    capped: std::sync::atomic::AtomicBool,
    /// Until when Reason sessions are listed without `--fields`, which the installed `reason` refused.
    plain_until: Mutex<Option<Instant>>,
}

struct Inner {
    store: Store,
    live: HashMap<String, Live>,
    conns: HashMap<u64, mpsc::UnboundedSender<String>>,
    antigravity_conn: Option<u64>,
    next_conn: u64,
    team: Option<TeamState>,
    /// Messages handed to a push channel or a hook and not yet confirmed; hidden from inbox and hooks meanwhile.
    in_flight: HashMap<String, Instant>,
    /// Sessions waiting in `check_inbox` right now (they get new mail there instead of a push).
    polling: HashMap<String, usize>,
    /// Recent agent-to-agent message times per (from, to), for the loop guard.
    pairs: HashMap<(String, String), VecDeque<Instant>>,
    /// When the inbox folder was last swept of files no longer kept (ms).
    swept: i64,
    /// Teammates' files taken in the last hour, for the quota.
    quota: crate::files::Quota,
    /// Sessions whose hooks say they are in a folder that is off: whether each is mid-turn, and when it last said.
    off: HashMap<String, (bool, i64)>,
    /// Codex sessions whose wake mail waits for a hook of the turn they are in, watched for that turn ending first.
    codex_watch: HashSet<String>,
    /// Some mail kept from before notices doesn't say which machine it came from yet (see `find_sources`).
    sourceless: bool,
}

/// A session running on this machine.
struct Live {
    through_reason: Option<bool>,
    reg: SessionReg,
    name: String,
    /// The client connection (`reason network mcp`) that owns it; `None` for sessions known only through hooks.
    conn: Option<u64>,
    git: GitInfo,
    doing: Option<String>,
    since: i64,
    /// From hooks: `busy`/`idle` and when it changed.
    status: Option<(String, i64)>,
    last_hook: Option<i64>,
    /// Its push channel failed (e.g. `codex queue`); stick to hooks from now on.
    push_failed: bool,
    /// The agent's transcript (from its hooks), which says what the session is doing.
    transcript: Option<std::path::PathBuf>,
    /// A push that failed is retried from this time on, after waiting this long (doubling up to 30 min).
    retry: Option<(Instant, Duration)>,
    /// Bumped to end this session's `check_inbox` wait (its client gave up on it).
    stop_gen: u64,
    /// Its MCP connection pushes into it (it registered as `push`, knowing the agent's inbox). Claude Code's only.
    mcp_push: bool,
    /// One of the person's own mailboxes: `terminal`, or a name they gave with `listen --as`. Kept for good.
    mailbox: bool,
    /// It could be reached beyond its own hooks: an MCP server connected for it or attached a call to it, a push channel
    /// delivers into it (Claude Code's inbox, `codex queue`, the opencode and pi plugins, Reason), or it is one of the
    /// person's own mailboxes. Only such a session keeps its name while offline: a run only its hooks ever saw (a
    /// one-shot or headless run without Reason Network's tools) was never reachable, and won't be.
    attached: bool,
}

pub fn load_key() -> Result<SecretKey> {
    let path = paths::home().join("machine.key");
    if let Ok(bytes) = std::fs::read(&path) {
        let arr: [u8; 32] = bytes.as_slice().try_into().map_err(|_| anyhow!("{} is corrupt", path.display()))?;
        return Ok(SecretKey::from_bytes(&arr));
    }
    let key = SecretKey::generate();
    std::fs::write(&path, key.to_bytes())?;
    #[cfg(unix)]
    util::chmod(&path, 0o600)?;
    Ok(key)
}

impl Daemon {
    pub(crate) fn usage_transport(&self, metric: &'static str, relay: bool, rtt: u64) {
        let mut dims = crate::usage::Dimensions { locality: "remote".into(), ..Default::default() };
        if metric == "path_changed" {
            dims.transport = if relay { "relay" } else { "direct" }.into();
        }
        self.usage.count(metric, dims.clone(), 1);
        if rtt > 0 {
            dims.bucket = crate::usage::duration(rtt).into();
            self.usage.count("rtt_ms", dims, 1);
        }
    }
    fn open() -> Result<Self> {
        let mut daemon = Self::with(Config::load(), Store::open(&paths::db(), paths::inbox())?, load_key()?)?;
        daemon.usage = crate::usage::Usage::open(paths::home().join("usage.sqlite"));
        Ok(daemon)
    }

    fn with(cfg: Config, store: Store, key: SecretKey) -> Result<Self> {
        let team: Option<TeamState> =
            store.get("team")?.and_then(|s| serde_json::from_str(&s).ok()).map(TeamState::without_odd_names);
        // In a team, who we are is what the signed roster says, whatever the config file says.
        let member = team.as_ref().and_then(|t| t.member(&key.public()).cloned());
        let daemon = Daemon {
            usage: Default::default(),
            person: member.as_ref().map_or_else(|| cfg.person(), |m| m.person.clone()),
            machine: member.map_or_else(util::hostname, |m| m.machine),
            key,
            cfg,
            inner: Mutex::new(Inner {
                store,
                live: HashMap::new(),
                conns: HashMap::new(),
                antigravity_conn: None,
                next_conn: 0,
                team,
                in_flight: HashMap::new(),
                polling: HashMap::new(),
                pairs: HashMap::new(),
                swept: 0,
                quota: Default::default(),
                off: HashMap::new(),
                codex_watch: HashSet::new(),
                sourceless: false,
            }),
            arrived: Notify::new(),
            git_cache: Mutex::new(HashMap::new()),
            net: RwLock::new(None),
            net_lock: tokio::sync::Mutex::new(()),
            rate: Mutex::new(HashMap::new()),
            shown: Mutex::new(HashMap::new()),
            asks: Mutex::new(HashMap::new()),
            started: util::now_ms(),
            plain_until: Mutex::new(None),
            capped: Default::default(),
        };
        if let Err(e) = daemon.find_sources(&mut daemon.inner.lock().unwrap()) {
            log(&format!("finding where mail from before notices came from: {e:#}"));
        }
        Ok(daemon)
    }

    fn addr(&self, name: &str) -> String {
        format!("{}/{name}", self.person)
    }

    /// A session, as the log names it: by its address. Its key carries its agent's session id, which stays out of it.
    fn logged(&self, g: &Inner, key: &str) -> String {
        g.live.get(key).map_or_else(|| "a session that has gone".into(), |l| self.addr(&l.name))
    }

    fn net(&self) -> Option<Arc<Net>> {
        self.net.read().unwrap().clone()
    }

    /// A session here came or went: teammates hear about it right away.
    fn presence_changed(&self) {
        if let Some(net) = self.net() {
            net.presence_changed();
        }
    }

    async fn serve(self: Arc<Self>, stream: ipc::Accepted) {
        let (rd, mut wr) = tokio::io::split(stream);
        let (tx, mut rx) = mpsc::unbounded_channel::<String>();
        let conn = {
            let mut g = self.inner.lock().unwrap();
            g.next_conn += 1;
            let id = g.next_conn;
            g.conns.insert(id, tx.clone());
            id
        };
        let writer = tokio::spawn(async move {
            while let Some(mut line) = rx.recv().await {
                line.push('\n');
                if wr.write_all(line.as_bytes()).await.is_err() {
                    break;
                }
            }
        });
        let mut lines = BufReader::new(rd).lines();
        // Inbox long-polls end with their connection; anything else finishes (a half-done `leave` would not).
        let mut waits = tokio::task::JoinSet::new();
        while let Ok(Some(line)) = lines.next_line().await {
            while waits.try_join_next().is_some() {}
            let f = match serde_json::from_str::<Frame<Request>>(&line) {
                Ok(f) => f,
                Err(e) => {
                    let _ = tx.send(serde_json::to_string(&bad_request(&line, e)).expect("reply serializes"));
                    continue;
                }
            };
            let wait = matches!(f.body, Request::Inbox { .. } | Request::Ask { .. });
            let (this, tx) = (self.clone(), tx.clone());
            // Each request runs on its own task: an inbox long-poll must not block the connection.
            let task = async move {
                let reply = match this.handle(conn, f.body).await {
                    Ok(v) => Reply::ok(f.id, v),
                    Err(e) => Reply::err(f.id, e),
                };
                let _ = tx.send(serde_json::to_string(&reply).expect("reply serializes"));
            };
            if wait {
                waits.spawn(task);
            } else {
                tokio::spawn(task);
            }
        }
        waits.abort_all();
        self.drop_conn(conn);
        writer.abort();
    }

    async fn handle(self: &Arc<Self>, conn: u64, req: Request) -> Result<Value> {
        let operation = match &req {
            Request::Who { .. } => Some("who"),
            Request::Send { .. } => Some("send"),
            Request::Inbox { .. } => Some("inbox"),
            Request::Ask { .. } => Some("ask"),
            _ => None,
        };
        if let Some(operation) = operation {
            self.usage.count("tool", crate::usage::Dimensions { bucket: operation.into(), ..Default::default() }, 1);
        }
        Ok(match req {
            Request::Register { session } => {
                let git = self.git(&session.cwd).await;
                json!(self.register(Some(conn), session, git)?)
            }
            Request::SetStatus { key, doing, name } => json!(self.set_status(&key, doing, name)?),
            Request::Who { me, filter, all } => json!(self.who(me, filter, all).await?),
            Request::Send { from, to, body, reply_to, wake, notify, files, purpose, idempotency_key } => {
                let dims = crate::usage::Dimensions {
                    bucket: purpose.unwrap_or_else(|| "unknown".into()),
                    ..Default::default()
                };
                self.usage.count("purpose", dims, 1);
                json!(
                    self.send_idempotent(from, &to, &body, reply_to, wake, notify, None, files, idempotency_key)
                        .await?
                )
            }
            Request::Inbox { key, wait_secs, peek } => json!(self.inbox(key, wait_secs.min(MAX_WAIT), peek).await?),
            Request::Unread { key } => json!(self.unread(&key)?),
            Request::Ask { key, question, about } => json!(self.ask(key, question, about).await?),
            Request::StopWaiting { key } => {
                self.stop_waiting(&key).await;
                Value::Null
            }
            Request::Delivered { key, ids } => {
                let mut g = self.inner.lock().unwrap();
                let changed = g.store.mark_read(&ids, util::now_ms())?;
                // Hosted relays acknowledge a handover to a server inbox, not agent consumption.
                let handed_to_agent = g.live.get(&key).is_some_and(|l| l.through_reason == Some(false));
                for id in &changed {
                    if handed_to_agent {
                        self.usage.stage(id, "delivered");
                    }
                }
                for id in &ids {
                    g.in_flight.remove(id);
                }
                if handed_to_agent {
                    g.store.handoff(&changed, util::now_ms())?;
                }
                if let Some(l) = g.live.get_mut(&key) {
                    l.retry = None;
                }
                Value::Null
            }
            Request::PushFailed { key, ids } => {
                self.push_failed(&key, &ids);
                Value::Null
            }
            Request::Hook(h) => json!(self.hook(h).await?),
            Request::Attach { agent, cwd } => json!(self.attach(&agent, &cwd)?),
            Request::Linked { agent, pid } => json!(self.linked(&agent, pid)),
            Request::CreateTeam { name, person, relay, proof } => {
                json!(self.create_team(&name, &person, relay, proof).await?)
            }
            Request::Join { ticket, person, proof } => json!(self.join_team(&ticket, &person, proof).await?),
            Request::Proof { jws } => json!(self.attach_proof(&jws)?),
            Request::Invite { me, hours } => json!(self.invite(me, hours).await?),
            Request::Team => json!(self.team_info()?),
            Request::Usage => {
                let mut report = json!(self.usage.snapshot());
                if report.is_object() {
                    report["health"] = self.diagnostics()?;
                }
                report
            }
            Request::Diagnostics => self.diagnostics()?,
            Request::DeliveryStatus { from, ids } => {
                json!(self.inner.lock().unwrap().store.delivery_status(&from, &ids)?)
            }
            Request::AntigravityAttach => json!(self.attach_antigravity(conn)?),
            Request::AntigravitySession { conversation_id } => {
                let mut g = self.inner.lock().unwrap();
                self.prune(&mut g);
                let key = format!("antigravity:{conversation_id}");
                json!(g.live.get(&key).filter(|l| l.last_hook.is_some()).map(|_| key))
            }
            Request::Link { new } => json!(self.link(new).await?),
            Request::Remove { who } => json!(self.remove(&who)?),
            Request::Leave { force } => json!(self.leave(force).await?),
            Request::SetRelay { url } => json!(self.set_relay(url).await?),
            Request::Held => json!(self.inner.lock().unwrap().store.held()?),
            Request::Settle { ids, release } => json!(self.settle(&ids, release)?),
            Request::Shutdown => {
                self.restart_soon();
                Value::Null
            }
        })
    }

    /// Keeps Reason's relay list fresh, unless the team runs its own relay. A new list applies at the next start.
    async fn relays_loop(self: Arc<Self>) {
        loop {
            if self.team_state().is_none_or(|t| t.meta.body.relay.is_none()) {
                crate::relays::refresh().await;
            }
            tokio::time::sleep(crate::relays::EVERY).await;
        }
    }

    /// Exits shortly (after the reply goes out). The service manager, or the next client, starts a fresh
    /// daemon, which picks up changed config and team state.
    fn restart_soon(self: &Arc<Self>) {
        let this = self.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(150)).await;
            if let Some(net) = this.net() {
                net.shutdown().await;
            }
            #[cfg(unix)]
            let _ = std::fs::remove_file(paths::socket());
            // Windows has no service manager to bring us back: start the one that takes over once we're gone.
            #[cfg(windows)]
            if crate::setup::service_installed() {
                let _ = crate::client::start_daemon(true);
            }
            crate::debuglog::flush();
            std::process::exit(0);
        });
    }

    async fn git(&self, cwd: &str) -> GitInfo {
        if let Some((at, info)) = self.git_cache.lock().unwrap().get(cwd)
            && at.elapsed() < GIT_TTL
        {
            return info.clone();
        }
        let dir = cwd.to_string();
        let info = tokio::task::spawn_blocking(move || git::lookup(&dir)).await.unwrap_or_default();
        self.git_cache.lock().unwrap().insert(cwd.to_string(), (Instant::now(), info.clone()));
        info
    }
}

/// A daemon on an in-memory store, outside any team, with `cfg`.
#[cfg(test)]
pub(crate) fn test_daemon(cfg: Config) -> Daemon {
    let inbox = std::env::temp_dir().join(format!("reason-network-inbox-{}", ulid::Ulid::generate()));
    let store = Store::open(std::path::Path::new(":memory:"), inbox).unwrap();
    Daemon::with(cfg, store, SecretKey::generate()).unwrap()
}

#[cfg(test)]
impl Daemon {
    /// Its store and its person, for tests outside the daemon.
    pub(crate) fn with_store<R>(&self, f: impl FnOnce(&Store, &str) -> R) -> R {
        f(&self.inner.lock().unwrap().store, &self.person)
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn a_request_this_daemon_cant_read_is_answered_under_its_id() {
        let line = r#"{"id":42,"op":"something_newer","key":"x"}"#;
        let e = serde_json::from_str::<super::Frame<super::Request>>(line).unwrap_err();
        let r = super::bad_request(line, e);
        assert_eq!((r.id, r.ok), (42, false));
        assert!(r.error.unwrap().starts_with("bad request:"));
        let e = serde_json::from_str::<super::Frame<super::Request>>("not json").unwrap_err();
        assert_eq!(super::bad_request("not json", e).id, 0);
    }
}
