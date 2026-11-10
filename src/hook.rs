//! `reason-network hook <agent> <event>`: run by an agent's hook system. Tells the daemon the session is alive (and busy
//! or idle), and prints whatever makes the agent show incoming mail. It never fails loudly: a broken Reason Network must
//! not break the agent.

use crate::{agent, client::Link, proto::*};
use serde_json::{Value, json};
use std::{
    io::{Read, Write},
    time::Duration,
};

pub async fn run(agent_name: &str, event: &str, session: Option<String>, state: Option<String>, doing: Option<String>) {
    let cfg = crate::config::Config::load();
    let input = if session.is_none() { read_stdin(Duration::from_secs(3)) } else { String::new() };
    let link = Link::new(None);
    let Ok(Some((out, key, ids))) = tokio::time::timeout(
        Duration::from_secs(8),
        handle((&link, &cfg), agent_name, event, &input, session, state, doing),
    )
    .await
    else {
        return;
    };
    let mut stdout = std::io::stdout();
    if writeln!(stdout, "{out}").and_then(|_| stdout.flush()).is_ok() && !ids.is_empty() {
        // Printed, so the agent has it: now it counts as read.
        let _ = tokio::time::timeout(Duration::from_secs(3), link.call::<Value>(Request::Delivered { key, ids })).await;
    }
}

/// The `CODEX_HOME` a Codex hook runs with, for the daemon to run `codex queue` against: only a full path to a folder
/// that is there, as its real path. Anything else is left out, and the daemon uses the default home.
fn codex_home(value: Option<std::ffi::OsString>) -> Option<String> {
    let path = std::path::PathBuf::from(value?);
    if !path.is_absolute() || !path.is_dir() {
        return None;
    }
    let real = std::fs::canonicalize(path).ok()?;
    let real = real.to_str()?;
    // Windows gives it as `\\?\C:\…`; kept as `C:\…`, as Codex would be given it (a share's path as it comes).
    Some(real.strip_prefix(r"\\?\").filter(|p| !p.starts_with("UNC")).unwrap_or(real).to_string())
}

/// The hook payload, without hanging if the agent never closes our stdin.
fn read_stdin(limit: Duration) -> String {
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let mut s = String::new();
        let _ = std::io::stdin().read_to_string(&mut s);
        let _ = tx.send(s);
    });
    rx.recv_timeout(limit).unwrap_or_default()
}

/// Returns what to print, the session key, and the ids of the messages in it.
async fn handle(
    (link, cfg): (&std::sync::Arc<Link>, &crate::config::Config),
    agent_name: &str,
    event: &str,
    input: &str,
    session: Option<String>,
    state: Option<String>,
    doing: Option<String>,
) -> Option<(String, String, Vec<String>)> {
    // Windows PowerShell pipes it in after a byte-order mark.
    let v: Value = serde_json::from_str(input.trim_start_matches('\u{feff}')).unwrap_or(Value::Null);
    let agent = agent::from_client_name(agent_name).unwrap_or(agent_name);
    let str_at = |k: &str| v.get(k).and_then(Value::as_str).filter(|s| !s.is_empty()).map(str::to_string);
    if agent == "grok" && grok_ignores(event, &v) {
        return None;
    }

    let session_id = session
        .or_else(|| str_at("session_id"))
        .or_else(|| str_at("conversation_id"))
        .or_else(|| str_at("conversationId"))
        .or_else(|| str_at("sessionId"))?;
    // Never a session: the daemon is told, so nothing else (its MCP server's calls) makes it one either. Nothing printed.
    if agent == "codex" && codex_keeps_no_transcript(&v) {
        let req = Request::Hook(HookCall {
            through_reason: Some(false),
            agent: agent.to_string(),
            event: "no_transcript".into(),
            session_id,
            cwd: None,
            pid: None,
            mail: false,
            transcript: None,
            doing: None,
            name: None,
            dirs: Vec::new(),
            keep: false,
            codex_home: None,
        });
        let _ = link.call::<HookReply>(req).await;
        return None;
    }
    // Every folder the agent names for the session, the one it works in first.
    let mut dirs: Vec<String> = str_at("cwd").into_iter().collect();
    dirs.extend(v["workspace_roots"].as_array().into_iter().flatten().filter_map(Value::as_str).map(str::to_string));
    dirs.extend(v["workspacePaths"].as_array().into_iter().flatten().filter_map(Value::as_str).map(str::to_string));
    dirs.extend(
        ["CURSOR_PROJECT_DIR", "GEMINI_CWD", "CLAUDE_PROJECT_DIR"]
            .iter()
            .filter_map(|k| std::env::var(k).ok())
            .filter(|s| !s.is_empty()),
    );
    let cwd = dirs.first().cloned().or_else(|| std::env::current_dir().ok().map(|p| p.display().to_string()));
    // In a folder that is off, the agent hears nothing; the daemon only that this session is off.
    let off = cwd.iter().chain(&dirs).any(|d| cfg.off_in(d));
    let ev = match (event, state.as_deref()) {
        ("session-start", _) => "start",
        ("user-prompt-submit", _) => "prompt",
        ("post-tool-use", _) => "tool",
        ("stop" | "poll", _) => "stop",
        ("session-end", _) => "end",
        ("status", Some("busy" | "retry")) => "prompt",
        ("status", _) => "stop",
        ("doing", _) => "doing",
        _ => return None,
    };
    let mail = !off
        && takes_mail(agent, event)
        && !(agent == "antigravity" && event == "stop" && v["terminationReason"] != "model_stop");

    let key = format!("{agent}:{session_id}");
    let native_app =
        agent == "antigravity" && str_at("transcriptPath").as_deref().is_some_and(crate::antigravity::app_transcript);
    let req = Request::Hook(HookCall {
        through_reason: Some(false),
        agent: agent.to_string(),
        event: ev.to_string(),
        session_id,
        cwd,
        pid: if native_app { None } else { agent::hook_agent_pid() },
        mail,
        transcript: str_at("transcript_path").or_else(|| str_at("transcriptPath")),
        doing,
        name: None,
        dirs: dirs.into_iter().skip(1).collect(),
        keep: false,
        codex_home: (agent == "codex").then(|| codex_home(std::env::var_os("CODEX_HOME"))).flatten(),
    });
    let reply: HookReply = link.call(req).await.ok()?;
    if off {
        return None;
    }
    let ids = reply.ids;
    // An older daemon doesn't say: then at session start, as before.
    let intro = reply.intro.unwrap_or(ev == "start");
    let text = match (reply.text, reply.address) {
        (Some(t), _) => t,
        // Say who it is on Reason Network once per session.
        (None, Some(addr)) if intro => {
            format!(
                "Reason Network: you are {addr}. Teammates' agents can message you; list_team and send_message reach them."
            )
        }
        _ => return None,
    };
    let text = if agent == "antigravity" {
        format!(
            "{text}\nFor Reason Network tools, pass session_id: {}. This identifies this conversation even when other chats use the same project.",
            key.strip_prefix("antigravity:").unwrap_or(&key)
        )
    } else {
        text
    };
    Some((format_for(agent, event, ev, &text), key, ids))
}

