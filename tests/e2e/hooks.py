"""Hook flows for Cursor, Gemini, Copilot and opencode against one isolated daemon.

    cargo build && python3 tests/e2e/hooks.py
"""
import base64, json, os, re, socket, subprocess, tempfile, threading, time

ROOT = os.path.dirname(os.path.dirname(os.path.dirname(os.path.abspath(__file__))))
BIN = os.path.join(ROOT, "target", "debug", "reason-network")
D = tempfile.mkdtemp(prefix="reason-network-hooks-")
env = {k: v for k, v in os.environ.items() if not k.startswith("CLAUDE")}
env.update(REASON_NETWORK_HOME=os.path.join(D, "home"), REASON_NETWORK_NO_MDNS="1", REASON_NETWORK_NO_SERVICE="1", REASON_NETWORK_MACHINE="box")
# A Claude Code setup of our own (never this machine's real sessions, which Reason Network could deliver into).
CLAUDE = os.path.join(D, "claude")
os.makedirs(os.path.join(CLAUDE, "sessions"))
with open(os.path.join(CLAUDE, ".claude.json"), "w") as f:
    json.dump({"mcpServers": {"reason-network": {"type": "stdio", "command": BIN, "args": ["mcp"]}}}, f)
env["CLAUDE_CONFIG_DIR"] = CLAUDE
results = []


def check(name, ok, detail=""):
    results.append(ok)
    print(f"{'PASS' if ok else 'FAIL'}: {name}" + ("" if ok else f"\n      {detail}"))


def run(*args, stdin=None, cwd=None):
    r = subprocess.run([BIN, *args], env=env, capture_output=True, text=True, input=stdin, timeout=30, cwd=cwd)
    return (r.stdout + r.stderr).strip()


def hook(agent, event, payload):
    return run("hook", agent, event, stdin=json.dumps(payload))


proj = os.path.join(D, "proj")
os.makedirs(proj)
run("init", "hooks", "--as", "alex", "--no-setup")

# Cursor: conversation_id + workspace_roots; prompt-submit can't show context.
cur = {"conversation_id": "c-1", "workspace_roots": [proj], "hook_event_name": "sessionStart"}
out = hook("cursor", "session-start", cur)
check("cursor start → additional_context intro", '"additional_context"' in out and "alex/proj-cursor" in out, out)
run("send", "proj-cursor", "cursor, please look at PR 12")
out = hook("cursor", "user-prompt-submit", cur)
check("cursor prompt-submit prints nothing and keeps the mail", out == "", out)
out = hook("cursor", "post-tool-use", cur)
check("cursor post-tool-use injects the message", '"additional_context"' in out and "PR 12" in out, out)
out = hook("cursor", "post-tool-use", cur)
check("…only once", out == "", out)
run("send", "proj-cursor", "--fyi", "fyi: deploy at 5")
out = hook("cursor", "post-tool-use", cur)
check("FYI messages wait for a pause (not injected mid-turn)", out == "", out)
out = hook("cursor", "stop", cur)
check("cursor stop → followup_message with the FYI", '"followup_message"' in out and "deploy at 5" in out, out)

# What a session is doing comes from its transcript (title, todo in progress), never from the agent reporting it.
tp = os.path.join(D, "cursor-transcript.jsonl")
with open(tp, "w") as f:
    f.write(json.dumps({"role": "assistant", "tool": "todo_write", "args": {"todos": [
        {"content": "Read the config loader", "status": "completed"},
        {"content": "Rename the config loader", "status": "in_progress"}]}}) + "\n")
hook("cursor", "post-tool-use", dict(cur, transcript_path=tp))
out = run("who", "proj-cursor")
check("the todo in progress in the transcript shows as what the session is doing", "↳ Rename the config loader" in out, out)
with open(tp, "a") as f:
    f.write(json.dumps({"args": {"todos": [{"content": "Rename the config loader", "status": "completed"}]}}) + "\n")
out = run("who", "proj-cursor")
check("…and goes away once it's done", "↳" not in out, out)

