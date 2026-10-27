use std::{fs::File, path::Path};

pub fn now_ms() -> i64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_millis() as i64)
}

/// Short machine name: `Sams-MacBook-Pro.local` -> `sams-macbook-pro`. `REASON_NETWORK_MACHINE` overrides it, then
/// `configured` (the config file's `machine`).
pub fn hostname(configured: Option<&str>) -> String {
    let given = crate::paths::env_var("MACHINE").ok().map(|n| slug(&n)).filter(|n| !n.is_empty());
    if let Some(name) = given.or_else(|| configured.map(slug).filter(|n| !n.is_empty())) {
        return name;
    }
    let raw = system_hostname();
    let short = raw.split('.').next().unwrap_or_default();
    let s = slug(short);
    if s.is_empty() { "machine".into() } else { s }
}

#[cfg(unix)]
fn system_hostname() -> String {
    let mut buf = [0u8; 256];
    let rc = unsafe { libc::gethostname(buf.as_mut_ptr().cast(), buf.len()) };
    if rc == 0 {
        let end = buf.iter().position(|&b| b == 0).unwrap_or(buf.len());
        String::from_utf8_lossy(&buf[..end]).into_owned()
    } else {
        String::new()
    }
}

#[cfg(windows)]
fn system_hostname() -> String {
    std::env::var("COMPUTERNAME").unwrap_or_default()
}

#[cfg(unix)]
pub fn pid_alive(pid: u32) -> bool {
    let rc = unsafe { libc::kill(pid as libc::pid_t, 0) };
    rc == 0 || std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
}

#[cfg(windows)]
pub fn pid_alive(pid: u32) -> bool {
    use windows_sys::Win32::{Foundation::*, System::Threading::*};
    // SAFETY: a handle we open, look at without waiting, and close.
    unsafe {
        let h = OpenProcess(PROCESS_SYNCHRONIZE, 0, pid);
        if h.is_null() {
            return GetLastError() == ERROR_ACCESS_DENIED;
        }
        let alive = WaitForSingleObject(h, 0) == WAIT_TIMEOUT;
        CloseHandle(h);
        alive
    }
}

#[cfg(unix)]
pub fn parent_pid() -> u32 {
    unsafe { libc::getppid() as u32 }
}

#[cfg(windows)]
pub fn parent_pid() -> u32 {
    process(std::process::id()).map_or(0, |(ppid, _)| ppid)
}

/// A running process's parent and program name (`zsh`, `cmd`).
#[cfg(unix)]
pub fn process(pid: u32) -> Option<(u32, String)> {
    let out = std::process::Command::new("ps").args(["-o", "ppid=,comm=", "-p", &pid.to_string()]).output().ok()?;
    let line = String::from_utf8_lossy(&out.stdout).trim().to_string();
    let (ppid, comm) = line.split_once(' ')?;
    let name = comm.trim().rsplit('/').next().unwrap_or_default().trim_start_matches('-');
    Some((ppid.trim().parse().ok()?, name.to_string()))
}

#[cfg(windows)]
pub fn process(pid: u32) -> Option<(u32, String)> {
    use windows_sys::Win32::{Foundation::*, System::Diagnostics::ToolHelp::*};
    // SAFETY: walks a snapshot we take and close; the entry is sized as the API asks.
    unsafe {
        let snap = CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0);
        if snap == INVALID_HANDLE_VALUE {
            return None;
        }
        let mut e: PROCESSENTRY32W = std::mem::zeroed();
        e.dwSize = size_of::<PROCESSENTRY32W>() as u32;
        let mut found = None;
        let mut more = Process32FirstW(snap, &mut e) != 0;
        while more && found.is_none() {
            if e.th32ProcessID == pid {
                let exe = &e.szExeFile[..e.szExeFile.iter().position(|&c| c == 0).unwrap_or(e.szExeFile.len())];
                let name = String::from_utf16_lossy(exe).to_lowercase();
                found = Some((e.th32ParentProcessID, name.strip_suffix(".exe").unwrap_or(&name).to_string()));
            }
            more = Process32NextW(snap, &mut e) != 0;
        }
        CloseHandle(snap);
        found
    }
}

