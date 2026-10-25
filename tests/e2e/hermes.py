"""Exercise the native Hermes adapter against a real, isolated Reason daemon.

Build the engine, then run with Hermes on PYTHONPATH (no model or account needed):
    PYTHONPATH=/path/to/hermes-agent python tests/e2e/hermes.py
"""
import importlib.util
import json
import os
from pathlib import Path
import subprocess
import tempfile
import threading
import time
import unittest
from types import SimpleNamespace
import shutil
import sys

ROOT = Path(__file__).resolve().parents[2]
BIN = Path(os.environ.get("REASON_TEST_BIN", ROOT / "target/debug/reason-network"))
spec = importlib.util.spec_from_file_location("reason_hermes", ROOT / "integrations/hermes/__init__.py")
plugin = importlib.util.module_from_spec(spec)
spec.loader.exec_module(plugin)


class Context:
    profile_name = "test"

    def __init__(self, directory, wake=True):
        self.directory = str(directory)
        self.route = "route-first"
        self.wake = wake
        self.executable = str(BIN)
        self.accept = True
        self.received = []
        self.delivered = threading.Event()
        data = {}
        self.state = SimpleNamespace(get=data.get, set=data.__setitem__, data_dir=directory / "plugin-data")

    def is_interrupted(self):
        from tools.interrupt import is_interrupted
        return is_interrupted()

    def session_supports_async_delivery(self):
        return True

    def get_config(self, key, default=None):
        return {"executable": self.executable, "wake": self.wake}.get(key, default)

    def session_cwd(self):
        return self.directory

    def session_surface(self):
        return "desktop"

    def session_key(self):
        return self.route

    def inject_message(self, text, *, session_key=None):
        self.received.append((session_key, text))
        self.delivered.set()
        return self.accept


STAND_IN_ENGINE = '''
import json, sys, threading, time
out = threading.Lock()
def answer(call):
    result = {}
    if call.get("op") == "register":
        key = call["session"]["key"]
        if key.endswith(":slow"):
            open(MARKER, "w").close()
            time.sleep(8)
        result = {"address": key}
    elif call.get("op") == "unread":
        result = {"count": 0}
    with out:
        print(json.dumps({"id": call["id"], "ok": True, "result": result}), flush=True)
# Calls finish out of order, as the real bridge's do.
for line in sys.stdin:
    threading.Thread(target=answer, args=(json.loads(line),)).start()
'''


class ObservedTurnLock:
    """Observe lock contention without guessing how long a worker takes."""

    def __init__(self, lock, watched_name):
        self.lock = lock
        self.watched_name = watched_name
        self.observed = threading.Event()
        self.acquired_immediately = None

    def __enter__(self):
        if threading.current_thread().name == self.watched_name and not self.observed.is_set():
            acquired = self.lock.acquire(blocking=False)
            self.acquired_immediately = acquired
            self.observed.set()
            if not acquired:
                self.lock.acquire()
        else:
            self.lock.acquire()
        return self

    def __exit__(self, *_):
        self.lock.release()