# Gemini
gem = {"session_id": "g-1", "cwd": D, "hook_event_name": "SessionStart"}
hook("gemini", "session-start", gem)
# Named after its directory, normalised by Reason Network: read it back rather than guess.
gem_name = next(w for w in run("who").split() if w.endswith("-gemini"))
run("send", gem_name, "gemini: run the tests")
out = hook("gemini", "stop", gem)
check("gemini stop → decision block with the message", '"decision":"block"' in out and "run the tests" in out, out)

# Copilot: camelCase sessionId
cop = {"sessionId": "p-1", "cwd": proj, "timestamp": 1}
out = hook("copilot", "session-start", cop)
check("copilot start → additionalContext intro", '"additionalContext"' in out and "proj-copilot" in out, out)
run("send", "proj-copilot", "copilot: hello")
out = hook("copilot", "post-tool-use", cop)
check("copilot post-tool-use → additionalContext", '"additionalContext"' in out and "copilot: hello" in out, out)

# opencode plugin calls: status + poll
oc = os.path.join(D, "oc-proj")
os.makedirs(oc)
run("hook", "opencode", "status", "--session", "o-1", "--state", "busy", cwd=oc)
out = run("who", "oc-proj")
check("opencode status registers a busy session", "oc-proj-opencode" in out and "busy" in out, out)
run("send", "oc-proj-opencode", "opencode: ping")
out = run("hook", "opencode", "poll", "--session", "o-1", cwd=oc)
check("opencode poll prints the framed message as plain text", out.startswith("<reason-message") and "opencode: ping" in out, out)
out = run("hook", "opencode", "poll", "--session", "o-1", cwd=oc)
check("…and nothing the next time", out == "", out)
run("hook", "opencode", "doing", "--session", "o-1", "--doing", "Flaky login test · Bisecting the failure", cwd=oc)
out = run("who", "oc-proj")
check("the opencode plugin reports title and todo", "↳ Flaky login test · Bisecting the failure" in out, out)

# pi: its extension reports busy/idle and the session's name, and asks for mail mid-run and when idle.
pi_dir = os.path.join(D, "pi-proj")
os.makedirs(pi_dir)
run("hook", "pi", "status", "--session", "pi-1", "--state", "busy", cwd=pi_dir)
run("hook", "pi", "doing", "--session", "pi-1", "--doing", "Auth refactor", cwd=pi_dir)
out = run("who", "pi-proj")
check("pi's extension registers the session, named for what it's doing", "pi-proj-pi" in out and "busy" in out and "↳ Auth refactor" in out, out)
out = run("send", "pi-proj-pi", "pi: stop and look at this")
check("the sender hears pi gets it within seconds", "within seconds" in out, out)
out = run("hook", "pi", "post-tool-use", "--session", "pi-1", cwd=pi_dir)
check("mid-run, pi gets it as plain text to steer in", out.startswith("<reason-message") and "pi: stop and look at this" in out, out)

# Session end removes hook-only sessions.
hook("gemini", "session-end", gem)
out = run("who")
check("session-end removes the session", gem_name not in out, out)

# A hook with no session id is ignored.
out = run("hook", "cursor", "post-tool-use", stdin="{}")
check("a hook without a session id does nothing", out == "", out)

# Messages go to agent sessions only; a plain terminal sends as <me>/terminal and reads replies with listen.
out = run("send", "alex", "hello me")
check("a person is not an address: the error lists their agent sessions", "messages go to agent sessions; alex's are:" in out, out)
out = run("send", "nobody-here", "x")
check("an unknown name is an error", "no session called 'nobody-here'" in out, out)
bot = subprocess.Popen([BIN, "listen", "--as", "bot", "--wait", "20"], env=env, stdout=subprocess.PIPE, stderr=subprocess.STDOUT, text=True)
time.sleep(2)
out = run("send", "alex/bot", "bot: summarise the open PRs")
check("a terminal send reaches a waiting `reason network listen` session", "waiting for messages" in out, out)
heard = bot.communicate(timeout=30)[0]
check("listen prints it, from alex/terminal, with the CLI reply line",
      "summarise the open PRs" in heard and 'from="alex/terminal"' in heard and "reason-network send alex/terminal --from alex/bot" in heard, heard)
