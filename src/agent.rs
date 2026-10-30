//! Which agent are we running under, what is its session, and how do we reach it.

use crate::{git::GitInfo, paths, util};
use serde_json::{Value, json};
use std::{
    io::Write,
    path::{Path, PathBuf},
};

pub const CLAUDE_CODE: &str = "claude-code";

/// Best guess from the environment the agent gives its children. `REASON_NETWORK_AGENT` (set by `reason network setup` in the
/// MCP config) wins.
pub fn detect_env() -> Option<&'static str> {
    let has = |k: &str| std::env::var_os(k).is_some();
    if let Ok(a) = crate::paths::env_var("AGENT") {
        return from_client_name(&a);
    }
    if has("CLAUDECODE") || has("CLAUDE_CODE_SESSION_ID") {
        Some(CLAUDE_CODE)
    } else if has("CODEX_THREAD_ID") || has("CODEX_SESSION_ID") {
        Some("codex")
    } else if has("GROK_SESSION_ID") || has("GROK_WORKSPACE_ROOT") {
        Some("grok")
    } else if has("GEMINI_CLI") {
        Some("gemini")
    } else if has("CURSOR_AGENT") {
        Some("cursor")
    } else if has("OPENCODE") {
        Some("opencode")
    } else if has("COPILOT_CLI") || has("COPILOT_AGENT_SESSION_ID") {
        Some("copilot")
    } else {
        None
    }
}

/// From the MCP client's `clientInfo.name`, which beats the environment (agents get launched from each
/// other's terminals and inherit their variables).
pub fn from_client_name(name: &str) -> Option<&'static str> {
    let n = name.to_ascii_lowercase();
    let has = |s: &str| n.contains(s);
    Some(if has("claude-code") || has("claude code") {
        CLAUDE_CODE
    } else if n == "cmd"
        || n == "cmdc"
        || n.starts_with("cmdc-")
        || n.starts_with("cmdc ")
        || has("command-code")
        || has("command code")
        || has("commandcode")
    {
        "cmd"
    } else if has("codex") {
        "codex"
    } else if has("cursor") {
        "cursor"
    } else if has("antigravity") {
        "antigravity"
    } else if has("gemini") {
        "gemini"
    } else if has("grok") {
        "grok"
    } else if has("opencode") {
        "opencode"
    } else if has("copilot") {
        "copilot"
    } else if n == "pi" || n.starts_with("pi-") || n.starts_with("pi ") {
        "pi"
    } else if n == "dsh" || n.starts_with("dsh-") {
        // DeepSeek Harness: its MCP client says `dsh-mcp-client`.
        "dsh"
    } else if n == "zcode" || n.starts_with("zcode-") || n.starts_with("zcode ") {
        "zcode"
    } else if has("reason") {
        "reason"
    } else if n == "amp" || n.starts_with("amp-") || n.starts_with("amp ") {
        "amp"
    } else if has("zed") {
        "zed"
    } else if has("devin") || has("windsurf") {
        "devin"
    } else if has("claude") {
        "claude-desktop"
    } else {
        return None;
    })
}

/// Agents that run one MCP server process for many sessions, so the session must come from each call.
pub fn multiplexed(agent: &str) -> bool {
    matches!(agent, "codex" | "opencode" | "cursor" | "antigravity" | "zcode")
}

/// ZCode shares a workspace MCP process; each call supplies its real session and working directory.
pub fn zcode_context(meta: &serde_json::Map<String, Value>) -> Option<(String, String)> {
    let context = meta.get("com.zcode/request-context").unwrap_or(&Value::Null);
    let field = |name: &str| {
        context.get(name).or_else(|| meta.get(name)).and_then(Value::as_str).filter(|s| !s.trim().is_empty())
    };
    Some((field("session_id")?.to_string(), field("workspace_path")?.to_string()))
}

