"""Durable handoffs over iroh, using disposable homes and test sign-in proofs.

    cargo build && python3 tests/e2e/anchor.py --old-bin /path/to/unmodified/reason-network

Needs Unix file locks and internet for iroh discovery/relays. Never signs in or touches real agent sessions.
The optional old binary exercises mixed versions; all other cases always run.
"""

import argparse
import fcntl
import json
import os
from pathlib import Path
import re
import socket
import sqlite3
import subprocess
import tempfile
import time

from proofs import TRUST, proof, serve_relay_lists

ROOT = Path(__file__).resolve().parents[2]
WAIT_SECONDS = 60
POLL_SECONDS = 0.2
WEEK_MS = 7 * 86_400_000


def wait_for(read, description):
    deadline = time.monotonic() + WAIT_SECONDS
    while time.monotonic() < deadline:
        result = read()
        if result:
            print(f"PASS: {description}", flush=True)
            return result
        time.sleep(POLL_SECONDS)
    raise AssertionError(f"Timed out: {description}")


def daemon_stopped(home):
    try:
        lock = (home / "daemon.lock").open("rb")
    except FileNotFoundError:
        return True
    with lock:
        try:
            fcntl.flock(lock, fcntl.LOCK_SH | fcntl.LOCK_NB)
        except BlockingIOError:
            return False
        fcntl.flock(lock, fcntl.LOCK_UN)
    return True


class Machine:
    def __init__(self, network, name):
        self.name = name
        self.home = network.root / name
        self.home.mkdir()
        self.binary = network.binary
        self.env = dict(network.env, REASON_NETWORK_HOME=str(self.home), REASON_NETWORK_MACHINE=name)
        self.endpoint = self.cli("endpoint").strip()

    def cli(self, *args):
        result = subprocess.run([str(self.binary), *args], env=self.env, cwd=self.home,
                                text=True, capture_output=True, timeout=90)
        output = result.stdout + result.stderr
        if result.returncode:
            raise AssertionError(f"{self.name}: {args!r}: {output}")
        return output.strip()

    def rows(self, query, parameters=()):
        with sqlite3.connect(self.home / "network.db", timeout=10) as connection:
            return connection.execute(query, parameters).fetchall()

    def stop(self):
        self.cli("stop")
        wait_for(lambda: daemon_stopped(self.home), f"{self.name} daemon releases its lifetime lock")

    def pending(self):
        return self.rows("SELECT id, source, target, msg FROM anchor_mail WHERE msg IS NOT NULL ORDER BY rowid")

    def inbox(self):
        return self.rows("SELECT id, body, sender FROM messages WHERE recipient = 'alex/inbox' ORDER BY rowid")

    def send(self, body, *args):
        output = self.cli("send", "alex/inbox@laptop", body, *args)
        return re.search(r"message id ([0-9A-Z]{26})", output).group(1)

    def restore_unacked(self, target, held):
        # Rewind the source's whole acceptance transaction, not just its payload deletion.
        with sqlite3.connect(self.home / "network.db", timeout=10) as connection:
            connection.execute("DELETE FROM anchor_sends WHERE id = ?", (held[0],))
            connection.execute("INSERT INTO outbox(id,target,msg,created_at) VALUES (?,?,?,?)",
                               (held[0], target, held[3], json.loads(held[3])["sent_at"]))

    def receipt(self, message_id):
        rows = self.rows("SELECT stage FROM sent_receipts WHERE id = ?", (message_id,))
        return rows[0][0] if rows else None


class Network:
    def __init__(self, binary):
        self.root = Path(tempfile.mkdtemp(prefix="reason-network-anchor-"))
        self.binary = binary
        fake_bin = self.root / "bin"
        fake_bin.mkdir()
        for name in ("open", "xdg-open", "osascript", "notify-send", "codex", "reason"):
            executable = fake_bin / name
            executable.write_text("#!/bin/sh\nexit 1\n")
            executable.chmod(0o755)
        self.env = {key: value for key, value in os.environ.items()
                    if not key.startswith(("CLAUDE", "REASON_NETWORK", "TINCAN"))}
        self.env.update(HOME=str(self.root / "home"), CLAUDE_CONFIG_DIR=str(self.root / "claude"),
                        PATH=str(fake_bin) + os.pathsep + os.environ["PATH"],
                        REASON_NETWORK_NO_MDNS="1", REASON_NETWORK_NO_SERVICE="1",
                        REASON_NETWORK_NO_UPDATE="1", REASON_NETWORK_NO_TRAY="1",
                        REASON_NETWORK_TRUST_KEYS=TRUST,
                        REASON_NETWORK_RELAYS_URL=serve_relay_lists({"all": ["sfo1", "iad1", "fra1", "sin1"]}) + "/all.json")
        self.machines = []

    def machine(self, name):
        machine = Machine(self, name)
        self.machines.append(machine)
        return machine

    def join(self, machine, anchor):
        invite = re.search(r"One-time code for another machine of yours: (\S+)", anchor.cli("invite", "--me")).group(1)
        machine.cli("join", invite, "--as", "alex", "--no-setup")

    def close(self):
        for machine in self.machines:
            try:
                machine.stop()
            except (AssertionError, subprocess.TimeoutExpired) as error:
                print(f"Cleanup failed for {machine.name}: {error}", flush=True)
        print(f"Disposable evidence: {self.root}", flush=True)


