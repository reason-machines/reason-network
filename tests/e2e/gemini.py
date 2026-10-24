"""Gemini CLI windows against one isolated daemon, launched the way Gemini 0.65 does it: each window is its own process
(standing in for Gemini's node), which runs its hooks through a shell (`powershell -NoProfile -Command` on Windows,
`sh -c` elsewhere) with GEMINI_SESSION_ID in the hooks' env only, and starts `reason-network mcp` directly with no
session id. A window is one session, at its hooks' tier, and the one its agent speaks as.

    cargo build && python3 tests/e2e/gemini.py
"""
import json, os, subprocess, sys, tempfile, threading, time

ROOT = os.path.dirname(os.path.dirname(os.path.dirname(os.path.abspath(__file__))))
EXE = ".exe" if os.name == "nt" else ""
BIN = os.path.join(ROOT, "target", "debug", "reason-network" + EXE)
UTF8 = dict(text=True, encoding="utf-8", errors="replace")


def window(proj, base):
    """The stand-in Gemini process: one command per line on stdin (mcp, start <id>, end <id>, call, call-bg and
    call-join for a call left running meanwhile, close), one JSON answer line each."""
    def hook(event, sid, name):
        payload = json.dumps({"session_id": sid, "cwd": proj, "hook_event_name": name})
        env = dict(base, GEMINI_CLI="1", GEMINI_PROJECT_DIR=proj, GEMINI_CWD=proj, GEMINI_SESSION_ID=sid)
        cmd = f'& "{BIN}" hook gemini {event}' if os.name == "nt" else f'"{BIN}" hook gemini {event}'
        shell = ["powershell", "-NoProfile", "-Command", cmd] if os.name == "nt" else ["sh", "-c", cmd]
        subprocess.run(shell, input=payload, env=env, cwd=proj, capture_output=True, timeout=60, **UTF8)

    def rpc(msg):
        mcp.stdin.write(json.dumps(msg) + "\n")
        mcp.stdin.flush()
        while "id" in msg:
            r = json.loads(mcp.stdout.readline())
            if r.get("id") == msg["id"]:
                return r

    def you():
        r = rpc({"jsonrpc": "2.0", "id": 2, "method": "tools/call", "params": {"name": "list_team", "arguments": {}}})
        return " | ".join(l for l in r["result"]["content"][0]["text"].splitlines() if "← you" in l)

    mcp, bg = None, {}
    for line in sys.stdin:
        cmd, _, arg = line.strip().partition(" ")
        out = ""
        if cmd == "mcp":
            mcp = subprocess.Popen([BIN, "mcp"], env=dict(base, GEMINI_CLI="1", REASON_NETWORK_AGENT="gemini"),
                                   cwd=proj, stdin=subprocess.PIPE, stdout=subprocess.PIPE,
                                   stderr=subprocess.DEVNULL, **UTF8)
            rpc({"jsonrpc": "2.0", "id": 1, "method": "initialize", "params": {
                "protocolVersion": "2025-06-18", "capabilities": {},
                "clientInfo": {"name": "gemini-cli-mcp-client", "version": "0.65.0"}}})
            rpc({"jsonrpc": "2.0", "method": "notifications/initialized"})
        elif cmd in ("start", "end"):
            hook(f"session-{cmd}", arg, "SessionStart" if cmd == "start" else "SessionEnd")
        elif cmd == "call":
            out = you()
        elif cmd == "call-bg":
            bg["t"] = threading.Thread(target=lambda: bg.update(out=you()))
            bg["t"].start()
        elif cmd == "call-join":
            bg["t"].join(timeout=60)
            out = bg.get("out", "")
        elif cmd == "close":
            mcp.stdin.close()
            mcp.wait(timeout=30)
        print(json.dumps(out), flush=True)


