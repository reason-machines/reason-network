//! Reason sessions. Reason's agent runs in its cloud, so there is no hook or MCP server here: when `reason` is on
//! (`reason network setup`), the daemon lists your live Reason sessions (`reason session list --json`) every few seconds,
//! and delivers to them with `reason session send`. They reply through the Reason Network CLI, which runs here.
//!
//! Where the Reason CLI manages this engine, the daemon also keeps the CLI's `reason network __relay` running: it
//! carries the workspace's cloud agents' calls (Reason's hosted tools) onto the network, for as long as the daemon runs.

use super::{Daemon, log};
use crate::{agent, git::GitInfo, proto::*, util};
use anyhow::{Result, bail};
use serde_json::Value;
use std::{
    collections::HashSet,
    sync::Arc,
    time::{Duration, Instant},
};

const EVERY: Duration = Duration::from_secs(15);
/// What the loop reads of each session. With only these, Reason answers "unchanged" (the CLI keeps the last page)
/// until one of them changes, instead of sending every session's prompt and usage every 15 s.
const FIELDS: &str = "title,repo,branch,status,finished_at,is_archived";
/// When the narrow list fails and the plain one works (a CLI from before `--fields`, a server that refuses one of
/// them), the plain list is read for this long before trying again (the CLI updates itself).
const PLAIN_FOR: Duration = Duration::from_secs(3600);
/// The relay is started again this long after it stops, doubling up to `RELAY_MAX_WAIT` while it keeps stopping.
const RELAY_WAIT: Duration = Duration::from_secs(30);
const RELAY_MAX_WAIT: Duration = Duration::from_secs(600);

impl Daemon {
    pub(super) async fn reason_loop(self: Arc<Self>) {
        let mut failing = false;
        loop {
            match self.reason_sync().await {
                Ok(()) => failing = false,
                Err(e) if !failing => {
                    log(&format!("listing Reason sessions: {e:#}"));
                    failing = true;
                }
                Err(_) => {}
            }
            tokio::time::sleep(EVERY).await;
        }
    }

    /// Keeps `reason network __relay` running. It reads nothing from its stdin, and ends when that closes: with this
    /// daemon, however it ends. Its stderr goes where the daemon's does (the log).
    pub(super) async fn relay_loop(self: Arc<Self>) {
        let mut wait = RELAY_WAIT;
        let mut missing = false;
        loop {
            let started = std::time::Instant::now();
            match agent::find_bin("reason") {
                None if !missing => {
                    log(
                        "reason network __relay: the reason CLI isn't on this daemon's PATH; Reason's cloud agents can't reach this computer's agents",
                    );
                    missing = true;
                }
                None => {}
                Some(bin) => {
                    missing = false;
                    let child = crate::util::no_window(tokio::process::Command::new(&bin))
                        .args(["network", "__relay"])
                        .stdin(std::process::Stdio::piped())
                        .stdout(std::process::Stdio::null())
                        .kill_on_drop(true)
                        .spawn();
                    match child {
                        Ok(mut child) => {
                            // Held here, not left to `wait`, which would close it first.
                            let _stdin = child.stdin.take();
                            match child.wait().await {
                                Ok(status) => log(&format!("reason network __relay stopped ({status})")),
                                Err(e) => log(&format!("reason network __relay: {e}")),
                            }
                        }
                        Err(e) => log(&format!("starting reason network __relay: {e}")),
                    }
                }
            }
            if started.elapsed() > RELAY_MAX_WAIT {
                wait = RELAY_WAIT;
            }
            tokio::time::sleep(wait).await;
            wait = (wait * 2).min(RELAY_MAX_WAIT);
        }
    }

    async fn reason_sync(self: &Arc<Self>) -> Result<()> {
        let Some(bin) = agent::find_bin("reason") else { return Ok(()) };
        self.sync_from(&bin).await
    }