class HostedSession:
    """The daemon-facing side of a hosted call; the external connector is outside this repository."""

    def __init__(self, machine):
        self.socket = socket.socket(socket.AF_UNIX)
        self.socket.settimeout(20)
        self.socket.connect(str(machine.home / "network.sock"))
        self.stream = self.socket.makefile("rwb")
        self.request_id = 0
        self.key = "hosted:same-cloud-session"
        registration = self.request({"op": "register", "session": {
            "key": self.key, "agent": "hosted", "agent_session_id": "same-cloud-session",
            "cwd": str(machine.home), "tier": "pull", "name": "cloud"}})
        self.address = registration["address"]

    def request(self, body):
        self.request_id += 1
        self.stream.write((json.dumps(dict(body, id=self.request_id)) + "\n").encode())
        self.stream.flush()
        while True:
            response = json.loads(self.stream.readline())
            if response.get("id") == self.request_id:
                assert response["ok"], response
                return response["result"]

    def inbox(self):
        return self.request({"op": "inbox", "key": self.key, "peek": True})["messages"]

    def close(self):
        self.stream.close()
        self.socket.close()


def hosted_connection_switch(machines):
    sender, anchor, laptop = machines
    laptop.cli("team")
    session = HostedSession(laptop)
    try:
        wait_for(lambda: any(row["address"] == session.address for row in json.loads(sender.cli("who", "--json"))["sessions"]), "hosted session appears through the laptop")
        sender.cli("send", session.address + "@laptop", "hosted before switch")
        wait_for(session.inbox, "hosted mail is saved on the laptop immediately before the switch")
    finally:
        session.close()
    laptop.stop()
    replacement = HostedSession(anchor)
    try:
        assert replacement.inbox() == []
        assert laptop.rows("SELECT id FROM messages WHERE body = 'hosted before switch'")
        assert not anchor.pending()
        print("PASS: switching hosted calls to VPS does not pretend to migrate an ordinary inbox", flush=True)
    finally:
        replacement.close()
    laptop.cli("team")
    restored = HostedSession(laptop)
    try:
        assert any(message["body"] == "hosted before switch" for message in restored.inbox())
        print("PASS: hosted mail remains retrievable when its original laptop returns", flush=True)
    finally:
        restored.close()


def learn_laptop(network, anchor):
    laptop = network.machine("laptop")
    network.join(laptop, anchor)
    laptop.cli("listen", "--as", "inbox", "--wait", "0")
    wait_for(lambda: any("inbox" in json.loads(row[0])["names"] for row in
                         anchor.rows("SELECT value FROM kv WHERE key = ?", (f"anchor-route:{laptop.endpoint}",))),
             "anchor learns the laptop's permanent inbox")
    return laptop


def explicit_anchor_trust(network, sender, anchor):
    proof_only = network.machine("proof-only")
    link = re.search(r"https://\S+#rn1-\S+", anchor.cli("link")).group()
    proof_only.cli("join", link, "--proof", proof("u_alex", "alex", proof_only.endpoint), "--no-setup")
    state = json.loads(proof_only.rows("SELECT value FROM kv WHERE key='team'")[0][0])
    assert proof_only.endpoint not in state.get("anchors", {})
    print("PASS: a public team-link sign-in does not grant anchor routing authority", flush=True)
    proof_only.cli("anchor", "vps", "--trust")
    wait_for(lambda: any(row["address"] == "alex/inbox" for row in json.loads(proof_only.cli("who", "--json"))["sessions"]),
             "an explicit signed trust choice enables saved mailbox discovery")
    proof_only.stop()
    sender.stop()
    config = sender.home / "config.toml"
    config.write_text("anchor = true\n" + config.read_text())
    sender.cli("team")
    wait_for(lambda: any(row["address"] == "alex/inbox" for row in json.loads(sender.cli("who", "--json"))["sessions"]),
             "the restarted sender rediscovers the selected remote mailbox")
    wait_for(lambda: "multiple anchor claims" in sender.cli("team"), "extra local anchor claim is visible while signed VPS remains selected")


