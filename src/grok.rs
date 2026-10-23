//! Grok sessions hosted by a shared leader (`[cli] use_leader = true`): the TUI is one client of the leader, which
//! runs the session, and another client can start a turn in it over ACP (`session/load`, then `session/prompt`). The
//! TUI shows that turn as its own, and a prompt sent while a turn runs waits for it to end.

use serde_json::{Value, json};
use std::{path::Path, time::Duration};
use tokio::io::{AsyncBufRead, AsyncBufReadExt, AsyncWrite, AsyncWriteExt, BufReader};

/// Long enough for `session/load`, which replays the whole conversation first.
const PATIENCE: Duration = Duration::from_secs(60);

/// In the MCP server Grok started for a session: the leader's socket, when a leader runs the session. Its tools run
/// in the leader, so the leader is our parent. A session Grok runs in the TUI's own process has none: loading it
/// through a leader would make a second copy of it, which the TUI never sees.
pub fn leader_socket() -> Option<String> {
    if let Some(s) = std::env::var("GROK_LEADER_SOCKET").ok().filter(|s| !s.is_empty()) {
        return Some(s);
    }
    leader_from_args(&crate::util::process_args(crate::util::parent_pid())?, &crate::setup::grok_home())
}

/// The socket of `grok agent leader [--leader-socket <path>]`, from its command line.
fn leader_from_args(args: &str, home: &Path) -> Option<String> {
    let words: Vec<&str> = args.split_whitespace().collect();
    let agent = words.iter().position(|w| *w == "agent")?;
    if !words[agent..].contains(&"leader") {
        return None;
    }
    let given = words.iter().enumerate().find_map(|(i, w)| match w.strip_prefix("--leader-socket") {
        Some("") => words.get(i + 1).map(|s| s.to_string()),
        Some(rest) => rest.strip_prefix('=').map(str::to_string),
        None => None,
    });
    Some(given.unwrap_or_else(|| home.join("leader.sock").display().to_string()))
}

/// Whether a TUI has `session` open: Grok lists the sessions its clients have open, with the client's process. A
/// session the leader keeps after its TUI quit is not woken: nobody would see the turn.
pub fn tui_open(session: &str) -> bool {
    let text = std::fs::read_to_string(crate::setup::grok_home().join("active_sessions.json")).unwrap_or_default();
    tui_pid(&text, session).is_some_and(crate::util::pid_alive)
}

fn tui_pid(text: &str, session: &str) -> Option<u32> {
    let v: Value = serde_json::from_str(text).ok()?;
    v.as_array()?
        .iter()
        .find(|s| s["session_id"] == session)
        .and_then(|s| s["pid"].as_u64())
        .and_then(|p| u32::try_from(p).ok())
}

/// Starts a turn with `text` in the leader's session `session`, through a client of our own (`grok agent --leader
/// stdio`). Returns once the turn has started: hanging up doesn't stop it.
pub async fn prompt(bin: &Path, socket: &str, session: &str, cwd: &str, text: &str) -> Result<(), String> {
    let mut cmd = tokio::process::Command::new(bin);
    cmd.args(["agent", "--leader", "--leader-socket", socket, "stdio"])
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .kill_on_drop(true);
    if Path::new(cwd).is_dir() {
        cmd.current_dir(cwd);
    }
    let mut child = cmd.spawn().map_err(|e| e.to_string())?;
    let (Some(stdin), Some(stdout)) = (child.stdin.take(), child.stdout.take()) else {
        return Err("no pipes to grok".into());
    };
    let talk = converse(BufReader::new(stdout), stdin, session, cwd, text);
    tokio::time::timeout(PATIENCE, talk).await.unwrap_or_else(|_| Err("timed out".into()))
}

