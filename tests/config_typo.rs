//! A malformed config must not stop the daemon or make private folders register.
#![cfg(unix)]
use serde_json::{Value, json};
use std::{
    fs,
    io::{BufRead, BufReader, Write},
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
    sync::mpsc,
    time::{Duration, Instant},
};
struct Home(PathBuf);
impl Drop for Home {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}
struct Daemon(Child);
impl Drop for Daemon {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}
/// The binary, keeping its state in `home` and starting no service, updater, tray or mDNS.
fn reason_network(home: &Path) -> Command {
    let mut c = Command::new(env!("CARGO_BIN_EXE_reason-network"));
    c.env("HOME", home)
        .env("REASON_NETWORK_HOME", home)
        .env("REASON_NETWORK_NO_UPDATE", "1")
        .env("REASON_NETWORK_NO_TRAY", "1")
        .env("REASON_NETWORK_NO_MDNS", "1")
        .env("REASON_NETWORK_NO_SERVICE", "1");
    c
}
#[test]
fn broken_config_keeps_the_daemon_available_and_warns_clients() {
    for (case, config) in [("syntax", "[inbound\n"), ("type", "port=\"41641\"\n")] {
        let home = Home(std::env::temp_dir().join(format!("rn-config-private-{}-{case}", std::process::id())));
        fs::create_dir_all(&home.0).unwrap();
        fs::write(home.0.join("config.toml"), config).unwrap();
        let command = || reason_network(&home.0);
        let mut daemon = Daemon(command().arg("daemon").stdout(Stdio::null()).stderr(Stdio::null()).spawn().unwrap());
        let deadline = Instant::now() + Duration::from_secs(10);
        while !home.0.join("network.sock").exists() && Instant::now() < deadline {
            assert!(daemon.0.try_wait().unwrap().is_none());
            std::thread::sleep(Duration::from_millis(20));
        }
        assert!(home.0.join("network.sock").exists());
        let who = command().arg("who").output().unwrap();
        assert!(who.status.success(), "{}", String::from_utf8_lossy(&who.stderr));
        let output = String::from_utf8_lossy(&who.stdout);
        assert!(output.contains("! Reason Network"));
        assert!(output.contains("ask your user to fix"));
        assert_eq!(output.contains("Reason Network is paused"), case == "syntax");
        // Warnings stay on stderr. Hosts may parse stdout as JSON, so it must remain
        // empty when the paused folder produces no hook response.
        for host in [
            "codex",
            "cmd",
            "grok",
            "gemini",
            "cursor",
            "copilot",
            "antigravity",
            "opencode",
            "claude-code",
            "pi",
            "hermes",
        ] {
            for event in [
                "session-start",
                "user-prompt-submit",
                "post-tool-use",
                "stop",
                "session-end",
                "status",
                "poll",
                "doing",
            ] {
                let hook = command()
                    .args(["hook", host, event, "--session", "private-test"])
                    .stdin(Stdio::null())
                    .output()
                    .unwrap();
                assert!(hook.status.success(), "{host} {event}");
                if case == "syntax" {
                    assert!(hook.stdout.is_empty(), "{host} {event}: {:?}", hook.stdout);
                } else if !hook.stdout.is_empty()
                    && (matches!(host, "cursor" | "copilot")
                        || matches!(
                            (host, event),
                            (
                                "codex" | "gemini",
                                "session-start" | "user-prompt-submit" | "post-tool-use" | "stop" | "poll" | "status"
                            ) | ("cmd", "session-start" | "post-tool-use" | "stop" | "poll" | "status")
                                | ("grok", "post-tool-use" | "stop" | "poll" | "status")
                                | ("antigravity", "session-start" | "user-prompt-submit" | "stop" | "poll" | "status")
                        ))
                {
                    let _: serde_json::Value = serde_json::from_slice(&hook.stdout)
                        .unwrap_or_else(|e| panic!("{host} {event}: {e}: {:?}", hook.stdout));
                }
                assert!(!String::from_utf8_lossy(&hook.stdout).contains("settings could not be read"));
                assert!(String::from_utf8_lossy(&hook.stderr).contains("fix"), "{host} {event}");
            }
        }
        assert!(daemon.0.try_wait().unwrap().is_none());
        assert_eq!(fs::read_to_string(home.0.join("config.toml")).unwrap(), config);
    }
}

