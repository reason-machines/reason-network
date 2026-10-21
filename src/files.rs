//! Small text files carried in a message: read from paths on the sender's computer, checked against the limits on both
//! sides, and saved in the recipient's inbox folder (`<state home>/inbox/<folder>/<name>`, the folder named by a local
//! id, never the sender's), never executable and never opened. They travel inside the message itself, so they are as
//! authenticated as its text.

use crate::{proto::Attachment, render};
use anyhow::{Result, anyhow, bail};
use std::{
    collections::{HashMap, HashSet, VecDeque},
    io::Read,
    path::{Component, Path, PathBuf},
    time::{Duration, Instant},
};

pub const MAX_FILE: usize = 256 * 1024;
pub const MAX_TOTAL: usize = 512 * 1024;
pub const MAX_FILES: usize = 10;
/// A name is cut to this many characters and UTF-8 bytes (with any `-2` suffix), keeping its extension: every file
/// system takes it.
const MAX_NAME: usize = 100;
const MAX_NAME_BYTES: usize = 200;
/// What a machine's presence lists when its engine takes files. One that doesn't would drop them.
pub const FEATURE: &str = "files";
/// A message's files are kept for a day once it was read, and at most a week unread (or held).
pub const KEEP_MS: i64 = 86_400_000;
pub const KEEP_UNREAD_MS: i64 = 7 * 86_400_000;
/// The most this machine keeps of teammates' files (saved, and held for approval), and takes from one person in an
/// hour. Past either, a teammate's machine is asked to retry later.
pub const QUOTA_TOTAL: usize = 100 * 1024 * 1024;
pub const QUOTA_SENDER_HOUR: usize = 20 * 1024 * 1024;
/// And keeps at most this much from one person, so no teammate can take all of it.
pub const QUOTA_SENDER_KEPT: usize = 25 * 1024 * 1024;

const TEXT_ONLY: &str = "only text files can be sent (markdown, source code, JSON, logs, config)";

/// Where an agent's `send_message` may read files from: its own folder (its repository, or where it works), and none of
/// that folder's hidden files or folders, nor anything under the places that hold credentials or Reason Network's state.
/// It keeps a prompt-injected agent from mailing out what its harness wouldn't let it read.
pub struct Confine {
    root: PathBuf,
    forbidden: Vec<PathBuf>,
    /// For the folders the person turned Reason Network off for.
    folders: crate::config::Config,
}

impl Confine {
    pub fn new(root: &Path, home: &Path, state: &Path) -> Result<Confine> {
        let root = std::fs::canonicalize(root).map_err(|_| anyhow!("can't find this session's folder"))?;
        let real_home = std::fs::canonicalize(home).unwrap_or_else(|_| home.to_path_buf());
        // The home folder, or a folder holding it (`/`, `/Users`), is everything the person has, not a project.
        if root.parent().is_none() || real_home.starts_with(&root) {
            bail!(
                "this session works in {}, not a project folder: start the session in a project folder to send files",
                root.display()
            );
        }
        let mut refused =
            vec![home.join(".config"), home.join(".ssh"), home.join(".aws"), home.join("Library"), state.to_path_buf()];
        refused.extend(["APPDATA", "LOCALAPPDATA"].iter().filter_map(std::env::var_os).map(PathBuf::from));
        let forbidden = refused.iter().map(|p| std::fs::canonicalize(p).unwrap_or_else(|_| p.clone())).collect();
        Ok(Confine { root, forbidden, folders: Default::default() })
    }

    /// Nothing from the folders `cfg` turns Reason Network off for, either.
    pub fn with_folders(self, cfg: crate::config::Config) -> Confine {
        Confine { folders: cfg, ..self }
    }

    /// The real file at `path` (symlinks resolved), if it may be sent.
    fn allow(&self, given: &str, path: &Path) -> Result<PathBuf> {
        let real = std::fs::canonicalize(path).map_err(|_| anyhow!("can't read {given}"))?;
        let Ok(inside) = real.strip_prefix(&self.root) else {
            bail!(
                "{given} is outside this session's folder ({}); send_message sends files from there only",
                self.root.display()
            );
        };
        let hidden =
            inside.components().any(|c| matches!(c, Component::Normal(n) if n.to_string_lossy().starts_with('.')));
        if hidden {
            bail!(
                "{given} is a hidden file or in a hidden folder, which often hold secrets; send_message doesn't send those"
            );
        }
        if self.folders.off_in(&real.display().to_string()) {
            return Err(in_off_folder(given));
        }
        if self.forbidden.iter().any(|f| real.starts_with(f)) {
            bail!(
                "{given} is where credentials or Reason Network's own state are kept; send_message doesn't send those"
            );
        }
        Ok(real)
    }
}

fn in_off_folder(given: &str) -> anyhow::Error {
    anyhow!("{given} is in a folder your user turned Reason Network off for; files aren't sent from there")
}

/// Reads the files at `paths` (relative ones from `cwd`) for a message: anywhere, for a person's own `send --file`.
pub fn read(paths: &[String], cwd: &Path) -> Result<Vec<Attachment>> {
    read_from(paths, cwd, None, None)
}

