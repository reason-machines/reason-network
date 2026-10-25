"""Two 'machines' (two REASON_NETWORK_HOMEs) on one box, talking over iroh, with simulated agent sessions.

    cargo build && env -u CLAUDE_CODE_MESSAGING_SOCKET python3 tests/e2e/two_machines.py

Needs internet (iroh relays/discovery). Run inside a Claude Code session, the simulated Claude session pushes its
test messages into that real session (they say "ignore this").
"""
import base64, json, os, re, sqlite3, subprocess, sys, tempfile, time
sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
from proofs import TRUST, keygen, proof, serve_relay_lists

ROOT = os.path.dirname(os.path.dirname(os.path.dirname(os.path.abspath(__file__))))
BIN = os.path.join(ROOT, "target", "debug", "reason-network")
D = tempfile.mkdtemp(prefix="reason-network-e2e-")
H = {"alex": os.path.join(D, "alex"), "alice": os.path.join(D, "alice")}
MACHINE = {"alex": "laptop", "alice": "desktop"}
# A stable fixture folder: long worktree names are legitimately shortened by the engine.
SESSION_REPO = os.path.join(D, "project")
os.makedirs(SESSION_REPO)
FAKE_CODEX = os.path.join(D, "fake-codex.sh")
QUEUE_LOG = os.path.join(D, "codex-queue.log")
# Stands in for osascript / notify-send, so the test shows nothing on screen: each machine logs what it would show.
FAKE_NOTIFY = os.path.join(D, "fake-notify.sh")
IN_CLAUDE = "CLAUDE_CODE_MESSAGING_SOCKET" in os.environ
# First on PATH: nothing here may open a browser or reach this machine's real notifications.
FAKE_BIN = os.path.join(D, "bin")
UNEXPECTED = os.path.join(D, "unexpected.log")
# Reason's relay list, served here: never the real one.
RELAYS_URL = serve_relay_lists({"all": ["sfo1", "iad1", "fra1", "sin1"]}) + "/all.json"
results = []
out_codes = ""


def check(name, ok, detail=""):
    results.append(ok)
    print(f"{'PASS' if ok else 'FAIL'}: {name}" + ("" if ok else f"\n      {detail}"))


def env(who, strip_claude=True, extra=None):
    e = dict(os.environ, REASON_NETWORK_HOME=H[who], REASON_NETWORK_MACHINE=MACHINE[who], REASON_NETWORK_NO_MDNS="1", REASON_NETWORK_NO_SERVICE="1", REASON_NETWORK_CODEX_BIN=FAKE_CODEX)
    e.update({"HOME": os.path.join(D, "home"), "PATH": FAKE_BIN + os.pathsep + os.environ["PATH"], "REASON_NETWORK_NO_UPDATE": "1",
              "REASON_NETWORK_NO_TRAY": "1", "REASON_NETWORK_TRUST_KEYS": TRUST, "REASON_NETWORK_RELAYS_URL": RELAYS_URL})
    e.update(extra or {})
    e.update({"REASON_NETWORK_OSASCRIPT_BIN": FAKE_NOTIFY, "REASON_NETWORK_NOTIFY-SEND_BIN": FAKE_NOTIFY, "DBUS_SESSION_BUS_ADDRESS": "test",
              "NOTIFY_LOG": os.path.join(D, f"notify-{who}.log")})
    if strip_claude:
        for k in list(e):
            if k.startswith("CLAUDE"):
                del e[k]
    # Never this machine's real Claude sessions: Reason Network could deliver into them.
    e["CLAUDE_CONFIG_DIR"] = os.path.join(D, "claude-config")
    return e


def cli(who, *args, extra=None):
    r = subprocess.run([BIN, *args], env=env(who, extra=extra), capture_output=True, text=True, timeout=90)
    return (r.stdout + r.stderr).strip()


def run(e, *args):
    r = subprocess.run([BIN, *args], env=e, capture_output=True, text=True, timeout=90)
    return r.stdout.strip(), (r.stdout + r.stderr).strip()


def as_json(text):
    try:
        return json.loads(text)
    except ValueError:
        return {}


def hook(who, agent, event, payload):
    r = subprocess.run([BIN, "hook", agent, event], env=env(who), capture_output=True, text=True,
                       input=json.dumps(payload), timeout=30)
    return r.stdout.strip()


class Mcp:
    def __init__(self, who, client, strip_claude, cwd):
        self.p = subprocess.Popen([BIN, "mcp"], stdin=subprocess.PIPE, stdout=subprocess.PIPE, env=env(who, strip_claude),
                                  cwd=cwd, text=True, bufsize=1)
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

    def tool(self, name, args, meta=None):
        params = {"name": name, "arguments": args}
        if meta:
            params["_meta"] = meta
        res = self.req("tools/call", params).get("result", {})
        return "\n".join(c.get("text", "") for c in res.get("content", []))

    def close(self):
        self.p.stdin.close()
        self.p.wait(5)


with open(FAKE_CODEX, "w") as f:
    f.write(f'#!/bin/sh\nprintf "%s\\n" "$*" >> {QUEUE_LOG}\necho "Queued message"\n')
os.chmod(FAKE_CODEX, 0o755)
with open(FAKE_NOTIFY, "w") as f:
    f.write('#!/bin/sh\nprintf "%s\\n" "$*" >> "$NOTIFY_LOG"\n')
os.chmod(FAKE_NOTIFY, 0o755)
os.makedirs(FAKE_BIN)
os.makedirs(os.path.join(D, "home"))
for name in ("open", "xdg-open", "osascript", "notify-send"):
    with open(os.path.join(FAKE_BIN, name), "w") as f:
        f.write(f'#!/bin/sh\nprintf "{name} %s\\n" "$*" >> {UNEXPECTED}\n')
    os.chmod(os.path.join(FAKE_BIN, name), 0o755)


def shown(who):
    time.sleep(0.5)
    p = os.path.join(D, f"notify-{who}.log")
    return open(p).read() if os.path.exists(p) else ""


def link_relays(link):
    """A team link's relay fields: the team's own, and the inviting machine's (0xFF: one of n0's, by region)."""
    t = re.split(r"#rn1-|#tincan1", link)[1]
    raw, at, fields = base64.urlsafe_b64decode(t + "=" * (-len(t) % 4)), 32, []
    for _ in range(3):  # the team name first
        short = raw[at] == 0xFF
        at += short
        fields.append(("n0 " if short else "") + raw[at + 1:at + 1 + raw[at]].decode())
        at += 1 + raw[at]
    return fields[1:]


def link_secret(link):
    """The team link's secret (the 16 bytes after the team id), as hex."""
    t = re.split(r"#rn1-|#tincan1", link)[1]
    return base64.urlsafe_b64decode(t + "=" * (-len(t) % 4))[16:32].hex()


