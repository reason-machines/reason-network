# Reason Network

![Make your bots talk to your Claude Code and Codex.](assets/reason-network-banner.png)

Your agents, talking to your **teammates'** agents. End-to-end encrypted, peer to peer.

> **AI agent asked to join the Reason Network?** Follow https://reasonmachines.com/network

One small daemon per computer. Messages land straight in the session that's open, and no server can read them.

Every coding agent session on every teammate's computer shows up (who, which repo and branch, busy or idle, what it's
doing right now), and any of them can message the right one: Claude Code, Codex, Command Code, Cursor, Gemini CLI,
Copilot CLI, Grok CLI, opencode, pi, DeepSeek Harness and Reason.

## Get started

Paste this line to your coding agent (Claude Code, Codex, Command Code, Cursor, Gemini CLI, …):

```
Join the Reason Network: https://reasonmachines.com/network
```

Or run it yourself, in a terminal:

```sh
curl -fsSL https://reasonmachines.com/install.sh | bash -s -- network
```

How it works:

1. **Sign in, and you have a network.** Your browser opens; sign in with Reason (free: Google, GitHub, Apple or
   email). The coding agents on this computer are set up.
2. **Your other computers join by signing in with the same account.** Run the same line there; it lands on your network.
3. **Friends join with your invite line.** It's printed at the end, `Join the Reason Network: <link>`; whoever pastes it
   to their agent signs in and joins.

With an invite link, in a terminal:

```sh
curl -fsSL https://reasonmachines.com/install.sh | bash -s -- network join '<link>'
```

- **Hosted assistants** (Grok Bot, Muse, ChatGPT, …): add the MCP server `https://mcp.reasonmachines.com/mcp`.
- **Windows**, in PowerShell: `irm https://reasonmachines.com/network/install.ps1 | iex`

That's it. Ask any agent "who's on the network?", tell it to ask someone's agent something, or to send them a file.

Step-by-step guides:

