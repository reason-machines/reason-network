//! A `git` that hangs in one folder never holds up `who`: the daemon looks up each session folder's checkout with a
//! deadline, and the folders that miss it show without their repo and branch. Runs the real daemon with a stand-in
//! `git` that hangs in one folder (as git can on a stuck network mount or a lock) and passes everything else through.
#![cfg(unix)]

use std::{
    fs,
    os::unix::fs::PermissionsExt,
    path::Path,
    process::{Child, Command, Stdio},
    time::{Duration, Instant},
};

struct Kill(Child);

impl Drop for Kill {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

/// An open Claude Code session in `cwd`, which `who` lists (and looks up the checkout of) with nothing registered.
fn claude_session(sessions: &Path, pid: u32, name: &str, cwd: &Path) {
    let body = serde_json::json!({ "pid": pid, "sessionId": name, "cwd": cwd, "name": name, "nameSource": "user" });
    fs::write(sessions.join(format!("{pid}.json")), body.to_string()).unwrap();
}

#[test]
fn a_hanging_git_in_one_folder_does_not_hold_up_who() {
    let root = std::env::temp_dir().join(format!("rn-slow-git-{}", std::process::id()));
    let _ = fs::remove_dir_all(&root);
    let (home, bin, fast, slow) = (root.join("home"), root.join("bin"), root.join("fast"), root.join("slow"));
    let sessions = home.join(".claude/sessions");
    for d in [&bin, &fast, &slow, &sessions] {
        fs::create_dir_all(d).unwrap();
    }
    let real = String::from_utf8(Command::new("sh").args(["-c", "command -v git"]).output().unwrap().stdout).unwrap();
    assert!(Command::new(real.trim()).args(["init", "-q"]).current_dir(&fast).status().unwrap().success());
    let git = bin.join("git");
    let case = format!("case \"$*\" in *{}*) exec sleep 600 ;; esac", slow.display());
    let hang = format!("#!/bin/sh\n{case}\nexec {} \"$@\"\n", real.trim());
    fs::write(&git, hang).unwrap();
    fs::set_permissions(&git, fs::Permissions::from_mode(0o755)).unwrap();

    // Two processes that stay alive for the test stand in for the Claude Code sessions' own.
    let alive = || Kill(Command::new("sleep").arg("600").spawn().unwrap());
    let (a, b) = (alive(), alive());
    claude_session(&sessions, a.0.id(), "fastone", &fast);
    claude_session(&sessions, b.0.id(), "slowone", &slow);

    let exe = env!("CARGO_BIN_EXE_reason-network");
    let path = format!("{}:{}", bin.display(), std::env::var("PATH").unwrap());
    let cmd = |args: &[&str]| {
        let mut c = Command::new(exe);
        c.args(args)
            .env("HOME", &home)
            .env("REASON_NETWORK_HOME", root.join("state"))
            .env_remove("CLAUDE_CONFIG_DIR")
            .env("PATH", &path);
        c
    };
    let _daemon = Kill(cmd(&["daemon"]).stdout(Stdio::null()).stderr(Stdio::null()).spawn().unwrap());

    let started = Instant::now();
    let mut who = cmd(&["who"]).stdout(Stdio::piped()).stderr(Stdio::piped()).spawn().unwrap();
    let out = loop {
        if who.try_wait().unwrap().is_some() {
            break who.wait_with_output().unwrap();
        }
        if started.elapsed() > Duration::from_secs(20) {
            let _ = who.kill();
            let _ = who.wait();
            let _ = fs::remove_dir_all(&root);
            panic!("`who` still hadn't answered after 20s with git hanging in one folder");
        }
        std::thread::sleep(Duration::from_millis(50));
    };
    let _ = fs::remove_dir_all(&root);
    let text = String::from_utf8_lossy(&out.stdout);
    assert!(out.status.success(), "who failed: {}{text}", String::from_utf8_lossy(&out.stderr));
    assert!(text.contains("fastone") && text.contains("slowone"), "both sessions listed:\n{text}");
    assert!(text.contains("fast"), "the folder git answered for keeps its repo:\n{text}");
}