    async fn sync_from(self: &Arc<Self>, bin: &std::path::Path) -> Result<()> {
        let narrow = self.plain_until.lock().unwrap().is_none_or(|until| Instant::now() >= until);
        let narrowed =
            if narrow { list_sessions(bin, true).await.ok().filter(|out| out.status.success()) } else { None };
        let out = match narrowed {
            Some(out) => out,
            None => {
                let out = list_sessions(bin, false).await?;
                // Whatever the narrow list failed on, the plain one works: read it for a while. When both fail
                // (signed out, offline), the next time tries narrow again.
                if narrow && out.status.success() {
                    *self.plain_until.lock().unwrap() = Some(Instant::now() + PLAIN_FOR);
                }
                out
            }
        };
        if !out.status.success() {
            bail!("{}", String::from_utf8_lossy(&out.stderr).trim());
        }
        let list: Value = serde_json::from_slice(&out.stdout)?;
        let live: Vec<&Value> = list["items"]
            .as_array()
            .map(|a| a.iter().filter(|s| s["finished_at"].is_null() && s["is_archived"] != true).collect())
            .unwrap_or_default();

        let now = util::now_ms();
        let mut seen = HashSet::new();
        for s in live {
            let Some(id) = s["session_id"].as_str().or(s["id"].as_str()) else { continue };
            let key = format!("reason:{id}");
            let text = |k: &str| s[k].as_str().filter(|v| !v.is_empty()).map(str::to_string);
            let title = text("title").map(|t| crate::team::without_tickets(&t));
            seen.insert(key.clone());
            if !self.inner.lock().unwrap().live.contains_key(&key) {
                let git = GitInfo { root: None, repo: text("repo"), branch: text("branch") };
                // Named for its repo, else for the first words of its title (kept once chosen).
                let name = git
                    .repo
                    .is_none()
                    .then(|| title.as_deref().map(first_words))
                    .flatten()
                    .map(|t| util::joined(&t, "reason"));
                let reg = SessionReg {
                    key: key.clone(),
                    agent: "reason".into(),
                    cwd: "~".into(),
                    agent_session_id: Some(id.to_string()),
                    pid: None,
                    tier: Tier::Push,
                    name,
                    claude_dir: None,
                };
                self.register(None, reg, git)?;
            }
            let mut g = self.inner.lock().unwrap();
            if let Some(l) = g.live.get_mut(&key) {
                // Seen just now: keeps it from being pruned like a session whose hooks went quiet.
                l.last_hook = Some(now);
                l.doing = title;
                let status = if s["status"].as_str() == Some("running") { "busy" } else { "idle" };
                if l.status.as_ref().is_none_or(|(st, _)| st != status) {
                    l.status = Some((status.to_string(), now));
                }
            }
            // Mail a failed push left behind goes out again.
            let address = g.live.get(&key).map(|l| self.addr(&l.name));
            if let Some(address) = address {
                let waiting: Vec<_> = g.pending(&address)?.into_iter().filter(|m| m.wake).collect();
                if !waiting.is_empty() {
                    self.deliver(&mut g, &key, waiting, true);
                }
            }
        }
        let mut g = self.inner.lock().unwrap();
        let gone: Vec<String> = g
            .live
            .iter()
            .filter(|(k, l)| l.reg.agent == "reason" && !seen.contains(*k))
            .map(|(k, _)| k.clone())
            .collect();
        for key in gone {
            if let Some(l) = g.live.remove(&key) {
                let _ = g.store.touch_session(&l.name, now);
            }
        }
        Ok(())
    }
}

async fn list_sessions(bin: &std::path::Path, narrow: bool) -> Result<std::process::Output> {
    let mut args = vec!["session", "list", "--limit", "20", "--json"];
    if narrow {
        args.extend(["--fields", FIELDS]);
    }
    let out = crate::util::no_window(tokio::process::Command::new(bin))
        .args(args)
        .stdin(std::process::Stdio::null())
        .kill_on_drop(true)
        .output();
    Ok(tokio::time::timeout(Duration::from_secs(30), out).await??)
}