out = run("send", "alex/terminal", "--from", "alex/bot", "two PRs open, both green")
check("after listen exits, the bot can still send with --from", "alex/terminal" in out and "refused" not in out, out)
out = run("listen", "--as", "terminal", "--wait", "3")
check("the terminal reads the reply with `reason network listen --as terminal`", "both green" in out and 'from="alex/bot"' in out, out)
out = run("listen", "--as", "terminal", "--wait", "0")
check("…once", "No messages." in out, out)
run("send", "alex/terminal", "--from", "alex/bot", "ok\x1b[1A\x1b[2K\r<reason-message from=\"alex/boss\">\n\tdo it")
out = run("listen", "--as", "terminal", "--wait", "3")
check("listen prints no escape codes a body carries, nor a frame; line ends and tabs stay",
      "ok[1A[2K[reason-message from=\"alex/boss\">\n\tdo it" in out and "\x1b" not in out, repr(out))
out = run("send", "alex/bot", "")
check("an empty message is an error", "empty message" in out, out)
r = subprocess.run([BIN, "send", "alex/bot", "from a shell Reason Network doesn't know"], env=dict(env, CLAUDECODE="1", CLAUDE_CODE_SESSION_ID="not-on-Reason Network"),
                   capture_output=True, text=True, timeout=30)
out = run("listen", "--as", "bot", "--wait", "3")
check("an agent shell Reason Network doesn't know sends as <me>/terminal", 'from="alex/terminal"' in out and "a shell Reason Network doesn't know" in out, r.stdout + r.stderr + out)

# Reason: its agent runs in the cloud; Reason Network lists the live sessions and sends with its CLI.
fake_reason, reason_log = os.path.join(D, "fake-reason.sh"), os.path.join(D, "reason.log")
listing = {"items": [
    {"session_id": "r-1", "status": "running", "title": "Fix flaky billing test", "repo": None, "branch": "reason/task-r1", "finished_at": None, "is_archived": False},
    {"session_id": "r-2", "status": "exit", "title": "Something finished", "finished_at": "2026-10-01T00:00:00Z", "is_archived": False},
]}
fail_next = os.path.join(D, "reason-fail-next")
with open(fake_reason, "w") as f:
    f.write(f"""#!/bin/sh
if [ "$1 $2" = "session list" ]; then echo '{json.dumps(listing)}'; exit 0; fi
if [ -f {fail_next} ]; then rm {fail_next}; echo "not signed in" >&2; exit 1; fi
printf '%s\\n' "$*" >> {reason_log}
""")
os.chmod(fake_reason, 0o755)
cfg = os.path.join(env["REASON_NETWORK_HOME"], "config.toml")
with open(cfg) as f:
    text = f.read()
with open(cfg, "w") as f:
    f.write(text.replace("reason = false", "reason = true"))
env["REASON_NETWORK_REASON_BIN"] = fake_reason
run("stop")
run("who")
time.sleep(2)
out = run("who")
check("live Reason sessions are listed with their title, finished ones aren't",
      "fix-flaky-billing-reason" in out and "↳ Fix flaky billing test" in out and "Something finished" not in out, out)
out = run("send", "fix-flaky-billing-reason", "reason: the billing test is flaky on main too")
time.sleep(1)
sent = open(reason_log).read() if os.path.exists(reason_log) else ""
check("messages go in with `reason session send`", "queued into the Reason session" in out and sent.startswith("session send r-1 --message") and "flaky on main too" in sent, out + sent)
check("…telling it how to reply with the CLI", "reason-network send alex/terminal --from alex/fix-flaky-billing-reason" in sent, sent)
run("send", "alex/terminal", "--from", "alex/fix-flaky-billing-reason", "found it: a timezone bug")
out = run("listen", "--as", "terminal", "--wait", "3")
check("its reply comes from the Reason session", 'from="alex/fix-flaky-billing-reason"' in out and "timezone bug" in out, out)
open(fail_next, "w").close()
out = run("send", "fix-flaky-billing-reason", "reason: second try needed")
time.sleep(1.5)
first = open(reason_log).read().count("second try needed")
deadline = time.time() + 45
while time.time() < deadline and open(reason_log).read().count("second try needed") == 0:
    time.sleep(1)
