//! The debug log, for working out why connections drop or switch to a relay. Off unless `debug_log = true` in
//! config.toml. The daemon then writes the networking library's own events (iroh and its QUIC, at debug level) and its
//! own network lines to `debug.log` next to `daemon.log`, on this machine only. What could carry message text, files,
//! tickets, links or keys stays out of it: only networking targets are kept, fields named like secrets are dropped, and
//! every line is scrubbed before it is written.

use crate::{paths, util};
use std::{
    fmt,
    fs::{File, OpenOptions},
    io::Write,
    path::{Path, PathBuf},
    sync::{Mutex, OnceLock},
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

/// Where the networking library's events come from: iroh's crates, its QUIC (noq), and the network watchers.
const TARGETS: &[&str] = &["iroh", "noq", "netwatch", "portmapper"];

/// Fields whose names say they may hold a secret or someone's content are left out whole.
const SECRET_FIELDS: &[&str] =
    &["secret", "private", "token", "password", "ticket", "auth", "cookie", "credential", "payload", "body", "content"];

static LOG: OnceLock<Mutex<Writer>> = OnceLock::new();

pub fn path() -> PathBuf {
    paths::home().join("debug.log")
}

/// Turns the log on for this process (the daemon, when its config says so). Says why not if it can't be.
pub fn start() -> Result<(), String> {
    let writer = Writer::open(path()).map_err(|e| format!("can't write {}: {e}", path().display()))?;
    LOG.set(Mutex::new(writer)).map_err(|_| "already on".to_string())?;
    dispatcher::set_global_default(Dispatch::new(Networking)).map_err(|e| e.to_string())
}

/// One of the daemon's own network lines (`what` names the part), when the log is on. Quoted text in it is dropped
/// too: here that can only be an error's echo of what a peer sent.
pub fn ours(what: &str, msg: &str) {
    if LOG.get().is_some() {
        write(&format!("{what}: {}", unquote(msg, 0)));
    }
}

fn write(line: &str) {
    if let Some(log) = LOG.get() {
        let line = format!("[{}] {}\n", util::now_ms(), scrub(&util::one_line(line, 4000)));
        let _ = log.lock().unwrap_or_else(|e| e.into_inner()).write(line.as_bytes());
    }
}

struct Writer {
    path: PathBuf,
    /// None between closing the full file and opening the next.
    file: Option<File>,
    len: u64,
}

impl Writer {
    fn open(path: PathBuf) -> std::io::Result<Writer> {
        let file = open_private(&path)?;
        let len = file.metadata()?.len();
        Ok(Writer { path, file: Some(file), len })
    }

    fn write(&mut self, line: &[u8]) -> std::io::Result<()> {
        if self.len > 0 && self.len + line.len() as u64 > MAX_BYTES {
            // Closed first: Windows doesn't rename a file that is open.
            drop(self.file.take());
            let mut old = self.path.clone().into_os_string();
            old.push(".1");
            if std::fs::rename(&self.path, old).is_err() {
                // Never past the cap, even when the old copy can't be kept.
                let _ = std::fs::remove_file(&self.path);
            }
            self.len = 0;
        }
        let file = match &mut self.file {
            Some(file) => file,
            None => self.file.insert(open_private(&self.path)?),
        };
        file.write_all(line)?;
        self.len += line.len() as u64;
        Ok(())
    }
}

/// Opened for appending, readable by its owner only.
fn open_private(path: &Path) -> std::io::Result<File> {
    let mut opts = OpenOptions::new();
    opts.create(true).append(true);
    #[cfg(unix)]
    std::os::unix::fs::OpenOptionsExt::mode(&mut opts, 0o600);
    let file = opts.open(path)?;
    // A file left from before with wider permissions gets narrowed too.
    #[cfg(unix)]
    util::chmod(path, 0o600)?;
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
            self.message = unquote(&format!("{value:?}"), 64);
        } else if !secret_field(name) {
            self.fields += &format!(" {name}={}", unquote(&format!("{value:?}"), 64));
        }
    }
}

fn secret_field(name: &str) -> bool {
    let name = name.to_ascii_lowercase();
    SECRET_FIELDS.iter().any(|s| name.contains(s)) || (name.contains("key") && !name.contains("public"))
}

/// Drops what is inside each pair of double quotes, unless it is one short word (at most `keep` characters, no
/// spaces): a host or an error name stays, a sentence doesn't.
fn unquote(s: &str, keep: usize) -> String {
    let mut out = String::with_capacity(s.len());
    let mut parts = s.split('"');
    out += parts.next().unwrap_or_default();
    let mut inside = true;
    for part in parts {
        out.push('"');
        if inside && (part.chars().count() > keep || part.chars().any(char::is_whitespace)) {
            out.push('…');
        } else {
            out += part;
        }
        inside = !inside;
    }
    out
}