os.makedirs(os.path.join(D, "work"), exist_ok=True)

# alice's and alex's machines keep the debug log (and bob's, below, which joins with a code); the others don't, as by
# default.
for home in (H["alice"], H["alex"], os.path.join(D, "bob")):
    os.makedirs(home, exist_ok=True)
    with open(os.path.join(home, "config.toml"), "w") as f:
        f.write("debug_log = true\n")

# --- team formation, signed in with Reason ---
ep = {who: cli(who, "endpoint") for who in H}
check("endpoint prints this machine's id, before any team", all(re.fullmatch(r"[0-9a-f]{64}", e) for e in ep.values()), ep)
check("…the same one each time", cli("alex", "endpoint") == ep["alex"], ep)
out = cli("alex", "init", "vibes", "--as", "whoever", "--proof", proof("u_alex", "alex", ep["alex"]), "--no-setup")
check("init with a proof: the name is the proof's", "Created team vibes. You are alex on laptop." in out, out)
check("…and prints the invite line", re.search(r"Invite people with this line:\nJoin the Reason Network: https://reasonmachines.com/network/join/#rn1-\S+", out) is not None, out)
out = cli("alex", "invite")
link = re.search(r"(https://\S+/join/#rn1-\S+)", out)
check("invite prints the team link, as an rn1 link", bool(link), out)
check("…in the invite line", f"Join the Reason Network: {link.group(1) if link else '?'}" in out, out)
out, _ = run(env("alice"), "join", link.group(1), "--as", "alice", "--proof", proof("u_alice", "alice", ep["alice"]), "--no-setup", "--json")
j = as_json(out)
check("join with a proof and --json gives the team", j.get("team_name") == "vibes" and j.get("person") == "alice" and j.get("machine") == "desktop"
      and j.get("endpoint") == ep["alice"] and re.fullmatch(r"[0-9a-f]{32}", j.get("team_id", "")) is not None, out)
check("…with the link and invite line", j.get("link", "").startswith("https://reasonmachines.com/network/join/#rn1-")
      and j.get("invite_line") == f"Join the Reason Network: {j.get('link')}", out)
members = {m["person"]: m for m in j.get("members", [])}
check("…and both people verified", members.get("alex", {}).get("verified") is True and members.get("alice", {}).get("verified") is True, out)
time.sleep(3)
out = cli("alice", "team")
check("joiner sees alex connected, verified", re.search(r"alex \(verified\)\s+laptop\s+(direct|relay)", out) is not None, out)
out = cli("alex", "team")
check("founder sees alice connected, verified", re.search(r"alice \(verified\)\s+desktop\s+(direct|relay)", out) is not None, out)
check("a team without a relay of its own is on Reason's: the link names the one alex is on",
      link_relays(link.group(1))[0] == "" and re.fullmatch(r"https://relay-\w+\.compute\.reasonmachines\.com/", link_relays(link.group(1))[1])
      and "relay: Reason's (the default)" in out, out)
j = as_json(cli("alex", "team", "--json"))
check("team --json marks people verified, and each machine's proof", [(m["verified"], m["has_proof"]) for m in j.get("members", [])] == [(True, True)] * 2
      and j.get("team_name") == "vibes" and len(j.get("team_id", "")) == 32, j)
check("…and leaves out the team link (agents run it)", "link" not in j and "invite_line" not in j and "rn1-" not in json.dumps(j), j)
j = as_json(cli("alex", "link", "--json"))
check("link --json gives the invite line", j.get("invite_line", "").startswith("Join the Reason Network: https://reasonmachines.com/network/join/#rn1-"), j)
out = cli("alex", "link")
check("link prints the invite line", "Join the Reason Network: https://reasonmachines.com/network/join/#rn1-" in out, out)
j = as_json(cli("alex", "who", "--json"))
check("who --json marks machines verified", all(m.get("verified") for m in j.get("machines", [])) and len(j.get("machines", [])) == 2, j)
out = cli("alice", "proof", proof("u_alice", "alice", ep["alice"]))
check("proof refreshes this machine's proof", "alice on desktop is verified, for 89 more days." in out or "for 90 more days." in out, out)
out = cli("alice", "proof", proof("u_alice", "alicia", ep["alice"]))
check("…but not one for another name", "this proof is for alicia, but this machine is alice in the team" in out, out)
# Someone else signed in, or not at all, can't be alex or alice; a proof Reason didn't sign, or for another machine, gets nobody in.
mal = dict(env("alice"), REASON_NETWORK_HOME=os.path.join(D, "mallory"), REASON_NETWORK_MACHINE="evil")
# mallory's machine keeps a debug log too: it dials the team's machines, which aren't its team's.
os.makedirs(os.path.join(D, "mallory"), exist_ok=True)
with open(os.path.join(D, "mallory", "config.toml"), "w") as f:
    f.write("debug_log = true\n")
mal_ep = run(mal, "endpoint")[0]
out, both = run(mal, "join", link.group(1), "--proof", proof("u_mallory", "alex", mal_ep), "--no-setup", "--json")
check("a name verified by another account is taken: name_taken", as_json(out) == {"error": "name_taken"}, both)
out, both = run(mal, "join", link.group(1), "--as", "alice", "--no-setup", "--json")
check("…and without a proof too", as_json(out) == {"error": "name_taken"}, both)
_, out = run(mal, "join", link.group(1), "--proof", proof("u_mallory", "mallory", mal_ep, key=keygen()), "--no-setup")
check("a forged proof is refused", "signature doesn't verify" in out and "Joined" not in out, out)
_, out = run(mal, "join", link.group(1), "--proof", proof("u_alex", "alex", ep["alex"]), "--no-setup")
check("someone else's proof is refused", "for another machine" in out and "Joined" not in out, out)
_, out = run(mal, "init", "x", "--proof", proof("u_mallory", "mallory", mal_ep, aud="reason"), "--no-setup")
check("init refuses a proof for something else", "isn't from Reason Network" in out and "Created" not in out, out)
out, both = run(mal, "join", link.group(1), "--proof", "garbage", "--no-setup", "--json")
check("with --json every failure is JSON", as_json(out) == {"error": "this sign-in proof is damaged"}, both)
# The Reason CLI joins over the daemon's socket (the proof never on a command line): its error replies carry a code.
sock_path = os.path.join(mal["REASON_NETWORK_HOME"], "network.sock")
if len(sock_path) < 100:  # longer ones get a hashed name elsewhere (see paths::socket)
    import socket
    with socket.socket(socket.AF_UNIX) as sock:
        sock.settimeout(90)
        sock.connect(sock_path)
        req = {"id": 1, "op": "join", "ticket": link.group(1), "person": "x", "proof": proof("u_mallory", "alex", mal_ep)}
        sock.sendall((json.dumps(req) + "\n").encode())
        reply = as_json(sock.makefile().readline())
    check("over the socket, a taken name's error reply has code name_taken",
          reply.get("id") == 1 and reply.get("ok") is False and reply.get("code") == "name_taken"
          and "is already in this team" in reply.get("error", ""), reply)
