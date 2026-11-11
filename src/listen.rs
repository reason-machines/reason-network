use crate::{cli_session, client::Link, proto::*, render, util};
use anyhow::{Context, Result, bail};
use std::{future::Future, process::Stdio, sync::Arc, time::Duration};
use tokio::{io::AsyncWriteExt, process::Command};

#[derive(Debug, PartialEq)]
enum Outcome {
    Success,
    Failed,
    Timeout,
}

struct Running {
    child: tokio::process::Child,
    reaped: bool,
}

impl Running {
    fn new(child: tokio::process::Child) -> Self {
        Self { child, reaped: false }
    }

    async fn wait(&mut self) -> std::io::Result<std::process::ExitStatus> {
        let status = self.child.wait().await?;
        self.reaped = true;
        Ok(status)
    }
}

impl Drop for Running {
    fn drop(&mut self) {
        // After wait() the pid can be reused; only kill a process still running.
        if self.reaped {
            return;
        }
        if let Some(pid) = self.child.id() {
            #[cfg(unix)]
            // SAFETY: this child owns the process group and hasn't been reaped.
            unsafe {
                libc::killpg(pid as libc::pid_t, libc::SIGKILL);
            }
            #[cfg(windows)]
            {
                let _ = util::no_window(std::process::Command::new("taskkill.exe"))
                    .args(["/T", "/F", "/PID", &pid.to_string()])
                    .stdout(Stdio::null())
                    .stderr(Stdio::null())
                    .status();
            }
        }
    }
}

async fn run(command: &str, text: &str, timeout: Duration) -> Result<Outcome> {
    #[cfg(unix)]
    let mut cmd = Command::new("sh");
    #[cfg(unix)]
    cmd.arg("-c").process_group(0);
    #[cfg(windows)]
    let mut cmd = Command::new("cmd");
    #[cfg(windows)]
    cmd.arg("/C");
    cmd.arg(command).stdin(Stdio::piped()).stdout(Stdio::inherit()).stderr(Stdio::inherit()).kill_on_drop(true);
    let mut child = Running::new(util::no_window(cmd).spawn().context("starting listen command")?);
    let mut stdin = child.child.stdin.take().unwrap();
    let result = tokio::time::timeout(timeout, async {
        let written = stdin.write_all(text.as_bytes()).await;
        drop(stdin);
        let status = child.wait().await?;
        if !status.success() {
            return Ok(Outcome::Failed);
        }
        written.context("writing message to listen command")?;
        Ok(Outcome::Success)
    })
    .await;
    match result {
        Ok(result) => result,
        Err(_) => {
            // Drop kills the group; kill_on_drop also arranges to reap the shell.
            drop(child);
            Ok(Outcome::Timeout)
        }
    }
}

async fn deliver<F, Fut>(messages: &[Message], command: &str, timeout: Duration, mut delivered: F) -> Result<()>
where
    F: FnMut(String) -> Fut,
    Fut: Future<Output = Result<()>>,
{
    for msg in messages {
        let text = render::for_agent(std::slice::from_ref(msg), Some("cli"), true);
        match run(command, &text, timeout).await? {
            Outcome::Success => delivered(msg.id.clone()).await?,
            Outcome::Failed => bail!("listen command failed; message {} remains unread", msg.id),
            Outcome::Timeout => bail!("listen command timed out; message {} remains unread", msg.id),
        }
    }
    Ok(())
}

