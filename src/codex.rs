//! What a Codex thread's rollout (the transcript its hooks name) says about its turn.
//!
//! Codex records each turn's start (`task_started`) and its end: `task_complete` when it finishes or fails,
//! `turn_aborted` when it is interrupted. The last two run no Stop hook, and a single tool call can run for hours
//! without any hook, so only the rollout tells a running turn from one that is over.

use std::{
    collections::HashMap,
    fs::File,
    io::{Read, Seek, SeekFrom},
    path::{Path, PathBuf},
    sync::{Mutex, OnceLock},
};

/// How much is read at a time, from the end back, to find the last turn event.
const CHUNK: u64 = 256 << 10;

/// Whether the turn the rollout at `path` records last is still running. A rollout without a turn yet is idle.
/// `None` when it can't be read. Remembers how far it read, so later calls read only what was appended.
pub fn turn_running(path: &Path) -> Option<bool> {
    static SEEN: OnceLock<Mutex<HashMap<PathBuf, (u64, bool)>>> = OnceLock::new();
    // Only a regular file: opening a pipe a hook named would wait for a writer.
    if !std::fs::metadata(path).ok()?.is_file() {
        return None;
    }
    let mut seen = SEEN.get_or_init(Default::default).lock().unwrap();
    let mut file = File::open(path).ok()?;
    let len = file.metadata().ok()?.len();
    // Rollouts only grow: a shorter one was rewritten, and is read again.
    let (from, before) = match seen.get(path) {
        Some(&(at, running)) if at <= len => (at, running),
        _ => (0, false),
    };
    let (end, last) = last_turn_event(&mut file, from, len).ok()?;
    let running = last.unwrap_or(before);
    seen.insert(path.to_path_buf(), (end, running));
    Some(running)
}

/// The Codex home: `home` (the `CODEX_HOME` a thread's hooks ran with), else the daemon's own, else `~/.codex`.
fn codex_home(home: Option<&Path>) -> PathBuf {
    // An empty one is no home, as Codex has it.
    home.filter(|h| !h.as_os_str().is_empty())
        .map(Path::to_path_buf)
        .or_else(|| std::env::var_os("CODEX_HOME").filter(|h| !h.is_empty()).map(PathBuf::from))
        .unwrap_or_else(|| crate::paths::user_home().join(".codex"))
}

/// The control socket of the Codex home `home` (as in [`archived`]) to connect to. Codex advertises it at
/// `app-server-control/app-server-control.sock`, on Unix as a symlink to a short protected socket: a long `CODEX_HOME`
/// makes the advertised path too long for an AF_UNIX address, so the link's target is what is connected to.
pub fn control_socket(home: Option<&Path>) -> PathBuf {
    let advertised = codex_home(home).join("app-server-control").join("app-server-control.sock");
    match std::fs::read_link(&advertised) {
        // A relative target is relative to the link's folder; an absolute one replaces it.
        Ok(target) => advertised.parent().map_or_else(|| target.clone(), |dir| dir.join(&target)),
        Err(_) => advertised,
    }
}

/// Whether a Codex app-server daemon answers on the control socket of the Codex home (`home` as in [`archived`]): the
/// shared server `codex queue --resume` loads a thread through. With none, `codex queue` queues through a server of its
/// own, which can't load the thread (it warns "could not resume thread" and exits 0), so the mail would wait in Codex
/// until the person opens the thread.
pub async fn app_server_up(home: Option<&Path>) -> bool {
    let socket = control_socket(home);
    #[cfg(unix)]
    {
        let connect = tokio::net::UnixStream::connect(socket);
        matches!(tokio::time::timeout(std::time::Duration::from_secs(1), connect).await, Ok(Ok(_)))
    }
    // Codex's socket there is an AF_UNIX one too, which tokio can't open on Windows: one that exists counts.
    #[cfg(not(unix))]
    {
        tokio::fs::metadata(socket).await.is_ok()
    }
}

