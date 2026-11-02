use std::{
    io::Read,
    process::{Command, Stdio},
    time::{Duration, Instant},
};

/// How long one git command may take. Under a macOS LaunchAgent, git in a folder behind a privacy prompt nobody sees,
/// an iCloud file not downloaded yet or a hung mount can block for good: past this the folder goes without repo and
/// branch rather than holding up whoever asked.
pub const COMMAND_TIMEOUT: Duration = Duration::from_secs(2);

#[derive(Debug, Clone, Default)]
pub struct GitInfo {
    pub root: Option<String>,
    /// `owner/name` from the origin remote, or the checkout's directory name.
    pub repo: Option<String>,
    pub branch: Option<String>,
}

/// git didn't answer in time.
#[derive(Debug)]
pub struct TimedOut;

pub fn lookup(cwd: &str) -> GitInfo {
    lookup_within(cwd, COMMAND_TIMEOUT).unwrap_or_default()
}

/// The checkout `cwd` is in, each git command given `timeout`.
pub fn lookup_within(cwd: &str, timeout: Duration) -> Result<GitInfo, TimedOut> {
    let run = |args: &[&str]| -> Result<Option<String>, TimedOut> {
        let mut cmd = Command::new("git");
        cmd.arg("-C").arg(cwd).args(args);
        // CREATE_NO_WINDOW: `reason-network mcp` started by an agent without a console would flash one per lookup.
        #[cfg(windows)]
        std::os::windows::process::CommandExt::creation_flags(&mut cmd, 0x0800_0000);
        let Some((ok, out)) = output_within(cmd, timeout)? else { return Ok(None) };
        let s = String::from_utf8_lossy(&out).trim().to_string();
        Ok((ok && !s.is_empty()).then_some(s))
    };
    let Some(root) = run(&["rev-parse", "--show-toplevel"])? else {
        return Ok(GitInfo::default());
    };
    let branch = run(&["rev-parse", "--abbrev-ref", "HEAD"])?;
    let repo = run(&["config", "--get", "remote.origin.url"])?
        .as_deref()
        .and_then(repo_slug)
        .or_else(|| root.rsplit('/').next().map(str::to_string));
    Ok(GitInfo { root: Some(root), repo, branch })
}

/// Runs `cmd` without stdin or stderr: whether it succeeded and its stdout, `None` if it couldn't start, `TimedOut`
/// if it, or anything it started that holds its stdout, ran past `timeout`. On Unix it runs in a process group of its
/// own, killed whole at the timeout.
fn output_within(mut cmd: Command, timeout: Duration) -> Result<Option<(bool, Vec<u8>)>, TimedOut> {
    cmd.stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::null());
    #[cfg(unix)]
    std::os::unix::process::CommandExt::process_group(&mut cmd, 0);
    let Ok(mut child) = cmd.spawn() else { return Ok(None) };
    let mut stdout = child.stdout.take();
    // Read on the side, so output larger than the pipe holds can't stall the child.
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let mut out = Vec::new();
        if let Some(s) = stdout.as_mut() {
            let _ = s.read_to_end(&mut out);
        }
        let _ = tx.send(out);
    });
    let deadline = Instant::now() + timeout;
    let status = loop {
        match child.try_wait() {
            Ok(Some(st)) => break Some(st),
            Ok(None) if Instant::now() < deadline => std::thread::sleep(Duration::from_millis(5)),
            Ok(None) => break None,
            Err(_) => break None,
        }
    };
    let out = status.and_then(|_| rx.recv_timeout(deadline.saturating_duration_since(Instant::now())).ok());
    match (status, out) {
        (Some(st), Some(out)) => Ok(Some((st.success(), out))),
        _ => {
            // Its whole group, so nothing it started keeps the pipe (and the reader) open.
            // SAFETY: signals the process group it leads, which exists until it is reaped below.
            #[cfg(unix)]
            unsafe {
                libc::killpg(child.id() as libc::pid_t, libc::SIGKILL);
            }
            let _ = child.kill();
            let _ = child.wait();
            Err(TimedOut)
        }
    }
}

/// `git@github.com:acme/api.git`, `https://github.com/acme/api`, `ssh://git@host:22/acme/api.git` -> `acme/api`.
pub fn repo_slug(url: &str) -> Option<String> {
    let url = url.trim().trim_end_matches('/');
    let url = url.strip_suffix(".git").unwrap_or(url);
    let mut parts = url.rsplit(['/', ':']).filter(|p| !p.is_empty());
    let name = parts.next()?;
    match parts.next() {
        Some(owner) if !owner.contains('@') && !owner.contains('.') => Some(format!("{owner}/{name}")),
        _ => Some(name.to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(unix)]
    #[test]
    fn a_command_that_hangs_is_killed_at_the_timeout() {
        let started = Instant::now();
        let mut cmd = Command::new("sh");
        cmd.args(["-c", "sleep 30"]);
        assert!(output_within(cmd, Duration::from_millis(200)).is_err());
        assert!(started.elapsed() < Duration::from_secs(5), "{:?}", started.elapsed());
        // Nor does something it left running that holds its stdout; and that goes with it.
        let pidfile = std::env::temp_dir().join(format!("reason-network-git-{}", std::process::id()));
        let started = Instant::now();
        let mut cmd = Command::new("sh");
        cmd.args(["-c", "sleep 30 & echo $! > \"$1\"; printf done", "sh"]).arg(&pidfile);
        assert!(output_within(cmd, Duration::from_millis(300)).is_err());
        assert!(started.elapsed() < Duration::from_secs(5), "{:?}", started.elapsed());
        let pid: libc::pid_t = std::fs::read_to_string(&pidfile).unwrap().trim().parse().unwrap();
        let _ = std::fs::remove_file(&pidfile);
        std::thread::sleep(Duration::from_millis(100));
        // SAFETY: signal 0 only asks whether the process exists.
        assert_ne!(unsafe { libc::kill(pid, 0) }, 0, "the sleep it left running was killed");
        // One that reads stdin gets end of input rather than waiting on the daemon's.
        let mut cmd = Command::new("sh");
        cmd.args(["-c", "cat; echo done"]);
        let (ok, out) = output_within(cmd, Duration::from_secs(5)).unwrap().unwrap();
        assert!(ok);
        assert_eq!(String::from_utf8_lossy(&out).trim(), "done");
    }

    #[test]
    fn slugs() {
        assert_eq!(repo_slug("git@github.com:acme/api.git").as_deref(), Some("acme/api"));
        assert_eq!(repo_slug("https://github.com/acme/api").as_deref(), Some("acme/api"));
        assert_eq!(repo_slug("https://github.com/acme/api.git/").as_deref(), Some("acme/api"));
        assert_eq!(repo_slug("ssh://git@host:22/acme/api.git").as_deref(), Some("acme/api"));
        assert_eq!(repo_slug("/srv/git/api.git").as_deref(), Some("git/api"));
        assert_eq!(repo_slug("git@host.com:api.git").as_deref(), Some("api"));
    }
}