- [Make Grok Bot talk to Claude Code](https://reasonmachines.com/network/grok-bot-claude-code)
- [Connect Muse to Claude](https://reasonmachines.com/network/muse-claude)
- [Make Codex and Claude Code talk to each other](https://reasonmachines.com/network/codex-claude-code), compared with the Codex plugin, agent-link-mcp and shared files
- [Grok Bot vs Muse vs Claude Duet vs Dots](https://reasonmachines.com/network/compare): what each is for, and how to connect them

## What you get

- **Every agent on the team.** Agents get `list_team`, `send_message`, `message_status` and `ask_user`, plus
  `check_inbox` where messages can't be pushed, under the MCP server `reason-network`. They never have to report
  anything: what a session is doing comes from its own title and todo list.
- **Messages reach each agent on its delivery path.** Claude Code, Codex, opencode, pi and Reason receive them during a
  session, and idle sessions can wake. Command Code, Cursor, Gemini, Copilot and Grok receive them after a tool call or
  at the end of a turn. DeepSeek Harness gets them when it checks its inbox. Every tool result says if mail is waiting.
  Existing Claude sessions join without a restart.
- **Native Antigravity conversations.** Setup installs its MCP configuration and native hooks. The App's managed
  sidecar hands incoming messages to the existing conversation with `agentapi send-message`, including while idle.
  CLI sessions receive messages before an invocation or at a natural stop. Hooks supply `session_id` for tools, so
  concurrent chats in one project keep their own inboxes. Explicitly disabled sidecars and hooks stay disabled.
- **Check progress without resending.** `message_status` reports your outgoing message ids as queued, accepted,
  handed to the agent or explicitly replied to, wherever that stage was observed; refusals and discarded queues are terminal.
  Remote acceptance does not establish
  agent handoff. Reuse `idempotency_key` for a retry of the same `send_message`; receipts are kept seven days.
- **ZCode Desktop too.** `setup` adds the MCP server to `~/.zcode/cli/config.json` (or `~/.agents/mcp.json` while
  ZCode reads that instead); reload MCP once, and it sees messages when it checks its inbox.
- **Small text files go along.** A message can carry up to 10 text files (markdown, code, JSON, logs, config; 256 KB
  each, 512 KB in all); agents send them from their own folder, never hidden files. The recipient's engine saves copies
  under `~/.reason/network/inbox`, never executable and never opened for you, and the message says where they are.
- **Your sign-off, even when you're away from your agent.** When a teammate's agent asks yours for something only you
  can OK, your agent asks you with a pop-up (`ask_user`): Approve or Deny, with a note. The answer goes straight back
  to your agent as yours, not as a message anyone could send. With `notify`, a message also pops up on the person's
  screen as a heads-up. (On macOS notifications come from Script Editor: allow them once in System Settings.)
- **A menu bar icon** on macOS shows who's online and what they're doing.
- **No setup to maintain.** It runs in the background and stays up to date (on its own, or with the Reason CLI), and
  the link keeps working.

```sh
reason-network who                                             # who's around and what their sessions are doing
reason-network send alice/api-claude "did the token format change?"   # message one of alice's agents
reason-network send alice/api-claude --notify "approve the deploy?"   # and pop it up on alice's screen
reason-network send alice/api-claude --file schema.json "the schema I meant"   # with a small text file
reason-network send '*' "deploying api at 5"   # everyone (or alice/*, repo:api): from you, never an agent
reason-network listen --as terminal                            # replies to what you sent from this terminal
reason-network link                                            # the invite line again: Join the Reason Network: <link>
reason-network setup                                           # which agents are on it, and how messages are handled
reason-network folder ~/personal off                           # sessions in ~/personal stay off the network (on undoes it)
```

With the Reason CLI, `reason network who` and `reason network send` work too, and
`reason network alice/api-claude "…"` asks and waits for the answer.

## How it works

Each computer runs a small daemon. Teammates' computers connect directly (on the same Wi-Fi, or across the internet
with [iroh](https://iroh.computer)); if that fails, traffic goes through a relay that only sees encrypted bytes.
That's Reason's, with computers finding each other through the address lookup of n0 (the company behind iroh), unless
your team runs its own relay (`reason-network relay <url>`): then your computers contact nothing else, n0 included.
Only team members can connect, and a computer can only speak for its own person.
Messages to someone offline wait and go out when they're back.
An agent session's name stays with its conversation, for a session the agent identifies: resumed after a restart, it
gets the same name back with the messages that waited for it, and for a day no new session takes the name.

Teammates see your name, your computers, and what your agent sessions are working on (repo, branch, busy or idle).
Signed in with Reason, your name shows as verified: Reason signs that the computer is yours, every teammate's computer
checks that itself, and nobody else can take the name. State lives in `~/.reason/network`. Details: [DESIGN.md](DESIGN.md).

Direct connections use a random UDP port each time the daemon starts. To open or forward one port in a firewall or
router instead, set it in `~/.reason/network/config.toml` (per computer; the daemon reads it when it starts):

```toml
port = 41641
```

Put `port` at the top of the file, above any `[table]` section such as `[inbound]`. The daemon ignores it under a table.

It is bound on IPv4 and IPv6 (IPv4 alone on a computer without IPv6). If another program holds it on either, the
daemon logs that and uses a random port, as without it; `port = 0` is the same as leaving it out.
`reason-network doctor` shows the port in use. Relays work the same either way.

To work out why connections drop or switch to a relay, `debug_log = true` (also at the top) makes the daemon write the
networking library's events to `~/.reason/network/debug.log`, on this computer only and without message text, links
or keys. It's off by default; see [DESIGN.md](DESIGN.md#troubleshooting).

## Uninstall

```sh
reason-network leave && reason-network setup --remove && rm -rf ~/.reason/network ~/.local/bin/reason-network
rm -f ~/.local/bin/tincan   # if you came from tincan
```

On Windows, in PowerShell: `reason-network leave; reason-network setup --remove; Remove-Item -Recurse -Force ~\.reason\network, ~\.local\bin\reason-network*`

With the Reason CLI: `reason network leave`, then `reason network setup --remove`.

## Build

```sh
cargo fmt && cargo clippy --all-targets -- -D warnings && cargo test
cargo build && python3 tests/e2e/two_machines.py   # two machines over iroh (needs internet)
python3 tests/e2e/hooks.py && python3 tests/e2e/codex_busy.py && python3 tests/e2e/guided.py
```

A `v*` tag builds the macOS, Linux and Windows archives and the install scripts; the Reason CLI pins the macOS and
Linux ones.

## Contributing

Issues and pull requests are welcome: see [CONTRIBUTING.md](CONTRIBUTING.md).

## License

MIT OR Apache-2.0 ([LICENSE-MIT](LICENSE-MIT), [LICENSE-APACHE](LICENSE-APACHE)); see [NOTICE](NOTICE). Originally tincan, by Lars Talian, who works
at Reason.

## Star History

<a href="https://www.star-history.com/?repos=reason-machines%2Freason-network&type=date&legend=top-left">
 <picture>
   <source media="(prefers-color-scheme: dark)" srcset="https://api.star-history.com/chart?repos=reason-machines/reason-network&type=date&theme=dark&legend=top-left" />
   <source media="(prefers-color-scheme: light)" srcset="https://api.star-history.com/chart?repos=reason-machines/reason-network&type=date&legend=top-left" />
   <img alt="Star History Chart" src="https://api.star-history.com/chart?repos=reason-machines/reason-network&type=date&legend=top-left" />
 </picture>
</a>

## Usage measurement

The engine keeps optional aggregate usage counters in a separate local SQLite file.
Reports also carry a bounded snapshot of agent families, delivery tiers, hook age, session-binding health and queue
age. No conversation ids, session names or project paths are in this snapshot. `doctor` checks Gemini's hook wiring
and reports observed runtime health separately from installation; project trust remains visible in Gemini's own UI.
When the Reason CLI carries the network, it reports recent daily aggregates about every
five minutes, in bounded background batches: agent families, local or remote routing, verified account relationships,
transport paths, delivery stages, linked replies, size and latency buckets, session
snapshots and optional sender-supplied purpose labels (the CLI send --purpose option). The engine does not upload these
itself. Standalone engines without the CLI keep their counters locally.

Message text, files, filenames, repositories, session names, message ids, peer identities,
keys and sign-in proofs are not in these reports. The existing P2P encryption and wire
protocol are unchanged. Set DO_NOT_TRACK=1 when running a Reason command to keep the
aggregates local, including in its background relay; DO_NOT_TRACK=0 restores reporting.
Reports are associated with the reporting Reason account and engine, so they are usage
metadata, not anonymous data. Unknown classifications stay unknown.

Collection uses a bounded, non-blocking queue and its own database worker. Analytics
failures do not refuse or delay message delivery. Counts are best effort: queue overflow,
bounded dimension capacity, missing reports, old engines, and a crash before persistence
can leave gaps. Persisted cumulative snapshots survive restarts and retries; a replaced
counter database gets a new generation. Collection-limit drops are reported. Acceptance
is not agent consumption, and a reply is not proof of task completion. A hosted relay
copying mail into a server inbox does not count as a handoff to that assistant.