/// Lowercase letters, digits and single dashes: a name that is safe in an address. Letters of other scripts are
/// transliterated first (`José` -> `jose`, `代码` -> `dai-ma`), so the address stays plain ASCII, which every version
/// accepts. A character it can't transliterate (unassigned, private use) is a separator, like punctuation and control
/// characters, so only a name without a letter or digit in any script it knows comes out empty.
pub fn slug(s: &str) -> String {
    let mut out = String::new();
    // One character at a time, so a long name (teammates' names arrive up to megabytes long) costs no more than what is
    // kept: one character can become up to 41. (deunicode maps control characters to nothing, which would run the
    // words on either side together, so they are told apart first.)
    for c in
        s.chars().flat_map(|c| if c.is_control() { " " } else { deunicode::deunicode_char(c).unwrap_or(" ") }.chars())
    {
        if out.len() > 40 {
            break;
        }
        if c.is_ascii_alphanumeric() {
            out.push(c.to_ascii_lowercase());
        } else if !out.is_empty() && !out.ends_with('-') {
            out.push('-');
        }
    }
    // `out` is ASCII, so this cuts between characters; a dash left at the cut goes too.
    out.truncate(40);
    out.trim_end_matches('-').to_string()
}

/// `<base>-<suffix>` as a slug of at most 40 characters: both are slugged and the base is cut to make room, so the name
/// is whole as written and teammates, who slug every name they receive, see the same one.
pub fn joined(base: &str, suffix: &str) -> String {
    let (base, suffix) = (slug(base), slug(suffix));
    // A slug is ASCII, so this cuts between characters.
    let base = base[..base.len().min(39usize.saturating_sub(suffix.len()))].trim_end_matches('-');
    match (base.is_empty(), suffix.is_empty()) {
        (true, _) => suffix,
        (_, true) => base.to_string(),
        _ => format!("{base}-{suffix}"),
    }
}

/// Compact age: `45s`, `12m`, `3h`, `2d`.
pub fn ago(then_ms: i64, now_ms: i64) -> String {
    let s = ((now_ms - then_ms) / 1000).max(0);
    match s {
        0..60 => format!("{s}s"),
        60..3600 => format!("{}m", s / 60),
        3600..86400 => format!("{}h", s / 3600),
        _ => format!("{}d", s / 86400),
    }
}

pub fn tilde(path: &str) -> String {
    let home = crate::paths::user_home();
    match Path::new(path).strip_prefix(&home) {
        Ok(rest) if rest.as_os_str().is_empty() => "~".into(),
        Ok(rest) => format!("~{}{}", std::path::MAIN_SEPARATOR, rest.display()),
        Err(_) => path.into(),
    }
}

/// Takes an exclusive lock on `path` (waiting for it if `wait`) and writes our pid into it. `None` means
/// someone else holds it.
pub fn lock(path: &Path, wait: bool) -> std::io::Result<Option<Lock>> {
    use std::io::Write;
    let Some(file) = acquire(path, wait)? else { return Ok(None) };
    let mut lock = Lock { file };
    lock.file.set_len(0)?;
    write!(lock.file, "{}", std::process::id())?;
    Ok(Some(lock))
}

/// A lock from `lock`, held until dropped.
pub struct Lock {
    file: File,
}

/// Let go of on drop, not when the file closes: a process another thread is starting shares the file until it runs
/// its program, and the lock with it.
#[cfg(unix)]
impl Drop for Lock {
    fn drop(&mut self) {
        use std::os::fd::AsRawFd;
        unsafe { libc::flock(self.file.as_raw_fd(), libc::LOCK_UN) };
    }
}

