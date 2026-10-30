"""Installed ZCode hooks fail safely when the engine is downgraded.

    cargo build
    python tests/e2e/zcode_rollback.py --old-bin /path/to/v0.12.17/reason-network

Uses the real old executable supplied by the caller; never downloads or replaces a user installation.
"""
import argparse
import json
import os
from pathlib import Path
import queue
import shutil
import sqlite3
import subprocess
import tempfile
import threading

parser = argparse.ArgumentParser(description=__doc__)
parser.add_argument("--old-bin", type=Path, required=True)
args = parser.parse_args()
old = args.old_bin.resolve(strict=True)
source = Path(__file__).resolve().parents[2] / "target/debug" / ("reason-network.exe" if os.name == "nt" else "reason-network")
source = source.resolve(strict=True)


def recover(engine, env, cwd):
    process = subprocess.Popen([str(engine), "mcp"], cwd=cwd,
                               env=dict(env, REASON_NETWORK_AGENT="zcode", REASON_NETWORK_SESSION="rollback"),
                               stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=subprocess.DEVNULL,
                               text=True, encoding="utf-8")
    replies = queue.Queue()
    threading.Thread(target=lambda: [replies.put(line) for line in process.stdout], daemon=True).start()

    def rpc(request):
        process.stdin.write(json.dumps(request) + "\n")
        process.stdin.flush()
        if "id" in request:
            while True:
                reply = json.loads(replies.get(timeout=12))
                if reply.get("id") == request["id"]:
                    assert "error" not in reply, reply
                    return reply["result"]

    try:
        rpc({"jsonrpc":"2.0", "id":1, "method":"initialize", "params":{
            "protocolVersion":"2024-11-05", "capabilities":{}, "clientInfo":{"name":"zcode", "version":"rollback-test"}}})
        rpc({"jsonrpc":"2.0", "method":"notifications/initialized"})
        result = rpc({"jsonrpc":"2.0", "id":2, "method":"tools/call", "params":{
            "name":"check_inbox", "arguments":{}}})
        assert not result.get("isError"), result
        return "\n".join(c.get("text", "") for c in result.get("content", []))
    finally:
        process.stdin.close()
        try:
            process.wait(timeout=5)
        except subprocess.TimeoutExpired:
            process.kill()
            process.wait(timeout=5)


root = Path(tempfile.mkdtemp(prefix="rn-zcode-rollback-用户 空格-"))
print("Isolated fixtures:", root, flush=True)
for custom in (False, True):
    name = "custom" if custom else "default"
    project = root / name
    profile = project / "profile"
    (profile / ".zcode/cli").mkdir(parents=True)
    engine = project / source.name
    shutil.copy2(source, engine)
    env = {k:v for k,v in os.environ.items() if not k.startswith(("CLAUDE", "CODEX", "GROK", "REASON_", "TINCAN_"))}
    env.update(HOME=str(profile), USERPROFILE=str(profile), APPDATA=str(profile / "AppData/Roaming"),
               LOCALAPPDATA=str(profile / "AppData/Local"), PATH="", REASON_NETWORK_NO_SERVICE="1",
               REASON_NETWORK_NO_UPDATE="1", REASON_NETWORK_NO_MDNS="1", REASON_NETWORK_NO_TRAY="1")
    state = project / "custom state" if custom else profile / ".reason/network"
    if custom:
        env["REASON_NETWORK_HOME"] = str(state)

    def run(*command):
        result = subprocess.run([str(engine), *command], cwd=project, env=env, stdin=subprocess.DEVNULL,
                                capture_output=True, text=True, encoding="utf-8", timeout=15)
        assert result.returncode == 0, result.stderr
        return result.stdout

    def rows():
        with sqlite3.connect((state / "network.db").as_uri() + "?mode=ro", uri=True) as db:
            return db.execute("SELECT id,body,read_at,zcode_hook_attempts FROM messages ORDER BY sent_at,rowid").fetchall()

    def invoke(entry, event):
        return subprocess.run([entry["command"], *entry["args"]], cwd=project, env=env,
                              input=json.dumps({"session_id":"rollback", "cwd":str(project), "hook_event_name":event}) + "\n",
                              capture_output=True, text=True, encoding="utf-8", timeout=5)

    try:
        run("init", "rollback", "--as", "tester", "--no-setup")
        run("who")
        run("setup", "--no-service")
        config = json.loads((profile / ".zcode/cli/config.json").read_text(encoding="utf-8"))
        entries = {event:config["hooks"]["events"][event][0]["hooks"][0] for event in ("SessionStart", "PostToolUse")}
        for entry in entries.values():
            assert entry["args"][3] == "--network-home", entry
            assert Path(entry["args"][4]).resolve() == state.resolve(), entry
        start = invoke(entries["SessionStart"], "SessionStart")
        assert start.returncode == 0, start.stderr
        intro = json.loads(start.stdout)["hookSpecificOutput"]["additionalContext"]
        address = intro.split("you are ", 1)[1].split(".", 1)[0]
        run("send", address, "offered-before-rollback")
        offered = invoke(entries["PostToolUse"], "PostToolUse")
        assert offered.returncode == 0 and "offered-before-rollback" in offered.stdout, offered.stderr
        json.loads(offered.stdout)
        run("send", address, "never-offered-before-rollback")
        before = rows()
        assert len(before) == 2 and [r[2] for r in before] == [None, None] and [r[3] for r in before] == [1, 0], before
        run("stop")
        # Rename works even while Windows finishes exiting the old process; configuration still points to engine.
        engine.rename(project / ("current-" + source.name))
        shutil.copy2(old, engine)
        assert "0.12.17" in run("--version"), "This regression expects the v0.12.17 engine"
        run("who")  # The downgraded daemon opens the already-migrated store.
        for event, entry in entries.items():
            rejected = invoke(entry, event)
            assert rejected.returncode == 2 and rejected.stdout == "" and "--network-home" in rejected.stderr, rejected
        assert rows() == before, "Downgraded hooks consumed or changed pending mail"
        recovered = recover(engine, env, project)
        assert "offered-before-rollback" in recovered and "never-offered-before-rollback" in recovered, recovered
        assert all(r[2] is not None for r in rows()), "Explicit inbox recovery did not confirm receipt"
        print(f"PASS: {name} home: installed hooks reject on v0.12.17; both messages survive and check_inbox recovers them", flush=True)
    finally:
        run("stop")
print("2 real rollback scenarios passed", flush=True)
