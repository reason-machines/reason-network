//! Where Reason Network keeps its state: `~/.reason/network`, or `REASON_NETWORK_HOME` (handy for running two
//! "machines" side by side). On Windows, `~` is the profile folder.
//!
//! Builds before 0.10.1 kept it in `~/.<LEGACY_NAME>` (or `TINCAN_HOME`) as `<LEGACY_NAME>.db` and `.sock`. [`migrate`]
//! moves it here once, with the daemon that holds it stopped. Until that has happened, every path below still names
//! the old state, so a machine never loses its team.

use crate::LEGACY_NAME;
use std::path::{Path, PathBuf};

pub fn user_home() -> PathBuf {
    std::env::var_os(if cfg!(windows) { "USERPROFILE" } else { "HOME" })
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("/"))
}

/// A setting from the environment: `REASON_NETWORK_<name>`, else the name earlier builds read (which earlier Reason
/// CLIs and agent configs set).
pub fn env_var(name: &str) -> Result<String, std::env::VarError> {
    std::env::var(format!("REASON_NETWORK_{name}")).or_else(|_| std::env::var(legacy_env(name)))
}

pub fn env_var_os(name: &str) -> Option<std::ffi::OsString> {
    std::env::var_os(format!("REASON_NETWORK_{name}")).or_else(|| std::env::var_os(legacy_env(name)))
}

fn legacy_env(name: &str) -> String {
    format!("{}_{name}", LEGACY_NAME.to_uppercase())
}

fn default_home() -> PathBuf {
    user_home().join(".reason").join("network")
}

fn legacy_home() -> PathBuf {
    user_home().join(format!(".{LEGACY_NAME}"))
}

pub fn home() -> PathBuf {
    if let Some(h) = env_var_os("HOME") {
        return h.into();
    }
    let home = default_home();
    if !has_db(&home) && has_db(&legacy_home()) {
        return legacy_home();
    }
    home
}

fn has_db(home: &Path) -> bool {
    home.join("network.db").exists() || home.join(format!("{LEGACY_NAME}.db")).exists()
}

/// What the state files are called in `home`: `network`, or the old name while they haven't been moved.
fn stem(home: &Path) -> &'static str {
    if !home.join("network.db").exists() && home.join(format!("{LEGACY_NAME}.db")).exists() {
        LEGACY_NAME
    } else {
        "network"
    }
}

/// `~/.reason/network/network.sock`, unless that is too long for a Unix socket address (104 bytes on macOS): then a
/// short path in a private `/tmp/reason-network-<uid>` directory, derived from the home directory so every client
/// agrees on it.
#[cfg(unix)]
pub fn socket() -> PathBuf {
    socket_in(&home())
}

#[cfg(unix)]
fn socket_in(home: &Path) -> PathBuf {
    let stem = stem(home);
    let path = home.join(format!("{stem}.sock"));
    if path.as_os_str().len() < 100 {
        return path;
    }
    short_dir(stem).join(format!("{:016x}.sock", hash(&path)))
}

/// A named pipe, named for the home directory so every client agrees on it.
#[cfg(windows)]
pub fn socket() -> PathBuf {
    pipe_in(&home())
}

