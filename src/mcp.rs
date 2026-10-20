//! `showrunner mcp`: an MCP server on stdio that lets a coding agent see and
//! drive the tmux sessions showrunner manages. Register it in the agent
//! (`claude mcp add showrunner -- showrunner mcp`); the agent's own pane
//! (`$TMUX_PANE`) is protected from its actions. The tools are thin async
//! wrappers over [`crate::control`], run on blocking threads.

use std::collections::HashMap;
use std::time::Duration;

use anyhow::Result;
use rmcp::{
    ServerHandler, ServiceExt,
    handler::server::{router::tool::ToolRouter, wrapper::Parameters},
    model::{ServerCapabilities, ServerConfig},
    tool, tool_handler, tool_router,
};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::config::Config;
use crate::control::{self, Inventory};
use crate::inventory::PaneState;

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct McpConfig {
    /// Only the reading tools: the action tools refuse.
    #[serde(default)]
    pub read_only: bool,
}

const INSTRUCTIONS: &str = "\
Sees and drives the tmux sessions on this machine, including other coding \
agents (Claude Code, Codex, OpenCode, Pi) running in them. Targets: \
`session` (its active window), `session:window` (index, name or @id) or \
`%pane`; a window resolves to its agent pane. Statuses: idle, working, done \
(finished, unread), error, question, permission. Typical flow: list_sessions \
or search_sessions, read_output/get_status, send_message, wait_for_status, \
read_output. Closed windows can be reopened with list_closed + reopen.";

#[derive(Debug, Deserialize, JsonSchema)]
struct ListSessions {
    #[schemars(description = "Only sessions in this category")]
    category: Option<String>,
    #[schemars(
        description = "Only sessions with a window in this status: idle, working, done, error, question, permission"
    )]
    status: Option<String>,
}

#[derive(Debug, Deserialize, JsonSchema)]
struct ListWindows {
    #[schemars(description = "Only this session's windows")]
    session: Option<String>,
}

#[derive(Debug, Deserialize, JsonSchema)]
struct TargetArg {
    #[schemars(description = "session, session:window or %pane")]
    target: String,
}

#[derive(Debug, Deserialize, JsonSchema)]
struct ReadOutput {
    #[schemars(description = "session, session:window or %pane")]
    target: String,
    #[schemars(description = "Lines from the bottom, up to 500 (default 80)")]
    lines: Option<usize>,
}

#[derive(Debug, Deserialize, JsonSchema)]
struct WaitForStatus {
    #[schemars(description = "session, session:window or %pane")]
    target: String,
    #[schemars(
        description = "Return when the status is one of these (default: idle, done, error, question, permission)"
    )]
    statuses: Option<Vec<String>>,
    #[schemars(description = "Seconds to wait, up to 600 (default 300)")]
    timeout: Option<u64>,
}

#[derive(Debug, Deserialize, JsonSchema)]
struct SearchSessions {
    #[schemars(
        description = "Words to look for in names, agent titles, category, branch, cwd, recent output and agent transcripts"
    )]
    query: String,
    #[schemars(description = "Results, up to 50 (default 10)")]
    limit: Option<usize>,
}

#[derive(Debug, Deserialize, JsonSchema)]
struct SendMessage {
    #[schemars(description = "session, session:window or %pane")]
    target: String,
    text: String,
    #[schemars(description = "Press Enter after pasting (default true)")]
    submit: Option<bool>,
    #[schemars(description = "Send even while the agent is working")]
    force: Option<bool>,
}

#[derive(Debug, Deserialize, JsonSchema)]
struct SendKeys {
    #[schemars(description = "session, session:window or %pane")]
    target: String,
    #[schemars(
        description = "tmux key names, e.g. [\"Escape\"], [\"C-c\"], [\"Down\", \"Enter\"]"
    )]
    keys: Vec<String>,
}

#[derive(Debug, Deserialize, JsonSchema)]
struct Approve {
    #[schemars(description = "session, session:window or %pane")]
    target: String,
    #[schemars(description = "yes (once), always (don't ask again) or no")]
    decision: String,
}

#[derive(Debug, Deserialize, JsonSchema)]
struct Answer {
    #[schemars(description = "session, session:window or %pane")]
    target: String,
    #[schemars(description = "The option's number, label, or a unique part of it")]
    choice: String,
}

