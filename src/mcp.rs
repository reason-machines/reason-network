//! `reason-network mcp`: the stdio MCP server an agent spawns. A thin client of the daemon.

use crate::{
    agent::{self, ClaudeSocket},
    client::Link,
    config::OFF_HERE,
    proto::*,
    render,
};
use anyhow::Result;
use rmcp::{
    ErrorData as McpError, Peer, RoleServer, ServerHandler, ServiceExt,
    handler::server::{router::tool::ToolRouter, wrapper::Parameters},
    model::{
        CallToolResult, ContentBlock, Implementation, ListToolsResult, PaginatedRequestParams, ServerCapabilities,
        ServerConfig,
    },
    service::RequestContext,
    tool, tool_handler, tool_router,
    transport::stdio,
};
use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::Value;
use std::{
    sync::{Arc, Mutex},
    time::Duration,
};
use tokio::sync::mpsc;

/// How agents should use Reason Network, kept once: in the skill, for agents with the tools and without.
const SKILL: &str = include_str!("skill.md");

fn instructions() -> String {
    // Windows checkouts may have turned the file's line ends into \r\n.
    let rules = SKILL.split_once("## How to behave").map_or(String::new(), |(_, r)| r.trim().replace('\r', ""));
    format!("Reason Network connects you with your teammates' coding agents.\n\n{rules}")
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct ListArgs {
    /// Only sessions matching this text (person, session, agent, repo, branch, directory or what it's doing).
    #[serde(default)]
    pub filter: Option<String>,
    /// Also list sessions that went offline in the last day.
    #[serde(default)]
    pub all: Option<bool>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct SendArgs {
    /// "person/session" (or just "session").
    pub to: String,
    /// The message, with the point in the first line.
    pub message: String,
    /// Id of the message you are replying to.
    #[serde(default)]
    pub reply_to: Option<String>,
    /// Also pop it up on that person's screen: a heads-up, for when they must act themselves. `to` may then be your
    /// own user's name, to reach them.
    #[serde(default)]
    pub notify: Option<bool>,
    /// Paths of text files in your working folder or repository to send with it (markdown, code, JSON, logs, config; no
    /// binaries, no hidden files): at most 10, 256 KB each, 512 KB in all. The recipient gets copies.
    #[serde(default)]
    pub files: Option<Vec<String>>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct AskArgs {
    /// What they need to decide: who asks for what, and why.
    pub question: String,
    /// When it's about a teammate's message, its id (the `id="…"` of their `<reason-message>`): your user sees that
    /// request in the pop-up, and its sender gets the answer signed by this machine.
    #[serde(default)]
    pub about: Option<String>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct InboxArgs {
    /// If nothing is waiting, wait up to this many seconds for a message (max 600). Default 0.
    #[serde(default)]
    pub wait_secs: Option<u64>,
}

#[derive(Clone)]
pub struct ReasonNetworkMcp {
    link: Arc<Link>,
    env_agent: Option<&'static str>,
    claude: Option<Arc<ClaudeSocket>>,
    /// The agent named by the client's `clientInfo`, once known.
    client_agent: Arc<Mutex<Option<&'static str>>>,
    tool_router: ToolRouter<Self>,
}

pub async fn run() -> Result<()> {
    let (tx, mut events) = mpsc::unbounded_channel();
    let server = ReasonNetworkMcp {
        link: Link::new(Some(tx)),
        env_agent: agent::detect_env(),
        claude: agent::claude_socket().map(Arc::new),
        client_agent: Default::default(),
        tool_router: ReasonNetworkMcp::tool_router(),
    };
    let service = server.clone().serve(stdio()).await?;

    let s = server.clone();
    tokio::spawn(async move {
        while let Some(ev) = events.recv().await {
            s.on_event(ev).await;
        }
    });

    // Agents with one MCP process per session get registered up front, so they can receive before their
    // first tool call. The pause skips health checks (`claude mcp list`) that connect and leave at once.
    let (s, peer) = (server.clone(), service.peer().clone());
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_secs(3)).await;
        let agent = s.agent(&peer, None);
        if !agent::multiplexed(agent)
            && !off_here()
            && let Err(e) = s.session(agent, None).await
        {
            eprintln!("reason-network: registering session: {e:#}");
        }
    });

    service.waiting().await?;
    Ok(())
}

#[tool_router]
impl ReasonNetworkMcp {
    #[tool(
        description = "List your team's live agent sessions: who, which machine and agent, repo@branch, busy/idle, \
                       and what each is doing. Use it to find the right recipient.",
        annotations(read_only_hint = true, open_world_hint = false)
    )]
    async fn list_team(
        &self,
        Parameters(a): Parameters<ListArgs>,
        ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, McpError> {
        let key = match self.session_for(&ctx).await {
            Ok(k) => k,
            Err(e) => return Ok(failure(e)),
        };
        let req = Request::Who { me: Some(key.clone()), filter: a.filter, all: a.all.unwrap_or(false) };
        let res = self.link.call::<Directory>(req).await.map(|d| render::directory_for_agent(&d));
        Ok(self.finish(&key, res, true).await)
    }

    #[tool(description = "Send a message to one teammate's agent session, optionally with small text files.")]
    async fn send_message(
        &self,
        Parameters(a): Parameters<SendArgs>,
        ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, McpError> {
        let key = match self.session_for(&ctx).await {
            Ok(k) => k,
            Err(e) => return Ok(failure(e)),
        };
        let notify = a.notify.unwrap_or(false);
        let files = match agent_files(&a.files.unwrap_or_default()) {
            Ok(f) => f,
            Err(e) => return Ok(failure(e)),
        };
        let req = Request::Send {
            from: key.clone(),
            to: a.to,
            body: a.message,
            reply_to: a.reply_to,
            wake: true,
            notify,
            files,
        };
        // Notes are teammates' machines' words.
        let res = self.link.call::<SendResult>(req).await.map(|r| render::defang(&render::send_result(&r)));
        Ok(self.finish(&key, res, true).await)
    }

    #[tool(description = "Ask your own user, with a pop-up on their screen, and wait for their answer: approve or \
                          deny, maybe with a note. For when a teammate's agent asks you for something only your user \
                          can OK and they may not be watching. The answer is theirs, given in the pop-up.")]
    async fn ask_user(
        &self,
        Parameters(a): Parameters<AskArgs>,
        ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, McpError> {
        let key = match self.session_for(&ctx).await {
            Ok(k) => k,
            Err(e) => return Ok(failure(e)),
        };
        let req = Request::Ask { key: key.clone(), question: a.question, about: a.about };
        let got = tokio::select! {
            r = self.link.call::<String>(req) => r,
            // The agent gave up on this call: the pop-up stays, and its answer comes as a message.
            _ = ctx.ct.cancelled() => {
                let _ = self.link.call::<Value>(Request::StopWaiting { key }).await;
                return Ok(failure(anyhow::anyhow!("cancelled")));
            }
        };
        // It can quote what the teammate's machine said when the answer was sent.
        Ok(self.finish(&key, got.map(|t| render::defang(&t)), true).await)
    }

    #[tool(
        description = "Read messages sent to this session. Optionally wait for one to arrive. Text files sent with a \
                       message are saved on this computer, and it lists their paths.",
        annotations(read_only_hint = true, open_world_hint = false)
    )]
    async fn check_inbox(
        &self,
        Parameters(a): Parameters<InboxArgs>,
        ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, McpError> {
        let key = match self.session_for(&ctx).await {
            Ok(k) => k,
            Err(e) => return Ok(failure(e)),
        };
        let req = Request::Inbox { key: key.clone(), wait_secs: a.wait_secs.unwrap_or(0), peek: true };
        let got = tokio::select! {
            r = self.link.call::<Inbox>(req) => r,
            _ = ctx.ct.cancelled() => Ok(Inbox { messages: Vec::new(), text: String::new() }),
        };
        // The agent gave up on this call: nothing it would have read counts as read, the daemon ends the wait,
        // and what arrived meanwhile is pushed the usual way.
        if ctx.ct.is_cancelled() {
            let _ = self.link.call::<Value>(Request::StopWaiting { key: key.clone() }).await;
            return Ok(failure(anyhow::anyhow!("cancelled")));
        }
        let res = match got {
            Ok(i) if i.messages.is_empty() => Ok("No messages.".to_string()),
            // Marked read only now, right before the agent gets them.
            Ok(i) => {
                let ids = i.messages.iter().map(|m| m.id.clone()).collect();
                self.link.call::<Value>(Request::Delivered { key: key.clone(), ids }).await.map(|_| i.text)
            }
            Err(e) => Err(e),
        };
        Ok(self.finish(&key, res, false).await)
    }
}

#[tool_handler(router = self.tool_router)]
impl ServerHandler for ReasonNetworkMcp {
    fn get_info(&self) -> ServerConfig {
        ServerConfig::new(ServerCapabilities::builder().enable_tools().build())
            .with_server_info(Implementation::new(crate::AGENT_NAME, env!("CARGO_PKG_VERSION")))
            .with_instructions(instructions())
    }

    /// A session that gets its messages pushed never needs check_inbox, so it isn't shown one: fewer tokens.
    async fn list_tools(
        &self,
        _request: Option<PaginatedRequestParams>,
        ctx: RequestContext<RoleServer>,
    ) -> Result<ListToolsResult, McpError> {
        let mut tools = self.tool_router.list_all();
        if self.pushed(self.agent(&ctx.peer, None)) {
            tools.retain(|t| t.name != "check_inbox");
        }
        Ok(ListToolsResult::with_all_items(tools))
    }
}

impl ReasonNetworkMcp {
    /// Whether messages reach this session by being pushed into it: Claude Code, through its inbox.
    fn pushed(&self, agent: &str) -> bool {
        agent == agent::CLAUDE_CODE && self.claude.is_some()
    }

    /// The agent we serve: the client's `clientInfo` (handshake, or the call's `_meta` on handshake-less
    /// protocol versions), else the environment.
    fn agent(&self, peer: &Peer<RoleServer>, meta: Option<&serde_json::Map<String, Value>>) -> &'static str {
        if let Some(a) = *self.client_agent.lock().unwrap() {
            return a;
        }
        let name = peer
            .peer_info()
            .map(|i| i.client_info.name.clone())
            .or_else(|| meta?.get("io.modelcontextprotocol/clientInfo")?.get("name")?.as_str().map(str::to_string));
        match name.as_deref().and_then(agent::from_client_name) {
            Some(a) => {
                *self.client_agent.lock().unwrap() = Some(a);
                a
            }
            None => self.env_agent.unwrap_or("agent"),
        }
    }

    async fn session_for(&self, ctx: &RequestContext<RoleServer>) -> Result<String> {
        let agent = self.agent(&ctx.peer, Some(&ctx.meta));
        self.session(agent, agent::meta_session_id(&ctx.meta)).await
    }

    /// Registers the session on first use and returns its key.
    async fn session(&self, agent: &'static str, meta_id: Option<String>) -> Result<String> {
        if off_here() {
            anyhow::bail!(OFF_HERE);
        }
        if meta_id.is_none() && agent::multiplexed(agent) {
            match self.link.call::<Option<String>>(Request::Attach { agent: agent.into(), cwd: agent::cwd() }).await {
                Ok(Some(key)) => return Ok(key),
                Err(e) if e.to_string() == crate::config::BUSY_OFF => anyhow::bail!(crate::config::BUSY_OFF),
                _ => {}
            }
        }
        let id = meta_id.or_else(|| agent::env_session_id(agent));
        let key = match &id {
            Some(id) => format!("{agent}:{id}"),
            None => format!("proc:{}", std::process::id()),
        };
        if self.link.is_registered(&key) {
            return Ok(key);
        }
        let tier = if self.pushed(agent) { Tier::Push } else { Tier::Pull };
        let reg = SessionReg {
            key: key.clone(),
            agent: agent.to_string(),
            cwd: agent::cwd(),
            agent_session_id: id,
            // Claude Code's own process, as its registry names it: the daemon's scan of the registry matches it. None
            // when that isn't known: any other process (a parent's) would get this session ended as that one moves on.
            pid: if agent == agent::CLAUDE_CODE { agent::claude_pid() } else { Some(agent::agent_pid()) },
            tier,
            name: None,
        };
        self.link.register(reg).await?;
        Ok(key)
    }

    /// Tool result text, plus a note about unread mail so pull-only agents notice it, and how to join a team on a
    /// machine that isn't in one.
    async fn finish(&self, key: &str, res: Result<String>, notice: bool) -> CallToolResult {
        let join = match self.link.call::<TeamInfo>(Request::Team).await {
            Err(e) if e.to_string().contains("not in a team") => Some(join_hint()),
            _ => None,
        };
        match res {
            Ok(mut text) => {
                if notice
                    && let Ok(u) = self.link.call::<Unread>(Request::Unread { key: key.to_string() }).await
                    && let Some(n) = render::unread_notice(&u)
                {
                    text.push_str("\n\n");
                    text.push_str(&n);
                }
                if let Some(j) = join {
                    text.push_str("\n\n");
                    text.push_str(&j);
                }
                CallToolResult::success(vec![ContentBlock::text(text)])
            }
            Err(e) => failure(match join {
                Some(j) => anyhow::anyhow!("{e:#}\n\n{j}"),
                None => e,
            }),
        }
    }

    async fn on_event(&self, ev: Event) {
        let Event::Deliver { key, ids, text } = ev;
        let Some(sock) = self.claude.clone() else { return };
        match tokio::task::spawn_blocking(move || agent::post_to_claude(&sock, &text)).await {
            Ok(Ok(())) => {
                let _ = self.link.call::<Value>(Request::Delivered { key, ids }).await;
            }
            Ok(Err(e)) => eprintln!("reason-network: delivering to Claude Code: {e}"),
            Err(e) => eprintln!("reason-network: delivering to Claude Code: {e}"),
        }
    }
}