/// The session id an agent exports to its MCP servers, if any. `REASON_NETWORK_SESSION` is set by our own pi extension.
/// Claude Code's is read from its session registry first: `/clear` starts a new conversation in the same process, and
/// the variable it gave us at start still names the old one.
pub fn env_session_id(agent: &str) -> Option<String> {
    if let Some(id) = crate::paths::env_var("SESSION").ok().filter(|s| !s.is_empty()) {
        return Some(id);
    }
    if agent == CLAUDE_CODE
        && let Some(id) = claude_pid().and_then(|pid| registry_session_id(&paths::claude_sessions(), pid))
    {
        return Some(id);
    }
    let var = match agent {
        CLAUDE_CODE => "CLAUDE_CODE_SESSION_ID",
        "copilot" => "COPILOT_AGENT_SESSION_ID",
        "gemini" => "GEMINI_SESSION_ID",
        "codex" => "CODEX_THREAD_ID",
        "grok" => "GROK_SESSION_ID",
        "opencode" => "OPENCODE_SESSION_ID",
        _ => return None,
    };
    std::env::var(var).ok().filter(|s| !s.is_empty())
}

/// The session id carried in a tool call's `_meta`. Codex sends `sessionId` (the root thread, the same id its
/// hooks see) and `threadId` (which differs inside subagents); opencode V2 sends `ai.opencode/sessionID`.
pub fn meta_session_id(meta: &serde_json::Map<String, Value>) -> Option<String> {
    ["conversationId", "sessionId", "ai.opencode/sessionID", "threadId"]
        .iter()
        .find_map(|k| meta.get(*k).and_then(Value::as_str))
        .filter(|s| !s.is_empty())
        .map(str::to_string)
}

/// Finds an executable on `PATH` or in the usual install places (a launchd/systemd daemon has a bare `PATH`). On
/// Windows that is `<name>.exe`, or the `<name>.cmd` npm installs.
pub fn find_bin(name: &str) -> Option<PathBuf> {
    if let Some(p) = crate::paths::env_var_os(&format!("{}_BIN", name.to_uppercase())) {
        let p = PathBuf::from(p);
        return p.is_file().then_some(p);
    }
    let home = paths::user_home();
    #[cfg(unix)]
    let extra = [
        home.join(".local/bin"),
        home.join(".bun/bin"),
        home.join(".npm-global/bin"),
        home.join(".cargo/bin"),
        home.join(".opencode/bin"),
        PathBuf::from("/opt/homebrew/bin"),
        PathBuf::from("/usr/local/bin"),
        PathBuf::from("/usr/bin"),
    ];
    #[cfg(windows)]
    let extra = [home.join(r".local\bin"), home.join(r"AppData\Roaming\npm"), home.join(r".bun\bin")];
    let exts: &[&str] = if cfg!(windows) { &[".exe", ".cmd"] } else { &[""] };
    let path = std::env::var_os("PATH").unwrap_or_default();
    std::env::split_paths(&path)
        .chain(extra)
        .flat_map(|d| exts.iter().map(move |ext| d.join(format!("{name}{ext}"))))
        .find(|p| executable(p))
}

/// How to run `name`, for the caller to add its arguments to. On Windows the `<name>.cmd` npm (or pnpm) installs runs
/// through cmd.exe, which can't be handed an argument with a line break (Rust refuses one rather than let cmd.exe cut it
/// short), and a message has them: its Node script is run with node instead, when the shim names one.
pub fn command_for(name: &str) -> Option<std::process::Command> {
    find_bin(name).map(|bin| command_at(&bin))
}

/// [`command_for`] for the program at `bin`.
fn command_at(bin: &Path) -> std::process::Command {
    #[cfg(windows)]
    if bin.extension().is_some_and(|e| e.eq_ignore_ascii_case("cmd"))
        && let Some(dir) = bin.parent()
        && let Some((node, script)) = std::fs::read_to_string(bin).ok().and_then(|shim| node_shim(&shim, dir))
    {
        let mut command = std::process::Command::new(node);
        command.args(script);
        return command;
    }
    std::process::Command::new(bin)
}

