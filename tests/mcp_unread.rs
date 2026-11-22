//! Unread notices must agree with the tools the MCP client was shown.
#![cfg(unix)]

use serde_json::{Value, json};
use std::{
    fs,
    io::{BufRead, BufReader, Write},
    os::unix::net::UnixListener,
    path::PathBuf,
    process::{Child, Command, Stdio},
    sync::mpsc,
    time::Duration,
};

struct Home(PathBuf);
impl Drop for Home {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}
struct Server(Child);
impl Drop for Server {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

#[test]
fn unread_notice_obeys_the_tool_list_even_before_the_first_listing() {
    for (pushed, listed) in [(true, true), (false, true), (true, false), (false, false)] {
        let home = Home(std::env::temp_dir().join(format!("rn-unread-{}-{pushed}-{listed}", std::process::id())));
        fs::create_dir_all(&home.0).unwrap();
        let listener = UnixListener::bind(home.0.join("network.sock")).unwrap();
        let daemon = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            stream.set_read_timeout(Some(Duration::from_secs(10))).unwrap();
            for line in BufReader::new(stream.try_clone().unwrap()).lines().map_while(Result::ok) {
                let request: Value = serde_json::from_str(&line).unwrap();
                let result = match request["op"].as_str().unwrap() {
                    "register" => json!({"name": "test", "address": "me/test", "unread": 1}),
                    "delivery_status" => json!([]),
                    "unread" => json!({"count": 1, "from": ["alice/api"]}),
                    "team" => json!({"name": "test", "person": "me", "machine": "test", "machines": []}),
                    op => panic!("unexpected daemon request: {op}"),
                };
                writeln!(stream, "{}", json!({"id": request["id"], "ok": true, "result": result})).unwrap();
            }
        });
        let mut command = Command::new(env!("CARGO_BIN_EXE_reason-network"));
        command
            .arg("mcp")
            .env_clear()
            .env("REASON_NETWORK_HOME", &home.0)
            .env("CLAUDE_CONFIG_DIR", &home.0)
            .env("CLAUDE_CODE_SESSION_ID", "test")
            .current_dir(&home.0)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null());
        if pushed {
            command.env("CLAUDE_CODE_MESSAGING_SOCKET", home.0.join("claude.sock"));
        }
        let mut server = Server(command.spawn().unwrap());
        let mut stdin = server.0.stdin.take().unwrap();
        let stdout = server.0.stdout.take().unwrap();
        let (lines, replies) = mpsc::channel();
        let reader = std::thread::spawn(move || {
            for line in BufReader::new(stdout).lines().map_while(Result::ok) {
                let _ = lines.send(line);
            }
        });
        let reply_to = |id: u64| loop {
            let line = replies.recv_timeout(Duration::from_secs(10)).expect("no reply from the MCP server");
            let reply: Value = serde_json::from_str(&line).unwrap();
            if reply["id"] == id {
                return reply["result"].clone();
            }
        };
        let mut send = |message: Value| writeln!(stdin, "{message}").unwrap();
        let init = json!({"protocolVersion": "2025-06-18", "capabilities": {},
            "clientInfo": {"name": "claude-code", "version": "1"}});
        send(json!({"jsonrpc": "2.0", "id": 1, "method": "initialize", "params": init}));
        reply_to(1);
        send(json!({"jsonrpc": "2.0", "method": "notifications/initialized"}));
        if listed {
            send(json!({"jsonrpc": "2.0", "id": 2, "method": "tools/list"}));
            let tools = reply_to(2);
            let inbox = tools["tools"].as_array().unwrap().iter().any(|t| t["name"] == "check_inbox");
            assert_eq!(inbox, !pushed);
        }
        let call = json!({"name": "message_status", "arguments": {"ids": []}});
        send(json!({"jsonrpc": "2.0", "id": 3, "method": "tools/call", "params": call}));
        let result = reply_to(3);
        let text = result["content"][0]["text"].as_str().unwrap();
        assert!(text.contains("1 unread message from alice/api."), "{text}");
        assert_eq!(text.contains("Call check_inbox"), listed && !pushed, "{text}");
        drop(server);
        reader.join().unwrap();
        daemon.join().unwrap();
    }
}
