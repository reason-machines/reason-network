//! `ask_user`: an agent asks its own person, with a pop-up on this machine, and gets the answer back as the tool's
//! result: their own decision, not a message from anyone. A panel with a note on macOS and Windows, a notification
//! with buttons on Linux.

use super::{Daemon, MAX_WAIT, log, mail::unregistered};
use crate::{
    proto::{Approval, HookCall, Message},
    team::Signed,
    util,
};
use anyhow::{Result, anyhow, bail};
use std::{process::Command, sync::Arc, time::Duration};
use ulid::Ulid;

/// A pop-up nobody answers closes after this.
const OPEN_SECS: u64 = 3600;

/// What came of a question.
#[derive(Clone)]
pub(super) enum Answer {
    Approved(String),
    Denied(String),
    /// Closed, or not answered in time.
    Unanswered,
    /// It couldn't be shown.
    Failed(String),
}

/// A session's question while it is up.
pub(super) struct Asking {
    question: String,
    /// Calls waiting for its answer now.
    waiting: usize,
    /// The answer, and what to add to it for the asking agent (whether a signed answer went to a teammate).
    answer: Option<(Answer, String)>,
    /// Whether a waiting call has returned the answer.
    taken: bool,
}

impl Daemon {
    /// Asks this machine's person on behalf of the session `key`, and waits for the answer. A session has one
    /// question up at a time: asking it again waits for it, asking another is refused until it is answered. An answer
    /// that comes once no call waits for it reaches the session as a message from its person. `about`, a teammate's
    /// message to this session, is shown with the question, and its sender gets the answer signed by this machine.
    pub(super) async fn ask(self: &Arc<Self>, key: String, question: String, about: Option<String>) -> Result<String> {
        let (asker, stop, request) = {
            let g = self.inner.lock().unwrap();
            let l = g.live.get(&key).ok_or_else(unregistered)?;
            let asker = self.addr(&l.name);
            let request = match about {
                Some(id) => Some(
                    g.store
                        .message(&id)?
                        .filter(|m| m.to == asker && !m.from.starts_with(&format!("{}/", self.person)))
                        .ok_or_else(|| anyhow!("no message {id} from a teammate to this session"))?,
                ),
                None => None,
            };
            (asker, l.stop_gen, request)
        };
        // A teammate's request comes first, in their words (cut only where it ends in "…"), and the agent's question gets
        // the room left: the request the person sees is the one the signed answer says they saw.
        let request = request.map(|m| {
            let text = clip(&plain(&util::one_line(&m.body, 2000)), 500);
            (m, text)
        });
        let question = clip(&plain(&question), if request.is_some() { 400 } else { 900 });
        let asked = request.as_ref().map_or(String::new(), |(m, text)| format!("{} asked: {text}", m.from));
        let popup = if asked.is_empty() { question.clone() } else { format!("{asked}\n\nYour agent asks: {question}") };
        {
            let mut asks = self.asks.lock().unwrap();
            match asks.get_mut(&key) {
                Some(up) if up.question != popup => bail!(
                    "your question \"{}\" is still up for {}: ask it again to keep waiting for its answer",
                    util::one_line(&up.question, 200),
                    self.person
                ),
                Some(up) => up.waiting += 1,
                None => {
                    let cmd = dialog(&format!("{asker} asks"), &asked, &question).ok_or_else(|| {
                        anyhow!(
                            "{} has no screen to ask {} on: ask them in your own session",
                            self.machine,
                            self.person
                        )
                    })?;
                    let up = Asking { question: popup, waiting: 1, answer: None, taken: false };
                    asks.insert(key.clone(), up);
                    tokio::spawn(self.clone().pop_up(key.clone(), question, request, cmd));
                }
            }
        }
        let waiting = Waiting { daemon: self, key: &key };
        let deadline = tokio::time::Instant::now() + Duration::from_secs(MAX_WAIT);
        loop {
            // The answer, and the client giving up on the call (`StopWaiting`), both wake `arrived`. Giving up is per
            // session: its other waits end too, and their answer comes as a message.
            let woke = self.arrived.notified();
            tokio::pin!(woke);
            woke.as_mut().enable();
            if let Some((answer, receipt)) = waiting.take() {
                return Ok(format!("{}{receipt}", self.said(&answer)?));
            }
            let gave_up = self.inner.lock().unwrap().live.get(&key).is_none_or(|l| l.stop_gen != stop);
            if gave_up || tokio::time::Instant::now() >= deadline {
                return Ok(format!(
                    "{} hasn't answered yet; the pop-up stays up. Ask again to keep waiting: an answer after that \
                     comes to you as a message from them.",
                    self.person
                ));
            }
            let _ = tokio::time::timeout_at(deadline, woke).await;
        }
    }