/// The same, but none from a folder `folders` turns Reason Network off for: for `send --file` from an agent's shell.
pub fn read_outside(paths: &[String], cwd: &Path, folders: &crate::config::Config) -> Result<Vec<Attachment>> {
    read_from(paths, cwd, None, Some(folders))
}

/// The same, only from where `confine` allows: for an agent's `send_message`.
pub fn read_within(paths: &[String], cwd: &Path, confine: &Confine) -> Result<Vec<Attachment>> {
    read_from(paths, cwd, Some(confine), None)
}

fn read_from(
    paths: &[String],
    cwd: &Path,
    confine: Option<&Confine>,
    folders: Option<&crate::config::Config>,
) -> Result<Vec<Attachment>> {
    if paths.len() > MAX_FILES {
        bail!(too_many(paths.len()));
    }
    let mut files = Vec::new();
    for p in paths {
        let mut path = cwd.join(p);
        if let Some(c) = confine {
            path = c.allow(p, &path)?;
        }
        // Judged by where the file really is (links followed), whichever folder the command runs in; read, and named,
        // as given.
        if let Some(cfg) = folders
            && std::fs::canonicalize(&path).is_ok_and(|r| cfg.off_in(&r.display().to_string()))
        {
            return Err(in_off_folder(p));
        }
        // Asked before opening, too: Windows doesn't open a folder at all ("Access is denied"), which would hide why.
        if std::fs::metadata(&path).is_ok_and(|m| !m.is_file()) {
            bail!("{p} is not a file");
        }
        let file = std::fs::File::open(&path).map_err(|e| anyhow!("can't read {p}: {e}"))?;
        if !file.metadata().is_ok_and(|m| m.is_file()) {
            bail!("{p} is not a file");
        }
        // However big it says it is (or grows to), no more than the limit and a byte is read.
        let mut bytes = Vec::new();
        file.take(MAX_FILE as u64 + 1).read_to_end(&mut bytes).map_err(|e| anyhow!("can't read {p}: {e}"))?;
        if bytes.len() > MAX_FILE {
            bail!(
                "{p} is over {} KB; a file can be at most {} KB (send a link for bigger ones)",
                MAX_FILE / 1024,
                MAX_FILE / 1024
            );
        }
        let content = text(p, bytes)?;
        let name = path.file_name().map_or_else(|| p.clone(), |n| n.to_string_lossy().into_owned());
        files.push(Attachment { name, content, path: None });
    }
    check(files)
}

/// The text of a file, if it is one: valid UTF-8 (control characters are refused by [`check`]).
fn text(name: &str, bytes: Vec<u8>) -> Result<String> {
    if bytes.contains(&0) {
        bail!("{name} looks like a binary file: {TEXT_ONLY}");
    }
    String::from_utf8(bytes).map_err(|_| anyhow!("{name} isn't UTF-8 text: {TEXT_ONLY}"))
}

/// Holds files to the limits and gives them safe, distinct names. Both sides run it: the sender's daemon, and the
/// recipient's, which takes nothing a peer sends on trust. Text has no control characters but tab and line ends.
pub fn check(files: Vec<Attachment>) -> Result<Vec<Attachment>> {
    if files.len() > MAX_FILES {
        bail!(too_many(files.len()));
    }
    let mut total = 0;
    let mut taken = HashSet::new();
    let mut out = Vec::with_capacity(files.len());
    for f in files {
        if f.content.len() > MAX_FILE {
            bail!(too_big(&f.name, f.content.len()));
        }
        if f.content.chars().any(|c| c.is_control() && !matches!(c, '\t' | '\n' | '\r')) {
            bail!("{} has control characters, so it looks like a binary file: {TEXT_ONLY}", f.name);
        }
        total += f.content.len();
        if total > MAX_TOTAL {
            bail!(
                "the files come to more than {} KB; a message carries at most {} KB of files (send a link for more)",
                MAX_TOTAL / 1024,
                MAX_TOTAL / 1024
            );
        }
        let name = distinct(&safe_name(&f.name), &mut taken);
        out.push(Attachment { name, content: f.content, path: None });
    }
    Ok(out)
}

/// The text of files, in bytes.
pub fn size(files: &[Attachment]) -> usize {
    files.iter().map(|f| f.content.len()).sum()
}

fn too_many(n: usize) -> String {
    format!("{n} files; a message carries at most {MAX_FILES}")
}

fn too_big(name: &str, bytes: usize) -> String {
    format!(
        "{name} is {} KB; a file can be at most {} KB (send a link for bigger ones)",
        bytes.div_ceil(1024),
        MAX_FILE / 1024
    )
}

/// A file name that is just a name, here and on Windows: no folders (no separators, no `..`), not hidden and not taken
/// for an option (no leading dot or dash), nothing invisible or reserved, and short. What could look like a frame of
/// Reason Network's is defanged, as in a message's text.
pub fn safe_name(name: &str) -> String {
    let swapped: String =
        name.chars()
            .filter(|c| !invisible(*c))
            .map(|c| {
                if c.is_control() || matches!(c, '/' | '\\' | ':' | '<' | '>' | '"' | '|' | '?' | '*') {
                    '_'
                } else {
                    c
                }
            })
            .collect();
    let mut s = swapped;
    while s.contains("..") {
        s = s.replace("..", ".");
    }
    let s = s.trim_start_matches(|c: char| c == '.' || c == '-' || c.is_whitespace());
    let s = s.trim_end_matches(|c: char| c == '.' || c.is_whitespace());
    let mut s = render::defang(&shorten(s, ""));
    if reserved(&s) {
        s.insert(0, '_');
        s = shorten(&s, "");
    }
    if s.is_empty() { "file".into() } else { s }
}