/// Node and the script an npm or pnpm `.cmd` shim in `dir` runs (`"%dp0%\\node_modules\\…\\codex.js" %*`): the node
/// beside the shim if there is one, as the shim itself prefers, else the one on `PATH`. Only when the shim runs that
/// script with node and nothing else in between (no flags, not bun), and sets nothing in the environment but its own
/// bookkeeping (`dp0`, `_prog`, `PATHEXT`, `NODE_PATH`): any other shim is left to run as it is.
#[cfg(any(windows, test))]
fn node_shim(shim: &str, dir: &Path) -> Option<(PathBuf, Vec<PathBuf>)> {
    let lower = shim.to_ascii_lowercase();
    // Running node directly would skip whatever else the shim sets for it.
    let sets_more = lower.split(['\n', '&', '(']).any(|part| {
        let part = part.trim().trim_start_matches('@');
        let Some(set) = part.strip_prefix("set ").or_else(|| part.strip_prefix("set\t")) else { return false };
        let name = set.trim_start().trim_start_matches('"').split('=').next().unwrap_or_default().trim();
        !["dp0", "_prog", "pathext", "node_path"].contains(&name)
    });
    if sets_more {
        return None;
    }
    let script = shim.match_indices('"').find_map(|(at, _)| {
        let part = shim[at + 1..].split('"').next()?;
        let rest = part.strip_prefix("%dp0%\\").or_else(|| part.strip_prefix("%~dp0\\"))?;
        let ext = rest.to_ascii_lowercase();
        if ![".js", ".cjs", ".mjs"].iter().any(|e| ext.ends_with(e)) {
            return None;
        }
        // What runs it: npm's `"%_prog%"` (node beside the shim, else `node`), node.exe, or plain `node`.
        let before = lower[..at].rsplit(['\n', '&', '(']).next()?.trim();
        let by_node = (before.ends_with("\"%_prog%\"") && lower.contains("_prog=node\""))
            || before.ends_with("\\node.exe\"")
            || before.trim_start_matches('@') == "node";
        by_node.then(|| rest.split(['\\', '/']).fold(dir.to_path_buf(), |p, c| p.join(c)))
    })?;
    if !script.is_file() {
        return None;
    }
    let beside = dir.join("node.exe");
    let node = if beside.is_file() { beside } else { find_bin("node")? };
    Some((node, vec![script]))
}

#[cfg(unix)]
fn executable(p: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    p.metadata().is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
}

#[cfg(windows)]
fn executable(p: &Path) -> bool {
    p.is_file()
}

pub fn session_from_hooks(agent: &str) -> bool {
    matches!(agent, "gemini" | "cmd")
}

/// The agent process we serve, as [`hook_agent_pid`] finds it for a hook: found once (each step up can run `ps`).
pub fn agent_process() -> Option<u32> {
    static PID: std::sync::OnceLock<Option<u32>> = std::sync::OnceLock::new();
    *PID.get_or_init(hook_agent_pid)
}

/// The agent process behind a hook: our parent, skipping the shell some agents run hooks through.
pub fn hook_agent_pid() -> Option<u32> {
    let mut pid = util::parent_pid();
    for _ in 0..3 {
        let (ppid, name) = util::process(pid)?;
        if !matches!(name.as_str(), "sh" | "bash" | "zsh" | "fish" | "dash" | "ksh" | "cmd" | "powershell" | "pwsh") {
            return Some(pid);
        }
        pid = ppid;
    }
    Some(pid)
}

pub fn short(agent: &str) -> &str {
    match agent {
        CLAUDE_CODE | "claude-desktop" => "claude",
        "" => "agent",
        a => a,
    }
}

/// How people write the agent's name, for notes to senders.
pub fn label(agent: &str) -> &str {
    match agent {
        "codex" => "Codex",
        "grok" => "Grok",
        "cmd" => "Command Code",
        "reason" => "Reason",
        a => a,
    }
}

/// `<repo-or-dir>-<agent>`, e.g. `api-claude`.
pub fn default_name(agent: &str, cwd: &str, git: &GitInfo) -> String {
    let dir = git.root.as_deref().unwrap_or(cwd);
    let base = util::slug(Path::new(dir).file_name().and_then(|n| n.to_str()).unwrap_or("home"));
    let base = if base.is_empty() { "home".into() } else { base };
    util::joined(&base, short(agent))
}

pub fn cwd() -> String {
    std::env::var("CLAUDE_PROJECT_DIR")
        .ok()
        .filter(|s| !s.is_empty())
        .or_else(|| std::env::current_dir().ok().map(|p| p.display().to_string()))
        .unwrap_or_else(|| "/".into())
}

/// The agent process: `CLAUDE_PID` when Claude Code sets it, else our parent.
pub fn agent_pid() -> u32 {
    std::env::var("CLAUDE_PID").ok().and_then(|s| s.parse().ok()).unwrap_or_else(util::parent_pid)
}

