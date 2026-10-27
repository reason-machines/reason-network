//! The local API between the daemon and its clients (`reason network mcp`, the CLI, hooks): one JSON object per line
//! over `~/.Reason Network/Reason Network.sock`. Requests carry an `id` and get a [`Reply`] with the same id; a connection that
//! registered sessions also receives [`Event`]s for them.

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// How quickly a session sees a message (ordered fastest first).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Tier {
    /// Injected into the conversation right away; wakes the session if it is idle.
    Push,
    /// Injected at the next tool call or when the turn ends; an idle session waits for its human.
    Turn,
    /// Seen only when the agent checks its inbox.
    Pull,
    /// Not on Reason Network: cannot receive.
    None,
}

impl Tier {
    pub fn as_str(self) -> &'static str {
        match self {
            Tier::Push => "push",
            Tier::Turn => "turn",
            Tier::Pull => "pull",
            Tier::None => "none",
        }
    }

    pub fn parse(s: &str) -> Tier {
        match s {
            "push" => Tier::Push,
            "turn" => Tier::Turn,
            "pull" => Tier::Pull,
            _ => Tier::None,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionReg {
    /// Stable id for this agent session: `<agent>:<agent's session id>`, or `proc:<pid>` when the agent gives none.
    pub key: String,
    pub agent: String,
    pub cwd: String,
    #[serde(default)]
    pub agent_session_id: Option<String>,
    /// The agent's process, used to match Claude Code's own session registry.
    #[serde(default)]
    pub pid: Option<u32>,
    pub tier: Tier,
    #[serde(default)]
    pub name: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HookCall {
    pub agent: String,
    /// `start`, `prompt`, `tool`, `stop`, `end`, or `doing` (only reports what the session is doing).
    pub event: String,
    pub session_id: String,
    #[serde(default)]
    pub cwd: Option<String>,
    #[serde(default)]
    pub pid: Option<u32>,
    /// False when this event can't show the agent anything (so mail stays for a later event).
    #[serde(default = "yes")]
    pub mail: bool,
    /// The agent's transcript, which says what the session is doing.
    #[serde(default)]
    pub transcript: Option<String>,
    /// What the session is doing, from agents that say so themselves (the opencode plugin).
    #[serde(default)]
    pub doing: Option<String>,
    /// One of the person's own mailboxes, kept for good: a name they gave with `reason network listen --as` (the
    /// terminal always is one). Other `cli` sessions, such as the Reason CLI's one-off ask sessions, are sessions like
    /// any other. Older clients leave it out.
    #[serde(default)]
    pub keep: bool,
    /// The session's name, for sessions that pick one (`reason network listen --as`).
    #[serde(default)]
    pub name: Option<String>,
    /// Every folder the agent says the session is in (besides `cwd`): one turned off keeps it off.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub dirs: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum Request {
    Register {
        session: SessionReg,
    },
    SetStatus {
        key: String,
        #[serde(default)]
        doing: Option<String>,
        #[serde(default)]
        name: Option<String>,
    },
    Who {
        #[serde(default)]
        me: Option<String>,
        #[serde(default)]
        filter: Option<String>,
        #[serde(default)]
        all: bool,
    },
    /// From an agent session: its key (or, for agents that reply through the CLI, its address or name).
    Send {
        from: String,
        to: String,
        body: String,
        #[serde(default)]
        reply_to: Option<String>,
        #[serde(default = "yes")]
        wake: bool,
        /// Also pop it up on the person's screen. With it, `to` may be your own user.
        #[serde(default)]
        notify: bool,
        /// Small text files carried in the message (the client reads them from the sender's paths).
        #[serde(default)]
        files: Vec<Attachment>,
    },
    /// A session's unread messages. With `peek` nothing is marked read: the client acknowledges with `Delivered`
    /// after showing the messages, so nothing is lost if it goes away first.
    Inbox {
        key: String,
        #[serde(default)]
        wait_secs: u64,
        #[serde(default)]
        peek: bool,
    },
    /// A session's unread messages, counted.
    Unread {
        key: String,
    },
    /// Asks this machine's person with a pop-up, for the session `key`, and waits (up to 10 minutes) for the answer.
    Ask {
        key: String,
        question: String,
        /// A teammate's message the question is about: shown with it, and sent the answer, signed.
        #[serde(default)]
        about: Option<String>,
    },
    /// The client gave up on this session's `Inbox` or `Ask` wait (the agent cancelled it): end it, and push what it
    /// was waiting for the usual way.
    StopWaiting {
        key: String,
    },
    /// The client injected these messages into its agent's conversation.
    Delivered {
        key: String,
        ids: Vec<String>,
    },
    /// The client couldn't push these messages into its agent's conversation (it knows no inbox for it): they stay
    /// unread.
    PushFailed {
        key: String,
        ids: Vec<String>,
    },
    /// A conversation on a connection that carries others ended: it leaves now, not when the connection closes.
    EndSession {
        key: String,
    },
    /// An agent hook fired (`reason network hook`): keeps hook-only sessions alive and returns mail to inject.
    Hook(HookCall),
    /// For agents whose MCP calls carry no session id (Cursor, opencode): the hook-known session of this agent
    /// that is most likely making the call.
    Attach {
        agent: String,
        cwd: String,
    },
    /// For agents that give their MCP servers no session id, only their hooks (Gemini): the session id the hooks of
    /// agent process `pid` registered, if any.
    Linked {
        agent: String,
        pid: u32,
    },
    CreateTeam {
        name: String,
        person: String,
        #[serde(default)]
        relay: Option<String>,
        /// A Reason sign-in proof for this machine: then the name is the proof's.
        #[serde(default)]
        proof: Option<String>,
    },
    Join {
        ticket: String,
        person: String,
        #[serde(default)]
        proof: Option<String>,
    },
    /// Attach or refresh this machine's Reason sign-in proof.
    Proof {
        jws: String,
    },
    Invite {
        #[serde(default)]
        me: bool,
        #[serde(default = "day")]
        hours: u64,
    },
    Team,
    /// The team link, which anyone can use to join; `new` replaces it.
    Link {
        #[serde(default)]
        new: bool,
    },
    /// Remove a person (all their machines) or one machine from the team.
    Remove {
        who: String,
    },
    Leave {
        /// Leave even if no teammate is online to hear it (they'll need to `reason network remove` this machine).
        #[serde(default)]
        force: bool,
    },
    SetRelay {
        #[serde(default)]
        url: Option<String>,
    },
    Held,
    /// Approve (`release`) or drop held messages; no ids means all of them.
    Settle {
        #[serde(default)]
        ids: Vec<String>,
        release: bool,
    },
    Shutdown,
}

fn day() -> u64 {
    24
}

fn yes() -> bool {
    true
}

/// `*`, `person/*` or `repo:<name>`, on any machine (`…@machine`): for a person at a terminal, never an agent.
pub fn is_group(to: &str) -> bool {
    let to = to.rsplit_once('@').map_or(to, |(s, _)| s);
    to == "*" || to.starts_with("repo:") || to.ends_with("/*")
}

#[derive(Debug, Serialize, Deserialize)]
pub struct Frame<T> {
    pub id: u64,
    #[serde(flatten)]
    pub body: T,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct Reply {
    pub id: u64,
    pub ok: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub result: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    /// For a failure the Reason CLI handles: `name_taken` or `same_account_needs_owner` (see [`error_code`]).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub code: Option<String>,
}

impl Reply {
    pub fn ok(id: u64, result: Value) -> Self {
        Reply { id, ok: true, result: Some(result), error: None, code: None }
    }

    pub fn err(id: u64, e: anyhow::Error) -> Self {
        let error = format!("{e:#}");
        Reply { id, ok: false, result: None, code: error_code(&error).map(String::from), error: Some(error) }
    }
}

/// The failures a client acts on, by what the message says (as every build says it, a joiner hears an inviting machine's
/// own words): `same_account_needs_owner` when one of the person's own computers should let this one in, `name_taken`
/// when the name is someone else's in the team.
pub fn error_code(error: &str) -> Option<&'static str> {
    if error.contains(crate::team::SAME_ACCOUNT) {
        Some(crate::team::SAME_ACCOUNT)
    } else if error.contains("is already in this team") {
        Some("name_taken")
    } else {
        None
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "event", rename_all = "snake_case")]
pub enum Event {
    /// Inject `text` into the conversation of session `key`, then acknowledge with [`Request::Delivered`].
    Deliver { key: String, ids: Vec<String>, text: String },
}

/// A person's answer in an `ask_user` pop-up about a teammate's request, signed by the machine they gave it on: it
/// names the request, so it can't be passed off as the answer to another.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Approval {
    /// The request (a message id) and the session that sent it.
    pub about: String,
    pub asker: String,
    pub approver: String,
    pub approved: bool,
    /// What the person saw: the agent's question and the request.
    pub question: String,
    pub request: String,
    pub note: String,
    pub at: i64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Message {
    pub id: String,
    /// The agent session it came from (`person/session`).
    pub from: String,
    pub from_agent: Option<String>,
    pub from_repo: Option<String>,
    /// The resolved recipient address.
    pub to: String,
    /// The address as the sender typed it, when it was a group (`*`, `repo:api`, `alex/*`).
    pub via: Option<String>,
    pub reply_to: Option<String>,
    pub body: String,
    pub wake: bool,
    /// Pop it up on the person's screen too (and, sent to your own user rather than a session, only that).
    #[serde(default)]
    pub notify: bool,
    pub sent_at: i64,
    /// A person's signed answer to the request this replies to (`ask_user` with `about`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub approval: Option<Box<crate::team::Signed<Approval>>>,
    /// Small text files that came with it. An engine from before files drops this field, so a sender only puts it on
    /// a message for a machine whose presence says it takes files.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub files: Vec<Attachment>,
}

/// A small text file carried in a message.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Attachment {
    pub name: String,
    /// The text: on the wire, in the outbox and while held. Empty once it is saved in the recipient's inbox folder.
    #[serde(default)]
    pub content: String,
    /// Where it is saved on this computer: only in what the daemon tells its own clients, never taken from a peer.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Registered {
    pub name: String,
    pub address: String,
    pub unread: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Directory {
    pub team: Option<String>,
    pub person: String,
    pub machine: String,
    pub machines: Vec<MachineInfo>,
    pub sessions: Vec<SessionInfo>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MachineInfo {
    pub person: String,
    pub machine: String,
    pub endpoint: String,
    pub is_me: bool,
    pub online: bool,
    /// `direct` or `relay` while connected.
    pub link: Option<String>,
    pub rtt_ms: Option<u64>,
    pub last_seen: Option<i64>,
    /// The person is verified (it's per person, the same on each of their machines): a machine of theirs carries a
    /// Reason sign-in proof for their name, good now.
    #[serde(default)]
    pub verified: bool,
    /// This machine carries such a proof itself.
    #[serde(default)]
    pub has_proof: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TeamInfo {
    /// The team's id, 32 hex digits (the Reason CLI and connector route by it).
    #[serde(default, rename = "team_id")]
    pub id: String,
    pub name: String,
    /// `name` again, under the name `team --json` gives it.
    #[serde(default)]
    pub team_name: String,
    pub relay: Option<String>,
    /// Whether this machine is on its relay right now (`None` while the network isn't up).
    #[serde(default)]
    pub relay_connected: Option<bool>,
    pub person: String,
    pub machine: String,
    pub machines: Vec<MachineInfo>,
    /// What this engine takes beyond what every version does (`files`), for local clients that pass such things on
    /// for others. Older engines leave it out.
    #[serde(default)]
    pub features: Vec<String>,
    /// Per-day totals of messages sent and received on this machine, returned to local clients on the socket: sent
    /// once the receiving machine took them, received once in a mailbox here (a held one once approved). For the days
    /// (UTC) from six before today to tomorrow that had any. Older engines leave it out.
    #[serde(default)]
    pub messages: Vec<DayCount>,
    /// The UDP port the network is on (`None` while it isn't up). Older engines leave it out.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub udp: Option<UdpPort>,
}

/// The UDP port direct paths come in on, and the one `port` in the config asked for, if any.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct UdpPort {
    pub port: u16,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fixed: Option<u16>,
    /// `fixed` couldn't be bound, so `port` is a random one.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub fell_back: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DayCount {
    /// `YYYY-MM-DD`, UTC.
    pub day: String,
    pub sent: u64,
    pub received: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LinkInfo {
    pub team: String,
    /// A page with the one command to run (the ticket rides in the `#fragment`, which never reaches the server).
    pub url: String,
    pub ticket: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InviteInfo {
    /// Four words, like `plum-tiger-lake-orbit`. For a team with its own relay, the long form (a code would need n0's
    /// address lookup to find this machine).
    pub code: String,
    /// The same invite as a long `rn1-…` string, for when the inviting machine will be offline.
    pub ticket: String,
    pub hours: u64,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct HookReply {
    pub address: Option<String>,
    /// Messages to put in the agent's context, framed.
    pub text: Option<String>,
    /// Their ids, to acknowledge with `Delivered` once shown.
    #[serde(default)]
    pub ids: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SessionInfo {
    pub address: String,
    pub name: String,
    pub person: String,
    pub machine: String,
    pub agent: String,
    pub cwd: String,
    pub repo: Option<String>,
    pub branch: Option<String>,
    /// `busy`, `idle`, `live` (running, activity unknown) or `offline`.
    pub status: String,
    /// When `status` last changed (for `offline`: last seen), ms since epoch.
    pub since: Option<i64>,
    pub doing: Option<String>,
    pub tier: Tier,
    pub unread: u32,
    pub is_me: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SendResult {
    pub ids: Vec<String>,
    pub recipients: Vec<Recipient>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Recipient {
    pub address: String,
    /// `pushed`, `queued` or `inbox`.
    pub state: String,
    pub note: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Inbox {
    pub messages: Vec<Message>,
    /// The messages framed for an agent's context.
    pub text: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Unread {
    pub count: u32,
    pub from: Vec<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_team_reply_names_the_team_by_id_and_name() {
        let t = TeamInfo {
            id: "00112233445566778899aabbccddeeff".into(),
            name: "vibes".into(),
            team_name: "vibes".into(),
            relay: None,
            relay_connected: None,
            person: "alex".into(),
            machine: "mbp".into(),
            machines: vec![],
            features: vec![],
            messages: vec![DayCount { day: "2026-10-20".into(), sent: 3, received: 1 }],
            udp: Some(UdpPort { port: 41641, fixed: Some(41641), fell_back: false }),
        };
        let v = serde_json::to_value(&t).unwrap();
        assert_eq!(v["team_id"], "00112233445566778899aabbccddeeff");
        assert_eq!((v["team_name"].as_str(), v["name"].as_str()), (Some("vibes"), Some("vibes")));
        assert_eq!(v["messages"], serde_json::json!([{"day": "2026-10-20", "sent": 3, "received": 1}]));
        // A reply from a daemon before these fields still reads.
        let old: TeamInfo =
            serde_json::from_value(serde_json::json!({"name": "vibes", "relay": null, "person": "alex",
            "machine": "mbp", "machines": []}))
            .unwrap();
        assert_eq!((old.id.as_str(), old.name.as_str()), ("", "vibes"));
        assert!(old.messages.is_empty());
        assert_eq!(v["udp"], serde_json::json!({"port": 41641, "fixed": 41641}));
        assert!(old.udp.is_none());
    }

    #[test]
    fn error_replies_carry_a_code_the_reason_cli_acts_on() {
        let reply = |m: &str| serde_json::to_value(Reply::err(7, anyhow::anyhow!(m.to_string()))).unwrap();
        assert_eq!(
            reply("'alex' is already in this team; join with another name (--as)"),
            serde_json::json!({"id": 7, "ok": false, "error": "'alex' is already in this team; join with another name (--as)",
                               "code": "name_taken"})
        );
        let same = "only one of alex's own computers lets this one in as alex (same_account_needs_owner)";
        assert_eq!(reply(same)["code"], "same_account_needs_owner");
        // Anything else, and every success, has none.
        assert_eq!(reply("this sign-in proof expired; sign in again").get("code"), None);
        assert_eq!(serde_json::to_value(Reply::ok(7, serde_json::json!(1))).unwrap().get("code"), None);
    }
}
