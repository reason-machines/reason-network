//! Desktop notifications, for a message that needs the person and not only their agent (`notify`). Shown with
//! what the system has: osascript on macOS, notify-send on Linux, a toast from Windows PowerShell. Where there is
//! nothing to show one with, the sender is told; people turn them off in their system's notification settings.

use super::{Daemon, log};
use crate::{agent, proto::Message, util};
use std::{
    process::{Command, Stdio},
    time::{Duration, Instant},
};

/// At most one notification per person sending in this time: a looping agent can't flood the screen.
const GAP: Duration = Duration::from_secs(60);
/// Older than this (it waited in an outbox while this machine was off), it no longer pops up.
const STALE_MS: i64 = 15 * 60 * 1000;

impl Daemon {
    /// Shows `msg` to this machine's person. Returns whether it was handed to the system's notifications (which may
    /// still hide it, say if the person turned them off), and a note for the sender.
    pub(super) fn notify(&self, msg: &Message) -> (bool, String) {
        if util::now_ms() - msg.sent_at > STALE_MS {
            return (false, "not popped up: it's over 15 minutes old".into());
        }
        let person = msg.from.split('/').next().unwrap_or_default().to_string();
        let title = match msg.to.split_once('/') {
            Some((_, session)) => format!("{} → {session}", msg.from),
            None => msg.from.clone(),
        };
        let mut shown = self.shown.lock().unwrap();
        shown.retain(|_, at| at.elapsed() < GAP);
        if shown.contains_key(&person) {
            return (false, format!("not popped up: {} had one from {person} a moment ago", self.machine));
        }
        let Some(mut cmd) = command(&title, &util::one_line(&msg.body, 200)) else {
            return (false, format!("not popped up: {} has no desktop to show it on", self.machine));
        };
        shown.insert(person, Instant::now());
        std::thread::spawn(move || {
            match cmd.stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::piped()).output() {
                Ok(o) if o.status.success() => {}
                Ok(o) => log(&format!("notification failed: {}", String::from_utf8_lossy(&o.stderr).trim())),
                Err(e) => log(&format!("notification failed: {e}")),
            }
        });
        (true, format!("popped up for {} ({})", self.person, self.machine))
    }
}

#[cfg(target_os = "macos")]
fn command(title: &str, body: &str) -> Option<Command> {
    let mut cmd = crate::util::no_window(Command::new(agent::find_bin("osascript")?));
    let show = r#"display notification (item 2 of argv) with title (item 1 of argv) sound name "Glass""#;
    cmd.args(["-e", "on run argv", "-e", show, "-e", "end run", title, body]);
    Some(cmd)
}

#[cfg(all(unix, not(target_os = "macos")))]
fn command(title: &str, body: &str) -> Option<Command> {
    let mut cmd = notify_send()?;
    // `--` keeps a body starting with `-` from being taken for an option.
    cmd.args(["--app-name=Reason Network".into(), "--".into(), markup(title), markup(body)]);
    Some(cmd)
}

/// A daemon started outside the desktop session may lack the session bus's address; without a bus, no desktop.
#[cfg(all(unix, not(target_os = "macos")))]
pub(super) fn notify_send() -> Option<Command> {
    notify_send_at(agent::find_bin("notify-send")?)
}

#[cfg(all(unix, not(target_os = "macos")))]
pub(super) fn notify_send_at(bin: std::path::PathBuf) -> Option<Command> {
    let mut cmd = crate::util::no_window(Command::new(bin));
    let bus = match std::env::var_os("DBUS_SESSION_BUS_ADDRESS") {
        Some(bus) => bus,
        None => {
            let bus = format!("/run/user/{}/bus", unsafe { libc::getuid() });
            if !std::path::Path::new(&bus).exists() {
                return None;
            }
            format!("unix:path={bus}").into()
        }
    };
    // Linger keeps a user bus up with nobody at the screen (a server, a box reached over SSH): only a notification
    // server on it makes a desktop, running or one the bus starts on demand.
    if !notifications_on(&bus) {
        return None;
    }
    cmd.env("DBUS_SESSION_BUS_ADDRESS", bus);
    Some(cmd)
}

/// Whether the bus at `bus` has a notification server: one running, or one it starts on demand (dunst, mako, MATE's).
/// Only a clear no counts: without busctl, or on a bus it can't reach, notify-send finds out. It's asked under the
/// daemon's lock, so a bus that hangs gets a second in all (`--timeout` doesn't cover connecting).
#[cfg(all(unix, not(target_os = "macos")))]
fn notifications_on(bus: &std::ffi::OsStr) -> bool {
    const NAME: &str = "org.freedesktop.Notifications";
    let Some(busctl) = super::ask::asker_bin("busctl", &["/usr/bin/busctl", "/run/current-system/sw/bin/busctl"])
    else {
        return true;
    };
    let call = |args: &[&str]| {
        crate::util::no_window(Command::new(&busctl))
            .env("DBUS_SESSION_BUS_ADDRESS", bus)
            .args(["--user", "--timeout=1", "call", "org.freedesktop.DBus", "/org/freedesktop/DBus"])
            .arg("org.freedesktop.DBus")
            .args(args)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .ok()
    };
    let calls = [call(&["NameHasOwner", "s", NAME]), call(&["ListActivatableNames"])];
    let until = Instant::now() + Duration::from_secs(1);
    let [owned, activatable] = calls.map(|child| {
        let mut child = child?;
        while matches!(child.try_wait(), Ok(None)) && Instant::now() < until {
            std::thread::sleep(Duration::from_millis(2));
        }
        let _ = child.kill();
        child.wait_with_output().ok().filter(|o| o.status.success()).map(|o| o.stdout)
    });
    // `as 2 "org.freedesktop.DBus" "org.freedesktop.Notifications"`
    let quoted = format!("\"{NAME}\"");
    let unowned = owned.is_some_and(|o| o.trim_ascii() == b"b false");
    let unlisted = activatable.is_some_and(|o| !o.windows(quoted.len()).any(|w| w == quoted.as_bytes()));
    !(unowned && unlisted)
}

