//! `reason network tray`: the menu bar icon (macOS). The background service starts it and it exits with the daemon. It
//! shows who's around and what their sessions are doing, and the messages waiting for you.

#[cfg(not(target_os = "macos"))]
pub fn run() -> anyhow::Result<()> {
    anyhow::bail!("the menu bar icon is macOS-only for now")
}

#[cfg(target_os = "macos")]
pub use mac::run;

#[cfg(target_os = "macos")]
mod mac {
    use crate::{CLI, client::Link, config::Config, proto::*, util, wizard};
    use anyhow::{Result, anyhow};
    use objc2::{MainThreadMarker, rc::autoreleasepool};
    use objc2_app_kit::{NSApplication, NSApplicationActivationPolicy, NSCellImagePosition, NSEventMask, NSImage};
    use objc2_foundation::{NSDate, NSDefaultRunLoopMode, NSString};
    use std::{
        sync::mpsc,
        time::{Duration, Instant},
    };
    use tokio::sync::mpsc as amsc;
    use tray_icon::{
        TrayIcon, TrayIconBuilder,
        menu::{IsMenuItem, Menu, MenuEvent, MenuItem, PredefinedMenuItem, Submenu},
    };

    /// What the daemon says, every couple of seconds.
    struct View {
        /// Everything shown, to rebuild the menu only when something changed.
        key: String,
        team: Option<String>,
        me: String,
        machines: Vec<(MachineInfo, Vec<SessionInfo>, usize)>,
    }

    enum Update {
        View(Box<View>),
        Flash(&'static str),
        /// The daemon is gone for good.
        Gone,
    }

    enum Action {
        Copy(String),
        CopyLink,
    }

    pub fn run() -> Result<()> {
        let mtm = MainThreadMarker::new().ok_or_else(|| anyhow!("the menu bar icon needs the main thread"))?;
        let (app, tray) = autoreleasepool(|_| -> Result<_> {
            let app = NSApplication::sharedApplication(mtm);
            // A menu bar item only: no Dock icon, no app menu.
            app.setActivationPolicy(NSApplicationActivationPolicy::Accessory);
            app.finishLaunching();

            let tray = TrayIconBuilder::new()
                .with_tooltip("Reason")
                .with_autosave_name("com.reasonmachines.network.tray")
                .with_menu(Box::new(Menu::new()))
                .build()?;
            set_icon(&tray, false, None, mtm);
            Ok((app, tray))
        })?;

        let (tx, updates) = mpsc::channel();
        let (act, actions) = amsc::unbounded_channel();
        tokio::spawn(poll(tx, actions));

        let parent = util::parent_pid();
        let mut shown: Option<Box<View>> = None;
        let mut flash: Option<(&str, Instant)> = None;
        loop {
            // Our manual event loop must drain AppKit's temporary images and menus each time around.
            autoreleasepool(|_| {
                let until = NSDate::dateWithTimeIntervalSinceNow(0.5);
                // SAFETY: the main thread's event loop, with AppKit's own run loop mode.
                let ev = unsafe {
                    app.nextEventMatchingMask_untilDate_inMode_dequeue(
                        NSEventMask::Any,
                        Some(&until),
                        NSDefaultRunLoopMode,
                        true,
                    )
                };
                if let Some(ev) = ev {
                    app.sendEvent(&ev);
                }
                while let Ok(ev) = MenuEvent::receiver().try_recv() {
                    match ev.id.as_ref() {
                        "link" => {
                            let _ = act.send(Action::CopyLink);
                        }
                        "hide" => hide(),
                        id => {
                            if let Some(addr) = id.strip_prefix("copy:") {
                                let _ = act.send(Action::Copy(addr.to_string()));
                            }
                        }
                    }
                }
                let mut changed = false;
                while let Ok(u) = updates.try_recv() {
                    match u {
                        Update::View(v) if shown.as_ref().is_none_or(|s| s.key != v.key) => {
                            tray.set_menu(Some(Box::new(menu(&v))));
                            shown = Some(v);
                            changed = true;
                        }
                        Update::View(_) => {}
                        Update::Flash(text) => {
                            flash = Some((text, Instant::now()));
                            changed = true;
                        }
                        Update::Gone => std::process::exit(0),
                    }
                }
                if flash.is_some_and(|(_, at)| at.elapsed() > Duration::from_secs(2)) {
                    flash = None;
                    changed = true;
                }
                if changed {
                    let v = shown.as_deref();
                    let online = v.is_some_and(|v| v.machines.iter().any(|(m, ..)| m.online && !m.is_me));
                    let title = flash.map(|(t, _)| t.to_string());
                    set_icon(&tray, online, title.as_deref(), mtm);
                }
                // Our daemon exited (we were handed to launchd): go with it.
                if util::parent_pid() != parent {
                    std::process::exit(0);
                }
            });
        }
    }

    /// Two speech bubbles, filled while a teammate is connected, plus an optional title (a brief notice).
    fn set_icon(tray: &TrayIcon, online: bool, title: Option<&str>, mtm: MainThreadMarker) {
        let name = if online { "bubble.left.and.bubble.right.fill" } else { "bubble.left.and.bubble.right" };
        if let Some(button) = tray.ns_status_item().and_then(|i| i.button(mtm)) {
            let image = NSImage::imageWithSystemSymbolName_accessibilityDescription(
                &NSString::from_str(name),
                Some(&NSString::from_str("Reason")),
            );
            button.setImage(image.as_deref());
            button.setImagePosition(NSCellImagePosition::ImageLeft);
        }
        // Also resizes the clickable area to fit the new image and title.
        tray.set_title(title.map(|t| format!(" {t}")));
    }

