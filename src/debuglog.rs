//! The debug log, for working out why connections drop or switch to a relay. Off unless `debug_log = true` in
//! config.toml. The daemon then writes the networking library's own events (iroh and its QUIC, at debug level) and its
//! own network lines to `debug.log` next to `daemon.log`, on this machine only. What could carry message text, files,
//! tickets, links, codes, keys or other people's machines stays out of it: only networking targets are kept, fields
//! named like secrets are dropped, and every line is scrubbed before it is written. Only ids of this machine and its
//! team's show (cut to 10 characters); any other id is `[id]`.

use crate::{paths, util};
use std::{
    cell::Cell,
    fmt,
    fs::{File, OpenOptions},
    io::{BufWriter, Write},
    path::{Path, PathBuf},
    sync::{
        Mutex, OnceLock,
        atomic::{AtomicBool, AtomicU64, Ordering},
        mpsc::{Receiver, RecvTimeoutError, SyncSender, TrySendError, sync_channel},
    },
    time::{Duration, Instant},
};
use tracing_core::{
    Event, Interest, Level, Metadata, Subscriber,
    dispatcher::{self, Dispatch},
    field::{Field, Visit},
    metadata::LevelFilter,
    span,
};

/// Past this the file starts over and the full one is kept as `debug.log.1`: at most twice this on disk.
const MAX_BYTES: u64 = 4 << 20;

/// Lines in a burst, then this many a second: a flood (of packets from a peer, say) can't fill the file and push out
/// what came before it, nor cost a scrub per packet. Starting up and connecting is a few hundred.
const BURST: u64 = 2000;
const PER_SECOND: u64 = 50;

/// How often the writer writes out what it has, and how many lines were left out.
const FLUSH_EVERY: Duration = Duration::from_secs(2);

/// A line is cut to this many characters before it is scrubbed (a relay can send a problem report of a megabyte), and
/// each field's value to this many before that.
const MAX_LINE: usize = 4000;
const MAX_VALUE: usize = 2000;

/// Lines waiting for the writer; past this they are left out (and counted), never waited for.
const QUEUE: usize = 4096;

/// Where the networking library's events come from: iroh's crates, its QUIC (noq), and the network watchers.
const TARGETS: &[&str] = &["iroh", "noq", "netwatch", "portmapper"];

/// Left out although under `TARGETS`: mDNS discovery logs every machine on the local network and what it announces.
const NOT_TARGETS: &[&str] = &["iroh_mdns_address_lookup"];

/// Fields whose names say they may hold a secret or someone's content are left out whole.
const SECRET_FIELDS: &[&str] = &[
    "secret",
    "private",
    "token",
    "password",
    "username",
    "ticket",
    "auth",
    "cookie",
    "credential",
    "payload",
    "body",
    "content",
];

/// What a hidden id or secret becomes, and how much of it is enough to recognize it.
const HIDDEN: &str = "[invite]";
const HIDDEN_PREFIX: usize = 8;

/// iroh's z-base-32 alphabet (for keys in DNS names), and that length for a key.
const Z32: &str = "ybndrfg8ejkmcpqxot1uwisza345h769";
const Z32_KEY: usize = 52;

enum Msg {
    Line(String, bool),
    Flush(SyncSender<()>),
}

static LOG: OnceLock<SyncSender<Msg>> = OnceLock::new();

/// Lines left out because the writer was behind, or over the rate limit.
static BEHIND: AtomicU64 = AtomicU64::new(0);
static OVER: AtomicU64 = AtomicU64::new(0);

/// The rate limit, kept without a lock: when the next line is due, in microseconds since `START` (a GCRA bucket).
static DUE: AtomicU64 = AtomicU64::new(0);
static START: OnceLock<Instant> = OnceLock::new();

/// Whether the writer thread is still there to wait for.
static WRITER: AtomicBool = AtomicBool::new(false);

/// Ids and secrets that stand for something guessable (an invite code, its key and secret), in every form a line may
/// show them in: even a short prefix of one would let someone holding the log test codes offline.
static HIDE: Mutex<Vec<String>> = Mutex::new(Vec::new());

/// The machines whose ids may show (this one and its team's): hex and z-base-32.
static KNOWN: Mutex<Vec<(String, String)>> = Mutex::new(Vec::new());

thread_local! {
    /// Set while a line is scrubbed: a panic there drops the line, and its message (which would quote it) is not shown.
    static SCRUBBING: Cell<bool> = const { Cell::new(false) };
}

pub fn path() -> PathBuf {
    paths::home().join("debug.log")
}

/// Turns the log on for this process (the daemon, when its config says so). Says why not if it can't be.
pub fn start() -> Result<(), String> {
    let writer = Writer::open(path()).map_err(|e| format!("can't write {}: {e}", path().display()))?;
    let (tx, rx) = sync_channel(QUEUE);
    START.get_or_init(Instant::now);
    WRITER.store(true, Ordering::Relaxed);
    std::thread::Builder::new().name(WRITER_NAME.into()).spawn(move || writer.run(rx)).map_err(|e| e.to_string())?;
    dispatcher::set_global_default(Dispatch::new(Networking)).map_err(|e| e.to_string())?;
    LOG.set(tx).map_err(|_| "already on".to_string())?;
    let shown = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        if SCRUBBING.with(Cell::get) {
            return;
        }
        flush();
        shown(info);
    }));
    Ok(())
}

const WRITER_NAME: &str = "debug-log";

