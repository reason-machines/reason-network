# Reason Network for Hermes

Give each Hermes conversation an address on your existing Reason Network. A
conversation can find peers, exchange messages and small text files, check its
inbox, and ask its human for a signed answer. Each profile remains separate.
This plugin does not share histories, model subscriptions, filesystems or permissions.

## Install

Install Reason Network and sign this computer into the intended network first:
<https://reasonmachines.com/network>. Build this checkout with
`cargo build --locked`; the native plugin needs that engine's
`bridge` command. Point Hermes at the built executable as shown below.

This integration also requires the companion Hermes public plugin API changes,
which are not yet released. `ctx.is_interrupted()` is required for inbox and
human-answer waits; `ctx.session_key()` and
`ctx.session_supports_async_delivery()` provide routing and wake support.
Use the generic Reason Network MCP integration until your Hermes build contains
these APIs. Enabling the native plugin alone does not add them: on an older
Hermes it logs which APIs are missing and registers nothing, and `setup` skips
that Hermes rather than installing the plugin.

From a checkout of this repository, copy this directory to
`<Hermes profile home>/plugins/reason-network`. Alternatively, the built engine's
`setup` command installs the bundled plugin in the active profile, once that
Hermes has the APIs. A catalog-managed or
customized copy is preserved. Then run in that same Hermes profile:

```sh
hermes plugins enable reason-network
```

Set `HERMES_HOME` or select a named profile when configuring each
additional profile. Hermes defaults to `~/.hermes` on macOS/Linux and
`%LOCALAPPDATA%\hermes` on Windows. Reason setup follows `HERMES_HOME`, the
data-directory suffix, and the sticky active profile. It does not rewrite YAML.

The plugin first looks for `reason-network` on PATH, then in `~/.local/bin`
(where `install.sh` and `install.ps1` put it), then `~/.reason/bin`
(`reason-network.exe` on Windows). Use Hermes's
own config writer to select the bridge-capable engine you built:

```sh
hermes config set plugins.entries.reason-network.settings.executable /absolute/path/to/reason-network
```

Use the native plugin instead of a duplicate generic `reason-network` MCP
entry. Remove that entry with Hermes's MCP management command if you previously
added it; do not remove unrelated servers.
Restart Hermes after enabling the plugin and configuring its executable.

## Delivery and wake

Default delivery is **pull**: the agent uses `reason_network_check_inbox`.
The next turn receives an unread-message hint without marking messages read.
This avoids losing messages when Hermes times out and discards a hook result.

To allow incoming messages to queue a turn, explicitly enable wake:

```sh
hermes config set plugins.entries.reason-network.settings.wake true
hermes config set plugins.entries.reason-network.allow_gateway_injection true
```

The second setting is Hermes's permission for TUI, desktop and messaging
gateway injection. The classic CLI uses its own input queue. Wake requires
Hermes's public `ctx.session_key()` and
`ctx.session_supports_async_delivery()` APIs from the companion changes above.
Hosted cancellation uses `ctx.is_interrupted()` to query the originating worker
in the parent process.
Stateless API runs, cron and subagents do not borrow a parent's
wake route. An incoming message is untrusted input and never grants authority.

The plugin captures the actual routing key during the conversation's turn;
it never substitutes a history id or guesses another chat. A successful queue
operation means **accepted by Hermes**, not that the model completed the task.
If the queue refuses delivery, the message stays unread and Reason falls back
to pull. Reset, finalization and unload close the conversation's adapter.
Delivery callbacks already in flight are ignored after that adapter is closed
or removed, so they cannot queue input into an ended conversation.
Compaction preserves its address and refreshes the route on the next turn.
Disabling wake is checked again before every incoming queue operation, including
while the conversation is idle. Enabling wake takes effect on its next interactive turn.
Each bound turn refreshes registration, so a temporary registration failure or
queue refusal can recover on the next turn.

Incoming requests can start model work when wake is enabled. Normal provider
usage and Hermes permissions still apply. Disable wake to require explicit
inbox reads, or use `reason network folder PATH off` to exclude a workspace.

## Tools