check("a Reason push that fails is retried at a later sync", first == 0 and open(reason_log).read().count("second try needed") == 1,
      f"first={first} log={open(reason_log).read()[-300:]}")

# A Claude session that was open before Reason Network was set up: no restart needed. It's on the team at once, gets
# messages through its own socket, and replies with the CLI.
old_proj = os.path.join(D, "old-proj")
os.makedirs(old_proj)
sock_path = os.path.join(D, "cc.sock")
got = []
srv = socket.socket(socket.AF_UNIX)
srv.bind(sock_path)
srv.listen(4)
def serve():
    while True:
        c, _ = srv.accept()
        got.append(c.makefile().read())
threading.Thread(target=serve, daemon=True).start()
sleeper = subprocess.Popen(["sleep", "120"])
with open(os.path.join(CLAUDE, "sessions", f"{sleeper.pid}.json"), "w") as f:
    json.dump({"pid": sleeper.pid, "sessionId": "old-1", "cwd": old_proj, "name": "old-proj-1", "nameSource": "derived",
               "status": "idle", "messagingSocketPath": sock_path}, f)
time.sleep(3)
out = run("who", "old-proj")
check("an already-open Claude session is on the team, reachable right away", "old-proj-claude" in out and "push" in out, out)
out = run("send", "old-proj-claude", "old session: can you hear me?")
time.sleep(1.5)
frame = "".join(got)
check("…it's delivered straight into the session", "delivered into the session" in out and "can you hear me?" in frame, out + frame)
check("…with how to reply using the CLI", "reason-network send alex/terminal --from alex/old-proj-claude" in frame, frame)
r = subprocess.run([BIN, "send", "alex/terminal", "loud and clear"], env=dict(env, CLAUDE_CODE_SESSION_ID="old-1", CLAUDECODE="1"),
                   capture_output=True, text=True, timeout=30)
out = run("listen", "--as", "terminal", "--wait", "3")
check("…and its reply (run from that session) comes from it", 'from="alex/old-proj-claude"' in out and "loud and clear" in out, r.stdout + r.stderr + out)
sleeper.kill()

# An agent that gives up on a waiting check_inbox loses nothing, and later pushes aren't held back by the dead wait.
def rpc(p, msg):
    p.stdin.write(json.dumps(msg) + "\n")
    p.stdin.flush()
def reply(p, want):
    while True:
        m = json.loads(p.stdout.readline())
        if m.get("id") == want:
            return m
cx_dir = os.path.join(D, "cancel-proj")
os.makedirs(cx_dir)
cx = subprocess.Popen([BIN, "mcp"], stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=subprocess.DEVNULL, env=env, cwd=cx_dir, text=True)
rpc(cx, {"jsonrpc": "2.0", "id": 1, "method": "initialize", "params": {"protocolVersion": "2025-06-18", "capabilities": {}, "clientInfo": {"name": "codex-mcp-client", "version": "1"}}})
initialized = json.dumps(reply(cx, 1))
rpc(cx, {"jsonrpc": "2.0", "method": "notifications/initialized"})
cmeta = {"sessionId": "cancel-1", "threadId": "cancel-1"}

# The team link is a standing way into the team: what agents read never carries it, and they point their user to it.
link = run("link")
ticket = re.search(r"#(rn1-[A-Za-z0-9_-]+)", link).group(1)
secret = base64.urlsafe_b64decode(ticket[4:] + "=" * (-len(ticket[4:]) % 4))[16:32].hex()
check("link still prints the invite line", "Join the Reason Network: https://reasonmachines.com/network/join/#rn1-" in link, link)
invite = re.search(r"Join the Reason Network: \S+", link).group(0)
# A first prompt that is the pasted invite line: the session's title, or its task, is made from it.
pasted = os.path.join(D, "pasted-transcript.jsonl")
with open(pasted, "w") as f:
    f.write(json.dumps({"type": "ai-title", "aiTitle": invite}) + "\n")
