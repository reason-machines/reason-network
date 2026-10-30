//! ZCode has no hook-output acceptance callback. Lease mail with one retry; explicit inbox reads recover it.
use super::*;
use crate::render;

const LIMIT: Duration = Duration::from_millis(1500);
pub(crate) const OUTPUT_LIMIT: usize = 24 * 1024;

pub(crate) fn output_limit() -> usize {
    let path = crate::paths::user_home().join(".zcode/cli/config.json");
    match std::fs::read(&path) {
        Ok(bytes) => serde_json::from_slice::<Value>(&bytes).ok().map_or(0, |v| {
            v.pointer("/hooks/maxOutputBytes")
                .map_or(OUTPUT_LIMIT, |n| n.as_u64().map_or(0, |n| n.min(OUTPUT_LIMIT as u64) as usize))
        }),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => OUTPUT_LIMIT,
        Err(_) => 0,
    }
}

fn fits(text: &str, limit: usize) -> bool {
    // Include the newline written by the subprocess, not just the JSON itself.
    format_for("zcode", "post-tool-use", "tool", text).len() < limit
}

/// The process executor has no `env` field. Re-exec only this fixed hook with its configured state directory.
pub(crate) async fn at_home(event: &str, home: std::path::PathBuf) {
    if !matches!(event, "session-start" | "post-tool-use") {
        return;
    }
    let Ok(exe) = crate::util::current_exe() else { return };
    let mut child = tokio::process::Command::new(exe);
    child.args(["hook", "zcode", event]).env("REASON_NETWORK_HOME", home).kill_on_drop(true);
    // The short-lived state wrapper is not the host process that keeps the session alive.
    if let Some(pid) = agent::hook_agent_pid() {
        child.env("REASON_NETWORK_HOOK_PID", pid.to_string());
    }
    #[cfg(windows)]
    child.creation_flags(windows_sys::Win32::System::Threading::CREATE_NO_WINDOW);
    let _ = tokio::time::timeout(Duration::from_millis(2500), child.status()).await;
}

pub(super) async fn run(event: &str) {
    // Host stdin is a single JSON line. A bounded reader also handles a host that leaves stdin open.
    let input = read_input();
    if let Ok(Some(out)) = tokio::time::timeout(LIMIT, handle(&Link::existing(), event, &input)).await {
        let _ = writeln!(std::io::stdout(), "{out}");
    }
}

fn read_input() -> String {
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        use std::io::BufRead;
        let mut line = String::new();
        let _ = std::io::BufReader::new(std::io::stdin().take(1024 * 1024)).read_line(&mut line);
        let _ = tx.send(line);
    });
    rx.recv_timeout(Duration::from_millis(300)).unwrap_or_default()
}

fn payload(event: &str, input: &str) -> Option<HookCall> {
    let name = match event {
        "session-start" => "SessionStart",
        "post-tool-use" => "PostToolUse",
        _ => return None,
    };
    let v: Value = serde_json::from_str(input.trim_start_matches('\u{feff}')).ok()?;
    let field = |snake: &str, camel: &str| {
        v.get(snake)
            .or_else(|| v.get(camel))
            .and_then(Value::as_str)
            .filter(|s| !s.trim().is_empty())
            .map(str::to_string)
    };
    if field("hook_event_name", "hookEventName")?.as_str() != name {
        return None;
    }
    Some(HookCall {
        through_reason: Some(false),
        agent: "zcode".into(),
        event: if event == "session-start" { "start" } else { "tool" }.into(),
        session_id: field("session_id", "sessionId")?,
        cwd: Some(field("cwd", "cwd")?),
        pid: crate::paths::env_var("HOOK_PID").ok().and_then(|p| p.parse().ok()).or_else(agent::hook_agent_pid),
        // SessionStart registers identity only. No pending mail is consumed by a hook.
        mail: event == "post-tool-use",
        // ZCode's transcript is a temporary event snapshot, not a persistent session log.
        transcript: None,
        doing: None,
        name: None,
        dirs: Vec::new(),
        keep: false,
    })
}

async fn handle(link: &std::sync::Arc<Link>, event: &str, input: &str) -> Option<String> {
    let call = payload(event, input)?;
    let off = crate::config::Config::load_strict().unwrap_or_default().off_in(call.cwd.as_deref()?);
    let reply: HookReply = link.call(Request::Hook(call)).await.ok()?;
    if off {
        return None;
    }
    let text = match reply.text {
        Some(text) => text,
        None if event == "session-start" => format!(
            "Reason Network: you are {}. Messages arrive after successful tools while you work, with at most one retry. Use check_inbox to recover pending or oversized messages. Reply only when needed, using the incoming id as reply_to. Idle sessions are not woken.",
            reply.address?
        ),
        _ => return None,
    };
    let out = format_for("zcode", event, if event == "session-start" { "start" } else { "tool" }, &text);
    (out.len() < output_limit()).then_some(out)
}

