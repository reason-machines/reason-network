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
    home.map(Path::to_path_buf)
        .or_else(|| std::env::var_os("CODEX_HOME").map(PathBuf::from))
        .unwrap_or_else(|| crate::paths::user_home().join(".codex"))
}

/// Whether a Codex app-server daemon answers on the control socket of the Codex home (`home` as in [`archived`]),
/// `app-server-control/app-server-control.sock`: the shared server `codex queue --resume` loads a thread through. With
/// none, `codex queue` queues through a server of its own, which can't load the thread (it warns "could not resume
/// thread" and exits 0), so the mail would wait in Codex until the person opens the thread.
pub async fn app_server_up(home: Option<&Path>) -> bool {
    let socket = codex_home(home).join("app-server-control").join("app-server-control.sock");
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
}