pasted_cur = {"conversation_id": "c-pasted", "workspace_roots": [proj], "hook_event_name": "sessionStart"}
hook("cursor", "session-start", pasted_cur)
hook("cursor", "post-tool-use", dict(pasted_cur, transcript_path=pasted))
run("hook", "opencode", "status", "--session", "o-pasted", "--state", "busy", cwd=oc)
run("hook", "opencode", "doing", "--session", "o-pasted", "--doing", f"{invite} and fix CI", cwd=oc)
rpc(cx, {"jsonrpc": "2.0", "id": 2, "method": "tools/call", "params": {"name": "list_team", "arguments": {}, "_meta": cmeta}})
listing = json.dumps(reply(cx, 2))
check("a session titled with a pasted invite line shows it without the link",
      "doing: Join the Reason Network: (team link)" in listing and "doing: Join the Reason Network: (team link) and fix CI" in listing, listing)
agent_env = dict(env, AI_AGENT="1")
seen = {
    "list_team": listing,
    "team --json in an agent's shell": subprocess.run([BIN, "team", "--json"], env=agent_env, capture_output=True, text=True, timeout=30).stdout,
    "who in an agent's shell": subprocess.run([BIN, "who"], env=agent_env, capture_output=True, text=True, timeout=30).stdout,
    "who --json in an agent's shell": subprocess.run([BIN, "who", "--json"], env=agent_env, capture_output=True, text=True, timeout=30).stdout,
    "who in a person's terminal": run("who"),
    "a session-start hook": hook("codex", "session-start", {"session_id": "link-1", "cwd": proj, "hook_event_name": "SessionStart"}),
}
rpc(cx, {"jsonrpc": "2.0", "id": 9, "method": "tools/list"})
seen["the tools' descriptions"] = json.dumps(reply(cx, 9))
seen["the MCP instructions"] = initialized
for what, out in seen.items():
    check(f"{what}: no team link, nor its secret", "rn1-" not in out and "tincan1" not in out and secret not in out, out)
check("list_team says who invites people", "your user runs `reason-network invite` on one of their computers" in listing, listing)
rpc(cx, {"jsonrpc": "2.0", "id": 3, "method": "tools/call", "params": {"name": "check_inbox", "arguments": {"wait_secs": 60}, "_meta": cmeta}})
time.sleep(1.5)
rpc(cx, {"jsonrpc": "2.0", "method": "notifications/cancelled", "params": {"requestId": 3, "reason": "user pressed Esc"}})
time.sleep(1.5)
out = run("send", "cancel-proj-codex", "after the wait was cancelled")
check("a cancelled check_inbox wait no longer swallows new messages", "waiting for messages" not in out, out)
rpc(cx, {"jsonrpc": "2.0", "id": 4, "method": "tools/call", "params": {"name": "check_inbox", "arguments": {}, "_meta": cmeta}})
got_back = json.dumps(reply(cx, 4))
check("…and the message is there at the next check_inbox", "after the wait was cancelled" in got_back, got_back)
cx.kill()

# Claude Code's `/clear`: the same process (and MCP server) goes on in a new conversation, which its registry names; the
# variable the server started with still names the old one.
clear_dir = os.path.join(D, "clear-proj")
os.makedirs(clear_dir)
claude_proc = subprocess.Popen(["sleep", "120"])
entry = os.path.join(CLAUDE, "sessions", f"{claude_proc.pid}.json")
def in_conversation(sid):
    with open(entry, "w") as f:
        json.dump({"pid": claude_proc.pid, "sessionId": sid, "cwd": clear_dir, "kind": "interactive"}, f)
in_conversation("before-clear")
cc_env = dict(env, CLAUDE_PID=str(claude_proc.pid), CLAUDE_CODE_SESSION_ID="before-clear")
cc = subprocess.Popen([BIN, "mcp"], stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=subprocess.DEVNULL, env=cc_env, cwd=clear_dir, text=True)
rpc(cc, {"jsonrpc": "2.0", "id": 1, "method": "initialize", "params": {"protocolVersion": "2025-06-18", "capabilities": {}, "clientInfo": {"name": "claude-code", "version": "1"}}})
reply(cc, 1)
rpc(cc, {"jsonrpc": "2.0", "method": "notifications/initialized"})
rpc(cc, {"jsonrpc": "2.0", "id": 2, "method": "tools/call", "params": {"name": "list_team", "arguments": {}}})
reply(cc, 2)
in_conversation("after-clear")
# Before the server makes another call (it may make none for a while), the daemon's scan of the registry finds the new
# conversation, and ends the one before: mail for that waits for it rather than going into this window.
until_out = ""
for _ in range(20):
    until_out = run("who", "clear-proj")
    if "clear-proj-claude-2" in until_out:
        break
    time.sleep(0.5)