else:
    check("the socket's path is short enough to reach (set TMPDIR shorter)", False, sock_path)
code = re.search(r"One-time code for a teammate: ([a-z]+(?:-[a-z]+){3})", cli("alex", "invite", "--code"))
_, out = run(mal, "join", code.group(1), "--proof", proof("u_mallory", "mallory", mal_ep), "--no-setup")
check("a sign-in joins only with a link, never a four-word code", "not a four-word code" in out and "Joined" not in out, out)
run(mal, "stop")
check("alex's team still has just the two", len(as_json(cli("alex", "team", "--json")).get("members", [])) == 2)
# alex signed in on a second computer: the same account takes its name there, let in by anyone's machine.
mini = dict(env("alex"), REASON_NETWORK_HOME=os.path.join(D, "alex-mini"), REASON_NETWORK_MACHINE="mini")
mini_ep = run(mini, "endpoint")[0]
alice_link = re.search(r"(https://\S+#rn1-\S+)", cli("alice", "link")).group(1)
_, out = run(mini, "join", alice_link, "--proof", proof("u_alex", "alex", mini_ep), "--no-setup")
check("another computer of the same account joins under its verified name (alice's link: alex's machine lets it in)", "Joined team vibes as alex on mini." in out, out)
check("…and gets the invite line", "Join the Reason Network: https://reasonmachines.com/network/join/#rn1-" in out, out)
time.sleep(3)
out = cli("alex", "team")
check("…which every machine takes", re.search(r"alex \(verified\)\s+mini", out) is not None, out)
# A link this machine made names it first: after leaving, it rejoins through the others on it.
mini_link = re.search(r"(https://\S+#rn1-\S+)", run(mini, "link")[1]).group(1)
run(mini, "leave")
_, out = run(mini, "join", mini_link, "--proof", proof("u_alex", "alex", mini_ep), "--no-setup")
check("a machine that left rejoins with a link it made itself", "Joined team vibes as alex on mini." in out, out)
run(mini, "leave")
run(mini, "stop")
out = cli("alice", "join", link.group(1), "--as", "bob", "--no-setup")
check("joining a second team is refused while yours has others, and says how to switch",
      "already in team 'vibes' (with alex)" in out and "reason-network leave" in out, out)
# Installed on its own and started a team by itself first: the team link still just works.
sam = dict(env("alice"), REASON_NETWORK_HOME=os.path.join(D, "sam"), REASON_NETWORK_MACHINE="mini")
r = subprocess.run([BIN, "init", "solo", "--as", "sam", "--no-setup"], env=sam, capture_output=True, text=True, timeout=90)
solo_link = re.search(r"(https://\S+#rn1-\S+)", run(sam, "link")[1]).group(1)
# (With a link as earlier builds wrote it.)
old_link = link.group(1).replace("#rn1-", "#tincan1")
r = subprocess.run([BIN, "join", old_link, "--as", "sam", "--no-setup"], env=sam, capture_output=True, text=True, timeout=90)
check("a team of one is swapped for the team you join (a tincan1 link)", "Joined team vibes as sam on mini." in r.stdout, r.stdout + r.stderr)
check("…unverified, without a proof", re.search(r"sam\s+mini", r.stdout) is not None and "sam (verified)" not in r.stdout, r.stdout)
subprocess.run([BIN, "leave"], env=sam, capture_output=True, timeout=60)
_, out = run(sam, "join", solo_link, "--as", "sam", "--no-setup")
check("a link that names only this machine, which left, says to ask for a fresh one",
      "names only this computer" in out and "fresh link" in out and "ourself" not in out, out)
subprocess.run([BIN, "stop"], env=sam, capture_output=True)
bob = dict(env("alice"), REASON_NETWORK_HOME=os.path.join(D, "bob"), REASON_NETWORK_MACHINE="nuc")
out = cli("alex", "invite", "--code")
out_codes += out
code = re.search(r"One-time code for a teammate: ([a-z]+(?:-[a-z]+){3})", out)
check("invite --code prints a four-word code", bool(code), out)
r = subprocess.run([BIN, "join", code.group(1).replace("-", " ").upper(), "--as", "bob", "--no-setup"], env=bob,
                   capture_output=True, text=True, timeout=90)
check("joining with just the code works", "Joined team vibes as bob on nuc." in r.stdout, r.stdout + r.stderr)
r = subprocess.run([BIN, "join", code.group(1), "--as", "bob2", "--no-setup"], env=dict(bob, REASON_NETWORK_HOME=os.path.join(D, "bob2")),
                   capture_output=True, text=True, timeout=90)
check("a used code doesn't work twice", "Joined" not in r.stdout + r.stderr, r.stdout + r.stderr)
subprocess.run([BIN, "stop"], env=dict(bob, REASON_NETWORK_HOME=os.path.join(D, "bob2")), capture_output=True)
out = subprocess.run([BIN, "link"], env=bob, capture_output=True, text=True, timeout=30).stdout
check("any member can show the same team link", link.group(1).split("#")[1][:40] in out.replace("\n", ""), out)
subprocess.run([BIN, "stop"], env=bob, capture_output=True)

# --- a team with a long name: shown cut, but whole where the Reason CLI keeps and compares it ---
LONG = "Acme platform and infrastructure engineering, west coast + EMEA" + " (2026)" * 3
CUT = LONG[:59] + "…"
lena = dict(env("alice"), REASON_NETWORK_HOME=os.path.join(D, "lena"), REASON_NETWORK_MACHINE="ws")
lou = dict(env("alice"), REASON_NETWORK_HOME=os.path.join(D, "lou"), REASON_NETWORK_MACHINE="pad")


def over_socket(e, req):
    import socket
    with socket.socket(socket.AF_UNIX) as sock:
        sock.settimeout(90)
        sock.connect(os.path.join(e["REASON_NETWORK_HOME"], "network.sock"))
        sock.sendall((json.dumps(dict(id=1, **req)) + "\n").encode())
        return as_json(sock.makefile().readline()).get("result") or {}


_, out = run(lena, "init", LONG, "--as", "lena", "--no-setup")
check("init with a long team name prints it cut", f"Created team {CUT}." in out and LONG not in out, out)
lj = as_json(run(lena, "link", "--json")[0])
check("…link --json too, with the link", lj.get("team_name") == CUT and lj.get("link", "").startswith("https://"), lj)
run(lou, "who", "--json")  # starts lou's daemon
joined = over_socket(lou, {"op": "join", "ticket": lj.get("link", ""), "person": "lou"})
check("the join reply over the socket names the team whole (the Reason CLI keeps it)",
      joined.get("team_name") == LONG and joined.get("name") == LONG, joined)
