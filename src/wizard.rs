//! The guided setup: what `reason network`, `reason network init`, `reason network join`, `reason network invite` and `reason network setup` do when a person
//! runs them in a terminal. Without a terminal (an agent running them), the commands use defaults and flags.

use crate::{
    AGENT_NAME, CLI,
    client::Link,
    config::{Config, Policy},
    install_lines, invite,
    proto::*,
    render, setup, util,
};
use anyhow::{Result, anyhow};
use cliclack::{confirm, input, intro, log, multiselect, note, outro, outro_cancel, select, spinner};
use std::io::IsTerminal;

pub fn interactive() -> bool {
    std::io::stdin().is_terminal() && std::io::stderr().is_terminal() && crate::paths::env_var_os("NO_PROMPT").is_none()
}

/// Runs a guided flow, turning Ctrl-C at a prompt into a quiet exit.
pub async fn guided(f: impl std::future::Future<Output = Result<()>>) -> Result<()> {
    match f.await {
        Err(e) if e.downcast_ref::<std::io::Error>().is_some_and(|e| e.kind() == std::io::ErrorKind::Interrupted) => {
            let _ = outro_cancel(format!("Stopped. Run `{CLI}` to pick up where you left off."));
            Ok(())
        }
        r => r,
    }
}

fn name_ok(s: &str) -> Result<(), &'static str> {
    if util::slug(s).is_empty() { Err("letters or digits, please") } else { Ok(()) }
}

fn invite_ok(s: &str) -> Result<(), &'static str> {
    if crate::team::Ticket::is_ticket(s) || invite::normalize(s).is_some() {
        Ok(())
    } else {
        Err("paste the team link your friend sent")
    }
}

fn ask_name() -> Result<String> {
    let name: String = input("What should teammates call you?")
        .default_input(&Config::load().person())
        .validate(|s: &String| name_ok(s))
        .interact()?;
    Ok(util::slug(&name))
}

/// Start or join a team. With an invite (a team link, from the installer), it only asks for your name.
/// `start`: `Some(true)` from `reason network init`, `Some(false)` from `reason network join`, `None` to ask.
pub async fn onboard(
    start: Option<bool>,
    team: Option<String>,
    invite: Option<String>,
    person: Option<String>,
) -> Result<()> {
    intro(format!(" {CLI} "))?;
    let link = Link::new(None);
    if let Ok(t) = link.call::<TeamInfo>(Request::Team).await {
        log::info(crate::shown(&format!("You're in team {} as {} on {}.", t.name, t.person, t.machine)))?;
        outro(format!("`{CLI}` shows who's around; `{CLI} invite` gives you the team link."))?;
        return Ok(());
    }
    let mut person = match person {
        Some(p) => util::slug(&p),
        None => ask_name()?,
    };
    let start = match (start, &invite) {
        (Some(s), _) => s,
        (None, Some(_)) => false,
        (None, None) => select("Start a new team, or join one?")
            .item(false, "Join a team", "a friend sent me a link")
            .item(true, "Start a new team", "")
            .interact()?,
    };

    if start {
        let name: String = match team {
            Some(t) => t,
            None => input("Name your team").placeholder("the boys").validate(|s: &String| name_ok(s)).interact()?,
        };
        let sp = spinner();
        sp.start("Creating the team...");
        let t: TeamInfo = link
            .call(Request::CreateTeam { name, person, relay: None, proof: None })
            .await
            .inspect_err(|e| sp.error(crate::shown(&format!("{e:#}"))))?;
        crate::wait_exit(crate::daemon_pid()).await;
        sp.stop(crate::shown(&format!("Created team {}. You are {} on {}.", t.name, t.person, t.machine)));
    } else {
        let mut invite = invite;
        loop {
            let text: String = match invite.take() {
                Some(t) => t,
                None => input("Paste the team link")
                    .placeholder("https://reasonmachines.com/network/join/#…")
                    .validate(|s: &String| invite_ok(s))
                    .interact()?,
            };
            let sp = spinner();
            sp.start("Joining...");
            match link
                .call::<TeamInfo>(Request::Join { ticket: text.clone(), person: person.clone(), proof: None })
                .await
            {
                Ok(t) => {
                    crate::wait_exit(crate::daemon_pid()).await;
                    sp.stop(crate::shown(&format!("Joined team {} as {} on {}.", t.name, t.person, t.machine)));
                    break;
                }
                Err(e) if e.to_string().starts_with("already in team") => {
                    let why = e.to_string();
                    sp.error(crate::shown(&format!("You're {}.", why.split(". To switch").next().unwrap_or(&why))));
                    if !confirm("Leave it and join this team instead?").initial_value(false).interact()? {
                        return Err(anyhow!("not joined; you're still in your old team"));
                    }
                    let pid = crate::daemon_pid();
                    link.call::<String>(Request::Leave { force: false }).await?;
                    crate::wait_exit(pid).await;
                    invite = Some(text);
                }
                Err(e) if e.to_string().contains("already in this team") => {
                    sp.error(format!("Someone in the team is already called {person}."));
                    person =
                        util::slug(&input("Pick another name").validate(|s: &String| name_ok(s)).interact::<String>()?);
                    invite = Some(text);
                }
                Err(e) => {
                    sp.error(crate::shown(&format!("{e:#}")));
                    if !confirm("Try again?").initial_value(true).interact()? {
                        return Err(anyhow!("not joined"));
                    }
                    // Nobody reachable: the same link may work in a moment. Anything else: ask for it again.
                    invite = e.to_string().contains("reach").then_some(text);
                }
            }
        }
    }

    set_up_agents_with_defaults().await?;

    if start {
        let url = show_link(false).await?;
        outro("Done. Ask any of your agents: \"who's on the team?\"")?;
        // Outside the frame, so the terminal copies it whole.
        println!("\n{url}\n");
    } else {
        if let Ok(t) = Link::new(None).call::<TeamInfo>(Request::Team).await {
            note("Your team", crate::shown(render::team(&t).trim_end()))?;
        }
        outro("You're in. Ask any of your agents: \"who's on the team?\"")?;
    }
    Ok(())
}