def main():
    d = tempfile.mkdtemp(prefix="reason-network-gemini-")
    proj = os.path.join(d, "app")
    os.makedirs(proj)
    base = {k: v for k, v in os.environ.items() if not k.startswith(("CLAUDE", "CODEX", "GEMINI", "REASON_NETWORK", "TINCAN"))}
    base.update(REASON_NETWORK_HOME=os.path.join(d, "home"), REASON_NETWORK_NO_MDNS="1", REASON_NETWORK_NO_SERVICE="1",
                REASON_NETWORK_NO_TRAY="1", REASON_NETWORK_NO_UPDATE="1", REASON_NETWORK_MACHINE="box")
    results = []

    def check(name, ok, detail=""):
        results.append(ok)
        print(f"{'PASS' if ok else 'FAIL'}: {name}" + ("" if ok else f"\n      {detail}"), flush=True)

    def cli(*args):
        r = subprocess.run([BIN, *args], env=base, capture_output=True, timeout=60, **UTF8)
        return (r.stdout + r.stderr).strip()

    def sessions():
        rows = json.loads(cli("who", "--json") or "{}")
        rows = rows.get("sessions", rows) if isinstance(rows, dict) else rows
        return sorted((s.get("name"), s.get("tier")) for s in rows if isinstance(s, dict) and s.get("agent") == "gemini")

    def watch(secs):
        """What `who` lists over `secs`: every change, in order."""
        seen, end = [], time.time() + secs
        while time.time() < end:
            now = sessions()
            if not seen or now != seen[-1]:
                seen.append(now)
            time.sleep(0.25)
        return seen

    def open_window():
        g = subprocess.Popen([sys.executable, os.path.abspath(__file__), "--window", proj, json.dumps(base)],
                             stdin=subprocess.PIPE, stdout=subprocess.PIPE, **UTF8)

        def do(cmd):
            g.stdin.write(cmd + "\n")
            g.stdin.flush()
            return json.loads(g.stdout.readline())

        return g, do

    cli("init", "gemini", "--as", "alex", "--no-setup")

    # Its hooks soon after its MCP server starts, as Gemini goes.
    g, do = open_window()
    do("mcp")
    do("start s-1")
    seen = watch(5)
    check("a window is one session, at its hooks' tier", seen[-1] == [("app-gemini", "turn")] and all(len(s) <= 1 for s in seen), seen)
    check("…and the one its agent speaks as", " app-gemini " in f" {do('call')} ", do("call"))
    # `/clear`: SessionEnd, a new id, SessionStart, the same MCP server throughout; a call made in between waits for
    # the new conversation.
    do("end s-1")
    do("call-bg")
    time.sleep(1)
    do("start s-2")
    between = do("call-join")
    seen = watch(2)
    check("/clear: the window is one session again, the new conversation", len(seen[-1]) == 1 and seen[-1][0][1] == "turn", seen)
    me = do("call")
    check("…which its agent now speaks as, also in a call made in between",
          bool(seen[-1]) and f" {seen[-1][0][0]} " in f" {me} " and f" {seen[-1][0][0]} " in f" {between} ", (me, between))
    do("close")
    do("end s-2")
    g.stdin.close()
    g.wait(timeout=30)
    check("closing the window leaves no session", watch(1)[-1] == [], sessions())

    # Hooks that take seconds (PowerShell on Windows): its MCP server waits for them, never a second session.
    g, do = open_window()
    do("mcp")
    time.sleep(6)
    check("before its hooks, no session by process yet", sessions() == [], sessions())
    do("start s-3")
    seen = watch(3)
    check("slow hooks: one session, at its hooks' tier", len(seen[-1]) == 1 and seen[-1][0][1] == "turn" and all(len(s) <= 1 for s in seen), seen)
    me = do("call")
    check("…which its agent speaks as", bool(seen[-1]) and f" {seen[-1][0][0]} " in f" {me} ", me)
    do("close")
    do("end s-3")
    g.stdin.close()
    g.wait(timeout=30)
    check("closing it leaves no session", watch(1)[-1] == [], sessions())

    cli("stop")
    print(f"\n{sum(results)}/{len(results)} gemini checks passed  (state in {d})")
    sys.exit(0 if all(results) else 1)


if __name__ == "__main__":
    if sys.argv[1:2] == ["--window"]:
        window(sys.argv[2], json.loads(sys.argv[3]))
    else:
        main()