    /// The answer, as the asking agent reads it.
    fn said(&self, answer: &Answer) -> Result<String> {
        let note = |n: &str| if n.is_empty() { String::new() } else { format!(" Their note: {n}") };
        Ok(match answer {
            Answer::Approved(n) => format!("{} approved.{}", self.person, note(n)),
            Answer::Denied(n) => format!("{} said no.{}", self.person, note(n)),
            Answer::Unanswered => format!("{} didn't answer.", self.person),
            Answer::Failed(e) => {
                bail!("couldn't ask {} with a pop-up ({e}): ask them in your own session", self.person)
            }
        })
    }

    /// Shows the question until it is answered or closes, then hands the answer to the calls waiting for it (and, for
    /// a teammate's request, to its sender).
    async fn pop_up(self: Arc<Self>, key: String, question: String, request: Option<(Message, String)>, cmd: Command) {
        let (answer, shown) = match tokio::time::timeout(Duration::from_secs(OPEN_SECS + 60), show(cmd)).await {
            Ok(Ok(out)) => parse(&out),
            Ok(Err(e)) => (Answer::Failed(e), None),
            Err(_) => (Answer::Unanswered, None),
        };
        if let Answer::Failed(e) = &answer {
            log(&format!("pop-up failed: {e}"));
        }
        // The teammate's answer goes first, so the calls waiting can say whether it went.
        let receipt = match &request {
            Some((m, text)) => self.send_approval(&key, m, text, shown, &question, &answer).await,
            None => String::new(),
        };
        let waited = {
            let mut asks = self.asks.lock().unwrap();
            let Some(up) = asks.get_mut(&key) else { return };
            let waited = up.waiting > 0;
            if waited {
                up.answer = Some((answer.clone(), receipt));
            } else {
                asks.remove(&key);
            }
            waited
        };
        if waited {
            self.arrived.notify_waiters();
        } else {
            self.answer_later(&key, &question, &answer);
        }
    }

    /// The person's answer to a teammate's request, with what they saw, signed by this machine, to the session that sent
    /// the request: from the asking session, or this person's terminal if that has gone. Says whether it went.
    async fn send_approval(
        self: &Arc<Self>,
        key: &str,
        request: &Message,
        text: &str,
        shown: Option<(usize, usize)>,
        question: &str,
        answer: &Answer,
    ) -> String {
        let (approved, note) = match answer {
            Answer::Approved(n) => (true, n),
            Answer::Denied(n) => (false, n),
            _ => return String::new(),
        };
        let (Some(team), Ok(said)) = (self.team_state(), self.said(answer)) else { return String::new() };
        // As much as the person was shown: a panel says how much of the request (after the "<from> asked: " it starts
        // with) and of the question; a notification shows what it shows.
        let (seen, question) = match shown {
            Some((r, q)) => {
                (clip(text, r.saturating_sub(format!("{} asked: ", request.from).chars().count())), clip(question, q))
            }
            None => (text.to_string(), question.to_string()),
        };
        let approval = Approval {
            about: request.id.clone(),
            asker: request.from.clone(),
            approver: self.person.clone(),
            approved,
            question,
            request: seen,
            note: clip(note, 500),
            at: util::now_ms(),
        };
        let signed = Box::new(Signed::new(approval, &self.key, team.name(), "approval"));
        let live = self.inner.lock().unwrap().live.contains_key(key);
        let from = if live { key.to_string() } else { self.terminal().await };
        match self
            .send(from, &request.from, &said, Some(request.id.clone()), true, false, Some(signed), Vec::new())
            .await
        {
            Ok(_) => format!(" (Sent to {}, signed.)", request.from),
            Err(e) => {
                log(&format!("couldn't send {}'s answer to {}: {e:#}", self.person, request.from));
                format!(" (Couldn't send it to {}: {e:#}.)", request.from)
            }
        }
    }