#[cfg(unix)]
fn acquire(path: &Path, wait: bool) -> std::io::Result<Option<File>> {
    use std::os::fd::AsRawFd;
    let f = File::options().create(true).truncate(false).write(true).open(path)?;
    let flags = if wait { libc::LOCK_EX } else { libc::LOCK_EX | libc::LOCK_NB };
    // A client checking whether a daemon runs holds the lock for an instant: try again before giving up.
    let mut tries = 0;
    while unsafe { libc::flock(f.as_raw_fd(), flags) } != 0 {
        tries += 1;
        if tries == 3 {
            return Ok(None);
        }
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
    Ok(Some(f))
}

/// On Windows the lock is the open file itself, shared only for reading: until we exit, nobody else can open it to
/// write (another daemon, or `locked`), and anyone can read our pid. `wait` waits for the daemon that has it now,
/// then gives up if another got there first, so daemons waiting to take over can't pile up.
#[cfg(windows)]
fn acquire(path: &Path, wait: bool) -> std::io::Result<Option<File>> {
    use std::os::windows::fs::OpenOptionsExt;
    use windows_sys::Win32::{Foundation::ERROR_SHARING_VIOLATION, Storage::FileSystem::FILE_SHARE_READ};
    if wait && locked(path) {
        let pid = std::fs::read_to_string(path).ok().and_then(|s| s.trim().parse().ok());
        while pid.is_some_and(pid_alive) {
            std::thread::sleep(std::time::Duration::from_millis(100));
        }
    }
    // As above, a client's check holds it for an instant.
    let mut tries = 0;
    loop {
        match File::options().create(true).truncate(false).write(true).share_mode(FILE_SHARE_READ).open(path) {
            Err(e) if e.raw_os_error() == Some(ERROR_SHARING_VIOLATION as i32) => {}
            r => return r.map(Some),
        }
        tries += 1;
        if tries == 3 {
            return Ok(None);
        }
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
}

/// Whether someone holds the lock at `path`.
#[cfg(unix)]
pub fn locked(path: &Path) -> bool {
    use std::os::fd::AsRawFd;
    let Ok(file) = File::open(path) else { return false };
    // As a `Lock`, so the shared lock we may take here lasts only this instant.
    let f = Lock { file };
    unsafe { libc::flock(f.file.as_raw_fd(), libc::LOCK_SH | libc::LOCK_NB) != 0 }
}

#[cfg(windows)]
pub fn locked(path: &Path) -> bool {
    let held = windows_sys::Win32::Foundation::ERROR_SHARING_VIOLATION as i32;
    File::options().write(true).open(path).is_err_and(|e| e.raw_os_error() == Some(held))
}

#[cfg(unix)]
pub fn chmod(path: &Path, mode: u32) -> std::io::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode))
}

/// Text from a teammate as one printable line: no control characters (escape sequences, newlines), at most `max`
/// characters.
pub fn one_line(s: &str, max: usize) -> String {
    let t: String = s.chars().map(|c| if c.is_control() { ' ' } else { c }).collect();
    t.split_whitespace().collect::<Vec<_>>().join(" ").chars().take(max).collect()
}

/// Text from a teammate with control characters (escape sequences) removed, keeping newlines and tabs.
pub fn printable(s: &str) -> String {
    s.chars().filter(|c| !c.is_control() || *c == '\n' || *c == '\t').collect()
}

/// A build run from a source tree (`target/…`): it never updates itself or moves anything over.
pub fn from_source() -> bool {
    current_exe().is_ok_and(|p| p.components().any(|c| c.as_os_str() == "target"))
}