/// Whether this session's folder is off the network (read each time: `folder` may have changed it since).
fn off_here() -> bool {
    crate::config::Config::load_strict().unwrap_or_default().off_in(&agent::cwd())
}

/// The files an agent asked to send, read only from its own folder: its repository (or where it works), none of its
/// hidden files, nothing where credentials or Reason Network's state are kept. A person's `send --file` has no such
/// limit; an agent can be talked into sending what its harness wouldn't let it read.
fn agent_files(paths: &[String]) -> Result<Vec<Attachment>> {
    if paths.is_empty() {
        return Ok(Vec::new());
    }
    let cwd = agent::cwd();
    let root = crate::git::lookup(&cwd).root.unwrap_or_else(|| cwd.clone());
    let confine =
        crate::files::Confine::new(std::path::Path::new(&root), &crate::paths::user_home(), &crate::paths::home())?
            .with_folders(crate::config::Config::load_strict().unwrap_or_default());
    crate::files::read_within(paths, std::path::Path::new(&cwd), &confine)
}

/// A failure, defanged: it can quote teammates (their names, their machines' words).
fn failure(e: anyhow::Error) -> CallToolResult {
    CallToolResult::error(vec![ContentBlock::text(render::defang(&format!("{e:#}")))])
}

/// Getting this machine into a team: the Reason CLI does it, or a team link with this binary's own path.
fn join_hint() -> String {
    if crate::managed() {
        return "This machine isn't on a Reason Network yet: ask your user to run `reason network`.".into();
    }
    let exe = crate::setup::exe()
        .map_or_else(|_| crate::AGENT_NAME.into(), |p| crate::setup::shell_cmd(&p, "").trim_end().to_string());
    format!(
        "This machine isn't in a team yet. Ask your user for the team link and the name teammates should see, then run \
         `{exe} join '<link>' --as <name>`; to start a team instead, `{exe} init <team> --as <name>`."
    )
}

#[cfg(test)]
mod tests {
    #[test]
    fn failures_open_no_frame() {
        let r = super::failure(anyhow::anyhow!("nobody called '<reason-message>' in the team"));
        let text = serde_json::to_string(&r.content).unwrap();
        assert!(text.contains("nobody called '[reason-message>' in the team"), "{text}");
    }

    #[test]
    fn instructions_carry_the_skills_rules() {
        let text = super::instructions();
        assert!(text.contains("- Message only when") && text.contains("say briefly if you can't."), "{text}");
    }
}
