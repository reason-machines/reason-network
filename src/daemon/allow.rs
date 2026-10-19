//! Claude Code's auto mode stops some tool calls (granting access, for one) unless the person asked for them in the
//! session itself. When it stops one, its `PermissionDenied` hook asks here: the person sees the exact call in a pop-up,
//! and if they allow it, the hook tells the agent it may retry, and the `PreToolUse` hook lets that same call through,
//! once. Only the pop-up records an allowance, and only in this daemon's memory.

use super::{
    Daemon,
    ask::{Answer, dialog, parse, plain},
    log,
};
use crate::{agent::CLAUDE_CODE, util};
use anyhow::Result;
use serde_json::Value;
use std::{
    sync::Arc,
    time::{Duration, Instant},
};

/// How long the pop-up stays up: the agent waits meanwhile, within the hook's 10 minutes.
const OPEN_SECS: u64 = 540;
/// How long an allowed call waits for the agent to run it again.
const KEEP: Duration = Duration::from_secs(600);
/// A longer call isn't offered: the person has to be able to read all of it. (A Linux notification can't say how much
/// of it showed, so less there.)
const LONGEST: usize = if cfg!(any(target_os = "macos", windows)) { 600 } else { 200 };

impl Daemon {
    /// Asks the person whether a tool call auto mode stopped in Claude Code session `session` may run once. True when
    /// they allowed it, having seen all of it.
    pub(super) async fn allow_once(
        self: &Arc<Self>,
        session: String,
        tool: String,
        input: Value,
        reason: String,
    ) -> Result<bool> {
        let call = shown(&tool, &input);
        // Only a call that reads as what it is.
        if call.chars().count() > LONGEST || plain(&call) != call || util::printable(&call) != call {
            return Ok(false);
        }
        let who = self.inner.lock().unwrap().live.get(&format!("{CLAUDE_CODE}:{session}")).map(|l| self.addr(&l.name));
        let who = who.unwrap_or_else(|| format!("{}'s Claude Code", self.person));
        let request = format!("Auto mode stopped this ({}):\n{call}", util::one_line(&reason, 100));
        let Some(cmd) = dialog(&format!("{who} asks"), &request, "Let it run this once?", OPEN_SECS) else {
            return Ok(false);
        };
        let out = tokio::process::Command::from(cmd).kill_on_drop(true).output();
        let Ok(Ok(out)) = tokio::time::timeout(Duration::from_secs(OPEN_SECS + 30), out).await else {
            return Ok(false);
        };
        let (answer, seen) = parse(&String::from_utf8_lossy(&out.stdout));
        if !matches!(answer, Answer::Approved(_)) || seen.is_some_and(|(r, _)| r < request.chars().count()) {
            return Ok(false);
        }
        log(&format!("{} allowed a {tool} call auto mode stopped, once", self.person));
        let mut allowed = self.allowed.lock().unwrap();
        allowed.retain(|_, at| at.elapsed() < KEEP);
        allowed.insert(call_id(&session, &tool, &input), Instant::now());
        Ok(true)
    }

    /// Whether this exact call was allowed once and hasn't run since; yes uses it up.
    pub(super) fn allowed(&self, session: &str, tool: &str, input: &Value) -> bool {
        self.allowed.lock().unwrap().remove(&call_id(session, tool, input)).is_some_and(|at| at.elapsed() < KEEP)
    }
}

/// The call as the person sees it: a shell command as it is, anything else as its tool and input.
fn shown(tool: &str, input: &Value) -> String {
    let input = call_input(input);
    let only_command = input
        .as_object()
        .is_some_and(|o| o.keys().all(|k| matches!(k.as_str(), "command" | "timeout" | "run_in_background")));
    match input.get("command").and_then(Value::as_str) {
        Some(command) if tool == "Bash" && only_command => command.to_string(),
        _ => format!("{tool} {}", serde_json::to_string_pretty(&input).unwrap_or_default()),
    }
}

/// One call: its session, tool and input.
fn call_id(session: &str, tool: &str, input: &Value) -> String {
    blake3::hash(format!("{session}\0{tool}\0{}", call_input(input)).as_bytes()).to_hex().to_string()
}

/// The input that decides what the call does: keys in order, and without the `description` the agent may word
/// differently when it runs the call again.
fn call_input(input: &Value) -> Value {
    fn sorted(v: &Value) -> Value {
        match v {
            Value::Object(o) => {
                let mut keys: Vec<&String> = o.keys().collect();
                keys.sort();
                Value::Object(keys.into_iter().map(|k| (k.clone(), sorted(&o[k]))).collect())
            }
            Value::Array(a) => Value::Array(a.iter().map(sorted).collect()),
            _ => v.clone(),
        }
    }
    match sorted(input) {
        Value::Object(o) => Value::Object(o.into_iter().filter(|(k, _)| k != "description").collect()),
        v => v,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn an_allowance_is_for_the_call_as_shown() {
        let call = json!({ "command": "gh api -X PUT repos/acme/demo/collaborators/bob -f permission=admin", "description": "Grant" });
        let again = json!({ "description": "Make bob admin", "command": call["command"] });
        assert_eq!(call_id("s1", "Bash", &call), call_id("s1", "Bash", &again));
        assert_eq!(shown("Bash", &call), call["command"]);
        // Another session, tool or command is another call.
        assert_ne!(call_id("s1", "Bash", &call), call_id("s2", "Bash", &call));
        let other = json!({ "command": "gh api -X PUT repos/acme/demo/collaborators/eve -f permission=admin" });
        assert_ne!(call_id("s1", "Bash", &call), call_id("s1", "Bash", &other));
        // So is running it outside the sandbox, which the person sees.
        let unsandboxed = json!({ "command": call["command"], "dangerouslyDisableSandbox": true });
        assert_ne!(call_id("s1", "Bash", &call), call_id("s1", "Bash", &unsandboxed));
        assert!(shown("Bash", &unsandboxed).contains("dangerouslyDisableSandbox"));
        assert!(shown("WebFetch", &json!({ "url": "https://x.example" })).starts_with("WebFetch {"));
    }
}