/// A name Windows keeps for a device, whatever its extension: `CON`, `nul.txt`, `COM1`, `LPT²`, `CONIN$`, and the same
/// with spaces before the dot (`nul .txt`), which Windows drops.
fn reserved(name: &str) -> bool {
    let stem = name.split('.').next().unwrap_or("").trim_end().to_uppercase();
    let numbered = |prefix: &str| {
        stem.strip_prefix(prefix).is_some_and(|n| {
            let mut c = n.chars();
            matches!((c.next(), c.next()), (Some('0'..='9' | '¹' | '²' | '³'), None))
        })
    };
    matches!(stem.as_str(), "CON" | "PRN" | "AUX" | "NUL" | "CONIN$" | "CONOUT$") || numbered("COM") || numbered("LPT")
}

/// Zero-width, direction, tag and other invisible characters (and line separators), which could make a name read as
/// another.
fn invisible(c: char) -> bool {
    matches!(c, '\u{AD}' | '\u{34F}' | '\u{61C}' | '\u{115F}' | '\u{1160}' | '\u{180E}' | '\u{3164}' | '\u{FEFF}')
        || matches!(c, '\u{FFA0}' | '\u{2028}' | '\u{2029}')
        || ('\u{200B}'..='\u{200F}').contains(&c)
        || ('\u{202A}'..='\u{202E}').contains(&c)
        || ('\u{2060}'..='\u{206F}').contains(&c)
        || ('\u{FE00}'..='\u{FE0F}').contains(&c)
        || ('\u{E0000}'..='\u{E007F}').contains(&c)
}

/// `name` with `suffix` (such as `-2`) before its extension, cut to [`MAX_NAME`] characters and [`MAX_NAME_BYTES`]
/// bytes, keeping the extension.
fn shorten(name: &str, suffix: &str) -> String {
    let (stem, ext) = split_ext(name);
    let fits = |s: &str| {
        let all = format!("{s}{suffix}{ext}");
        all.chars().count() <= MAX_NAME && all.len() <= MAX_NAME_BYTES
    };
    let mut stem = stem.to_string();
    let cut = !fits(&stem);
    while !fits(&stem) && stem.pop().is_some() {}
    if cut {
        stem.truncate(stem.trim_end_matches(['.', ' ']).len());
    }
    format!("{stem}{suffix}{ext}")
}

/// `notes.md` -> (`notes`, `.md`); an extension longer than 16 characters isn't one.
fn split_ext(name: &str) -> (&str, &str) {
    match name.rfind('.') {
        Some(i) if i > 0 && name[i..].chars().count() <= 16 => name.split_at(i),
        _ => (name, ""),
    }
}

/// `name`, or `name-2`, `name-3`, … when another file of the message has it (ignoring case, as macOS and Windows do).
fn distinct(name: &str, taken: &mut HashSet<String>) -> String {
    let mut pick = name.to_string();
    let mut n = 2;
    while !taken.insert(pick.to_lowercase()) {
        pick = shorten(name, &format!("-{n}"));
        n += 1;
    }
    pick
}

/// Saves a message's files (already through [`check`]) in `dir`, its own new folder in the inbox folder: readable by
/// this user only, never executable, and marked as downloaded (macOS quarantine, Windows' internet zone). A name the
/// file system takes for one already there (other Unicode forms, case or width) gets `-2`, `-3`, …; returns the files
/// as saved, by those names. Nothing is left behind if it fails, and errors name no local paths (a teammate's machine
/// may be told them).
pub fn save(dir: &Path, files: &[Attachment]) -> Result<Vec<Attachment>> {
    let mut folder = std::fs::DirBuilder::new();
    #[cfg(unix)]
    std::os::unix::fs::DirBuilderExt::mode(&mut folder, 0o700);
    if let Some(inbox) = dir.parent() {
        folder.recursive(true).create(inbox).map_err(|e| anyhow!("can't create the inbox folder: {}", e.kind()))?;
        #[cfg(unix)]
        crate::util::chmod(inbox, 0o700).map_err(|e| anyhow!("can't secure the inbox folder: {}", e.kind()))?;
    }
    // A new folder: one already there (it never should be) is left alone.
    folder.recursive(false).create(dir).map_err(|e| anyhow!("can't create a folder for the files: {}", e.kind()))?;
    let mut saved = Vec::with_capacity(files.len());
    for (i, f) in files.iter().enumerate() {
        match save_one(dir, f) {
            Ok(name) => saved.push(Attachment { name, content: String::new(), path: None }),
            Err(e) => {
                let _ = std::fs::remove_dir_all(dir);
                return Err(anyhow!("can't save file {} of the message: {}", i + 1, e.kind()));
            }
        }
    }
    Ok(saved)
}

