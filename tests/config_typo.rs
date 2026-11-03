//! A config file that doesn't parse stops the daemon, rather than running it on defaults that undo what the person
//! chose (a folder off, mail held).
#![cfg(unix)]

use std::{
    fs,
    process::{Command, Stdio},
    time::{Duration, Instant},
};

#[test]
fn the_daemon_does_not_start_on_a_config_it_cannot_read() {
    let home = std::env::temp_dir().join(format!("rn-config-typo-{}", std::process::id()));
    let state = home.join("state");
    fs::create_dir_all(&state).unwrap();
    fs::write(state.join("config.toml"), "port = \"41641\"\n[folders]\n\"~/personal\" = \"off\"\n").unwrap();
    let mut daemon = Command::new(env!("CARGO_BIN_EXE_reason-network"))
        .arg("daemon")
        .env("HOME", &home)
        .env("REASON_NETWORK_HOME", &state)
        .env("REASON_NETWORK_NO_UPDATE", "1")
        .env("REASON_NETWORK_NO_TRAY", "1")
        .env("REASON_NETWORK_NO_MDNS", "1")
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(10);
    while daemon.try_wait().unwrap().is_none() && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(50));
    }
    let running = daemon.try_wait().unwrap().is_none();
    if running {
        daemon.kill().unwrap();
    }
    let out = daemon.wait_with_output().unwrap();
    let _ = fs::remove_dir_all(&home);
    assert!(!running, "the daemon started anyway");
    assert!(!out.status.success());
    assert!(String::from_utf8_lossy(&out.stderr).contains("config.toml"));
}