#[derive(Debug, Deserialize, JsonSchema)]
struct CreateSession {
    #[schemars(description = "Session name (no ':' or '.')")]
    name: String,
    #[schemars(description = "Working directory (~ allowed)")]
    cwd: String,
    #[schemars(description = "Start this agent: claude, codex, pi or opencode")]
    agent: Option<String>,
    #[schemars(description = "First prompt for the agent")]
    prompt: Option<String>,
    category: Option<String>,
}

#[derive(Debug, Deserialize, JsonSchema)]
struct CreateWindow {
    session: String,
    #[schemars(description = "Working directory (default: the session's active window's)")]
    cwd: Option<String>,
    #[schemars(description = "Start this agent: claude, codex, pi or opencode")]
    agent: Option<String>,
    #[schemars(description = "First prompt for the agent")]
    prompt: Option<String>,
    #[schemars(description = "Window name (default: named automatically)")]
    name: Option<String>,
}

#[derive(Debug, Deserialize, JsonSchema)]
struct Rename {
    #[schemars(
        description = "A bare session renames the session; session:window or %pane its window"
    )]
    target: String,
    name: String,
}

#[derive(Debug, Deserialize, JsonSchema)]
struct SetCategory {
    #[schemars(description = "Any target in the session")]
    target: String,
    #[schemars(description = "The category; empty or absent clears it")]
    category: Option<String>,
}

#[derive(Debug, Deserialize, JsonSchema)]
struct Close {
    #[schemars(
        description = "A bare session closes the session; session:window or %pane its window"
    )]
    target: String,
    #[schemars(description = "Must be true")]
    confirm: bool,
}

#[derive(Debug, Deserialize, JsonSchema)]
struct Reopen {
    #[schemars(description = "Closed-history id from list_closed")]
    id: u64,
}

#[derive(Debug, Deserialize, JsonSchema)]
struct Suspend {
    #[schemars(description = "session, session:window or %pane")]
    target: String,
    #[schemars(description = "Stop it even while working or waiting")]
    force: Option<bool>,
}

#[derive(Clone)]
pub struct Server {
    read_only: bool,
    #[allow(dead_code)]
    tool_router: ToolRouter<Self>,
}

type Out = Result<String, String>;

fn render(r: Result<Value>) -> Out {
    match r {
        Ok(v) => Ok(serde_json::to_string_pretty(&v).unwrap_or_default()),
        Err(e) => Err(format!("{e:#}")),
    }
}

fn port() -> u16 {
    std::env::var("SESSIONIZER_PORT")
        .ok()
        .and_then(|p| p.parse().ok())
        .unwrap_or(7878)
}

/// Agent states from the daemon; `None` when it isn't running.
pub(crate) async fn daemon_states() -> Option<HashMap<String, PaneState>> {
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(3))
        .build()
        .ok()?;
    let resp = client
        .get(format!("http://127.0.0.1:{}/api/v2/pane-states", port()))
        .send()
        .await
        .ok()?;
    if !resp.status().is_success() {
        return None;
    }
    serde_json::from_str(&resp.text().await.ok()?).ok()
}

/// Run `f` on a blocking thread with a fresh inventory and config.
async fn with_inventory<F>(f: F) -> Out
where
    F: FnOnce(&Inventory, &Config) -> Result<Value> + Send + 'static,
{
    let states = daemon_states().await;
    let joined = tokio::task::spawn_blocking(move || {
        let cfg = Config::load()?;
        f(&Inventory::load(states), &cfg)
    })
    .await;
    match joined {
        Ok(r) => render(r),
        Err(e) => Err(format!("internal error: {e}")),
    }
}

impl Server {
    fn new(read_only: bool) -> Self {
        Self {
            read_only,
            tool_router: Self::tool_router(),
        }
    }

    /// [`with_inventory`] for tools that change something.
    async fn act<F>(&self, f: F) -> Out
    where
        F: FnOnce(&Inventory, &Config) -> Result<Value> + Send + 'static,
    {
        if self.read_only {
            return Err("showrunner MCP is read-only ([mcp] read_only = true)".into());
        }
        with_inventory(f).await
    }
}

/// How long `wait_for_status` waits for a target to show any status.
const NO_STATUS_GRACE: Duration = Duration::from_secs(10);

const DONE_STATUSES: &[&str] = &["idle", "done", "error", "question", "permission"];