/// What a `list_team` call says when its MCP server starts in `folder`.
fn mcp_says(home: &Path, folder: &Path) -> String {
    let mut server = Daemon(
        reason_network(home)
            .arg("mcp")
            .current_dir(folder)
            .env_remove("CLAUDE_PROJECT_DIR")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .unwrap(),
    );
    let mut stdin = server.0.stdin.take().unwrap();
    let stdout = server.0.stdout.take().unwrap();
    let (lines, replies) = mpsc::channel();
    std::thread::spawn(move || {
        for line in BufReader::new(stdout).lines().map_while(Result::ok) {
            let _ = lines.send(line);
        }
    });
    let reply_to = |id: u64| loop {
        let line = replies.recv_timeout(Duration::from_secs(10)).expect("no reply from the MCP server");
        let reply: Value = serde_json::from_str(&line).unwrap();
        if reply["id"] == id {
            return reply;
        }
    };
    let mut send = |message: Value| writeln!(stdin, "{message}").unwrap();
    let client = json!({"name": "codex-mcp-client", "version": "1"});
    let init = json!({"protocolVersion": "2025-06-18", "capabilities": {}, "clientInfo": client});
    send(json!({"jsonrpc": "2.0", "id": 1, "method": "initialize", "params": init}));
    reply_to(1);
    send(json!({"jsonrpc": "2.0", "method": "notifications/initialized"}));
    send(json!({"jsonrpc": "2.0", "id": 2, "method": "tools/call", "params": {"name": "list_team", "arguments": {}}}));
    let text = reply_to(2)["result"]["content"][0]["text"].as_str().unwrap().to_string();
    // In a folder that is on, the server starts a daemon: don't leave it running.
    if home.join("network.sock").exists() {
        let _ = reason_network(home).arg("stop").output();
    }
    text
}

#[test]
fn mcp_tools_blame_the_file_only_when_the_user_turned_nothing_off() {
    let home = Home(std::env::temp_dir().join(format!("rn-config-mcp-{}", std::process::id())));
    let folder = home.0.join("private");
    fs::create_dir_all(&folder).unwrap();
    let off = format!("[folders]\n\"{}\" = \"off\"\n", folder.display());
    let says = |config: &str| {
        fs::write(home.0.join("config.toml"), config).unwrap();
        mcp_says(&home.0, &folder)
    };
    // The file can't be read, so every folder is off. The user turned none off, and is the one to fix the file.
    let text = says("[inbound\n");
    assert!(text.starts_with("Reason Network is paused: ask your user to fix"), "{text}");
    assert!(!text.contains("turned Reason Network off"), "{text}");
    // A folder the user turned off says so.
    assert_eq!(says(&off), "Your user turned Reason Network off for this folder");
    // Also when another setting can't be read: the warning follows.
    let text = says(&format!("port = \"41641\"\n{off}"));
    let said = "Your user turned Reason Network off for this folder; Reason Network could not read some settings:";
    assert!(text.starts_with(said), "{text}");
    // A folder that is off only because its own entry can't be read wasn't turned off by the user either.
    let text = says(&format!("[folders]\n\"{}\" = true\n", folder.display()));
    assert!(text.starts_with("Reason Network could not read some settings: ask your user to fix"), "{text}");
}

#[test]
fn an_agents_shell_is_told_the_file_is_unreadable_not_that_its_folder_was_turned_off() {
    let home = Home(std::env::temp_dir().join(format!("rn-config-send-{}", std::process::id())));
    fs::create_dir_all(&home.0).unwrap();
    fs::write(home.0.join("config.toml"), "[inbound\n").unwrap();
    let send = reason_network(&home.0).args(["send", "alice/work", "hello"]).env("AI_AGENT", "1").output().unwrap();
    // It must stop before reaching a daemon; if it did start one, don't leave it running.
    let started = home.0.join("network.sock").exists();
    if started {
        let _ = reason_network(&home.0).arg("stop").output();
    }
    let stderr = String::from_utf8_lossy(&send.stderr);
    let error = stderr.lines().find(|l| l.starts_with("Error:")).unwrap_or_default();
    assert!(!send.status.success() && !started, "{stderr}");
    assert!(error.contains("ask your user to fix") && !error.contains("turned Reason Network off"), "{stderr}");
}
