//! `mcp --no-register` keeps the agent out of the team's directory, and its tools still work.
#![cfg(unix)]
use serde_json::{Value, json};
use std::{
    fs,
    io::{BufRead, BufReader, Write},
    path::{Path, PathBuf},
    process::{Child, ChildStdin, Command, Stdio},
    sync::mpsc,
    time::Duration,
};
struct Home(PathBuf);
impl Drop for Home {
    fn drop(&mut self) {
        let _ = reason_network(&self.0).arg("stop").output();
        let _ = fs::remove_dir_all(&self.0);
    }
}
struct Running(Child);
impl Drop for Running {
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
        .env("REASON_NETWORK_NO_SERVICE", "1")
        .env_remove("REASON_NETWORK_NO_REGISTER")
        .env_remove("CLAUDE_PROJECT_DIR")
        .env_remove("CLAUDECODE")
        .env_remove("CLAUDE_CODE_SESSION_ID")
        .env_remove("AI_AGENT");
    c
}
fn run(home: &Path, args: &[&str]) -> String {
    let out = reason_network(home).args(args).output().unwrap();
    assert!(out.status.success(), "{args:?}: {}", String::from_utf8_lossy(&out.stderr));
    String::from_utf8_lossy(&out.stdout).into_owned()
}
/// Every session's address in the team's directory.
fn directory(home: &Path) -> Vec<String> {
    let who: Value = serde_json::from_str(&run(home, &["who", "--json", "--all"])).unwrap();
    who["sessions"].as_array().unwrap().iter().map(|s| s["address"].as_str().unwrap().to_string()).collect()
}
/// An MCP server for a Codex client, started with `args` (and `env`) in `folder`.
struct Mcp {
    _server: Running,
    stdin: ChildStdin,
    replies: mpsc::Receiver<Value>,
    instructions: String,
}
impl Mcp {
    fn start(home: &Path, folder: &Path, args: &[&str], env: &[(&str, &str)]) -> Mcp {
        let mut server = reason_network(home);
        server.arg("mcp").args(args).envs(env.iter().copied()).current_dir(folder);
        let mut server =
            Running(server.stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::null()).spawn().unwrap());
        let stdin = server.0.stdin.take().unwrap();
        let stdout = server.0.stdout.take().unwrap();
        let (tx, replies) = mpsc::channel();
        std::thread::spawn(move || {
            for line in BufReader::new(stdout).lines().map_while(Result::ok) {
                let _ = tx.send(serde_json::from_str(&line).unwrap());
            }
        });
        let mut mcp = Mcp { _server: server, stdin, replies, instructions: String::new() };
        let client = json!({"name": "codex-mcp-client", "version": "1"});
        let init = json!({"protocolVersion": "2025-06-18", "capabilities": {}, "clientInfo": client});
        let reply = mcp.request(1, "initialize", init);
        mcp.instructions = reply["result"]["instructions"].as_str().unwrap_or_default().to_string();
        writeln!(mcp.stdin, "{}", json!({"jsonrpc": "2.0", "method": "notifications/initialized"})).unwrap();
        mcp
    }
    fn request(&mut self, id: u64, method: &str, params: Value) -> Value {
        writeln!(self.stdin, "{}", json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params})).unwrap();
        loop {
            let reply = self.replies.recv_timeout(Duration::from_secs(20)).expect("no reply from the MCP server");
            if reply["id"] == id {
                return reply;
            }
        }
    }
    fn call(&mut self, id: u64, tool: &str, arguments: Value) -> String {
        let reply = self.request(id, "tools/call", json!({"name": tool, "arguments": arguments}));
        reply["result"]["content"][0]["text"].as_str().unwrap().to_string()
    }
}
/// A team of one (alice), with `alice/role` standing in for a long-lived session to message.
fn team(name: &str) -> (Home, PathBuf) {
    let home = Home(std::env::temp_dir().join(format!("rn-no-register-{name}-{}", std::process::id())));
    let folder = home.0.join("task");
    fs::create_dir_all(&folder).unwrap();
    run(&home.0, &["init", "team", "--as", "alice", "--no-setup"]);
    run(&home.0, &["listen", "--as", "role", "--wait", "0"]);
    (home, folder)
}

#[test]
fn no_register_lists_sends_and_reads_without_a_session_of_its_own() {
    for (case, args, env) in
        [("flag", &["--no-register"][..], &[][..]), ("env", &[], &[("REASON_NETWORK_NO_REGISTER", "1")])]
    {
        let (home, folder) = team(case);
        let mut mcp = Mcp::start(&home.0, &folder, args, env);
        assert!(mcp.instructions.contains("--no-register"), "{case}");
        let list = mcp.call(2, "list_team", json!({}));
        assert!(list.contains("role") && !list.contains("← you"), "{case}: {list}");
        assert_eq!(directory(&home.0), ["alice/role"], "{case}: list_team registered nothing");
        let sent = mcp.call(3, "send_message", json!({"to": "alice/role", "message": "hello from a task"}));
        assert!(sent.contains("alice/role"), "{case}: {sent}");
        let got = run(&home.0, &["listen", "--as", "role", "--wait", "0"]);
        assert!(got.contains("from=\"alice/tasks\"") && got.contains("hello from a task"), "{case}: {got}");
        // An agent's message, not one the person typed: no group sends.
        assert!(got.contains("From alice's agent."), "{case}: {got}");
        let group = mcp.call(4, "send_message", json!({"to": "alice/*", "message": "everyone"}));
        assert!(group.contains("not a group"), "{case}: {group}");
        run(&home.0, &["send", "--from", "alice/role", "alice/tasks", "the reply"]);
        let inbox = mcp.call(5, "check_inbox", json!({}));
        assert!(inbox.contains("the reply") && inbox.contains("from=\"alice/role\""), "{case}: {inbox}");
        // The one shared mailbox is all there is: no session named after this agent or its folder.
        let mut sessions = directory(&home.0);
        sessions.sort();
        assert_eq!(sessions, ["alice/role", "alice/tasks"], "{case}");
    }
}

#[test]
fn by_default_the_agent_is_a_session_of_its_own() {
    let (home, folder) = team("default");
    let mut mcp = Mcp::start(&home.0, &folder, &[], &[]);
    assert!(!mcp.instructions.contains("--no-register"));
    let list = mcp.call(2, "list_team", json!({}));
    assert!(list.contains("← you"), "{list}");
    let sessions = directory(&home.0);
    assert!(sessions.iter().any(|s| s.ends_with("-codex")), "{sessions:?}");
    mcp.call(3, "send_message", json!({"to": "alice/role", "message": "hello"}));
    let got = run(&home.0, &["listen", "--as", "role", "--wait", "0"]);
    assert!(got.contains("-codex\"") && !got.contains("alice/tasks"), "{got}");
}
