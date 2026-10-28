//! What a session is doing, read from what its agent records anyway, so agents never have to report it: the
//! session's title (Claude Code's `ai-title`, Codex's thread name) and the task in progress on its own todo list
//! (Claude Code's tasks or TodoWrite, Codex's update_plan, Gemini's write_todos, Cursor's todo_write).

use crate::{agent, paths};
use serde_json::Value;
use std::{
    collections::HashMap,
    fs::File,
    io::{Read, Seek, SeekFrom},
    path::{Path, PathBuf},
    sync::{Mutex, OnceLock},
    time::SystemTime,
};

/// How much of a transcript we read the first time we see it; after that, only what was appended.
const FIRST_READ: u64 = 2 << 20;
const MAX_LEN: usize = 120;

/// Where to look for one session.
pub struct Source {
    pub agent: String,
    /// The agent's own session id (Claude Code session, Codex thread).
    pub session: Option<String>,
    /// The transcript its hooks told us about.
    pub transcript: Option<PathBuf>,
}

/// `title · task in progress`, either alone, or nothing.
pub fn of(s: &Source) -> Option<String> {
    let claude = s.agent == agent::CLAUDE_CODE;
    let transcript =
        s.transcript.clone().or_else(|| claude.then(|| claude_transcript(s.session.as_deref()?)).flatten());
    let seen = transcript.as_deref().map(scan).unwrap_or_default();
    let task = claude.then(|| claude_task(s.session.as_deref()?)).flatten().or(seen.task.flatten());
    let title = seen.title.or_else(|| (s.agent == "codex").then(|| codex_title(s.session.as_deref()?)).flatten());
    let line = match (title, task) {
        (Some(t), Some(k)) if !t.eq_ignore_ascii_case(&k) => format!("{t} · {k}"),
        (Some(t), _) => t,
        (None, Some(k)) => k,
        (None, None) => return None,
    };
    Some(clip(&crate::team::without_tickets(&line)))
}

fn clip(s: &str) -> String {
    let s = s.split_whitespace().collect::<Vec<_>>().join(" ");
    if s.chars().count() <= MAX_LEN { s } else { format!("{}…", s.chars().take(MAX_LEN - 1).collect::<String>()) }
}

/// What a transcript says so far. `task` is `Some(None)` when its latest todo list has nothing in progress.
#[derive(Clone, Default)]
struct Seen {
    title: Option<String>,
    task: Option<Option<String>>,
}

impl Seen {
    fn visit(&mut self, v: &Value) {
        match v {
            Value::Object(o) => {
                if o.get("type").and_then(Value::as_str) == Some("ai-title")
                    && let Some(t) = o.get("aiTitle").and_then(Value::as_str)
                {
                    self.title = Some(t.to_string());
                }
                for (k, v) in o {
                    match (k.as_str(), v) {
                        ("todos" | "plan", Value::Array(items)) if items.iter().any(|i| i.get("status").is_some()) => {
                            self.task = Some(items.iter().find(|i| in_progress(i)).and_then(task_text));
                        }
                        _ => self.visit(v),
                    }
                }
            }
            Value::Array(items) => items.iter().for_each(|i| self.visit(i)),
            // Codex keeps tool arguments as a JSON string.
            Value::String(s) if s.starts_with('{') && s.contains("\"status\"") => {
                if let Ok(v) = serde_json::from_str::<Value>(s) {
                    self.visit(&v);
                }
            }
            _ => {}
        }
    }
}

fn in_progress(item: &Value) -> bool {
    matches!(item.get("status").and_then(Value::as_str), Some("in_progress" | "in-progress" | "active"))
}

fn task_text(item: &Value) -> Option<String> {
    ["activeForm", "content", "step", "description", "subject", "title"]
        .iter()
        .find_map(|k| item.get(*k).and_then(Value::as_str))
        .map(str::to_string)
}

struct Tail {
    modified: Option<SystemTime>,
    offset: u64,
    /// Not JSON lines (Gemini rewrites one JSON document): read whole each time it changes.
    whole: bool,
    seen: Seen,
}

