use crate::{CLI, paths, util};
use anyhow::{Result, anyhow};
use serde::{Deserialize, Serialize};
use std::{collections::BTreeMap, path::Path};

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct Config {
    /// Your name in the team, e.g. `alex`. Defaults to `$USER`.
    pub person: Option<String>,
    #[serde(skip)]
    pub unreadable: Option<String>,
    #[serde(skip)]
    pub all_folders_off: bool,
    /// `folders` entries whose value can't be read; they count as off.
    #[serde(skip)]
    pub unreadable_folders: Vec<String>,
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
    /// Write the networking library's events to `debug.log`, on this machine only, to work out why connections drop
    /// (read when the daemon starts). Off by default.
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub debug_log: bool,
    /// Durably hold this person's mail for offline permanent mailboxes on their other machines.
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub anchor: bool,
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
            unreadable: None,
            all_folders_off: false,
            unreadable_folders: Vec::new(),
            share_paths: false,
            share_unmanaged: false,
            lan: true,
            tray: true,
            reason: false,
            auto_update: true,
            port: None,
            debug_log: false,
            anchor: false,
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
        let cfg = match std::fs::read_to_string(paths::config()) {
            Ok(text) => Self::read_private(&text),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Self::default(),
            Err(e) => Self::paused(e.to_string()),
        };
        if let Some(warning) = cfg.warning() {
            eprintln!("{warning}");
        }
        cfg
    }

    pub fn warning(&self) -> Option<String> {
        self.unreadable.as_ref().map(|error| {
            let status = if self.all_folders_off {
                "Reason Network is paused"
            } else {
                "Reason Network could not read some settings"
            };
            format!(
                "{status}: ask your user to fix {}, then run `{CLI} stop` and `{CLI} who`: {error}",
                paths::config().display()
            )
        })
    }

    /// What an agent is told in `cwd`, a folder that is off. If the file can't be read at all, or the entry that turns
    /// `cwd` off can't be read, the user turned nothing off, so it is just the warning; else the usual message, with the
    /// warning if some other setting is unreadable. (The daemon refuses such a session with exactly `OFF_HERE`: clients
    /// of earlier releases drop it on reconnect only then.)
    pub fn off_message(&self, cwd: &str) -> String {
        let by_unreadable = self.all_folders_off
            || folder_rule(&self.folders, &home(), cwd)
                .is_some_and(|(entry, _)| self.unreadable_folders.iter().any(|f| f == entry));
        match self.warning() {
            Some(warning) if by_unreadable => warning,
            Some(warning) => format!("{OFF_HERE}; {warning}"),
            None => OFF_HERE.into(),
        }
    }

    fn paused(error: String) -> Self {
        Self {
            unreadable: Some(error),
            all_folders_off: true,
            lan: false,
            share_paths: false,
            share_unmanaged: false,
            inbound: Inbound { default: Policy::Hold, people: BTreeMap::new() },
            ..Self::default()
        }
    }

    fn read_private(text: &str) -> Self {
        if let Ok(cfg) = toml::from_str(text) {
            return cfg;
        }
        let table = match text.parse::<toml::Table>() {
            Ok(table) => table,
            Err(error) => return Self::paused(error.to_string()),
        };
        let mut cfg = Self::default();
        let mut errors = Vec::new();
        macro_rules! key {
            ($name:ident, $fallback:expr) => {
                if let Some(value) = table.get(stringify!($name)) {
                    cfg.$name = value.clone().try_into().unwrap_or_else(|error| {
                        errors.push(format!("{}: {error}", stringify!($name)));
                        $fallback
                    });
                }
            };
        }
        key!(person, None);
        key!(share_paths, false);
        key!(share_unmanaged, false);
        key!(lan, false);
        key!(tray, false);
        key!(reason, false);
        key!(auto_update, true);
        key!(port, None);
        key!(debug_log, false);
        if let Some(value) = table.get("folders") {
            if let Some(folders) = value.as_table() {
                for (folder, value) in folders {
                    let rule = if value.as_str() == Some("off") {
                        "off"
                    } else {
                        errors.push(format!("folders.{folder}: expected \"off\""));
                        cfg.unreadable_folders.push(folder.clone());
                        "off"
                    };
                    cfg.folders.insert(folder.clone(), rule.into());
                }
            } else {
                errors.push("folders: expected a table".into());
                cfg.all_folders_off = true;
            }
        }
        if let Some(value) = table.get("inbound") {
            if let Some(inbound) = value.as_table() {
                if let Some(value) = inbound.get("default") {
                    cfg.inbound.default = value.clone().try_into().unwrap_or_else(|error| {
                        errors.push(format!("inbound.default: {error}"));
                        Policy::Hold
                    });
                }
                // What an unreadable `people` or person counts as: hold, unless the default already refuses.
                let fallback = if cfg.inbound.default == Policy::Refuse { Policy::Refuse } else { Policy::Hold };
                if let Some(value) = inbound.get("people") {
                    if let Some(people) = value.as_table() {
                        for (person, value) in people {
                            let policy = value.clone().try_into().unwrap_or_else(|error| {
                                errors.push(format!("inbound.people.{person}: {error}"));
                                fallback
                            });
                            cfg.inbound.people.insert(person.clone(), policy);
                        }
                    } else {
                        errors.push("inbound.people: expected a table".into());
                        cfg.inbound.default = fallback;
                    }
                }
            } else {
                errors.push("inbound: expected a table".into());
                cfg.inbound.default = Policy::Hold;
            }
        }
        if !errors.is_empty() {
            cfg.lan = false;
            cfg.unreadable = Some(errors.join("; "));
        }
        cfg
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
        if let Some(warning) = self.warning() {
            anyhow::bail!(warning);
        }
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
        if self.all_folders_off {
            return true;
        }
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

/// The `folders` entry that decides for `cwd`: of those that hold it, the deepest. Either path counts as written or by
/// where its links lead (a folder not yet made included), so a link can't take a session out of an off folder.
fn folder_rule<'a>(folders: &'a BTreeMap<String, String>, home: &str, cwd: &str) -> Option<(&'a str, &'a str)> {
    let at = cwd_forms(cwd, home);
    folders
        .iter()
        .filter(|(k, v)| rule_holds(k, v, home))
        .filter_map(|(k, v)| {
            let held = [parts(k, home), entry_parts(k, home)]
                .into_iter()
                .filter(|p| at.iter().any(|a| a.starts_with(p)))
                .map(|p| p.len())
                .max()?;
            Some((k, v, held))
        })
        .max_by_key(|(_, _, depth)| *depth)
        .map(|(k, v, _)| (k.as_str(), v.as_str()))
}

