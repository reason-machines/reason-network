use super::*;

fn gaps(exe: &Path, value: &Value) -> Vec<String> {
    let mut gaps = Vec::new();
    // A path is the same whatever its case on Windows: the settings say `D:\program files\…` when that is how the
    // program was started, and `doctor` may be run as `D:\Program Files\…`.
    if !value["mcpServers"][AGENT_NAME]["command"].as_str().is_some_and(|c| same_path(c, exe)) {
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
            .any(|h| h["command"].as_str().is_some_and(|c| same_text(c, &command)) && h["type"] == "command");
        if !present {
            gaps.push(format!("{event} hook is missing or outdated"));
        }
        if ["hooksConfig", "hooks"].iter().any(|section| {
            value[section]["disabled"]
                .as_array()
                .into_iter()
                .flatten()
                .any(|v| v.as_str().is_some_and(|v| v == name || same_text(v, &command)))
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

#[cfg(test)]
mod tests {
    use super::*;

    /// What `setup` writes for a Gemini CLI that runs `exe`.
    fn settings(exe: &Path) -> Value {
        let hook = |call: &str| {
            json!([{ "matcher": "*", "hooks": [{
                "type": "command",
                "name": format!("{AGENT_NAME}-{call}"),
                "command": shell_cmd(exe, &format!("hook gemini {call}")),
            }] }])
        };
        let mut servers = serde_json::Map::new();
        servers.insert(AGENT_NAME.into(), json!({ "command": exe.display().to_string() }));
        json!({
            "mcpServers": servers,
            "hooks": {
                "SessionStart": hook("session-start"),
                "BeforeAgent": hook("user-prompt-submit"),
                "AfterTool": hook("post-tool-use"),
                "AfterAgent": hook("stop"),
                "SessionEnd": hook("session-end"),
            },
        })
    }

    #[test]
    fn settings_written_for_this_binary_have_no_gaps() {
        let exe = Path::new(if cfg!(windows) {
            r"D:\program files\reason-network\reason-network.exe"
        } else {
            "/opt/reason-network/reason-network"
        });
        assert_eq!(gaps(exe, &settings(exe)), Vec::<String>::new());
        assert_eq!(
            gaps(Path::new("/elsewhere/reason-network"), &settings(exe)).len(),
            6,
            "another binary: the server and every hook"
        );
    }

    #[test]
    fn a_path_in_another_case_is_the_same_path_on_windows_only() {
        let (written, asked) = if cfg!(windows) {
            (
                Path::new(r"D:\program files\reason-network\reason-network.exe"),
                Path::new(r"D:\Program Files\reason-network\reason-network.exe"),
            )
        } else {
            (Path::new("/opt/reason-network/reason-network"), Path::new("/opt/Reason-Network/reason-network"))
        };
        // The same program started as `D:\Program Files\…` is no outdated copy of the one the settings name.
        let found = gaps(asked, &settings(written));
        if cfg!(windows) {
            assert!(found.is_empty(), "{found:?}");
        } else {
            assert_eq!(found.len(), 6, "paths differ by case on this system: {found:?}");
        }
    }

    #[test]
    fn a_disabled_hook_is_found_in_either_case_too() {
        let exe = Path::new(if cfg!(windows) {
            r"D:\program files\reason-network\reason-network.exe"
        } else {
            "/opt/reason-network/reason-network"
        });
        let mut value = settings(exe);
        value["hooksConfig"] = json!({ "disabled": [format!("{AGENT_NAME}-stop")] });
        assert_eq!(gaps(exe, &value), vec!["AfterAgent hook is disabled".to_string()]);
    }
}
