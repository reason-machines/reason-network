use crate::{paths, util};
use anyhow::{Result, anyhow};
use serde::{Deserialize, Serialize};
use std::{collections::BTreeMap, path::Path};

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct Config {
    /// Your name in the team, e.g. `alex`. Defaults to `$USER`.
    pub person: Option<String>,
    /// Show teammates full working directories (otherwise only the last component, plus repo@branch).
    pub share_paths: bool,
    /// Show teammates this machine's Claude Code sessions that don't have Reason Network loaded.
    pub share_unmanaged: bool,
    /// Find teammates on the local network with mDNS.
    pub lan: bool,
    /// Show the menu bar icon (macOS, while the background service runs).
    pub tray: bool,
    /// List your live Reason sessions and deliver to them (`reason session list`/`send`).
    pub reason: bool,
    /// Install new releases when they come out (the daemon checks every few hours).
    pub auto_update: bool,
    /// The UDP port teammates reach this machine on directly, e.g. `41641` for a firewall or router rule. None: a
    /// random one each start. Taken by something else, a random one is used.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub port: Option<u16>,
    pub inbound: Inbound,
    /// Folders whose sessions stay off the network, e.g. `"~/personal" = "off"` (`reason-network folder`). The deepest
    /// entry that holds a session's folder decides; `off` is the only value so far.
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    pub folders: BTreeMap<String, String>,
}

/// What an agent's tools say in a folder whose sessions are off the network.
pub const OFF_HERE: &str = "Your user turned Reason Network off for this folder";

/// What a call naming no session gets, from an agent whose sessions share one MCP server, while one of them works in a
/// folder that is off (the call may be that one's).
pub const BUSY_OFF: &str = "Reason Network can't tell which session this call comes from while another session of this \
                            agent works in a folder where it is off; try again in a few minutes";

/// What happens to messages from teammates' agents: `accept` delivers them, `hold` waits for you to approve
/// (`reason-network held`), `refuse` bounces them.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct Inbound {
    pub default: Policy,
    pub people: BTreeMap<String, Policy>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Policy {
    #[default]
    Accept,
    Hold,
    Refuse,
}

impl Default for Config {
    fn default() -> Self {
        Config {
            person: None,
            share_paths: false,
            share_unmanaged: false,
            lan: true,
            tray: true,
            reason: false,
            auto_update: true,
            port: None,
            inbound: Inbound::default(),
            folders: BTreeMap::new(),
        }
    }
}

impl Default for Inbound {
    fn default() -> Self {
        Inbound { default: Policy::Accept, people: BTreeMap::new() }
    }
}

impl Config {
    pub fn load() -> Self {
        Self::load_strict().unwrap_or_else(|e| {
            eprintln!("reason-network: ignoring {} (fix or delete it): {e}", paths::config().display());
            Config::default()
        })
    }

    /// Like `load`, but a file that doesn't parse is an error (before rewriting it, say).
    pub fn load_strict() -> Result<Self> {
        match std::fs::read_to_string(paths::config()) {
            Ok(s) => Ok(toml::from_str(&s)?),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Config::default()),
            Err(e) => Err(e.into()),
        }
    }

    pub fn save(&self) -> Result<()> {
        paths::ensure_home()?;
        std::fs::write(paths::config(), toml::to_string_pretty(self)?)?;
        Ok(())
    }

    pub fn person(&self) -> String {
        self.person
            .clone()
            .or_else(|| std::env::var(if cfg!(windows) { "USERNAME" } else { "USER" }).ok())
            .map(|s| util::slug(&s))
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| "me".into())
    }

    pub fn policy_for(&self, person: &str) -> Policy {
        self.inbound.people.get(person).copied().unwrap_or(self.inbound.default)
    }

    pub fn lan(&self) -> bool {
        self.lan && crate::paths::env_var_os("NO_MDNS").is_none()
    }

    /// The fixed UDP port, if one is set; `0` (any port) is the same as none.
    pub fn port(&self) -> Option<u16> {
        self.port.filter(|&p| p != 0)
    }

    pub fn tray(&self) -> bool {
        cfg!(target_os = "macos") && self.tray && crate::paths::env_var_os("NO_TRAY").is_none()
    }

    /// Whether sessions working in `cwd` stay off the network.
    pub fn off_in(&self, cwd: &str) -> bool {
        if self.folders.is_empty() {
            return false;
        }
        let off = |cwd: &str| folder_rule(&self.folders, &home(), cwd).is_some_and(|(_, v)| v == "off");
        // An agent may report the folder by a path with a link in it (`/var/…` for macOS's `/private/var/…`).
        off(cwd) || std::fs::canonicalize(cwd).is_ok_and(|real| off(&real.display().to_string()))
    }

    /// `folders` entries this version ignores: a value other than `off`, or not a full path.
    pub fn odd_folders(&self) -> Vec<(&String, &String)> {
        self.folders.iter().filter(|(k, v)| !rule_holds(k, v, &home())).collect()
    }
}

fn home() -> String {
    paths::user_home().to_string_lossy().into_owned()
}