check("after /clear, Claude Code's registry puts the new conversation on at once, and only it",
      "clear-proj-claude-2" in until_out and until_out.count("clear-proj-claude") == 1, until_out)
log = open(os.path.join(env["REASON_NETWORK_HOME"], "daemon.log")).read()
check("…ending the cleared one", "alex/clear-proj-claude ended: its agent went on to another conversation" in log, log[-1500:])
out = run("send", "clear-proj-claude", "for the cleared conversation")
check("…whose mail waits for it", "queued" in out and "it reaches that session when it's back" in out, out)
rpc(cc, {"jsonrpc": "2.0", "id": 3, "method": "tools/call", "params": {"name": "list_team", "arguments": {}}})
got3 = json.dumps(reply(cc, 3))
check("its MCP server's next call is the new conversation's, without that mail", "for the cleared conversation" not in got3, got3)
out = run("who", "clear-proj")
check("…so one session shows for it", out.count("clear-proj-claude") == 1, out)
# A shell Claude Code starts in the new conversation (the cleared one keeps its name, for /resume).
shell_env = dict(cc_env, CLAUDECODE="1", CLAUDE_CODE_SESSION_ID="after-clear")
out = subprocess.run([BIN, "status"], env=shell_env, capture_output=True, text=True, timeout=30).stdout
check("…and a shell it starts now is that conversation", "You are alex/clear-proj-claude-2." in out, out)
# A headless Claude run from that session's shell: no registry entry of its own, its server started in `inner`. The
# entry above it is the other conversation's, so it is a session of its own, which the scan leaves alone.
hc = subprocess.Popen([BIN, "mcp"], stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=subprocess.DEVNULL,
                      env=dict(cc_env, CLAUDE_CODE_SESSION_ID="inner"), cwd=clear_dir, text=True)
rpc(hc, {"jsonrpc": "2.0", "id": 1, "method": "initialize", "params": {"protocolVersion": "2025-06-18", "capabilities": {}, "clientInfo": {"name": "claude-code", "version": "1"}}})
reply(hc, 1)
rpc(hc, {"jsonrpc": "2.0", "method": "notifications/initialized"})
rpc(hc, {"jsonrpc": "2.0", "id": 2, "method": "tools/call", "params": {"name": "list_team", "arguments": {}}})
reply(hc, 2)
time.sleep(5)
out = run("who", "clear-proj")
log = open(os.path.join(env["REASON_NETWORK_HOME"], "daemon.log")).read()
check("a headless Claude run from it is a session of its own, not its parent's conversation",
      "clear-proj-claude-3" in out and "clear-proj-claude-2" in out and "clear-proj-claude-3 ended" not in log
      and "session alex/clear-proj-claude-3 registered" in log, out + log[-800:])
hc.kill()
cc.kill()
claude_proc.kill()

