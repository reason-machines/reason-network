//! Keeps a standalone install up to date: every few hours the daemon looks for a newer release and, if there is one,
//! installs it over its own binary (with install.sh, or install.ps1 on Windows, checksum verified) and restarts on it.
//! Off with `auto_update = false`, and in a state folder that has never been set up (no `config.toml`); never for builds
//! run from a source tree, nor for the build the Reason CLI pins (in `~/.reason/bin`), which updates with the CLI.

use super::{Daemon, log};
use anyhow::{Result, bail};
use std::{path::Path, sync::Arc, time::Duration};

const EVERY: Duration = Duration::from_secs(6 * 3600);

impl Daemon {
    pub(super) async fn update_loop(self: Arc<Self>) {
        tokio::time::sleep(Duration::from_secs(90)).await;
        loop {
            match update().await {
                Ok(Some(v)) => {
                    log(&format!("updated to {v}; restarting on it"));
                    self.restart_soon();
                    return;
                }
                Ok(None) => {}
                Err(e) => log(&format!("checking for updates: {e:#}")),
            }
            tokio::time::sleep(EVERY).await;
        }
    }
}

/// Whether this binary may replace itself.
pub(super) fn allowed(enabled: bool) -> bool {
    wanted(enabled, &crate::paths::config())
        && !crate::util::from_source()
        && !crate::managed()
        && crate::paths::env_var_os("NO_UPDATE").is_none()
}

/// `auto_update` counts only in a folder that has been set up. `init`, `join` and the setup wizard always write its
/// `config.toml`, so a folder without one (a stray or mistyped `REASON_NETWORK_HOME`) runs on defaults nobody chose for
/// it, and must not replace the binary that every folder shares.
fn wanted(enabled: bool, config: &Path) -> bool {
    enabled && config.exists()
}

/// Installs the latest release if it is newer than this one; returns its version.
async fn update() -> Result<Option<String>> {
    let latest = latest_version().await?;
    if !newer(&latest, env!("CARGO_PKG_VERSION")) {
        return Ok(None);
    }
    let dir = crate::util::current_exe()?.parent().map(|d| d.to_path_buf()).unwrap_or_default();
    let exe = dir.join(format!("{}{}", crate::AGENT_NAME, std::env::consts::EXE_SUFFIX));
    let waiting = crate::migrate::waiting(&exe);
    let mut cmd = crate::util::no_window(tokio::process::Command::from(crate::installer(&dir)));
    cmd.stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .kill_on_drop(true);
    // In its own process group, so a timeout kills the whole pipeline (curl and the inner sh too). (On Windows the
    // installer is one PowerShell, which `kill_on_drop` ends.)
    #[cfg(unix)]
    cmd.process_group(0);
    let child = cmd.spawn()?;
    #[cfg(unix)]
    let group = child.id();
    let out = match tokio::time::timeout(Duration::from_secs(300), child.wait_with_output()).await {
        Ok(out) => out?,
        Err(_) => {
            #[cfg(unix)]
            if let Some(pgid) = group {
                // SAFETY: signals the process group we just created.
                unsafe { libc::killpg(pgid as libc::pid_t, libc::SIGKILL) };
            }
            bail!("install timed out");
        }
    };
    if !out.status.success() {
        bail!("install failed: {}", String::from_utf8_lossy(&out.stderr).trim());
    }
    if waiting {
        crate::migrate::keep_waiting_copy(&exe);
    }
    // Restart only on a binary that really is newer (a release tagged without bumping its version must not loop).
    let installed =
        crate::util::no_window(tokio::process::Command::new(&exe)).arg("--version").kill_on_drop(true).output();
    let installed = tokio::time::timeout(Duration::from_secs(30), installed).await??;
    let installed =
        String::from_utf8_lossy(&installed.stdout).trim().rsplit(' ').next().unwrap_or_default().to_string();
    if !newer(&installed, env!("CARGO_PKG_VERSION")) {
        bail!("the latest release installed as {installed:?}, which is not newer than this one; not restarting");
    }
    Ok(Some(installed))
}

/// The newest release's version, from where GitHub's "latest release" link redirects.
async fn latest_version() -> Result<String> {
    let repo = crate::REPO;
    let null = if cfg!(windows) { "NUL" } else { "/dev/null" };
    let out = crate::util::no_window(tokio::process::Command::new("curl"))
        .args([
            "-fsSLI",
            "--max-time",
            "50",
            "-o",
            null,
            "-w",
            "%{url_effective}",
            &format!("https://github.com/{repo}/releases/latest"),
        ])
        .stdin(std::process::Stdio::null())
        .kill_on_drop(true)
        .output();
    let out = tokio::time::timeout(Duration::from_secs(60), out).await??;
    let url = String::from_utf8_lossy(&out.stdout).to_string();
    match url.rsplit_once("/tag/v") {
        Some((_, v)) if out.status.success() && !v.is_empty() => Ok(v.trim().to_string()),
        _ => bail!("no release found at {url}"),
    }
}

/// `a` is a later version than `b` (`major.minor.patch`).
fn newer(a: &str, b: &str) -> bool {
    let parse = |v: &str| -> Vec<u64> { v.split('.').map(|p| p.parse().unwrap_or(0)).collect() };
    parse(a) > parse(b)
}

#[cfg(test)]
mod tests {
    #[test]
    fn versions_compare_by_number() {
        assert!(super::newer("0.10.0", "0.9.3"));
        assert!(super::newer("1.0.0", "0.99.99"));
        assert!(!super::newer("0.6.3", "0.6.3"));
        assert!(!super::newer("0.6.2", "0.6.10"));
    }

    #[test]
    fn only_a_set_up_folder_updates() {
        let home = std::env::temp_dir().join(format!("reason-network-update-{}", ulid::Ulid::generate()));
        std::fs::create_dir_all(&home).unwrap();
        let config = home.join("config.toml");
        // No config.toml: defaults, so `auto_update` is on, but this folder was never set up (#171).
        assert!(!super::wanted(true, &config));
        // Set up, with `auto_update` at its default.
        std::fs::write(&config, "").unwrap();
        assert!(super::wanted(true, &config));
        assert!(!super::wanted(false, &config));
        std::fs::remove_dir_all(&home).unwrap();
    }
}