    fn menu(v: &View) -> Menu {
        let menu = Menu::new();
        let info = |text: &str| MenuItem::new(text, false, None);
        let sep = PredefinedMenuItem::separator;
        let mut items: Vec<Box<dyn IsMenuItem>> = Vec::new();
        match &v.team {
            Some(t) => items.push(Box::new(info(&format!("{t} · you're {}", v.me)))),
            None => {
                items.push(Box::new(info("Not in a team yet")));
                items.push(Box::new(info(&format!("Run `{CLI}` in a terminal to set up"))));
            }
        }
        items.push(Box::new(sep()));
        for (m, sessions, without) in &v.machines {
            let dot = if m.online || m.is_me { "●" } else { "○" };
            let label = format!("{dot} {} · {} · {}", m.person, m.machine, how(m));
            if sessions.is_empty() && *without == 0 {
                // Enabled (it does nothing): a greyed-out line would read as offline.
                items.push(Box::new(MenuItem::with_id("none", label, true, None)));
                continue;
            }
            let sub = Submenu::new(label, true);
            for s in sessions {
                let place = match (&s.repo, &s.branch) {
                    (Some(r), Some(b)) => format!("{r}@{b}"),
                    (Some(r), None) => r.clone(),
                    _ => s.cwd.clone(),
                };
                let _ = sub.append(&MenuItem::with_id(
                    format!("copy:{}", s.address),
                    format!("{} · {} · {} · {place}", s.name, s.agent, s.status),
                    true,
                    None,
                ));
                if let Some(doing) = &s.doing {
                    let doing: String = doing.chars().take(70).collect();
                    let _ = sub.append(&info(&format!("    {doing}")));
                }
            }
            if *without > 0 {
                let _ = sub.append(&info(&format!("+ {without} without Reason Network")));
            }
            let _ = sub.append(&sep());
            let _ = sub.append(&info("Click a session to copy its address"));
            items.push(Box::new(sub));
        }
        items.push(Box::new(sep()));
        if v.team.is_some() {
            items.push(Box::new(MenuItem::with_id("link", "Copy team link", true, None)));
        }
        items.push(Box::new(MenuItem::with_id("hide", "Hide menu bar icon", true, None)));
        let refs: Vec<&dyn IsMenuItem> = items.iter().map(|i| i.as_ref()).collect();
        let _ = menu.append_items(&refs);
        menu
    }

    fn how(m: &MachineInfo) -> String {
        if m.is_me {
            return "this Mac".into();
        }
        match (m.online, &m.link, m.rtt_ms, m.last_seen) {
            (true, Some(l), Some(r), _) => format!("{l} {r}ms"),
            (true, ..) => "connected".into(),
            (false, _, _, Some(t)) => format!("offline {}", util::ago(t, util::now_ms())),
            _ => "not seen yet".into(),
        }
    }

    /// Hidden for good: `reason network setup` brings it back.
    fn hide() -> ! {
        if let Ok(mut cfg) = Config::load_strict() {
            cfg.tray = false;
            let _ = cfg.save();
        }
        std::process::exit(0)
    }

    async fn poll(tx: mpsc::Sender<Update>, mut actions: amsc::UnboundedReceiver<Action>) {
        let link = Link::existing();
        let mut tick = tokio::time::interval(Duration::from_secs(2));
        let mut fails = 0;
        loop {
            let update = tokio::select! {
                _ = tick.tick() => match view(&link).await {
                    Ok(v) => {
                        fails = 0;
                        Update::View(Box::new(v))
                    }
                    Err(_) if fails >= 15 => Update::Gone,
                    Err(_) => {
                        fails += 1;
                        continue;
                    }
                },
                Some(a) = actions.recv() => match a {
                    Action::Copy(text) => Update::Flash(if wizard::copy(&text) { "copied" } else { "couldn't copy" }),
                    Action::CopyLink => match link.call::<LinkInfo>(Request::Link { new: false }).await {
                        Ok(l) if wizard::copy(&l.url) => Update::Flash("team link copied"),
                        _ => Update::Flash("couldn't copy"),
                    },
                },
            };
            if tx.send(update).is_err() {
                return;
            }
        }
    }

    async fn view(link: &std::sync::Arc<Link>) -> Result<View> {
        let d: Directory = link.call(Request::Who { me: None, filter: None, all: false }).await?;
        let machines = d
            .machines
            .iter()
            .map(|m| {
                let mine = |s: &&SessionInfo| s.person == m.person && s.machine == m.machine;
                let on: Vec<SessionInfo> =
                    d.sessions.iter().filter(mine).filter(|s| s.tier != Tier::None).cloned().collect();
                let without = d.sessions.iter().filter(mine).filter(|s| s.tier == Tier::None).count();
                (m.clone(), on, without)
            })
            .collect();
        let me = format!("{} on {}", d.person, d.machine);
        let key = serde_json::to_string(&(&d.team, &me, &machines))?;
        Ok(View { key, team: d.team, me, machines })
    }
}
