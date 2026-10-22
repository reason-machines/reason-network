"""A session's name stays with its conversation across daemon restarts, here and on the person's other machine.

    cargo build && env -u CLAUDE_CODE_MESSAGING_SOCKET python3 tests/e2e/names.py

Needs internet for the second machine (iroh relays/discovery).
"""
import json, os, re, subprocess, sys, tempfile, time
sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
from proofs import TRUST, proof, serve_relay_lists

ROOT = os.path.dirname(os.path.dirname(os.path.dirname(os.path.abspath(__file__))))
BIN = os.path.join(ROOT, "target", "debug", "reason-network")
D = tempfile.mkdtemp(prefix="reason-network-names-")
MACHINE = {"one": "laptop", "two": "desk"}
FAKE_BIN = os.path.join(D, "bin")
UNEXPECTED = os.path.join(D, "unexpected.log")
RELAYS_URL = serve_relay_lists({"all": ["sfo1", "iad1", "fra1", "sin1"]}) + "/all.json"
PROJ = os.path.join(D, "proj")
results = []


def check(name, ok, detail=""):
    results.append(ok)
    print(f"{'PASS' if ok else 'FAIL'}: {name}" + ("" if ok else f"\n      {detail}"))


def env(who, **extra):
    e = {k: v for k, v in os.environ.items() if not k.startswith("CLAUDE")}
    e.update(HOME=os.path.join(D, "home"), PATH=FAKE_BIN + os.pathsep + os.environ["PATH"], CLAUDE_CONFIG_DIR=os.path.join(D, "claude"),
             REASON_NETWORK_HOME=os.path.join(D, who), REASON_NETWORK_MACHINE=MACHINE[who], REASON_NETWORK_NO_MDNS="1",
             REASON_NETWORK_NO_SERVICE="1", REASON_NETWORK_NO_UPDATE="1", REASON_NETWORK_NO_TRAY="1",
             REASON_NETWORK_TRUST_KEYS=TRUST, REASON_NETWORK_RELAYS_URL=RELAYS_URL,
             REASON_NETWORK_OSASCRIPT_BIN=os.path.join(FAKE_BIN, "osascript"), REASON_NETWORK_CODEX_BIN=os.path.join(FAKE_BIN, "codex"))
    e.update(extra)
    return e


def cli(who, *args, **extra):
    r = subprocess.run([BIN, *args], env=env(who, **extra), capture_output=True, text=True, timeout=90, cwd=PROJ)
    return (r.stdout + r.stderr).strip()


def claude(who, event, sid):
    """A hook of a Claude Code session `sid` working in PROJ; what it prints."""
    payload = {"session_id": sid, "cwd": PROJ}
    r = subprocess.run([BIN, "hook", "claude-code", event], env=env(who), capture_output=True, text=True,
                       input=json.dumps(payload), timeout=30, cwd=PROJ)
    return r.stdout.strip()


def you_are(out):
    m = re.search(r"you are (\S+?)\.", out)
    return m.group(1) if m else out


def name_of(who, sid):
    """The address of Claude Code session `sid`, as `status` run in its shell says."""
    out = cli(who, "status", CLAUDECODE="1", CLAUDE_CODE_SESSION_ID=sid)
    m = re.search(r"You are (\S+?)\.$", out)
    return m.group(1) if m else out


class Mcp:
    """An MCP client that gives no session id (one session per server process)."""

    def __init__(self, who, client):
        self.p = subprocess.Popen([BIN, "mcp"], stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=subprocess.DEVNULL,
                                  env=env(who), cwd=PROJ, text=True, bufsize=1)
        self.id = 0
        self.req("initialize", {"protocolVersion": "2025-06-18", "capabilities": {}, "clientInfo": {"name": client, "version": "1"}})
        self.p.stdin.write(json.dumps({"jsonrpc": "2.0", "method": "notifications/initialized"}) + "\n")

    def req(self, method, params):
        self.id += 1
        self.p.stdin.write(json.dumps({"jsonrpc": "2.0", "id": self.id, "method": method, "params": params}) + "\n")
        while True:
            msg = json.loads(self.p.stdout.readline())
            if msg.get("id") == self.id:
                return msg

    def tool(self, name, args):
        res = self.req("tools/call", {"name": name, "arguments": args}).get("result", {})
        return "\n".join(c.get("text", "") for c in res.get("content", []))

    def close(self):
        self.p.stdin.close()
        self.p.wait(5)


def until(fn, ok, secs=45):
    deadline = time.time() + secs
    while True:
        out = fn()
        if ok(out) or time.time() > deadline:
            return out
        time.sleep(1)


os.makedirs(FAKE_BIN)
os.makedirs(os.path.join(D, "home"))
os.makedirs(os.path.join(D, "claude", "sessions"))
os.makedirs(PROJ)
for name in ("open", "xdg-open", "osascript", "notify-send", "codex"):
    with open(os.path.join(FAKE_BIN, name), "w") as f:
        f.write(f'#!/bin/sh\nprintf "{name} %s\\n" "$*" >> {UNEXPECTED}\n')
    os.chmod(os.path.join(FAKE_BIN, name), 0o755)