fn save_one(dir: &Path, f: &Attachment) -> std::io::Result<String> {
    let mut name = f.name.clone();
    for n in 2.. {
        let mut opts = std::fs::OpenOptions::new();
        opts.write(true).create_new(true);
        #[cfg(unix)]
        std::os::unix::fs::OpenOptionsExt::mode(&mut opts, 0o600);
        match opts.open(dir.join(&name)) {
            Ok(mut file) => {
                std::io::Write::write_all(&mut file, f.content.as_bytes())?;
                mark_downloaded(&dir.join(&name));
                return Ok(name);
            }
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists && n <= 100 => {
                name = shorten(&f.name, &format!("-{n}"))
            }
            Err(e) => return Err(e),
        }
    }
    unreachable!("the loop returns")
}

/// Marks a saved file as come from elsewhere, as browsers do: macOS asks before it opens it, Windows treats it as from
/// the internet. Best effort.
fn mark_downloaded(path: &Path) {
    #[cfg(target_os = "macos")]
    {
        use std::os::unix::ffi::OsStrExt;
        let secs = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_secs());
        let value = format!("0081;{secs:x};Reason Network;");
        if let Ok(p) = std::ffi::CString::new(path.as_os_str().as_bytes()) {
            // SAFETY: both strings are NUL-terminated and outlive the call; the value's length is passed.
            unsafe {
                libc::setxattr(p.as_ptr(), c"com.apple.quarantine".as_ptr(), value.as_ptr().cast(), value.len(), 0, 0);
            }
        }
    }
    #[cfg(windows)]
    {
        let mut stream = path.as_os_str().to_owned();
        stream.push(":Zone.Identifier");
        let _ = std::fs::write(stream, "[ZoneTransfer]\r\nZoneId=3\r\n");
    }
    #[cfg(not(any(target_os = "macos", windows)))]
    let _ = path;
}

/// Files saved in `dir`, as the recipient sees them: their paths, no text.
pub fn located(dir: &Path, files: &[Attachment]) -> Vec<Attachment> {
    files
        .iter()
        .map(|f| Attachment {
            name: f.name.clone(),
            content: String::new(),
            path: Some(dir.join(&f.name).display().to_string()),
        })
        .collect()
}

/// What each teammate sent in files in the last hour, against [`QUOTA_SENDER_HOUR`] and [`QUOTA_TOTAL`].
#[derive(Default)]
pub struct Quota {
    recent: HashMap<String, VecDeque<(Instant, usize)>>,
}

impl Quota {
    /// Takes `bytes` of files from `sender`, with `kept` bytes of teammates' files on this machine already (`from_sender`
    /// of them that person's), or says why not (a teammate's machine retries later).
    pub fn admit(
        &mut self,
        sender: &str,
        bytes: usize,
        kept: usize,
        from_sender: usize,
        now: Instant,
    ) -> Result<(), String> {
        if kept + bytes > QUOTA_TOTAL {
            return Err(format!(
                "it keeps {} MB of teammates' files at most and has that many now; it takes these once some are read",
                QUOTA_TOTAL >> 20
            ));
        }
        if from_sender + bytes > QUOTA_SENDER_KEPT {
            return Err(format!(
                "it keeps {} MB of files from {sender} at most and has that many now; it takes these once some are read",
                QUOTA_SENDER_KEPT >> 20
            ));
        }
        let q = self.recent.entry(sender.to_string()).or_default();
        while q.front().is_some_and(|(at, _)| now.duration_since(*at) > Duration::from_secs(3600)) {
            q.pop_front();
        }
        if q.iter().map(|(_, b)| b).sum::<usize>() + bytes > QUOTA_SENDER_HOUR {
            return Err(format!("it takes {} MB of files an hour from {sender} at most", QUOTA_SENDER_HOUR >> 20));
        }
        q.push_back((now, bytes));
        Ok(())
    }
}

