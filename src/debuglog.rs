//! The debug log, for working out why connections drop or switch to a relay. Off unless `debug_log = true` in
//! config.toml. The daemon then writes the networking library's own events (iroh and its QUIC, at debug level) and its
//! own network lines to `debug.log` next to `daemon.log`, on this machine only. What could carry message text, files,
//! tickets, links, codes or keys stays out of it: only networking targets are kept, fields named like secrets are
//! dropped, and every line is scrubbed before it is written.

use crate::{paths, util};
use std::{
    fmt,
    fs::{File, OpenOptions},
    io::{BufWriter, Write},
    path::{Path, PathBuf},
    sync::{Mutex, OnceLock},
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
/// what came before it.
const BURST: f64 = 300.0;
const PER_SECOND: f64 = 5.0;

/// Where the networking library's events come from: iroh's crates, its QUIC (noq), and the network watchers.
const TARGETS: &[&str] = &["iroh", "noq", "netwatch", "portmapper"];

/// Fields whose names say they may hold a secret or someone's content are left out whole.
const SECRET_FIELDS: &[&str] =
    &["secret", "private", "token", "password", "ticket", "auth", "cookie", "credential", "payload", "body", "content"];

/// What a hidden id or secret becomes, and how much of it is enough to recognize it.
const HIDDEN: &str = "[invite]";
const HIDDEN_PREFIX: usize = 8;

static LOG: OnceLock<Mutex<Writer>> = OnceLock::new();

/// Ids and secrets that stand for something guessable (an invite code's key and secret), in every form a line may show
/// them in: even a short prefix of one would let someone holding the log test codes offline.
static HIDE: Mutex<Vec<String>> = Mutex::new(Vec::new());

pub fn path() -> PathBuf {
    paths::home().join("debug.log")
}

/// Turns the log on for this process (the daemon, when its config says so). Says why not if it can't be.
pub fn start() -> Result<(), String> {
    let writer = Writer::open(path()).map_err(|e| format!("can't write {}: {e}", path().display()))?;
    LOG.set(Mutex::new(writer)).map_err(|_| "already on".to_string())?;
    dispatcher::set_global_default(Dispatch::new(Networking)).map_err(|e| e.to_string())?;
    // Written out every couple of seconds, so a quiet stretch still shows what came before it.
    std::thread::spawn(|| {
        loop {
            std::thread::sleep(Duration::from_secs(2));
            if let Some(log) = LOG.get() {
                log.lock().unwrap_or_else(|e| e.into_inner()).flush();
            }
        }
    });
    Ok(())
}

/// Keeps `id` (an invite code's key) out of the log: its hex and z-base-32 forms, and any prefix of them.
pub fn hide(id: &iroh::EndpointId) {
    hide_text(&id.to_string());
    hide_text(&id.to_z32());
}

/// Keeps `text` (a secret that stands for a guessable code) and any prefix of it out of the log.
pub fn hide_text(text: &str) {
    let text = text.to_ascii_lowercase();
    let mut hide = HIDE.lock().unwrap_or_else(|e| e.into_inner());
    if text.len() >= HIDDEN_PREFIX && !hide.contains(&text) {
        hide.push(text);
    }
}

/// One of the daemon's own network lines (`what` names the part), when the log is on. Quoted text in it is dropped
/// too: here that can only be an error's echo of what a peer sent.
pub fn ours(what: &str, msg: &str) {
    if LOG.get().is_some() {
        let msg = unquote(&unquote(&unquote(msg, '"', 0), '`', 0), '\'', 0);
        write(&format!("{what}: {msg}"));
    }
}

fn write(line: &str) {
    if let Some(log) = LOG.get() {
        let line = format!("[{}] {}\n", util::now_ms(), util::one_line(&scrub(line), 4000));
        log.lock().unwrap_or_else(|e| e.into_inner()).line(line.as_bytes());
    }
}

struct Writer {
    path: PathBuf,
    /// None between closing the full file and opening the next, and for good once that fails.
    file: Option<BufWriter<File>>,
    len: u64,
    tokens: f64,
    filled: Instant,
    dropped: u64,
}

impl Writer {
    fn open(path: PathBuf) -> std::io::Result<Writer> {
        let file = open_private(&path, false)?;
        let len = file.metadata()?.len();
        let file = Some(BufWriter::with_capacity(64 << 10, file));
        Ok(Writer { path, file, len, tokens: BURST, filled: Instant::now(), dropped: 0 })
    }

    /// Writes `line` unless the rate limit is spent; says how many were left out once there is room again.
    fn line(&mut self, line: &[u8]) {
        let now = Instant::now();
        self.tokens = (self.tokens + now.duration_since(self.filled).as_secs_f64() * PER_SECOND).min(BURST);
        self.filled = now;
        if self.tokens < 1.0 {
            self.dropped += 1;
            return;
        }
        self.tokens -= 1.0;
        if self.dropped > 0 {
            let note = format!("[{}] ({} lines left out: too many at once)\n", util::now_ms(), self.dropped);
            self.dropped = 0;
            let _ = self.write(note.as_bytes());
        }
        let _ = self.write(line);
    }

    fn write(&mut self, line: &[u8]) -> std::io::Result<()> {
        if self.len > 0 && self.len + line.len() as u64 > MAX_BYTES {
            // Closed first: Windows doesn't rename a file that is open.
            if let Some(mut file) = self.file.take() {
                let _ = file.flush();
            }
            let mut old = self.path.clone().into_os_string();
            old.push(".1");
            if std::fs::rename(&self.path, old).is_err() {
                let _ = std::fs::remove_file(&self.path);
            }
            // Started over even if the full one is still there: never past the cap. If that fails too, the log stops.
            self.file = Some(BufWriter::with_capacity(64 << 10, open_private(&self.path, true)?));
            self.len = 0;
        }
        let Some(file) = &mut self.file else { return Ok(()) };
        file.write_all(line)?;
        self.len += line.len() as u64;
        Ok(())
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
    meta.is_event() && *meta.level() <= Level::DEBUG && TARGETS.iter().any(|t| meta.target().starts_with(t))
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
        let meta = event.metadata();
        let mut line = Line::default();
        event.record(&mut line);
        write(&format!("{} {}: {}{}", meta.level(), meta.target(), line.message, line.fields));
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
            self.message = unquote(&format!("{value:?}"), '"', 64);
        } else if !secret_field(name) {
            self.fields += &format!(" {name}={}", unquote(&format!("{value:?}"), '"', 64));
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
        let (mut j, mut escaped) = (i + 1, false);
        while j < chars.len()
            && !(chars[j] == q && !escaped && (q != '\'' || chars.get(j + 1).is_none_or(|n| !n.is_alphanumeric())))
        {
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

/// Takes out what could be a secret or someone's content: invite codes' ids and secrets (and their prefixes), URLs'
/// user info, query and fragment (where links carry tickets and proxies their passwords), tickets, long byte lists,
/// long hex (ids and keys, cut to its first 10 characters), and any other long run of letters and digits (encoded
/// keys, tokens, tickets).
pub fn scrub(line: &str) -> String {
    tokens(&byte_lists(&credentials(&urls(&hidden(line)))))
}

/// What follows `Basic `, `Bearer `, `username=` or `password=` (a proxy's sign-in, as iroh's relay client logs it),
/// up to the next space or quote.
fn credentials(line: &str) -> String {
    const BEFORE: &[&str] = &["basic ", "bearer ", "username=", "password=", "user="];
    let mut out = String::with_capacity(line.len());
    let mut rest = line;
    loop {
        let lower = rest.to_ascii_lowercase();
        let Some((at, len)) = BEFORE.iter().filter_map(|b| lower.find(b).map(|at| (at, b.len()))).min() else { break };
        let value = &rest[at + len..];
        let end = value.find(|c: char| c.is_whitespace() || "\"'`,;)}]".contains(c)).unwrap_or(value.len());
        out += &rest[..at + len];
        if end > 0 {
            out += "[redacted]";
        }
        rest = &value[end..];
    }
    out + rest
}

fn hidden(line: &str) -> String {
    let hide = HIDE.lock().unwrap_or_else(|e| e.into_inner());
    let mut line = line.to_string();
    for text in hide.iter() {
        loop {
            let lower = line.to_ascii_lowercase();
            let Some(at) = lower.find(&text[..HIDDEN_PREFIX]) else { break };
            let end = at + lower[at..].bytes().zip(text.bytes()).take_while(|(a, b)| a == b).count();
            line.replace_range(at..end, HIDDEN);
        }
    }
    line
}

/// Each `scheme://…` keeps its scheme, host and path. Everything up to the URL's last `@` goes (user and password,
/// whatever characters the password has), and so do the query and fragment.
fn urls(line: &str) -> String {
    let mut out = String::with_capacity(line.len());
    let mut rest = line;
    while let Some(sep) = rest.find("://") {
        let scheme_at =
            rest[..sep].rfind(|c: char| !(c.is_ascii_alphanumeric() || "+.-".contains(c))).map_or(0, |i| i + 1);
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

/// A list of 8 or more small numbers (`[112, 145, …]`, a nonce or other raw bytes) becomes its length.
fn byte_lists(line: &str) -> String {
    let mut out = String::with_capacity(line.len());
    let mut rest = line;
    while let Some(open) = rest.find('[') {
        out += &rest[..open];
        let after = &rest[open + 1..];
        let bytes = after.find(']').and_then(|close| {
            let items: Vec<&str> = after[..close].split(',').map(str::trim).collect();
            (items.len() >= 8 && items.iter().all(|n| n.parse::<u8>().is_ok())).then_some((close, items.len()))
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

fn tokens(line: &str) -> String {
    // `=` splits, so `name=value` is judged by its value; `/` and `+` don't, so base64 can't hide in short pieces.
    let opaque = |c: char| c.is_ascii_alphanumeric() || "-_+/".contains(c);
    let mut out = String::with_capacity(line.len());
    let mut run = String::new();
    for c in line.chars() {
        if opaque(c) {
            run.push(c);
        } else {
            out += &token(&run);
            run.clear();
            out.push(c);
        }
    }
    out + &token(&run)
}

fn token(run: &str) -> String {
    let n = run.len();
    if crate::team::Ticket::is_ticket(run) {
        return "[ticket]".into();
    }
    if n >= 32 && run.chars().all(|c| c.is_ascii_hexdigit()) {
        return format!("{}…", &run[..10]);
    }
    // A field name like `mapping_varies_by_dest_ipv4`: short lowercase words, joined by `_`.
    if run.contains('_')
        && run.split('_').all(|w| w.len() <= 12 && w.chars().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit()))
    {
        return run.into();
    }
    let has = |f: fn(&char) -> bool| run.chars().any(|c| f(&c));
    let (digits, upper, lower) =
        (has(char::is_ascii_digit), has(char::is_ascii_uppercase), has(char::is_ascii_lowercase));
    if n >= 24 && digits && (upper || lower) || n >= 32 && upper && lower { "[redacted]".into() } else { run.into() }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn links_tickets_and_keys_are_taken_out() {
        let ticket = format!("rn1-{}", "QUJDRA".repeat(30));
        let line = format!(
            "join https://reasonmachines.com/network/join/#{ticket} via https://user:pw@proxy.example:8080/p?token=abc \
             ticket {ticket} old tincan1abcdefg key 0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef \
             b64 c2VjcmV0LWtleS1ieXRlcy0xMjM0NTY3ODkw zb32 ybndrfg8ejkmcpqxot1uwisza345h769ybndrfg8ejkmcpqxot1uwi"
        );
        let out = scrub(&line);
        for gone in [&ticket[..], "QUJDRA", "user:pw", "token=abc", "tincan1abcdefg", "abcdef0123456789abcdef01234"] {
            assert!(!out.contains(gone), "{gone} in {out}");
        }
        for gone in ["c2VjcmV0LWtleS1ieXRlcy0xMjM0NTY3ODkw", "ybndrfg8ejkmcpqxot1uwisza345h769"] {
            assert!(!out.contains(gone), "{gone} in {out}");
        }
        assert!(out.contains("https://reasonmachines.com/network/join/ via https://proxy.example:8080/p"), "{out}");
        assert!(out.contains("key 0123456789…"), "{out}");
    }

    #[test]
    fn proxy_passwords_go_whatever_they_contain() {
        for pw in ["s3cr(et", "s3cr)et", "s3cr'et", "s3cr,et", "s3cr/et", "s3cr?et", "s3cr#et", "s3cr@et", "s3cr[et]"] {
            for scheme in ["http", "https", "socks5", "wss"] {
                let line = format!("dial url via proxy url={scheme}://alice:{pw}@proxy.example:3128/ next=1");
                let out = scrub(&line);
                assert!(!out.contains("s3cr") && !out.contains("alice"), "{pw}: {out}");
                assert!(out.contains(&format!("{scheme}://proxy.example:3128/ next=1")), "{pw}: {out}");
            }
        }
        assert_eq!(scrub("proxy_url=\"http://u:p(w@h:8080\""), "proxy_url=\"http://h:8080\"");
        // iroh's relay client also logs the proxy's user name, and the request with its Basic header.
        assert_eq!(
            scrub("setting proxy-authorization: username=alice"),
            "setting proxy-authorization: username=[redacted]"
        );
        let req =
            r#"Sending proxy request: Request { method: CONNECT, headers: {"proxy-authorization": "Basic YTpi"} }"#;
        assert!(!scrub(req).contains("YTpi") && !unquote(req, '"', 64).contains("YTpi"), "{}", scrub(req));
    }

    #[test]
    fn an_invite_codes_key_and_secret_never_show() {
        let code = "acid-acorn-acre-acid";
        let id = crate::invite::key(code).public();
        let secret = crate::invite::secret(code);
        let (hex, z32) = (id.to_string(), id.to_z32());
        let line = format!(
            "iroh endpoint bound id={} remote_id={} full={hex} PublicKey({}…) dns=_iroh.{z32}.dns.iroh.link \
             upper={} secret={secret} short={}",
            id.fmt_short(),
            &hex[..10],
            &hex[..10],
            hex[..12].to_uppercase(),
            &secret[..9]
        );
        let out = scrub(&line);
        for gone in [&hex[..HIDDEN_PREFIX], &z32[..HIDDEN_PREFIX], &secret[..HIDDEN_PREFIX], &hex[..8].to_uppercase()] {
            assert!(!out.to_ascii_lowercase().contains(&gone.to_ascii_lowercase()), "{gone} in {out}");
        }
        assert!(out.contains("bound id=[invite]") && out.contains("_iroh.[invite].dns"), "{out}");
        // Another endpoint's id stays (cut short, as iroh shows it).
        let other = iroh::SecretKey::generate().public();
        assert!(scrub(&format!("remote_id={}", other.fmt_short())).contains(&other.fmt_short().to_string()));
    }

    #[test]
    fn what_helps_diagnose_stays() {
        let line = "path direct -> relay (rtt 41 ms) relay=https://relay-sfo1.compute.reasonmachines.com/ \
                    remote=7f3a9c01de addr=192.168.1.20:41641 [2001:db8::1]:41641 err=\"timed out\" \
                    mapping_varies_by_dest_ipv4: None conn_id=524305809616 [1, 2, 3]";
        assert_eq!(scrub(line), line);
    }

    #[test]
    fn byte_lists_become_their_length() {
        assert_eq!(
            scrub("ping data=[112, 145, 70, 165, 181, 195, 189, 229] rtt=28ms"),
            "ping data=[<8 bytes>] rtt=28ms"
        );
        assert_eq!(
            scrub("ports [1000, 2000, 3000, 4000, 5000, 6000, 7000, 8000]"),
            "ports [1000, 2000, 3000, 4000, 5000, 6000, 7000, 8000]"
        );
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
        for name in ["secret_key", "auth_token", "ticket", "Password", "payload", "body", "content_len", "key"] {
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
            w.write(&line).unwrap();
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
    fn the_cap_holds_when_the_full_file_cant_be_moved_or_removed() {
        let dir = temp("stuck");
        let path = dir.join("debug.log");
        let mut w = Writer::open(path.clone()).unwrap();
        // `debug.log.1` a non-empty directory: the rename fails.
        std::fs::create_dir_all(dir.join("debug.log.1").join("x")).unwrap();
        let line = vec![b'x'; 1 << 16];
        for _ in 0..(2 * MAX_BYTES / line.len() as u64) {
            w.write(&line).unwrap();
        }
        w.flush();
        assert!(std::fs::metadata(&path).unwrap().len() <= MAX_BYTES);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_flood_is_cut_off_and_counted() {
        let dir = temp("flood");
        let path = dir.join("debug.log");
        let mut w = Writer::open(path.clone()).unwrap();
        for i in 0..1000 {
            w.line(format!("line {i}\n").as_bytes());
        }
        w.tokens = 1.0;
        w.line(b"after\n");
        w.flush();
        let text = std::fs::read_to_string(&path).unwrap();
        assert_eq!(text.lines().filter(|l| l.starts_with("line ")).count(), BURST as usize);
        assert!(text.contains(&format!("({} lines left out: too many at once)", 1000 - BURST as usize)), "{text}");
        assert!(text.ends_with("after\n"));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