/// Reads what was appended since last time (transcripts only grow), or the whole file if it was rewritten.
fn scan(path: &Path) -> Seen {
    static TAILS: OnceLock<Mutex<HashMap<PathBuf, Tail>>> = OnceLock::new();
    let Ok(meta) = std::fs::metadata(path) else { return Seen::default() };
    let (len, modified) = (meta.len(), meta.modified().ok());
    let mut tails = TAILS.get_or_init(Default::default).lock().unwrap();
    let t = tails.entry(path.to_path_buf()).or_insert(Tail {
        modified: None,
        offset: 0,
        whole: false,
        seen: Seen::default(),
    });
    // Nothing new to read (by its length too: Windows' file times are too coarse to tell appends apart).
    if t.modified == modified && t.offset == len {
        return t.seen.clone();
    }
    if t.whole || len < t.offset {
        *t = Tail { modified: None, offset: 0, whole: t.whole, seen: Seen::default() };
    }
    let start = if t.offset == 0 { len.saturating_sub(FIRST_READ) } else { t.offset };
    let mut buf = Vec::new();
    let read = File::open(path).and_then(|mut f| {
        f.seek(SeekFrom::Start(start))?;
        f.take(len - start).read_to_end(&mut buf)
    });
    if read.is_err() {
        return t.seen.clone();
    }
    t.modified = modified;
    if t.whole || start == 0 {
        if let Ok(v) = serde_json::from_slice::<Value>(&buf) {
            t.whole = true;
            t.offset = len;
            t.seen = Seen::default();
            t.seen.visit(&v);
            return t.seen.clone();
        }
        t.whole = false;
    }
    // Up to the last complete line; a line still being written is read next time.
    let end = buf.iter().rposition(|&b| b == b'\n').map_or(0, |i| i + 1);
    let skip = if start > 0 && t.offset == 0 { buf.iter().position(|&b| b == b'\n').map_or(end, |i| i + 1) } else { 0 };
    for line in buf[skip.min(end)..end].split(|&b| b == b'\n') {
        if let Ok(v) = serde_json::from_slice::<Value>(line) {
            t.seen.visit(&v);
        }
    }
    t.offset = start + end as u64;
    t.seen.clone()
}

/// `~/.claude/projects/<project>/<session>.jsonl`.
pub(crate) fn claude_transcript(session: &str) -> Option<PathBuf> {
    static FOUND: OnceLock<Mutex<HashMap<String, PathBuf>>> = OnceLock::new();
    let mut found = FOUND.get_or_init(Default::default).lock().unwrap();
    if let Some(p) = found.get(session).filter(|p| p.exists()) {
        return Some(p.clone());
    }
    let file = format!("{session}.jsonl");
    let path = paths::claude_dirs()
        .iter()
        .filter_map(|d| std::fs::read_dir(d.join("projects")).ok())
        .flat_map(|dir| dir.flatten())
        .map(|e| e.path().join(&file))
        .find(|p| p.exists())?;
    found.insert(session.to_string(), path.clone());
    Some(path)
}

/// The task in progress on the session's task list (`~/.claude/tasks/<session>/<n>.json`).
fn claude_task(session: &str) -> Option<String> {
    let dirs = paths::claude_dirs();
    dirs.iter()
        .filter_map(|d| std::fs::read_dir(d.join("tasks").join(session)).ok())
        .flat_map(|dir| dir.flatten())
        .filter_map(|e| serde_json::from_slice::<Value>(&std::fs::read(e.path()).ok()?).ok())
        .filter(in_progress)
        .max_by_key(|t| t.get("id").and_then(Value::as_str).and_then(|i| i.parse::<u64>().ok()))
        .and_then(|t| {
            ["activeForm", "subject"].iter().find_map(|k| t.get(*k).and_then(Value::as_str)).map(str::to_string)
        })
}