/// The first oversized message holds later messages for check_inbox, even while its reminder is leased.
pub(crate) fn before_oversized(mut messages: Vec<Message>, limit: usize) -> Vec<Message> {
    if let Some(at) =
        messages.iter().position(|m| !fits(&render::for_agent(std::slice::from_ref(m), Some("zcode"), false), limit))
    {
        messages.truncate(at + 1);
    }
    messages
}

/// Return complete framed messages, or a small recovery notice for an oversized head of the queue.
pub(crate) fn batch(messages: Vec<Message>, limit: usize) -> (Vec<String>, Option<String>) {
    let mut selected = Vec::new();
    for message in messages {
        selected.push(message);
        let text = render::for_agent(&selected, Some("zcode"), false);
        if !fits(&text, limit) {
            let oversized = selected.pop().expect("just added");
            if selected.is_empty() {
                let notice =
                    "Reason Network: unread messages exceed this hook's output limit. Use check_inbox to read them.";
                return if fits(notice, limit) { (vec![oversized.id], Some(notice.into())) } else { (vec![], None) };
            }
            break;
        }
    }
    let ids = selected.iter().map(|m| m.id.clone()).collect();
    let text = (!selected.is_empty()).then(|| render::for_agent(&selected, Some("zcode"), false));
    (ids, text)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn validates_real_session_and_event_in_both_host_spellings() {
        let a =
            payload("session-start", r#"{"session_id":"one","cwd":"C:\\用户 空格","hook_event_name":"SessionStart"}"#)
                .unwrap();
        let b = payload("post-tool-use", r#"{"sessionId":"two","cwd":"C:\\用户 空格","hookEventName":"PostToolUse"}"#)
            .unwrap();
        assert_eq!(a.session_id, "one");
        assert_eq!(b.session_id, "two");
        assert_ne!(format!("zcode:{}", a.session_id), format!("zcode:{}", b.session_id));
        assert_eq!(a.cwd.as_deref(), Some(r"C:\用户 空格"));
        assert!(!a.mail);
        assert!(b.mail);
        assert_eq!(a.through_reason, Some(false));
        for bad in [
            "{}",
            "{",
            r#"{"session_id":"","cwd":"/","hook_event_name":"SessionStart"}"#,
            r#"{"session_id":"s","cwd":"/","hook_event_name":"Stop"}"#,
        ] {
            assert!(payload("session-start", bad).is_none());
        }
        assert!(payload("stop", "{}").is_none());
    }

    #[test]
    fn stdout_is_context_json_and_preserves_unicode() {
        for (event, ev, expected) in
            [("session-start", "start", "SessionStart"), ("post-tool-use", "tool", "PostToolUse")]
        {
            let out = format_for("zcode", event, ev, "你好 \"消息\" 🦀\nnext");
            let v: Value = serde_json::from_str(&out).unwrap();
            assert_eq!(v["hookSpecificOutput"]["hookEventName"], expected);
            assert_eq!(v["hookSpecificOutput"]["additionalContext"], "你好 \"消息\" 🦀\nnext");
            assert!(v.get("decision").is_none());
        }
    }

    #[test]
    fn batching_keeps_json_complete_and_leaves_large_mail_for_the_inbox() {
        let message = |id: &str, body: String| Message {
            id: id.into(),
            from: "a/codex".into(),
            to: "b/zcode".into(),
            from_agent: Some("codex".into()),
            from_repo: None,
            via: None,
            reply_to: None,
            body,
            wake: true,
            notify: false,
            sent_at: 0,
            approval: None,
            files: Vec::new(),
        };
        let messages =
            vec![message("big", "中".repeat(64 * 1024)), message("one", "你好".into()), message("two", "hello".into())];
        let (ids, text) = batch(before_oversized(messages, 512), 512);
        assert_eq!(ids, ["big"]);
        let text = text.unwrap();
        assert!(text.contains("check_inbox"));
        assert!(!text.contains("hello"));
        assert!(fits(&text, 512));
        let (ids, text) = batch(vec![message("one", "你好".into())], OUTPUT_LIMIT);
        assert_eq!(ids, ["one"]);
        let out = format_for("zcode", "post-tool-use", "tool", &text.unwrap());
        let v: Value = serde_json::from_str(&out).unwrap();
        assert!(v["hookSpecificOutput"]["additionalContext"].as_str().unwrap().contains("你好"));
        assert_eq!(batch(vec![message("one", "hello".into())], 1), (vec![], None));
    }
}
