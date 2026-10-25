//! Install a native plugin without rewriting Hermes YAML or opting into background turns.
use super::*;

const MANIFEST: &str = include_str!("../../integrations/hermes/plugin.yaml");
const PLUGIN: &str = include_str!("../../integrations/hermes/__init__.py");
const README: &str = include_str!("../../integrations/hermes/README.md");
const MARKER: &str = ".reason-setup.json";

#[cfg(test)]
thread_local! {
    /// A test's Hermes home, so setup's own dispatch can be run against it.
    static TEST_HOME: std::cell::RefCell<Option<PathBuf>> = const { std::cell::RefCell::new(None) };
}

/// The Hermes home, before a sticky profile is chosen.
fn base() -> PathBuf {
    #[cfg(test)]
    if let Some(home) = TEST_HOME.with(|h| h.borrow().clone()) {
        return home;
    }
    if let Some(path) = std::env::var_os("HERMES_HOME").filter(|p| !p.is_empty()) {
        let path = PathBuf::from(path);
        return path.strip_prefix("~").map(|p| home().join(p)).unwrap_or(path);
    }
    let suffix = std::env::var("HERMES_DATA_DIR_SUFFIX").unwrap_or_default();
    if cfg!(windows) {
        std::env::var_os("LOCALAPPDATA")
            .map(PathBuf::from)
            .unwrap_or_else(|| home().join("AppData/Local"))
            .join(format!("hermes{suffix}"))
    } else {
        home().join(format!(".hermes{suffix}"))
    }
}

fn root() -> PathBuf {
    active_root(&base())
}

/// Where this Hermes's own code is: its installer's checkout, else what `hermes --version` reports.
fn source() -> Option<PathBuf> {
    let bundled = base().join("hermes-agent");
    if bundled.join("hermes_cli").is_dir() {
        return Some(bundled);
    }
    let mut version = Command::new(agent::find_bin("hermes")?);
    version.arg("--version");
    install_dir(version)
}

/// The `Install directory:` a Hermes command reports, waiting at most a few seconds for it.
fn install_dir(mut command: Command) -> Option<PathBuf> {
    command.stdin(std::process::Stdio::null()).stdout(std::process::Stdio::piped()).stderr(std::process::Stdio::null());
    // In its own process group, so giving up on it ends what it started too (a launcher's Python, a shell's `sleep`).
    #[cfg(unix)]
    std::os::unix::process::CommandExt::process_group(&mut command, 0);
    let mut child = command.spawn().ok()?;
    let stdout = child.stdout.take()?;
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let found = std::io::BufRead::lines(std::io::BufReader::new(stdout))
            .map_while(Result::ok)
            .find_map(|l| l.trim().strip_prefix("Install directory:").map(|p| PathBuf::from(p.trim())));
        let _ = tx.send(found);
    });
    // A Hermes that hangs on `--version` (or leaves something holding its output) mustn't hang setup, init and join.
    let found = rx.recv_timeout(std::time::Duration::from_secs(5)).ok().flatten();
    kill_tree(&mut child);
    let _ = child.wait();
    found
}

/// Ends `child` and everything it started: its process group on Unix, its process tree on Windows.
fn kill_tree(child: &mut std::process::Child) {
    #[cfg(unix)]
    // SAFETY: signals the process group `install_dir` created for this child, which isn't reaped yet.
    unsafe {
        libc::killpg(child.id() as libc::pid_t, libc::SIGKILL);
    }
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        let taskkill = std::env::var_os("SystemRoot")
            .map_or_else(|| PathBuf::from("taskkill.exe"), |r| PathBuf::from(r).join(r"System32\taskkill.exe"));
        let _ = Command::new(taskkill)
            .args(["/T", "/F", "/PID", &child.id().to_string()])
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .creation_flags(0x0800_0000)
            .status();
    }
    let _ = child.kill();
}

/// The plugin context methods the plugin needs (NousResearch/hermes-agent#143310), wherever in `hermes_cli` they are.
const APIS: [&str; 3] = ["def session_key(self", "def is_interrupted(self", "def session_supports_async_delivery(self"];