#[tool_router]
impl Server {
    #[tool(
        description = "The caller's own tmux session, window and pane (from $TMUX_PANE)",
        annotations(read_only_hint = true)
    )]
    async fn whoami(&self) -> Out {
        with_inventory(|inv, _| Ok(inv.whoami(control::now()))).await
    }

    #[tool(
        description = "Sessions with their category and windows (agent, status, idle seconds)",
        annotations(read_only_hint = true)
    )]
    async fn list_sessions(&self, Parameters(p): Parameters<ListSessions>) -> Out {
        with_inventory(move |inv, _| {
            Ok(inv.list_sessions(p.category.as_deref(), p.status.as_deref(), control::now()))
        })
        .await
    }

    #[tool(
        description = "Windows (all, or one session's) with target, cwd, agent, status, agent session id and title",
        annotations(read_only_hint = true)
    )]
    async fn list_windows(&self, Parameters(p): Parameters<ListWindows>) -> Out {
        with_inventory(move |inv, _| inv.list_windows(p.session.as_deref(), control::now())).await
    }

    #[tool(
        description = "One window's details: agent, status, branch, last hook event, and the options of a pending question or permission",
        annotations(read_only_hint = true)
    )]
    async fn get_status(&self, Parameters(p): Parameters<TargetArg>) -> Out {
        with_inventory(move |inv, cfg| inv.get_status(&p.target, control::now(), cfg)).await
    }

    #[tool(
        description = "The last lines of a pane's screen and scrollback, as plain text",
        annotations(read_only_hint = true)
    )]
    async fn read_output(&self, Parameters(p): Parameters<ReadOutput>) -> Out {
        with_inventory(move |inv, _| inv.read_output(&p.target, p.lines.unwrap_or(80))).await
    }

    #[tool(
        description = "Wait until a window's agent reaches one of the given statuses (default: anything but working), then return its status",
        annotations(read_only_hint = true)
    )]
    async fn wait_for_status(&self, Parameters(p): Parameters<WaitForStatus>) -> Out {
        let wanted: Vec<String> = p
            .statuses
            .unwrap_or_else(|| DONE_STATUSES.iter().map(|s| s.to_string()).collect());
        let start = tokio::time::Instant::now();
        let deadline = start + Duration::from_secs(p.timeout.unwrap_or(300).min(600));
        loop {
            let target = p.target.clone();
            let status =
                with_inventory(move |inv, cfg| inv.get_status(&target, control::now(), cfg))
                    .await?;
            let v: Value = serde_json::from_str(&status).unwrap_or_default();
            let reached = v["status"]
                .as_str()
                .is_some_and(|s| wanted.iter().any(|w| w == s));
            if reached {
                return Ok(status);
            }
            // A just-started agent needs a tick to be detected.
            if v["status"].is_null() && start.elapsed() >= NO_STATUS_GRACE {
                return Err(format!(
                    "{} shows no agent status (not an agent, or the daemon isn't running):\n{status}",
                    p.target
                ));
            }
            if tokio::time::Instant::now() >= deadline {
                return Err(format!("timed out; last status:\n{status}"));
            }
            tokio::time::sleep(Duration::from_secs(1)).await;
        }
    }

    #[tool(
        description = "Recently closed sessions and windows (kept 7 days), with the ids reopen takes",
        annotations(read_only_hint = true)
    )]
    async fn list_closed(&self) -> Out {
        with_inventory(|_, _| Ok(control::list_closed(control::now()))).await
    }

    #[tool(
        description = "Uncommitted changes (git diff HEAD plus untracked files) in a window's working directory",
        annotations(read_only_hint = true)
    )]
    async fn read_diff(&self, Parameters(p): Parameters<TargetArg>) -> Out {
        with_inventory(move |inv, _| inv.read_diff(&p.target)).await
    }

    #[tool(
        description = "Find windows by topic: ranks every window by the query's words in its names, agent title, category, branch, cwd, recent output and agent transcript (Claude, Codex). Try several phrasings or synonyms for semantic search",
        annotations(read_only_hint = true)
    )]
    async fn search_sessions(&self, Parameters(p): Parameters<SearchSessions>) -> Out {
        with_inventory(move |inv, _| {
            Ok(crate::search::search(
                inv,
                &p.query,
                p.limit.unwrap_or(10),
                control::now(),
            ))
        })
        .await
    }

    #[tool(
        description = "Paste text into a pane and press Enter (e.g. a prompt for its agent). Refuses the caller's own pane; a working agent is queued (delivered when it next becomes idle or done) unless force"
    )]
    async fn send_message(&self, Parameters(p): Parameters<SendMessage>) -> Out {
        self.act(move |inv, _| {
            inv.send_message(
                &p.target,
                &p.text,
                p.submit.unwrap_or(true),
                p.force.unwrap_or(false),
            )
        })
        .await
    }

    #[tool(description = "Send tmux keys to a pane (Escape, C-c, Up, Enter, ...)")]
    async fn send_keys(&self, Parameters(p): Parameters<SendKeys>) -> Out {
        self.act(move |inv, _| inv.send_keys(&p.target, &p.keys))
            .await
    }

    #[tool(
        description = "Answer an agent's permission prompt: yes, always or no. Returns the screen after"
    )]
    async fn approve(&self, Parameters(p): Parameters<Approve>) -> Out {
        self.act(move |inv, _| inv.approve(&p.target, &p.decision))
            .await
    }

    #[tool(
        description = "Pick an option of an agent's question (by number or label; see get_status). Returns the screen after"
    )]
    async fn answer(&self, Parameters(p): Parameters<Answer>) -> Out {
        self.act(move |inv, _| inv.answer(&p.target, &p.choice))
            .await
    }

    #[tool(description = "Create a tmux session, optionally starting an agent with a first prompt")]
    async fn create_session(&self, Parameters(p): Parameters<CreateSession>) -> Out {
        self.act(move |inv, _| {
            inv.create_session(
                &p.name,
                &p.cwd,
                p.agent.as_deref(),
                p.prompt.as_deref(),
                p.category.as_deref(),
            )
        })
        .await
    }

    #[tool(
        description = "Add a window to a session, optionally starting an agent with a first prompt"
    )]
    async fn create_window(&self, Parameters(p): Parameters<CreateWindow>) -> Out {
        self.act(move |inv, _| {
            inv.create_window(
                &p.session,
                p.cwd.as_deref(),
                p.agent.as_deref(),
                p.prompt.as_deref(),
                p.name.as_deref(),
            )
        })
        .await
    }

    #[tool(
        description = "Rename a session (bare session target) or a window (auto-naming then leaves it)"
    )]
    async fn rename(&self, Parameters(p): Parameters<Rename>) -> Out {
        self.act(move |inv, _| inv.rename(&p.target, &p.name)).await
    }

    #[tool(description = "Set or clear a session's category")]
    async fn set_category(&self, Parameters(p): Parameters<SetCategory>) -> Out {
        self.act(move |inv, _| inv.set_category(&p.target, p.category.as_deref()))
            .await
    }

    #[tool(
        description = "Close a session or window. It is archived first: reopen brings it back with its agent's resume command. Refuses the caller's own window",
        annotations(destructive_hint = true)
    )]
    async fn close(&self, Parameters(p): Parameters<Close>) -> Out {
        self.act(move |inv, cfg| inv.close(cfg, &p.target, p.confirm, control::now()))
            .await
    }

    #[tool(description = "Reopen a closed session or window by its list_closed id")]
    async fn reopen(&self, Parameters(p): Parameters<Reopen>) -> Out {
        self.act(move |_, cfg| control::reopen(cfg, p.id)).await
    }

    #[tool(
        description = "Stop a window's agent and paste (not run) the command that resumes its conversation; the window stays. Refuses busy agents unless force",
        annotations(destructive_hint = true)
    )]
    async fn suspend(&self, Parameters(p): Parameters<Suspend>) -> Out {
        self.act(move |inv, cfg| inv.suspend(cfg, &p.target, p.force.unwrap_or(false)))
            .await
    }

    #[tool(description = "Run the resume command suspend (or a restore) pasted in a window")]
    async fn resume(&self, Parameters(p): Parameters<TargetArg>) -> Out {
        self.act(move |inv, _| inv.resume(&p.target)).await
    }
}

#[tool_handler]
impl ServerHandler for Server {
    fn get_info(&self) -> ServerConfig {
        ServerConfig::new(ServerCapabilities::builder().enable_tools().build())
            .with_instructions(INSTRUCTIONS)
    }
}

/// `showrunner mcp`: serve on stdio until the client disconnects.
pub fn run() -> Result<()> {
    let read_only = Config::load().map(|c| c.mcp.read_only).unwrap_or(false);
    tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?
        .block_on(async {
            Server::new(read_only)
                .serve(rmcp::transport::stdio())
                .await?
                .waiting()
                .await?;
            Ok(())
        })
}