/// Asks the shared app-server of the Codex home `home` to load Codex thread `thread` again if it isn't loaded, the
/// way `codex queue --resume` does after queueing (`thread/read`, then `thread/resume`), without queueing anything:
/// for mail Codex took but could not load the thread for. Through `codex app-server proxy` (`program`), which relays
/// stdio to the control socket on every platform. `Ok(true)` when it loaded it, `Ok(false)` when it was loaded already.
pub async fn resume(mut program: std::process::Command, thread: &str, home: Option<&Path>) -> Result<bool, String> {
    program.args(["app-server", "proxy"]);
    let advertised = codex_home(home).join("app-server-control").join("app-server-control.sock");
    let socket = control_socket(home);
    // Only a resolved link is named: the proxy's own default is the advertised path, too long to connect to.
    if socket != advertised {
        program.arg("--sock").arg(&socket);
    }
    if let Some(home) = home {
        program.env("CODEX_HOME", home);
    }
    let mut command = crate::util::no_window(tokio::process::Command::from(program));
    command
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .kill_on_drop(true);
    let mut child = command.spawn().map_err(|e| format!("codex app-server proxy: {e}"))?;
    let (Some(stdin), Some(stdout)) = (child.stdin.take(), child.stdout.take()) else {
        return Err("codex app-server proxy: no stdio".into());
    };
    let mut stream = tokio::io::join(stdout, stdin);
    match tokio::time::timeout(std::time::Duration::from_secs(20), resume_over(&mut stream, thread)).await {
        Ok(res) => res,
        Err(_) => Err("timed out".into()),
    }
}

/// [`resume`] over `stream`, a connection to a Codex app-server's control socket: JSON-RPC in WebSocket text frames.
pub async fn resume_over<S>(stream: &mut S, thread: &str) -> Result<bool, String>
where
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin,
{
    let mut ws = ws::Client::connect(stream).await?;
    let client =
        serde_json::json!({"name": "reason_network", "title": "Reason Network", "version": env!("CARGO_PKG_VERSION")});
    ws.call("initialize", serde_json::json!({ "clientInfo": client })).await?;
    ws.send(&serde_json::json!({ "method": "initialized" })).await?;
    let read = ws.call("thread/read", serde_json::json!({ "threadId": thread })).await?;
    if read.pointer("/thread/status/type").and_then(|t| t.as_str()) != Some("notLoaded") {
        return Ok(false);
    }
    ws.call("thread/resume", serde_json::json!({ "threadId": thread, "excludeTurns": true })).await?;
    Ok(true)
}

/// The little of a WebSocket client a Codex control socket needs: the upgrade, then JSON text messages each way.
mod ws {
    use serde_json::Value;
    use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

    /// The most a message from the server may hold.
    const MAX: usize = 64 << 20;