/// Writes out what is waiting (before the process exits, say), waiting at most a second; not at all if the writer is
/// gone, or this is the writer (panicking).
pub fn flush() {
    if !WRITER.load(Ordering::Relaxed) || std::thread::current().name() == Some(WRITER_NAME) {
        return;
    }
    if let Some(tx) = LOG.get() {
        let (ack, done) = sync_channel(1);
        if tx.try_send(Msg::Flush(ack)).is_ok() {
            let _ = done.recv_timeout(Duration::from_secs(1));
        }
    }
}

/// The machines whose ids the log may show: this one and its team's.
pub fn know(ids: &[iroh::EndpointId]) {
    let ids = ids.iter().map(|id| (id.to_string(), id.to_z32())).collect();
    *KNOWN.lock().unwrap_or_else(|e| e.into_inner()) = ids;
}

/// Keeps `id` (an invite code's key) out of the log: its hex and z-base-32 forms, and any prefix of them. Only while
/// the log is on (the daemon turns it on before it binds or dials anything); a process without it keeps nothing.
pub fn hide(id: &iroh::EndpointId) {
    if !collecting() {
        return;
    }
    hide_text(&id.to_string());
    hide_text(&id.to_z32());
}

/// Keeps `text` (a code, or a secret that stands for one) and any prefix of it out of the log, while it is on.
pub fn hide_text(text: &str) {
    if !collecting() {
        return;
    }
    let text = text.to_ascii_lowercase();
    let mut hide = HIDE.lock().unwrap_or_else(|e| e.into_inner());
    if text.len() >= HIDDEN_PREFIX && text.is_ascii() && !hide.contains(&text) {
        hide.push(text);
    }
}

/// Whether invite codes are collected to hide: while the log is on (and in tests).
fn collecting() -> bool {
    LOG.get().is_some() || cfg!(test)
}

/// One of the daemon's own network lines (`what` names the part), when the log is on. Quoted text in it is dropped
/// too: here that can only be an error's echo of what a peer sent.
pub fn ours(what: &str, msg: &str) {
    if LOG.get().is_some() && admit() {
        let msg = cap(msg, MAX_LINE);
        let msg = unquote(&unquote(&unquote(msg, '"', 0), '`', 0), '\'', 0);
        write(&format!("{what}: {msg}"), false);
    }
}

/// Whether the rate limit lets one more line through now; counted if not.
fn admit() -> bool {
    let now = START.get_or_init(Instant::now).elapsed().as_micros() as u64;
    if admit_at(&DUE, now) {
        return true;
    }
    OVER.fetch_add(1, Ordering::Relaxed);
    false
}

/// A GCRA bucket: `BURST` lines at once, then one every `1/PER_SECOND` seconds.
fn admit_at(due: &AtomicU64, now: u64) -> bool {
    let (every, slack) = (1_000_000 / PER_SECOND, (BURST - 1) * (1_000_000 / PER_SECOND));
    let mut was = due.load(Ordering::Relaxed);
    loop {
        let next = was.max(now) + every;
        if next > now + slack + every {
            return false;
        }
        match due.compare_exchange_weak(was, next, Ordering::Relaxed, Ordering::Relaxed) {
            Ok(_) => return true,
            Err(v) => was = v,
        }
    }
}

/// `s` cut to `n` characters, then back to the last character that ends a run of letters and digits: no id or key is
/// left cut short below the length it would be recognized at.
fn cap(s: &str, n: usize) -> &str {
    match s.char_indices().nth(n) {
        None => s,
        Some((at, _)) => {
            let head = &s[..at];
            head.char_indices().rev().find(|&(_, c)| !opaque(c)).map_or("", |(i, c)| &head[..i + c.len_utf8()])
        }
    }
}

/// Scrubs `line` and hands it to the writer; `urgent` ones are written out at once. Dropped if scrubbing fails or the
/// writer is behind.
fn write(line: &str, urgent: bool) {
    let Some(tx) = LOG.get() else { return };
    let line = cap(line, MAX_LINE);
    SCRUBBING.with(|s| s.set(true));
    let scrubbed = std::panic::catch_unwind(|| scrub(line));
    SCRUBBING.with(|s| s.set(false));
    let Ok(scrubbed) = scrubbed else { return };
    let line = format!("[{}] {}\n", util::now_ms(), util::one_line(&scrubbed, MAX_LINE));
    if let Err(TrySendError::Full(_)) = tx.try_send(Msg::Line(line, urgent)) {
        BEHIND.fetch_add(1, Ordering::Relaxed);
    }
}

struct Writer {
    path: PathBuf,
    /// None between closing the full file and opening the next, and for good once that fails.
    file: Option<BufWriter<File>>,
    len: u64,
}

/// Marks the writer gone when its thread ends, however it ends.
struct Alive;

impl Drop for Alive {
    fn drop(&mut self) {
        WRITER.store(false, Ordering::Relaxed);
    }
}

impl Writer {
    fn open(path: PathBuf) -> std::io::Result<Writer> {
        let file = open_private(&path, false)?;
        let len = file.metadata()?.len();
        Ok(Writer { path, file: Some(BufWriter::with_capacity(64 << 10, file)), len })
    }

    /// The writer thread: lines as they come; every `FLUSH_EVERY` (and at once for urgent ones) written out, with a
    /// count of any left out.
    fn run(mut self, rx: Receiver<Msg>) {
        let _alive = Alive;
        let mut flushed = Instant::now();
        loop {
            match rx.recv_timeout(FLUSH_EVERY.saturating_sub(flushed.elapsed())) {
                Ok(Msg::Line(line, urgent)) => {
                    self.write(line.as_bytes());
                    if urgent {
                        self.flush();
                    }
                }
                Ok(Msg::Flush(ack)) => {
                    self.note_left_out();
                    self.flush();
                    let _ = ack.send(());
                }
                Err(RecvTimeoutError::Timeout) => {}
                Err(RecvTimeoutError::Disconnected) => {
                    self.note_left_out();
                    self.flush();
                    return;
                }
            }
            if flushed.elapsed() >= FLUSH_EVERY {
                self.note_left_out();
                self.flush();
                flushed = Instant::now();
            }
        }
    }