/// The Claude Code process we run under, if any: the nearest of our ancestors (a shell or a launcher can sit between),
/// else `CLAUDE_PID`, whose entry in Claude Code's session registry names the conversation Claude Code started us in
/// (`CLAUDE_CODE_SESSION_ID`). Another's isn't ours: a headless Claude run from an interactive one's shell has no entry
/// of its own, and the entry above it is the other conversation's. Kept once found, and its entry followed from then
/// on (after `/clear` it names the new conversation). Not found, it is looked for again after a while (Claude Code may
/// not have written its entry yet), not on every call: each look can run `ps` for each ancestor.
pub fn claude_pid() -> Option<u32> {
    static PID: std::sync::OnceLock<u32> = std::sync::OnceLock::new();
    if let Some(pid) = PID.get() {
        return Some(*pid);
    }
    // Our ancestors don't change: they are looked up once (each can take a `ps`), their entries each time.
    let chain = ancestors();
    let dir = paths::claude_sessions();
    let found = find_claude(
        chain.first().copied().unwrap_or_else(util::parent_pid),
        std::env::var("CLAUDE_CODE_SESSION_ID").ok().as_deref(),
        std::env::var("CLAUDE_PID").ok().and_then(|s| s.parse().ok()),
        |p| registry_session_id(&dir, p),
        |p| chain.iter().position(|a| *a == p).and_then(|i| chain.get(i + 1).copied()),
    );
    if let Some(pid) = found {
        let _ = PID.set(pid);
    }
    found
}

/// Our parent and its ancestors, a few levels up: found once.
fn ancestors() -> &'static [u32] {
    static CHAIN: std::sync::OnceLock<Vec<u32>> = std::sync::OnceLock::new();
    CHAIN.get_or_init(|| {
        let mut chain = vec![util::parent_pid()];
        while chain.len() < 5 {
            let last = *chain.last().expect("never empty");
            match parent_of(last) {
                Some(p) if p > 1 && !chain.contains(&p) => chain.push(p),
                _ => break,
            }
        }
        chain
    })
}

/// The Claude Code process in conversation `given`: the nearest of `start` and its ancestors, else `claude_pid`, whose
/// registry entry (`entry`) names it. None without a conversation to match.
fn find_claude(
    start: u32,
    given: Option<&str>,
    claude_pid: Option<u32>,
    entry: impl Fn(u32) -> Option<String>,
    parent: impl Fn(u32) -> Option<u32>,
) -> Option<u32> {
    let given = given.filter(|g| !g.is_empty())?;
    let ours = |p: u32| entry(p).as_deref() == Some(given);
    registry_ancestor(start, ours, parent).or_else(|| claude_pid.filter(|p| ours(*p)))
}

/// The nearest of `pid` and its ancestors (a few levels up) that `has_entry`.
fn registry_ancestor(
    mut pid: u32,
    has_entry: impl Fn(u32) -> bool,
    parent: impl Fn(u32) -> Option<u32>,
) -> Option<u32> {
    for _ in 0..5 {
        if has_entry(pid) {
            return Some(pid);
        }
        match parent(pid) {
            Some(p) if p > 1 && p != pid => pid = p,
            _ => return None,
        }
    }
    None
}

/// A process's parent: from `/proc` on Linux, else as `ps` (or Windows) says.
fn parent_of(pid: u32) -> Option<u32> {
    #[cfg(target_os = "linux")]
    return stat_parent(&std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?);
    #[cfg(not(target_os = "linux"))]
    return util::process(pid).map(|(parent, _)| parent);
}

/// The parent pid in a `/proc/<pid>/stat` line: `<pid> (<name>) <state> <ppid> …`, where the name can hold spaces and
/// parentheses of its own.
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
fn stat_parent(stat: &str) -> Option<u32> {
    stat.rsplit_once(')')?.1.split_whitespace().nth(1)?.parse().ok()
}

/// The conversation Claude Code process `pid` is in now, from its registry entry.
fn registry_session_id(dir: &Path, pid: u32) -> Option<String> {
    let s: Value = serde_json::from_str(&std::fs::read_to_string(dir.join(format!("{pid}.json"))).ok()?).ok()?;
    s["sessionId"].as_str().filter(|id| !id.is_empty()).map(str::to_string)
}

