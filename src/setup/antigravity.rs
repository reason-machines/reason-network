//! Native contracts: antigravity.google/docs/{hooks,mcp,sidecars}. PostTool cannot inject messages.
use super::*;

fn config_dir() -> PathBuf {
    home().join(".gemini/config")
}
pub(super) fn installed() -> bool {
    config_dir().exists() || agent::find_bin("antigravity").is_some()
}
pub(super) fn wired(exe: &Path) -> bool {
    ["mcp_config.json", "hooks.json"].iter().all(|p| has(config_dir().join(p), &me(exe)))
}

pub(super) fn apply(exe: &Path, remove: bool, dry: bool) -> Result<Option<Report>> {
    if !installed() {
        return Ok(None);
    }
    let dir = config_dir();
    let mut report = Report { agent: "Antigravity", lines: Vec::new(), notes: Vec::new() };
    let mcp = dir.join("mcp_config.json");
    let mut value = read_json(&mcp)?;
    let servers = obj(obj(&mut value)?.entry("mcpServers").or_insert_with(|| json!({})))?;
    if remove {
        servers.remove(AGENT_NAME);
    } else {
        servers.insert(AGENT_NAME.into(), json!({"command": exe.display().to_string(), "args": ["mcp"], "env": {"REASON_NETWORK_AGENT": "antigravity"}}));
    }
    report.lines.push(changed(&mcp, write_json(&mcp, &value, dry)?));
    let hooks = dir.join("hooks.json");
    let mut value = read_json(&hooks)?;
    let definitions = obj(&mut value)?;
    if remove {
        definitions.remove(AGENT_NAME);
    } else {
        let enabled =
            definitions.get(AGENT_NAME).and_then(|value| value.get("enabled")).cloned().unwrap_or(json!(true));
        let handler = |event| json!({"type":"command","command":shell_cmd(exe,&format!("hook antigravity {event}")),"timeout":15});
        definitions.insert(
            AGENT_NAME.into(),
            json!({
                "enabled":enabled, "SessionStart":[handler("session-start")],
                "PreInvocation":[handler("user-prompt-submit")],
                "PostToolUse":[{"matcher":"*","hooks":[handler("post-tool-use")]}],
                "Stop":[handler("stop")],
            }),
        );
    }
    report.lines.push(changed(&hooks, write_json(&hooks, &value, dry)?));
    let sidecar = dir.join("sidecars/reason-network/sidecar.json");
    let config = dir.join("config.json");
    let mut value = read_json(&config)?;
    let sidecars = obj(obj(&mut value)?.entry("sidecars").or_insert_with(|| json!({})))?;
    if remove {
        sidecars.remove(AGENT_NAME);
        if !dry && sidecar.exists() {
            std::fs::remove_file(&sidecar)?;
        }
    } else {
        let data = json!({"command": exe.display().to_string(), "args": ["antigravity-sidecar"], "restart_policy": "on-failure", "display_name": "Reason Network", "description": "Deliver messages to your existing agent conversations"});
        report.lines.push(changed(&sidecar, write_json(&sidecar, &data, dry)?));
        // Preserve an explicit decision to disable this sidecar.
        sidecars.entry(AGENT_NAME).or_insert_with(|| json!({"enabled": true}));
        report.notes.push("Reload Antigravity's MCP configuration and start a new conversation. App idle delivery needs the Reason Network sidecar enabled; CLI conversations receive mail at their next invocation.".into());
    }
    report.lines.push(changed(&config, write_json(&config, &value, dry)?));
    Ok(Some(report))
}