/// Prompt-submit hooks in Cursor, Copilot and grok can't add context, grok drops what SessionStart prints, and
/// opencode's status call shows nothing.
fn takes_mail(agent: &str, event: &str) -> bool {
    // Antigravity consumes injectSteps before invocation, and a continue reason at a natural Stop.
    if agent == "antigravity" {
        return matches!(event, "session-start" | "user-prompt-submit" | "stop");
    }
    !matches!(
        (agent, event),
        ("cursor" | "copilot" | "grok", "user-prompt-submit") | ("grok", "session-start") | (_, "status" | "doing")
    )
}

/// grok runs a subagent's hooks under the subagent's own session id, and after SessionEnd fires one more Stop
/// (reason "shutdown" or "channel_closed") whose block it ignores.
fn grok_ignores(event: &str, v: &Value) -> bool {
    !v["subagentType"].is_null() || (event == "stop" && v["reason"] != "end_turn")
}

/// A Codex thread whose hooks name its transcript as null keeps no local rollout: an ephemeral thread (the Codex app
/// names a chat with one, for instance), or one kept in storage that isn't on this disk (in memory, for testing). No
/// chat on this computer anyone can come back to or reach, so never a session here, nor a name taken from the chats
/// that are. Older Codex, whose hooks don't name a transcript at all, is not this.
fn codex_keeps_no_transcript(v: &Value) -> bool {
    v.get("transcript_path").is_some_and(Value::is_null)
}

