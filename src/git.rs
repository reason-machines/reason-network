use std::{
    process::{Command, Stdio},
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

/// The checkout `cwd` is in. `None` (an empty `GitInfo` is the default) when git didn't answer within [`BUDGET`].
pub fn lookup(cwd: &str) -> Option<GitInfo> {
    lookup_with("git", cwd, BUDGET)
}

fn lookup_with(git: &str, cwd: &str, budget: Duration) -> Option<GitInfo> {
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
        let mut child = cmd.spawn().ok()?;
        // What these print is a line, far below a pipe's buffer, so waiting before reading can't deadlock.
        loop {
            match child.try_wait() {
                Ok(Some(_)) => break,
                Ok(None) if Instant::now() < deadline => std::thread::sleep(Duration::from_millis(5)),
                _ => {
                    timed_out = true;
                    let _ = child.kill();
                    std::thread::spawn(move || child.wait());
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
        let info = lookup_with(git.to_str().unwrap(), "/", Duration::from_millis(300));
        let _ = std::fs::remove_file(&git);
        assert!(info.is_none());
        assert!(started.elapsed() < Duration::from_secs(5), "took {:?}", started.elapsed());
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
