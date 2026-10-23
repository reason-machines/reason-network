"""Codex delivery routing against an isolated daemon, a queue-accepting fake CLI and fake rollouts.

Mail to a Codex turn that is still running waits for its next PostToolUse (or Stop), however long the turn runs.
Mail to a thread whose last turn ended (completed, interrupted or failed: the last two run no Stop hook) wakes it
through `codex queue`. Each fake rollout records turns the way Codex does (`task_started`, then `task_complete` or
`turn_aborted`); the fake CLI records queue commands. Neither proves what real Codex shows.

    cargo build && python3 tests/e2e/codex_busy.py     # about 2.5 minutes: it waits out real two-minute gaps
"""
import json
import os
import queue
import socket
import sqlite3
import subprocess
import sys
import tempfile
import threading
import time

ROOT = os.path.dirname(os.path.dirname(os.path.dirname(os.path.abspath(__file__))))
BIN = os.path.join(ROOT, "target", "debug", "reason-network")
LONG = 130  # past two minutes, the bound the hooks alone could tell

results = []
lock = threading.Lock()


def check(name, ok, detail=""):
    with lock:
        results.append(ok)
        print(f"{'PASS' if ok else 'FAIL'}: {name}" + ("" if ok else f"\n      {detail}"), flush=True)


with tempfile.TemporaryDirectory(prefix="rn-codex-") as d:
    queue_log = os.path.join(d, "queue.log")
    fake = os.path.join(d, "codex")
    with open(fake, "w") as f:
        f.write('#!/bin/sh\nprintf "%s\\n" "$*" >> "$CODEX_QUEUE_LOG"\n')
    os.chmod(fake, 0o755)
    env = {k: v for k, v in os.environ.items() if not k.startswith(("CLAUDE", "CODEX", "REASON_NETWORK", "TINCAN"))}
    env.update(REASON_NETWORK_HOME=os.path.join(d, "home"), REASON_NETWORK_NO_MDNS="1",
               REASON_NETWORK_NO_SERVICE="1", REASON_NETWORK_MACHINE="box",
               REASON_NETWORK_CODEX_BIN=fake, CODEX_QUEUE_LOG=queue_log)

    def run(*args, payload=None, cwd=d):
        r = subprocess.run([BIN, *args], env=env, cwd=cwd, input=json.dumps(payload) if payload else None,
                           capture_output=True, text=True, timeout=30)
        assert r.returncode == 0, r.stdout + r.stderr
        return (r.stdout + r.stderr).strip()

    class Thread:
        """One Codex thread: its folder (so its name, `<folder>-codex`), its hooks and its rollout."""

        def __init__(self, name, transcript=True):
            self.name, self.cwd = f"{name}-codex", os.path.join(d, name)
            os.makedirs(self.cwd)
            self.rollout = os.path.join(d, f"rollout-{name}.jsonl") if transcript else None
            if self.rollout:
                self.record("session_meta", top=True)

        def record(self, kind, top=False, **extra):
            line = {"timestamp": "2026-10-22T00:00:00.000Z"}
            if top:
                line.update(type=kind, payload={"id": self.name})
            else:
                line.update(type="event_msg", payload={"type": kind, **extra})
            with open(self.rollout, "a") as f:
                f.write(json.dumps(line) + "\n")

        def hook(self, event):
            payload = {"session_id": f"thread-{self.name}", "cwd": self.cwd}
            if self.rollout:
                payload["transcript_path"] = self.rollout
            return run("hook", "codex", event, payload=payload, cwd=self.cwd)

        def start_turn(self):
            self.record("task_started", turn_id="t")
            self.hook("user-prompt-submit")

        def send(self, body, *flags):
            return run("send", self.name, *flags, body)

    def queued(body=None):
        """How many queue commands carried `body`; without one, the bodies queued so far (for failure details)."""
        text = open(queue_log).read() if os.path.exists(queue_log) else ""
        if body is not None:
            return text.count(body)
        return [line for line in text.splitlines() if line and not line.startswith(("queue ", "<", "From "))]

    def queued_within(body, secs):
        deadline = time.monotonic() + secs
        while not queued(body) and time.monotonic() < deadline:
            time.sleep(0.05)
        return queued(body) > 0

    def read(body, secs=5):
        with sqlite3.connect(os.path.join(env["REASON_NETWORK_HOME"], "network.db")) as db:
            deadline = time.monotonic() + secs
            while True:
                row = db.execute("SELECT read_at FROM messages WHERE body = ?", (body,)).fetchone()
                if (row and row[0] is not None) or time.monotonic() >= deadline:
                    return bool(row and row[0] is not None)
                time.sleep(0.05)

    def shown_once(t, body, event="post-tool-use"):
        out = t.hook(event)
        again = t.hook("post-tool-use")
        return body in out and body not in again, f"{event}: {out!r}; next: {again!r}"

    # Each scenario runs in its own thread against the one daemon, so the real waits overlap.

    def busy():
        t = Thread("busy")
        t.start_turn()
        t.send("busy message")
        run("who")  # a queue command, if one were started, finishes meanwhile
        ok, detail = shown_once(t, "busy message")
        check("busy: the next PostToolUse shows the message, once", ok and '"PostToolUse"' in detail, detail)
        check("busy: …and it is not queued as well", queued("busy message") == 0, queued())

    def idle():
        t = Thread("idle")
        t.start_turn()
        t.hook("post-tool-use")
        t.hook("stop")
        t.record("task_complete", turn_id="t")
        out = t.send("idle message")
        check("idle: the sender hears it is queued into Codex", "queued into the Codex session" in out, out)
        check("idle: woken through codex queue", queued_within("idle message", 5), queued())
        check("idle: the queue's success counts as delivered", read("idle message"))
        check("idle: …and no hook shows it again", t.hook("post-tool-use") == "")

    def ending():
        # Mail that lands between Stop and the turn's end in the rollout.
        t = Thread("ending")
        t.start_turn()
        t.hook("post-tool-use")
        t.hook("stop")
        t.send("ending message")
        time.sleep(0.5)
        t.record("task_complete", turn_id="t")
        check("turn ending: queued once the rollout records the end", queued_within("ending message", 10), queued())
        check("turn ending: …once", read("ending message") and queued("ending message") == 1, queued())

    def interrupted():
        t = Thread("interrupted")
        t.start_turn()
        t.hook("post-tool-use")
        t.record("turn_aborted", turn_id="t", reason="interrupted")  # no Stop
        t.send("after interrupt")
        check("interrupted: new mail wakes through codex queue right away", queued_within("after interrupt", 10),
              queued())
        check("interrupted: …once, and no hook shows it again",
              read("after interrupt") and queued("after interrupt") == 1 and t.hook("post-tool-use") == "")

    def interrupted_pending():
        t = Thread("pending")
        t.start_turn()
        t.hook("post-tool-use")
        bodies = [f"waiting at interrupt {i}" for i in range(3)]
        for body in bodies:
            t.send(body)
        time.sleep(1)
        check("interrupted with mail waiting: held for the hook while the turn runs",
              all(queued(b) == 0 for b in bodies), queued())
        t.record("turn_aborted", turn_id="t", reason="interrupted")  # no Stop
        check("interrupted with mail waiting: queued within seconds of the interrupt",
              all(queued_within(b, 10) for b in bodies), queued())
        time.sleep(3)
        check("interrupted with mail waiting: each once, in one queue command",
              all(read(b) and queued(b) == 1 for b in bodies) and queued("--thread thread-pending-codex ") == 1,
              queued())
        check("interrupted with mail waiting: no hook shows it again", t.hook("post-tool-use") == "")

    def daemon(request):
        """One request straight over the daemon's socket, as a hook client sends it; returns its result."""
        with socket.socket(socket.AF_UNIX) as sock:
            sock.settimeout(30)
            sock.connect(os.path.join(env["REASON_NETWORK_HOME"], "network.sock"))
            sock.sendall((json.dumps({"id": 1, **request}) + "\n").encode())
            reply = json.loads(sock.makefile().readline())
        assert reply.get("ok"), reply
        return reply.get("result")

    def unconfirmed():
        # A PostToolUse showed the mail, and its confirmation comes only after the turn was interrupted: while it may
        # still come, the mail is left to that hook, not queued as well (which would show it twice).
        t = Thread("late")
        t.start_turn()
        t.hook("post-tool-use")
        t.send("confirmed late")
        shown = daemon({"op": "hook", "agent": "codex", "event": "tool", "session_id": f"thread-{t.name}",
                        "cwd": t.cwd, "transcript": t.rollout})
        t.record("turn_aborted", turn_id="t", reason="interrupted")  # no Stop
        time.sleep(6)
        daemon({"op": "delivered", "key": f"codex:thread-{t.name}", "ids": shown["ids"]})
        time.sleep(3)
        check("confirmed late: the hook showed it, and it is not queued as well",
              "confirmed late" in (shown["text"] or "") and read("confirmed late") and queued("confirmed late") == 0,
              (shown, queued()))
        # A PostToolUse took the mail but could not print it, so never confirms: queued once that is overdue (90 s).
        u = Thread("unconfirmed")
        u.start_turn()
        u.hook("post-tool-use")
        u.send("never confirmed")
        r, w = os.pipe()
        os.close(r)
        try:
            payload = {"session_id": f"thread-{u.name}", "cwd": u.cwd, "transcript_path": u.rollout}
            subprocess.run([BIN, "hook", "codex", "post-tool-use"], env=env, cwd=u.cwd, input=json.dumps(payload),
                           text=True, stdout=w, stderr=subprocess.DEVNULL, timeout=30)
        finally:
            os.close(w)
        u.record("turn_aborted", turn_id="t", reason="interrupted")  # no Stop
        time.sleep(6)
        check("never confirmed: not queued at the interrupt, while its confirmation may come",
              queued("never confirmed") == 0, queued())
        check("never confirmed: queued once that is overdue", queued_within("never confirmed", 100), queued())
        time.sleep(3)
        check("never confirmed: …once", read("never confirmed") and queued("never confirmed") == 1, queued())

    def no_rollout():
        # A thread whose hooks name no rollout (Codex has none for a remote thread store): as before, codex queue.
        t = Thread("norollout", transcript=False)
        t.hook("user-prompt-submit")
        t.hook("post-tool-use")
        t.send("no rollout message")
        check("no rollout known: woken through codex queue, as before", queued_within("no rollout message", 5),
              queued())

    def failed():
        t = Thread("failed")
        t.start_turn()
        t.hook("post-tool-use")
        t.record("task_complete", turn_id="t", last_agent_message=None,
                 error={"message": "stream disconnected"})  # a failed turn: no Stop
        t.send("after failure")
        check("failed turn: mail wakes through codex queue right away", queued_within("after failure", 10), queued())

    def continued():
        # Stop hands over mail (decision: block), so the turn goes on; mail for it still waits for the hook.
        t = Thread("continued")
        t.start_turn()
        t.hook("post-tool-use")
        t.send("first message")
        out = t.hook("stop")
        check("continued: Stop shows the first message as a block", "first message" in out and '"block"' in out, out)
        t.send("second message")
        time.sleep(3)
        check("continued: the second message is not queued while the turn goes on", queued("second message") == 0,
              queued())
        ok, detail = shown_once(t, "second message")
        check("continued: the next PostToolUse shows the second message, once", ok, detail)

    def long_call():
        # One tool call runs past two minutes (a long build); mail arrives during it.
        t = Thread("longcall")
        t.start_turn()
        t.hook("post-tool-use")
        time.sleep(LONG)
        t.send("during the long call")
        time.sleep(3)
        check("long tool call: mail during it is not queued behind the turn", queued("during the long call") == 0,
              queued())
        ok, detail = shown_once(t, "during the long call")
        check("long tool call: the PostToolUse after it shows the mail, once", ok, detail)

    def long_gap():
        # Mail arrives just after a tool call; the next one comes more than two minutes later (long thinking).
        t = Thread("longgap")
        t.start_turn()
        t.hook("post-tool-use")
        t.send("early in the gap")
        time.sleep(LONG)
        check("long gap: mail is still waiting for the hook, not queued", queued("early in the gap") == 0, queued())
        ok, detail = shown_once(t, "early in the gap")
        check("long gap: the next PostToolUse shows it, once", ok, detail)

    def long_turn():
        # A turn with many short tool calls, longer than any bound: every message arrives at the next tool call.
        t = Thread("longturn")
        t.start_turn()
        started, sent, missed = time.monotonic(), 0, []
        while time.monotonic() - started < LONG:
            body = f"long turn message {sent}"
            t.send(body)
            sent += 1
            time.sleep(10)
            out = t.hook("post-tool-use")
            if body not in out:
                missed.append((body, out))
            time.sleep(10)
            t.hook("post-tool-use")
        check(f"long turn: all {sent} messages arrived at the next PostToolUse", not missed, missed)
        check("long turn: none queued", queued("long turn message") == 0, queued())

    try:
        run("init", "codex-test", "--as", "alex", "--no-setup")

        # FYI mail never wakes: it waits for check_inbox (or a pause).
        fyi = Thread("fyi")
        fyi.start_turn()
        cx = subprocess.Popen([BIN, "mcp"], env=env, cwd=fyi.cwd, stdin=subprocess.PIPE, stdout=subprocess.PIPE,
                              stderr=subprocess.DEVNULL, text=True)
        try:
            def rpc(request):
                cx.stdin.write(json.dumps(request) + "\n")
                cx.stdin.flush()
                if "id" not in request:
                    return None
                responses = queue.Queue()

                def receive():
                    while line := cx.stdout.readline():
                        response = json.loads(line)
                        if response.get("id") == request["id"]:
                            responses.put(response)
                            return

                threading.Thread(target=receive, daemon=True).start()
                return responses.get(timeout=10)

            rpc({"jsonrpc": "2.0", "id": 1, "method": "initialize", "params": {
                "protocolVersion": "2025-06-18", "capabilities": {},
                "clientInfo": {"name": "codex-mcp-client", "version": "1"}}})
            rpc({"jsonrpc": "2.0", "method": "notifications/initialized"})
            out = fyi.send("polled FYI", "--fyi")
            check("fyi: queued as FYI", "queued as FYI" in out, out)
            check("fyi: not shown at PostToolUse", fyi.hook("post-tool-use") == "")
            response = rpc({"jsonrpc": "2.0", "id": 2, "method": "tools/call", "params": {
                "name": "check_inbox", "arguments": {}, "_meta": {"sessionId": f"thread-{fyi.name}"}}})
            check("fyi: check_inbox returns it", "polled FYI" in json.dumps(response), response)
            check("fyi: never queued", queued("polled FYI") == 0, queued())
        finally:
            cx.kill()
            cx.wait()

        scenarios = [busy, idle, ending, interrupted, interrupted_pending, unconfirmed, failed, continued, no_rollout,
                     long_call, long_gap, long_turn]
        print(f"Running {len(scenarios)} scenarios at once; the long ones wait {LONG} seconds", flush=True)
        threads = [threading.Thread(target=s) for s in scenarios]
        for th in threads:
            th.start()
        for th in threads:
            th.join()

        # A daemon restart while mail waits for a running turn: the next PostToolUse still shows it, once.
        r = Thread("restart")
        r.start_turn()
        r.hook("post-tool-use")
        r.send("across a restart")
        run("stop")
        ok, detail = shown_once(r, "across a restart")
        check("restart: mail waiting for a running turn reaches the next PostToolUse", ok, detail)
        check("restart: …and is not queued", queued("across a restart") == 0, queued())
    finally:
        run("stop")

    failed_count = results.count(False)
    print(f"\n{len(results) - failed_count}/{len(results)} passed")
    sys.exit(1 if failed_count else 0)