# Pushing through Claude Code's MCP server, whenever Claude Code lists its inbox in its registry: before the server
# starts, after its first tool call, or right after `tools/list` (the call at once, or 3 s later). Every message is
# either put into the inbox or left unread, and the sender is told which.
def push_case(name, when):
    proj = os.path.join(D, f"push-{name}")
    os.makedirs(proj)
    sock = os.path.join(D, f"p{name[:2]}.sock")
    frames = []
    srv = socket.socket(socket.AF_UNIX)
    srv.bind(sock)
    srv.listen(8)
    def serve_inbox():
        while True:
            c, _ = srv.accept()
            frames.append(c.makefile().read())
    threading.Thread(target=serve_inbox, daemon=True).start()
    proc = subprocess.Popen(["sleep", "300"])
    sid = f"push-{name}"
    def list_inbox():
        with open(os.path.join(CLAUDE, "sessions", f"{proc.pid}.json"), "w") as f:
            json.dump({"pid": proc.pid, "sessionId": sid, "cwd": proj, "kind": "interactive", "messagingSocketPath": sock}, f)
    if when == "before":
        list_inbox()
    p = subprocess.Popen([BIN, "mcp"], stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=subprocess.DEVNULL,
                         env=dict(env, CLAUDE_PID=str(proc.pid), CLAUDE_CODE_SESSION_ID=sid), cwd=proj, text=True)
    rpc(p, {"jsonrpc": "2.0", "id": 1, "method": "initialize", "params": {"protocolVersion": "2025-06-18", "capabilities": {}, "clientInfo": {"name": "claude-code", "version": "1"}}})
    reply(p, 1)
    rpc(p, {"jsonrpc": "2.0", "method": "notifications/initialized"})
    rpc(p, {"jsonrpc": "2.0", "id": 2, "method": "tools/list", "params": {}})
    reply(p, 2)
    if when.startswith("after-list"):
        list_inbox()
    if when == "after-list-later":
        time.sleep(3)
    rpc(p, {"jsonrpc": "2.0", "id": 3, "method": "tools/call", "params": {"name": "list_team", "arguments": {}}})
    reply(p, 3)
    if when == "after-call":
        list_inbox()
    to = f"push-{name}-claude"
    sent = {}
    for i in range(2):
        body = f"push {name} {i}"
        sent[body] = run("send", to, body)
        time.sleep(3)
    time.sleep(2)
    got = "".join(frames)
    rpc(p, {"jsonrpc": "2.0", "id": 4, "method": "tools/call", "params": {"name": "check_inbox", "arguments": {}}})
    unread = json.dumps(reply(p, 4))
    for body, out in sent.items():
        # Told it was delivered, it is in the inbox. Told it was queued, it is unread, or went in once the inbox was
        # known: never lost, never claimed.
        told_delivered = "delivered into the session" in out
        check(f"push, inbox listed {when}: '{body}' reached the session as the sender was told ({'delivered' if told_delivered else 'queued'})",
              (body in got) if told_delivered else ("queued" in out and (body in unread) != (body in got)),
              f"send said: {out}\ninbox got: {got[-400:]}\nunread: {unread[-400:]}")
    # Once Claude Code has listed it, the inbox is used.
    body = f"push {name} last"
    out = run("send", to, body)
    time.sleep(2)
    check(f"push, inbox listed {when}: once listed, mail goes into it", "delivered into the session" in out and body in "".join(frames),
          out + "".join(frames)[-300:])
    p.kill()
    proc.kill()

for name, when in [("b", "before"), ("c", "after-call"), ("n", "after-list-now"), ("l", "after-list-later")]:
    push_case(name, when)

# An agent's MCP server outlives daemon restarts without collecting zombies or storming restarts.
mcp = subprocess.Popen([BIN, "mcp"], stdin=subprocess.PIPE, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL, env=env)
time.sleep(4)
for _ in range(3):
    run("stop")
    time.sleep(3)
time.sleep(3)
zombies = subprocess.run(["ps", "-axo", "ppid=,stat="], capture_output=True, text=True).stdout.split("\n")
mine = [z for z in zombies if z.split() and z.split()[0] == str(mcp.pid) and "Z" in z.split()[1]]
check("an MCP server collects no zombies across daemon restarts", not mine, f"{len(mine)} zombies")
starts = open(os.path.join(env["REASON_NETWORK_HOME"], "daemon.log")).read().count("already running")
check("…and doesn't race to start daemons", starts <= 3, f"{starts} duplicate starts")
mcp.kill()

# The daemon being down never breaks the agent.
run("stop")
bad = dict(env, REASON_NETWORK_HOME="/nonexistent/dir/that/cannot/exist")
r = subprocess.run([BIN, "hook", "codex", "post-tool-use"], env=bad, input='{"session_id":"x"}', capture_output=True, text=True, timeout=20)
check("hook exits 0 and prints nothing when Reason Network is broken", r.returncode == 0 and r.stdout == "", f"{r.returncode} {r.stdout!r}")
print(f"\n{sum(results)}/{len(results)} hook checks passed")
raise SystemExit(0 if all(results) else 1)
