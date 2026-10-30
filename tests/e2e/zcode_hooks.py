"""ZCode hook protocol against an isolated real daemon (not a Desktop simulation).

    cargo build && python tests/e2e/zcode_hooks.py

Requires psutil for suspending only our own test daemon during timeout fault injection.
"""
import json
import os
from pathlib import Path
import queue
import shutil
import sqlite3
import subprocess
import tempfile
import threading
import time

import psutil

ROOT = Path(__file__).resolve().parents[2]
SOURCE = ROOT / "target/debug" / ("reason-network.exe" if os.name == "nt" else "reason-network")
DIR = Path(tempfile.mkdtemp(prefix="reason-zcode-hooks-用户 空格-"))
BIN = DIR / "程序 空格" / SOURCE.name
BIN.parent.mkdir()
shutil.copy2(SOURCE, BIN)
HOME = DIR / "profile"
(HOME / ".zcode/cli").mkdir(parents=True)
STATE = DIR / "network state"
ENV = {k: v for k, v in os.environ.items() if not k.startswith(("CLAUDE", "CODEX", "GROK", "REASON_", "TINCAN_"))}
ENV.update(HOME=str(HOME), USERPROFILE=str(HOME), APPDATA=str(HOME / "AppData/Roaming"),
           LOCALAPPDATA=str(HOME / "AppData/Local"), PATH="", REASON_NETWORK_HOME=str(STATE),
           REASON_NETWORK_NO_MDNS="1", REASON_NETWORK_NO_SERVICE="1", REASON_NETWORK_NO_UPDATE="1",
           REASON_NETWORK_NO_TRAY="1", REASON_NETWORK_MACHINE="zcode-test")
RESULTS = []


def check(name, ok):
    RESULTS.append(ok)
    print(("PASS: " if ok else "FAIL: ") + name, flush=True)
    assert ok, name


def run(*args, data=None, env=ENV):
    r = subprocess.run([str(BIN), *args], input=data, env=env, cwd=DIR,
                       capture_output=True, text=True, encoding="utf-8", timeout=12)
    assert r.returncode == 0, r.stderr
    return r.stdout


def hook(event="post-tool-use", session="one", data=None):
    payload = {"session_id": session, "cwd": str(DIR),
               "hook_event_name": "SessionStart" if event == "session-start" else "PostToolUse"}
    return run("hook", "zcode", event, "--network-home", str(STATE), data=json.dumps(payload) + "\n" if data is None else data)


class Mcp:
    def __init__(self):
        self.p = subprocess.Popen([str(BIN), "mcp"], env=dict(ENV, REASON_NETWORK_AGENT="zcode"), cwd=DIR,
                                  stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=subprocess.DEVNULL,
                                  text=True, encoding="utf-8")
        self.q = queue.Queue()
        threading.Thread(target=lambda: [self.q.put(line) for line in self.p.stdout], daemon=True).start()
        self.seq = 0
        self.request("initialize", {"protocolVersion": "2024-11-05", "capabilities": {}, "clientInfo": {"name": "zcode", "version": "test"}})
        self.p.stdin.write(json.dumps({"jsonrpc": "2.0", "method": "notifications/initialized"}) + "\n")
        self.p.stdin.flush()

    def request(self, method, params):
        self.seq += 1
        self.p.stdin.write(json.dumps({"jsonrpc": "2.0", "id": self.seq, "method": method, "params": params}) + "\n")
        self.p.stdin.flush()
        while True:
            answer = json.loads(self.q.get(timeout=12))
            if answer.get("id") == self.seq:
                assert "error" not in answer, answer
                return answer["result"]

    def tool_result(self, name, arguments=None, session="one", workspace=DIR):
        params = {"name": name, "arguments": arguments or {}}
        if session is not None:
            params["_meta"] = {"com.zcode/request-context": {"session_id": session, "workspace_path": str(workspace)}}
        return self.request("tools/call", params)

    def tool(self, name, arguments=None, session="one", workspace=DIR):
        result = self.tool_result(name, arguments, session, workspace)
        assert not result.get("isError"), result
        return "\n".join(c.get("text", "") for c in result.get("content", []))

    def close(self):
        self.p.stdin.close()
        self.p.wait(timeout=5)


def unread():
    with sqlite3.connect((STATE / "network.db").as_uri() + "?mode=ro", uri=True) as db:
        return db.execute("SELECT COUNT(*) FROM messages WHERE recipient=? AND read_at IS NULL", (address,)).fetchone()[0]


