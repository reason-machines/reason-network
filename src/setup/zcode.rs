//! ZCode Desktop (https://zcode.z.ai/en/docs/mcp-services). Its user MCP servers live in `~/.zcode/cli/config.json`
//! (`mcp.servers`); while that has none, it reads `~/.agents/mcp.json` (`mcpServers`) instead, so a server added to the
//! first would hide every server in the second. Workspace servers can't mask ours: a user server wins on a name clash.

use super::*;

pub(super) fn installed() -> bool {
    let dir = home().join(".zcode");
    dir.join("cli").is_dir() || dir.join("v2").is_dir()
}

pub(super) fn wired(exe: &Path) -> bool {
    wired_in(&home(), exe)
}

/// Whether ours is in the file ZCode reads: its own while that has any server, else the `.agents` one.
fn wired_in(home: &Path, exe: &Path) -> bool {
    let read = |(path, native): (PathBuf, bool)| -> Result<Map<String, Value>> {
        Ok(servers(&mut read_json(&path)?, native)?.clone())
    };
    let [native, fallback] = files(home);
    let servers = match read(native) {
        Ok(s) if s.is_empty() => read(fallback),
        s => s,
    };
    servers.is_ok_and(|s| s.get(AGENT_NAME).is_some_and(|e| contains(&e.to_string(), &me(exe))))
}

pub(super) fn apply(exe: &Path, remove: bool, dry: bool) -> Result<Option<Report>> {
    if !installed() {
        return Ok(None);
    }
    apply_in(&home(), exe, remove, dry).map(Some)
}

/// ZCode's own file, then the `.agents` one; `true` for ZCode's own.
fn files(home: &Path) -> [(PathBuf, bool); 2] {
    [(home.join(".zcode/cli/config.json"), true), (home.join(".agents/mcp.json"), false)]
}

fn servers(v: &mut Value, native: bool) -> Result<&mut Map<String, Value>> {
    let root = obj(v)?;
    let mcp = if native { obj(root.entry("mcp").or_insert_with(|| json!({})))? } else { root };
    obj(mcp.entry(if native { "servers" } else { "mcpServers" }).or_insert_with(|| json!({})))
}

