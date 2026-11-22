use std::{
    collections::BTreeSet,
    process::{Command, Stdio},
    sync::Mutex,
    time::{Duration, Instant},
};

#[derive(Debug, Clone, Default)]
pub struct GitInfo {
    pub root: Option<String>,
    /// `owner/name` from the origin remote, or the checkout's directory name.
    pub repo: Option<String>,
    pub branch: Option<String>,
}

/// How long one folder's lookup may take in all: git can hang (a stuck network mount, a lock), and `who` waits on it.
pub const BUDGET: Duration = Duration::from_secs(2);

/// Folders where a git that ran over its budget was killed and hasn't gone yet: on a dead hard network mount a
/// process can outlast its SIGKILL. No other git is started there until it has, so such a folder holds one stuck git
/// and the thread waiting on it, not one more of each at every lookup.
static STUCK: Mutex<BTreeSet<String>> = Mutex::new(BTreeSet::new());

/// A folder's place in [`STUCK`], for as long as this lives.
struct Stuck(String);

impl Stuck {
    fn mark(cwd: &str) -> Self {
        STUCK.lock().unwrap().insert(cwd.to_string());
        Self(cwd.to_string())
    }
}

impl Drop for Stuck {
    fn drop(&mut self) {
        STUCK.lock().unwrap().remove(&self.0);
    }
}

/// The checkout `cwd` is in. `None` (an empty `GitInfo` is the default) when git didn't answer within [`BUDGET`], or
/// one that didn't is still there.
pub fn lookup(cwd: &str) -> Option<GitInfo> {
    lookup_with("git", cwd, BUDGET)
}

fn lookup_with(git: &str, cwd: &str, budget: Duration) -> Option<GitInfo> {
    if STUCK.lock().unwrap().contains(cwd) {
        return None;
    }
    let deadline = Instant::now() + budget;
    let mut timed_out = false;
    let mut run = |args: &[&str]| {
        if timed_out {
            return None;
        }
        let mut cmd = Command::new(git);
        cmd.arg("-C").arg(cwd).args(args).stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::null());
        // CREATE_NO_WINDOW: `reason-network mcp` started by an agent without a console would flash one per lookup.
        #[cfg(windows)]
        std::os::windows::process::CommandExt::creation_flags(&mut cmd, 0x0800_0000);
        // Its own process group, so whatever git started (a hook, a helper) is killed with it.
        #[cfg(unix)]
        std::os::unix::process::CommandExt::process_group(&mut cmd, 0);
        let mut child = cmd.spawn().ok()?;
        // What these print is a line, far below a pipe's buffer, so waiting before reading can't deadlock.
        loop {
            match child.try_wait() {
                Ok(Some(_)) => break,
                Ok(None) if Instant::now() < deadline => std::thread::sleep(Duration::from_millis(5)),
                _ => {
                    timed_out = true;
                    // Not waited for yet, so the id is still this git's (and its group's).
                    #[cfg(unix)]
                    unsafe {
                        libc::killpg(child.id() as libc::pid_t, libc::SIGKILL);
                    }
                    let _ = child.kill();
                    let stuck = Stuck::mark(cwd);
                    std::thread::spawn(move || {
                        let _ = child.wait();
                        drop(stuck);
                    });
                    return None;
                }
            }
        }
        let out = child.wait_with_output().ok()?;
        let s = String::from_utf8_lossy(&out.stdout).trim().to_string();
        (out.status.success() && !s.is_empty()).then_some(s)
    };
    let Some(root) = run(&["rev-parse", "--show-toplevel"]) else {
        return (!timed_out).then(GitInfo::default);
    };
    let branch = run(&["rev-parse", "--abbrev-ref", "HEAD"]);
    let repo = run(&["config", "--get", "remote.origin.url"])
        .as_deref()
        .and_then(repo_slug)
        .or_else(|| root.rsplit('/').next().map(str::to_string));
    (!timed_out).then_some(GitInfo { root: Some(root), repo, branch })
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
    use super::repo_slug;

    #[cfg(unix)]
    #[test]
    fn a_git_that_hangs_gives_up_at_the_budget() {
        use super::lookup_with;
        use std::os::unix::fs::PermissionsExt;
        use std::time::{Duration, Instant};
        let git = std::env::temp_dir().join(format!("rn-hanging-git-{}", std::process::id()));
        std::fs::write(&git, "#!/bin/sh\nexec sleep 600\n").unwrap();
        std::fs::set_permissions(&git, std::fs::Permissions::from_mode(0o755)).unwrap();
        let started = Instant::now();
        let info = lookup_with(git.to_str().unwrap(), "/rn-hanging-git", Duration::from_millis(300));
        let _ = std::fs::remove_file(&git);
        assert!(info.is_none());
        assert!(started.elapsed() < Duration::from_secs(5), "took {:?}", started.elapsed());
        // The killed git goes, and with it what kept another from being started there.
        while super::STUCK.lock().unwrap().contains("/rn-hanging-git") {
            assert!(started.elapsed() < Duration::from_secs(30), "the killed git was never reaped");
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    #[cfg(unix)]
    #[test]
    fn no_git_is_started_in_a_folder_where_a_killed_one_is_still_there() {
        use super::{Stuck, lookup_with};
        use std::time::Duration;
        // `echo` stands in for a git that answers at once, with what it was asked.
        let ask = || lookup_with("echo", "/rn-stuck-git", Duration::from_secs(5));
        // As after a timeout whose git SIGKILL hasn't taken away yet.
        let stuck = Stuck::mark("/rn-stuck-git");
        assert!(ask().is_none());
        // Once it has gone, git is asked again.
        drop(stuck);
        assert!(ask().is_some_and(|info| info.root.is_some()));
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