    pub struct Client<'a, S> {
        stream: &'a mut S,
        next: u64,
    }

    impl<'a, S: AsyncRead + AsyncWrite + Unpin> Client<'a, S> {
        /// Upgrades `stream` to a WebSocket at `/rpc`, as Codex's own clients do on its control socket.
        pub async fn connect(stream: &'a mut S) -> Result<Self, String> {
            let mut key = [0u8; 16];
            getrandom::fill(&mut key).map_err(|e| e.to_string())?;
            let request = format!(
                "GET /rpc HTTP/1.1\r\nHost: localhost\r\nUpgrade: websocket\r\nConnection: Upgrade\r\n\
                 Sec-WebSocket-Key: {}\r\nSec-WebSocket-Version: 13\r\n\r\n",
                data_encoding::BASE64.encode(&key)
            );
            stream.write_all(request.as_bytes()).await.map_err(|e| e.to_string())?;
            stream.flush().await.map_err(|e| e.to_string())?;
            // The response head, a byte at a time: nothing past it is read before the first frame.
            let mut head = Vec::new();
            while !head.ends_with(b"\r\n\r\n") {
                if head.len() > 16 << 10 {
                    return Err("app-server: response head too long".into());
                }
                head.push(stream.read_u8().await.map_err(|e| format!("app-server closed the upgrade: {e}"))?);
            }
            let status = String::from_utf8_lossy(&head);
            let status = status.lines().next().unwrap_or_default();
            if status.split_whitespace().nth(1) != Some("101") {
                return Err(format!("app-server refused the upgrade: {status}"));
            }
            Ok(Self { stream, next: 1 })
        }

        /// Sends a JSON message.
        pub async fn send(&mut self, message: &Value) -> Result<(), String> {
            self.frame(0x1, message.to_string().as_bytes()).await
        }

        /// Calls `method` and waits for its result, answering any request of the server's meanwhile with an error and
        /// passing over notifications.
        pub async fn call(&mut self, method: &str, params: Value) -> Result<Value, String> {
            let id = self.next;
            self.next += 1;
            self.send(&serde_json::json!({ "id": id, "method": method, "params": params })).await?;
            loop {
                let message: Value = serde_json::from_slice(&self.receive().await?)
                    .map_err(|e| format!("app-server sent something that isn't JSON: {e}"))?;
                match (message.get("id"), message.get("method")) {
                    (Some(their), Some(_)) => {
                        let reply =
                            serde_json::json!({ "id": their, "error": { "code": -32601, "message": "unsupported" } });
                        self.send(&reply).await?;
                    }
                    (Some(their), None) if their.as_u64() == Some(id) => {
                        if let Some(error) = message.get("error") {
                            let why = error.get("message").and_then(Value::as_str).unwrap_or("failed");
                            return Err(format!("{method}: {why}"));
                        }
                        return Ok(message.get("result").cloned().unwrap_or(Value::Null));
                    }
                    _ => {}
                }
            }
        }

        /// One client frame: final, masked, as a client's must be.
        async fn frame(&mut self, opcode: u8, payload: &[u8]) -> Result<(), String> {
            let mut mask = [0u8; 4];
            getrandom::fill(&mut mask).map_err(|e| e.to_string())?;
            let mut out = vec![0x80 | opcode];
            match payload.len() {
                n if n < 126 => out.push(0x80 | n as u8),
                n if n <= u16::MAX as usize => {
                    out.push(0x80 | 126);
                    out.extend_from_slice(&(n as u16).to_be_bytes());
                }
                n => {
                    out.push(0x80 | 127);
                    out.extend_from_slice(&(n as u64).to_be_bytes());
                }
            }
            out.extend_from_slice(&mask);
            out.extend(payload.iter().enumerate().map(|(i, b)| b ^ mask[i % 4]));
            self.stream.write_all(&out).await.map_err(|e| e.to_string())?;
            self.stream.flush().await.map_err(|e| e.to_string())
        }

        /// The next whole data message, answering pings; an error once the server closes.
        async fn receive(&mut self) -> Result<Vec<u8>, String> {
            let mut message = Vec::new();
            loop {
                let io = |e: std::io::Error| format!("app-server connection: {e}");
                let (b0, b1) = (self.stream.read_u8().await.map_err(io)?, self.stream.read_u8().await.map_err(io)?);
                let (fin, opcode, masked) = (b0 & 0x80 != 0, b0 & 0x0f, b1 & 0x80 != 0);
                let len = match b1 & 0x7f {
                    126 => self.stream.read_u16().await.map_err(io)? as u64,
                    127 => self.stream.read_u64().await.map_err(io)?,
                    n => n as u64,
                };
                if len > MAX as u64 || message.len() as u64 + len > MAX as u64 {
                    return Err("app-server message too large".into());
                }
                let mut mask = [0u8; 4];
                if masked {
                    self.stream.read_exact(&mut mask).await.map_err(io)?;
                }
                let mut payload = vec![0u8; len as usize];
                self.stream.read_exact(&mut payload).await.map_err(io)?;
                if masked {
                    payload.iter_mut().enumerate().for_each(|(i, b)| *b ^= mask[i % 4]);
                }
                match opcode {
                    0x8 => return Err("app-server closed the connection".into()),
                    0x9 => self.frame(0xA, &payload).await?,
                    0xA => {}
                    _ => {
                        message.extend_from_slice(&payload);
                        if fin {
                            return Ok(message);
                        }
                    }
                }
            }
        }
    }
}

/// Whether the person archived Codex thread `thread`: archiving moves its rollout into `archived_sessions/` of the
/// Codex home (`home`, the `CODEX_HOME` its hooks ran with; else the daemon's own, else `~/.codex`), plain or
/// compressed. A thread Codex only unloaded (idle, or its app-server shut down) keeps its rollout in `sessions/`.
pub fn archived(thread: &str, home: Option<&Path>) -> bool {
    let dir = codex_home(home).join("archived_sessions");
    let Ok(entries) = std::fs::read_dir(dir) else { return false };
    // `rollout-<YYYY-MM-DDThh-mm-ss>-<thread>[_<rollout>].jsonl[.zst]` (`_<rollout>` names a replacement rollout, after
    // `thread/revert`): the whole thread id, not one that merely ends the same way.
    let id = |name: &str| {
        let rest = name.strip_prefix("rollout-")?;
        let rest = rest.strip_suffix(".jsonl.zst").or_else(|| rest.strip_suffix(".jsonl"))?;
        let ids = rest.get(20..).filter(|_| rest.as_bytes().get(19) == Some(&b'-'))?;
        match ids.split_once('_') {
            Some((thread, rollout)) if !thread.is_empty() && !rollout.is_empty() && !rollout.contains('_') => {
                Some(thread.to_string())
            }
            Some(_) => None,
            None => Some(ids.to_string()),
        }
    };
    entries.flatten().any(|e| e.file_name().to_str().and_then(id).is_some_and(|t| t == thread))
}