    /// This person's own session (`<person>/terminal`), registered the way the CLI registers it.
    async fn terminal(self: &Arc<Self>) -> String {
        let call = HookCall {
            agent: "cli".into(),
            event: "stop".into(),
            session_id: "terminal".into(),
            cwd: None,
            pid: None,
            mail: false,
            transcript: None,
            doing: None,
            name: Some("terminal".into()),
            dirs: Vec::new(),
        };
        if let Err(e) = self.hook(call).await {
            log(&format!("couldn't register {}/terminal: {e:#}", self.person));
        }
        "cli:terminal".into()
    }

    /// An answer no call waited for any more: to the session, as a message from its person.
    fn answer_later(self: &Arc<Self>, key: &str, question: &str, answer: &Answer) {
        if !matches!(answer, Answer::Approved(_) | Answer::Denied(_)) {
            return;
        }
        let Ok(said) = self.said(answer) else { return };
        let mut g = self.inner.lock().unwrap();
        let Some(to) = g.live.get(key).map(|l| self.addr(&l.name)) else {
            return log(&format!("{key} had gone before {} answered its question", self.person));
        };
        let mut msg = Message {
            id: Ulid::generate().to_string(),
            from: format!("{}/terminal", self.person),
            from_agent: None,
            from_repo: None,
            to,
            via: None,
            reply_to: None,
            body: format!("Your question \"{}\" (ask_user): {said}", util::one_line(question, 200)),
            wake: true,
            notify: false,
            sent_at: util::now_ms(),
            approval: None,
            files: Vec::new(),
        };
        if let Err(e) = g.store.insert(&mut msg, false) {
            return log(&format!("couldn't keep a late answer: {e}"));
        }
        self.deliver(&mut g, key, vec![msg], true);
        drop(g);
        self.arrived.notify_waiters();
    }
}

/// A call waiting for its session's question, counted for as long as it waits: when the last one leaves without the
/// answer, the answer goes to the session as a message instead.
struct Waiting<'a> {
    daemon: &'a Arc<Daemon>,
    key: &'a str,
}

impl Waiting<'_> {
    fn take(&self) -> Option<(Answer, String)> {
        let mut asks = self.daemon.asks.lock().unwrap();
        let up = asks.get_mut(self.key)?;
        up.taken |= up.answer.is_some();
        up.answer.clone()
    }
}

impl Drop for Waiting<'_> {
    fn drop(&mut self) {
        let mut asks = self.daemon.asks.lock().unwrap();
        let Some(up) = asks.get_mut(self.key) else { return };
        up.waiting -= 1;
        if up.waiting > 0 || up.answer.is_none() {
            return;
        }
        let up = asks.remove(self.key).expect("just looked up");
        drop(asks);
        if !up.taken
            && let Some((answer, _)) = up.answer
        {
            self.daemon.answer_later(self.key, &up.question, &answer);
        }
    }
}

/// What notify-send says when it couldn't show the question: the first since libnotify 0.7.10, the second since
/// 0.8.8. It exits 1 then, but only once its expiry (an hour) is out. With no notification server, all say the first.
const NOT_SHOWN: [&str; 2] = ["Actions are not supported", "Failed to show notification"];

