"""This build and an older one in one team: sign-in proofs and rn1 links leave older machines working.

    cargo build && REASON_NETWORK_OLD_BIN=<an older reason-network, e.g. 0.10.8> \
        env -u CLAUDE_CODE_MESSAGING_SOCKET python3 tests/e2e/mixed_versions.py

Needs internet (iroh relays/discovery).
"""
import json, os, re, subprocess, sys, tempfile, time
sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
from proofs import TRUST, proof, serve_relay_lists

ROOT = os.path.dirname(os.path.dirname(os.path.dirname(os.path.abspath(__file__))))
NEW = os.path.join(ROOT, "target", "debug", "reason-network")
OLD = os.environ["REASON_NETWORK_OLD_BIN"]
D = tempfile.mkdtemp(prefix="reason-network-mixed-")
FAKE_BIN = os.path.join(D, "bin")
UNEXPECTED = os.path.join(D, "unexpected.log")
# Reason's relay list, served here for this build (debug): never the real one. The older build has no list.
RELAYS_URL = serve_relay_lists({"all": ["sfo1", "iad1", "fra1", "sin1"]}) + "/all.json"
# alex and nina run this build, olga the older one.
WHO = {"alex": (NEW, "laptop"), "olga": (OLD, "oldbox"), "nina": (NEW, "desk"), "nina2": (NEW, "pad"), "nina3": (NEW, "tab"), "mal": (NEW, "evil")}
results = []


def check(name, ok, detail=""):
    results.append(ok)
    print(f"{'PASS' if ok else 'FAIL'}: {name}" + ("" if ok else f"\n      {detail}"))


def env(who):
    e = {k: v for k, v in os.environ.items() if not k.startswith("CLAUDE")}
    e.update(HOME=os.path.join(D, "home"), PATH=FAKE_BIN + os.pathsep + os.environ["PATH"], CLAUDE_CONFIG_DIR=os.path.join(D, "claude"),
             REASON_NETWORK_HOME=os.path.join(D, who), REASON_NETWORK_MACHINE=WHO[who][1], REASON_NETWORK_NO_MDNS="1",
             REASON_NETWORK_NO_SERVICE="1", REASON_NETWORK_NO_UPDATE="1", REASON_NETWORK_NO_TRAY="1",
             REASON_NETWORK_TRUST_KEYS=TRUST, REASON_NETWORK_RELAYS_URL=RELAYS_URL)
    return e


def cli(who, *args):
    r = subprocess.run([WHO[who][0], *args], env=env(who), capture_output=True, text=True, timeout=90)
    return (r.stdout + r.stderr).strip()


def until(who, args, ok, secs=45):
    """Runs the command until its output passes `ok` (machines find each other in their own time); the last output."""
    deadline = time.time() + secs
    while True:
        out = cli(who, *args)
        if ok(out) or time.time() > deadline:
            return out
        time.sleep(1)


def logged(who):
    p = os.path.join(D, who, "daemon.log")
    return open(p, errors="replace").read() if os.path.exists(p) else ""


def link_of(out):
    m = re.search(r"https://\S+/join/#\S+", out)
    return m.group(0) if m else ""


os.makedirs(FAKE_BIN)
os.makedirs(os.path.join(D, "home"))
for name in ("open", "xdg-open", "osascript", "notify-send"):
    with open(os.path.join(FAKE_BIN, name), "w") as f:
        f.write(f'#!/bin/sh\nprintf "{name} %s\\n" "$*" >> {UNEXPECTED}\n')
    os.chmod(os.path.join(FAKE_BIN, name), 0o755)