    /// Says how many lines were left out since it last said, if any.
    fn note_left_out(&mut self) {
        let n = BEHIND.swap(0, Ordering::Relaxed) + OVER.swap(0, Ordering::Relaxed);
        if n > 0 {
            self.write(format!("[{}] ({n} lines left out: too many at once)\n", util::now_ms()).as_bytes());
        }
    }

    fn write(&mut self, line: &[u8]) {
        if self.len > 0 && self.len + line.len() as u64 > MAX_BYTES && self.file.is_some() {
            if let Some(mut file) = self.file.take() {
                let _ = file.flush();
            }
            // Closed first: Windows doesn't rename a file that is open.
            let mut old = self.path.clone().into_os_string();
            old.push(".1");
            if std::fs::rename(&self.path, old).is_err() {
                let _ = std::fs::remove_file(&self.path);
            }
            // Started over even if the full one is still there: never past the cap. If that fails too, the log stops.
            match open_private(&self.path, true) {
                Ok(file) => {
                    self.file = Some(BufWriter::with_capacity(64 << 10, file));
                    self.len = 0;
                }
                Err(e) => {
                    eprintln!("[{}] debug log stopped: can't start {} over: {e}", util::now_ms(), self.path.display())
                }
            }
        }
        if let Some(file) = &mut self.file
            && file.write_all(line).is_ok()
        {
            self.len += line.len() as u64;
        }
    }

    fn flush(&mut self) {
        if let Some(file) = &mut self.file {
            let _ = file.flush();
        }
    }
}

/// Opened for appending (or emptied), readable by its owner only, and never through a symlink.
fn open_private(path: &Path, truncate: bool) -> std::io::Result<File> {
    let mut opts = OpenOptions::new();
    if truncate {
        opts.write(true).truncate(true);
    } else {
        opts.append(true);
    }
    opts.create(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.mode(0o600).custom_flags(libc::O_NOFOLLOW);
    }
    let file = opts.open(path)?;
    // A file left from before with wider permissions gets narrowed too.
    #[cfg(unix)]
    file.set_permissions(std::os::unix::fs::PermissionsExt::from_mode(0o600))?;
    Ok(file)
}

/// The networking library's events at debug level and above; everything else (spans included) is off.
struct Networking;

fn wanted(meta: &Metadata) -> bool {
    let target = meta.target();
    meta.is_event()
        && *meta.level() <= Level::DEBUG
        && TARGETS.iter().any(|t| target.starts_with(t))
        && !NOT_TARGETS.iter().any(|t| target.starts_with(t))
}

impl Subscriber for Networking {
    fn register_callsite(&self, meta: &'static Metadata<'static>) -> Interest {
        if wanted(meta) { Interest::always() } else { Interest::never() }
    }

    fn enabled(&self, meta: &Metadata) -> bool {
        wanted(meta)
    }

    fn max_level_hint(&self) -> Option<LevelFilter> {
        Some(LevelFilter::DEBUG)
    }

    fn new_span(&self, _: &span::Attributes) -> span::Id {
        span::Id::from_u64(1)
    }

    fn record(&self, _: &span::Id, _: &span::Record) {}

    fn record_follows_from(&self, _: &span::Id, _: &span::Id) {}

    fn event(&self, event: &Event) {
        if !admit() {
            return;
        }
        let meta = event.metadata();
        let mut line = Line::default();
        event.record(&mut line);
        let urgent = *meta.level() <= Level::WARN;
        write(&format!("{} {}: {}{}", meta.level(), meta.target(), line.message, line.fields), urgent);
    }

    fn enter(&self, _: &span::Id) {}

    fn exit(&self, _: &span::Id) {}
}

#[derive(Default)]
struct Line {
    message: String,
    fields: String,
}

impl Visit for Line {
    fn record_debug(&mut self, field: &Field, value: &dyn fmt::Debug) {
        let name = field.name();
        if name == "message" {
            self.message = unquote(cap(&format!("{value:?}"), MAX_LINE), '"', 64);
        } else if !secret_field(name) {
            self.fields += &format!(" {name}={}", unquote(cap(&format!("{value:?}"), MAX_VALUE), '"', 64));
        }
    }

    fn record_bytes(&mut self, field: &Field, value: &[u8]) {
        if !secret_field(field.name()) {
            self.fields += &format!(" {}=<{} bytes>", field.name(), value.len());
        }
    }
}

fn secret_field(name: &str) -> bool {
    let name = name.to_ascii_lowercase();
    SECRET_FIELDS.iter().any(|s| name.contains(s)) || (name.contains("key") && !name.contains("public"))
}

/// Drops what is inside each pair of `q` quotes, unless it is one short word (at most `keep` characters, no spaces, no
/// escapes): a host or an error name stays, a sentence doesn't. Escaped quotes (`\"`) don't end a quoted part, and one
/// that never ends is dropped to the end. A `'` counts only as a quote, not an apostrophe: after no letter or digit.
fn unquote(s: &str, q: char, keep: usize) -> String {
    let chars: Vec<char> = s.chars().collect();
    let mut out = String::with_capacity(s.len());
    let mut i = 0;
    while i < chars.len() {
        let c = chars[i];
        let opens = c == q && (q != '\'' || i == 0 || !chars[i - 1].is_alphanumeric());
        if !opens {
            out.push(c);
            i += 1;
            continue;
        }
        let closes = |j: usize| q != '\'' || chars.get(j + 1).is_none_or(|n| !n.is_alphanumeric());
        let (mut j, mut escaped) = (i + 1, false);
        while j < chars.len() && !(chars[j] == q && !escaped && closes(j)) {
            escaped = chars[j] == '\\' && !escaped;
            j += 1;
        }
        let inside = &chars[i + 1..j.min(chars.len())];
        out.push(q);
        if inside.len() > keep || inside.iter().any(|c| c.is_whitespace() || *c == '\\') {
            out.push('…');
        } else {
            out.extend(inside);
        }
        if j < chars.len() {
            out.push(q);
        }
        i = j + 1;
    }
    out
}

