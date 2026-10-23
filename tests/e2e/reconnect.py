"""A reconnect never delivers a message twice: not when the receiving machine comes back, not when both daemons restart
after delivery, and not when the sender resends a message whose ack it never saw (its outbox as a lost ack leaves it:
the messages put back while its daemon is stopped). Two machines (two REASON_NETWORK_HOMEs) on one box, over iroh,
with a session of each kind: a pull session (check_inbox), a pushed one (`codex queue`), a hooked one (Claude Code's
hooks) and `listen` mailboxes, both ways, with a reply.

    cargo build && env -u CLAUDE_CODE_MESSAGING_SOCKET python3 tests/e2e/reconnect.py

Needs internet (iroh relays/discovery).
"""
import json, os, re, sqlite3, subprocess, sys, tempfile, time
sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
from proofs import TRUST, proof, serve_relay_lists

ROOT = os.path.dirname(os.path.dirname(os.path.dirname(os.path.abspath(__file__))))
BIN = os.path.join(ROOT, "target", "debug", "reason-network")
D = tempfile.mkdtemp(prefix="reason-network-reconnect-")
MACHINE = {"alex": "laptop", "alice": "desktop"}
FAKE_BIN = os.path.join(D, "bin")
UNEXPECTED = os.path.join(D, "unexpected.log")
# Stands in for `codex queue`: each push into the Codex session is a line here.
FAKE_CODEX = os.path.join(D, "fake-codex.sh")
QUEUE_LOG = os.path.join(D, "codex-queue.log")
RELAYS_URL = serve_relay_lists({"all": ["sfo1", "iad1", "fra1", "sin1"]}) + "/all.json"
PULL, PUSH, HOOKED = (os.path.join(D, d) for d in ("pull", "push", "hooked"))
META = {"sessionId": "pull-thread", "threadId": "pull-thread"}
results = []


def check(name, ok, detail=""):
    results.append(ok)
    print(f"{'PASS' if ok else 'FAIL'}: {name}" + ("" if ok else f"\n      {detail}"))


def env(who):
    e = {k: v for k, v in os.environ.items() if not k.startswith("CLAUDE")}
    e.update(HOME=os.path.join(D, "home"), PATH=FAKE_BIN + os.pathsep + os.environ["PATH"], CLAUDE_CONFIG_DIR=os.path.join(D, "claude"),
             REASON_NETWORK_HOME=os.path.join(D, who), REASON_NETWORK_MACHINE=MACHINE[who], REASON_NETWORK_NO_MDNS="1",
             REASON_NETWORK_NO_SERVICE="1", REASON_NETWORK_NO_UPDATE="1", REASON_NETWORK_NO_TRAY="1",
             REASON_NETWORK_TRUST_KEYS=TRUST, REASON_NETWORK_RELAYS_URL=RELAYS_URL, REASON_NETWORK_CODEX_BIN=FAKE_CODEX,
             REASON_NETWORK_OSASCRIPT_BIN=os.path.join(FAKE_BIN, "osascript"))
    return e


def cli(who, *args):
    r = subprocess.run([BIN, *args], env=env(who), capture_output=True, text=True, timeout=90, cwd=D)
    return (r.stdout + r.stderr).strip()


def hook(who, agent, event, sid, cwd):
    """A hook of `agent`'s session `sid` working in `cwd`; what it prints."""
    r = subprocess.run([BIN, "hook", agent, event], env=env(who), capture_output=True, text=True,
                       input=json.dumps({"session_id": sid, "cwd": cwd}), timeout=30, cwd=cwd)
    return r.stdout.strip()


class Mcp:
    """Codex's MCP server, a pull session: it sees mail when it calls check_inbox."""

    def __init__(self, who, cwd):
        self.p = subprocess.Popen([BIN, "mcp"], stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=subprocess.DEVNULL,
                                  env=env(who), cwd=cwd, text=True, bufsize=1)
        self.id = 0
        self.req("initialize", {"protocolVersion": "2025-06-18", "capabilities": {}, "clientInfo": {"name": "codex-mcp-client", "version": "1"}})
        self.p.stdin.write(json.dumps({"jsonrpc": "2.0", "method": "notifications/initialized"}) + "\n")

    def req(self, method, params):
        self.id += 1
        self.p.stdin.write(json.dumps({"jsonrpc": "2.0", "id": self.id, "method": method, "params": params}) + "\n")
        while True:
            msg = json.loads(self.p.stdout.readline())
            if msg.get("id") == self.id:
                return msg

    def tool(self, name, args):
        res = self.req("tools/call", {"name": name, "arguments": args, "_meta": META}).get("result", {})
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