/// Takes out what could be a secret or someone's content: URLs keep only scheme, host and path (no user, query or
/// fragment, where links carry tickets), tickets go, long hex (ids and keys) is cut to its first 10 characters, and any
/// other long run of letters and digits (encoded keys, tokens, tickets) goes.
pub fn scrub(line: &str) -> String {
    tokens(&urls(line))
}

fn urls(line: &str) -> String {
    let mut out = String::with_capacity(line.len());
    let mut rest = line;
    while let Some(at) = find_url(rest) {
        out += &rest[..at];
        let url = &rest[at..];
        let end = url.find(|c: char| c.is_whitespace() || "\"'<>()[]{},`".contains(c)).unwrap_or(url.len());
        let (scheme, after) = url[..end].split_once("://").unwrap_or(("", &url[..end]));
        let authority_end = after.find(['/', '?', '#']).unwrap_or(after.len());
        let host = after[..authority_end].rsplit('@').next().unwrap_or_default();
        let path = after[authority_end..].split(['?', '#']).next().unwrap_or_default();
        out += &format!("{scheme}://{host}{path}");
        rest = &url[end..];
    }
    out + rest
}

fn find_url(s: &str) -> Option<usize> {
    let lower = s.to_ascii_lowercase();
    [lower.find("https://"), lower.find("http://")].into_iter().flatten().min()
}

fn tokens(line: &str) -> String {
    // `=` splits, so `name=value` is judged by its value; `/` and `+` don't, so base64 can't hide in short pieces.
    let opaque = |c: char| c.is_ascii_alphanumeric() || "-_+/".contains(c);
    let mut out = String::with_capacity(line.len());
    let mut run = String::new();
    let flush = |run: &mut String, out: &mut String| {
        *out += &token(run);
        run.clear();
    };
    for c in line.chars() {
        if opaque(c) {
            run.push(c);
        } else {
            flush(&mut run, &mut out);
            out.push(c);
        }
    }
    flush(&mut run, &mut out);
    out
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
    fn what_helps_diagnose_stays() {
        let line = "path direct -> relay (rtt 41 ms) relay=https://relay-sfo1.compute.reasonmachines.com/ \
                    remote=7f3a9c01de addr=192.168.1.20:41641 [2001:db8::1]:41641 err=\"timed out\" \
                    mapping_varies_by_dest_ipv4: None";
        assert_eq!(scrub(line), line);
    }

    #[test]
    fn quoted_text_goes_unless_it_is_one_short_word() {
        let msg = r#"invalid type: string "please deploy the staging branch", expected u64 host "relay.example""#;
        assert_eq!(unquote(msg, 64), r#"invalid type: string "…", expected u64 host "relay.example""#);
        assert_eq!(unquote(r#"bad frame: "deploy""#, 0), r#"bad frame: "…""#);
    }

    #[test]
    fn fields_named_like_secrets_are_left_out() {
        for name in ["secret_key", "auth_token", "ticket", "Password", "payload", "body", "content_len", "key"] {
            assert!(secret_field(name), "{name}");
        }
        for name in ["remote", "public_key", "path", "rtt", "relay_url", "addr"] {
            assert!(!secret_field(name), "{name}");
        }
    }

    #[test]
    fn only_the_networking_librarys_events_are_kept() {
        use tracing_core::{callsite::Callsite, field::FieldSet, metadata::Kind};
        struct Site;
        impl Callsite for Site {
            fn set_interest(&self, _: Interest) {}
            fn metadata(&self) -> &Metadata<'_> {
                unreachable!()
            }
        }
        static SITE: Site = Site;
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

    #[test]
    fn the_file_is_its_owners_and_starts_over_past_the_cap() {
        let dir = std::env::temp_dir().join(format!("rn-debuglog-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("debug.log");
        let mut w = Writer::open(path.clone()).unwrap();
        let line = vec![b'x'; 1 << 16];
        for _ in 0..(2 * MAX_BYTES / line.len() as u64 + 3) {
            w.write(&line).unwrap();
        }
        let size = |p: &Path| std::fs::metadata(p).map(|m| m.len()).unwrap_or(0);
        let old = dir.join("debug.log.1");
        assert!(size(&path) <= MAX_BYTES && size(&old) <= MAX_BYTES && size(&old) > 0);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            for p in [&path, &old] {
                assert_eq!(std::fs::metadata(p).unwrap().permissions().mode() & 0o777, 0o600, "{}", p.display());
            }
        }
        let _ = std::fs::remove_dir_all(&dir);
    }
}