# Its daemon restarts onto the team once it has answered (the Reason CLI waits for that too): wait for the new one.
for _ in range(60):
    run(lou, "who", "--json")
    if "as lou@pad" in open(os.path.join(lou["REASON_NETWORK_HOME"], "daemon.log")).read():
        break
    time.sleep(0.5)
for want, got in (("team", over_socket(lou, {"op": "team"}).get("name")), ("link", over_socket(lou, {"op": "link", "new": False}).get("team"))):
    check(f"…as do {want} over the socket on the joined machine, the same as the join reply", got == LONG, got)
tj = as_json(run(lou, "team", "--json")[0])
check("team --json on the joined machine: cut, and no link", tj.get("team_name") == CUT and "link" not in tj, tj)
check("…who --json too", as_json(run(lou, "who", "--json")[0]).get("team") == CUT)
for _ in range(30):
    if re.search(r"lou\s+pad\s+(direct|relay)", run(lena, "team")[1]):
        break
    time.sleep(1)
_, out = run(lena, "team")
check("the joined machine connects to the founder's", re.search(r"lou\s+pad\s+(direct|relay)", out) is not None and CUT in out and LONG not in out, out)
run(lena, "listen", "--as", "notes", "--wait", "0")
_, out = run(lou, "send", "lena/notes", "hello across a team with a long name")
_, out = run(lena, "listen", "--as", "notes", "--wait", "30")
check("…and a message reaches it", "hello across a team with a long name" in out, out)
for e in (lena, lou):
    subprocess.run([BIN, "stop"], env=e, capture_output=True)

# --- removal while online: the removed machine never gets the link that replaces the one it knew ---
eve = dict(env("alice"), REASON_NETWORK_HOME=os.path.join(D, "eve"), REASON_NETWORK_MACHINE="tab")
eve_link = re.search(r"(https://\S+#rn1-\S+)", cli("alex", "link")).group(1)
_, out = run(eve, "join", eve_link, "--as", "eve", "--no-setup")
check("eve joins", "Joined team vibes as eve on tab." in out, out)
for _ in range(30):
    if re.search(r"eve\s+tab\s+(direct|relay)", cli("alex", "team")):
        break
    time.sleep(1)
out = cli("alex", "remove", "eve")
check("remove eve while her machine is online", "Removed eve on tab" in out, out)
for _ in range(30):
    if "not in a team" in run(eve, "team")[1]:
        break
    time.sleep(1)
check("eve's machine hears it was removed", "not in a team" in run(eve, "team")[1])
new_link = re.search(r"(https://\S+#rn1-\S+)", cli("alex", "link")).group(1)
new_secret = link_secret(new_link)
check("the link changed", new_secret != link_secret(eve_link))
held = b"".join(open(os.path.join(D, "eve", f), "rb").read() for f in ("network.db", "network.db-wal", "network.db-shm")
                if os.path.exists(os.path.join(D, "eve", f)))
check("the removed machine's database and its log hold no current link secret", held and new_secret.encode() not in held,
      f"{len(held)} bytes")
seen = set(re.findall(rb'"secret":"([0-9a-f]{32})"', held))
check("…nor any link secret it could rebuild a link from", all(sec.decode() != new_secret for sec in seen), seen)
_, out = run(eve, "join", eve_link, "--as", "eve2", "--no-setup")
check("…so it can't rejoin with the old link", "Joined" not in out and "replaced" in out, out)
if os.name != "nt":
    modes = {f: oct(os.stat(os.path.join(D, "eve", f)).st_mode & 0o777) for f in ("network.db", "network.db-wal", "network.db-shm")
             if os.path.exists(os.path.join(D, "eve", f))}
    check("the database files are the user's alone", modes and all(m == "0o600" for m in modes.values()), modes)
run(eve, "stop")
out = cli("alice", "team")
check("the others are still connected", re.search(r"alex \(verified\)\s+laptop\s+(direct|relay)", out) is not None, out)

# --- a folder that is off: its sessions are never shared ---
PRIVATE = os.path.join(D, "private")
os.makedirs(PRIVATE, exist_ok=True)
out = cli("alex", "folder", PRIVATE, "off")
check("folder <path> off turns Reason Network off there", "Reason Network is off in" in out and "private on` undoes it" in out, out)
check("…in the config's [folders]", re.search(r'\[folders\]\n".*private" = "off"', open(os.path.join(H["alex"], "config.toml")).read()) is not None)
out = cli("alex", "folder", os.path.join(PRIVATE, "sub"), "on")
check("…and a folder in it stays off", "sub is still off: it is in" in out, out)
time.sleep(3)

# --- sessions on both machines ---
claude = Mcp("alex", "claude-code", strip_claude=False, cwd=SESSION_REPO)
hidden = Mcp("alex", "claude-code", strip_claude=True, cwd=PRIVATE)
codex = Mcp("alice", "codex-mcp-client", strip_claude=True, cwd=os.path.join(D, "work"))
meta = {"sessionId": "11111111-aaaa", "threadId": "11111111-aaaa"}
time.sleep(7)  # Claude's session registers after 3s; presence propagates every 2s
out = codex.tool("list_team", {}, meta)
claude_name = "project-claude"
check("alice's agent sees alex's session across machines", claude_name in out and "alex (verified) · laptop" in out, out)
check("list_team gives no team link, and says who invites people", "rn1-" not in out and "your user runs `reason-network invite`" in out, out)
check("…but not the one in alex's folder that is off", "private-claude" not in out, out)
out = hidden.tool("list_team", {})
check("in a folder that is off, the tools say so", out == "Your user turned Reason Network off for this folder", out)
out = hook("alex", "codex", "session-start", {"session_id": "33333333-cccc", "cwd": PRIVATE, "hook_event_name": "SessionStart"})
check("…and hooks say nothing", out == "", out)
out = hook("alex", "cursor", "session-start", {"conversation_id": "c-off", "workspace_roots": [os.path.join(D, "work"), PRIVATE]})
check("…nor for a session with that folder open beside another", out == "", out)
# A person's terminal: none of the variables agents set for their commands (this test may run in an agent's).
AGENT_VARS = ("AI_AGENT", "CODEX_", "GEMINI_CLI", "REASON_NETWORK_AGENT", "TINCAN_AGENT", "GROK_", "CURSOR_AGENT", "OPENCODE", "COPILOT_")
person = {k: v for k, v in env("alex").items() if not k.startswith(AGENT_VARS)}
r = subprocess.run([BIN, "send", "alice/work-codex", "from alex's terminal in the folder that is off"], env=person, cwd=PRIVATE,
                   capture_output=True, text=True, timeout=90)