/// The JSON (or text) each agent expects from this hook to put `text` in front of the model.
fn format_for(agent: &str, event: &str, ev: &str, text: &str) -> String {
    let specific = |name: &str| json!({ "hookSpecificOutput": { "hookEventName": name, "additionalContext": text } });
    let block = || json!({ "decision": "block", "reason": text });
    let out = match (agent, ev) {
        ("antigravity", "start" | "prompt") => json!({"injectSteps": [{"userMessage": text}]}),
        ("antigravity", "stop") => json!({"decision":"continue","reason":text}),
        ("opencode", _) if event == "poll" => return text.to_string(),
        ("codex" | "cmd", "start") => specific("SessionStart"),
        ("cmd", "tool") => specific("PostToolUse"),
        ("cmd", "stop") => block(),
        ("codex", "prompt") => specific("UserPromptSubmit"),
        ("codex", "tool") => specific("PostToolUse"),
        ("grok", "tool") => specific("PostToolUse"),
        ("gemini", "start") => specific("SessionStart"),
        ("gemini", "prompt") => specific("BeforeAgent"),
        ("gemini", "tool") => specific("AfterTool"),
        ("gemini", "stop") => json!({"decision": "deny", "reason": text}),
        ("codex" | "copilot" | "grok", "stop") => block(),
        ("cursor", "stop") => json!({ "followup_message": text }),
        ("cursor", _) => json!({ "additional_context": text }),
        ("copilot", _) => json!({ "additionalContext": text }),
        _ => return text.to_string(),
    }
    .to_string();
    // On Windows agents run hooks through PowerShell, which can re-encode what passes through it: JSON in ASCII is
    // safe from that.
    if !cfg!(windows) {
        return out;
    }
    out.chars()
        .map(|c| {
            if c.is_ascii() {
                c.to_string()
            } else {
                c.encode_utf16(&mut [0; 2]).iter().map(|u| format!("\\u{u:04x}")).collect()
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::{codex_home, codex_keeps_no_transcript, format_for, grok_ignores, takes_mail};
    use serde_json::json;

    #[test]
    fn formats() {
        assert!(format_for("codex", "post-tool-use", "tool", "hi").contains("\"PostToolUse\""));
        assert!(format_for("cmd", "session-start", "start", "hi").contains("\"SessionStart\""));
        assert!(format_for("cmd", "post-tool-use", "tool", "hi").contains("\"PostToolUse\""));
        assert_eq!(format_for("cmd", "stop", "stop", "hi"), r#"{"decision":"block","reason":"hi"}"#);
        assert_eq!(format_for("codex", "stop", "stop", "hi"), r#"{"decision":"block","reason":"hi"}"#);
        assert_eq!(format_for("cursor", "stop", "stop", "hi"), r#"{"followup_message":"hi"}"#);
        assert_eq!(format_for("cursor", "post-tool-use", "tool", "hi"), r#"{"additional_context":"hi"}"#);
        assert_eq!(format_for("copilot", "session-start", "start", "hi"), r#"{"additionalContext":"hi"}"#);
        assert!(format_for("gemini", "user-prompt-submit", "prompt", "hi").contains("BeforeAgent"));
        assert!(format_for("grok", "post-tool-use", "tool", "hi").contains("\"PostToolUse\""));
        assert_eq!(format_for("grok", "stop", "stop", "hi"), r#"{"decision":"block","reason":"hi"}"#);
        assert_eq!(format_for("opencode", "poll", "stop", "hi"), "hi");
    }

    #[test]
    fn a_codex_home_is_named_only_as_the_real_path_of_a_folder_that_is_there() {
        let dir = std::env::temp_dir().join(format!("reason-network-codex-home-{}", std::process::id()));
        let home = dir.join("codex-b");
        std::fs::create_dir_all(&home).unwrap();
        std::fs::create_dir_all(dir.join("x")).unwrap();
        std::fs::write(dir.join("file"), "").unwrap();
        let real = std::fs::canonicalize(&home).unwrap();
        let real = real.to_str().unwrap();
        let real = real.strip_prefix(r"\\?\").unwrap_or(real);
        let named = |p: &std::path::Path| codex_home(Some(p.as_os_str().to_owned()));
        assert_eq!(named(&home).as_deref(), Some(real));
        // By another path to it: its real one (macOS's temp folder is under a link, `/var` to `/private/var`).
        assert_eq!(named(&dir.join("x").join("..").join("codex-b")).as_deref(), Some(real));
        // Not a folder, not there, not a full path, or empty: none, so the daemon's own home.
        assert_eq!(named(&dir.join("file")), None);
        assert_eq!(named(&dir.join("missing")), None);
        assert_eq!(named(std::path::Path::new("codex-b")), None);
        assert_eq!(codex_home(Some("".into())), None);
        assert_eq!(codex_home(None), None);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_codex_thread_whose_transcript_is_null_keeps_none() {
        assert!(codex_keeps_no_transcript(&json!({ "session_id": "t", "transcript_path": null })));
        assert!(!codex_keeps_no_transcript(
            &json!({ "session_id": "t", "transcript_path": "/h/.codex/sessions/rollout-t.jsonl" })
        ));
        assert!(!codex_keeps_no_transcript(&json!({ "session_id": "t" })));
        assert!(!codex_keeps_no_transcript(&serde_json::Value::Null));
    }

    #[test]
    fn grok_payloads() {
        assert!(!takes_mail("grok", "session-start"));
        assert!(!takes_mail("grok", "user-prompt-submit"));
        assert!(takes_mail("grok", "post-tool-use"));
        assert!(takes_mail("grok", "stop"));
        assert!(takes_mail("codex", "session-start"));
        assert!(!grok_ignores("stop", &json!({ "reason": "end_turn" })));
        assert!(grok_ignores("stop", &json!({ "reason": "shutdown" })));
        assert!(grok_ignores("stop", &json!({ "reason": "channel_closed" })));
        assert!(!grok_ignores("session-end", &json!({ "reason": "shutdown" })));
        assert!(!grok_ignores("post-tool-use", &json!({ "subagentType": null })));
        assert!(grok_ignores("post-tool-use", &json!({ "subagentType": "general-purpose" })));
    }
}