fn has_apis(source: &Path) -> bool {
    let Ok(files) = std::fs::read_dir(source.join("hermes_cli")) else { return false };
    let code: String = files
        .flatten()
        .map(|f| f.path())
        .filter(|p| p.extension().is_some_and(|e| e == "py"))
        .filter_map(|p| std::fs::read_to_string(p).ok())
        .collect();
    APIS.iter().all(|api| code.contains(api))
}

fn supported() -> bool {
    source().is_some_and(|s| has_apis(&s))
}

fn active_root(base: &Path) -> PathBuf {
    let active = std::fs::read_to_string(base.join("active_profile")).unwrap_or_default();
    let active = active.trim();
    if !active.is_empty() && active != "default" && active != "." && active != ".." && !active.contains(['/', '\\']) {
        let profile = base.join("profiles").join(active);
        if profile.is_dir() {
            return profile;
        }
    }
    base.to_path_buf()
}

/// A Hermes that can run the plugin, or one it is already in (so it can be updated or removed).
pub(super) fn installed() -> bool {
    (root().exists() || agent::find_bin("hermes").is_some())
        && (root().join("plugins/reason-network").exists() || supported())
}

pub(super) fn wired(_: &Path) -> bool {
    let dir = root().join("plugins/reason-network");
    std::fs::read_to_string(dir.join("__init__.py")).is_ok_and(|s| s == PLUGIN)
        && std::fs::read_to_string(dir.join("plugin.yaml")).is_ok_and(|s| s == MANIFEST)
}

pub(super) fn apply(_: &Path, remove: bool, dry: bool) -> Result<Option<Report>> {
    if !remove && !supported() {
        return Err(anyhow!(
            "this Hermes doesn't have the plugin APIs the native plugin needs yet (NousResearch/hermes-agent#143310); \
             Reason's MCP server works with it meanwhile"
        ));
    }
    apply_in(&root(), remove, dry)
}

/// Takes `remove, dry` in the order setup passes them to every agent: a dry run swapped for a removal deletes.
fn apply_in(root: &Path, remove: bool, dry: bool) -> Result<Option<Report>> {
    let dir = root.join("plugins/reason-network");
    let files = [("plugin.yaml", MANIFEST), ("__init__.py", PLUGIN), ("README.md", README)];
    let mut lines = Vec::new();
    let marker = dir.join(MARKER);
    let saved =
        std::fs::read_to_string(&marker).ok().and_then(|s| serde_json::from_str::<BTreeMap<String, String>>(&s).ok());
    let owned = saved.as_ref().is_some_and(|saved| {
        files.iter().all(|(file, _)| {
            std::fs::read(dir.join(file))
                .is_ok_and(|bytes| saved.get(*file) == Some(&blake3::hash(&bytes).to_hex().to_string()))
        })
    });
    let populated =
        files.iter().any(|(file, _)| dir.join(file).exists()) || marker.exists() || dir.join(".git").exists();
    let shown = crate::util::tilde(&dir.display().to_string());
    if populated && !owned {
        lines.push(format!("kept the existing plugin at {shown} (customized or managed by Hermes)"));
        return Ok(Some(Report { agent: "Hermes", lines, notes: Vec::new() }));
    }
    if remove {
        if owned {
            for (file, _) in files {
                if !dry {
                    std::fs::remove_file(dir.join(file))?;
                }
            }
            if !dry {
                std::fs::remove_file(&marker)?;
            }
            lines.push(format!("removed the plugin at {shown}"));
        }
    } else {
        let mut wrote = false;
        for (file, content) in files {
            wrote |= write_file(&dir.join(file), content, dry)?;
        }
        if wrote {
            lines.push(format!("installed the plugin at {shown}"));
        }
        let hashes: BTreeMap<&str, String> = files
            .iter()
            .map(|(file, content)| (*file, blake3::hash(content.as_bytes()).to_hex().to_string()))
            .collect();
        write_file(&marker, &(serde_json::to_string(&hashes)? + "\n"), dry)?;
    }
    // Said once, when something changed; a run that changes nothing says so, as the other agents do.
    let notes = if lines.is_empty() {
        lines.push(changed(&dir, false));
        Vec::new()
    } else if remove {
        vec!["disable the plugin in Hermes if it was enabled".into()]
    } else {
        vec![
            format!(
                "plugin profile: {}; enable with hermes plugins enable reason-network in this profile",
                root.display()
            ),
            "restart Hermes afterward; repeat with HERMES_HOME set for each additional profile".into(),
            "wake is off until configured; use the native plugin instead of a duplicate MCP entry".into(),
        ]
    };
    Ok(Some(Report { agent: "Hermes", lines, notes }))
}