check("the person's own terminal there still sends", "→ alice/work-codex" in r.stdout, r.stdout + r.stderr)
out = codex.tool("check_inbox", {}, meta)
check("…as alex/terminal, from alex's laptop", 'from="alex/terminal@laptop"' in out and "in the folder that is off" in out, out)
with open(os.path.join(PRIVATE, "notes.txt"), "w") as f:
    f.write("private\n")
agent_shell = dict(person, CLAUDECODE="1", CLAUDE_CODE_SESSION_ID="44444444-dddd", CLAUDE_PROJECT_DIR=PRIVATE)
r = subprocess.run([BIN, "send", "alice/work-codex", "--file", "notes.txt", "from an agent's shell there"], env=agent_shell, cwd=PRIVATE,
                   capture_output=True, text=True, timeout=90)
check("…but an agent's shell there sends nothing, not even as the terminal", "turned Reason Network off for this folder" in r.stderr
      and "→" not in r.stdout, r.stdout + r.stderr)
out = cli("alex", "who")
check("…nor is either session on alex's own list", "private-" not in out and claude_name in out, out)
out = cli("alice", "send", "alex/private-claude", "x")
check("…and a teammate can't message it", "no session called 'alex/private-claude'" in out, out)
hidden.close()
# (After Codex's first tool call registered it.) Nobody reports a status: Reason Network reads the plan step in progress from Codex's own transcript.
rollout = os.path.join(D, "rollout.jsonl")
plan = {"plan": [{"step": "set up fixtures", "status": "completed"}, {"step": "testing cross-machine Reason Network", "status": "in_progress"}]}
with open(rollout, "w") as f:
    f.write(json.dumps({"type": "response_item", "payload": {"type": "function_call", "name": "update_plan", "arguments": json.dumps(plan)}}) + "\n")
hook("alice", "codex", "doing", {"session_id": "11111111-aaaa", "cwd": os.path.join(D, "work"), "transcript_path": rollout})
time.sleep(3)
out = cli("alex", "who")
check("alex sees what alice's codex is doing, read from its plan", "work-codex" in out and "testing cross-machine Reason Network" in out, out)

# --- messages across machines ---
def counts(who):
    """Today's (sent, received) on `who`'s machine, from the daemon's team reply, where the Reason CLI reads them."""
    import socket
    with socket.socket(socket.AF_UNIX) as sock:
        sock.settimeout(30)
        sock.connect(os.path.join(H[who], "network.sock"))
        sock.sendall(b'{"id": 1, "op": "team"}\n')
        days = as_json(sock.makefile().readline()).get("result", {}).get("messages", [])
    today = next((d for d in days if d["day"] == time.strftime("%Y-%m-%d", time.gmtime())), {})
    return today.get("sent", 0), today.get("received", 0)
before = {who: counts(who) for who in H}
out = codex.tool("send_message", {"to": f"alex/{claude_name}", "message": "Reason Network e2e (cross-machine push): ignore this."}, meta)
want = "delivered into the session" if IN_CLAUDE else "only sees it when it checks"
check("cross-machine send reaches the Claude session", want in out, out)
out = cli("alex", "send", "alice/work-codex", "hello from alex's terminal")
check("a terminal send to a remote pull session is queued there", "only sees it when it checks its inbox" in out, out)
after = {who: counts(who) for who in H}
check("each machine counts the messages it sent and received today",
      all(after[who] == (before[who][0] + 1, before[who][1] + 1) for who in H), (before, after))
out = codex.tool("check_inbox", {}, meta)
check("remote message arrives in codex inbox, from alex's terminal on his laptop", 'from="alex/terminal@laptop"' in out and "hello from alex's terminal" in out, out)
out = codex.tool("check_inbox", {}, meta)
check("…and is gone once read", out.startswith("No messages."), out)
out = cli("alice", "send", "alex", "dinner at 8?")
check("a person is not an address: the error lists their agent sessions",
      "messages go to agent sessions; alex's are:" in out and f"alex/{claude_name}" in out, out)
out = codex.tool("send_message", {"to": f"alex/{claude_name}", "message": "can you approve the deploy? (e2e test, ignore)", "notify": True}, meta)
check("notify: the message also pops up on the person's screen", "popped up for alex (laptop)" in out, out)
log = shown("alex")
check("…naming who asks, which session, and the ask", f"alice/work-codex → {claude_name}" in log and "approve the deploy" in log, log)
out = cli("alice", "send", f"alex/{claude_name}", "--notify", "and from the terminal (e2e test, ignore)")
check("one pop-up per person a minute", "not popped up: laptop had one from alice" in out and "from the terminal" not in shown("alex"), out)
out = codex.tool("send_message", {"to": "alex", "message": "x", "notify": True}, meta)
check("a teammate's pop-up goes with a message to one of their sessions", "messages go to agent sessions" in out, out)
out = codex.tool("send_message", {"to": "alice", "message": "need your ok (e2e test, ignore)", "notify": True}, meta)
check("an agent can tap its own user on the shoulder", "popped up for alice (desktop)" in out and "need your ok" in shown("alice"), out)
out = codex.tool("send_message", {"to": "alex/*", "message": "x", "notify": True}, meta)
check("notify doesn't go to groups", "not a group" in out, out)
# No waiting for presence: the terminal that just messaged alice can be answered at once.
out = codex.tool("send_message", {"to": "alex/terminal", "message": "codex here: replying to your terminal"}, meta)
check("an agent can reply at once to a terminal that just messaged it", "→ alex/terminal@laptop: queued" in out or "→ alex/terminal@laptop: delivered" in out, out)
time.sleep(1)
out = cli("alex", "listen", "--as", "terminal", "--wait", "5")
check("…and `reason network listen --as terminal` shows the reply", "replying to your terminal" in out and 'from="alice/work-codex"' in out, out)
# An ask of a teammate's terminal, answered there with no reply_to: the ask's receipt names the terminal as the answer
# comes from, which is how the Reason CLI tells the answer.
out = codex.tool("send_message", {"to": "alex/terminal", "message": "codex here: ok to ship? (e2e test, ignore)"}, meta)
asked = re.search(r"→ (\S+): ", out)
check("a teammate's terminal is named in the receipt with its machine", asked is not None and asked.group(1) == "alex/terminal@laptop", out)
cli("alex", "listen", "--as", "terminal", "--wait", "5")
cli("alex", "send", "alice/work-codex", "--from", "alex/terminal", "yes, ship it")
time.sleep(1)
out = codex.tool("check_inbox", {}, meta)
answer = re.search(r'from="([^"]+)"[^>]*>\s*yes, ship it', out)
check("…and the answer comes from exactly that address", answer is not None and asked is not None and answer.group(1) == asked.group(1), out)
cli("alice", "listen", "--as", "fresh", "--wait", "0")
cli("alice", "send", "alex/terminal", "--from", "alice/fresh", "first words from a new session")
out = cli("alex", "send", "alice/fresh", "answered at once")
check("a brand-new session that just messaged can be answered at once", "→ alice/fresh" in out and "no session" not in out, out)
out = cli("alice", "listen", "--as", "fresh", "--wait", "5")
check("…and it gets the answer", "answered at once" in out, out)
cli("alex", "listen", "--as", "terminal", "--wait", "0")
out = cli("alex", "send", "*", "a script's broadcast (e2e test, ignore)")
check("a group send from a script, not a person at the terminal, is refused", "only a person at a terminal" in out, out)
out = cli("alex", "send", "*", "--from", "alex/terminal", "broadcast from alex (e2e test, ignore)")
check("…but goes out with --from alex/terminal", "alice/work-codex" in out, out)
out = codex.tool("check_inbox", {}, meta)
check("…and only that one arrived", "broadcast from alex" in out and "a script's broadcast" not in out, out)
for to in ("alex/*", "*@laptop"):
    out = codex.tool("send_message", {"to": to, "message": "an agent's broadcast (e2e test, ignore)"}, meta)
    check(f"an agent can't send to a group ({to})", "agents message one session, not a group" in out and "→" not in out, out)
