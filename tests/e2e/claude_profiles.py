"""A Claude Code session in another profile (`CLAUDE_CONFIG_DIR`) goes by the name it was given, though the daemon and
the CLI run without that variable.

    cargo build && python3 tests/e2e/claude_profiles.py
"""
import json, os, subprocess, sys, tempfile, time

ROOT = os.path.dirname(os.path.dirname(os.path.dirname(os.path.abspath(__file__))))
BIN = os.path.join(ROOT, "target", "debug", "reason-network")
D = tempfile.mkdtemp(prefix="reason-network-profiles-")
HOME = os.path.join(D, "home")
PROFILE = os.path.join(D, "claude-b")
PROJ = os.path.join(D, "proj")
for d in (HOME, os.path.join(PROFILE, "sessions"), PROJ):
    os.makedirs(d)
with open(os.path.join(PROFILE, ".claude.json"), "w") as f:
    json.dump({"mcpServers": {"reason-network": {"type": "stdio", "command": BIN, "args": ["mcp"]}}}, f)
# The shell `who` and `send` run from, and so the daemon it starts: no CLAUDE_CONFIG_DIR.
env = {k: v for k, v in os.environ.items() if not k.startswith("CLAUDE")}
env.update(HOME=HOME, REASON_NETWORK_HOME=os.path.join(D, "rn"), REASON_NETWORK_NO_MDNS="1",
           REASON_NETWORK_NO_SERVICE="1", REASON_NETWORK_NO_UPDATE="1", REASON_NETWORK_NO_TRAY="1",
           REASON_NETWORK_MACHINE="box")
results = []


def check(name, ok, detail=""):
    results.append(ok)
    print(f"{'PASS' if ok else 'FAIL'}: {name}" + ("" if ok else f"\n      {detail}"))


def run(*args):
    r = subprocess.run([BIN, *args], env=env, capture_output=True, text=True, timeout=30, cwd=PROJ)
    return r.returncode, (r.stdout + r.stderr).strip()


def rpc(p, msg):
    p.stdin.write(json.dumps(msg) + "\n")
    p.stdin.flush()


def reply(p, id):
    while True:
        msg = json.loads(p.stdout.readline())
        if msg.get("id") == id:
            return msg


claude_proc = subprocess.Popen(["sleep", "120"])
mcp = None
try:
    run("init", "profiles", "--as", "alex", "--no-setup")
    # `CLAUDE_CONFIG_DIR=<profile> claude --name my-session`: Claude Code's registry entry is in that profile.
    with open(os.path.join(PROFILE, "sessions", f"{claude_proc.pid}.json"), "w") as f:
        json.dump({"pid": claude_proc.pid, "sessionId": "conv-b", "cwd": PROJ, "name": "my-session",
                   "nameSource": "user", "kind": "interactive", "status": "idle"}, f)
    mcp_env = dict(env, CLAUDE_CONFIG_DIR=PROFILE, CLAUDE_PID=str(claude_proc.pid), CLAUDE_CODE_SESSION_ID="conv-b")
    mcp = subprocess.Popen([BIN, "mcp"], stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=subprocess.DEVNULL,
                           env=mcp_env, cwd=PROJ, text=True)
    rpc(mcp, {"jsonrpc": "2.0", "id": 1, "method": "initialize", "params": {"protocolVersion": "2025-06-18",
              "capabilities": {}, "clientInfo": {"name": "claude-code", "version": "1"}}})
    reply(mcp, 1)
    rpc(mcp, {"jsonrpc": "2.0", "method": "notifications/initialized"})
    rpc(mcp, {"jsonrpc": "2.0", "id": 2, "method": "tools/call", "params": {"name": "list_team", "arguments": {}}})
    reply(mcp, 2)

    names = []
    for _ in range(20):
        rc, out = run("who", "--json")
        rows = json.loads(out) if rc == 0 else {}
        rows = rows.get("sessions", rows) if isinstance(rows, dict) else rows
        names = [r.get("address") or r.get("name") for r in rows]
        if any(n and n.endswith("my-session") for n in names):
            break
        time.sleep(0.5)
    check("`who` lists the session by its --name, read from its profile's registry",
          any(n and n.endswith("my-session") for n in names), names)
    rc, out = run("send", "alex/my-session", "hello from another shell")
    check("…and `send` to it works", rc == 0, out)

    # Its MCP server gone, the daemon still finds the session in that profile's registry.
    mcp.stdin.close()
    mcp.wait(5)
    mcp = None
    rc, out = run("stop")
    names = []
    for _ in range(20):
        rc, out = run("who", "--json")
        rows = json.loads(out) if rc == 0 else {}
        rows = rows.get("sessions", rows) if isinstance(rows, dict) else rows
        names = [r.get("address") or r.get("name") for r in rows]
        if any(n and n.endswith("my-session") for n in names):
            break
        time.sleep(0.5)
    check("after a daemon restart, the session is still found in its profile, by its name",
          any(n and n.endswith("my-session") for n in names), names)
finally:
    if mcp:
        mcp.kill()
    claude_proc.kill()
    run("stop")

print(f"\n{sum(results)}/{len(results)} passed")
sys.exit(0 if all(results) else 1)