/// The last turn event in `from..to` (whole lines from `from`, which starts one), and where the last whole line in it
/// ends: a line still being written is read next time.
fn last_turn_event(file: &mut File, from: u64, to: u64) -> std::io::Result<(u64, Option<bool>)> {
    // `rest`: the pieces of a line that begins in a chunk before, last piece first.
    let (mut end, mut whole_to, mut rest) = (to, None, Vec::<Vec<u8>>::new());
    while end > from {
        let start = end.saturating_sub(CHUNK).max(from);
        let mut buf = vec![0; (end - start) as usize];
        file.seek(SeekFrom::Start(start))?;
        file.read_exact(&mut buf)?;
        if whole_to.is_none() {
            // Up to the last newline: what follows it isn't a whole line yet.
            let Some(i) = buf.iter().rposition(|&b| b == b'\n') else {
                end = start;
                continue;
            };
            whole_to = Some(start + i as u64 + 1);
            buf.truncate(i + 1);
        } else if start > from && !buf.contains(&b'\n') {
            // All of it is inside that line: put the line together once its start is read, not every chunk.
            rest.push(buf);
            end = start;
            continue;
        } else {
            for piece in rest.drain(..).rev() {
                buf.extend_from_slice(&piece);
            }
        }
        // The first line here may begin in the chunk before: keep it for then.
        let first = if start > from { buf.iter().position(|&b| b == b'\n').map_or(buf.len(), |i| i + 1) } else { 0 };
        if let Some(running) = buf[first..].split(|&b| b == b'\n').rev().find_map(turn_event) {
            return Ok((whole_to.unwrap_or(from), Some(running)));
        }
        rest.push(buf[..first].to_vec());
        end = start;
    }
    Ok((whole_to.unwrap_or(from), None))
}