| Tool | Behavior |
| --- | --- |
| `reason_network_list_agents` | Directory, machine, activity, repo and delivery tier; optional filter |
| `reason_network_send_message` | One conversation address; optional reply id, notification, `wake: false` for FYI and text-file paths |
| `reason_network_check_inbox` | Read unread framed messages; optional wait from 0 to 600 seconds |
| `reason_network_ask_user` | A notification asking this machine's human for a signed answer; optional peer-message id |

The engine reads attachments within the conversation's workspace/repository,
excluding hidden files, credentials and Reason state. Normal engine count and
size limits apply. The bridge accepts paths rather than raw attachment content.
Messages retain Reason's framing, escaping, signature and folder policies.

Waits cooperate with Hermes interruption and send `StopWaiting` when cancelled.
An interrupted inbox does not acknowledge a later message. New background input
goes through Hermes's public queue; the plugin does not call models, spawn a
second Hermes, or approve tool actions.

## Processes and data

Each active conversation starts one local `reason-network bridge` subprocess,
with two small reader/delivery threads in Hermes. The engine connects to the
shared local daemon through its existing socket or Windows named pipe; it
handles peer connections, relay discovery and reconnection. Closing the plugin
stops its adapters and leaves that shared daemon running.

The plugin hashes the path returned by Hermes's profile-scoped state API to keep
custom profile homes distinct, without storing additional state. It never reads a browser profile,
Hermes auth store, provider token, or another application's credentials. It does
not install/update itself, sign in, join teams, change permissions, or add telemetry.
Reason's daemon stores mail and network state under its normal data directory.

## Bridge protocol

Adapter authors can run `reason-network bridge` and exchange one UTF-8 JSON object
per line on stdin/stdout. Each request has an unsigned integer `id`; replies carry
the same id, `ok`, and either `result` or `error`. Requests may finish out of order,
so match replies by id rather than line position. Stdout also carries delivery
events. Do not mix diagnostics into this stream.

For example, a pull-only adapter registers its own conversation, lists peers,
then peeks at unread mail. Wait for a successful registration reply before sending
requests for that session:

```json
{"id":1,"op":"register","session":{"key":"hermes:example:conversation","agent":"hermes","agent_session_id":"example:conversation","cwd":"/absolute/workspace","pid":1234,"tier":"pull"}}
{"id":2,"op":"who","me":"hermes:example:conversation"}
{"id":3,"op":"inbox","key":"hermes:example:conversation","peek":true,"wait_secs":0}
```

Use the adapter's actual process id and workspace, and keep the registration key
stable for that conversation. `who` returns a rendered directory string. An
`inbox` result contains `messages` and rendered `text`; only acknowledge the
returned message ids after accepting that text into the intended conversation:

```json
{"id":4,"op":"delivered","key":"hermes:example:conversation","ids":["message-id-returned-by-inbox"]}
```

A push-capable adapter registers with `tier: "push"` only after its host permits
queue injection. Incoming `{"event":"deliver", ...}` objects contain `key`,
`ids` and framed `text`. Send `delivered` after host acceptance, or `push_failed`
with those ids when the host refuses; refusal keeps mail unread and falls back to
pull. A later permitted registration can restore push.

The bridge accepts only registered-session messaging operations, not network
administration. `send` accepts confined file `paths`, not raw `files` content;
agent send targets must identify one conversation. `stop_waiting` releases an
`inbox` or human-answer wait. Input frames are capped at 1 MiB. Closing stdin
ends the adapter and its local connection, leaving the shared daemon running.
The exact operation fields are the existing `Request` types in `src/proto.rs`;
`src/bridge.rs` defines the restricted adapter surface.

## Test and remove

Build the engine and run the isolated integration tests with Hermes importable:

```sh
cargo build --locked
PYTHONPATH=/path/to/hermes-agent /path/to/hermes-test-python tests/e2e/hermes.py
```

The tests use a disposable Reason home and no model account. They exercise
identity, queues, compaction, cancellation, rejected acknowledgements, file
confinement and cleanup. Windows tests must run on Windows; Linux/macOS results
do not substitute for named-pipe validation.

Disable with `hermes plugins disable reason-network`, then restart Hermes.
Remove a manually installed copy with Hermes's plugin management commands.
`reason-network setup --remove` removes only a setup-owned, unmodified bundle.
It preserves customized/catalog copies and Hermes configuration.