/// Takes out what could be a secret, someone's content or someone else's machine: invite codes and their ids and
/// secrets (and prefixes), URLs' user info, query and fragment (where links carry tickets and proxies their passwords),
/// credentials, tickets, byte lists, ids of machines outside this team (and cuts this team's to 10 characters), and any
/// other long run of letters and digits (encoded keys, tokens, tickets).
pub fn scrub(line: &str) -> String {
    let hide = HIDE.lock().unwrap_or_else(|e| e.into_inner()).clone();
    let known = KNOWN.lock().unwrap_or_else(|e| e.into_inner()).clone();
    scrub_with(line, &hide, &known)
}

fn scrub_with(line: &str, hide: &[String], known: &[(String, String)]) -> String {
    tokens(&byte_lists(&credentials(&urls(&hidden(line, hide)))), known)
}

fn hidden(line: &str, hide: &[String]) -> String {
    let mut line = line.to_string();
    for text in hide {
        let lower = line.to_ascii_lowercase();
        let (mut out, mut done) = (String::with_capacity(line.len()), 0);
        while let Some(at) = lower[done..].find(&text[..HIDDEN_PREFIX]).map(|i| i + done) {
            let end = at + lower[at..].bytes().zip(text.bytes()).take_while(|(a, b)| a == b).count();
            out += &line[done..at];
            out += HIDDEN;
            done = end;
        }
        out += &line[done..];
        line = out;
    }
    line
}

/// Each `scheme://…` keeps its scheme, host and path. Everything up to the URL's last `@` goes (user and password,
/// whatever characters the password has), and so do the query and fragment.
fn urls(line: &str) -> String {
    let mut out = String::with_capacity(line.len());
    let mut rest = line;
    while let Some(sep) = rest.find("://") {
        let scheme_at = rest[..sep]
            .char_indices()
            .rev()
            .find(|&(_, c)| !(c.is_ascii_alphanumeric() || "+.-".contains(c)))
            .map_or(0, |(i, c)| i + c.len_utf8());
        let scheme = &rest[scheme_at..sep];
        let after = &rest[sep + 3..];
        let end = after.find(|c: char| c.is_whitespace() || "\"<>`".contains(c)).unwrap_or(after.len());
        let url = &after[..end];
        let url = url.rfind('@').map_or(url, |at| &url[at + 1..]);
        let host_end = url.find(['/', '?', '#']).unwrap_or(url.len());
        let path = url[host_end..].split(['?', '#']).next().unwrap_or_default();
        out += &rest[..scheme_at];
        out += &format!("{scheme}://{}{path}", &url[..host_end]);
        rest = &after[end..];
    }
    out + rest
}

/// The value after a sign-in marker, up to the next whitespace: `Basic`/`Bearer` (and the whitespace after them),
/// `username=`/`password=`/`user=`, and the parts of a `Url` printed as a struct that can hold a secret.
fn credentials(line: &str) -> String {
    // (marker, whitespace must follow it, a value starting with `None` or `""` stays)
    const MARKERS: &[(&str, bool, bool)] = &[
        ("basic", true, false),
        ("bearer", true, false),
        ("username=", false, false),
        ("password=", false, false),
        ("user=", false, false),
        ("username: ", false, true),
        ("password: ", false, true),
        ("query: ", false, true),
        ("fragment: ", false, true),
    ];
    let lower = line.to_ascii_lowercase();
    let (mut out, mut done, mut i) = (String::with_capacity(line.len()), 0, 0);
    while i < line.len() {
        let at = &lower.as_bytes()[i..];
        let marker = MARKERS
            .iter()
            .filter(|&&(m, ws, _)| {
                at.starts_with(m.as_bytes()) && (!ws || at.get(m.len()).is_some_and(|b| b.is_ascii_whitespace()))
            })
            .max_by_key(|(m, ..)| m.len());
        let Some(&(m, _, keep_none)) = marker else {
            i += 1;
            continue;
        };
        let after = &line[i + m.len()..];
        let gap = after.len() - after.trim_start().len();
        let value = &after[gap..];
        let end = value.find(char::is_whitespace).unwrap_or(value.len());
        out += &line[done..i + m.len() + gap];
        let none = keep_none && (value.starts_with("None") || value.starts_with("\"\""));
        out += if end > 0 && !none { "[redacted]" } else { &value[..end] };
        i += m.len() + gap + end;
        done = i;
    }
    out + &line[done..]
}