/// Runs the pop-up and returns what it printed, or why it couldn't be shown: at once when notify-send says so.
async fn show(cmd: Command) -> Result<String, String> {
    use std::process::Stdio;
    use tokio::io::{AsyncBufReadExt, AsyncReadExt, BufReader};
    let mut child = tokio::process::Command::from(cmd)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .map_err(|e| e.to_string())?;
    let mut stdout = child.stdout.take().expect("piped");
    let mut stderr = BufReader::new(child.stderr.take().expect("piped")).split(b'\n');
    let out = async {
        let mut out = Vec::new();
        stdout.read_to_end(&mut out).await.map_err(|e| e.to_string())?;
        Ok(out)
    };
    let said = async {
        let mut said = String::new();
        // Read on whatever it is: a line that isn't UTF-8 (PowerShell's errors on Windows needn't be) mustn't stop it.
        while let Ok(Some(line)) = stderr.next_segment().await {
            let line = String::from_utf8_lossy(&line);
            if NOT_SHOWN.iter().any(|s| line.starts_with(s)) {
                return Err(format!("{said}{line}").trim().to_string());
            }
            said += line.trim_end_matches('\r');
            said.push('\n');
        }
        Ok(said)
    };
    let (out, said) = tokio::try_join!(out, said)?;
    if !child.wait().await.map_err(|e| e.to_string())?.success() {
        return Err(said.trim().to_string());
    }
    Ok(String::from_utf8_lossy(&out).into_owned())
}

/// The button on the first line (`approve` or `deny`); from a panel, how much of the request and of the question it
/// showed (characters) on the next two, then the note. Anything else is no answer.
fn parse(out: &str) -> (Answer, Option<(usize, usize)>) {
    let mut lines = out.trim().splitn(4, '\n');
    let choice = lines.next().unwrap_or_default().trim().to_lowercase();
    // A panel (macOS, Windows) that doesn't say how much it showed showed none of it.
    let mut count = || lines.next().and_then(|l| l.trim().parse().ok()).unwrap_or(0);
    let shown = cfg!(any(target_os = "macos", windows)).then(|| (count(), count()));
    let note = lines.next().unwrap_or_default().trim().to_string();
    let answer = match choice.as_str() {
        "approve" => Answer::Approved(note),
        "deny" => Answer::Denied(note),
        _ => Answer::Unanswered,
    };
    (answer, shown)
}

/// A floating panel with a note box, Deny (Escape) and Approve (Return). It shows on top without taking the keyboard,
/// so nothing typed elsewhere reaches it; it gets keys once clicked, in the note box. A teammate's request comes first:
/// what doesn't fit is cut from the question, and only then from the request. Prints the button, how much of the request
/// and of the question showed (characters), and the note; nothing if it closed unanswered.
#[cfg(target_os = "macos")]
const PANEL: &str = r#"ObjC.import("Cocoa")
function run([title, request, question, secs, icon]) {
  const app = $.NSApplication.sharedApplication
  app.setActivationPolicy($.NSApplicationActivationPolicyAccessory)
  let answer = null
  ObjC.registerSubclass({ name: "Answer", methods: {
    "approve:": { types: ["void", ["id"]], implementation: () => { answer = ["approve", note.stringValue.js]; app.stop(null) } },
    "deny:": { types: ["void", ["id"]], implementation: () => { answer = ["deny", note.stringValue.js]; app.stop(null) } },
  } })
  const target = $.Answer.alloc.init
  const [rs, qs] = [Array.from(request), Array.from(question)]
  let [r, q] = [rs.length, qs.length]
  const cut = (s, n) => s.slice(0, n).join("").trimEnd() + (n < s.length ? "…" : "")
  const text = () => (request ? `${cut(rs, r)}\n\nYour agent asks: ` : "") + cut(qs, q)
  const label = $.NSTextField.wrappingLabelWithString(text())
  // Measured as it is drawn, in a frame 330 wide.
  const tall = () => label.cell.cellSizeForBounds($.NSMakeRect(0, 0, 330, 1e7)).height
  while (tall() > 320 && r + q > 0) {
    if (q > 0) q = Math.max(0, q - 20); else r = Math.max(0, r - 20)
    label.stringValue = text()
  }
  const h = Math.max(tall(), 48)
  // Titled, non-activating: it doesn't take the keyboard from the app in front.
  const panel = $.NSPanel.alloc.initWithContentRectStyleMaskBackingDefer($.NSMakeRect(0, 0, 440, h + 96), 1 | 128, 2, false)
  panel.title = title
  panel.level = 8
  panel.becomesKeyOnlyIfNeeded = true
  panel.collectionBehavior = 1 | 256 // on every Space, and over full-screen apps
  const put = (view, x, y, w, vh) => { view.frame = $.NSMakeRect(x, y, w, vh); panel.contentView.addSubview(view) }
  put(label, 86, 84, 330, h)
  const image = icon && $.NSImage.alloc.initWithContentsOfFile(icon)
  if (image && !image.isNil()) put($.NSImageView.imageViewWithImage(image), 20, h + 28, 48, 48)
  const note = $.NSTextField.alloc.init
  note.placeholderString = "Note (optional)"
  note.target = target
  note.action = "approve:"
  put(note, 86, 48, 330, 24)
  panel.initialFirstResponder = note
  const button = (name, x, action, key) => {
    const b = $.NSButton.buttonWithTitleTargetAction(name, target, action)
    b.keyEquivalent = key
    put(b, x, 10, 96, 30)
  }
  button("Deny", 220, "deny:", "\u001b")
  button("Approve", 320, "approve:", "\r")
  panel.center
  panel.orderFrontRegardless
  $.NSTimer.scheduledTimerWithTimeIntervalTargetSelectorUserInfoRepeats(Number(secs), app, "stop:", null, false)
  app.run
  return answer ? `${answer[0]}\n${r}\n${q}\n${answer[1]}` : ""
}"#;