use std::collections::BTreeMap;

#[cfg(test)]
mod tests {
    use super::*;
    fn temp() -> PathBuf {
        std::env::temp_dir().join(format!("reason-hermes-{}", ulid::Ulid::generate()))
    }

    #[cfg(unix)]
    #[test]
    fn a_hermes_that_hangs_on_version_does_not_hang_setup() {
        let sh = |script: &str| {
            let mut c = Command::new("sh");
            c.args(["-c", script]);
            c
        };
        assert_eq!(install_dir(sh("echo 'Install directory: /opt/hermes'")), Some(PathBuf::from("/opt/hermes")));
        // The line is used as soon as it's there, and what's still running is ended, including its children.
        let pid = temp();
        let started = std::time::Instant::now();
        let script = format!("sleep 30 & echo $! > {}; echo 'Install directory: /opt/hermes'; wait", pid.display());
        assert_eq!(install_dir(sh(&script)), Some(PathBuf::from("/opt/hermes")));
        assert!(started.elapsed() < std::time::Duration::from_secs(4), "{:?}", started.elapsed());
        let gone = |pid: &PathBuf| {
            let sleep: libc::pid_t = std::fs::read_to_string(pid).unwrap().trim().parse().unwrap();
            // SAFETY: signal 0 only checks that the process exists.
            (0..50).any(|_| {
                let alive = unsafe { libc::kill(sleep, 0) } == 0;
                if alive {
                    std::thread::sleep(std::time::Duration::from_millis(100));
                }
                !alive
            })
        };
        assert!(gone(&pid), "the sleep outlived the probe");
        // Nothing said: given up on after a few seconds, and ended the same way.
        let started = std::time::Instant::now();
        let script = format!("sleep 30 & echo $! > {}; wait", pid.display());
        assert_eq!(install_dir(sh(&script)), None);
        assert!(started.elapsed() < std::time::Duration::from_secs(15), "{:?}", started.elapsed());
        assert!(gone(&pid), "the sleep outlived the probe");
        let _ = std::fs::remove_file(&pid);
    }