/// Some(true) for a line recording a turn's start, Some(false) for one recording its end.
fn turn_event(line: &[u8]) -> Option<bool> {
    // Most lines are something else: parse only those that may be one.
    let text = std::str::from_utf8(line).ok()?;
    if !text.contains("event_msg") || !["task_", "turn_"].iter().any(|k| text.contains(k)) {
        return None;
    }
    let v: serde_json::Value = serde_json::from_str(text).ok()?;
    if v["type"] != "event_msg" {
        return None;
    }
    match v["payload"]["type"].as_str()? {
        "task_started" | "turn_started" => Some(true),
        "task_complete" | "turn_complete" | "turn_aborted" => Some(false),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn event(kind: &str) -> String {
        format!(r#"{{"timestamp":"t","type":"event_msg","payload":{{"type":"{kind}","turn_id":"1"}}}}"#) + "\n"
    }

    fn append(path: &Path, s: &str) {
        std::fs::OpenOptions::new().create(true).append(true).open(path).unwrap().write_all(s.as_bytes()).unwrap();
    }

    #[test]
    fn only_a_thread_whose_rollout_codex_moved_to_archived_sessions_is_archived() {
        let home = std::env::temp_dir().join(format!("reason-network-codex-archived-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&home);
        let (live, archived_dir) = (home.join("sessions/2026/11/11"), home.join("archived_sessions"));
        std::fs::create_dir_all(&live).unwrap();
        std::fs::create_dir_all(&archived_dir).unwrap();
        let kept = "01a1384b-f90e-75a0-a6f7-a23c9a50e216";
        let (put_away, zst) = ("019a0000-0000-7000-8000-000000000001", "019a0000-0000-7000-8000-000000000002");
        append(&live.join(format!("rollout-2026-11-11T02-32-20-{kept}.jsonl")), "{}\n");
        append(&archived_dir.join(format!("rollout-2026-11-10T09-00-00-{put_away}.jsonl")), "{}\n");
        append(&archived_dir.join(format!("rollout-2026-11-10T09-00-00-{zst}.jsonl.zst")), "x");
        // Unloaded, not archived: its rollout is where Codex keeps live threads.
        assert!(!archived(kept, Some(&home)));
        assert!(archived(put_away, Some(&home)));
        assert!(archived(zst, Some(&home)));
        // Only that thread's own rollout: not one whose id merely ends the same way.
        assert!(!archived("8000-000000000001", Some(&home)));
        // A replacement rollout (after `thread/revert`) is named `<thread>_<rollout>`: still that thread's, plain or
        // compressed; and never the rollout's own id.
        let (reverted, reverted_zst, rollout) = (
            "019a0000-0000-7000-8000-000000000003",
            "019a0000-0000-7000-8000-000000000004",
            "019a0000-0000-7000-8000-0000000000ff",
        );
        append(&archived_dir.join(format!("rollout-2026-11-10T09-00-00-{reverted}_{rollout}.jsonl")), "{}\n");
        append(&archived_dir.join(format!("rollout-2026-11-10T09-00-00-{reverted_zst}_{rollout}.jsonl.zst")), "x");
        assert!(archived(reverted, Some(&home)));
        assert!(archived(reverted_zst, Some(&home)));
        assert!(!archived(rollout, Some(&home)));
        // No Codex home there at all: nothing says it was archived.
        assert!(!archived(put_away, Some(&home.join("missing"))));
        std::fs::remove_dir_all(&home).unwrap();
    }

    #[test]
    fn the_last_turn_event_says_whether_a_turn_runs() {
        let dir = std::env::temp_dir().join(format!("reason-network-codex-turn-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("rollout.jsonl");
        assert_eq!(turn_running(&path), None, "no rollout");

        append(&path, "{\"type\":\"session_meta\",\"payload\":{}}\n");
        assert_eq!(turn_running(&path), Some(false), "no turn yet");
        append(&path, &event("task_started"));
        assert_eq!(turn_running(&path), Some(true));
        // A message that mentions turn events is not one.
        let quoted = r#"{"type":"response_item","payload":{"text":"\"type\":\"event_msg\" task_complete"}}"#;
        append(&path, &format!("{quoted}\n"));
        append(&path, "{\"type\":\"turn_context\",\"payload\":{\"type\":\"task_complete\",\"note\":\"event_msg\"}}\n");
        append(&path, r#"{"type":"event_msg","payload":{"type":"token_count"}}"#);
        append(&path, "\n");
        assert_eq!(turn_running(&path), Some(true), "only turn events count");
        append(&path, &event("turn_aborted"));
        assert_eq!(turn_running(&path), Some(false), "interrupted");
        append(&path, &event("turn_started"));
        assert_eq!(turn_running(&path), Some(true), "the v2 name");
        append(&path, &event("task_complete"));
        assert_eq!(turn_running(&path), Some(false), "finished or failed");

        // A line still being written counts once it is whole.
        append(&path, &event("task_started"));
        let end = event("task_complete");
        append(&path, &end[..20]);
        assert_eq!(turn_running(&path), Some(true));
        append(&path, &end[20..]);
        assert_eq!(turn_running(&path), Some(false), "the rest of the line was read");

        // Read fresh (a restarted daemon), from far back: the turn started long before the last chunk.
        let fresh = dir.join("long.jsonl");
        append(&fresh, &event("task_complete"));
        append(&fresh, &event("task_started"));
        let output = format!(r#"{{"type":"response_item","payload":{{"output":"{}"}}}}"#, "x".repeat(100_000)) + "\n";
        for _ in 0..12 {
            append(&fresh, &output);
        }
        assert_eq!(turn_running(&fresh), Some(true), "found {} KB back", 12 * 100);
        // One line longer than a chunk, right after the start.
        let fresh2 = dir.join("longline.jsonl");
        append(&fresh2, &event("task_started"));
        append(&fresh2, &(format!(r#"{{"x":"{}"}}"#, "y".repeat(3 * CHUNK as usize)) + "\n"));
        assert_eq!(turn_running(&fresh2), Some(true));

        // A turn event cut by a chunk boundary, read fresh.
        let cut = dir.join("cut.jsonl");
        append(&cut, &event("task_complete"));
        append(&cut, &(format!(r#"{{"x":"{}"}}"#, "z".repeat(CHUNK as usize)) + "\n"));
        append(&cut, &event("task_started"));
        let tail = CHUNK as usize - 10;
        append(&cut, &(format!(r#"{{"x":"{}"}}"#, "w".repeat(tail - 9)) + "\n"));
        assert_eq!(turn_running(&cut), Some(true), "the line across the boundary was put back together");
        // A turn event across four chunks, its type in the middle one, read fresh: its pieces go back in order.
        let spread = dir.join("spread.jsonl");
        append(&spread, &event("task_started"));
        let half = "m".repeat(3 * CHUNK as usize / 2);
        let end = format!(r#"{{"type":"event_msg","payload":{{"a":"{half}","type":"task_complete","b":"{half}"}}}}"#);
        append(&spread, &(end + "\n"));
        assert_eq!(turn_running(&spread), Some(false), "the end, put back together");

        // A pipe is not a rollout, and is not waited on.
        #[cfg(unix)]
        {
            let pipe = dir.join("pipe");
            assert!(std::process::Command::new("mkfifo").arg(&pipe).status().unwrap().success());
            assert_eq!(turn_running(&pipe), None);
        }

        // Rewritten shorter: read again.
        std::fs::write(&path, event("task_started")).unwrap();
        assert_eq!(turn_running(&path), Some(true));
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A Codex app-server on its control socket, as far as [`resume_over`] needs: it answers the WebSocket upgrade,
    /// then each request, with a notification, a ping and a long frame along the way. Returns the messages it got.
    async fn fake_app_server(mut s: tokio::io::DuplexStream, status: &str, refuse: bool) -> Vec<serde_json::Value> {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        async fn frame(s: &mut tokio::io::DuplexStream, opcode: u8, payload: &[u8]) {
            let mut out = vec![0x80 | opcode];
            if payload.len() < 126 {
                out.push(payload.len() as u8);
            } else {
                out.push(126);
                out.extend_from_slice(&(payload.len() as u16).to_be_bytes());
            }
            out.extend_from_slice(payload);
            s.write_all(&out).await.unwrap();
        }
        let mut head = Vec::new();
        while !head.ends_with(b"\r\n\r\n") {
            head.push(s.read_u8().await.unwrap());
        }
        let head = String::from_utf8(head).unwrap();
        assert!(head.starts_with("GET /rpc HTTP/1.1\r\n") && head.contains("Upgrade: websocket\r\n"), "{head}");
        assert!(head.contains("Sec-WebSocket-Version: 13\r\n") && head.contains("Sec-WebSocket-Key: "), "{head}");
        s.write_all(b"HTTP/1.1 101 Switching Protocols\r\nUpgrade: websocket\r\nConnection: Upgrade\r\n\r\n")
            .await
            .unwrap();
        let mut got = Vec::new();
        loop {
            let Ok(b0) = s.read_u8().await else { return got };
            let b1 = s.read_u8().await.unwrap();
            assert!(b1 & 0x80 != 0, "a client frame must be masked");
            let len = match b1 & 0x7f {
                126 => s.read_u16().await.unwrap() as usize,
                127 => s.read_u64().await.unwrap() as usize,
                n => n as usize,
            };
            let mut mask = [0u8; 4];
            s.read_exact(&mut mask).await.unwrap();
            let mut payload = vec![0u8; len];
            s.read_exact(&mut payload).await.unwrap();
            payload.iter_mut().enumerate().for_each(|(i, b)| *b ^= mask[i % 4]);
            match b0 & 0x0f {
                0xA => {
                    assert_eq!(payload, b"are you there");
                    continue;
                }
                0x1 => {}
                other => panic!("unexpected opcode {other}"),
            }
            let m: serde_json::Value = serde_json::from_slice(&payload).unwrap();
            got.push(m.clone());
            let (Some(id), Some(method)) = (m.get("id"), m["method"].as_str()) else { continue };
            let reply = match method {
                "initialize" => {
                    frame(&mut s, 0x9, b"are you there").await;
                    let note = serde_json::json!({"method": "x/note", "params": {"pad": "p".repeat(300)}});
                    frame(&mut s, 0x1, note.to_string().as_bytes()).await;
                    serde_json::json!({"id": id, "result": {"userAgent": "codex/1"}})
                }
                "thread/read" => serde_json::json!({"id": id, "result": {"thread": {"status": {"type": status}}}}),
                "thread/resume" if refuse => {
                    serde_json::json!({"id": id, "error": {"code": -32600, "message": "no rollout"}})
                }
                "thread/resume" => serde_json::json!({"id": id, "result": {"thread": {}}}),
                other => panic!("unexpected method {other}"),
            };
            // A reply split across two frames, as a server may send it.
            let text = reply.to_string();
            let (a, b) = text.as_bytes().split_at(5);
            s.write_all(&[0x01, a.len() as u8]).await.unwrap();
            s.write_all(a).await.unwrap();
            frame(&mut s, 0x0, b).await;
        }
    }

    /// Review of #218 (P1): mail Codex took but could not load the thread for is not queued again; the thread is loaded
    /// with `thread/resume`, after `thread/read` says it isn't, as `codex queue --resume` does.
    #[tokio::test]
    async fn resume_loads_an_unloaded_thread_over_the_control_socket_and_leaves_a_loaded_one() {
        let methods = |got: &[serde_json::Value]| {
            got.iter().map(|m| m["method"].as_str().unwrap().to_string()).collect::<Vec<_>>()
        };
        let (mut client, server) = tokio::io::duplex(1 << 16);
        let fake = tokio::spawn(fake_app_server(server, "notLoaded", false));
        assert_eq!(resume_over(&mut client, "t1").await, Ok(true));
        drop(client);
        let got = fake.await.unwrap();
        assert_eq!(methods(&got), ["initialize", "initialized", "thread/read", "thread/resume"]);
        assert_eq!(got[0]["params"]["clientInfo"]["name"], "reason_network");
        assert_eq!(got[2]["params"], serde_json::json!({"threadId": "t1"}));
        assert_eq!(got[3]["params"], serde_json::json!({"threadId": "t1", "excludeTurns": true}));
        // Loaded already: nothing to resume.
        let (mut client, server) = tokio::io::duplex(1 << 16);
        let fake = tokio::spawn(fake_app_server(server, "idle", false));
        assert_eq!(resume_over(&mut client, "t1").await, Ok(false));
        drop(client);
        assert_eq!(methods(&fake.await.unwrap()), ["initialize", "initialized", "thread/read"]);
        // Refused: an error, so it is asked again later.
        let (mut client, server) = tokio::io::duplex(1 << 16);
        let fake = tokio::spawn(fake_app_server(server, "notLoaded", true));
        let refused = resume_over(&mut client, "t1").await.unwrap_err();
        assert!(refused.contains("thread/resume: no rollout"), "{refused}");
        drop(client);
        fake.await.unwrap();
    }

    /// Review of #218 (P2): an empty `CODEX_HOME` is no home, as Codex has it: the default one, not this folder.
    #[test]
    fn an_empty_codex_home_is_the_default_one() {
        assert_eq!(control_socket(Some(Path::new(""))), control_socket(None));
        assert!(control_socket(None).ends_with("app-server-control/app-server-control.sock"));
        assert!(control_socket(None).is_absolute());
    }

    /// Review of #218: Codex advertises its control socket as a link to a short protected one. With a long
    /// `CODEX_HOME`, the advertised path is too long to connect to: the link's target is what answers.
    #[cfg(unix)]
    #[tokio::test]
    async fn an_app_server_behind_a_long_codex_home_answers_through_its_socket_link() {
        let base = std::env::temp_dir().join(format!("rn-sock-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        let home =
            base.join("a-codex-home-whose-path-is-longer-than-any-unix-socket-address-can-hold").join("x".repeat(60));
        let dir = home.join("app-server-control");
        std::fs::create_dir_all(&dir).unwrap();
        let short = base.join("s");
        let _listener = tokio::net::UnixListener::bind(&short).unwrap();
        let advertised = dir.join("app-server-control.sock");
        std::os::unix::fs::symlink(&short, &advertised).unwrap();
        assert!(advertised.as_os_str().len() > 108, "{}", advertised.display());
        // The premise: the advertised path itself can't be connected to.
        assert!(tokio::net::UnixStream::connect(&advertised).await.is_err());
        assert_eq!(control_socket(Some(&home)), short);
        assert!(app_server_up(Some(&home)).await);
        let _ = std::fs::remove_dir_all(&base);
    }
}