/// The ACP exchange: initialize, load the session (its replay is skipped), prompt, and wait for the turn to start.
async fn converse<R, W>(mut from: R, mut to: W, session: &str, cwd: &str, text: &str) -> Result<(), String>
where
    R: AsyncBufRead + Unpin,
    W: AsyncWrite + Unpin,
{
    let caps = json!({ "fs": { "readTextFile": false, "writeTextFile": false }, "terminal": false });
    let steps = [
        ("initialize", json!({ "protocolVersion": 1, "clientCapabilities": caps })),
        ("session/load", json!({ "sessionId": session, "cwd": cwd, "mcpServers": [] })),
    ];
    for (id, (method, params)) in (1..).zip(steps) {
        send(&mut to, json!({ "jsonrpc": "2.0", "id": id, "method": method, "params": params })).await?;
        loop {
            let m = next(&mut from, &mut to).await?;
            if m["id"] == id {
                if let Some(e) = m.get("error") {
                    return Err(format!("{method}: {}", e["message"].as_str().unwrap_or("refused")));
                }
                break;
            }
        }
    }
    let prompt = json!({ "sessionId": session, "prompt": [{ "type": "text", "text": text }] });
    send(&mut to, json!({ "jsonrpc": "2.0", "id": 3, "method": "session/prompt", "params": prompt })).await?;
    // The first update after it is this turn (or the one it waits behind); a turn that ended at once just answers.
    loop {
        let m = next(&mut from, &mut to).await?;
        if m["method"] == "session/update" {
            return Ok(());
        }
        if m["id"] == 3 {
            return match m.get("error") {
                Some(e) => Err(format!("session/prompt: {}", e["message"].as_str().unwrap_or("refused"))),
                None => Ok(()),
            };
        }
    }
}

async fn send<W: AsyncWrite + Unpin>(to: &mut W, msg: Value) -> Result<(), String> {
    let mut line = msg.to_string();
    line.push('\n');
    to.write_all(line.as_bytes()).await.map_err(|e| e.to_string())?;
    to.flush().await.map_err(|e| e.to_string())
}