/// The `folders` entry that decides for `cwd`: of those that hold it, the deepest.
fn folder_rule<'a>(folders: &'a BTreeMap<String, String>, home: &str, cwd: &str) -> Option<(&'a str, &'a str)> {
    let at = parts(cwd, home);
    folders
        .iter()
        .filter(|(k, v)| rule_holds(k, v, home))
        .map(|(k, v)| (k, v, entry_parts(k, home)))
        .filter(|(_, _, p)| at.starts_with(p))
        .max_by_key(|(_, _, p)| p.len())
        .map(|(k, v, _)| (k.as_str(), v.as_str()))
}

fn rule_holds(folder: &str, value: &str, home: &str) -> bool {
    value == "off" && full(&expand(folder, home))
}

/// `~` is the home folder.
fn expand(path: &str, home: &str) -> String {
    match path.trim().strip_prefix('~') {
        Some(rest) if rest.is_empty() || rest.starts_with(['/', '\\']) => format!("{home}{rest}"),
        _ => path.trim().to_string(),
    }
}

/// `/…`, `\\…`, or `C:\…`, whichever system it is on.
fn full(path: &str) -> bool {
    let b = path.as_bytes();
    path.starts_with(['/', '\\'])
        || (b.len() > 2 && b[0].is_ascii_alphabetic() && b[1] == b':' && matches!(b[2], b'/' | b'\\'))
}