    /// On Windows a pip or uv `hermes.exe` launcher runs Python as its child: ending the launcher alone would leave it.
    #[cfg(windows)]
    #[test]
    fn giving_up_ends_what_the_command_started() {
        let mut child = Command::new("cmd")
            .args(["/C", "ping -n 60 127.0.0.1"])
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::piped())
            .spawn()
            .unwrap();
        let mut stdout = child.stdout.take().unwrap();
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let _ = std::io::Read::read_to_end(&mut stdout, &mut Vec::new());
            let _ = tx.send(());
        });
        std::thread::sleep(std::time::Duration::from_millis(500));
        kill_tree(&mut child);
        let _ = child.wait();
        // `ping` holds the output open until it's gone too.
        assert!(rx.recv_timeout(std::time::Duration::from_secs(10)).is_ok(), "ping outlived cmd");
    }

    #[test]
    fn install_and_remove_are_idempotent_and_never_rewrite_config() {
        let root = temp();
        std::fs::create_dir_all(&root).unwrap();
        let config = "# preserve formatting\nmodel: example\n";
        std::fs::write(root.join("config.yaml"), config).unwrap();
        let first = apply_in(&root, false, false).unwrap().unwrap();
        assert!(first.lines[0].starts_with("installed") && !first.notes.is_empty());
        // Again: one line saying so, and the notes aren't repeated.
        let again = apply_in(&root, false, false).unwrap().unwrap();
        assert!(again.lines.len() == 1 && again.lines[0].ends_with("unchanged"), "{:?}", again.lines);
        assert!(again.notes.is_empty());
        apply_in(&root, true, false).unwrap();
        assert!(!root.join("plugins/reason-network/plugin.yaml").exists());
        apply_in(&root, true, false).unwrap();
        assert_eq!(std::fs::read_to_string(root.join("config.yaml")).unwrap(), config);
        std::fs::remove_dir_all(root).unwrap();
    }
    #[test]
    fn a_customized_plugin_is_kept_whole_including_its_manifest() {
        let root = temp();
        apply_in(&root, false, false).unwrap();
        let plugin = root.join("plugins/reason-network/__init__.py");
        std::fs::write(&plugin, "# custom version\n").unwrap();
        apply_in(&root, false, false).unwrap();
        apply_in(&root, true, false).unwrap();
        assert_eq!(std::fs::read_to_string(&plugin).unwrap(), "# custom version\n");
        assert!(root.join("plugins/reason-network/plugin.yaml").exists());
        std::fs::remove_dir_all(root).unwrap();
    }
    #[test]
    fn dry_run_never_changes_an_existing_plugin_or_creates_a_profile() {
        let root = temp();
        apply_in(&root, false, true).unwrap();
        assert!(!root.exists());
        apply_in(&root, false, false).unwrap();
        apply_in(&root, true, true).unwrap();
        assert!(root.join("plugins/reason-network/plugin.yaml").exists());
        std::fs::remove_dir_all(root).unwrap();
    }
    #[test]
    fn active_named_profiles_are_selected_without_allowing_path_traversal() {
        let root = temp();
        let profile = root.join("profiles/work");
        std::fs::create_dir_all(&profile).unwrap();
        std::fs::write(root.join("active_profile"), "work\n").unwrap();
        assert_eq!(active_root(&root), profile);
        // A name that leads out of `profiles` is refused, even where it leads to a real folder.
        std::fs::create_dir_all(root.join("other")).unwrap();
        std::fs::write(root.join("active_profile"), "../other").unwrap();
        assert_eq!(active_root(&root), root);
        std::fs::write(root.join("active_profile"), "..").unwrap();
        assert_eq!(active_root(&root), root);
        std::fs::remove_dir_all(root).unwrap();
    }
    #[tokio::test]
    async fn setup_dry_runs_change_nothing_and_its_flags_reach_hermes_the_right_way_round() {
        let home = temp();
        // A Hermes with the plugin APIs, as its installer lays it out.
        let cli = home.join("hermes-agent/hermes_cli");
        std::fs::create_dir_all(&cli).unwrap();
        std::fs::write(cli.join("plugin_session.py"), APIS.map(|a| format!("{a}): ...\n")).concat()).unwrap();
        TEST_HOME.with(|h| *h.borrow_mut() = Some(home.clone()));
        let plugin = home.join("plugins/reason-network/__init__.py");
        let (exe, hermes) = (Path::new("/reason-network"), &["hermes"][..]);
        // Through setup's own dispatch, as `setup`, `--dry-run` and `--remove` run it.
        super::super::apply(exe, hermes, &[], None, true).await;
        assert!(!plugin.exists(), "a dry run installed");
        super::super::apply(exe, hermes, &[], None, false).await;
        assert!(plugin.exists());
        super::super::apply(exe, &[], hermes, None, true).await;
        assert!(plugin.exists(), "a dry run removed");
        super::super::apply(exe, &[], hermes, None, false).await;
        assert!(!plugin.exists());
        TEST_HOME.with(|h| h.borrow_mut().take());
        std::fs::remove_dir_all(home).unwrap();
    }

    #[test]
    fn only_a_hermes_with_the_plugin_apis_counts_as_supported() {
        let source = temp();
        let cli = source.join("hermes_cli");
        std::fs::create_dir_all(&cli).unwrap();
        // Released Hermes: session_cwd and session_surface, but no routing key or cancellation.
        std::fs::write(cli.join("plugins.py"), "class PluginContext:\n    def session_cwd(self) -> str: ...\n")
            .unwrap();
        assert!(!has_apis(&source));
        // With the APIs, in whichever module they live.
        std::fs::write(
            cli.join("plugin_session.py"),
            "class M:\n    def session_key(self) -> str: ...\n    def is_interrupted(self) -> bool: ...\n    \
             def session_supports_async_delivery(self) -> bool: ...\n",
        )
        .unwrap();
        assert!(has_apis(&source));
        assert!(!has_apis(&source.join("missing")));
        std::fs::remove_dir_all(source).unwrap();
    }
}