/// A list of bytes (`[112, 145, …]`, a nonce or raw data) becomes its length: 8 or more numbers up to 255, or 2 or more
/// written `0x..`, or 4 or more right after a field named like data.
fn byte_lists(line: &str) -> String {
    let mut out = String::with_capacity(line.len());
    let mut rest = line;
    while let Some(open) = rest.find('[') {
        out += &rest[..open];
        let named = ["data=", "data: ", "bytes=", "bytes: ", "nonce=", "nonce: "]
            .iter()
            .any(|m| out.len() >= m.len() && out.as_bytes()[out.len() - m.len()..].eq_ignore_ascii_case(m.as_bytes()));
        let after = &rest[open + 1..];
        // Only up to the next bracket of either kind: a `[` first means this one isn't a list (and keeps this linear).
        let close = after.find(['[', ']']).filter(|&i| after.as_bytes()[i] == b']');
        let bytes = close.and_then(|close| {
            let mut items: Vec<&str> = after[..close].split(',').map(str::trim).collect();
            if items.len() > 1 && items.last() == Some(&"") {
                items.pop();
            }
            let hex = items.iter().all(|n| n.strip_prefix("0x").is_some_and(|h| u8::from_str_radix(h, 16).is_ok()));
            let dec = items.iter().all(|n| n.parse::<u8>().is_ok());
            let n = items.len();
            ((hex && n >= 2) || (dec && (n >= 8 || named && n >= 4))).then_some((close, n))
        });
        match bytes {
            Some((close, n)) => {
                out += &format!("[<{n} bytes>]");
                rest = &after[close + 1..];
            }
            None => {
                out.push('[');
                rest = after;
            }
        }
    }
    out + rest
}

/// What a run of letters and digits is made of: `=` splits, so `name=value` is judged by its value; `/` and `+`
/// don't, so base64 can't hide in short pieces.
fn opaque(c: char) -> bool {
    c.is_ascii_alphanumeric() || "-_+/".contains(c)
}

fn tokens(line: &str, known: &[(String, String)]) -> String {
    let mut out = String::with_capacity(line.len());
    let mut run = String::new();
    for c in line.chars() {
        if opaque(c) {
            run.push(c);
        } else {
            out += &token(&run, known);
            run.clear();
            out.push(c);
        }
    }
    out + &token(&run, known)
}

fn token(run: &str, known: &[(String, String)]) -> String {
    if crate::team::Ticket::is_ticket(run) {
        return "[ticket]".into();
    }
    if let Some(id) = id(run, known) {
        return id;
    }
    // An id may sit between `-`, `_`, `/` or `+` (`relay-<id>`): each piece is checked on its own.
    let mut pieces = Vec::new();
    let mut from = 0;
    for (i, c) in run.char_indices().filter(|&(_, c)| "-_/+".contains(c)) {
        pieces.extend([&run[from..i], &run[i..i + c.len_utf8()]]);
        from = i + c.len_utf8();
    }
    pieces.push(&run[from..]);
    if pieces.iter().any(|p| id(p, known).is_some()) {
        if run.contains(['+', '/']) && long_opaque(run) {
            return "[redacted]".into();
        }
        return pieces
            .iter()
            .map(|p| id(p, known).unwrap_or_else(|| if long_opaque(p) { "[redacted]".into() } else { p.to_string() }))
            .collect();
    }
    // A field name like `mapping_varies_by_dest_ipv4`: short lowercase words, joined by `_`.
    if run.contains('_')
        && run.split('_').all(|w| w.len() <= 12 && w.chars().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit()))
    {
        return run.into();
    }
    if long_opaque(run) { "[redacted]".into() } else { run.into() }
}

/// An id, whole or its first characters: 8 or more hex digits, or a z-base-32 key. Shown, cut to 10, only if it is
/// this machine's or a teammate's; `[id]` otherwise. None if `piece` isn't one.
fn id(piece: &str, known: &[(String, String)]) -> Option<String> {
    let n = piece.len();
    let ours = if n >= 8 && piece.bytes().all(|b| b.is_ascii_hexdigit()) {
        let lower = piece.to_ascii_lowercase();
        known.iter().any(|(k, _)| if n <= k.len() { k.starts_with(&lower) } else { lower.starts_with(k) })
    } else if n == Z32_KEY && piece.chars().all(|c| Z32.contains(c)) {
        known.iter().any(|(_, z)| z == piece)
    } else {
        return None;
    };
    Some(match (ours, n > 10) {
        (true, true) => format!("{}…", &piece[..10]),
        (true, false) => piece.into(),
        (false, _) => "[id]".into(),
    })
}

/// A long run of letters and digits mixed (an encoded key, a token).
fn long_opaque(run: &str) -> bool {
    let n = run.len();
    let has = |f: fn(&char) -> bool| run.chars().any(|c| f(&c));
    let (digits, upper, lower) =
        (has(char::is_ascii_digit), has(char::is_ascii_uppercase), has(char::is_ascii_lowercase));
    n >= 24 && digits && (upper || lower) || n >= 32 && upper && lower
}

#[cfg(test)]
mod tests {
    use super::*;

    fn none(line: &str) -> String {
        scrub_with(line, &[], &[])
    }

    #[test]
    fn links_tickets_and_keys_are_taken_out() {
        let ticket = format!("rn1-{}", "QUJDRA".repeat(30));
        let line = format!(
            "join https://reasonmachines.com/network/join/#{ticket} via https://user:pw@proxy.example:8080/p?token=abc \
             ticket {ticket} old tincan1abcdefg key 0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef \
             b64 c2VjcmV0LWtleS1ieXRlcy0xMjM0NTY3ODkw zb32 ybndrfg8ejkmcpqxot1uwisza345h769ybndrfg8ejkmcpqxot1uwi"
        );
        let out = none(&line);
        for gone in [&ticket[..], "QUJDRA", "user:pw", "token=abc", "tincan1abcdefg", "0123456789abcdef"] {
            assert!(!out.contains(gone), "{gone} in {out}");
        }
        for gone in ["c2VjcmV0LWtleS1ieXRlcy0xMjM0NTY3ODkw", "ybndrfg8ejkmcpqxot1uwisza345h769"] {
            assert!(!out.contains(gone), "{gone} in {out}");
        }
        assert!(out.contains("https://reasonmachines.com/network/join/ via https://proxy.example:8080/p"), "{out}");
        assert!(out.contains("key [id]"), "{out}");
    }