fn apply_in(home: &Path, exe: &Path, remove: bool, dry: bool) -> Result<Report> {
    let mut r = Report { agent: "ZCode Desktop", lines: Vec::new(), notes: Vec::new() };
    let [native, fallback] = files(home);
    // Our own entry doesn't count: ZCode should go on reading `.agents` when ours is all its own file has.
    let to_fallback = !remove
        && !servers(&mut read_json(&native.0)?, true)?.keys().any(|k| k != AGENT_NAME)
        && !servers(&mut read_json(&fallback.0)?, false)?.is_empty();
    // Removing takes ours out of both: which one ZCode reads may have changed since. Moving writes `.agents` first,
    // so a failed write there leaves ours where it was.
    let targets = if remove {
        vec![native, fallback]
    } else if to_fallback {
        vec![fallback, native]
    } else {
        vec![native]
    };
    for (path, native) in targets {
        let mut v = read_json(&path)?;
        let servers = servers(&mut v, native)?;
        if remove || (native && to_fallback) {
            if servers.remove(AGENT_NAME).is_none() {
                continue;
            }
        } else {
            servers.insert(
                AGENT_NAME.into(),
                json!({ "type": "stdio", "command": exe.display().to_string(), "args": ["mcp"], "env": { "REASON_NETWORK_AGENT": "zcode" } }),
            );
        }
        r.lines.push(changed(&path, write_json(&path, &v, dry)?));
    }
    if r.lines.is_empty() {
        r.lines.push("no MCP server to remove".into());
    } else if !remove {
        r.notes.push("Reload MCP in ZCode Desktop (or start a new session) to load Reason Network.".into());
    }
    Ok(r)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch() -> PathBuf {
        let h = std::env::temp_dir().join(format!("reason-zcode-{}", ulid::Ulid::generate()));
        std::fs::create_dir_all(h.join(".zcode/cli")).unwrap();
        h
    }

    fn put(path: &Path, text: &str) {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, text).unwrap();
    }

    fn read(path: &Path) -> Value {
        read_json(path).unwrap()
    }

    #[test]
    fn adds_ours_beside_other_settings_and_takes_it_out_again() {
        let h = scratch();
        let [(native, _), (fallback, _)] = files(&h);
        put(&native, r#"{"provider":"keep","mcp":{"other":7,"servers":{"memory":{"command":"npx"}}}}"#);
        let exe = Path::new("/opt/reason/reason-network");
        apply_in(&h, exe, false, false).unwrap();
        let once = std::fs::read(&native).unwrap();
        apply_in(&h, exe, false, false).unwrap();
        assert_eq!(std::fs::read(&native).unwrap(), once);
        let v = read(&native);
        assert_eq!(v["provider"], "keep");
        assert_eq!(v["mcp"]["other"], 7);
        assert_eq!(v["mcp"]["servers"][AGENT_NAME]["args"], json!(["mcp"]));
        assert!(!fallback.exists());
        // By name, wherever the binary is now.
        apply_in(&h, Path::new("/elsewhere/reason-network"), true, false).unwrap();
        assert_eq!(read(&native)["mcp"]["servers"], json!({"memory": {"command": "npx"}}));
        std::fs::remove_dir_all(h).unwrap();
    }

    #[test]
    fn goes_where_zcode_reads_its_servers() {
        let h = scratch();
        let [(native, _), (fallback, _)] = files(&h);
        put(&native, r#"{"mcp":{"servers":{}}}"#);
        put(&fallback, r#"{"mcpServers":{"memory":{"command":"npx"}}}"#);
        apply_in(&h, Path::new("/opt/reason-network"), false, false).unwrap();
        assert_eq!(read(&native), json!({"mcp": {"servers": {}}}));
        assert!(read(&fallback)["mcpServers"][AGENT_NAME].is_object());
        apply_in(&h, Path::new("/opt/reason-network"), true, false).unwrap();
        assert_eq!(read(&fallback), json!({"mcpServers": {"memory": {"command": "npx"}}}));
        std::fs::remove_dir_all(h).unwrap();
    }

    #[test]
    fn moves_to_agents_when_ours_is_all_zcode_has() {
        let h = scratch();
        let [(native, _), (fallback, _)] = files(&h);
        let exe = Path::new("/opt/reason-network");
        apply_in(&h, exe, false, false).unwrap();
        put(&fallback, r#"{"mcpServers":{"memory":{"command":"npx"}}}"#);
        apply_in(&h, exe, false, false).unwrap();
        assert_eq!(read(&native), json!({"mcp": {"servers": {}}}));
        assert!(read(&fallback)["mcpServers"][AGENT_NAME].is_object());
        std::fs::remove_dir_all(h).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn keeps_ours_when_agents_cant_be_written() {
        use std::os::unix::fs::PermissionsExt;
        let h = scratch();
        let [(native, _), (fallback, _)] = files(&h);
        let exe = Path::new("/opt/reason-network");
        apply_in(&h, exe, false, false).unwrap();
        let before = std::fs::read(&native).unwrap();
        put(&fallback, r#"{"mcpServers":{"memory":{"command":"npx"}}}"#);
        let dir = fallback.parent().unwrap();
        std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o555)).unwrap();
        // Root writes anyway: nothing to check then.
        let checked = std::fs::write(dir.join("probe"), "").is_err();
        let failed = apply_in(&h, exe, false, false).is_err();
        std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o755)).unwrap();
        if checked {
            assert!(failed);
            assert_eq!(std::fs::read(&native).unwrap(), before);
            assert!(wired_in(&h, exe));
        }
        std::fs::remove_dir_all(h).unwrap();
    }

    #[test]
    fn wired_only_where_zcode_reads() {
        let h = scratch();
        let [(native, _), (fallback, _)] = files(&h);
        let exe = Path::new("/opt/reason-network");
        assert!(!wired_in(&h, exe));
        apply_in(&h, exe, false, false).unwrap();
        assert!(wired_in(&h, exe));
        assert!(!wired_in(&h, Path::new("/elsewhere/reason-network")));
        // In `.agents`, ours counts only while ZCode's own file has no server.
        put(&fallback, &json!({"mcpServers": {AGENT_NAME: {"command": "/opt/reason-network"}}}).to_string());
        put(&native, r#"{"mcp":{"servers":{}}}"#);
        assert!(wired_in(&h, exe));
        put(&native, r#"{"mcp":{"servers":{"memory":{"command":"npx"}}}}"#);
        assert!(!wired_in(&h, exe));
        std::fs::remove_dir_all(h).unwrap();
    }

    #[test]
    fn leaves_a_config_it_cant_read_alone() {
        let h = scratch();
        let [(native, _), _] = files(&h);
        for bad in ["{", "[]", r#"{"mcp":[]}"#, r#"{"mcp":{"servers":false}}"#] {
            put(&native, bad);
            assert!(apply_in(&h, Path::new("/opt/reason-network"), false, false).is_err());
            assert_eq!(std::fs::read_to_string(&native).unwrap(), bad);
        }
        std::fs::remove_dir_all(h).unwrap();
    }
}