/// For a recipient whose engine can't take files: the message's text with them written out in it, as fenced blocks,
/// if all of it fits in `max` bytes.
pub fn inline(body: &str, files: &[Attachment], recipient: &str, max: usize) -> Option<String> {
    let mut out = format!(
        "{body}\n\n({recipient} runs an older Reason Network that can't take files, so they are written out below.)"
    );
    for f in files {
        let longest = f.content.split(|c| c != '`').map(str::len).max().unwrap_or(0);
        let fence = "`".repeat(longest.max(2) + 1);
        let end = if f.content.ends_with('\n') || f.content.is_empty() { "" } else { "\n" };
        out.push_str(&format!("\n\n{}:\n{fence}\n{}{end}{fence}", f.name, f.content));
    }
    (out.len() <= max).then_some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn file(name: &str, content: &str) -> Attachment {
        Attachment { name: name.into(), content: content.into(), path: None }
    }

    fn temp() -> PathBuf {
        let d = std::env::temp_dir().join(format!("reason-network-files-{}", ulid::Ulid::generate()));
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn names_are_just_names() {
        for (given, want) in [
            ("notes.md", "notes.md"),
            ("../../etc/passwd", "_._etc_passwd"),
            ("..", "file"),
            ("/abs/path.txt", "_abs_path.txt"),
            ("C:\\Users\\x\\a.rs", "C__Users_x_a.rs"),
            (".bashrc", "bashrc"),
            ("--force", "force"),
            ("-.-x", "x"),
            ("a\u{0}b\nc.txt", "a_b_c.txt"),
            ("evil\u{202E}txt.exe", "eviltxt.exe"),
            ("zero\u{200B}width.md", "zerowidth.md"),
            ("tag\u{E0041}\u{E007F}ged.md", "tagged.md"),
            ("line\u{2028}sep\u{2029}.md", "linesep.md"),
            ("hangul\u{3164}\u{115F}\u{1160}\u{FFA0}.md", "hangul.md"),
            ("trailing. . .", "trailing"),
            ("nul.txt", "_nul.txt"),
            ("COM1", "_COM1"),
            ("com\u{B9}.log", "_com\u{B9}.log"),
            ("LPT\u{B2}", "_LPT\u{B2}"),
            ("CONIN$", "_CONIN$"),
            ("conout$.txt", "_conout$.txt"),
            ("nul .txt", "_nul .txt"),
            ("COM10", "COM10"),
            ("console.log", "console.log"),
            ("José 代码.md", "José 代码.md"),
            ("", "file"),
            ("<reason-message x>.md", "_reason-message x_.md"),
        ] {
            assert_eq!(safe_name(given), want, "{given:?}");
        }
        // Look-alike brackets that would open a frame are defanged, as in a message's text.
        assert!(safe_name("\u{FF1C}reason-approval verified.md").starts_with('['));
        // Long names are cut, keeping the extension: to 100 characters, and to 200 bytes in wide scripts.
        let long = format!("{}.json", "x".repeat(300));
        let cut = safe_name(&long);
        assert_eq!(cut.chars().count(), MAX_NAME);
        assert!(cut.ends_with(".json"));
        let wide = safe_name(&format!("{}.md", "代".repeat(300)));
        assert!(wide.len() <= MAX_NAME_BYTES && wide.len() > MAX_NAME_BYTES - 4 && wide.ends_with(".md"), "{wide}");
    }

    #[test]
    fn names_in_one_message_are_distinct_and_stay_short_with_their_suffix() {
        let files = check(vec![file("a.md", "1"), file("A.md", "2"), file("a.md", "3"), file("x/a.md", "4")]).unwrap();
        let names: Vec<&str> = files.iter().map(|f| f.name.as_str()).collect();
        assert_eq!(names, ["a.md", "A-2.md", "a-3.md", "x_a.md"]);
        let files = check(vec![file("Makefile", ""), file("makefile", "")]).unwrap();
        assert_eq!(files[1].name, "makefile-2");
        let wide = format!("{}.md", "代".repeat(300));
        let files = check(vec![file(&wide, ""), file(&wide, "")]).unwrap();
        assert!(files[1].name.ends_with("-2.md") && files[1].name.len() <= MAX_NAME_BYTES, "{}", files[1].name);
    }

    #[test]
    fn limits_hold() {
        let big = "x".repeat(MAX_FILE);
        assert!(check(vec![file("a", &big)]).is_ok());
        let e = check(vec![file("a", &format!("{big}x"))]).unwrap_err().to_string();
        assert!(e.contains("a is 257 KB; a file can be at most 256 KB"), "{e}");
        assert!(check(vec![file("a", &big), file("b", &big)]).is_ok());
        let e = check(vec![file("a", &big), file("b", &big), file("c", "x")]).unwrap_err().to_string();
        assert!(e.contains("at most 512 KB"), "{e}");
        assert!(check((0..MAX_FILES).map(|i| file(&i.to_string(), "x")).collect()).is_ok());
        let e = check((0..=MAX_FILES).map(|i| file(&i.to_string(), "x")).collect()).unwrap_err().to_string();
        assert!(e.contains("11 files; a message carries at most 10"), "{e}");
        // Control characters, but for tab and line ends, make it no text file.
        for bad in ["x\0y", "esc \u{1b}[31m", "bell\u{7}", "c1 \u{85}", "del\u{7f}"] {
            let e = check(vec![file("a.bin", bad)]).unwrap_err().to_string();
            assert!(e.contains("control characters") && e.contains("only text files"), "{bad:?}: {e}");
        }
        assert!(check(vec![file("ok.tsv", "a\tb\r\nc\n")]).is_ok());
        // A peer can't slip a path in.
        let mut f = file("a", "x");
        f.path = Some("/etc/passwd".into());
        assert_eq!(check(vec![f]).unwrap()[0].path, None);
    }

    #[test]
    fn only_text_files_are_read() {
        let d = temp();
        std::fs::create_dir_all(d.join("sub")).unwrap();
        std::fs::write(d.join("notes.md"), "# hi\n").unwrap();
        std::fs::write(d.join("sub/main.rs"), "fn main() {}\n").unwrap();
        std::fs::write(d.join("img.png"), [0x89, b'P', b'N', b'G', 0, 1]).unwrap();
        std::fs::write(d.join("latin1.txt"), [b'c', b'a', b'f', 0xe9]).unwrap();
        std::fs::write(d.join("big.log"), "x".repeat(MAX_FILE + 1)).unwrap();
        let files = read(&["notes.md".into(), d.join("sub/main.rs").display().to_string()], &d).unwrap();
        assert_eq!(files[0], file("notes.md", "# hi\n"));
        assert_eq!(files[1], file("main.rs", "fn main() {}\n"));
        for (p, want) in [
            ("img.png", "looks like a binary file"),
            ("latin1.txt", "isn't UTF-8 text"),
            ("big.log", "a file can be at most 256 KB"),
            ("sub", "is not a file"),
            ("missing.txt", "can't read missing.txt"),
        ] {
            let e = format!("{:#}", read(&[p.into()], &d).unwrap_err());
            assert!(e.contains(want), "{p}: {e}");
        }
        let many: Vec<String> = (0..11).map(|_| "notes.md".to_string()).collect();
        assert!(read(&many, &d).unwrap_err().to_string().contains("at most 10"));
        std::fs::remove_dir_all(&d).unwrap();
    }

    /// What doesn't take symlinks, on every platform (Windows included: its paths come back from `canonicalize` as
    /// `\\?\C:\…`).
    #[test]
    fn an_agent_sends_only_from_its_own_folder_everywhere() {
        // Not in the temp folder: on Windows that is under %LOCALAPPDATA%, which is out of reach (see `Confine`).
        let d =
            Path::new(env!("CARGO_MANIFEST_DIR")).join("target").join(format!("files-test-{}", ulid::Ulid::generate()));
        std::fs::create_dir_all(&d).unwrap();
        let (home, repo, state) = (d.join("home"), d.join("home").join("src").join("api"), d.join("state"));
        for dir in [repo.join("docs"), repo.join(".github"), home.join(".ssh"), state.clone()] {
            std::fs::create_dir_all(dir).unwrap();
        }
        std::fs::write(repo.join("docs").join("plan.md"), "plan").unwrap();
        std::fs::write(repo.join(".env"), "SECRET=1").unwrap();
        std::fs::write(home.join("notes.txt"), "outside").unwrap();
        let confine = Confine::new(&repo, &home, &state).unwrap();
        let docs_plan = Path::new("docs").join("plan.md").display().to_string();
        assert_eq!(read_within(&[docs_plan], &repo, &confine).unwrap()[0].content, "plan");
        let outside = Path::new("..").join("..").join("notes.txt").display().to_string();
        for (p, want) in [
            (".env".to_string(), "hidden file"),
            (outside, "outside this session's folder"),
            (home.join("notes.txt").display().to_string(), "outside this session's folder"),
            ("docs".to_string(), "is not a file"),
        ] {
            let e = read_within(std::slice::from_ref(&p), &repo, &confine).unwrap_err().to_string();
            assert!(e.contains(want), "{p}: {e}");
        }
        let top = d.ancestors().last().unwrap().to_path_buf();
        for root in [home.as_path(), d.as_path(), top.as_path()] {
            let e = Confine::new(root, &home, &state).err().map(|e| e.to_string()).unwrap_or_default();
            assert!(e.contains("start the session in a project folder to send files"), "{}: {e}", root.display());
        }
        std::fs::remove_dir_all(&d).unwrap();
    }

    #[test]
    #[cfg(unix)]
    fn an_agent_sends_only_from_its_own_folder() {
        let d = temp();
        let (home, repo, state) = (d.join("home"), d.join("home/src/api"), d.join("state"));
        for dir in
            [repo.join("docs"), repo.join(".github"), home.join(".ssh"), home.join(".config/reason"), state.clone()]
        {
            std::fs::create_dir_all(dir).unwrap();
        }
        for (p, text) in [
            (repo.join("docs/plan.md"), "plan"),
            (repo.join(".env"), "SECRET=1"),
            (repo.join(".github/ci.yml"), "ci"),
            (home.join(".ssh/id_ed25519"), "key"),
            (home.join("notes.txt"), "outside"),
            (state.join("network.db"), "state"),
        ] {
            std::fs::write(p, text).unwrap();
        }
        std::os::unix::fs::symlink(home.join(".ssh/id_ed25519"), repo.join("docs/key.txt")).unwrap();
        std::os::unix::fs::symlink(home.join("notes.txt"), repo.join("docs/notes.txt")).unwrap();
        std::os::unix::fs::symlink(repo.join("docs/plan.md"), repo.join("plan-link.md")).unwrap();
        let confine = Confine::new(&repo, &home, &state).unwrap();
        let ok = read_within(&["docs/plan.md".into(), "plan-link.md".into()], &repo, &confine).unwrap();
        // A link inside the folder to a file inside it is that file.
        assert_eq!(
            (ok[0].content.as_str(), ok[1].name.as_str(), ok[1].content.as_str()),
            ("plan", "plan-2.md", "plan")
        );
        for (p, want) in [
            (".env", "hidden file"),
            (".github/ci.yml", "hidden file"),
            ("../../.ssh/id_ed25519", "outside this session's folder"),
            ("docs/key.txt", "outside this session's folder"),
            ("docs/notes.txt", "outside this session's folder"),
            (&*home.join("notes.txt").display().to_string(), "outside this session's folder"),
        ] {
            let e = read_within(&[p.to_string()], &repo, &confine).unwrap_err().to_string();
            assert!(e.contains(want), "{p}: {e}");
        }
        // A session working in the home folder, or a folder above it, has no project folder: nothing is sent.
        for root in [home.as_path(), d.as_path(), Path::new("/")] {
            let e = Confine::new(root, &home, &state).err().map(|e| e.to_string()).unwrap_or_default();
            assert!(e.contains("start the session in a project folder to send files"), "{}: {e}", root.display());
        }
        // A project folder holding the state, or ~/Library: those stay out of reach.
        std::fs::create_dir_all(home.join("Library/Keychains")).unwrap();
        std::fs::write(home.join("Library/Keychains/login.txt"), "keychain").unwrap();
        let in_state = Confine::new(&home.join("src"), &home, &home.join("src/api/docs")).unwrap();
        let e = read_within(&["api/docs/plan.md".into()], &home.join("src"), &in_state).unwrap_err().to_string();
        assert!(e.contains("Reason Network's own state"), "{e}");
        std::os::unix::fs::symlink(home.join("Library"), repo.join("lib")).unwrap();
        let e = read_within(&["lib/Keychains/login.txt".into()], &repo, &confine).unwrap_err().to_string();
        assert!(e.contains("outside this session's folder"), "{e}");
        let library = Confine::new(&home.join("Library"), &home, &state).unwrap();
        let e = read_within(&["Keychains/login.txt".into()], &home.join("Library"), &library).unwrap_err().to_string();
        assert!(e.contains("credentials"), "{e}");
        // A person's own `send --file` reads anywhere.
        assert_eq!(read(&[home.join(".ssh/id_ed25519").display().to_string()], &repo).unwrap()[0].content, "key");
        std::fs::remove_dir_all(&d).unwrap();
    }

    #[test]
    fn an_agent_sends_nothing_from_a_folder_that_is_off() {
        // Not in the temp folder, which on Windows is out of reach anyway (see the test above).
        let d =
            Path::new(env!("CARGO_MANIFEST_DIR")).join("target").join(format!("files-test-{}", ulid::Ulid::generate()));
        let (home, repo, state) = (d.join("home"), d.join("home").join("work"), d.join("state"));
        std::fs::create_dir_all(repo.join("private")).unwrap();
        std::fs::create_dir_all(&state).unwrap();
        std::fs::write(repo.join("private").join("notes.md"), "mine").unwrap();
        std::fs::write(repo.join("plan.md"), "plan").unwrap();
        let private = repo.join("private").display().to_string();
        let cfg = crate::config::Config { folders: [(private, "off".to_string())].into(), ..Default::default() };
        let confine = Confine::new(&repo, &home, &state).unwrap().with_folders(cfg);
        let notes = Path::new("private").join("notes.md").display().to_string();
        let e = read_within(&[notes], &repo, &confine).unwrap_err().to_string();
        assert!(e.contains("turned Reason Network off"), "{e}");
        assert_eq!(read_within(&["plan.md".into()], &repo, &confine).unwrap()[0].content, "plan");
        // Nor through a link to it.
        #[cfg(unix)]
        {
            std::os::unix::fs::symlink(repo.join("private").join("notes.md"), repo.join("notes-link.md")).unwrap();
            assert!(read_within(&["notes-link.md".into()], &repo, &confine).is_err());
        }
        std::fs::remove_dir_all(&d).unwrap();
    }

    #[test]
    fn an_agents_shell_sends_nothing_from_a_folder_that_is_off_wherever_it_runs() {
        let d = temp();
        let (private, elsewhere) = (d.join("private"), d.join("elsewhere"));
        std::fs::create_dir_all(&private).unwrap();
        std::fs::create_dir_all(&elsewhere).unwrap();
        std::fs::write(private.join("notes.txt"), "mine").unwrap();
        std::fs::write(d.join("plan.md"), "plan").unwrap();
        let cfg = crate::config::Config {
            folders: [(private.display().to_string(), "off".to_string())].into(),
            ..Default::default()
        };
        // By its full path, from another folder, or by a path relative to that one.
        let full = private.join("notes.txt").display().to_string();
        let relative = Path::new("..").join("private").join("notes.txt").display().to_string();
        for p in [full.clone(), relative] {
            let e = read_outside(std::slice::from_ref(&p), &elsewhere, &cfg).unwrap_err().to_string();
            assert!(e.contains("turned Reason Network off"), "{p}: {e}");
        }
        let plan = d.join("plan.md").display().to_string();
        assert_eq!(read_outside(&[plan], &elsewhere, &cfg).unwrap()[0].content, "plan");
        // A person's own `send --file` reads anywhere.
        assert_eq!(read(&[full], &elsewhere).unwrap()[0].content, "mine");
        std::fs::remove_dir_all(&d).unwrap();
    }

    /// Without folders that are off, an agent's shell reads files just as a person's terminal does.
    #[test]
    fn with_no_folder_off_an_agents_shell_reads_as_a_person_does() {
        let d = temp();
        std::fs::write(d.join("2026-10-20.log"), "log").unwrap();
        let none = crate::config::Config::default();
        let missing = read_outside(&["nope.txt".into()], &d, &none).unwrap_err().to_string();
        assert_eq!(missing, read(&["nope.txt".into()], &d).unwrap_err().to_string());
        // A link is sent under its own name.
        #[cfg(unix)]
        {
            std::os::unix::fs::symlink(d.join("2026-10-20.log"), d.join("current.log")).unwrap();
            let sent = read_outside(&["current.log".into()], &d, &none).unwrap();
            assert_eq!((sent[0].name.as_str(), sent[0].content.as_str()), ("current.log", "log"));
        }
        std::fs::remove_dir_all(&d).unwrap();
    }

    #[test]
    fn saving_never_overwrites_and_keeps_every_file() {
        let d = temp();
        let dir = d.join("inbox").join("01FOLDER");
        // NFC and NFD é, and ß/ss: one name to a file system that folds them (macOS's does the first), two elsewhere.
        let files =
            check(vec![file("caf\u{e9}.md", "nfc"), file("cafe\u{301}.md", "nfd"), file("notes.md", "n")]).unwrap();
        let saved = save(&dir, &files).unwrap();
        let names: Vec<&str> = saved.iter().map(|f| f.name.as_str()).collect();
        assert_eq!(names.len(), 3);
        let on_disk: usize = std::fs::read_dir(&dir).unwrap().count();
        assert_eq!(on_disk, 3, "{names:?}");
        for (f, text) in saved.iter().zip(["nfc", "nfd", "n"]) {
            assert_eq!(std::fs::read_to_string(dir.join(&f.name)).unwrap(), text);
        }
        // A file already there under a name gets the next one, as a folding file system would make happen.
        std::fs::write(dir.join("x.md"), "first").unwrap();
        assert_eq!(save_one(&dir, &file("x.md", "second")).unwrap(), "x-2.md");
        assert_eq!(std::fs::read_to_string(dir.join("x.md")).unwrap(), "first");
        // An existing folder is never written into or removed, and errors name no paths.
        let e = save(&dir, &files).unwrap_err().to_string();
        assert!(!e.contains(&*d.display().to_string()), "{e}");
        assert_eq!(std::fs::read_to_string(dir.join("x.md")).unwrap(), "first");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            // An inbox folder left open is closed up again.
            let other = d.join("inbox").join("01OTHER");
            std::fs::set_permissions(d.join("inbox"), std::fs::Permissions::from_mode(0o755)).unwrap();
            save(&other, &files).unwrap();
            assert_eq!(std::fs::metadata(d.join("inbox")).unwrap().permissions().mode() & 0o777, 0o700);
        }
        #[cfg(target_os = "macos")]
        {
            use std::os::unix::ffi::OsStrExt;
            let p = std::ffi::CString::new(dir.join("notes.md").as_os_str().as_bytes()).unwrap();
            let mut buf = [0u8; 64];
            // SAFETY: NUL-terminated names and a buffer of the length passed.
            let n = unsafe {
                libc::getxattr(p.as_ptr(), c"com.apple.quarantine".as_ptr(), buf.as_mut_ptr().cast(), buf.len(), 0, 0)
            };
            assert!(n > 0 && buf[..n as usize].starts_with(b"0081;"), "no quarantine mark");
        }
        std::fs::remove_dir_all(&d).unwrap();
    }

    #[test]
    fn the_quota_holds_per_sender_and_in_all() {
        let mut q = Quota::default();
        let t = Instant::now();
        let mb = 1 << 20;
        assert!(q.admit("bob", 15 * mb, 0, 0, t).is_ok());
        let e = q.admit("bob", 6 * mb, 15 * mb, 15 * mb, t).unwrap_err();
        assert!(e.contains("20 MB of files an hour from bob"), "{e}");
        // Another teammate has their own hour; an hour later bob has his again.
        assert!(q.admit("alice", 6 * mb, 15 * mb, 0, t).is_ok());
        assert!(q.admit("bob", 6 * mb, 21 * mb, 15 * mb, t + Duration::from_secs(3601)).is_ok());
        let e = q.admit("carol", mb, QUOTA_TOTAL, 0, t).unwrap_err();
        assert!(e.contains("100 MB of teammates' files at most"), "{e}");
        // Nor does one teammate keep more than 25 MB of it, however slowly sent.
        let e = q.admit("bob", 5 * mb, 21 * mb, 21 * mb, t + Duration::from_secs(7300)).unwrap_err();
        assert!(e.contains("25 MB of files from bob at most"), "{e}");
        assert!(q.admit("dave", 5 * mb, 21 * mb, 0, t + Duration::from_secs(7300)).is_ok());
    }

    #[test]
    fn inlined_files_keep_their_fences_and_fit_or_dont() {
        let files = vec![file("a.md", "```rust\nfn x() {}\n```\n"), file("b.txt", "plain")];
        let out = inline("see these", &files, "bob/api", 64 * 1024).unwrap();
        assert!(out.starts_with("see these\n\n(bob/api runs an older Reason Network"), "{out}");
        assert!(out.contains("a.md:\n````\n```rust\nfn x() {}\n```\n````"), "{out}");
        assert!(out.ends_with("b.txt:\n```\nplain\n```"), "{out}");
        assert_eq!(inline("x", &[file("a", &"y".repeat(100))], "bob/api", 100), None);
    }
}