def db(who):
    return sqlite3.connect(os.path.join(D, who, "network.db"), timeout=10)


def outbox(who):
    """`who`'s outbox: the messages it keeps until the receiving machine acks them."""
    with db(who) as con:
        return con.execute("SELECT id, target, msg, created_at FROM outbox ORDER BY created_at, rowid").fetchall()


def unacked(who, rows):
    """Puts `rows` back in `who`'s outbox (its daemon stopped): as if it sent them but never saw the acks."""
    with db(who) as con:
        con.executemany("INSERT OR REPLACE INTO outbox (id, target, msg, created_at) VALUES (?1, ?2, ?3, ?4)", rows)


def times(out, msg_id):
    """How often message `msg_id` was shown in `out` (a hook's JSON escapes the quotes)."""
    return len(re.findall(rf'\bid=\\?"{msg_id}', out))


def queued():
    return open(QUEUE_LOG).read() if os.path.exists(QUEUE_LOG) else ""


def linked(a, b):
    return until(lambda: cli(a, "team"), lambda o: re.search(rf"{b} \(verified\)\s+{MACHINE[b]}\s+(direct|relay)", o) is not None)


for d in (FAKE_BIN, os.path.join(D, "home"), os.path.join(D, "claude", "sessions"), PULL, PUSH, HOOKED):
    os.makedirs(d)
for name in ("open", "xdg-open", "osascript", "notify-send"):
    with open(os.path.join(FAKE_BIN, name), "w") as f:
        f.write(f'#!/bin/sh\nprintf "{name} %s\\n" "$*" >> {UNEXPECTED}\n')
    os.chmod(os.path.join(FAKE_BIN, name), 0o755)
with open(FAKE_CODEX, "w") as f:
    f.write(f'#!/bin/sh\nprintf "%s\\n" "$*" >> {QUEUE_LOG}\necho "Queued message"\n')
os.chmod(FAKE_CODEX, 0o755)

