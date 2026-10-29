use super::*;

fn gaps(exe: &Path, value: &Value) -> Vec<String> {
    let mut gaps = Vec::new();
    if value["mcpServers"][AGENT_NAME]["command"].as_str() != exe.to_str() {
        gaps.push("MCP executable is missing or outdated".into());
    }
    if value["hooksConfig"]["enabled"] == false || value["hooks"]["enabled"] == false {
        gaps.push("hooks are disabled".into());
    }
    for (event, call) in [
        ("SessionStart", "session-start"),
        ("BeforeAgent", "user-prompt-submit"),
        ("AfterTool", "post-tool-use"),
        ("AfterAgent", "stop"),
        ("SessionEnd", "session-end"),
    ] {
        let command = shell_cmd(exe, &format!("hook gemini {call}"));
        let name = format!("{AGENT_NAME}-{call}");
        let present = value["hooks"][event]
            .as_array()
            .into_iter()
            .flatten()
            .flat_map(|g| g["hooks"].as_array().into_iter().flatten())
            .any(|h| h["command"] == command && h["type"] == "command");
        if !present {
            gaps.push(format!("{event} hook is missing or outdated"));
        }
        if ["hooksConfig", "hooks"].iter().any(|section| {
            value[section]["disabled"].as_array().into_iter().flatten().any(|v| v == &name || v == &command)
        }) {
            gaps.push(format!("{event} hook is disabled"));
        }
    }
    gaps
}

pub(super) fn wired(exe: &Path) -> bool {
    read_json(&gemini_home().join("settings.json")).is_ok_and(|v| gaps(exe, &v).is_empty())
}

pub(super) async fn doctor(exe: &Path, link: &std::sync::Arc<Link>) {
    if !gemini_home().exists() && agent::find_bin("gemini").is_none() {
        return;
    }
    match read_json(&gemini_home().join("settings.json")) {
        Ok(value) => {
            for gap in gaps(exe, &value) {
                println!("! Gemini CLI: {gap}");
            }
        }
        Err(_) => println!("! Gemini CLI: settings could not be read"),
    }
    // Project/system settings and runtime folder trust can override user settings; never claim to have verified them.
    println!("  Gemini CLI: confirm the project is trusted in /permissions and hooks are enabled in /hooks panel.");
    println!("  Active sessions receive mail at a tool/turn boundary; idle sessions wait for the next prompt.");
    if let Ok(health) = link.call::<Value>(Request::Diagnostics).await {
        let sessions =
            health["sessions"].as_array().into_iter().flatten().filter(|s| s["agent"] == "gemini").collect::<Vec<_>>();
        if sessions.is_empty() {
            println!("  Gemini CLI: no live session observed; hook execution is unverified.");
        }
        for session in sessions {
            println!(
                "  Gemini CLI: {} / {}; session linked: {}; last hook: {}; unread: {}",
                session["status"].as_str().unwrap_or("unknown"),
                session["tier"].as_str().unwrap_or("unknown"),
                session["sessionLinked"],
                session["hookAgeMs"]
                    .as_i64()
                    .map_or_else(|| "never observed".into(), |age| format!("{}s ago", age / 1000)),
                session["unread"]
            );
        }
    } else {
        println!("  Gemini CLI: this daemon cannot report hook health; update/restart Reason Network.");
    }
}
