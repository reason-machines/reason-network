//! Claude Code keeps one `~/.claude/sessions/<pid>.json` per live session (name, cwd, busy/idle), under
//! `CLAUDE_CONFIG_DIR` when that is set. Reading them lists every Claude session on this machine, including ones that
//! don't have Reason Network loaded.

use crate::{agent::ClaudeSocket, paths, util};
use serde::Deserialize;

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ClaudeSession {
    pub pid: u32,
    pub session_id: Option<String>,
    pub cwd: Option<String>,
    pub name: Option<String>,
    /// `user` after a `/rename` (`derived` for the default name).
    pub name_source: Option<String>,
    pub status: Option<String>,
    pub status_updated_at: Option<i64>,
    pub started_at: Option<i64>,
    /// Where the session takes messages from peers (what SendMessage uses).
    pub messaging_socket_path: Option<String>,
}

pub fn scan() -> Vec<ClaudeSession> {
    scan_dirs(&paths::claude_dirs())
}

fn scan_dirs(dirs: &[std::path::PathBuf]) -> Vec<ClaudeSession> {
    let mut seen = std::collections::HashSet::new();
    dirs.iter()
        .filter_map(|d| std::fs::read_dir(d.join("sessions")).ok())
        .flat_map(|dir| dir.flatten())
        .map(|e| e.path())
        .filter(|p| p.extension().is_some_and(|x| x == "json"))
        .filter_map(|p| serde_json::from_str::<ClaudeSession>(&std::fs::read_to_string(p).ok()?).ok())
        .filter(|s| util::pid_alive(s.pid) && seen.insert(s.pid))
        .collect()
}

impl ClaudeSession {
    /// A short name for the directory: Claude Code's own, or, when it named the session after its title (`auto`),
    /// the first words of it (the whole title shows as what it's doing).
    pub fn label(&self) -> Option<String> {
        let name = self.name.as_deref()?;
        let name = if self.name_source.as_deref() == Some("auto") {
            name.split_whitespace().take(3).collect::<Vec<_>>().join(" ")
        } else {
            name.to_string()
        };
        Some(util::slug(&name)).filter(|n| !n.is_empty())
    }

    /// The name someone chose for the session (`/rename`), as opposed to the one Claude Code made up.
    pub fn chosen_name(&self) -> Option<&str> {
        let chosen = matches!(self.name_source.as_deref(), Some("user" | "peer" | "hook"));
        self.name.as_deref().filter(|_| chosen)
    }
}

/// Whether Reason Network is one of Claude Code's MCP servers here (`reason network setup` chose Claude Code), in any of
/// its directories. Read again only when a directory turns up: the daemon restarts after setup.
pub fn wired() -> bool {
    static WIRED: std::sync::Mutex<Option<(Vec<std::path::PathBuf>, bool)>> = std::sync::Mutex::new(None);
    let dirs = paths::claude_dirs();
    let mut wired = WIRED.lock().unwrap();
    match &*wired {
        Some((read, yes)) if *read == dirs => *yes,
        _ => {
            let yes = dirs.iter().any(|d| wired_in(d));
            *wired = Some((dirs, yes));
            yes
        }
    }
}

/// A `CLAUDE_CONFIG_DIR` keeps its `.claude.json` inside; `~/.claude` has it beside, as `~/.claude.json`.
fn wired_in(dir: &std::path::Path) -> bool {
    let beside = (*dir == paths::user_home().join(".claude")).then(|| paths::user_home().join(".claude.json"));
    std::iter::once(dir.join(".claude.json")).chain(beside).any(|file| {
        std::fs::read(file)
            .ok()
            .and_then(|b| serde_json::from_slice::<serde_json::Value>(&b).ok())
            .is_some_and(|v| v["mcpServers"][crate::AGENT_NAME].is_object())
    })
}

const HELD: &str = "held by Claude Code for its user's approval: the session skips permission prompts and has no \
                    `reason-network mcp` of its own (restart it, or turn the server on in /mcp, and it takes messages \
                    straight away)";

/// What to tell the sender of a message posted straight into a Claude Code session, when Claude Code will hold it. A
/// session without a `reason-network mcp` of its own (open before setup, or its server turned off or crashed) can't
/// post with its own token, so the post reaches it as a stranger's, and a session that skips permission prompts holds
/// those for its user unless `crossSessionInbound` is `accept`. `None`: not known to be held (the mode is not in its
/// transcript).
pub fn held_note(session: Option<&str>) -> Option<&'static str> {
    let transcript = crate::doing::claude_transcript(session?)?;
    let mode = permission_mode(&transcript)?;
    // `<dir>/projects/<project>/<session>.jsonl`: the settings are the session's profile's.
    let dir = transcript.parent()?.parent()?.parent()?;
    held_by(&mode, inbound_setting(dir).as_deref()).then_some(HELD)
}