/// Every agent found, the background service, the default preferences; then says what it did.
async fn set_up_agents_with_defaults() -> Result<()> {
    let exe = setup::exe()?;
    let found = setup::detect(&exe);
    let ids: Vec<&str> = found.iter().map(|f| f.id).collect();
    let sp = spinner();
    sp.start("Setting up your agents...");
    crate::stop_daemon().await;
    let out = setup::apply(&exe, &ids, &[], Some(true), false).await;
    sp.stop(match out.agents.as_slice() {
        [] => "No coding agents found on this machine (yet)".to_string(),
        names => format!("Set up {}", and_list(names)),
    });
    if out.service == Some(true) {
        log::success("Runs in the background, from login")?;
    }
    for n in &out.notes {
        log::warning(n)?;
    }
    Ok(())
}

/// `a, b and c`.
fn and_list(items: &[&str]) -> String {
    match items {
        [] => String::new(),
        [one] => one.to_string(),
        [rest @ .., last] => format!("{} and {last}", rest.join(", ")),
    }
}

/// Which agents are on Reason Network, the background service, and how teammates' messages are handled.
pub async fn agents_and_preferences() -> Result<()> {
    let exe = setup::exe()?;
    let found = setup::detect(&exe);
    let chosen: Vec<&str> = if found.is_empty() {
        log::warning(format!(
            "No coding agents found here (Claude Code, Codex, Grok CLI, Cursor, ZCode Desktop, Gemini CLI, Antigravity CLI, Copilot CLI, opencode, pi, DeepSeek Harness, Reason). Install one, then run `{CLI} setup`."
        ))?;
        Vec::new()
    } else {
        let initial: Vec<&str> = found.iter().filter(|f| f.wired).map(|f| f.id).collect();
        let mut pick = multiselect("Which agents should be on Reason Network?  (space to toggle, enter to confirm)")
            .required(false)
            .initial_values(initial);
        for f in &found {
            pick = pick.item(f.id, f.name, f.hint);
        }
        pick.interact()?
    };
    let service = confirm("Keep Reason Network running in the background, so teammates can always reach you?")
        .initial_value(true)
        .interact()?;
    let mut cfg = Config::load_strict().map_err(|e| anyhow!("fix {} first: {e}", crate::paths::config().display()))?;
    cfg.inbound.default = select("When a teammate's agent messages one of yours")
        .item(Policy::Accept, "Deliver it", "recommended")
        .item(Policy::Hold, "Hold it until I approve", format!("you approve with `{CLI} held`"))
        .initial_value(cfg.inbound.default)
        .interact()?;
    cfg.share_paths = confirm("Show teammates your full working directories? (otherwise just the folder name)")
        .initial_value(cfg.share_paths)
        .interact()?;
    if cfg!(target_os = "macos") && service {
        cfg.tray = confirm("Show Reason in the menu bar?").initial_value(cfg.tray).interact()?;
    }
    cfg.save()?;

    let remove: Vec<&str> = found.iter().filter(|f| f.wired && !chosen.contains(&f.id)).map(|f| f.id).collect();
    let sp = spinner();
    sp.start("Setting up...");
    // The daemon reads its config at start: restart it (the service manager or the next command brings it back).
    crate::stop_daemon().await;
    let service = if service { Some(true) } else { setup::service_installed().then_some(false) };
    let out = setup::apply(&exe, &chosen, &remove, service, false).await;
    sp.stop("Done");
    for l in &out.lines {
        log::success(l)?;
    }
    for n in &out.notes {
        log::warning(n)?;
    }
    Ok(())
}