def handoff_and_restart(sender, anchor, laptop):
    attachment = sender.home / "task.md"
    attachment.write_text("durable attachment\n")
    message_id = sender.send("offline handoff", "--file", str(attachment))
    held = wait_for(anchor.pending, "anchor durably accepts the offline handoff")
    assert json.loads(held[0][3])["files"][0]["content"] == "durable attachment\n"
    wait_for(lambda: not sender.rows("SELECT id FROM outbox WHERE id = ?", (message_id,)),
             "sender releases its copy only after anchor acceptance")
    anchor.stop()
    sender.stop()
    anchor.cli("team")
    assert anchor.pending()[0][0] == message_id
    laptop.cli("team")
    wait_for(lambda: len(laptop.inbox()) == 1, "handoff arrives after anchor restart with sender off")
    wait_for(lambda: not anchor.pending(), "anchor releases payload after recipient storage")
    saved = next((laptop.home / "inbox").rglob("task.md"))
    assert saved.read_text() == "durable attachment\n"
    assert laptop.inbox()[0][2] == "alex/terminal@sender"
    return held[0]


def lost_ack_and_direct_retry(machines, held):
    sender, anchor, laptop = machines
    anchor.stop()
    # Restore only the last unacknowledged forward, as a crash before consuming the laptop's ACK would leave it.
    anchor.rows("UPDATE anchor_mail SET msg = ? WHERE id = ?", (held[3], held[0]))
    anchor.cli("team")
    wait_for(lambda: not anchor.pending(), "lost final ACK is safely retried")
    assert len(laptop.inbox()) == 1
    sender.restore_unacked(laptop.endpoint, held)
    sender.cli("team")
    wait_for(lambda: not sender.rows("SELECT id FROM outbox"), "direct retry deduplicates against the forwarded copy")
    assert len(laptop.inbox()) == 1
    print("PASS: direct and forwarded retries preserve one inbox message and file", flush=True)
    laptop.stop()
    sender.stop()
    sender.restore_unacked(laptop.endpoint, held)
    sender.cli("team")
    wait_for(lambda: not sender.rows("SELECT id FROM outbox"), "lost acceptance ACK uses the completed anchor receipt")
    assert not anchor.pending()


def expiry_and_removal(sender, anchor, laptop):
    laptop.stop()
    wait_for(lambda: any(row[0] == f"presence:{laptop.endpoint}" for row in anchor.rows("SELECT key FROM kv")),
             "laptop presence remains known while offline")
    message_id = sender.send("expires from original send")
    wait_for(lambda: any(row[0] == message_id for row in anchor.pending()), "second handoff is held")
    anchor.stop()
    encoded = json.loads(next(row[3] for row in anchor.pending() if row[0] == message_id))
    encoded["sent_at"] = int(time.time() * 1000) - WEEK_MS
    anchor.rows("UPDATE anchor_mail SET msg = ?, expires_at = ? WHERE id = ?",
                (json.dumps(encoded), encoded["sent_at"] + WEEK_MS, message_id))
    anchor.cli("team")
    wait_for(lambda: not anchor.pending(), "expiry uses the original send time across restart")
    wait_for(lambda: sender.receipt(message_id) == "expired", "sender receipt learns expiry after anchor restart")
    assert anchor.rows("SELECT body FROM messages WHERE reply_to = ?", (message_id,))
    notice = anchor.cli("listen", "--as", "terminal", "--wait", "0")
    assert message_id in notice and "seven days" in notice
    print("PASS: expiry leaves an owner-visible notice", flush=True)
    message_id = sender.send("revoked destination")
    wait_for(anchor.pending, "third handoff is held before revocation")
    sender.cli("remove", "alex/laptop")
    wait_for(lambda: not anchor.pending(), "revocation received from another machine drops pending mail")
    wait_for(lambda: sender.receipt(message_id) == "rejected", "sender receipt learns the revoked handoff outcome")
    assert not anchor.rows("SELECT key FROM kv WHERE key = ?", (f"anchor-route:{laptop.endpoint}",))
    assert anchor.rows("SELECT body FROM messages WHERE reply_to = ?", (message_id,))