/// The inbox socket of the Claude Code session that spawned us. Posting a `user` frame there puts the text in
/// the conversation between tool calls, or starts a turn if the session is idle.
#[derive(Debug, Clone)]
pub struct ClaudeSocket {
    pub path: PathBuf,
    pub token: Option<String>,
}

/// The inbox of the Claude Code session we serve: the one Claude Code gave us (`CLAUDE_CODE_MESSAGING_SOCKET`, which it
/// sets once its inbox listens, having dropped any it inherited), else the one named by the registry entry of the
/// process in our conversation (`claude_pid`). Never another's: a headless Claude run from another's shell has no entry,
/// and a `CLAUDE_PID` it inherited would be the other session's. Then nothing is pushed, and mail waits for
/// `check_inbox`.
pub fn claude_socket() -> Option<ClaudeSocket> {
    let token = std::env::var("CLAUDE_CODE_MESSAGING_TOKEN").ok().filter(|s| !s.is_empty());
    if let Some(path) = std::env::var_os("CLAUDE_CODE_MESSAGING_SOCKET").filter(|p| !p.is_empty()) {
        return Some(ClaudeSocket { path: path.into(), token });
    }
    let pid = claude_pid()?;
    socket_of(&paths::claude_sessions(), pid, None, token, crate::claude::peer_token)
}

/// The inbox Claude Code process `pid`'s registry entry names, with its token: the one we were given, when that is for
/// this very inbox, else what Claude Code keeps for its peers.
fn socket_of(
    dir: &Path,
    pid: u32,
    given: Option<PathBuf>,
    given_token: Option<String>,
    peer_token: impl Fn(u32) -> Option<String>,
) -> Option<ClaudeSocket> {
    let s: Value = serde_json::from_str(&std::fs::read_to_string(dir.join(format!("{pid}.json"))).ok()?).ok()?;
    let path = PathBuf::from(s["messagingSocketPath"].as_str().filter(|p| !p.is_empty())?);
    let token = given_token.filter(|_| given.as_deref() == Some(path.as_path())).or_else(|| peer_token(pid));
    Some(ClaudeSocket { path, token })
}

pub fn post_to_claude(sock: &ClaudeSocket, text: &str) -> std::io::Result<()> {
    let mut c = open_inbox(&sock.path)?;
    if let Some(t) = &sock.token {
        writeln!(c, "{}", json!({ "type": "auth", "token": t }))?;
    }
    writeln!(c, "{}", json!({ "type": "user", "message": { "role": "user", "content": text } }))?;
    c.flush()
}

#[cfg(unix)]
fn open_inbox(path: &Path) -> std::io::Result<std::os::unix::net::UnixStream> {
    let c = std::os::unix::net::UnixStream::connect(path)?;
    c.set_write_timeout(Some(std::time::Duration::from_secs(5)))?;
    Ok(c)
}

/// On Windows the session's inbox is a named pipe, which opens like a file.
#[cfg(windows)]
fn open_inbox(path: &Path) -> std::io::Result<std::fs::File> {
    std::fs::OpenOptions::new().read(true).write(true).open(path)
}

#[cfg(test)]
mod tests {
    #[test]
    fn zcode_sessions_come_from_each_calls_context_not_a_shared_process() {
        let a = serde_json::json!({"session_id":"one","workspace_path":"C:\\用户 空格"});
        let b = serde_json::json!({"com.zcode/request-context":{"session_id":"two","workspace_path":"C:\\用户 空格"}});
        assert_eq!(super::zcode_context(a.as_object().unwrap()), Some(("one".into(), r"C:\用户 空格".into())));
        assert_eq!(super::zcode_context(b.as_object().unwrap()), Some(("two".into(), r"C:\用户 空格".into())));
        assert!(
            super::zcode_context(serde_json::json!({"session_id":"","workspace_path":"/"}).as_object().unwrap())
                .is_none()
        );
        assert!(super::multiplexed("zcode"));
    }

    use super::*;