fn held_by(mode: &str, setting: Option<&str>) -> bool {
    mode == "bypassPermissions" && setting != Some("accept")
}

/// `crossSessionInbound` in the user's own `settings.json`, if set. Managed settings and a session's `--settings`
/// take precedence over it and are not read, so a session can differ.
fn inbound_setting(dir: &std::path::Path) -> Option<String> {
    let text = std::fs::read_to_string(dir.join("settings.json")).ok()?;
    let v: serde_json::Value = serde_json::from_str(&text).ok()?;
    v["crossSessionInbound"].as_str().map(str::to_string)
}

/// The permission mode a session's transcript last records (Claude Code writes it with the session's entries).
fn permission_mode(transcript: &std::path::Path) -> Option<String> {
    use std::io::{Read, Seek, SeekFrom};
    let mut file = std::fs::File::open(transcript).ok()?;
    let len = file.metadata().ok()?.len();
    file.seek(SeekFrom::Start(len.saturating_sub(2 << 20))).ok()?;
    let mut tail = Vec::new();
    file.read_to_end(&mut tail).ok()?;
    // The first line may be cut by the seek; it just doesn't parse.
    tail.split(|&b| b == b'\n').rev().find_map(|line| {
        let v: serde_json::Value = serde_json::from_slice(line).ok()?;
        v.get("permissionMode")?.as_str().map(str::to_string)
    })
}

/// The messaging socket of a live session, by its id or process.
pub fn socket_for(session_id: Option<&str>, pid: Option<u32>) -> Option<ClaudeSocket> {
    let c = scan().into_iter().find(|c| {
        (session_id.is_some() && c.session_id.as_deref() == session_id) || (pid.is_some() && Some(c.pid) == pid)
    })?;
    Some(ClaudeSocket { path: c.messaging_socket_path?.into(), token: peer_token(c.pid) })
}

/// The token a session's messaging socket wants from its peers, which Claude Code asks for on Windows only. It
/// leaves it for them in `~/.claude/sessions/<pid>.<hash>.key`.
pub fn peer_token(pid: u32) -> Option<String> {
    if !cfg!(windows) {
        return None;
    }
    let prefix = format!("{pid}.");
    paths::claude_dirs().iter().filter_map(|d| std::fs::read_dir(d.join("sessions")).ok()).flatten().flatten().find_map(
        |e| {
            let name = e.file_name().into_string().ok()?;
            if !name.starts_with(&prefix) || !name.ends_with(".key") {
                return None;
            }
            let v: serde_json::Value = serde_json::from_slice(&std::fs::read(e.path()).ok()?).ok()?;
            v["peerToken"].as_str().map(str::to_string)
        },
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sessions_are_read_from_every_claude_dir() {
        let root = std::env::temp_dir().join(format!("reason-network-dirs-{}", std::process::id()));
        let (a, b) = (root.join("claude"), root.join("claude-b"));
        let pid = std::process::id();
        std::fs::create_dir_all(a.join("sessions")).unwrap();
        std::fs::create_dir_all(b.join("sessions")).unwrap();
        let entry = format!(r#"{{"pid":{pid},"sessionId":"s","name":"my-session","nameSource":"user"}}"#);
        std::fs::write(b.join("sessions").join(format!("{pid}.json")), &entry).unwrap();
        let found = scan_dirs(&[a.clone(), b.clone()]);
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].chosen_name(), Some("my-session"));
        // The same process listed twice (a profile recorded under two spellings) is one session.
        std::fs::write(a.join("sessions").join(format!("{pid}.json")), &entry).unwrap();
        assert_eq!(scan_dirs(&[a, b]).len(), 1);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn held_only_when_the_session_skips_prompts_and_the_setting_doesnt_accept() {
        assert!(held_by("bypassPermissions", None));
        assert!(held_by("bypassPermissions", Some("hold")));
        assert!(!held_by("bypassPermissions", Some("accept")));
        assert!(!held_by("auto", None));
        assert!(!held_by("default", Some("hold")));
    }

    #[test]
    fn the_last_permission_mode_in_the_transcript_counts() {
        let dir = std::env::temp_dir().join(format!("reason-network-mode-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("session.jsonl");
        let lines = [
            r#"{"type":"user","permissionMode":"bypassPermissions","message":"a"}"#,
            r#"{"type":"assistant","message":"no mode here"}"#,
            r#"{"type":"permission-mode","permissionMode":"auto"}"#,
            r#"{"type":"assistant","message":"still no mode"}"#,
        ];
        std::fs::write(&path, lines.join("\n") + "\n").unwrap();
        assert_eq!(permission_mode(&path).as_deref(), Some("auto"));
        std::fs::write(&path, "not json\n{\"type\":\"user\"}\n").unwrap();
        assert_eq!(permission_mode(&path), None, "no mode recorded");
        assert_eq!(permission_mode(&dir.join("missing.jsonl")), None);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