try:
    ep = {w: cli(w, "endpoint") for w in MACHINE}
    out = cli("one", "init", "names", "--proof", proof("u_lars", "lars", ep["one"]), "--no-setup")
    check("a team, signed in", "You are lars on laptop." in out, out)

    # --- one machine: restarts ---
    a = you_are(claude("one", "session-start", "conv-a"))
    b = you_are(claude("one", "session-start", "conv-b"))
    check("two Claude Code conversations in one folder", (a, b) == ("lars/proj-claude", "lars/proj-claude-2"), (a, b))
    claude("one", "session-end", "conv-a")
    out = cli("one", "send", "proj-claude", "bug report for conversation A")
    check("mail for the offline conversation waits for it, and the sender is told so",
          "it reaches that session when it's back" in out and "a new session that takes this name" not in out, out)
    cli("one", "stop")
    # After the restart a new conversation in the same folder registers first.
    c = claude("one", "session-start", "conv-c")
    check("…a new conversation after the restart gets the next free name", you_are(c) == "lars/proj-claude-3", c)
    check("…and none of the other's mail", "bug report" not in c, c)
    back = claude("one", "session-start", "conv-a")
    check("the first conversation comes back as itself", name_of("one", "conv-a") == "lars/proj-claude", name_of("one", "conv-a"))
    check("…and its mail reaches it", "bug report for conversation A" in back, back)
    out = claude("one", "session-start", "conv-b")
    check("so does the second, whose name was kept through the restart", you_are(out) == "lars/proj-claude-2", out)
    out = cli("one", "who")
    check("who lists each under its own name", all(n in out for n in ("proj-claude ", "proj-claude-2", "proj-claude-3")), out)

    # Renames: never into a name kept for a conversation that is offline.
    claude("one", "session-end", "conv-a")
    out = cli("one", "status", "--name", "proj-claude", CLAUDECODE="1", CLAUDE_CODE_SESSION_ID="conv-c")
    check("a rename into the offline conversation's name is refused",
          "'proj-claude' belongs to a session that is offline" in out and "pick another name" in out, out)
    out = cli("one", "status", "--name", "auth", CLAUDECODE="1", CLAUDE_CODE_SESSION_ID="conv-c")
    check("…another name is fine", "You are lars/auth." in out, out)
    out = cli("one", "listen", "--as", "proj-claude", "--wait", "0")
    check("listen --as the offline conversation's name gets the next free one", "You are lars/proj-claude-3." in out, out)

    # An agent that gives no session id: as before, the next session in its place takes the name and its mail.
    zed = Mcp("one", "zed")
    out = zed.tool("list_team", {})
    check("an MCP client without session ids registers", "proj-zed" in out, out)
    zed.close()
    time.sleep(1)
    out = cli("one", "send", "proj-zed", "for whoever runs zed here")
    check("…its mail's note says a new session there gets it", "a new session that takes this name gets the mail" in out, out)
    zed = Mcp("one", "zed")
    out = zed.tool("check_inbox", {})
    check("…and one does", "for whoever runs zed here" in out, out)
    zed.close()

    # --- two machines of the same person ---
    link = re.search(r"https://\S+/join/#\S+", cli("one", "link")).group(0)
    out = cli("two", "join", link, "--proof", proof("u_lars", "lars", ep["two"]), "--no-setup")
    check("lars's second computer joins", "Joined team names as lars on desk." in out, out)
    out = until(lambda: cli("two", "who"), lambda o: "proj-claude-2" in o)
    check("the second computer sees the first's sessions", "proj-claude-2" in out, out)
    # Its presence once proj-claude-2 is offline too: both names are kept on the first computer.
    claude("one", "session-end", "conv-b")
    out = until(lambda: cli("two", "who"), lambda o: "proj-claude-2" not in o)
    check("…and when one goes offline", "auth" in out and "proj-claude-2" not in out, out)
    d = claude("two", "session-start", "conv-d")
    check("a new conversation there gets none of the names kept on the first", you_are(d) not in ("lars/proj-claude", "lars/proj-claude-2"), d)
    out = cli("two", "send", "lars/proj-claude", "from the desk")
    check("mail from there to the kept name goes to the first computer, for its conversation",
          "it reaches that session when it's back" in out, out)
    back = until(lambda: claude("one", "session-start", "conv-a"), lambda o: "from the desk" in o, secs=20)
    check("…which gets it when it's back, as itself", "from the desk" in back and name_of("one", "conv-a") == "lars/proj-claude", back)
finally:
    for w in MACHINE:
        subprocess.run([BIN, "stop"], env=env(w), capture_output=True, timeout=30)
log = open(UNEXPECTED).read() if os.path.exists(UNEXPECTED) else ""
check("nothing opened a browser or reached the real notifications", log == "", log)
print(f"\n{sum(results)}/{len(results)} checks passed  (state in {D})")
raise SystemExit(0 if all(results) else 1)