    #[test]
    fn client_names() {
        assert_eq!(from_client_name("claude-code"), Some(CLAUDE_CODE));
        assert_eq!(from_client_name("codex-mcp-client"), Some("codex"));
        assert_eq!(from_client_name("Command Code"), Some("cmd"));
        assert_eq!(from_client_name("command-code-mcp-client"), Some("cmd"));
        assert_eq!(from_client_name("cmd"), Some("cmd"));
        assert_eq!(from_client_name("cmdc"), Some("cmd"));
        assert_eq!(from_client_name("unrelated-command"), None);
        assert_eq!(from_client_name("cursor-vscode"), Some("cursor"));
        assert_eq!(from_client_name("gemini-cli-mcp-client"), Some("gemini"));
        // grok names its client after the server, so this one also contains "reason".
        assert_eq!(from_client_name("grok-shell-reason-network"), Some("grok"));
        assert_eq!(from_client_name("dsh-mcp-client"), Some("dsh"));
        assert_eq!(from_client_name("ashdsh"), None);
        assert_eq!(from_client_name("ZCode"), Some("zcode"));
        assert_eq!(from_client_name("zcode-mcp-client"), Some("zcode"));
        assert_eq!(from_client_name("not-zcode"), None);
        assert_eq!(from_client_name("claude-ai"), Some("claude-desktop"));
        assert_eq!(from_client_name("example-client"), None);
    }