/// `cwd`'s folder names as written, with its links followed, and with only the links above each of its folders followed
/// (`/tmp/personal/link` as `/private/tmp/personal/link`): links above an off folder and inside it can't, together,
/// take a session out.
fn cwd_forms(cwd: &str, home: &str) -> Vec<Vec<String>> {
    let mut forms = vec![parts(cwd, home), entry_parts(cwd, home)];
    let path = expand(cwd, home);
    if full(&path) {
        let path = Path::new(&path);
        for above in path.ancestors().skip(1).filter(|a| a.parent().is_some()) {
            if let Ok(rest) = path.strip_prefix(above) {
                forms.push(parts(&real(above).join(rest).display().to_string(), home));
            }
        }
    }
    forms
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

    /// Runs the test `name` again in a process of its own, with `config` as its config file: `Config::load` and `save`
    /// there use a folder made for it, never the real one. `check` runs in that process, given the folder.
    fn with_config_file(name: &str, config: &str, check: impl FnOnce(&Path)) {
        const CHILD: &str = "REASON_TEST_CONFIG_CHILD";
        if let Some(dir) = std::env::var_os(CHILD) {
            return check(Path::new(&dir));
        }
        let dir = std::env::temp_dir().join(format!("reason-config-{}", ulid::Ulid::generate()));
        std::fs::create_dir(&dir).unwrap();
        std::fs::write(dir.join("config.toml"), config).unwrap();
        let result = std::process::Command::new(std::env::current_exe().unwrap())
            .args(["--exact", &format!("config::tests::{name}"), "--nocapture"])
            .env(CHILD, &dir)
            .env("REASON_NETWORK_HOME", &dir)
            .output();
        let _ = std::fs::remove_dir_all(&dir);
        let output = result.unwrap();
        assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
        assert!(String::from_utf8_lossy(&output.stdout).contains("1 passed"), "{name} did not run");
    }

    #[test]
    fn malformed_people_preserves_explicit_refusal() {
        let config = "[inbound]\ndefault = \"refuse\"\npeople = 1\n";
        with_config_file("malformed_people_preserves_explicit_refusal", config, |_| {
            assert_eq!(Config::load().policy_for("bob"), Policy::Refuse);
        });
    }

    #[test]
    fn an_unreadable_person_is_held_unless_the_default_refuses() {
        let policy = |default: &str, entry: &str| {
            let text = format!("[inbound]\ndefault = \"{default}\"\n[inbound.people]\nalice = \"{entry}\"\n");
            Config::read_private(&text).policy_for("alice")
        };
        for entry in ["refused", "Refuse", "block", "refuse "] {
            assert_eq!(policy("refuse", entry), Policy::Refuse, "{entry}");
            assert_eq!(policy("accept", entry), Policy::Hold, "{entry}");
            assert_eq!(policy("hold", entry), Policy::Hold, "{entry}");
        }
    }

    #[test]
    fn people_that_is_not_a_table_holds_every_sender_unless_the_default_refuses() {
        let policy = |default: &str| {
            let text = format!("[inbound]\ndefault = \"{default}\"\npeople = [\"bob\"]\n");
            Config::read_private(&text).policy_for("bob")
        };
        assert_eq!(policy("accept"), Policy::Hold);
        assert_eq!(policy("hold"), Policy::Hold);
        assert_eq!(policy("refuse"), Policy::Refuse);
    }

    #[test]
    fn a_setting_that_cannot_be_read_turns_lan_off() {
        // `lan = false` added at the end of the file lands in `[folders]`, so the real `lan` is still on.
        assert!(!Config::read_private("[folders]\nlan = false\n").lan);
        assert!(Config::read_private("[folders]\n\"/private\" = \"off\"\n").lan);
    }

    #[test]
    fn a_file_that_parses_loads_as_written() {
        // `on` isn't a value yet: the folder is ignored (`doctor` lists it), not read as unreadable and kept off.
        let cfg = Config::read_private("[folders]\n\"/work\" = \"on\"\n");
        assert!(cfg.warning().is_none() && cfg.lan);
        assert!(!cfg.off_in("/work/api"));
        assert_eq!(cfg.odd_folders().len(), 1);
    }

    #[test]
    fn a_broken_file_keeps_off_folders_off_and_refused_senders_refused() {
        // A setting added at the end of the file lands in `[folders]`, where it can't be read.
        let config =
            "[inbound]\n[inbound.people]\nbob = \"refuse\"\n[folders]\n\"/private\" = \"off\"\ndebug_log = true\n";
        with_config_file("a_broken_file_keeps_off_folders_off_and_refused_senders_refused", config, |_| {
            let cfg = Config::load();
            assert!(cfg.warning().is_some());
            assert!(cfg.off_in("/private/work") && !cfg.off_in("/public"));
            assert_eq!(cfg.policy_for("bob"), Policy::Refuse);
            assert_eq!(cfg.policy_for("carol"), Policy::Accept);
        });
    }

    #[test]
    fn save_refuses_to_overwrite_a_file_it_could_not_fully_read() {
        let config = "# mine\nport = \"41641\"\n";
        with_config_file("save_refuses_to_overwrite_a_file_it_could_not_fully_read", config, |dir| {
            let file = dir.join("config.toml");
            assert!(Config::load().save().is_err());
            assert_eq!(std::fs::read_to_string(&file).unwrap(), config);
            std::fs::write(&file, "lan = false\n").unwrap();
            Config::load().save().unwrap();
            assert!(std::fs::read_to_string(&file).unwrap().contains("[inbound]"));
        });
    }

    #[test]
    fn a_folder_is_said_to_be_turned_off_only_when_the_user_turned_it_off() {
        let says = |text: &str| Config::read_private(text).off_message("/private/work");
        let off = "[folders]\n\"/private\" = \"off\"\n";
        // The file can't be read at all: nothing was turned off.
        for text in ["[inbound", "folders = 3"] {
            let said = says(text);
            assert!(said.starts_with("Reason Network is paused: ask your user to fix"), "{text}: {said}");
            assert!(!said.contains(OFF_HERE), "{text}: {said}");
        }
        assert_eq!(says(off), OFF_HERE);
        let said = says(&format!("port = \"41641\"\n{off}"));
        assert!(said.starts_with(&format!("{OFF_HERE}; Reason Network could not read some settings:")), "{said}");
        // A folder that is off only because its own entry can't be read wasn't turned off by the user either.
        let said = says("[folders]\n\"/private\" = true\n");
        assert!(said.starts_with("Reason Network could not read some settings: ask your user to fix"), "{said}");
        assert!(!said.contains(OFF_HERE), "{said}");
        // Unless a folder the user turned off is the one that decides.
        let nested = "[folders]\n\"/private\" = true\n\"/private/work\" = \"off\"\n";
        assert!(says(nested).starts_with(OFF_HERE));
        assert!(Config::read_private(nested).off_message("/private/other").starts_with("Reason Network could not"));
    }

    #[test]
    fn bad_syntax_pauses_folders_and_holds_mail_without_disabling_updates() {
        // Settings it turns on before the line that breaks it can't be trusted either.
        let on = "lan = true\nshare_paths = true\nshare_unmanaged = true\nreason = true\nauto_update = false\n[inbound";
        for text in [on, "[inbound", "[inbound]\n[inbound]", "folders = 3"] {
            let cfg = Config::read_private(text);
            assert!(cfg.off_in("/anywhere"));
            if text != "folders = 3" {
                assert_eq!(cfg.policy_for("alice"), Policy::Hold);
            }
            assert!(cfg.auto_update);
            assert!(!cfg.lan && !cfg.share_paths && !cfg.share_unmanaged && !cfg.reason, "{text}");
            assert!(cfg.warning().is_some());
        }
    }

    #[test]
    fn a_setting_that_cannot_be_read_never_turns_sharing_on_or_updates_off() {
        for key in ["lan", "share_paths", "share_unmanaged", "reason", "debug_log"] {
            let cfg = Config::read_private(&format!("{key} = \"yes\"\n"));
            assert!(cfg.warning().is_some(), "{key}");
            assert!(!cfg.lan && !cfg.share_paths && !cfg.share_unmanaged && !cfg.reason && !cfg.debug_log, "{key}");
        }
        assert!(Config::read_private("auto_update = \"no\"\n").auto_update);
    }

    #[test]
    fn an_inbound_that_is_not_a_table_holds_every_sender() {
        assert_eq!(Config::read_private("inbound = 3\n").policy_for("bob"), Policy::Hold);
    }

    #[test]
    fn a_file_that_cannot_be_read_pauses() {
        with_config_file("a_file_that_cannot_be_read_pauses", "", |dir| {
            // Not UTF-8, so not text at all.
            std::fs::write(dir.join("config.toml"), b"lan = true\n\xff\n").unwrap();
            let cfg = Config::load();
            assert!(cfg.warning().is_some_and(|w| w.starts_with("Reason Network is paused")), "{:?}", cfg.warning());
            assert!(cfg.off_in("/work") && !cfg.lan);
            assert_eq!(cfg.policy_for("bob"), Policy::Hold);
        });
    }

    #[test]
    fn missing_settings_and_unknown_keys_keep_normal_defaults() {
        for text in ["", "future_setting = true"] {
            let cfg = Config::read_private(text);
            assert!(!cfg.off_in("/work"));
            assert_eq!(cfg.policy_for("alice"), Policy::Accept);
            assert!(cfg.warning().is_none());
        }
    }

    #[test]
    fn typed_errors_preserve_other_privacy_settings() {
        let cfg = Config::read_private(
            r#"port = "41641"
lan = "yes"
share_paths = 5
[inbound]
default = "wrong"
[inbound.people]
alice = "refuse"
bob = false
[folders]
"/private" = "off"
"/other" = 7
"#,
        );
        assert!(cfg.off_in("/private/work"));
        assert!(cfg.off_in("/other/work"));
        assert!(!cfg.off_in("/public"));
        assert_eq!(cfg.policy_for("alice"), Policy::Refuse);
        assert_eq!(cfg.policy_for("bob"), Policy::Hold);
        assert_eq!(cfg.policy_for("carol"), Policy::Hold);
        assert!(!cfg.lan && !cfg.share_paths);
        assert_eq!(cfg.port(), None);
        assert!(cfg.auto_update);
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

    #[test]
    #[cfg(unix)]
    fn a_link_inside_an_off_folder_does_not_take_its_sessions_out() {
        let d = temp();
        let personal = d.join("real").join("personal");
        std::os::unix::fs::symlink(d.join("x"), personal.join("link")).unwrap();
        let cfg = Config { folders: folders(&[(&personal.display().to_string(), "off")]), ..Config::default() };
        // As written it is in the off folder, though it leads out of it; and the folder it leads to stays on.
        assert!(cfg.off_in(&personal.join("link").join("work").display().to_string()));
        assert!(cfg.off_in(&personal.join("link").display().to_string()));
        assert!(!cfg.off_in(&d.join("x").join("work").display().to_string()));
        std::fs::remove_dir_all(&d).unwrap();
    }

    #[test]
    #[cfg(unix)]
    fn a_link_above_an_off_folder_and_one_inside_it_do_not_take_its_sessions_out() {
        let d = temp();
        let personal = d.join("real").join("personal");
        std::os::unix::fs::symlink(d.join("x"), personal.join("link")).unwrap();
        // `/tmp` for `/private/tmp`, or a linked home: the folder as `reason-network folder` saves it, links followed.
        std::os::unix::fs::symlink(d.join("real"), d.join("outer")).unwrap();
        let entry = real(&personal).display().to_string();
        let cfg = Config { folders: folders(&[(&entry, "off")]), ..Config::default() };
        assert!(cfg.off_in(&d.join("outer").join("personal").join("link").join("work").display().to_string()));
        assert!(!cfg.off_in(&d.join("outer").join("x").display().to_string()));
        std::fs::remove_dir_all(&d).unwrap();
    }

    #[test]
    #[cfg(unix)]
    fn a_session_not_yet_made_under_a_link_is_held_by_where_the_link_leads() {
        let d = temp();
        std::os::unix::fs::symlink(d.join("real").join("personal"), d.join("link")).unwrap();
        let entry = d.join("real").join("personal").display().to_string();
        let cfg = Config { folders: folders(&[(&entry, "off")]), ..Config::default() };
        // `new` doesn't exist, so the folder can't be looked up whole; the link above it still leads into `personal`.
        assert!(cfg.off_in(&d.join("link").join("new").join("deeper").display().to_string()));
        std::fs::remove_dir_all(&d).unwrap();
    }
}