client = None
try:
    start = time.monotonic()
    check("offline hook exits without starting a daemon", hook("session-start") == "" and not (STATE / "daemon.lock").exists())
    check("offline hook stays below host budget", time.monotonic() - start < 3)
    run("init", "zcode-hooks", "--as", "alex", "--no-setup")
    # init changes identity and restarts the bootstrap daemon. Hooks deliberately do not start or await it.
    check("team daemon is ready after bootstrap restart", "alex" in run("who"))
    intro = json.loads(hook("session-start"))["hookSpecificOutput"]
    check("SessionStart returns valid context JSON", intro["hookEventName"] == "SessionStart")
    address = intro["additionalContext"].split("you are ", 1)[1].split(".", 1)[0]
    other = json.loads(hook("session-start", "two"))["hookSpecificOutput"]["additionalContext"]
    other_address = other.split("you are ", 1)[1].split(".", 1)[0]
    check("two real session ids receive distinct addresses", address != other_address)
    listing = run("who")
    check("state wrapper keeps the real host pid alive", all(a.split('/',1)[1] in listing for a in [address,other_address]))
    check("no-message PostToolUse is empty", hook() == "")
    check("invalid input and unsupported Stop are harmless", hook(data="{\n") == "" and hook("stop") == "")
    run("send", address, "消息一 unique-one")
    run("send", address, "message two unique-two")
    check("SessionStart never takes pending mail", "unique-one" not in hook("session-start"))
    check("another session cannot take this mail", hook(session="two") == "")
    output = hook()
    context = json.loads(output)["hookSpecificOutput"]
    check("PostToolUse injects both messages with Unicode intact", context["hookEventName"] == "PostToolUse" and "消息一" in context["additionalContext"] and "unique-two" in output)
    check("stdout is not marked read", unread() == 2)
    check("repeated hooks do not reinject leased messages", hook() == "" and hook() == "")
    # Discarded stdout stands for a host rejecting JSON or a killed hook after leasing. Restart must recover it.
    run("stop")
    run("who")
    recovered = hook()
    check("unaccepted output survives daemon restart", "unique-one" in recovered and "unique-two" in recovered and unread() == 2)
    run("stop")
    run("who")
    check("second restart does not reset the single-retry limit", hook() == "" and unread() == 2)
    client = Mcp()
    listing = client.tool("list_team")
    check("MCP attaches to the exact hook session", address.split("/", 1)[1] in listing and "← you" in listing)
    check("explicit inbox recovers leased messages immediately", "unique-one" in client.tool("check_inbox") and unread() == 0)
    check("confirmed messages are not reinjected", hook() == "")
    run("send", address, "confirm-with-reply")
    pending = json.loads(hook())["hookSpecificOutput"]["additionalContext"]
    message_id = pending.split(' id="', 1)[1].split('"', 1)[0]
    reply_args = {"to": "alex/terminal", "message": "received", "reply_to": message_id,
                  "idempotency_key": "zcode-correlated-reply"}
    sent = client.tool("send_message", reply_args)
    check("correlated model reply confirms receipt once", unread() == 0 and hook() == "")
    retried = client.tool("send_message", reply_args)
    ids = [line.split()[-1] for line in sent.splitlines() if line.startswith("message id ")]
    retried_ids = [line.split()[-1] for line in retried.splitlines() if line.startswith("message id ")]
    with sqlite3.connect((STATE / "network.db").as_uri() + "?mode=ro", uri=True) as db:
        copies = db.execute("SELECT COUNT(*) FROM messages WHERE sender=? AND reply_to=?", (address, message_id)).fetchone()[0]
    check("ZCode reply retries retain upstream idempotent receipts", len(ids) == 1 and ids == retried_ids and copies == 1)
    receipts = json.loads(client.tool("message_status", {"ids": ids}))
    check("ZCode can query its outgoing delivery receipt", len(receipts) == 1 and receipts[0]["id"] == ids[0]
          and receipts[0]["stage"] in ("queued", "accepted", "handed_to_agent"))
    sent = client.tool("send_message", {"to": address, "message": "recipient-receipt"}, session="sender")
    incoming_ids = [line.split()[-1] for line in sent.splitlines() if line.startswith("message id ")]
    check("hook stdout alone does not report agent handoff",
          "recipient-receipt" in hook() and json.loads(client.tool("message_status", {"ids": incoming_ids}, session="sender"))[0]["stage"] == "queued")
    client.tool("check_inbox")
    check("ZCode inbox read after a hook records agent handoff",
          json.loads(client.tool("message_status", {"ids": incoming_ids}, session="sender"))[0]["stage"] == "handed_to_agent")
    check("legacy list_team works without request metadata", "you" in client.tool("list_team", session=None))
    check("legacy send works without request metadata", "message id" in client.tool("send_message", {"to":"alex/terminal", "message":"legacy"}, session=None))
    run("send", address, "not-for-legacy-process")
    check("legacy inbox cannot consume a metadata session's mail", "not-for-legacy-process" not in client.tool("check_inbox", session=None) and unread() == 1)
    client.tool("check_inbox")
    partial = client.request("tools/call", {"name":"list_team", "arguments":{}, "_meta":{"com.zcode/request-context":{"session_id":"one"}}})
    check("incomplete metadata does not silently fall back to another workspace", partial.get("isError") is True)
    run("send", address, "ack-bookkeeping-failure")
    pending = json.loads(hook())["hookSpecificOutput"]["additionalContext"]
    pending_id = pending.split(' id="', 1)[1].split('"', 1)[0]
    with sqlite3.connect(STATE / "network.db") as db:
        db.execute("CREATE TRIGGER fail_ack BEFORE UPDATE OF read_at ON messages BEGIN SELECT RAISE(FAIL, 'test bookkeeping failure'); END")
    try:
        check("ack bookkeeping failure does not fail an already stored send", "message id" in client.tool("send_message", {"to":"alex/terminal", "message":"still stored", "reply_to":pending_id}))
    finally:
        with sqlite3.connect(STATE / "network.db") as db:
            db.execute("DROP TRIGGER fail_ack")
    client.tool("check_inbox")
    started = time.monotonic()
    client.tool("check_inbox", {"wait_secs": 60})
    check("ZCode inbox waits are bounded below 30 seconds", time.monotonic() - started < 8)
    run("send", address, "recover-after-timeout")
    daemon_pid = int((STATE / "daemon.lock").read_text().strip())
    daemon = psutil.Process(daemon_pid)
    assert Path(daemon.exe()).resolve() == BIN.resolve(), "Only suspend our isolated test daemon"
    daemon.suspend()
    try:
        started = time.monotonic()
        check("stalled daemon times out without hook output", hook() == "")
        check("stalled hook stays below three seconds", time.monotonic() - started < 3)
    finally:
        daemon.resume()
    check("timeout never marks the pending message read", unread() == 1)
    run("stop")
    run("who")
    check("message recovers after timeout and restart", "recover-after-timeout" in hook() and unread() == 1)
    client.tool("check_inbox")
    check("recovered message can be acknowledged normally", unread() == 0)
    # Hook output obeys user config. Later messages must not silently overtake an oversized head.
    native = HOME / ".zcode/cli/config.json"
    native.write_text(json.dumps({"hooks":{"enabled":True,"maxOutputBytes":512}}), encoding="utf-8")
    run("send", address, "-", data="大" * 9000)
    run("send", address, "later-small-message")
    small = hook()
    check("lowered maxOutputBytes emits bounded inbox recovery context", len(small.encode("utf-8")) <= 512 and "check_inbox" in small and "later-small-message" not in small)
    check("a leased oversized message still prevents later hook mail overtaking it", hook() == "" and unread() == 2)
    run("stop")
    run("who")
    check("oversized recovery notice gets only one retry", "check_inbox" in hook())
    run("stop")
    run("who")
    check("oversized recovery retry bound persists", hook() == "" and unread() == 2)
    recovered = client.tool("check_inbox")
    check("explicit inbox recovers oversized and later mail in order", recovered.index("大" * 10) < recovered.index("later-small-message") and unread() == 0)
    native.write_text(json.dumps({"hooks":{"enabled":True,"maxOutputBytes":1}}), encoding="utf-8")
    run("send", address, "below-json-minimum")
    check("a cap below JSON overhead neither truncates output nor consumes an attempt", hook() == "" and unread() == 1)
    native.write_text("{}", encoding="utf-8")
    check("mail remains injectable after restoring output budget", "below-json-minimum" in hook())
    client.tool("check_inbox")
    private = DIR / "private workspace"
    private.mkdir()
    run("folder", str(private), "off")
    private_payload = json.dumps({"session_id":"private", "cwd":str(private), "hook_event_name":"SessionStart"})+'\n'
    check("hooks respect upstream folder opt-out", hook("session-start", data=private_payload) == "")
    check("MCP respects the actual metadata workspace opt-out", client.tool_result("list_team", session="private", workspace=private).get("isError") is True)
    own = DIR / "active workspace"
    own.mkdir()
    (own / "note.txt").write_text("within calling workspace", encoding="utf-8")
    outside = DIR / "outside.txt"
    outside.write_text("not in calling workspace", encoding="utf-8")
    sent = client.tool("send_message", {"to":"alex/terminal","message":"safe file","files":["note.txt"]}, session="files", workspace=own)
    check("files resolve relative to the calling ZCode workspace", "message id" in sent)
    denied = client.tool_result("send_message", {"to":"alex/terminal","message":"no escape","files":[str(outside)]}, session="files", workspace=own)
    check("files cannot escape the calling ZCode workspace", denied.get("isError") is True)
    # Installation still works when hooks are absent, disabled or from a newer host schema.
    for bad_hooks in [None, {"enabled":False}, {"events":{"PostToolUse":[{"unexpected":"shape"}]}},
                      {"enabled":True,"events":{"PostToolUse":[None,{"hooks":"unknown"}]}}]:
        native.write_text(json.dumps({"provider":"keep", "hooks":bad_hooks}), encoding="utf-8")
        run("setup", "--no-service")
        installed = json.loads(native.read_text(encoding="utf-8"))
        check("MCP install succeeds with optional hooks " + repr(bad_hooks),
              Path(installed["mcp"]["servers"]["reason-network"]["command"]).resolve() == BIN.resolve()
              and installed["hooks"] == bad_hooks and installed["provider"] == "keep")
        run("setup", "--remove", "--no-service")
        removed = json.loads(native.read_text(encoding="utf-8"))
        check("MCP removal preserves optional hooks " + repr(bad_hooks),
              "reason-network" not in removed["mcp"]["servers"] and removed["hooks"] == bad_hooks)
    client.close()
    client = None
    print(f"{len(RESULTS)} checks passed; isolated fixtures: {DIR}")
finally:
    if client:
        client.close()
    run("stop")