/// Explains the team link and puts it on the clipboard; returns it for the caller to print after the frame
/// (inside it, the terminal would wrap it with borders). `new` replaces it first.
pub async fn show_link(new: bool) -> Result<String> {
    let l: LinkInfo = Link::new(None).call(Request::Link { new }).await?;
    let copied = copy(&l.url);
    let team = crate::shown(&l.team);
    let title =
        if copied { format!("Team link for {team} (on your clipboard)") } else { format!("Team link for {team}") };
    note(
        title,
        format!(
            "Send it as the invite line, Join the Reason Network: <link>: whoever pastes it to their agent joins.\nIt works for everyone until `{CLI} link --new`."
        ),
    )?;
    Ok(l.url)
}

/// A one-time code (for reading out loud, or for another machine of yours).
pub async fn show_code(me: bool) -> Result<()> {
    let i: InviteInfo = Link::new(None).call(Request::Invite { me, hours: 24 }).await?;
    let [unix, windows] = install_lines(&i.code);
    let who = if me { "your other machine" } else { "your teammate" };
    note(
        format!("Invite code: {}", i.code),
        format!(
            "On {who}: {unix}\n(on Windows: {windows})\n(or `{AGENT_NAME} join {}` if it's there already)\nWorks once, within {}h, while this machine is online.",
            i.code, i.hours
        ),
    )?;
    Ok(())
}

/// Puts text on the clipboard, if there's a clipboard tool. True if it worked.
pub fn copy(text: &str) -> bool {
    use std::io::Write;
    let tools: &[(&str, &[&str])] = if cfg!(windows) {
        &[("clip", &[])]
    } else {
        &[("pbcopy", &[]), ("wl-copy", &[]), ("xclip", &["-selection", "clipboard"])]
    };
    tools.iter().any(|(bin, args)| {
        let Ok(mut child) = std::process::Command::new(bin)
            .args(*args)
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
        else {
            return false;
        };
        let wrote = child.stdin.take().is_some_and(|mut s| s.write_all(text.as_bytes()).is_ok());
        child.wait().is_ok_and(|st| st.success()) && wrote
    })
}

/// `reason network invite` in a terminal: the team link, or a one-time code for another machine of yours.
pub async fn invite() -> Result<()> {
    intro(format!(" {CLI} invite "))?;
    let what = select("Who's joining?")
        .item(0, "Friends", "the team link, works for everyone")
        .item(1, "Another machine of mine", "a one-time code")
        .interact()?;
    if what == 0 {
        let url = show_link(false).await?;
        outro(format!("They'll show up in `{CLI}` once they've joined."))?;
        println!("\n{url}\n");
    } else {
        show_code(true).await?;
        outro(format!("It'll show up in `{CLI}` once it has joined."))?;
    }
    Ok(())
}

/// `reason network setup` in a terminal.
pub async fn setup_agents() -> Result<()> {
    intro(format!(" {CLI} setup "))?;
    agents_and_preferences().await?;
    outro("Done.")?;
    Ok(())
}