class NativeHermes(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        # Windows's system temp lives under protected AppData. Use a real project
        # workspace for the positive attachment case, keeping credential guards intact.
        scratch = ROOT / "target"
        scratch.mkdir(exist_ok=True)
        cls.temporary = tempfile.TemporaryDirectory(prefix="reason-hermes-", dir=scratch)
        cls.root = Path(cls.temporary.name)
        cls.previous = dict(os.environ)
        os.environ.update(
            REASON_NETWORK_HOME=str(cls.root / "network"), REASON_NETWORK_NO_MDNS="1",
            REASON_NETWORK_NO_SERVICE="1", REASON_NETWORK_NO_TRAY="1", REASON_NETWORK_NO_UPDATE="1",
            REASON_NETWORK_MACHINE="isolated-hermes-test", CLAUDE_CONFIG_DIR=str(cls.root / "claude"),
        )
        cls.cli("init", "hermes-adapter-test", "--as", "tester", "--no-setup")

    @classmethod
    def cli(cls, *args):
        result = subprocess.run([str(BIN), *args], text=True, capture_output=True, timeout=30)
        if result.returncode:
            raise RuntimeError(result.stderr or result.stdout)
        return result.stdout

    @classmethod
    def tearDownClass(cls):
        cls.cli("stop")
        os.environ.clear()
        os.environ.update(cls.previous)
        cls.temporary.cleanup()

    def setUp(self):
        self.directory = self.root / self._testMethodName
        self.directory.mkdir()
        self.context = Context(self.directory)
        self.context.profile_name = self._testMethodName
        self.network = plugin.Network(self.context)

    def tearDown(self):
        self.network.close()

    def start(self, ident="first", **kwargs):
        self.context.route = f"route-{ident}"
        result = self.network.before_turn(session_id=ident, platform="desktop", **kwargs)
        self.assertIsNotNone(result)
        return self.network.session(ident)

    def send(self, sender, target, body="test message", **kwargs):
        return sender["bridge"].request("send", **{"from": sender["key"]},
            to=target["registration"]["address"], body=body, **kwargs)

    def assert_acknowledged(self, session, network=None):
        network = network or self.network
        deadline = time.monotonic() + 5
        while time.monotonic() < deadline:
            network.register_session(session)
            if session["registration"]["unread"] == 0:
                return
            time.sleep(0.01)
        self.fail("Host accepted delivery but the adapter did not acknowledge it")

    def listed(self, session, viewer):
        return session["registration"]["name"] + " " in viewer["bridge"].request("who", me=viewer["key"])

    def test_two_conversations_use_captured_routes_and_end_independently(self):
        first = self.start()
        self.context.route = "route-second"
        second = self.start("second")
        self.assertNotEqual(first["registration"]["address"], second["registration"]["address"])
        self.send(first, second)
        self.assertTrue(self.context.delivered.wait(5))
        self.assertEqual(self.context.received[0][0], "route-second")
        self.assertIn("<reason-message", self.context.received[0][1])
        self.assert_acknowledged(second)
        self.network.finalize(session_id="first")
        self.assertTrue(second["bridge"].alive)
        self.assertTrue(self.listed(second, second))
        self.assertFalse(self.listed(first, second))

    def test_a_gateways_conversations_share_one_adapter(self):
        sessions = [self.start(f"chat-{n}") for n in range(4)]
        self.assertEqual(len({id(s["bridge"]) for s in sessions}), 1)
        for session in sessions:
            self.assertTrue(self.listed(session, sessions[0]))
        # Every conversation registered on it stays: none ends another.
        self.send(sessions[3], sessions[0], "to the first chat")
        self.assertTrue(self.context.delivered.wait(5))
        self.assertEqual(self.context.received[0][0], "route-chat-0")
        self.assert_acknowledged(sessions[0])

    def test_mail_held_for_a_conversation_reaches_it_when_it_comes_back(self):
        sender, target = self.start("sender"), self.start("returning")
        address = target["registration"]["address"]
        self.network.finalize(session_id="returning")
        self.send(sender, target, "held while away")
        self.assertEqual(self.context.received, [])
        # The daemon pushes held mail as the conversation registers, before the plugin has it stored.
        back = self.start("returning")
        self.assertEqual(back["registration"]["address"], address)
        # At once: a dropped push only comes back with the daemon's retry, seconds later.
        self.assertTrue(self.context.delivered.wait(2), "mail pushed at registration was dropped")
        self.assertIn("held while away", self.context.received[0][1])
        self.assert_acknowledged(back)

    def test_an_ended_conversation_stays_gone_after_the_daemon_restarts(self):
        ended, kept = self.start("ended"), self.start("kept")
        self.network.finalize(session_id="ended")
        self.cli("stop")
        # The adapter reconnects and registers again only the conversations still open.
        deadline = time.monotonic() + 15
        while True:
            try:
                if self.listed(kept, kept):
                    break
            except RuntimeError:
                pass
            self.assertLess(time.monotonic(), deadline, "the open conversation never came back")
            time.sleep(0.1)
        self.assertFalse(self.listed(ended, kept))

    def test_a_restarted_adapter_registers_each_conversation_again(self):
        self.context.wake = False
        first, second = self.start(), self.start("second")
        old = first["bridge"]
        old.process.kill()
        old.process.wait(timeout=5)
        deadline = time.monotonic() + 5
        while old.alive:
            self.assertLess(time.monotonic(), deadline)
            time.sleep(0.01)
        # One conversation's turn starts the new adapter; the other's next tool call joins it.
        self.network.before_turn(session_id="first", platform="desktop")
        self.assertIsNot(first["bridge"], old)
        self.assertIn("No messages", self.network.tool("check_inbox", {}, "second"))
        self.assertIs(second["bridge"], first["bridge"])
        self.send(first, second, "after the restart")
        self.assertIn("after the restart", self.network.inbox(second))

    def test_failed_push_stays_unread(self):
        sender = self.start()
        target = self.start("denied")
        self.context.accept = False
        self.send(sender, target, "keep this unread")
        self.assertTrue(self.context.delivered.wait(5))
        # Waiting for a barrier request keeps the assertion independent of reader scheduling.
        target["bridge"].request("who", me=target["key"])
        inbox = self.network.inbox(target)
        self.assertIn("keep this unread", inbox)
        self.assertEqual(self.network.inbox(target), "No messages.")

    def test_pull_mode_reads_at_the_next_turn_without_waking(self):
        self.context.wake = False
        first, second = self.start(), self.start("pull")
        self.send(first, second, "next turn")
        result = self.network.before_turn(session_id="pull", platform="desktop")
        self.assertIn("unread", result["context"])
        self.assertIn("next turn", self.network.inbox(second))
        self.assertEqual(self.context.received, [])

    def test_disabling_wake_applies_before_another_turn_and_can_be_reenabled(self):
        sender, target = self.start(), self.start("live-setting")
        address = target["registration"]["address"]
        demoted = threading.Event()
        register = self.network.register_session
        def observe_registration(session):
            register(session)
            if session is target and not session["wake"]:
                demoted.set()
        self.network.register_session = observe_registration
        self.context.wake = False
        self.send(sender, target, "disabled while idle")
        self.assertTrue(demoted.wait(5))
        self.assertFalse(target["wake"])
        self.assertEqual(self.context.received, [])
        self.assertIn("disabled while idle", self.network.inbox(target))
        self.context.wake = True
        self.network.before_turn(session_id="live-setting", platform="desktop")
        self.assertEqual(target["registration"]["address"], address)
        self.send(sender, target, "enabled again")
        self.assertTrue(self.context.delivered.wait(5))
        self.assertIn("enabled again", self.context.received[0][1])
        self.assert_acknowledged(target)

    def test_tool_first_session_gains_wake_only_after_a_bound_interactive_turn(self):
        target = self.network.session("tool-first")
        self.assertFalse(target["wake"])
        self.network.before_turn(session_id="tool-first", platform="desktop")
        self.assertTrue(target["wake"])
        sender = self.start("sender")
        self.send(sender, target, "interactive route ready")
        self.assertTrue(self.context.delivered.wait(5))
        self.assertEqual(self.context.received[0][0], "route-first")
        self.assert_acknowledged(target)

    def test_transient_registration_failure_is_retried_on_the_next_bound_turn(self):
        target = self.network.session("retry-registration")
        request = target["bridge"].request
        attempts = []
        def fail_once(operation, **kwargs):
            if operation == "register":
                attempts.append(operation)
                if len(attempts) == 1:
                    raise RuntimeError("temporary test registration failure")
            return request(operation, **kwargs)
        target["bridge"].request = fail_once
        self.assertIsNone(self.network.before_turn(session_id="retry-registration", platform="desktop"))
        self.assertIsNotNone(self.network.before_turn(session_id="retry-registration", platform="desktop"))
        self.assertEqual(len(attempts), 2)
        sender = self.start("sender")
        self.send(sender, target, "registration recovered")
        self.assertTrue(self.context.delivered.wait(5))
        self.assertIn("registration recovered", self.context.received[0][1])
        self.assert_acknowledged(target)

    def test_rejected_push_cannot_demote_a_newer_bound_turn(self):
        sender, target = self.start("sender"), self.start("target")
        refused, release, refusal_done, turn_done, fresh_delivery = (threading.Event() for _ in range(5))
        result = []
        watched_name = "test-bound-turn"
        # The conversation's own lock: a bound turn waits for its stale refusal, other conversations don't.
        observed_lock = ObservedTurnLock(target["lock"], watched_name)
        target["lock"] = self.network._turns["target"] = observed_lock
        request = target["bridge"].request
        inject = self.context.inject_message
        receive = self.network.receive

        def controlled_request(op, **kwargs):
            answer = request(op, **kwargs)
            if op == "push_failed" and not refused.is_set():
                refused.set()
                if not release.wait(5):
                    raise TimeoutError("test did not release the refused push")
            return answer

        def observed_inject(text, **kwargs):
            accepted = inject(text, **kwargs)
            if accepted and "fresh message after recovery" in text:
                fresh_delivery.set()
            return accepted

        def observed_receive(session, event):
            try:
                return receive(session, event)
            finally:
                if "rejected before the next turn" in event["text"]:
                    refusal_done.set()

        def bound_turn():
            try:
                result.append(self.network.before_turn(session_id="target", platform="desktop"))
            finally:
                turn_done.set()

        target["bridge"].request = controlled_request
        self.context.inject_message = observed_inject
        self.network.receive = observed_receive
        self.context.accept = False
        worker = threading.Thread(target=bound_turn, name=watched_name, daemon=True)
        try:
            self.send(sender, target, "rejected before the next turn")
            self.assertTrue(refused.wait(5))
            self.context.accept = True
            self.context.route = "route-target"
            worker.start()
            self.assertTrue(observed_lock.observed.wait(5))
            # With the defect, the turn completes while the stale rejection is
            # paused; fixed code contends, and finishes after that rejection.
            if observed_lock.acquired_immediately:
                self.assertTrue(turn_done.wait(5))
            release.set()
            self.assertTrue(refusal_done.wait(5))
            self.assertTrue(turn_done.wait(5))
            worker.join(timeout=5)
            self.assertFalse(worker.is_alive())
            self.assertIsNotNone(result[0])
            self.assertTrue(target["wake"], "stale rejection overwrote the newer turn's push mode")
            self.send(sender, target, "fresh message after recovery")
            self.assertTrue(fresh_delivery.wait(5))
            self.assertEqual(target["route"], "route-target")
            self.assert_acknowledged(target)
        finally:
            release.set()
            if worker.ident is not None:
                worker.join(timeout=5)

    def test_bridge_refuses_admin_commands_other_identities_and_raw_files(self):
        first = self.start()
        # Another Hermes process, with its own adapter: its conversations aren't this adapter's to speak for.
        elsewhere = plugin.Network(self.context)
        self.addCleanup(elsewhere.close)
        self.context.route = "route-elsewhere"
        other = elsewhere.session("other", platform="desktop")
        bridge = first["bridge"]
        for op, arguments in (
            ("who", {"me": other["key"]}), ("leave", {}),
            ("send", {"from": first["key"], "to": other["registration"]["address"],
                      "body": "no", "files": [{"name": "secret", "content": "bypass"}]}),
        ):
            with self.subTest(op=op), self.assertRaises(RuntimeError):
                bridge.request(op, **arguments)

    def test_text_files_are_confined_and_received(self):
        self.context.wake = False
        first, second = self.start(), self.start("files")
        (self.directory / "notes.txt").write_text("useful text", encoding="utf-8")
        self.send(first, second, "see notes", paths=["notes.txt"])
        inbox = self.network.inbox(second)
        self.assertIn("notes.txt", inbox)
        # This must be outside the repository too: attachments may use the entire repo.
        with tempfile.TemporaryDirectory(prefix="reason-hermes-excluded-") as excluded:
            outside = Path(excluded) / "outside.txt"
            outside.write_text("must stay outside", encoding="utf-8")
            with self.assertRaises(RuntimeError):
                self.send(first, second, paths=[str(outside)])

    def test_subagents_never_borrow_the_parent_wake_route(self):
        result = self.network.before_turn(session_id="child", platform="subagent")
        self.assertIsNotNone(result)
        self.assertFalse(self.network.session("child")["wake"])

    def test_history_compaction_keeps_identity_and_refreshes_the_delivery_route(self):
        original = self.start("before-compression")
        self.network.compacted(session_id="after-compression", old_session_id="before-compression")
        self.context.route = "new-routing-key"
        self.network.before_turn(session_id="after-compression", platform="desktop")
        continuation = self.network.session("after-compression")
        self.assertIs(continuation, original)
        self.assertEqual(continuation["route"], "new-routing-key")
        self.assertNotIn("before-compression", self.network._sessions)
        witness = self.start("witness")
        self.network.finalize(session_id="after-compression")
        self.assertFalse(self.listed(original, witness))

    def assert_late_delivery_is_ignored(self, close):
        sender, target = self.start("sender"), self.start("closing")
        entered, release, finished = (threading.Event() for _ in range(3))
        receive = self.network.receive
        def paused_receive(session, event):
            entered.set()
            try:
                if not release.wait(10):
                    raise TimeoutError("test did not release the pending callback")
                return receive(session, event)
            finally:
                finished.set()
        self.network.receive = paused_receive
        try:
            self.send(sender, target, "message arriving while closing")
            self.assertTrue(entered.wait(5))
            close()
            self.assertNotIn("closing", self.network._sessions)
            self.assertNotIn(target["key"], self.network._keys)
            release.set()
            self.assertTrue(finished.wait(5))
            self.assertEqual(self.context.received, [], "an old callback injected into a closed conversation")
        finally:
            release.set()
            target["bridge"]._delivery.join(timeout=5)

    def test_finalization_ignores_a_delivery_callback_already_in_flight(self):
        self.assert_late_delivery_is_ignored(lambda: self.network.finalize(session_id="closing"))

    def test_unload_ignores_a_delivery_callback_already_in_flight(self):
        self.assert_late_delivery_is_ignored(self.network.close)

    def test_cancelled_inbox_releases_the_wait_without_consuming_future_mail(self):
        from tools.interrupt import clear_current_thread_interrupt, set_interrupt
        self.context.wake = False
        sender, target = self.start(), self.start("cancelled")
        entered = threading.Event()
        finished = threading.Event()
        result = []
        def wait():
            entered.set()
            try:
                result.append(self.network.tool("check_inbox", {"wait_secs": 30}, "cancelled"))
            finally:
                clear_current_thread_interrupt()
                finished.set()
        worker = threading.Thread(target=wait)
        worker.start()
        self.assertTrue(entered.wait(5))
        set_interrupt(True, worker.ident)
        self.assertTrue(finished.wait(5))
        worker.join(timeout=5)
        self.assertIn("cancelled", result[0])
        self.send(sender, target, "read after cancellation")
        self.assertIn("read after cancellation", self.network.inbox(target))

    def test_preturn_hints_never_acknowledge_mail(self):
        self.context.wake = False
        sender, target = self.start(), self.start("hint")
        self.send(sender, target, "must remain unread")
        self.network.before_turn(session_id="hint", platform="desktop")
        self.assertEqual(target["bridge"].request("unread", key=target["key"])["count"], 1)

    def test_malformed_arguments_and_foreign_acknowledgements_fail_immediately(self):
        self.context.wake = False
        sender, target = self.start(), self.start("recipient")
        bridge = sender["bridge"]
        with self.assertRaises(RuntimeError):
            bridge.request("inbox", key=sender["key"], wait_secs=True, timeout=3)
        self.send(sender, target, "do not consume", wake=False)
        offered = target["bridge"].request("inbox", key=target["key"], peek=True)
        ids = [m["id"] for m in offered["messages"]]
        for op in ("delivered", "push_failed"):
            with self.assertRaises(RuntimeError):
                bridge.request(op, key=sender["key"], ids=ids)
        self.assertIn("do not consume", self.network.inbox(target))

    @unittest.skipIf(os.name == "nt", "the stand-in engine is a script with a shebang")
    def test_a_slow_answer_for_one_conversation_does_not_stall_another(self):
        # A stand-in engine that takes 8 s to register one conversation, as a busy daemon might.
        marker = self.directory / "slow-register-received"
        engine = self.directory / "stand-in-engine"
        engine.write_text(f"#!{sys.executable}\n" + STAND_IN_ENGINE.replace("MARKER", repr(str(marker))),
                          encoding="utf-8")
        engine.chmod(0o755)
        self.context.executable, self.context.route = str(engine), ""
        slow_done = threading.Event()
        def slow_turn():
            self.network.before_turn(session_id="slow", platform="desktop")
            slow_done.set()
        threading.Thread(target=slow_turn, daemon=True).start()
        deadline = time.monotonic() + 5
        while not marker.exists():
            self.assertLess(time.monotonic(), deadline, "the slow registration never reached the engine")
            time.sleep(0.01)
        started = time.monotonic()
        self.assertIsNotNone(self.network.before_turn(session_id="fast", platform="desktop"))
        self.assertLess(time.monotonic() - started, 2, "an unrelated conversation waited on the slow one")
        self.assertFalse(slow_done.is_set())
        self.assertTrue(slow_done.wait(15))

    def test_an_oversized_frame_fails_only_that_call(self):
        first, other = self.start(), self.start("other")
        bridge = first["bridge"]
        # One line over the 1 MiB cap: the adapter drops it unbuffered and keeps serving every conversation.
        with bridge._lock:
            bridge.process.stdin.write('{"id":1,"op":"unread","key":"' + "x" * (2 * 1024 * 1024) + '"}\n')
            bridge.process.stdin.flush()
        self.assertEqual(bridge.request("unread", key=first["key"])["count"], 0)
        self.assertEqual(other["bridge"].request("unread", key=other["key"])["count"], 0)
        self.assertTrue(bridge.alive)
        # The plugin never sends one: such a call fails at once.
        started = time.monotonic()
        with self.assertRaises(ValueError):
            self.send(first, other, "x" * (1024 * 1024))
        self.assertLess(time.monotonic() - started, 1)

    def test_an_older_hermes_without_the_plugin_apis_loads_nothing(self):
        registered = []
        older = SimpleNamespace(  # What released Hermes gives a plugin today: no routing key or cancellation.
            register_hook=lambda *a, **k: registered.append(a), register_tool=lambda **k: registered.append(k),
            on_unload=registered.append, session_cwd=lambda: "", session_surface=lambda: "cli",
            inject_message=lambda *a, **k: False,
        )
        with self.assertLogs(plugin.log, "WARNING") as logs:
            plugin.register(older)
        self.assertEqual(registered, [])
        self.assertIn("session_key", logs.output[0])
        self.assertIn("is_interrupted", logs.output[0])

    def test_the_engine_is_found_where_the_installers_put_it(self):
        home, previous = self.directory / "home", dict(os.environ)
        name = "reason-network.exe" if os.name == "nt" else "reason-network"
        local, app = home / ".local" / "bin" / name, home / ".reason" / "bin" / name
        self.context.executable = ""
        os.environ.update(HOME=str(home), USERPROFILE=str(home), PATH="")
        try:
            app.parent.mkdir(parents=True)
            app.write_text("")
            self.assertEqual(self.network.executable(), str(app))
            local.parent.mkdir(parents=True)
            local.write_text("")
            self.assertEqual(self.network.executable(), str(local))
        finally:
            os.environ.clear()
            os.environ.update(previous)

    def test_real_hermes_loader_routes_through_its_permission_checked_queue(self):
        from agent.runtime_cwd import reset_session_cwd, set_session_cwd
        from gateway.session_context import clear_session_vars, set_session_vars
        from hermes_cli.plugins import PluginManager
        profile = self.directory / "profile"
        profile.mkdir()
        shutil.copytree(ROOT / "integrations/hermes", profile / "plugins/reason-network",
                        ignore=shutil.ignore_patterns("__pycache__"))
        (profile / "config.yaml").write_text(json.dumps({"plugins": {
            "enabled": ["reason-network"], "entries": {"reason-network": {
                "allow_gateway_injection": True, "settings": {"wake": True, "executable": str(BIN)},
            }},
        }}), encoding="utf-8")
        previous = os.environ.get("HERMES_HOME")
        os.environ["HERMES_HOME"] = str(profile)
        manager = PluginManager()
        accepted = []
        queued = threading.Event()
        def inject(**kwargs):
            accepted.append(kwargs)
            queued.set()
            return True
        manager.set_tui_message_injector(object(), inject)
        token = set_session_cwd(str(self.directory))
        try:
            manager.discover_and_load()
            self.assertIn("reason-network", manager._plugins)
            for session_id, route in (("host-first", "real-host-first"), ("host-second", "real-host-second")):
                tokens = set_session_vars(platform="desktop", session_key=route, session_id=session_id,
                                          cwd=str(self.directory))
                try:
                    self.assertTrue(manager.invoke_hook("pre_llm_call", session_id=session_id, platform="desktop"))
                finally:
                    clear_session_vars(tokens)
            # The callback is registered by the real loader, with a real PluginContext.
            network = next(callback.__self__ for callback in manager._hooks["pre_llm_call"]
                           if callback.__self__.__class__.__name__ == "Network")
            self.send(network.session("host-first"), network.session("host-second"), "host queue check")
            self.assertTrue(queued.wait(5))
            self.assertEqual(accepted[0]["session_key"], "real-host-second")
            self.assertIn("host queue check", accepted[0]["content"])
            self.assert_acknowledged(network.session("host-second"), network)
            manager.unload("reason-network")
            self.assertFalse(network._sessions)
        finally:
            manager.unload("reason-network")
            reset_session_cwd(token)
            if previous is None:
                os.environ.pop("HERMES_HOME", None)
            else:
                os.environ["HERMES_HOME"] = previous


if __name__ == "__main__":
    unittest.main(verbosity=2)