    #[test]
    fn only_this_teams_ids_show() {
        let (me, mate, other) = (
            iroh::SecretKey::generate().public(),
            iroh::SecretKey::generate().public(),
            iroh::SecretKey::generate().public(),
        );
        let known = [(me.to_string(), me.to_z32()), (mate.to_string(), mate.to_z32())];
        let line = |id: &iroh::EndpointId| {
            format!(
                "bound id={} remote_id={} discovered_endpoint_id=PublicKey({id}) dns=_iroh.{}.dns.iroh.link",
                id.fmt_short(),
                id.fmt_short(),
                id.to_z32()
            )
        };
        for id in [me, mate] {
            let short = id.fmt_short().to_string();
            let out = scrub_with(&line(&id), &[], &known);
            assert_eq!(
                out,
                format!(
                    "bound id={short} remote_id={short} discovered_endpoint_id=PublicKey({short}…) dns=_iroh.{}….dns.iroh.link",
                    &id.to_z32()[..10]
                )
            );
        }
        // A machine on the local network, or anyone else's, that this one never knew.
        let out = scrub_with(&line(&other), &[], &known);
        assert_eq!(
            out,
            "bound id=[id] remote_id=[id] discovered_endpoint_id=PublicKey([id]) dns=_iroh.[id].dns.iroh.link"
        );
        let hex = other.to_string();
        for n in [8, 10, 12, 32, 64] {
            assert_eq!(scrub_with(&format!("x={}", &hex[..n]), &[], &known), "x=[id]", "{n}");
            // Also between `-`, `_`, `/` or `+` (where base64 can be, a long run goes whole).
            for sep in ["-", "_", "/", "+"] {
                let out = scrub_with(&format!("x=relay{sep}{}{sep}b", &hex[..n]), &[], &known);
                assert!(!out.contains(&hex[..8]), "{n} {sep}: {out}");
                if "-_".contains(sep) {
                    assert_eq!(out, format!("x=relay{sep}[id]{sep}b"), "{n} {sep}");
                }
            }
        }
        // Digits alone count too.
        assert_eq!(scrub_with("x=12345678 y=1234567", &[], &known), "x=[id] y=1234567");
        let short = me.fmt_short().to_string();
        assert_eq!(scrub_with(&format!("relay-{short}"), &[], &known), format!("relay-{short}"));
    }