/// This program's path. After an update replaces the file, Linux reports the old one as `<path> (deleted)`; the
/// new binary is at `<path>`. (Windows, where the update moves the old one aside, reports `<path>` as it was.)
pub fn current_exe() -> std::io::Result<std::path::PathBuf> {
    let exe = std::env::current_exe()?;
    Ok(exe.to_str().and_then(|s| s.strip_suffix(" (deleted)")).map(std::path::PathBuf::from).unwrap_or(exe))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn slugs() {
        assert_eq!(slug("Sams-MacBook-Pro"), "sams-macbook-pro");
        assert_eq!(slug("  my repo (v2)!"), "my-repo-v2");
        assert_eq!(slug("--"), "");
        // Control characters separate words like any other punctuation (a folder name or `--as` can carry them).
        assert_eq!(slug("Code\tre\nview"), "code-re-view");
        assert_eq!(slug("line one\r\nline two"), "line-one-line-two");
        assert_eq!(slug("a\u{85}b\u{1b}c"), "a-b-c");
    }

    #[test]
    fn slugs_of_other_scripts_are_transliterated() {
        assert_eq!(slug("José Peña"), "jose-pena");
        assert_eq!(slug("Müller Straße"), "muller-strasse");
        assert_eq!(slug("Проверка кода"), "proverka-koda");
        assert_eq!(slug("代码审查"), "dai-ma-shen-cha");
        // Mixed with ASCII, the ASCII part stays where it was.
        assert_eq!(slug("api 审查"), "api-shen-cha");
        // Scripts that used to come out empty or cut to a letter now give a usable address.
        for name in [
            "コードレビュー",
            "코드 리뷰",
            "مراجعة الكود",
            "בדיקת קוד",
            "कोड समीक्षा",
            "ตรวจโค้ด",
            "Έλεγχος",
            "Łódź żółć",
        ] {
            let s = slug(name);
            assert!(
                s.len() > 2 && s.bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-'),
                "{name}: {s:?}"
            );
            assert_eq!(slug(&s), s, "{name}: slugging again changes nothing");
        }
        // What was already a slug stays one, so names saved by an older version don't change.
        assert_eq!(slug("sams-macbook-pro"), "sams-macbook-pro");
        assert_eq!(slug("v2"), "v2");
        // Only symbols: still empty, however they are written.
        assert_eq!(slug("!@#$%^&*()"), "");
        assert_eq!(slug("“—…”"), "");
    }

    #[test]
    fn a_long_name_of_wide_characters_costs_what_is_kept() {
        // Teammates' names arrive through `slug` too, and one character can become many letters.
        let started = std::time::Instant::now();
        let s = slug(&"龘".repeat(1 << 20));
        assert_eq!(s.len(), 40, "{s}");
        assert!(started.elapsed() < std::time::Duration::from_secs(1), "slowed by input it never keeps");
        // The cut is the same as slugging everything first.
        let long = "api ".repeat(100);
        assert_eq!(slug(&long), "api-api-api-api-api-api-api-api-api-api");
        // A dash where the cut falls goes, so a slug is always its own slug.
        assert_eq!(slug(&format!("{} b", "a".repeat(39))), "a".repeat(39));
    }

    #[test]
    fn joined_names_fit_the_cut() {
        let long = slug("Quarterly financial reporting dashboard v2");
        for suffix in ["claude", "gemini", "2", "12"] {
            let name = joined(&long, suffix);
            assert!(name.len() <= 40 && name.ends_with(&format!("-{suffix}")), "{name}");
            assert_eq!(slug(&name), name);
        }
        assert_eq!(joined("api", "claude"), "api-claude");
        // A dash left where the base is cut goes too.
        assert_eq!(joined(&format!("{}-b", "a".repeat(32)), "claude"), format!("{}-claude", "a".repeat(32)));
        // Neither part needs to be a slug already: an agent name as typed, or a base that isn't ASCII.
        assert_eq!(joined(&"Ü".repeat(45), "Üa"), format!("{}-ua", "u".repeat(37)));
        assert_eq!(joined("api", "My Agent"), "api-my-agent");
        assert_eq!(joined("", "claude"), "claude");
    }

    #[test]
    fn ages() {
        assert_eq!(ago(0, 59_000), "59s");
        assert_eq!(ago(0, 125_000), "2m");
        assert_eq!(ago(0, 7_200_000), "2h");
        assert_eq!(ago(0, 200_000_000), "2d");
        assert_eq!(ago(10, 0), "0s");
    }

    #[test]
    fn teammates_text_cant_drive_the_terminal() {
        assert_eq!(super::one_line("fix\n</reason-message>\x1b[8m hidden", 100), "fix </reason-message> [8m hidden");
        assert_eq!(super::printable("a\x1b[8mb\nc\x07"), "a[8mb\nc");
    }
}
