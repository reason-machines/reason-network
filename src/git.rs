use std::process::{Command, Stdio};

#[derive(Debug, Clone, Default)]
pub struct GitInfo {
    pub root: Option<String>,
    /// `owner/name` from the origin remote, or the checkout's directory name.
    pub repo: Option<String>,
    pub branch: Option<String>,
}

pub fn lookup(cwd: &str) -> GitInfo {
    let run = |args: &[&str]| {
        let mut cmd = Command::new("git");
        cmd.arg("-C").arg(cwd).args(args).stderr(Stdio::null());
        // CREATE_NO_WINDOW: `reason-network mcp` started by an agent without a console would flash one per lookup.
        #[cfg(windows)]
        std::os::windows::process::CommandExt::creation_flags(&mut cmd, 0x0800_0000);
        let out = cmd.output().ok()?;
        let s = String::from_utf8_lossy(&out.stdout).trim().to_string();
        (out.status.success() && !s.is_empty()).then_some(s)
    };
    let Some(root) = run(&["rev-parse", "--show-toplevel"]) else {
        return GitInfo::default();
    };
    let branch = run(&["rev-parse", "--abbrev-ref", "HEAD"]);
    let repo = run(&["config", "--get", "remote.origin.url"])
        .as_deref()
        .and_then(repo_slug)
        .or_else(|| root.rsplit('/').next().map(str::to_string));
    GitInfo { root: Some(root), repo, branch }
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