    #[test]
    fn proxy_passwords_go_whatever_they_contain() {
        for pw in ["s3cr(et", "s3cr)et", "s3cr'et", "s3cr,et", "s3cr/et", "s3cr?et", "s3cr#et", "s3cr@et", "s3cr[et]"] {
            for scheme in ["http", "https", "socks5", "wss"] {
                let line = format!("dial url via proxy url={scheme}://alice:{pw}@proxy.example:3128/ next=1");
                let out = none(&line);
                assert!(!out.contains("s3cr") && !out.contains("alice"), "{pw}: {out}");
                assert!(out.contains(&format!("{scheme}://proxy.example:3128/ next=1")), "{pw}: {out}");
            }
        }
        assert_eq!(none("proxy_url=\"http://u:p(w@h:8080\""), "proxy_url=\"http://h:8080\"");
        // iroh's relay client also logs the proxy's user name, and the request with its Basic header.
        assert_eq!(
            none("setting proxy-authorization: username=alice"),
            "setting proxy-authorization: username=[redacted]"
        );
        let req =
            r#"Sending proxy request: Request { method: CONNECT, headers: {"proxy-authorization": "Basic YTpi"} }"#;
        assert!(!none(req).contains("YTpi") && !unquote(req, '"', 64).contains("YTpi"), "{}", none(req));
        assert_eq!(none("auth Basic\tYTpi,x next"), "auth Basic\t[redacted] next");
        assert_eq!(none("Bearer a,b;c)d}e]f next"), "Bearer [redacted] next");
        // A `Url` printed as a struct.
        let url = r#"Url { scheme: "http", username: "alice", password: Some("s3cr"), host: Some(Domain("proxy")), port: Some(3128), path: "/", query: Some("t=1"), fragment: None }"#;
        let out = none(url);
        assert!(!out.contains("alice") && !out.contains("s3cr") && !out.contains("t=1"), "{out}");
        assert!(out.contains("fragment: None") && out.contains("Domain(\"proxy\")"), "{out}");
        assert!(none(r#"Url { username: "", password: None }"#).contains(r#"username: "", password: None"#));
    }

    #[test]
    fn an_invite_codes_key_secret_and_words_never_show() {
        let code = "acid-acorn-acre-acid";
        let id = crate::invite::key(code).public();
        let secret = crate::invite::secret(code);
        let (hex, z32) = (id.to_string(), id.to_z32());
        let line = format!(
            "iroh endpoint bound id={} remote_id={} full={hex} PublicKey({}…) dns=_iroh.{z32}.dns.iroh.link \
             upper={} secret={secret} short={} code {code} or acid acorn acre acid",
            id.fmt_short(),
            &hex[..10],
            &hex[..10],
            hex[..12].to_uppercase(),
            &secret[..9]
        );
        let hide = HIDE.lock().unwrap().clone();
        // Even as if the invite's id were a teammate's: hidden comes first.
        let out = scrub_with(&line, &hide, &[(hex.clone(), z32.clone())]).to_ascii_lowercase();
        // No secret-derived text in a failure message.
        for gone in [&hex[..HIDDEN_PREFIX], &z32[..HIDDEN_PREFIX], &secret[..HIDDEN_PREFIX], "acid-aco", "acid aco"] {
            assert!(!out.contains(gone), "a form of the invite's code, id or secret is left");
        }
        assert!(
            out.contains("bound id=[invite]") && out.contains("_iroh.[invite].dns"),
            "the invite id isn't replaced"
        );
    }

    #[test]
    fn what_helps_diagnose_stays() {
        let me = iroh::SecretKey::generate().public();
        let known = [(me.to_string(), me.to_z32())];
        let line = format!(
            "path direct -> relay (rtt 41 ms) relay=https://relay-sfo1.compute.reasonmachines.com/ remote={} \
             addr=192.168.1.20:41641 [2001:db8::1]:41641 err=\"timed out\" mapping_varies_by_dest_ipv4: None \
             [1, 2, 3] path_id=7 ports [1000, 2000, 3000, 4000, 5000, 6000, 7000, 8000] rtt=28.369875ms",
            me.fmt_short()
        );
        assert_eq!(scrub_with(&line, &[], &known), line);
    }

    #[test]
    fn byte_lists_become_their_length() {
        assert_eq!(
            none("ping data=[112, 145, 70, 165, 181, 195, 189, 229] rtt=28ms"),
            "ping data=[<8 bytes>] rtt=28ms"
        );
        assert_eq!(none("x [1, 2, 3, 4, 5, 6, 7, 8,] y"), "x [<8 bytes>] y");
        assert_eq!(none("x [0x0a, 0xff] y"), "x [<2 bytes>] y");
        assert_eq!(none("nonce: [9, 8, 7, 6] y"), "nonce: [<4 bytes>] y");
        assert_eq!(none("ids [9, 8, 7, 6] y"), "ids [9, 8, 7, 6] y");
    }

    #[test]
    fn non_ascii_text_never_breaks_it() {
        for line in
            ["éhttps://x", "接続https://u:p@h/", "…://x", "relay=é://x", "a\u{301}://u:p@h", "€[1,2]", "é=Basic é"]
        {
            let out = none(line);
            assert!(!out.contains("u:p"), "{out}");
        }
        // Random text, heavy on what the scrubber looks for, never panics.
        let pool: Vec<char> =
            "aZ09fF-_+/=:@#?&[](){}<>\"'`\\,; \t\n.…éü接続€\u{301}\u{200b}xyzrn1-tincan1BasicBearerusername=password: None0x"
                .chars()
                .collect();
        let mut seed = 0x2545f4914f6cdd1du64;
        let mut next = || {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            seed
        };
        let hide = vec!["0123456789abcdef".to_string()];
        let known = vec![("abcdef0123456789".to_string(), "ybndrfg8ejkm".to_string())];
        for _ in 0..20_000 {
            let len = (next() % 80) as usize;
            let s: String = (0..len).map(|_| pool[(next() % pool.len() as u64) as usize]).collect();
            let s = if next() % 3 == 0 { format!("{s}://{s}@{s}") } else { s };
            let _ = scrub_with(&s, &hide, &known);
            let _ = unquote(&s, '"', 64);
            let _ = unquote(&s, '\'', 0);
        }
    }

    #[test]
    fn quoted_text_goes_unless_it_is_one_short_word() {
        let msg = r#"invalid type: string "please deploy the staging branch", expected u64 host "relay.example""#;
        assert_eq!(unquote(msg, '"', 64), r#"invalid type: string "…", expected u64 host "relay.example""#);
        assert_eq!(unquote(r#"bad frame: "deploy""#, '"', 0), r#"bad frame: "…""#);
        // An escaped quote doesn't end it: the whole literal goes.
        assert_eq!(unquote(r#"x="say \"ship\" now" y=1"#, '"', 64), r#"x="…" y=1"#);
        assert_eq!(unquote(r#"x="ship\"it" y=1"#, '"', 64), r#"x="…" y=1"#);
        assert_eq!(unquote(r#"x="never ends"#, '"', 64), r#"x="…"#);
        assert_eq!(unquote("unknown variant `ship it now`, expected one", '`', 0), "unknown variant `…`, expected one");
        assert_eq!(unquote("can't parse 'ship it' here", '\'', 0), "can't parse '…' here");
        assert_eq!(unquote("couldn't keep it; won't retry", '\'', 0), "couldn't keep it; won't retry");
    }

    #[test]
    fn our_lines_lose_all_quoted_text() {
        let out = unquote(&unquote(&unquote(r#"bad: "a b" `c` 'd' it's"#, '"', 0), '`', 0), '\'', 0);
        assert_eq!(out, r#"bad: "…" `…` '…' it's"#);
    }

    #[test]
    fn fields_named_like_secrets_are_left_out() {
        for name in
            ["secret_key", "auth_token", "ticket", "Password", "username", "payload", "body", "content_len", "key"]
        {
            assert!(secret_field(name), "{name}");
        }
        for name in ["remote", "public_key", "path", "rtt", "relay_url", "addr"] {
            assert!(!secret_field(name), "{name}");
        }
        let mut line = Line::default();
        line.record_bytes(&FIELD.field("data").unwrap(), &[1, 2, 3]);
        assert_eq!(line.fields, " data=<3 bytes>");
    }

    struct Site;
    impl tracing_core::callsite::Callsite for Site {
        fn set_interest(&self, _: Interest) {}
        fn metadata(&self) -> &Metadata<'_> {
            unreachable!()
        }
    }
    static SITE: Site = Site;
    static FIELD: tracing_core::field::FieldSet =
        tracing_core::field::FieldSet::new(&["data"], tracing_core::identify_callsite!(&SITE));

    #[test]
    fn only_the_networking_librarys_events_are_kept() {
        use tracing_core::{field::FieldSet, metadata::Kind};
        let meta = |target: &'static str, level: Level, kind: Kind| {
            Metadata::new(
                "e",
                target,
                level,
                None,
                None,
                None,
                FieldSet::new(&[], tracing_core::identify_callsite!(&SITE)),
                kind,
            )
        };
        assert!(wanted(&meta("iroh::socket", Level::DEBUG, Kind::EVENT)));
        assert!(wanted(&meta("noq_proto::connection", Level::WARN, Kind::EVENT)));
        assert!(wanted(&meta("iroh_relay::client", Level::INFO, Kind::EVENT)));
        assert!(!wanted(&meta("iroh::socket", Level::TRACE, Kind::EVENT)), "trace is too much");
        assert!(!wanted(&meta("iroh::socket", Level::DEBUG, Kind::SPAN)), "spans are off");
        // mDNS discovery lists every machine on the local network.
        assert!(!wanted(&meta("iroh_mdns_address_lookup", Level::DEBUG, Kind::EVENT)));
        // The MCP server's library would log tool calls, message text included.
        assert!(!wanted(&meta("rmcp::service", Level::ERROR, Kind::EVENT)));
        assert!(!wanted(&meta("hickory_proto::udp", Level::DEBUG, Kind::EVENT)));
        assert!(!wanted(&meta("reason_network::net", Level::ERROR, Kind::EVENT)));
    }

    fn temp(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("rn-debuglog-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn the_file_is_its_owners_and_starts_over_past_the_cap() {
        let dir = temp("cap");
        let path = dir.join("debug.log");
        let mut w = Writer::open(path.clone()).unwrap();
        let line = vec![b'x'; 1 << 16];
        for _ in 0..(2 * MAX_BYTES / line.len() as u64 + 3) {
            w.write(&line);
        }
        w.flush();
        let size = |p: &Path| std::fs::metadata(p).map(|m| m.len()).unwrap_or(0);
        let old = dir.join("debug.log.1");
        assert!(size(&path) <= MAX_BYTES && size(&old) <= MAX_BYTES && size(&old) > 0);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            for p in [&path, &old] {
                assert_eq!(std::fs::metadata(p).unwrap().permissions().mode() & 0o777, 0o600, "{}", p.display());
            }
            // An existing file with wider permissions is narrowed; a symlink in its place is refused.
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();
            drop(Writer::open(path.clone()).unwrap());
            assert_eq!(std::fs::metadata(&path).unwrap().permissions().mode() & 0o777, 0o600);
            let link = dir.join("link.log");
            std::os::unix::fs::symlink(&old, &link).unwrap();
            assert!(Writer::open(link).is_err());
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_cap_holds_when_the_full_file_cant_be_moved() {
        let dir = temp("stuck");
        let path = dir.join("debug.log");
        let mut w = Writer::open(path.clone()).unwrap();
        // `debug.log.1` a non-empty directory: the rename fails.
        std::fs::create_dir_all(dir.join("debug.log.1").join("x")).unwrap();
        let line = vec![b'x'; 1 << 16];
        for _ in 0..(2 * MAX_BYTES / line.len() as u64) {
            w.write(&line);
        }
        w.flush();
        assert!(std::fs::metadata(&path).unwrap().len() <= MAX_BYTES);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_flood_is_cut_off() {
        let due = AtomicU64::new(0);
        let at = |t: u64| (0..BURST + 500).filter(|_| admit_at(&due, t)).count() as u64;
        assert_eq!(at(1_000_000), BURST);
        // A second later, a second's worth more.
        assert_eq!(at(2_000_000), PER_SECOND);
        assert_eq!(at(2_000_000), 0);
    }

    #[test]
    fn what_was_left_out_is_said_every_tick_even_while_lines_keep_coming() {
        let dir = temp("tick");
        let path = dir.join("debug.log");
        let w = Writer::open(path.clone()).unwrap();
        let (tx, rx) = sync_channel(QUEUE);
        let writer = std::thread::spawn(move || w.run(rx));
        OVER.fetch_add(7, Ordering::Relaxed);
        let begun = Instant::now();
        while begun.elapsed() < FLUSH_EVERY + Duration::from_millis(800) {
            tx.send(Msg::Line("line\n".into(), false)).unwrap();
            std::thread::sleep(Duration::from_millis(20));
        }
        // Read while the writer still runs: only the tick (2 s after it started, lines arriving all along) wrote it out.
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(text.contains("(7 lines left out: too many at once)"), "{}", text.len());
        assert!(text.lines().any(|l| l == "line"));
        drop(tx);
        writer.join().unwrap();
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_huge_line_is_scrubbed_in_linear_time() {
        let mb = 1 << 20;
        let bound = Duration::from_millis(if cfg!(debug_assertions) { 3000 } else { 50 });
        let hide = vec!["0123456789abcdef".to_string()];
        for unit in ["[", "user=a ", "\"", "://", "@", "0x", "basic ", "[1,", "0123456", "a\"b ", "'"] {
            let line = unit.repeat(mb / unit.len());
            let begun = Instant::now();
            let _ = scrub_with(&line, &hide, &[]);
            let _ = unquote(&line, '"', 64);
            let _ = unquote(&line, '\'', 0);
            assert!(begun.elapsed() < bound, "{unit:?}: {:?}", begun.elapsed());
        }
        // And what is written is cut first, on a character boundary, back to where a run ends.
        assert_eq!(cap(&"a ".repeat(3000), 4000).len(), 4000);
        assert_eq!(
            cap(&format!("{} {}", "x".repeat(3990), "0123456789abcdef"), 4000),
            format!("{} ", "x".repeat(3990))
        );
        assert_eq!(cap(&"a".repeat(5000), 4000), "");
        assert_eq!(cap(&"é".repeat(5000), 4000).chars().count(), 4000);
    }
}