def keyed_remote_recovery(sender, anchor, laptop):
    laptop.stop()
    session = HostedSession(sender)
    request = {"op": "send", "from": session.key, "to": "alex/inbox@laptop",
               "body": "keyed remote recovery", "idempotency_key": "remote-recovery"}
    try:
        sender.rows("CREATE TRIGGER fail_keyed_receipt BEFORE UPDATE ON send_keys BEGIN SELECT RAISE(FAIL,'test receipt failure'); END")
        try:
            session.request(request)
            raise AssertionError("the injected receipt write failure must reach the caller")
        except AssertionError as error:
            assert "test receipt failure" in str(error), error
        held = wait_for(lambda: next((row for row in anchor.pending() if json.loads(row[3])["body"] == request["body"]), None),
                        "background custody survives a failed keyed API receipt write")
        wait_for(lambda: not sender.rows("SELECT id FROM outbox WHERE id=?", (held[0],)),
                 "failed keyed call transfers custody before its retry")
        sender.rows("DROP TRIGGER fail_keyed_receipt")
        result = session.request(request)
        assert result["ids"] == [held[0]], result
        assert not sender.rows("SELECT id FROM outbox WHERE id=?", (held[0],))
        assert next(row[3] for row in anchor.pending() if row[0] == held[0]) == held[3]
        assert sender.receipt(held[0]) == "anchored"
        print("PASS: keyed retry recovers original custody without rebuilding the envelope", flush=True)
        laptop.cli("team")
        wait_for(lambda: sender.receipt(held[0]) == "accepted", "recovered keyed handoff reports final recipient storage")
    finally:
        session.close()


def direct_delivery_first(machines):
    sender, anchor, laptop = machines
    wait_for(lambda: any(row["machine"] == "laptop" and row["online"] for row in json.loads(sender.cli("who", "--json"))["machines"]), "laptop is directly reachable")
    message_id = sender.send("direct delivery first")
    wait_for(lambda: laptop.rows("SELECT id FROM messages WHERE id = ?", (message_id,)), "online mail goes directly to the laptop")
    assert not anchor.rows("SELECT id FROM anchor_mail WHERE id = ?", (message_id,))
    print("PASS: direct delivery creates no anchor custody receipt", flush=True)


def recipient_consent(network, sender, anchor):
    target = network.machine("unconsented")
    link = re.search(r"https://\S+#rn1-\S+", anchor.cli("link")).group()
    target.cli("join", link, "--proof", proof("u_alex", "alex", target.endpoint), "--no-setup")
    target.cli("listen", "--as", "consent-check", "--wait", "0")
    wait_for(lambda: any(row["address"] == "alex/consent-check" for row in json.loads(sender.cli("who", "--json"))["sessions"]),
             "the unconsented laptop announces its direct mailbox")
    assert not anchor.rows("SELECT key FROM kv WHERE key=?", (f"anchor-route:{target.endpoint}",))
    target.stop()
    output = sender.cli("send", "alex/consent-check@unconsented", "recipient has not selected an anchor")
    message_id = re.search(r"message id ([0-9A-Z]{26})", output).group(1)
    assert sender.rows("SELECT id FROM outbox WHERE id=?", (message_id,))
    assert not anchor.rows("SELECT id FROM anchor_mail WHERE id=?", (message_id,))
    print("PASS: missing recipient consent keeps custody on the sender", flush=True)
    target.cli("team")
    wait_for(lambda: target.rows("SELECT id FROM messages WHERE id=?", (message_id,)),
             "unconsented recipient still receives ordinary direct delivery")
    wait_for(lambda: not sender.rows("SELECT id FROM outbox WHERE id=?", (message_id,)),
             "direct recipient storage releases the original sender copy")
    target.stop()