pub(super) async fn listen(link: &Arc<Link>, id: &str, wait: u64, command: &str, timeout: Duration) -> Result<()> {
    let key = format!("cli:{id}");
    loop {
        cli_session(link, id, "stop").await?;
        let inbox: Inbox = link.call(Request::Inbox { key: key.clone(), wait_secs: wait, peek: true }).await?;
        if inbox.messages.is_empty() {
            if wait == 0 {
                return Ok(());
            }
            continue;
        }
        cli_session(link, id, "prompt").await?;
        deliver(&inbox.messages, command, timeout, |id| async {
            link.call::<serde_json::Value>(Request::Delivered { key: key.clone(), ids: vec![id] }).await?;
            Ok(())
        })
        .await?;
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use std::sync::Mutex;

    struct Temp(std::path::PathBuf);

    impl Temp {
        fn new() -> Self {
            let path = std::env::temp_dir().join(format!("reason-listen-{}", ulid::Ulid::generate()));
            std::fs::create_dir(&path).unwrap();
            Self(path)
        }
    }

    impl Drop for Temp {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn messages() -> Vec<Message> {
        (1..=3)
            .map(|id| {
                serde_json::from_value(serde_json::json!({
                    "id": id.to_string(), "from": "alice/test", "to": "bob/test",
                    "body": format!("message {id}"), "wake": false, "sent_at": 0
                }))
                .unwrap()
            })
            .collect()
    }

    #[tokio::test]
    async fn success_reads_stdin() {
        assert_eq!(run("test \"$(cat)\" = hello", "hello", Duration::from_secs(2)).await.unwrap(), Outcome::Success);
    }

    #[tokio::test]
    async fn nonzero_leaves_message_and_later_messages_unread() {
        assert_eq!(run("exit 1", "hello", Duration::from_secs(2)).await.unwrap(), Outcome::Failed);
        let delivered = Mutex::new(Vec::new());
        assert!(
            deliver(&messages(), "exit 1", Duration::from_secs(2), |id| {
                delivered.lock().unwrap().push(id);
                async { Ok(()) }
            })
            .await
            .is_err()
        );
        assert!(delivered.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn failure_stops_after_successful_delivery() {
        let delivered = Mutex::new(Vec::new());
        let result = deliver(
            &messages(),
            "text=$(cat); case \"$text\" in *'message 2'*) exit 1;; esac",
            Duration::from_secs(2),
            |id| {
                delivered.lock().unwrap().push(id);
                async { Ok(()) }
            },
        )
        .await;
        assert!(result.is_err());
        assert_eq!(*delivered.lock().unwrap(), ["1"]);
    }

    #[tokio::test]
    async fn timeout_kills_children() {
        let dir = Temp::new();
        let pidfile = dir.0.join("pid");
        let command = format!("sleep 5 & echo $! > '{}'; wait", pidfile.display());
        let start = std::time::Instant::now();
        assert_eq!(run(&command, "", Duration::from_secs(1)).await.unwrap(), Outcome::Timeout);
        assert!(start.elapsed() < Duration::from_secs(3));
        let pid = std::fs::read_to_string(pidfile).unwrap();
        // ps reports a killed child as absent or a zombie until its parent reaps it.
        for _ in 0..100 {
            let output = std::process::Command::new("ps").args(["-o", "stat=", "-p", pid.trim()]).output().unwrap();
            let state = String::from_utf8_lossy(&output.stdout);
            if state.trim().is_empty() || state.trim().starts_with('Z') {
                return;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        panic!("timed-out child is still running");
    }

    #[tokio::test]
    async fn burst_is_serial_and_framed() {
        let dir = Temp::new();
        let command =
            format!("mkdir '{0}/lock' || exit 1; cat >> '{0}/text'; sleep 0.02; rmdir '{0}/lock'", dir.0.display());
        let messages = messages();
        let delivered = Mutex::new(Vec::new());
        deliver(&messages, &command, Duration::from_secs(2), |id| {
            delivered.lock().unwrap().push(id);
            async { Ok(()) }
        })
        .await
        .unwrap();
        assert_eq!(*delivered.lock().unwrap(), ["1", "2", "3"]);
        let expected: String =
            messages.iter().map(|m| render::for_agent(std::slice::from_ref(m), Some("cli"), true)).collect();
        assert_eq!(std::fs::read_to_string(dir.0.join("text")).unwrap(), expected);
    }
}