/// The next message from the agent. What it asks of us (files, a terminal, permission) is refused: this client only
/// hands over a message.
async fn next<R, W>(from: &mut R, to: &mut W) -> Result<Value, String>
where
    R: AsyncBufRead + Unpin,
    W: AsyncWrite + Unpin,
{
    let mut line = String::new();
    loop {
        line.clear();
        if from.read_line(&mut line).await.map_err(|e| e.to_string())? == 0 {
            return Err("grok closed the connection".into());
        }
        let Ok(m) = serde_json::from_str::<Value>(&line) else { continue };
        if m.get("method").is_some() && m.get("id").is_some() {
            let no = json!({ "jsonrpc": "2.0", "id": m["id"], "error": { "code": -32601, "message": "not offered" } });
            send(to, no).await?;
            continue;
        }
        return Ok(m);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::{AsyncReadExt, duplex};

    #[test]
    fn a_leaders_socket_comes_from_its_command_line() {
        let home = Path::new("/h/.grok");
        let auto = "/h/.grok/bin/grok agent leader --no-exit-on-disconnect --relay-on-demand --grok-ws-url wss://x";
        assert_eq!(leader_from_args(auto, home).as_deref(), Some("/h/.grok/leader.sock"));
        let custom = "grok agent --always-approve leader --leader-socket /h/.grok/leader-t.sock";
        assert_eq!(leader_from_args(custom, home).as_deref(), Some("/h/.grok/leader-t.sock"));
        let eq = "grok agent leader --leader-socket=/tmp/l.sock";
        assert_eq!(leader_from_args(eq, home).as_deref(), Some("/tmp/l.sock"));
        // The TUI running a session itself, and a client of a leader, are not leaders.
        assert_eq!(leader_from_args("grok", home), None);
        assert_eq!(leader_from_args("grok agent --leader stdio", home), None);
        assert_eq!(leader_from_args("grok --leader-socket /x.sock", home), None);
    }

    #[test]
    fn the_tui_holding_a_session_is_found_by_its_id() {
        let text = r#"[{"session_id":"a","pid":63782,"cwd":"/h"},{"session_id":"b","pid":72611,"cwd":"/h"}]"#;
        assert_eq!(tui_pid(text, "b"), Some(72611));
        assert_eq!(tui_pid(text, "c"), None);
        assert_eq!(tui_pid("not json", "a"), None);
    }

    /// Plays the agent's side: after each line it reads, writes the next lines of `answers`. Returns what it read.
    async fn agent_side(mut io: tokio::io::DuplexStream, answers: Vec<Vec<Value>>) -> Vec<Value> {
        let mut got = Vec::new();
        let mut buf = Vec::new();
        for lines in answers {
            loop {
                let mut b = [0u8; 1];
                if io.read(&mut b).await.unwrap() == 0 {
                    return got;
                }
                if b[0] == b'\n' {
                    break;
                }
                buf.push(b[0]);
            }
            got.push(serde_json::from_slice::<Value>(&buf).unwrap());
            buf.clear();
            for l in lines {
                io.write_all(format!("{l}\n").as_bytes()).await.unwrap();
            }
        }
        got
    }

    fn update(text: &str) -> Value {
        json!({ "jsonrpc": "2.0", "method": "session/update", "params": { "sessionId": "s",
            "update": { "sessionUpdate": "agent_message_chunk", "content": { "type": "text", "text": text } } } })
    }

    #[tokio::test]
    async fn a_prompt_counts_once_its_turn_starts_after_the_replay_of_the_session() {
        let (ours, theirs) = duplex(1 << 16);
        let agent = tokio::spawn(agent_side(
            theirs,
            vec![
                vec![json!({ "jsonrpc": "2.0", "id": 1, "result": { "protocolVersion": 1 } })],
                // Loading replays the conversation, and the agent asks us something on the way.
                vec![
                    update("an old answer"),
                    json!({ "jsonrpc": "2.0", "id": 99, "method": "fs/read_text_file", "params": {} }),
                    json!({ "jsonrpc": "2.0", "id": 2, "result": {} }),
                ],
                vec![],
                vec![update("the new turn")],
            ],
        ));
        let (r, w) = tokio::io::split(ours);
        let res = converse(BufReader::new(r), w, "s", "/w", "hello").await;
        assert_eq!(res, Ok(()));
        let sent = agent.await.unwrap();
        assert_eq!(sent[0]["method"], "initialize");
        assert_eq!(sent[1]["method"], "session/load");
        assert_eq!(sent[1]["params"]["sessionId"], "s");
        // Its question was refused before the prompt went.
        assert_eq!(sent[2]["id"], 99);
        assert!(sent[2].get("error").is_some());
        assert_eq!(sent[3]["method"], "session/prompt");
        assert_eq!(sent[3]["params"]["prompt"][0]["text"], "hello");
    }

    #[tokio::test]
    async fn a_session_the_leader_cant_load_is_an_error() {
        let (ours, theirs) = duplex(1 << 16);
        let agent = tokio::spawn(agent_side(
            theirs,
            vec![
                vec![json!({ "jsonrpc": "2.0", "id": 1, "result": {} })],
                vec![json!({ "jsonrpc": "2.0", "id": 2, "error": { "code": -32002, "message": "no such session" } })],
            ],
        ));
        let (r, w) = tokio::io::split(ours);
        let res = converse(BufReader::new(r), w, "s", "/w", "hello").await;
        assert_eq!(res, Err("session/load: no such session".into()));
        drop(agent);
    }

    #[tokio::test]
    async fn an_agent_that_hangs_up_is_an_error() {
        let (ours, theirs) = duplex(1 << 16);
        drop(theirs);
        let (r, w) = tokio::io::split(ours);
        assert!(converse(BufReader::new(r), w, "s", "/w", "hello").await.is_err());
    }
}