/// Codex names its threads in `~/.codex/session_index.jsonl` (appended to; the last entry wins).
fn codex_title(thread: &str) -> Option<String> {
    /// Thread names as of the file's modification time.
    type Names = (Option<SystemTime>, HashMap<String, String>);
    static INDEX: OnceLock<Mutex<Names>> = OnceLock::new();
    let path = std::env::var_os("CODEX_HOME")
        .map_or_else(|| paths::user_home().join(".codex"), PathBuf::from)
        .join("session_index.jsonl");
    let modified = std::fs::metadata(&path).and_then(|m| m.modified()).ok()?;
    let mut index = INDEX.get_or_init(Default::default).lock().unwrap();
    if index.0 != Some(modified) {
        let mut file = File::open(&path).ok()?;
        let len = file.metadata().ok()?.len();
        file.seek(SeekFrom::Start(len.saturating_sub(1 << 20))).ok()?;
        let mut text = String::new();
        file.read_to_string(&mut text).ok()?;
        let names = text
            .lines()
            .filter_map(|l| serde_json::from_str::<Value>(l).ok())
            .filter_map(|v| Some((v.get("id")?.as_str()?.to_string(), v.get("thread_name")?.as_str()?.to_string())));
        *index = (Some(modified), names.collect());
    }
    index.1.get(thread).cloned()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn seen(lines: &[&str]) -> Seen {
        let mut s = Seen::default();
        for l in lines {
            s.visit(&serde_json::from_str(l).unwrap());
        }
        s
    }

    #[test]
    fn reads_titles_and_tasks_from_each_agent() {
        // Claude Code: ai-title entries, TodoWrite calls.
        let s = seen(&[
            r#"{"type":"ai-title","aiTitle":"Old title"}"#,
            r#"{"type":"ai-title","aiTitle":"Auth refresh rework"}"#,
            r#"{"type":"assistant","message":{"content":[{"type":"tool_use","name":"TodoWrite","input":{"todos":[{"content":"Write tests","status":"completed","activeForm":"Writing tests"},{"content":"Run tests","status":"in_progress","activeForm":"Running tests"}]}}]}}"#,
        ]);
        assert_eq!(s.title.as_deref(), Some("Auth refresh rework"));
        assert_eq!(s.task, Some(Some("Running tests".into())));
        // Codex: update_plan with its arguments as a JSON string.
        let s = seen(&[
            r#"{"type":"response_item","payload":{"type":"function_call","name":"update_plan","arguments":"{\"plan\":[{\"step\":\"Map issues\",\"status\":\"completed\"},{\"step\":\"Spawn subagents\",\"status\":\"in_progress\"}]}"}}"#,
        ]);
        assert_eq!(s.task, Some(Some("Spawn subagents".into())));
        // A later list with nothing in progress clears the task.
        let s = seen(&[
            r#"{"input":{"todos":[{"content":"a","status":"in_progress"}]}}"#,
            r#"{"input":{"todos":[{"content":"a","status":"completed"}]}}"#,
        ]);
        assert_eq!(s.task, Some(None));
    }

    #[test]
    fn reads_appended_lines_and_rewritten_documents() {
        let dir = std::env::temp_dir().join(format!("reason-network-doing-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.join("t.jsonl");
        std::fs::write(&p, "{\"type\":\"ai-title\",\"aiTitle\":\"First\"}\n{\"partial\":").unwrap();
        assert_eq!(scan(&p).title.as_deref(), Some("First"));
        std::fs::write(
            &p,
            "{\"type\":\"ai-title\",\"aiTitle\":\"First\"}\n{\"type\":\"ai-title\",\"aiTitle\":\"Second\"}\n",
        )
        .unwrap();
        assert_eq!(scan(&p).title.as_deref(), Some("Second"));
        // Gemini: one JSON document, rewritten.
        let g = dir.join("chat.json");
        std::fs::write(&g, r#"{"messages":[{"toolCalls":[{"name":"write_todos","args":{"todos":[{"description":"Fix login","status":"in_progress"}]}}]}]}"#).unwrap();
        assert_eq!(scan(&g).task, Some(Some("Fix login".into())));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_title_or_task_made_from_a_pasted_invite_line_leaves_the_link_out() {
        let dir = std::env::temp_dir().join(format!("reason-network-doing-link-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.join("t.jsonl");
        let ticket = format!("rn1-{}", "AbCd_-09".repeat(12));
        let title = format!("Join the Reason Network: https://reasonmachines.com/network/join/#{ticket}");
        let line = |v: Value| format!("{v}\n");
        let todo = serde_json::json!({"todos": [{"content": format!("join {ticket}"), "status": "in_progress"}]});
        std::fs::write(&p, line(serde_json::json!({"type": "ai-title", "aiTitle": title})) + &line(todo)).unwrap();
        let s = Source { agent: "cursor".into(), session: None, transcript: Some(p) };
        assert_eq!(of(&s).as_deref(), Some("Join the Reason Network: (team link) · join (team link)"));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