#[cfg(target_os = "macos")]
fn dialog(title: &str, request: &str, question: &str) -> Option<Command> {
    let mut cmd = Command::new(asker_bin("osascript", &["/usr/bin/osascript"])?);
    let icon = icon().map(|p| p.display().to_string()).unwrap_or_default();
    cmd.args(["-l", "JavaScript", "-e", PANEL, title, request, question, &OPEN_SECS.to_string(), &icon]);
    Some(cmd)
}

/// Text as the person should see it: without direction overrides, invisible spacers or tag characters, which could make
/// what shows differ from what is there (joiners stay, for emoji and scripts that need them).
fn plain(text: &str) -> String {
    let hides = |c: &char| {
        matches!(c, '\u{61C}' | '\u{200B}' | '\u{200E}' | '\u{200F}' | '\u{202A}'..='\u{202E}' | '\u{2060}'..='\u{2069}')
            || matches!(c, '\u{FEFF}' | '\u{E0000}'..='\u{E007F}')
    };
    text.chars().filter(|c| !hides(c)).collect()
}

/// At most `max` characters of `text`, marked where it is cut.
fn clip(text: &str, max: usize) -> String {
    let t = util::printable(text).trim().to_string();
    if t.chars().count() <= max { t } else { format!("{}…", t.chars().take(max).collect::<String>().trim_end()) }
}

/// The program that shows the question, or finds where to: in a release build only the system's own, so an agent can't
/// start this machine's engine with one that answers for its person. (Debug builds take `REASON_NETWORK_<NAME>_BIN`.)
pub(super) fn asker_bin(name: &str, system: &[&str]) -> Option<std::path::PathBuf> {
    if cfg!(debug_assertions) {
        return crate::agent::find_bin(name);
    }
    system.iter().map(std::path::PathBuf::from).find(|p| p.is_file())
}

/// Reason's icon for the panel, kept beside the network's database.
#[cfg(target_os = "macos")]
fn icon() -> Option<std::path::PathBuf> {
    const ICON: &[u8] = include_bytes!("../../assets/reason.icns");
    let path = crate::paths::home().join("reason.icns");
    if std::fs::metadata(&path).ok().map(|m| m.len()) != Some(ICON.len() as u64) {
        std::fs::write(&path, ICON).ok()?;
    }
    Some(path)
}

