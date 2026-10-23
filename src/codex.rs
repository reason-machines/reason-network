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

/// The last turn event in `from..to` (whole lines from `from`, which starts one), and where the last whole line in it
/// ends: a line still being written is read next time.
fn last_turn_event(file: &mut File, from: u64, to: u64) -> std::io::Result<(u64, Option<bool>)> {
    let (mut end, mut whole_to, mut cut) = (to, None, Vec::new());
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
        } else {
            buf.extend_from_slice(&cut);
        }
        // The first line here may begin in the chunk before: keep it for then.
        let first = if start > from { buf.iter().position(|&b| b == b'\n').map_or(buf.len(), |i| i + 1) } else { 0 };
        if let Some(running) = buf[first..].split(|&b| b == b'\n').rev().find_map(turn_event) {
            return Ok((whole_to.unwrap_or(from), Some(running)));
        }
        cut = buf[..first].to_vec();
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

        // Rewritten shorter: read again.
        std::fs::write(&path, event("task_started")).unwrap();
        assert_eq!(turn_running(&path), Some(true));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