try:
    print(f"this build: {cli('alex', '--version')} · older: {cli('olga', '--version')}")
    ep = {w: cli(w, "endpoint") for w in ("alex", "nina", "nina2", "nina3", "mal")}
    out = cli("alex", "init", "mixed", "--proof", proof("u_alex", "alex", ep["alex"]), "--no-setup")
    check("this build starts a team, signed in", "You are alex on laptop." in out, out)
    link = link_of(cli("alex", "link"))
    out = cli("olga", "join", link, "--as", "olga", "--no-setup")
    check("the older build can't read an rn1 link", "Joined" not in out, out)
    out = cli("olga", "join", link.replace("#rn1-", "#tincan1"), "--as", "olga", "--no-setup")
    check("…but joins with the same link as tincan1", "Joined team mixed as olga on oldbox." in out, out)
    out = until("alex", ["team"], lambda o: re.search(r"olga\s+oldbox\s+(direct|relay)", o))
    check("the older machine is connected, and alex still verified after its state came back",
          re.search(r"olga\s+oldbox\s+(direct|relay)", out) is not None and "alex (verified)" in out, out)
    out = until("olga", ["team"], lambda o: re.search(r"alex\s+laptop\s+(direct|relay)", o))
    check("the older machine sees alex, connected", re.search(r"alex\s+laptop\s+(direct|relay)", out) is not None, out)

    # A machine of this build joins through the older one, which knows nothing of proofs.
    old_link = link_of(cli("olga", "link"))
    out = cli("nina", "join", old_link, "--proof", proof("u_nina", "nina", ep["nina"]), "--no-setup")
    check("this build joins, signed in, through the older machine", "Joined team mixed as nina on desk." in out, out)
    check("…and its proof goes along anyway", "nina (verified)" in out, out)
    out = until("alex", ["team"], lambda o: re.search(r"nina \(verified\)\s+desk\s+(direct|relay)", o))
    check("alex's machine sees nina verified", re.search(r"nina \(verified\)\s+desk\s+(direct|relay)", out) is not None, out)
    three = lambda o: all(re.search(rf"{p}\s+{m}\s+(direct|relay|this machine)", o) for p, m in [("alex", "laptop"), ("olga", "oldbox"), ("nina", "desk")])
    out = until("olga", ["team"], three)
    check("the older machine has all three, every signed entry checking out", three(out), out)
    # nina's second computer, through the older machine's link: it asks the link's machines for one of nina's own,
    # whose word the older machine takes too.
    # (nina's machine is up and reachable: every machine sees it connected.)
    until("alex", ["team"], lambda o: re.search(r"nina \(verified\)\s+desk\s+(direct|relay)", o))
    old_link = link_of(cli("olga", "link"))  # now listing nina's machine too
    out = cli("nina2", "join", old_link, "--proof", proof("u_nina", "nina", ep["nina2"]), "--no-setup")
    check("another computer of nina's account joins through the older machine's link", "Joined team mixed as nina on pad." in out, out)
    check("…let in by nina's own machine", "joined team mixed as nina: let in by nina on desk" in logged("nina2"), logged("nina2")[-2000:])
    out = until("olga", ["team"], lambda o: re.search(r"nina\s+pad\s+(direct|relay)", o))
    check("…so the older machine takes it too", re.search(r"nina\s+pad\s+(direct|relay)", out) is not None, out)
    cli("nina2", "leave")
    cli("nina2", "stop")
    # With nina's machine off, the older machine first on its link says the name is taken; alex's machine, further
    # down, lets the new computer in on its proof (which the older machine doesn't take, as documented).
    cli("nina", "stop")
    until("olga", ["team"], lambda o: not re.search(r"nina\s+desk\s+(direct|relay)", o))
    out = cli("nina3", "join", link_of(cli("olga", "link")), "--proof", proof("u_nina", "nina", ep["nina3"]), "--no-setup")
    check("past the older machine's refusal, a machine of this build lets nina's computer in", "Joined team mixed as nina on tab." in out, out)
    check("…alex's, on its proof", "joined team mixed as nina: let in by alex on laptop" in logged("nina3"), logged("nina3")[-2000:])
    out = until("alex", ["team"], lambda o: re.search(r"nina \(verified\)\s+tab\s+(direct|relay)", o))
    check("…which this build's machines take", re.search(r"nina \(verified\)\s+tab\s+(direct|relay)", out) is not None, out)
    cli("nina3", "leave")
    cli("nina3", "stop")
    out = subprocess.run([NEW, "join", old_link, "--proof", proof("u_mal", "alex", ep["mal"]), "--no-setup", "--json"],
                         env=env("mal"), capture_output=True, text=True, timeout=90).stdout.strip()
    check("a taken name through the older machine is name_taken", out == '{"error":"name_taken"}', out)

    # Messages both ways, and the state kept across restarts.
    cli("olga", "listen", "--as", "t", "--wait", "0")
    cli("alex", "listen", "--as", "t", "--wait", "0")
    out = until("alex", ["send", "olga/t", "--from", "alex/t", "hello old friend"], lambda o: "→ olga/t" in o and "no session" not in o)
    check("this build sends to the older one", "→ olga/t" in out, out)
    out = cli("olga", "listen", "--as", "t", "--wait", "10")
    check("…which gets it", "hello old friend" in out, out)
    cli("olga", "send", "alex/t", "--from", "olga/t", "hello back")
    out = cli("alex", "listen", "--as", "t", "--wait", "10")
    check("and back", "hello back" in out, out)
    for w in ("alex", "olga"):
        cli(w, "stop")
    cli("olga", "team")
    out = until("alex", ["team"], lambda o: re.search(r"olga\s+oldbox\s+(direct|relay)", o))
    check("after both restart, people stay verified", "alex (verified)" in out and "nina (verified)" in out
          and re.search(r"olga\s+oldbox\s+(direct|relay)", out) is not None, out)

    # This build's MCP server for a Claude Code session on the older machine: with no inbox to push into yet, it tells
    # the daemon so, which an older daemon can't read. That mustn't hold up the pushes after it.
    import json, socket, threading
    claude_dir = os.path.join(D, "claude")
    os.makedirs(os.path.join(claude_dir, "sessions"), exist_ok=True)
    with open(os.path.join(claude_dir, ".claude.json"), "w") as f:
        json.dump({"mcpServers": {"reason-network": {"type": "stdio", "command": NEW, "args": ["mcp"]}}}, f)
    cli("olga", "stop")
    cli("olga", "team")  # the older daemon again, now seeing Claude Code set up
    proj = os.path.join(D, "mv-proj")
    os.makedirs(proj)
    claude_proc = subprocess.Popen(["sleep", "300"])
    entry = os.path.join(claude_dir, "sessions", f"{claude_proc.pid}.json")
    inbox_path = os.path.join(D, "mv.sock")
    def list_session(inbox):
        with open(entry, "w") as f:
            json.dump({"pid": claude_proc.pid, "sessionId": "mv-1", "cwd": proj, "kind": "interactive",
                       **({"messagingSocketPath": inbox_path} if inbox else {})}, f)
    list_session(False)
    until("olga", ["who"], lambda o: "mv-proj-claude" in o)
    srv = socket.socket(socket.AF_UNIX)
    srv.bind(inbox_path)
    srv.listen(4)
    frames = []
    def serve_inbox():
        while True:
            c, _ = srv.accept()
            frames.append(c.makefile().read())
    threading.Thread(target=serve_inbox, daemon=True).start()
    mcp = subprocess.Popen([NEW, "mcp"], stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=subprocess.DEVNULL, cwd=proj, text=True,
                           env=dict(env("olga"), CLAUDE_PID=str(claude_proc.pid), CLAUDE_CODE_SESSION_ID="mv-1"))
    def rpc(msg):
        mcp.stdin.write(json.dumps(msg) + "\n")
        mcp.stdin.flush()
    def reply(want):
        while True:
            m = json.loads(mcp.stdout.readline())
            if m.get("id") == want:
                return m
    rpc({"jsonrpc": "2.0", "id": 1, "method": "initialize", "params": {"protocolVersion": "2025-06-18", "capabilities": {}, "clientInfo": {"name": "claude-code", "version": "1"}}})
    reply(1)
    rpc({"jsonrpc": "2.0", "method": "notifications/initialized"})
    rpc({"jsonrpc": "2.0", "id": 2, "method": "tools/call", "params": {"name": "list_team", "arguments": {}}})
    reply(2)
    out = cli("olga", "send", "mv-proj-claude", "first, with no inbox yet")
    check("the older daemon pushes to this build's Claude Code server, which has no inbox yet", "→ olga/mv-proj-claude" in out, out)
    time.sleep(2)
    list_session(True)
    cli("olga", "send", "mv-proj-claude", "second, once there is one")
    deadline = time.time() + 10
    while time.time() < deadline and "second, once there is one" not in "".join(frames):
        time.sleep(0.5)
    check("…and the next push still goes into the inbox once there is one", "second, once there is one" in "".join(frames),
          "".join(frames)[-500:])
    mcp.kill()
    claude_proc.kill()
finally:
    for w in WHO:
        subprocess.run([WHO[w][0], "stop"], env=env(w), capture_output=True, timeout=30)
log = open(UNEXPECTED).read() if os.path.exists(UNEXPECTED) else ""
check("nothing opened a browser or reached the real notifications", log == "", log)
print(f"\n{sum(results)}/{len(results)} checks passed  (state in {D})")
raise SystemExit(0 if all(results) else 1)