/// A notification with buttons: notify-send prints the one clicked, or nothing if it was closed or expired.
/// (`--wait` and `--action` need libnotify 0.7.10, from 2021; with an older one asking fails, and says so.)
#[cfg(all(unix, not(target_os = "macos")))]
fn dialog(title: &str, request: &str, question: &str) -> Option<Command> {
    use super::notify::{markup, notify_send_at};
    let body =
        if request.is_empty() { question.to_string() } else { format!("{request}\n\nYour agent asks: {question}") };
    let mut cmd =
        notify_send_at(asker_bin("notify-send", &["/usr/bin/notify-send", "/run/current-system/sw/bin/notify-send"])?)?;
    cmd.args(["--app-name=Reason Network", "--wait", "--action=approve=Approve", "--action=deny=Deny"])
        .arg(format!("--expire-time={}", OPEN_SECS * 1000))
        .args(["--".into(), markup(title), markup(&body)]);
    Some(cmd)
}

/// The same panel as on macOS: shown on top without taking the keyboard (once clicked into, Return approves and Escape
/// denies), the request first; prints the button, how much of the request and of the question showed, and the note.
#[cfg(windows)]
fn dialog(title: &str, request: &str, question: &str) -> Option<Command> {
    const PANEL: &str = r#"$ErrorActionPreference = 'Stop'
[Console]::OutputEncoding = [Text.Encoding]::UTF8
Add-Type -AssemblyName System.Windows.Forms, System.Drawing
Add-Type -WarningAction SilentlyContinue -ReferencedAssemblies System.Windows.Forms -TypeDefinition 'public class AskPanel : System.Windows.Forms.Form { protected override bool ShowWithoutActivation { get { return true; } } }'
$f = New-Object AskPanel
$f.Text = $env:TINCAN_TITLE; $f.TopMost = $true; $f.StartPosition = 'CenterScreen'; $f.FormBorderStyle = 'FixedDialog'
$f.MinimizeBox = $false; $f.MaximizeBox = $false; $f.AutoSize = $true; $f.AutoSizeMode = 'GrowAndShrink'; $f.Padding = 12
$l = New-Object Windows.Forms.FlowLayoutPanel; $l.FlowDirection = 'TopDown'; $l.AutoSize = $true
$q = New-Object Windows.Forms.Label; $q.UseMnemonic = $false; $q.UseCompatibleTextRendering = $false; $q.AutoSize = $true; $q.MaximumSize = '420,320'
$req = [Globalization.StringInfo]::new([string]$env:TINCAN_REQUEST); $ask = [Globalization.StringInfo]::new([string]$env:TINCAN_QUESTION)
$reqShown = $req.LengthInTextElements; $askShown = $ask.LengthInTextElements
# Cut and shown by text elements, so a mark stays with its letter; counted in characters, as the engine counts.
$cut = { param($s, $k) if ($k -ge $s.LengthInTextElements) { $s.String } elseif ($k -le 0) { [string][char]0x2026 } else { $s.SubstringByTextElements(0, $k).TrimEnd() + [char]0x2026 } }
$chars = { param($s, $k) [string]$t = if ($k -ge $s.LengthInTextElements) { $s.String } elseif ($k -le 0) { '' } else { $s.SubstringByTextElements(0, $k) }; $t.Length - @($t.ToCharArray() | Where-Object { [char]::IsLowSurrogate($_) }).Count }
$text = { if ($req.String) { (& $cut $req $reqShown) + "`n`nYour agent asks: " + (& $cut $ask $askShown) } else { & $cut $ask $askShown } }
$q.Text = & $text
# Measured the way the label lays it out.
$fits = { $m = New-Object Windows.Forms.Label; $m.UseMnemonic = $false; $m.UseCompatibleTextRendering = $false; $m.Font = $q.Font; $m.Text = $q.Text; $m.GetPreferredSize('420,0').Height -le 320 }
while (-not (& $fits) -and ($reqShown + $askShown) -gt 0) {
  if ($askShown -gt 0) { $askShown = [Math]::Max(0, $askShown - 20) } else { $reqShown = [Math]::Max(0, $reqShown - 20) }
  $q.Text = & $text
}
$note = New-Object Windows.Forms.TextBox; $note.Width = 420
$b = New-Object Windows.Forms.FlowLayoutPanel; $b.FlowDirection = 'RightToLeft'; $b.Width = 420; $b.Height = 34
$yes = New-Object Windows.Forms.Button; $yes.Text = 'Approve'; $no = New-Object Windows.Forms.Button; $no.Text = 'Deny'
$b.Controls.AddRange(@($yes, $no)); $l.Controls.AddRange(@($q, $note, $b)); $f.Controls.Add($l)
$f.AcceptButton = $yes; $f.CancelButton = $no; $f.ActiveControl = $note
$script:choice = ''
$yes.Add_Click({ $script:choice = 'approve'; $script:said = $note.Text; $f.Close() })
$no.Add_Click({ $script:choice = 'deny'; $script:said = $note.Text; $f.Close() })
$t = New-Object Windows.Forms.Timer; $t.Interval = [int]$env:TINCAN_SECS * 1000; $t.Add_Tick({ $f.Close() }); $t.Start()
[Windows.Forms.Application]::Run($f)
if ($script:choice) { "$($script:choice)`n$(& $chars $req $reqShown)`n$(& $chars $ask $askShown)`n$($script:said)" }"#;
    let powershell = asker_bin("powershell", &[r"C:\Windows\System32\WindowsPowerShell\v1.0\powershell.exe"])?;
    let mut cmd = super::notify::powershell_at(powershell, PANEL);
    cmd.env("TINCAN_TITLE", title).env("TINCAN_REQUEST", request).env("TINCAN_QUESTION", question);
    cmd.env("TINCAN_SECS", OPEN_SECS.to_string());
    Some(cmd)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn a_late_answer_reaches_the_session_but_isnt_counted_as_a_message() {
        let d = Arc::new(super::super::test_daemon(crate::config::Config::default()));
        let reg = crate::proto::SessionReg {
            key: "codex:1".into(),
            agent: "codex".into(),
            cwd: "/work/api".into(),
            agent_session_id: None,
            pid: None,
            tier: crate::proto::Tier::Pull,
            name: None,
        };
        let address = d.register(None, reg, crate::git::GitInfo::default()).unwrap().address;
        d.answer_later("codex:1", "ship it?", &Answer::Approved(String::new()));
        let g = d.inner.lock().unwrap();
        assert!(g.store.unread(&address).unwrap()[0].body.contains("ship it?"));
        assert_eq!(g.store.counts(util::now_ms()).unwrap(), []);
    }

    #[test]
    fn what_shows_is_what_is_there() {
        // A direction override can't turn text around, nor a spacer or tag character hide in it; joiners stay.
        let family = "\u{1F468}\u{200D}\u{1F469}";
        assert_eq!(
            plain(&format!("rename \u{202E}dorp\u{202C} it\u{200B}\u{E0041} {family}")),
            format!("rename dorp it {family}")
        );
        assert_eq!(clip("abcdef", 3), "abc…");
        assert_eq!(clip("abc", 3), "abc");
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn a_pop_up_that_cant_show_fails_at_once() {
        let sh = |script: &str| {
            let mut cmd = Command::new("sh");
            cmd.args(["-c", script]);
            cmd
        };
        // notify-send with no notification server: it says so, then waits out its expiry.
        let started = std::time::Instant::now();
        let failed = show(sh("echo 'Failed to show notification: ServiceUnknown' >&2; sleep 30")).await;
        assert_eq!(failed, Err("Failed to show notification: ServiceUnknown".into()));
        assert!(started.elapsed() < Duration::from_secs(10));
        assert_eq!(show(sh("echo 'Wait timeout expired' >&2; echo approve")).await, Ok("approve\n".into()));
        assert_eq!(show(sh("echo 'no bus' >&2; exit 1")).await, Err("no bus".into()));
    }

    #[test]
    fn a_panel_says_how_much_it_showed() {
        let (answer, shown) = parse("approve\n12\n3\nsend it\n");
        if cfg!(any(target_os = "macos", windows)) {
            assert!(matches!(answer, Answer::Approved(n) if n == "send it"));
            assert_eq!(shown, Some((12, 3)));
            // One that doesn't say showed nothing.
            assert_eq!(parse("deny\nabc").1, Some((0, 0)));
        } else {
            assert!(matches!(parse("approve\n").0, Answer::Approved(n) if n.is_empty()));
            assert_eq!(shown, None);
        }
    }
}
