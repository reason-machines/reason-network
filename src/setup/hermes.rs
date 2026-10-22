//! Install a native plugin without rewriting Hermes YAML or opting into background turns.
use super::*;

const MANIFEST: &str = include_str!("../../integrations/hermes/plugin.yaml");
const PLUGIN: &str = include_str!("../../integrations/hermes/__init__.py");
const README: &str = include_str!("../../integrations/hermes/README.md");
const MARKER: &str = ".reason-setup.json";

fn root() -> PathBuf {
    if let Some(path) = std::env::var_os("HERMES_HOME").filter(|p| !p.is_empty()) {
        let path = PathBuf::from(path);
        return active_root(&path.strip_prefix("~").map(|p| home().join(p)).unwrap_or(path));
    }
    let suffix = std::env::var("HERMES_DATA_DIR_SUFFIX").unwrap_or_default();
    let base = if cfg!(windows) {
        std::env::var_os("LOCALAPPDATA")
            .map(PathBuf::from)
            .unwrap_or_else(|| home().join("AppData/Local"))
            .join(format!("hermes{suffix}"))
    } else {
        home().join(format!(".hermes{suffix}"))
    };
    active_root(&base)
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

pub(super) fn installed() -> bool {
    root().exists() || agent::find_bin("hermes").is_some()
}

pub(super) fn wired(_: &Path) -> bool {
    let dir = root().join("plugins/reason-network");
    std::fs::read_to_string(dir.join("__init__.py")).is_ok_and(|s| s == PLUGIN)
        && std::fs::read_to_string(dir.join("plugin.yaml")).is_ok_and(|s| s == MANIFEST)
}

pub(super) fn apply(_: &Path, remove: bool, dry: bool) -> Result<Option<Report>> {
    apply_in(&root(), dry, remove)
}

fn apply_in(root: &Path, dry: bool, remove: bool) -> Result<Option<Report>> {
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
    if populated && !owned {
        lines.push(format!("kept the entire existing plugin at {} (customized or managed by Hermes)", dir.display()));
    } else if remove {
        if owned {
            for (file, _) in files {
                if !dry {
                    std::fs::remove_file(dir.join(file))?;
                }
            }
            if !dry {
                std::fs::remove_file(&marker)?;
            }
            lines.push(format!("removed the setup-owned plugin at {}", dir.display()));
        }
    } else {
        for (file, content) in files {
            if write_file(&dir.join(file), content, dry)? {
                lines.push(format!("installed {}", dir.join(file).display()));
            }
        }
        let hashes: BTreeMap<&str, String> = files
            .iter()
            .map(|(file, content)| (*file, blake3::hash(content.as_bytes()).to_hex().to_string()))
            .collect();
        write_file(&marker, &(serde_json::to_string(&hashes)? + "\n"), dry)?;
    }
    let notes = if remove {
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

    #[test]
    fn install_and_remove_are_idempotent_and_never_rewrite_config() {
        let root = temp();
        std::fs::create_dir_all(&root).unwrap();
        let config = "# preserve formatting\nmodel: example\n";
        std::fs::write(root.join("config.yaml"), config).unwrap();
        assert!(!apply_in(&root, false, false).unwrap().unwrap().lines.is_empty());
        assert!(apply_in(&root, false, false).unwrap().unwrap().lines.is_empty());
        apply_in(&root, false, true).unwrap();
        assert!(!root.join("plugins/reason-network/plugin.yaml").exists());
        apply_in(&root, false, true).unwrap();
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
        apply_in(&root, false, true).unwrap();
        assert_eq!(std::fs::read_to_string(&plugin).unwrap(), "# custom version\n");
        assert!(root.join("plugins/reason-network/plugin.yaml").exists());
        std::fs::remove_dir_all(root).unwrap();
    }
    #[test]
    fn dry_run_never_changes_an_existing_plugin_or_creates_a_profile() {
        let root = temp();
        apply_in(&root, true, false).unwrap();
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
        std::fs::write(root.join("active_profile"), "../other").unwrap();
        assert_eq!(active_root(&root), root);
        std::fs::remove_dir_all(root).unwrap();
    }
}