pull = None
try:
    ep = {w: cli(w, "endpoint") for w in MACHINE}
    out = cli("alex", "init", "again", "--proof", proof("u_alex", "alex", ep["alex"]), "--no-setup")
    check("a team", "You are alex on laptop." in out, out)
    link = re.search(r"(https://\S+#rn1-\S+)", cli("alex", "link")).group(1)
    out = cli("alice", "join", link, "--proof", proof("u_alice", "alice", ep["alice"]), "--no-setup")
    check("alice joins", "Joined team again as alice on desktop." in out, out)
    linked("alex", "alice")

    # alice's sessions: pull, pushed, hooked, and two mailboxes; alex's: hooked, and a mailbox.
    pull = Mcp("alice", PULL)
    pull.tool("list_team", {})
    hook("alice", "codex", "session-start", "push-thread", PUSH)
    hook("alice", "claude-code", "session-start", "alice-conv", HOOKED)
    cli("alice", "listen", "--as", "desk", "--wait", "0")
    cli("alice", "listen", "--as", "later", "--wait", "0")
    hook("alex", "claude-code", "session-start", "alex-conv", HOOKED)
    cli("alex", "listen", "--as", "lap", "--wait", "0")
    to_alice = ["alice/pull-codex", "alice/push-codex", "alice/hooked-claude", "alice/desk", "alice/later"]
    out = until(lambda: cli("alex", "who"), lambda o: all(s.split("/")[1] in o for s in to_alice))
    check("alex sees alice's sessions", all(s.split("/")[1] in out for s in to_alice), out)
    out = until(lambda: cli("alice", "who"), lambda o: "hooked-claude" in o and "lap" in o)
    check("…and alice alex's", "hooked-claude" in out and "lap" in out, out)

    # --- 1. the receiving machine is off: what waits in the sender's outbox reaches each session once ---
    pull.close()
    cli("alice", "stop")
    time.sleep(1)
    for to in to_alice:
        out = cli("alex", "send", to, f"for {to} while alice is off (e2e test, ignore)")
        check(f"a message for {to} waits while alice's machine is off", "not connected" in out or "no answer" in out, out)
    sent = outbox("alex")
    check("…in alex's outbox", len(sent) == 5, sent)
    ids = {json.loads(r[2])["to"]: r[0] for r in sent}
    # alex's machine restarts too, so they go out only once alice's sessions are back: the Codex one gets its message pushed.
    cli("alex", "stop")
    cli("alice", "team")
    pull = Mcp("alice", PULL)
    pull.tool("list_team", {})
    hook("alice", "codex", "session-start", "push-thread", PUSH)
    hook("alice", "claude-code", "session-start", "alice-conv", HOOKED)
    cli("alex", "team")
    linked("alex", "alice")
    left = until(lambda: outbox("alex"), lambda o: not o)
    check("alice comes back: alex's outbox empties, each acked", left == [], left)
    out = pull.tool("check_inbox", {})
    check("check_inbox shows it once", times(out, ids["alice/pull-codex"]) == 1, out)
    out = pull.tool("check_inbox", {})
    check("…and not again", out.startswith("No messages."), out)
    until(lambda: queued(), lambda o: ids["alice/push-codex"] in o, secs=10)
    shown = hook("alice", "codex", "post-tool-use", "push-thread", PUSH) + hook("alice", "codex", "stop", "push-thread", PUSH)
    check("the Codex session gets it pushed, once", times(queued(), ids["alice/push-codex"]) == 1 and ids["alice/push-codex"] not in shown,
          (queued(), shown))
    out = hook("alice", "claude-code", "stop", "alice-conv", HOOKED)
    check("the hooked session gets it once", times(out, ids["alice/hooked-claude"]) == 1, out)
    out = hook("alice", "claude-code", "stop", "alice-conv", HOOKED)
    check("…and not again", ids["alice/hooked-claude"] not in out, out)
    out = cli("alice", "listen", "--as", "desk", "--wait", "5")
    check("listen shows it once", times(out, ids["alice/desk"]) == 1, out)
    out = cli("alice", "listen", "--as", "desk", "--wait", "1")
    check("…and not again", out.startswith("No messages"), out)
    # (alice/later stays unread: it is read only after the resends below.)

    # --- the other way, with a reply ---
    cli("alex", "stop")
    time.sleep(1)
    out = pull.tool("send_message", {"to": "alex/lap", "message": "a reply while alex is off (e2e test, ignore)", "reply_to": ids["alice/pull-codex"]})
    check("a reply for alex waits while his machine is off", "not connected" in out or "no answer" in out, out)
    out = cli("alice", "send", "alex/hooked-claude", "--from", "alice/desk", "for alex's hooked session while he is off (e2e test, ignore)")
    check("…and a message for his hooked session", "not connected" in out or "no answer" in out, out)
    sent_back = outbox("alice")
    check("…in alice's outbox", len(sent_back) == 2, sent_back)
    back = {json.loads(r[2])["to"]: r[0] for r in sent_back}
    cli("alex", "team")
    linked("alice", "alex")
    left = until(lambda: outbox("alice"), lambda o: not o)
    check("alex comes back: alice's outbox empties", left == [], left)
    out = cli("alex", "listen", "--as", "lap", "--wait", "5")
    check("listen shows the reply once, naming what it answers",
          times(out, back["alex/lap"]) == 1 and f'reply_to="{ids["alice/pull-codex"]}"' in out, out)
    out = hook("alex", "claude-code", "stop", "alex-conv", HOOKED)
    check("alex's hooked session gets its message once", times(out, back["alex/hooked-claude"]) == 1, out)

    # --- 2. both daemons restart after delivery: nothing comes again ---
    pull.close()
    cli("alex", "stop")
    cli("alice", "stop")
    time.sleep(1)
    cli("alice", "team")
    cli("alex", "team")
    linked("alex", "alice")
    linked("alice", "alex")
    time.sleep(35)  # past the next outbox flush (every 30 s)
    pull = Mcp("alice", PULL)
    out = pull.tool("check_inbox", {})
    check("after both restart: check_inbox has nothing again", out.startswith("No messages."), out)
    shown = hook("alice", "codex", "session-start", "push-thread", PUSH) + hook("alice", "codex", "stop", "push-thread", PUSH)
    time.sleep(2)
    check("…nor the pushed session", ids["alice/push-codex"] not in shown and times(queued(), ids["alice/push-codex"]) == 1, (shown, queued()))
    shown = hook("alice", "claude-code", "session-start", "alice-conv", HOOKED) + hook("alice", "claude-code", "stop", "alice-conv", HOOKED)
    check("…nor the hooked session", ids["alice/hooked-claude"] not in shown, shown)
    out = cli("alice", "listen", "--as", "desk", "--wait", "1")
    check("…nor listen", out.startswith("No messages"), out)
    shown = hook("alex", "claude-code", "session-start", "alex-conv", HOOKED) + hook("alex", "claude-code", "stop", "alex-conv", HOOKED)
    out = cli("alex", "listen", "--as", "lap", "--wait", "1")
    check("…nor anything the other way", back["alex/hooked-claude"] not in shown and out.startswith("No messages"), (shown, out))
    check("…and both outboxes are empty", outbox("alex") == [] and outbox("alice") == [], (outbox("alex"), outbox("alice")))

    # --- 3. the acks were lost: the sender resends on reconnect, and no session sees any of it twice ---
    hook("alice", "codex", "stop", "push-thread", PUSH)
    cli("alex", "stop")
    unacked("alex", sent)
    cli("alex", "team")
    linked("alex", "alice")
    left = until(lambda: outbox("alex"), lambda o: not o)
    check("alex resends what he never saw acked, and alice acks it", left == [], left)
    time.sleep(2)
    out = pull.tool("check_inbox", {})
    check("a resend isn't shown again: check_inbox", out.startswith("No messages."), out)
    shown = hook("alice", "codex", "post-tool-use", "push-thread", PUSH) + hook("alice", "codex", "stop", "push-thread", PUSH)
    check("…nor pushed again", ids["alice/push-codex"] not in shown and times(queued(), ids["alice/push-codex"]) == 1, (shown, queued()))
    out = hook("alice", "claude-code", "stop", "alice-conv", HOOKED)
    check("…nor handed to the hooked session", ids["alice/hooked-claude"] not in out, out)
    out = cli("alice", "listen", "--as", "desk", "--wait", "1")
    check("…nor shown by listen", out.startswith("No messages"), out)
    out = cli("alice", "listen", "--as", "later", "--wait", "5")
    check("one that came but wasn't read yet is shown once", times(out, ids["alice/later"]) == 1, out)
    out = cli("alice", "listen", "--as", "later", "--wait", "1")
    check("…and not again", out.startswith("No messages"), out)
    # The other way: alice resends the reply and the message to alex's hooked session.
    cli("alice", "stop")
    unacked("alice", sent_back)
    cli("alice", "team")
    linked("alice", "alex")
    left = until(lambda: outbox("alice"), lambda o: not o)
    check("alice resends, and alex acks it", left == [], left)
    time.sleep(2)
    out = cli("alex", "listen", "--as", "lap", "--wait", "1")
    check("the resent reply isn't shown again", out.startswith("No messages"), out)
    out = hook("alex", "claude-code", "stop", "alex-conv", HOOKED)
    check("…nor the resent message to the hooked session", back["alex/hooked-claude"] not in out, out)
finally:
    if pull:
        pull.close()
    cli("alex", "stop")
    cli("alice", "stop")

log = open(UNEXPECTED).read() if os.path.exists(UNEXPECTED) else ""
check("nothing opened a browser or reached the real notifications", log == "", log)
print(f"\n{sum(results)}/{len(results)} checks passed  (state in {D})")
raise SystemExit(0 if all(results) else 1)
