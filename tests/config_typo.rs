//! A malformed config must not stop the daemon or make private folders register.
#![cfg(unix)]
use std::{
    fs,
    path::PathBuf,
    process::{Child, Command, Stdio},
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
#[test]
fn broken_config_keeps_the_daemon_available_and_warns_clients() {
    for (case, config) in [("syntax", "[inbound\n"), ("type", "port=\"41641\"\n")] {
        let home = Home(std::env::temp_dir().join(format!("rn-config-private-{}-{case}", std::process::id())));
        fs::create_dir_all(&home.0).unwrap();
        fs::write(home.0.join("config.toml"), config).unwrap();
        let command = || {
            let mut c = Command::new(env!("CARGO_BIN_EXE_reason-network"));
            c.env("HOME", &home.0)
                .env("REASON_NETWORK_HOME", &home.0)
                .env("REASON_NETWORK_NO_UPDATE", "1")
                .env("REASON_NETWORK_NO_TRAY", "1")
                .env("REASON_NETWORK_NO_MDNS", "1")
                .env("REASON_NETWORK_NO_SERVICE", "1");
            c
        };
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