/// Most notification servers read markup.
#[cfg(all(unix, not(target_os = "macos")))]
pub(super) fn markup(s: &str) -> String {
    s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;")
}

#[cfg(windows)]
fn command(title: &str, body: &str) -> Option<Command> {
    const TOAST: &str = "$ErrorActionPreference = 'Stop'
$n = [Windows.UI.Notifications.ToastNotificationManager, Windows.UI.Notifications, ContentType = WindowsRuntime]
$x = $n::GetTemplateContent([Windows.UI.Notifications.ToastTemplateType]::ToastText02)
$t = $x.GetElementsByTagName('text')
$t.Item(0).AppendChild($x.CreateTextNode($env:TINCAN_TITLE)) > $null
$t.Item(1).AppendChild($x.CreateTextNode($env:TINCAN_BODY)) > $null
$app = '{1AC14E77-02E7-4E5D-B744-2EB1AE5198B7}\\WindowsPowerShell\\v1.0\\powershell.exe'
$n::CreateToastNotifier($app).Show([Windows.UI.Notifications.ToastNotification]::new($x))";
    let mut cmd = powershell(TOAST)?;
    cmd.env("TINCAN_TITLE", title).env("TINCAN_BODY", body);
    Some(cmd)
}

#[cfg(windows)]
pub(super) fn powershell(script: &str) -> Option<Command> {
    Some(powershell_at(agent::find_bin("powershell")?, script))
}

#[cfg(windows)]
pub(super) fn powershell_at(bin: std::path::PathBuf, script: &str) -> Command {
    // No console flashing up next to the notification.
    let mut cmd = crate::util::no_window(Command::new(bin));
    cmd.args(["-NoProfile", "-NonInteractive", "-Command", script]);
    cmd
}

#[cfg(all(test, unix, not(target_os = "macos")))]
mod tests {
    use super::*;
    use std::io::BufRead;

    /// A private session bus whose only service directory is `services`, so what the host has installed doesn't count.
    fn bus(services: &[&str]) -> Option<(std::process::Child, String, std::path::PathBuf)> {
        let daemon = agent::find_bin("dbus-daemon").filter(|_| agent::find_bin("busctl").is_some())?;
        let dir = std::env::temp_dir().join(format!("reason-network-bus-{}", ulid::Ulid::generate()));
        std::fs::create_dir_all(dir.join("services")).unwrap();
        for name in services {
            let service = format!("[D-BUS Service]\nName={name}\nExec=/nonexistent\n");
            std::fs::write(dir.join("services").join(format!("{name}.service")), service).unwrap();
        }
        let config = format!(
            r#"<!DOCTYPE busconfig PUBLIC "-//freedesktop//DTD D-Bus Bus Configuration 1.0//EN"
 "http://www.freedesktop.org/standards/dbus/1.0/busconfig.dtd">
<busconfig>
  <type>session</type>
  <listen>unix:tmpdir={tmp}</listen>
  <servicedir>{tmp}/services</servicedir>
  <policy context="default"><allow send_destination="*" eavesdrop="true"/><allow eavesdrop="true"/><allow own="*"/></policy>
</busconfig>"#,
            tmp = dir.display()
        );
        std::fs::write(dir.join("bus.conf"), config).unwrap();
        let mut bus = Command::new(daemon)
            .arg(format!("--config-file={}", dir.join("bus.conf").display()))
            .args(["--nofork", "--print-address"])
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        let mut address = String::new();
        std::io::BufReader::new(bus.stdout.take().unwrap()).read_line(&mut address).unwrap();
        Some((bus, address.trim().to_string(), dir))
    }

    fn notifications_on_bus(services: &[&str]) -> Option<bool> {
        let (mut bus, address, dir) = bus(services)?;
        let on = notifications_on(address.as_ref());
        bus.kill().unwrap();
        bus.wait().unwrap();
        std::fs::remove_dir_all(dir).unwrap();
        Some(on)
    }

    #[test]
    fn a_bus_without_a_notification_server_is_no_desktop() {
        let Some(on) = notifications_on_bus(&["org.example.Other"]) else { return };
        assert!(!on);
        // A bus it can't reach is left to notify-send.
        assert!(notifications_on("unix:path=/nonexistent".as_ref()));
    }

    #[test]
    fn a_notification_server_the_bus_starts_on_demand_is_a_desktop() {
        let Some(on) = notifications_on_bus(&["org.freedesktop.Notifications"]) else { return };
        assert!(on);
    }
}