/// A path's folder names, `~` expanded, either slash, `..` taken back; without case where the file system ignores it.
fn parts(path: &str, home: &str) -> Vec<String> {
    let path = expand(path, home);
    let path = path.strip_prefix(r"\\?\").unwrap_or(&path);
    let mut out: Vec<String> = Vec::new();
    for p in path.split(['/', '\\']).filter(|p| !p.is_empty() && *p != ".") {
        if p == ".." {
            out.pop();
        } else {
            out.push(if cfg!(any(windows, target_os = "macos")) { p.to_lowercase() } else { p.to_string() });
        }
    }
    out
}

/// An entry's folder names, as the folder really is: a link in it followed (`~/personal` → a synced folder).
fn entry_parts(folder: &str, home: &str) -> Vec<String> {
    let path = expand(folder, home);
    if !full(&path) {
        return parts(&path, home);
    }
    parts(&real(Path::new(&path)).display().to_string(), home)
}

/// Sets `folder`'s entry in the config file, keeping the rest of the file as it is: `off`, or none (`on`). Returns the
/// entry's name (`~/personal`), and whether that changes which sessions are off.
pub fn set_folder(folder: &str, off: bool) -> Result<(String, bool)> {
    let home = home();
    let path = real(&std::path::absolute(expand(folder, &home))?).display().to_string();
    let name = util::tilde(path.strip_prefix(r"\\?\").unwrap_or(&path));
    let file = paths::config();
    let text = match std::fs::read_to_string(&file) {
        Ok(s) => s,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => String::new(),
        Err(e) => return Err(e.into()),
    };
    let mut doc: toml_edit::DocumentMut = text.parse().map_err(|e| anyhow!("fix {} first: {e}", file.display()))?;
    let table = doc.entry("folders").or_insert(toml_edit::table());
    let table =
        table.as_table_like_mut().ok_or_else(|| anyhow!("fix {} first: `folders` is a table", file.display()))?;
    // The same folder, however it was written.
    let same: Vec<String> =
        table.iter().map(|(k, _)| k.to_string()).filter(|k| entry_parts(k, &home) == parts(&name, &home)).collect();
    let was_off = same.iter().any(|k| table.get(k).and_then(|v| v.as_str()) == Some("off"));
    for k in &same {
        table.remove(k);
    }
    if off {
        table.insert(&name, toml_edit::value("off"));
    }
    if table.is_empty() {
        doc.remove("folders");
    }
    if doc.to_string() != text {
        paths::ensure_home()?;
        std::fs::write(&file, doc.to_string())?;
    }
    Ok((name, was_off != off))
}

/// `path` as agents report their folder, links followed (on macOS, /tmp is /private/tmp), even before it exists.
fn real(path: &Path) -> std::path::PathBuf {
    let mut rest = Vec::new();
    let mut at = path;
    // A root alone has nothing to follow (and on Windows `/` would come back as `C:\`).
    while at.parent().is_some() {
        if let Ok(r) = at.canonicalize() {
            return rest.iter().rev().fold(r, |p, n| p.join(n));
        }
        match (at.parent(), at.file_name()) {
            (Some(p), Some(n)) => {
                rest.push(n);
                at = p;
            }
            _ => break,
        }
    }
    path.to_path_buf()
}

/// The entry that keeps `folder` off when it has none of its own (`~/personal` for `~/personal/notes`).
pub fn off_by(folder: &str) -> Option<String> {
    let cfg = Config::load();
    let (k, v) = folder_rule(&cfg.folders, &home(), folder)?;
    (v == "off").then(|| k.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn folders(entries: &[(&str, &str)]) -> BTreeMap<String, String> {
        entries.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect()
    }

    #[test]
    fn the_deepest_folder_that_holds_a_session_decides() {
        let f = folders(&[("~/work", "off"), ("/Users/alex/work/oss", "off"), ("/srv", "off")]);
        let rule = |cwd: &str| folder_rule(&f, "/Users/alex", cwd).map(|(k, _)| k);
        assert_eq!(rule("/Users/alex/work/oss/engine"), Some("/Users/alex/work/oss"));
        assert_eq!(rule("/Users/alex/work/api/"), Some("~/work"));
        assert_eq!(rule("/Users/alex/work"), Some("~/work"));
        assert_eq!(rule("/srv/./x"), Some("/srv"));
        // Whole folder names only.
        assert_eq!(rule("/Users/alex/workshop"), None);
        assert_eq!(rule("/Users/alex"), None);
        assert_eq!(rule("/"), None);
    }

    #[test]
    fn home_and_windows_paths() {
        let home = r"C:\Users\alex";
        let f = folders(&[("~/Personal", "off"), (r"D:\scratch", "off")]);
        let off = |cwd: &str| folder_rule(&f, home, cwd).is_some();
        assert!(off(r"C:\Users\alex\Personal\taxes"));
        assert!(off("C:/Users/alex/Personal"));
        assert!(off(r"\\?\C:\Users\alex\Personal"));
        assert!(off(r"D:\scratch\x") && !off(r"D:\scratchpad") && !off(r"C:\Users\alex"));
        // `~` alone is the home folder; `~alex` is no one's.
        let f = folders(&[("~", "off"), ("~alex", "off")]);
        assert!(folder_rule(&f, "/h/alex", "/h/alex/x").is_some());
        assert!(folder_rule(&f, "/h/alex", "/h/bob").is_none());
    }

    #[test]
    fn only_off_counts_and_the_rest_is_ignored() {
        let f = folders(&[("~/work", "off"), ("~/work/team", "work"), ("relative", "off")]);
        let cfg = Config { folders: f.clone(), ..Config::default() };
        // `work` (a network to come) isn't understood yet: the folder above it decides.
        assert_eq!(folder_rule(&f, "/h/alex", "/h/alex/work/team/x"), Some(("~/work", "off")));
        assert!(folder_rule(&f, "/h/alex", "/relative").is_none());
        assert_eq!(cfg.odd_folders().len(), 2);
        // No entries: nothing is off.
        assert!(!Config::default().off_in("/h/alex/work"));
        let parsed: Config = toml::from_str("[folders]\n\"~/personal\" = \"off\"\n").unwrap();
        assert!(parsed.off_in(&format!("{}/personal/x", home())));
    }

    #[test]
    fn a_fixed_port_is_optional() {
        let parsed: Config = toml::from_str("lan = true\n").unwrap();
        assert_eq!(parsed.port, None);
        assert!(!toml::to_string_pretty(&parsed).unwrap().contains("port"), "unset: not written");
        let parsed: Config = toml::from_str("port = 41641\n").unwrap();
        assert_eq!(parsed.port, Some(41641));
        assert!(toml::to_string_pretty(&parsed).unwrap().contains("port = 41641"));
        assert_eq!(parsed.port(), Some(41641));
        let parsed: Config = toml::from_str("port = 0\n").unwrap();
        assert_eq!(parsed.port(), None, "0: random, as unset");
        for bad in ["port = 70000", "port = -1", "port = \"41641\""] {
            assert!(toml::from_str::<Config>(bad).is_err(), "{bad}");
        }
    }

    fn temp() -> std::path::PathBuf {
        let d = std::env::temp_dir().join(format!("reason-network-folders-{}", ulid::Ulid::generate()));
        std::fs::create_dir_all(d.join("real").join("personal").join("sub")).unwrap();
        std::fs::create_dir_all(d.join("x")).unwrap();
        d
    }

    #[test]
    fn an_entry_written_with_dot_dot_holds_the_folder_it_names() {
        let d = temp();
        let entry = d.join("x").join("..").join("real").join("personal").display().to_string();
        let cfg = Config { folders: folders(&[(&entry, "off")]), ..Config::default() };
        assert!(cfg.off_in(&d.join("real").join("personal").join("sub").display().to_string()));
        assert!(!cfg.off_in(&d.join("x").display().to_string()));
        // Even before the folder exists.
        let cfg = Config { folders: folders(&[("/h/alex/x/../personal", "off")]), ..Config::default() };
        assert!(cfg.off_in("/h/alex/personal/notes") && !cfg.off_in("/h/alex/x"));
        std::fs::remove_dir_all(&d).unwrap();
    }

    #[test]
    #[cfg(unix)]
    fn an_entry_that_is_a_link_holds_the_folder_it_points_to() {
        let d = temp();
        std::os::unix::fs::symlink(d.join("real").join("personal"), d.join("link")).unwrap();
        let cfg = Config { folders: folders(&[(&d.join("link").display().to_string(), "off")]), ..Config::default() };
        // Sessions report the real folder, or the link.
        assert!(cfg.off_in(&d.join("real").join("personal").join("sub").display().to_string()));
        assert!(cfg.off_in(&d.join("link").join("sub").display().to_string()));
        assert!(!cfg.off_in(&d.join("real").display().to_string()));
        std::fs::remove_dir_all(&d).unwrap();
    }
}