fn first_words(title: &str) -> String {
    util::slug(&title.split_whitespace().take(3).collect::<Vec<_>>().join(" "))
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use std::path::PathBuf;

    /// A stub `reason` whose `session list --fields …` fails with `narrow_err`, and whose plain list prints one live
    /// session, or fails with `plain_err`. Each call's arguments go to `calls`. Its folder goes when it does.
    struct Stub {
        dir: PathBuf,
        bin: PathBuf,
        calls: PathBuf,
    }

    impl Stub {
        fn new(narrow_err: &str, plain_err: Option<&str>) -> Stub {
            let plain = match plain_err {
                Some(e) => format!("echo '{e}' >&2; exit 1"),
                None => r#"echo '{"items":[{"session_id":"r-1","status":"running","title":"Fix flaky billing test","finished_at":null,"is_archived":false}]}'"#.into(),
            };
            Stub::running(narrow_err, plain)
        }

        /// The same, with the plain list running `plain`.
        fn running(narrow_err: &str, plain: String) -> Stub {
            let dir = std::env::temp_dir().join(format!("reason-network-reason-{}", ulid::Ulid::generate()));
            std::fs::create_dir_all(&dir).unwrap();
            let (bin, calls) = (dir.join("reason"), dir.join("calls"));
            let script = format!(
                "#!/bin/sh\necho \"$*\" >> '{}'\ncase \" $* \" in *' --fields '*) echo '{narrow_err}' >&2; exit 1;; esac\n{plain}\n",
                calls.display()
            );
            // Written by a child: a file this process has open for writing, which a test forking at that moment
            // holds open until its exec, can't be run (ETXTBSY).
            let written = std::process::Command::new("sh")
                .args(["-c", "printf '%s' \"$1\" > \"$2\" && chmod 755 \"$2\"", "sh", &script])
                .arg(&bin)
                .status()
                .unwrap();
            assert!(written.success());
            Stub { dir, bin, calls }
        }

        /// Whether each call so far asked narrow.
        fn calls(&self) -> Vec<bool> {
            std::fs::read_to_string(&self.calls).unwrap().lines().map(|l| l.contains("--fields")).collect()
        }
    }

    impl Drop for Stub {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.dir);
        }
    }

    #[tokio::test]
    async fn a_narrow_list_that_fails_falls_back_to_the_plain_list_for_a_while() {
        let d = Arc::new(super::super::test_daemon(Default::default()));
        let stub = Stub::new("Reason: 400 Bad Request: fields: \"repo\" is not a property", None);
        d.sync_from(&stub.bin).await.unwrap();
        assert!(d.inner.lock().unwrap().live.contains_key("reason:r-1"));
        d.sync_from(&stub.bin).await.unwrap();
        assert_eq!(stub.calls(), [true, false, false]);
        // Once the while is up, narrow is tried again.
        *d.plain_until.lock().unwrap() = Some(Instant::now());
        d.sync_from(&stub.bin).await.unwrap();
        assert_eq!(stub.calls(), [true, false, false, true, false]);
        assert!(d.inner.lock().unwrap().live.contains_key("reason:r-1"));
    }

    #[tokio::test]
    async fn a_reason_session_titled_with_an_invite_line_shows_it_without_the_link() {
        let d = Arc::new(super::super::test_daemon(Default::default()));
        let ticket = format!("rn1-{}", "AbCd_-09".repeat(12));
        let title = format!("Join the Reason Network: https://reasonmachines.com/network/join/#{ticket}");
        let item = serde_json::json!({"session_id": "r-1", "status": "running", "title": title, "finished_at": null});
        let stub = Stub::running("no", format!("echo '{}'", serde_json::json!({ "items": [item] })));
        d.sync_from(&stub.bin).await.unwrap();
        let doing = d.inner.lock().unwrap().live["reason:r-1"].doing.clone();
        assert_eq!(doing.as_deref(), Some("Join the Reason Network: (team link)"));
    }

    #[tokio::test]
    async fn a_list_that_fails_either_way_is_one_error_and_tries_narrow_again() {
        let d = Arc::new(super::super::test_daemon(Default::default()));
        let signed_out = "Reason: Not signed in. Run reason login.";
        let stub = Stub::new(signed_out, Some(signed_out));
        assert_eq!(d.sync_from(&stub.bin).await.unwrap_err().to_string(), signed_out);
        assert!(d.plain_until.lock().unwrap().is_none());
        assert_eq!(stub.calls(), [true, false]);
    }
}