out = cli("alex", "send", "nobody", "x")
check("unknown name fails helpfully", "no session called 'nobody'" in out, out)

# --- a session waiting in check_inbox gets the reply there, once ---
import threading
waited = {}
t = threading.Thread(target=lambda: waited.setdefault("out", codex.tool("check_inbox", {"wait_secs": 15}, meta)))
t.start()
time.sleep(2)
out = cli("alex", "send", "alice/work-codex", "reply while you wait")
t.join()
check("long-poll returns the message that arrives", "reply while you wait" in waited.get("out", ""), waited)

# --- small text files ---
FILES = os.path.join(D, "files")
os.makedirs(FILES, exist_ok=True)
def write(name, data):
    path = os.path.join(FILES, name)
    with open(path, "wb") as f:
        f.write(data if isinstance(data, bytes) else data.encode())
    return path
def inbox(who):
    p = os.path.join(H[who], "inbox")
    return sorted(os.listdir(p)) if os.path.isdir(p) else []
def message_with(out, text):
    """The part of an inbox listing for the message saying `text`: its frame and the file list after it."""
    return next((part for part in out.split("<reason-message") if text in part.split("</reason-message>")[0]), "")
def saved(out, who):
    """The file paths a message lists, if each is in `who`'s inbox folder, readable and not executable."""
    paths = re.findall(r"^- (/.+)$", out, re.M)
    ok = all(p.startswith(os.path.join(H[who], "inbox") + os.sep) and os.path.isfile(p) and not os.stat(p).st_mode & 0o111
             for p in paths)
    return paths if ok else []
notes = write("notes.md", "# Notes\nline two \u2014 caf\u00e9\n")
config = write("app.json", '{"port": 8080}\n')
out = cli("alex", "send", "alice/work-codex", "--file", notes, "--file", config, "the notes and the config (e2e test)")
check("a send with files is accepted", "→ alice/work-codex" in out and "refused" not in out, out)
out = codex.tool("check_inbox", {}, meta)
paths = saved(out, "alice")
check("files arrive in the message, listed by where they are saved",
      "Files they sent with" in out.split("</reason-message>")[-1] and [os.path.basename(p) for p in paths] == ["notes.md", "app.json"], out)
check("…in folders named by the recipient, not by the sender's message id",
      all(re.fullmatch(r"[0-9A-HJKMNP-TV-Z]{26}", os.path.basename(os.path.dirname(p))) for p in paths)
      and re.search(r'id="([0-9A-Z]{26})"', out).group(1) not in inbox("alice"), out)
check("…with the same text, readable and not executable",
      len(paths) == 2 and open(paths[0], encoding="utf-8").read() == "# Notes\nline two \u2014 caf\u00e9\n" and open(paths[1]).read() == '{"port": 8080}\n', paths)
with open(os.path.join(D, "work", "reply.md"), "w") as f:
    f.write("relative paths are from the agent's folder\n")
out = codex.tool("send_message", {"to": "alex/terminal", "message": "a file back (e2e test)", "files": ["reply.md"]}, meta)
check("an agent sends a file by a path relative to its folder", "→ alex/terminal" in out and "refused" not in out, out)
out = cli("alex", "listen", "--as", "terminal", "--wait", "5")
paths = saved(out, "alex")
check("…and it lands on the other machine", len(paths) == 1 and open(paths[0]).read().startswith("relative paths"), out)
def work(name, data):
    path = os.path.join(D, "work", name)
    os.makedirs(os.path.dirname(path), exist_ok=True)
    with open(path, "wb") as f:
        f.write(data if isinstance(data, bytes) else data.encode())
    return name
out = codex.tool("send_message", {"to": "alex/terminal", "message": "x", "files": [work("logo.png", b"\x89PNG\r\n\x1a\n\x00\x00")]}, meta)
check("a binary file is refused, saying why", "looks like a binary file" in out and "only text files" in out and "→" not in out, out)
out = codex.tool("send_message", {"to": "alex/terminal", "message": "x", "files": [work("term.log", "\x1b[31mred\x1b[0m\n")]}, meta)
check("…and so is text with control characters", "control characters" in out and "→" not in out, out)
out = codex.tool("send_message", {"to": "alex/terminal", "message": "x", "files": [work("big.log", "x" * (256 * 1024 + 1))]}, meta)
check("a file over 256 KB is refused", "a file can be at most 256 KB" in out and "→" not in out, out)
# An agent sends only from its own folder: nothing outside it, nothing hidden (a prompt-injected agent can't mail secrets).
work(".env", "SECRET=1\n")
for path, why in [(notes, "outside this session's folder"), (".env", "hidden file"), ("../alice/machine.key", "outside this session's folder")]:
    out = codex.tool("send_message", {"to": "alex/terminal", "message": "x", "files": [path]}, meta)
    check(f"an agent can't send {os.path.basename(path)} ({why})", why in out and "→" not in out, out)