/// Named for the directory as Windows knows it, not as it was spelled: `C:/Users/alex/.reason/network` and
/// `c:\users\alex\.reason\network\` are one directory, with one lock file, so they must be one pipe. (Before the
/// directory exists, as spelled; the daemon creating it names the pipe by what it then finds.)
#[cfg(windows)]
fn pipe_in(home: &Path) -> PathBuf {
    let stem = if stem(home) == LEGACY_NAME { LEGACY_NAME } else { "reason-network" };
    let dir = home.canonicalize().unwrap_or_else(|_| home.to_path_buf());
    let dir = dir.to_str().and_then(|s| s.strip_prefix(r"\\?\")).map_or(dir.clone(), PathBuf::from);
    PathBuf::from(format!(r"\\.\pipe\{stem}-{:016x}", hash(&dir)))
}

/// FNV-1a: stable across builds, unlike `DefaultHasher`.
fn hash(path: &Path) -> u64 {
    path.as_os_str()
        .as_encoded_bytes()
        .iter()
        .fold(0xcbf29ce484222325u64, |h, &b| (h ^ b as u64).wrapping_mul(0x100000001b3))
}

#[cfg(unix)]
fn short_dir(stem: &str) -> PathBuf {
    let name = if stem == LEGACY_NAME { LEGACY_NAME } else { "reason-network" };
    PathBuf::from(format!("/tmp/{name}-{}", unsafe { libc::getuid() }))
}

/// Moves state an earlier build left under the old names (see the module docs), once. Every command runs it first;
/// it costs a stat or two when there is nothing to move. One process moves while the others wait for it, and anything
/// that goes wrong leaves the old state in place, where [`home`] keeps finding it, to be moved next time.
pub fn migrate() {
    #[cfg(unix)]
    let _ = migrate_unix();
}

#[cfg(unix)]
fn migrate_unix() -> std::io::Result<()> {
    let explicit = env_var_os("HOME").map(PathBuf::from);
    let to = explicit.clone().unwrap_or_else(default_home);
    let legacy = legacy_home();
    // The old folder itself (not the link left in its place), holding a team this folder doesn't have yet.
    let folder = || {
        explicit.is_none()
            && std::fs::symlink_metadata(&legacy).is_ok_and(|m| m.is_dir())
            && legacy.join(format!("{LEGACY_NAME}.db")).exists()
            && !has_db(&to)
    };
    let files = || stem(&to) == LEGACY_NAME;
    if !folder() && !files() {
        return Ok(());
    }
    let dir = user_home().join(".reason");
    std::fs::create_dir_all(&dir)?;
    let Some(_moving) = crate::util::lock(&dir.join(".network-move.lock"), true)? else { return Ok(()) };
    if folder() {
        stop_daemon_in(&legacy)?;
        // A folder in the way holds no team (`folder` checked): only an empty one goes, anything else stops the move.
        let _ = std::fs::remove_dir(&to);
        std::fs::rename(&legacy, &to)?;
    }
    if files() {
        stop_daemon_in(&to)?;
        // Folding the write-ahead log into the database first makes the move one rename.
        let old = to.join(format!("{LEGACY_NAME}.db"));
        rusqlite::Connection::open(&old)
            .and_then(|db| db.query_row("PRAGMA wal_checkpoint(TRUNCATE)", [], |_| Ok(())))
            .map_err(std::io::Error::other)?;
        std::fs::rename(&old, to.join("network.db"))?;
        for leftover in ["db-wal", "db-shm", "sock"] {
            let _ = std::fs::remove_file(to.join(format!("{LEGACY_NAME}.{leftover}")));
        }
        // The earlier build's log talks about its own names; the moved state starts a fresh one.
        let _ = std::fs::remove_file(to.join("daemon.log"));
    }
    // Agent sessions opened before the update keep running the earlier build's MCP server, which looks for the old
    // folder (and the daemon's service may still log into it): a link there keeps them working until they restart.
    if explicit.is_none() && !legacy.exists() && to.is_dir() {
        let _ = std::fs::remove_dir(&legacy);
        let _ = std::os::unix::fs::symlink(&to, &legacy);
    }
    Ok(())
}

/// For the same sessions, the socket under its old name too.
#[cfg(unix)]
pub fn link_legacy_socket(sock: &Path) {
    if sock.file_name().is_some_and(|n| n == "network.sock") {
        let old = sock.with_file_name(format!("{LEGACY_NAME}.sock"));
        let _ = std::fs::remove_file(&old);
        let _ = std::os::unix::fs::symlink("network.sock", old);
    }
}

/// Asks the daemon holding `home` (an earlier build, on the old names) to exit, and waits until it has.
#[cfg(unix)]
fn stop_daemon_in(home: &Path) -> std::io::Result<()> {
    use std::io::Write;
    let lock = home.join("daemon.lock");
    if !crate::util::locked(&lock) {
        return Ok(());
    }
    if let Ok(mut s) = std::os::unix::net::UnixStream::connect(socket_in(home)) {
        let _ = s.write_all(b"{\"id\":1,\"op\":\"shutdown\"}\n");
    }
    for _ in 0..100 {
        if !crate::util::locked(&lock) {
            return Ok(());
        }
        std::thread::sleep(std::time::Duration::from_millis(100));
    }
    Err(std::io::Error::other("the daemon on the old state didn't stop"))
}

/// Makes sure the socket's directory exists and that nobody else can get at it.
#[cfg(unix)]
pub fn ensure_socket_dir() -> std::io::Result<()> {
    use std::os::unix::fs::{DirBuilderExt, MetadataExt};
    let dir = socket().parent().map(PathBuf::from).unwrap_or_else(home);
    if dir != short_dir("network") && dir != short_dir(LEGACY_NAME) {
        return ensure_home();
    }
    let _ = std::fs::DirBuilder::new().mode(0o700).create(&dir);
    let meta = std::fs::symlink_metadata(&dir)?;
    if !meta.is_dir() || meta.uid() != unsafe { libc::getuid() } || meta.mode() & 0o077 != 0 {
        return Err(std::io::Error::other(format!("{} is not a private directory of yours; remove it", dir.display())));
    }
    Ok(())
}

pub fn db() -> PathBuf {
    let home = home();
    home.join(format!("{}.db", stem(&home)))
}

pub fn config() -> PathBuf {
    home().join("config.toml")
}

pub fn log() -> PathBuf {
    home().join("daemon.log")
}

/// Files that came with messages, one folder per message.
pub fn inbox() -> PathBuf {
    home().join("inbox")
}

pub fn lock() -> PathBuf {
    home().join("daemon.lock")
}

/// Claude Code's own directory (`CLAUDE_CONFIG_DIR`, else `~/.claude`).
pub fn claude_dir() -> PathBuf {
    std::env::var_os("CLAUDE_CONFIG_DIR").map(PathBuf::from).unwrap_or_else(|| user_home().join(".claude"))
}

/// Claude Code's registry of live sessions, one `<pid>.json` per session.
pub fn claude_sessions() -> PathBuf {
    claude_dir().join("sessions")
}

/// Every Claude Code directory on this machine: ours, `~/.claude`, and each `CLAUDE_CONFIG_DIR` a session ran under.
/// The daemon (and a CLI run from another shell) doesn't have the session's environment, so it reads them all.
pub fn claude_dirs() -> Vec<PathBuf> {
    let recorded = std::fs::read_to_string(claude_dirs_file()).unwrap_or_default();
    let mut dirs = vec![claude_dir(), user_home().join(".claude")];
    dirs.extend(recorded.lines().filter(|l| !l.is_empty()).map(PathBuf::from).filter(|d| d.is_dir()));
    let mut seen = std::collections::HashSet::new();
    dirs.retain(|d| seen.insert(d.clone()));
    dirs
}

/// Notes the `CLAUDE_CONFIG_DIR` we run under, so the daemon finds this profile's sessions too.
pub fn remember_claude_dir() {
    let Some(dir) = std::env::var_os("CLAUDE_CONFIG_DIR").filter(|d| !d.is_empty()) else { return };
    let Some(dir) = std::path::absolute(dir).ok().and_then(|d| d.to_str().map(str::to_string)) else { return };
    let file = claude_dirs_file();
    let recorded = std::fs::read_to_string(&file).unwrap_or_default();
    if dir.contains('\n') || recorded.lines().any(|l| l == dir) || ensure_home().is_err() {
        return;
    }
    use std::io::Write;
    let _ = std::fs::OpenOptions::new().create(true).append(true).open(file).and_then(|mut f| writeln!(f, "{dir}"));
}

fn claude_dirs_file() -> PathBuf {
    home().join("claude-dirs")
}

/// Only for you: mode 0700 (on Windows, the profile folder already is).
pub fn ensure_home() -> std::io::Result<()> {
    let mut dir = std::fs::DirBuilder::new();
    #[cfg(unix)]
    std::os::unix::fs::DirBuilderExt::mode(&mut dir, 0o700);
    dir.recursive(true).create(home())
}

#[cfg(test)]
mod tests {
    #[test]
    #[cfg(windows)]
    fn every_spelling_of_the_home_names_one_pipe() {
        let dir = std::env::temp_dir().join(format!("reason-network-home-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let spelled = dir.to_str().unwrap();
        let slashes = spelled.replace('\\', "/");
        let shouting = format!("{}\\", spelled.to_uppercase());
        assert_eq!(super::pipe_in(&dir), super::pipe_in(std::path::Path::new(&slashes)));
        assert_eq!(super::pipe_in(&dir), super::pipe_in(std::path::Path::new(&shouting)));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