    #[test]
    fn claude_codes_registry_names_the_conversation_its_process_is_in_now() {
        let dir = std::env::temp_dir().join(format!("reason-network-claude-{}", ulid::Ulid::generate()));
        std::fs::create_dir_all(&dir).unwrap();
        // After `/clear`, the same process is in a new conversation.
        std::fs::write(dir.join("4242.json"), r#"{"pid":4242,"sessionId":"after-clear","kind":"interactive"}"#)
            .unwrap();
        std::fs::write(dir.join("4343.json"), r#"{"pid":4343,"sessionId":""}"#).unwrap();
        assert_eq!(registry_session_id(&dir, 4242).as_deref(), Some("after-clear"));
        assert_eq!(registry_session_id(&dir, 4343), None);
        assert_eq!(registry_session_id(&dir, 4444), None);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn gemini_and_command_code_mcp_servers_go_by_their_hooks_session() {
        assert!(session_from_hooks("gemini"));
        assert!(session_from_hooks("cmd"));
        for a in [CLAUDE_CODE, "codex", "cursor", "opencode", "copilot", "grok", "pi", "zed", ""] {
            assert!(!session_from_hooks(a), "{a}");
        }
    }

    #[test]
    fn the_claude_process_is_the_nearest_ancestor_with_a_registry_entry() {
        // 900 (us) ← 800 (a shell) ← 700 (Claude Code, with an entry) ← 600 (another Claude Code, with one) ← 1.
        let parent = |p: u32| match p {
            900 => Some(800),
            800 => Some(700),
            700 => Some(600),
            600 => Some(1),
            _ => None,
        };
        assert_eq!(registry_ancestor(900, |p| p == 700 || p == 600, parent), Some(700));
        assert_eq!(registry_ancestor(900, |p| p == 600, parent), Some(600));
        assert_eq!(registry_ancestor(900, |_| false, parent), None);
        // Not past init, nor round a loop.
        assert_eq!(registry_ancestor(900, |p| p == 1, parent), None);
        assert_eq!(registry_ancestor(5, |_| false, |_| Some(5)), None);
    }

    #[test]
    fn the_claude_process_is_the_one_whose_entry_names_our_conversation() {
        // 900 (us) ← 800 (a headless Claude, no entry) ← 700 (an interactive Claude, in `outer`) ← 1.
        let parent = |p: u32| match p {
            900 => Some(800),
            800 => Some(700),
            700 => Some(1),
            _ => None,
        };
        let entry = |p: u32| (p == 700).then(|| "outer".to_string());
        // The headless Claude's server, started in `inner`: not the interactive one's, by walking up or by CLAUDE_PID.
        assert_eq!(find_claude(900, Some("inner"), None, entry, parent), None);
        assert_eq!(find_claude(900, Some("inner"), Some(700), entry, parent), None);
        // The interactive one's own server.
        assert_eq!(find_claude(900, Some("outer"), None, entry, parent), Some(700));
        assert_eq!(find_claude(5, Some("outer"), Some(700), entry, |_| None), Some(700));
        // Nothing to match by.
        assert_eq!(find_claude(900, None, Some(700), entry, parent), None);
    }

    #[test]
    fn a_headless_claude_pushes_nothing_into_the_session_it_was_run_from() {
        let dir = std::env::temp_dir().join(format!("reason-network-claude-{}", ulid::Ulid::generate()));
        std::fs::create_dir_all(&dir).unwrap();
        // The interactive Claude (700, in `outer`) and its inbox, which a headless Claude it runs (no entry, in
        // `inner`) may have inherited, with CLAUDE_PID=700.
        std::fs::write(
            dir.join("700.json"),
            r#"{"pid":700,"sessionId":"outer","messagingSocketPath":"/run/outer.sock"}"#,
        )
        .unwrap();
        let entry = |p: u32| registry_session_id(&dir, p);
        let parent = |p: u32| match p {
            900 => Some(800),
            800 => Some(700),
            _ => None,
        };
        // The headless one's server finds no process of its own: no inbox, whatever it inherited.
        assert_eq!(find_claude(900, Some("inner"), Some(700), entry, parent), None);
        // The interactive one's server gets its own inbox, with the token it was given for it.
        let pid = find_claude(900, Some("outer"), None, entry, parent).unwrap();
        let none = |_| None;
        let s = socket_of(&dir, pid, Some("/run/outer.sock".into()), Some("t".into()), none).unwrap();
        assert_eq!((s.path, s.token.as_deref()), (PathBuf::from("/run/outer.sock"), Some("t")));
        // A token that came with another inbox isn't this one's.
        let s = socket_of(&dir, pid, Some("/run/other.sock".into()), Some("t".into()), none).unwrap();
        assert_eq!(s.token, None);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_parent_is_read_from_proc_stat() {
        assert_eq!(stat_parent("4242 (claude) S 4100 4242 4100 0 -1 4194560"), Some(4100));
        assert_eq!(stat_parent("4242 (node (my) ) app) R 77 1 1"), Some(77));
        assert_eq!(stat_parent("garbage"), None);
    }

    #[test]
    fn names() {
        let git = GitInfo { root: Some("/Users/alex/src/API".into()), ..Default::default() };
        assert_eq!(default_name(CLAUDE_CODE, "/Users/alex/src/API/web", &git), "api-claude");
        assert_eq!(default_name("codex", "/tmp/My Thing", &GitInfo::default()), "my-thing-codex");
    }

    #[test]
    fn an_npm_shim_is_run_as_node_and_its_script() {
        let d = std::env::temp_dir().join(format!("reason-network-shim-{}", ulid::Ulid::generate()));
        let script = d.join("node_modules").join("@openai").join("codex").join("bin").join("codex.js");
        std::fs::create_dir_all(script.parent().unwrap()).unwrap();
        std::fs::write(&script, "").unwrap();
        std::fs::write(d.as_path().join("node.exe"), "").unwrap();
        // What npm 10/11's cmd-shim writes for Codex.
        let npm = "@ECHO off\r\nGOTO start\r\n:find_dp0\r\nSET dp0=%~dp0\r\nEXIT /b\r\n:start\r\nSETLOCAL\r\n\
                   CALL :find_dp0\r\n\r\nIF EXIST \"%dp0%\\node.exe\" (\r\n  SET \"_prog=%dp0%\\node.exe\"\r\n) ELSE (\r\n  \
                   SET \"_prog=node\"\r\n  SET PATHEXT=%PATHEXT:;.JS;=;%\r\n)\r\n\r\nendLocal & goto #_undefined_# 2>NUL || \
                   title %COMSPEC% & \"%_prog%\"  \"%dp0%\\node_modules\\@openai\\codex\\bin\\codex.js\" %*\r\n";
        assert_eq!(node_shim(npm, d.as_path()), Some((d.as_path().join("node.exe"), vec![script.clone()])));
        // pnpm's points up out of its own folder, with `%~dp0`.
        let sub = d.as_path().join("bin");
        std::fs::create_dir_all(&sub).unwrap();
        std::fs::write(sub.join("node.exe"), "").unwrap();
        let pnpm = "@SETLOCAL\r\n\"%~dp0\\node.exe\" \"%~dp0\\..\\node_modules\\@openai\\codex\\bin\\codex.js\" %*\r\n";
        let (node, args) = node_shim(pnpm, &sub).unwrap();
        assert_eq!(node, sub.join("node.exe"));
        assert_eq!(std::fs::canonicalize(&args[0]).unwrap(), std::fs::canonicalize(&script).unwrap());
        // A shim that runs no Node script, or one that isn't there, is left to run as it is.
        assert_eq!(node_shim("@echo off\r\n\"%dp0%\\codex.exe\" %*\r\n", d.as_path()), None);
        assert_eq!(node_shim(&npm.replace("codex.js", "gone.js"), d.as_path()), None);
        // Plain `node` (pnpm's ELSE branch), and only node: not bun, and not node with flags we'd drop.
        let plain = "@SETLOCAL\r\nnode  \"%~dp0\\..\\node_modules\\@openai\\codex\\bin\\codex.js\" %*\r\n";
        assert!(node_shim(plain, &sub).is_some());
        assert_eq!(
            node_shim(&npm.replace("_prog=node\"", "_prog=bun\"").replace("node.exe", "bun.exe"), d.as_path()),
            None
        );
        let flags = npm.replace("\"%_prog%\"  \"%dp0%", "\"%_prog%\" --no-warnings \"%dp0%");
        assert_eq!(node_shim(&flags, d.as_path()), None);
        // pnpm's NODE_PATH is fine to skip; a shim that sets anything else is left to set it.
        let node_path =
            format!("@SETLOCAL\r\n@IF NOT DEFINED NODE_PATH (\r\n  @SET \"NODE_PATH=C:\\x\"\r\n)\r\n{plain}");
        assert!(node_shim(&node_path, &sub).is_some());
        for set in ["@SET \"NODE_OPTIONS=--max-old-space-size=8192\"", "SET CODEX_HOME=C:\\c", "set /a n=1"] {
            assert_eq!(node_shim(&format!("@SETLOCAL\r\n{set}\r\n{plain}"), &sub), None, "{set}");
        }
        assert_eq!(node_shim(&format!("@SETLOCAL\r\nIF 1==1 (SET X=1)\r\n{plain}"), &sub), None);
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn a_program_that_is_not_a_cmd_shim_runs_as_it_is() {
        let bin = std::env::temp_dir().join("codex");
        let command = command_at(&bin);
        assert_eq!(command.get_program(), bin.as_os_str());
        assert_eq!(command.get_args().count(), 0);
    }

    /// The point of it, on Windows itself: a message with line breaks and cmd.exe's special characters reaches the
    /// script npm installed, as it was sent, through the command delivery runs.
    #[cfg(windows)]
    #[test]
    fn a_message_with_line_breaks_reaches_an_npm_installed_script() {
        let Some(node) = find_bin("node") else {
            assert!(std::env::var_os("CI").is_none(), "CI must run this with node installed");
            return;
        };
        let d = std::env::temp_dir().join(format!("reason-network-shim-run-{}", ulid::Ulid::generate()));
        let bin = d.join("node_modules").join("x").join("bin");
        std::fs::create_dir_all(&bin).unwrap();
        let out = d.join("args.json");
        std::fs::write(
            bin.join("x.js"),
            format!("require('fs').writeFileSync({:?}, JSON.stringify(process.argv.slice(2)))", out.to_str().unwrap()),
        )
        .unwrap();
        let shim =
            "@ECHO off\r\nSETLOCAL\r\nSET \"_prog=node\"\r\n\"%_prog%\"  \"%dp0%\\node_modules\\x\\bin\\x.js\" %*\r\n";
        std::fs::write(d.join("x.cmd"), shim).unwrap();
        let mut command = command_at(&d.join("x.cmd"));
        assert_eq!(command.get_program(), node.as_os_str());
        let text = "<reason-message from=\"a\">\r\nline & two %PATH% ^ | < > \"quoted\"\n</reason-message>";
        let status = command.args(["queue", "--message", text]).status().unwrap();
        assert!(status.success());
        let got: Vec<String> = serde_json::from_str(&std::fs::read_to_string(&out).unwrap()).unwrap();
        assert_eq!(got, ["queue", "--message", text]);
        let _ = std::fs::remove_dir_all(&d);
    }
}