out = codex.tool("send_message", {"to": "alex/terminal", "message": "x", "files": [notes] * 11}, meta)
check("more than 10 files are refused", "at most 10" in out and "→" not in out, out)
out = cli("alex", "send", "alice/work-codex", "--file", FILES, "x")
check("a folder is not a file", "is not a file" in out, out)
# A teammate's files are taken up to 20 MB an hour; past that their machine keeps them and retries later.
quota = [write("q1.log", "q" * (256 * 1024)), write("q2.log", "r" * (256 * 1024))]
# (alex sent a few small files above, so 39 of 512 KB fit the hour, and the 40th doesn't.)
sent = [cli("alex", "send", "alice/work-codex", "--file", quota[0], "--file", quota[1], f"quota {i} (e2e test)") for i in range(39)]
check("39 messages of 512 KB of files are taken", all("→ alice/work-codex" in o and "can't take" not in o for o in sent), sent[-1])
out = cli("alex", "send", "alice/work-codex", "--file", quota[0], "--file", quota[1], "one more (e2e test)")
check("…and the next waits: the sender's machine retries later", "can't take these files now" in out and "20 MB of files an hour from alex" in out, out)
out = codex.tool("check_inbox", {}, meta)
check("…the 39 arrive", out.count("quota ") == 39 and "one more" not in out, out[-500:])

# --- Codex hooks: session via hooks, delivery via codex queue ---
start = hook("alice", "codex", "session-start", {"session_id": "22222222-bbbb", "cwd": SESSION_REPO, "hook_event_name": "SessionStart"})
codex_name = "project-codex"
check("codex SessionStart hook introduces the session", "SessionStart" in start and f"alice/{codex_name}" in start, start)
time.sleep(3)
out = cli("alex", "send", f"alice/{codex_name}", "please rebase on main")
check("message to a hooked codex session goes to codex queue", "queued into the Codex session" in out, out)
time.sleep(1.5)
log = open(QUEUE_LOG).read() if os.path.exists(QUEUE_LOG) else ""
check("codex queue got the thread and the message", "queue --thread 22222222-bbbb --message" in log and "please rebase on main" in log, log)
out = hook("alice", "codex", "post-tool-use", {"session_id": "22222222-bbbb", "cwd": SESSION_REPO, "hook_event_name": "PostToolUse"})
check("a message delivered by codex queue isn't injected again by the hook", out == "", out)

# --- hold policy ---
cfg = os.path.join(H["alice"], "config.toml")
text = open(cfg).read()
text = text.replace("[inbound.people]", '[inbound.people]\nalex = "hold"', 1) if "[inbound.people]" in text else text + '\n[inbound.people]\nalex = "hold"\n'
open(cfg, "w").write(text)
cli("alice", "stop")
time.sleep(3)
cli("alice", "team")
time.sleep(5)
before = inbox("alice")
totals = {who: counts(who) for who in H}
out = cli("alex", "send", "alice/work-codex", "--file", notes, "held message")
check("message to alice is held by her policy", "held until alice approves" in out, out)
check("a held message counts as sent, and as received only once approved",
      counts("alex")[0] == totals["alex"][0] + 1 and counts("alice") == totals["alice"], (totals, counts("alex"), counts("alice")))
out = codex.tool("check_inbox", {}, meta)
check("held message is not delivered", "held message" not in out, out)
check("…and its files aren't saved yet", inbox("alice") == before, inbox("alice"))
out = cli("alice", "held")
check("Reason Network held lists it", "held message" in out and "notes.md (saved once approved)" in out, out)
approved = cli("alice", "approve", "--all")
out = codex.tool("check_inbox", {}, meta)
check("approved message is delivered", "held message" in out, out)
# (With any of the quota test's messages that came back while alex was held.)
n = int(re.search(r"Delivered (\d+)", approved).group(1))
check("…and counted as received now", n >= 1 and counts("alice")[1] == totals["alice"][1] + n, (approved, totals, counts("alice")))
check("…with its files saved now", [os.path.basename(p) for p in saved(message_with(out, "held message"), "alice")] == ["notes.md"], out)
# Refused: nothing of it is kept.
text = open(cfg).read()
open(cfg, "w").write(text.replace('alex = "hold"', 'alex = "refuse"'))
cli("alice", "stop")
time.sleep(3)
cli("alice", "team")
time.sleep(5)
before = inbox("alice")
totals = {who: counts(who) for who in H}
out = cli("alex", "send", "alice/work-codex", "--file", notes, "refused message")
check("a refused message's files are never saved", "doesn't accept messages from alex" in out and inbox("alice") == before, out)
check("…and it counts as neither sent nor received", {who: counts(who) for who in H} == totals, totals)
open(cfg, "w").write(text)
cli("alice", "stop")
time.sleep(3)
cli("alice", "team")
time.sleep(5)

# --- a session forgotten with mail it never read: the sender hears, even when its machine was off then ---
os.makedirs(os.path.join(D, "drop"))
dropper = Mcp("alex", "codex-mcp-client", strip_claude=True, cwd=os.path.join(D, "drop"))
dropper.tool("list_team", {}, {"sessionId": "44444444-dddd", "threadId": "44444444-dddd"})
dropper.close()
time.sleep(4)
bodies = ["dropped unread (e2e test)\nthis second line isn't quoted", "queued before the upgrade (e2e test)"]
for body in bodies:
    out = codex.tool("send_message", {"to": "alex/drop-codex", "message": body}, meta)
    check(f"mail for a session that is offline is queued on its machine: {body.splitlines()[0]}", "queued: last seen" in out, out)
codex.close()  # (its MCP server would start alice's machine again)
cli("alice", "stop")
cli("alex", "stop")
db = sqlite3.connect(os.path.join(H["alex"], "network.db"), timeout=30)
rows = [db.execute("SELECT id, sent_at FROM messages WHERE body = ?", (body,)).fetchone() for body in bodies]
# The second as an engine from before notices kept it: without the machine it came from.
db.execute("UPDATE messages SET source = NULL WHERE body = ?", (bodies[1],))
db.execute("UPDATE sessions SET last_seen = ? WHERE name = 'drop-codex'", (int(time.time() * 1000) - 2 * 86_400_000,))
db.commit()
# alex's machine back, the session a day gone: forgotten, with its mail. Its daemon forgets from the start, not six hours
# after it (debug builds only), until it is started again below.
cli("alex", "who", extra={"REASON_NETWORK_FORGET_GRACE_MS": "0"})
time.sleep(1)
outbox = [json.loads(m) for (m,) in db.execute("SELECT msg FROM outbox WHERE target = ?", (ep["alice"],))]
left = db.execute("SELECT COUNT(*) FROM messages WHERE body IN (?, ?)", bodies).fetchone()[0]
db.close()
check("…dropped once the session is forgotten, each leaves a notice for the sender's machine, which is off (one from before notices too)",
      None not in rows and left == 0 and sorted(m.get("reply_to") for m in outbox if m.get("from_agent") == "reason-network") == sorted(r[0] for r in rows),
      (rows, outbox))
cli("alice", "team")
for _ in range(60):
    held = cli("alice", "held")
    if held.count("Not delivered") == 2:
        break
    time.sleep(1)