def full_anchor_keeps_the_sender_copy(machines):
    sender, anchor, laptop = machines
    laptop.stop()
    anchor.stop()
    expires = int(time.time() * 1000) + WEEK_MS
    anchor.rows("""WITH RECURSIVE numbers(n) AS (SELECT 1 UNION ALL SELECT n+1 FROM numbers WHERE n<10000)
                   INSERT INTO anchor_mail(id,source,target,digest,msg,expires_at)
                   SELECT 'capacity-'||n,'test','test','test',NULL,? FROM numbers""", (expires,))
    anchor.cli("team")
    wait_for(lambda: any(row["online"] and row["machine"] == "vps" for row in json.loads(sender.cli("who", "--json"))["machines"]), "full anchor is connected")
    output = sender.cli("send", "alex/inbox@laptop", "full anchor must retain the sender copy")
    assert "full" in output.lower(), output
    assert sender.rows("SELECT id FROM outbox WHERE json_extract(msg,'$.body') = 'full anchor must retain the sender copy'")
    assert not anchor.pending()
    print("PASS: a full anchor explicitly refuses acceptance while sender retains custody", flush=True)
    anchor.stop()
    anchor.rows("DELETE FROM anchor_mail WHERE id LIKE 'capacity-%'")
    anchor.cli("team")
    wait_for(anchor.pending, "handoff retries after capacity is available")
    wait_for(lambda: not sender.rows("SELECT id FROM outbox"), "sender releases the retained copy after successful retry")
    laptop.cli("team")
    wait_for(lambda: not anchor.pending(), "capacity recovery delivers and completes the handoff")


def mixed_version(network, anchor, old_binary):
    old = network.machine("old")
    old.binary = old_binary
    network.join(old, anchor)
    old.cli("listen", "--as", "legacy", "--wait", "0")
    wait_for(lambda: any(row["address"] == "alex/legacy" for row in json.loads(anchor.cli("who", "--json"))["sessions"]), "current release remains connected")
    anchor.cli("send", "alex/legacy@old", "ordinary mixed-version message")
    wait_for(lambda: old.rows("SELECT id FROM messages WHERE body = 'ordinary mixed-version message'"),
             "old release receives ordinary messages with a new anchor present")
    old.cli("listen", "--as", "terminal", "--wait", "0")

    def current_terminal():
        name = anchor.rows("SELECT name FROM sessions WHERE key='cli:terminal'")[0][0]
        sessions = json.loads(old.cli("who", "--json"))["sessions"]
        return name if any(row["address"] == f"alex/{name}" and row["machine"] == "vps" for row in sessions) else None

    terminal = wait_for(current_terminal, "old release discovers the VPS terminal's current name")
    old.cli("send", f"alex/{terminal}@vps", "ordinary reply from old release")
    wait_for(lambda: anchor.rows("SELECT id FROM messages WHERE body = 'ordinary reply from old release'"),
             "old release can reply to a new engine")
    assert not anchor.rows("SELECT value FROM kv WHERE key = ?", (f"anchor-route:{old.endpoint}",))


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    target_dir = Path(os.environ.get("CARGO_TARGET_DIR", ROOT / "target"))
    parser.add_argument("--bin", type=Path, default=target_dir / "debug" / "reason-network")
    parser.add_argument("--old-bin", type=Path)
    args = parser.parse_args()
    network = Network(args.bin.resolve())
    try:
        anchor = network.machine("vps")
        (anchor.home / "config.toml").write_text("anchor = true\n")
        anchor.cli("init", "anchor-test", "--proof", proof("u_alex", "alex", anchor.endpoint), "--no-setup")
        anchor.cli("anchor", "vps")
        laptop = learn_laptop(network, anchor)
        laptop.stop()
        sender = network.machine("sender")
        network.join(sender, anchor)
        wait_for(lambda: any(row["address"] == "alex/inbox" and row["tier"] != "none" for row in json.loads(sender.cli("who", "--json"))["sessions"]), "fresh workspace discovers offline laptop via anchor")
        explicit_anchor_trust(network, sender, anchor)
        unknown = subprocess.run([str(sender.binary), "send", "alex/unknown@laptop", "unknown"],
                                 env=sender.env, cwd=sender.home, text=True, capture_output=True, timeout=90)
        assert unknown.returncode != 0 and "unknown" in unknown.stderr.lower()
        print("PASS: unknown mailbox gives a clear error", flush=True)
        recipient_consent(network, sender, anchor)
        held = handoff_and_restart(sender, anchor, laptop)
        lost_ack_and_direct_retry((sender, anchor, laptop), held)
        hosted_connection_switch((sender, anchor, laptop))
        keyed_remote_recovery(sender, anchor, laptop)
        direct_delivery_first((sender, anchor, laptop))
        full_anchor_keeps_the_sender_copy((sender, anchor, laptop))
        expiry_and_removal(sender, anchor, laptop)
        if args.old_bin:
            mixed_version(network, anchor, args.old_bin.resolve())
        else:
            print("NOT RUN: mixed release (supply --old-bin)", flush=True)
    finally:
        network.close()


if __name__ == "__main__":
    main()