check("…which come once it is back, and are held like any mail from alex (the hold policy above)", held.count("Not delivered") == 2, held)
cli("alice", "approve", "--all")
codex = Mcp("alice", "codex-mcp-client", strip_claude=True, cwd=os.path.join(D, "work"))
out = codex.tool("check_inbox", {}, meta)
for body, row in zip(bodies, rows):
    first = body.splitlines()[0]
    when = time.strftime("%Y-%m-%d %H:%M UTC", time.gmtime(row[1] / 1000)) if row else "?"
    check(f"the sender gets it once back: who the mail was for, when it was sent, its first line ({first})",
          f'Not delivered: your message to alex/drop-codex (sent {when}) was dropped unread: "{first}"' in message_with(out, f'"{first}"'), out)
check("…only the first line, from Reason Network", "second line" not in out and "From Reason Network on alex's machine." in out, out)
out = codex.tool("check_inbox", {}, meta)
check("…once", "Not delivered" not in out, out)
cli("alex", "stop")  # the next command starts it again as usual

# --- offline machine: outbox ---
codex.close()
cli("alice", "stop")
time.sleep(1)
sent = counts("alex")[0]
out = cli("alex", "send", "alice/work-codex", "sent while you were offline")
check("message to an offline machine is queued", "not connected" in out or "no answer" in out, out)
check("…and not counted as sent while it waits in the outbox", counts("alex")[0] == sent, (sent, counts("alex")))
cli("alice", "team")  # restarts alice's daemon
time.sleep(8)
cli("alice", "approve", "--all")  # alice still holds messages from alex (the hold policy above)
codex = Mcp("alice", "codex-mcp-client", strip_claude=True, cwd=os.path.join(D, "work"))
out = codex.tool("check_inbox", {}, meta)
check("outbox delivers when the machine is back", "sent while you were offline" in out, out)
check("…and counts as sent once it got there", counts("alex")[0] == sent + 1, (sent, counts("alex")))
codex.close()

# --- removal while away ---
cli("alice", "stop")
out = cli("alex", "remove", "alice")
check("remove alice (while her machine is off)", "Removed alice on desktop" in out, out)
check("removing someone replaces the team link", "team link changed" in out, out)
out = cli("alex", "link")
check("the new link differs from the old one", link.group(1) not in out, out)
check("…and is still an rn1 link", "#rn1-" in out, out)
cli("alice", "team")  # alice's machine comes back and dials alex
time.sleep(10)
out = cli("alice", "team")
check("alice's machine learns it was removed when it reconnects", "not in a team" in out, out)

out = cli("alex", "folder", PRIVATE, "on")
check("folder <path> on puts it back", "Reason Network is on in" in out and "private" not in open(os.path.join(H["alex"], "config.toml")).read(), out)

claude.close()
cli("alex", "stop")
cli("alice", "stop")

# --- the debug log: the networking library's events, on the machines that turned it on, with nothing anyone wrote ---
logs = {who: os.path.join(home, "debug.log") for who, home in (("alice", H["alice"]), ("alex", H["alex"]), ("bob", os.path.join(D, "bob")))}
text = {who: open(p, errors="replace").read() if os.path.exists(p) else "" for who, p in logs.items()}
alltext = "\n".join(text.values())
check("debug_log = true writes the networking library's events to debug.log",
      all(re.search(r"^\[\d+\] (DEBUG|INFO|WARN|ERROR) (iroh|noq)", t, re.M) for t in text.values()), {w: t[:300] for w, t in text.items()})
check("…and the daemon's own network lines", "net: connected to" in text["alice"], text["alice"][:500])
check("…readable by its owner only", os.name == "nt" or all(os.stat(p).st_mode & 0o777 == 0o600 for p in logs.values()), logs)
check("without debug_log, no debug log", not any(os.path.exists(os.path.join(D, h, "debug.log")) for h in ("bob2", "sam", "lena")))
key = open(os.path.join(H["alice"], "machine.key"), "rb").read()
said = ["hello from alex's terminal", "dinner at 8?", "can you approve the deploy?", "codex here", "yes, ship it",
        "first words from a new session", "answered at once", "broadcast", "sent while you were offline",
        "from an agent's shell there", "cross-machine push", "line two", '"port": 8080', "q" * 64]
held = [link.group(1), link.group(1).split("#", 1)[1], link_secret(link.group(1)), "rn1-", "tincan1", "/join/#",
        key.hex(), base64.b64encode(key).decode().rstrip("="), base64.urlsafe_b64encode(key).decode().rstrip("="), "eyJ"]
found = [s for s in said + held if s in alltext]
check("…with no message text, file, link, ticket, key or sign-in proof in it", not found, found)
# The four-word code's own key (the inviting machine listens on it, the joiner dials it) is short enough to find from
# offline: neither log shows it, not even the 10 characters iroh shows of an id. Every id left is a machine's.
check("the invite code's key is hidden on both sides", "[invite]" in text["alex"] and "[invite]" in text["bob"],
      [l for l in alltext.splitlines() if "bound" in l][:5])
machines = [run(dict(env("alice"), REASON_NETWORK_HOME=os.path.join(D, h)), "endpoint")[0] for h in os.listdir(D)
            if os.path.exists(os.path.join(D, h, "machine.key"))]
ids = set(re.findall(r"(?<![0-9A-Za-z])([0-9a-f]{10})(?:…)?(?![0-9A-Za-z])", alltext))
unknown = sorted(i for i in ids if re.search(r"[a-f]", i) and not any(m.startswith(i) for m in machines))
check("…and every id in the logs is one of the machines'", not unknown, unknown)
# A machine outside the team (mallory's, or one on the local network) shows only as [id], on either side.
mal_log = open(os.path.join(D, "mallory", "debug.log"), errors="replace").read() if os.path.exists(os.path.join(D, "mallory", "debug.log")) else ""
check("ids of machines outside this one's team show as [id]: mallory's log names the team's machines only that way",
      "[id]" in mal_log and not any(ep[w][:10] in mal_log for w in ("alex", "alice")), mal_log[:300])
check("…and the team's logs never show mallory's machine", mal_ep[:10] not in alltext and mal_ep[:8] not in alltext)
for code_ in re.findall(r"One-time code for a teammate: ([a-z]+(?:-[a-z]+){3})", out_codes):
    check(f"…nor the code itself", code_ not in alltext and code_.replace("-", " ") not in alltext.lower())
log = open(UNEXPECTED).read() if os.path.exists(UNEXPECTED) else ""
check("nothing opened a browser or reached the real notifications", log == "", log)
print(f"\n{sum(results)}/{len(results)} checks passed  (state in {D})")
raise SystemExit(0 if all(results) else 1)
