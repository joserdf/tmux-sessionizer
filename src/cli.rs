//! Non-TUI entry points: `serve` plus the commands an agent running inside a
//! session uses to view and manage other tasks and sessions, and to ask another
//! session a question.

use std::collections::HashMap;
use std::thread::sleep;
use std::time::{Duration, Instant, SystemTime};

use anyhow::{Result, bail};
use serde_json::{Value, json};

use crate::agent::AgentKind;
use crate::config::Config;
use crate::ops;
use crate::server;
use crate::tmux::{self, SessionStatus, TmuxSession};

const HELP: &str = "\
showrunner — TUI for managing coding agent sessions

Usage:
  showrunner                                    launch the TUI
  showrunner serve [--bind <addr:port>]         serve the mobile web UI
                                                    (default 127.0.0.1:7878)

Managing tasks and sessions (usable from inside a session):
  showrunner list [--json] [--project <name>]
  showrunner task create <project> <name> [--branch <b>] [--base <b>]
                                           [--prompt <text>]
                                           [--agent claude|codex|pi]
  showrunner task set-base <project> <task> <branch>   ('main' resets)
  showrunner task delete <project> <task> --yes
  showrunner session create <project> <task> [--prompt <text>] [--no-worktree]
                                              [--agent claude|codex|pi]
  showrunner session kill <session> --yes

Talking to another session:
  showrunner ask <session> <question> [--timeout <secs>]
  showrunner send <session> <text> [--no-submit]
  showrunner output <session> [--lines <n>]

<session> is a ref from `list` — `<project>/<task>/<session>`, `<project>/<task>`
for that task's main session, or a raw tmux session name.
";

/// Handle non-TUI CLI invocations. Returns `Some(result)` when an argument was
/// recognized (the process should exit), or `None` to fall through to the TUI.
pub fn dispatch(args: &[String]) -> Option<Result<()>> {
    let rest = args.get(1..).unwrap_or_default();
    match args.first().map(String::as_str) {
        Some("serve") => Some(cmd_serve(rest)),
        Some("list") => Some(cmd_list(rest)),
        Some("task") => Some(cmd_task(rest)),
        Some("session") => Some(cmd_session(rest)),
        Some("ask") => Some(cmd_ask(rest)),
        Some("send") => Some(cmd_send(rest)),
        Some("output") => Some(cmd_output(rest)),
        Some("--help" | "-h" | "help") => {
            print!("{HELP}");
            Some(Ok(()))
        }
        Some("--version" | "-V" | "version") => {
            println!("showrunner {}", env!("CARGO_PKG_VERSION"));
            Some(Ok(()))
        }
        _ => None,
    }
}

/// Split args into positionals and flags. Value flags accept `--flag value` or
/// `--flag=value`; bool flags are stored as "true" when present.
fn parse_args(
    args: &[String],
    value_flags: &[&str],
    bool_flags: &[&str],
) -> Result<(Vec<String>, HashMap<String, String>)> {
    let mut positional = Vec::new();
    let mut flags = HashMap::new();
    let mut it = args.iter();

    while let Some(arg) = it.next() {
        let Some(name) = arg.strip_prefix("--") else {
            positional.push(arg.clone());
            continue;
        };
        let (name, inline) = match name.split_once('=') {
            Some((n, v)) => (n, Some(v.to_string())),
            None => (name, None),
        };

        if value_flags.contains(&name) {
            let value = match inline {
                Some(v) => v,
                None => it
                    .next()
                    .cloned()
                    .ok_or_else(|| anyhow::anyhow!("--{name} requires a value"))?,
            };
            flags.insert(name.to_string(), value);
        } else if bool_flags.contains(&name) {
            if inline.is_some() {
                bail!("--{name} takes no value");
            }
            flags.insert(name.to_string(), "true".to_string());
        } else {
            bail!("unknown flag '--{name}'");
        }
    }

    Ok((positional, flags))
}

fn cmd_serve(args: &[String]) -> Result<()> {
    let (positional, flags) = parse_args(args, &["bind"], &[])?;
    if let Some(extra) = positional.first() {
        bail!("unexpected argument '{extra}' (usage: serve [--bind addr:port])");
    }
    let bind = flags
        .get("bind")
        .cloned()
        .unwrap_or_else(|| "127.0.0.1:7878".to_string());
    server::run(&bind)
}

/// Canonical `<project>/<task>/<session>` ref for a session.
fn session_ref(s: &TmuxSession) -> String {
    format!("{}/{}/{}", s.project_name, s.task_name, s.session_name)
}

/// The tmux session this process runs in, when invoked from inside one.
fn current_session_name() -> Option<String> {
    std::env::var_os("TMUX")?;
    let pane = std::env::var("TMUX_PANE").ok();
    let mut args = vec!["display-message", "-p"];
    if let Some(pane) = &pane {
        args.extend(["-t", pane]);
    }
    args.push("#S");

    let out = std::process::Command::new("tmux")
        .args(&args)
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let name = String::from_utf8_lossy(&out.stdout).trim().to_string();
    if name.is_empty() { None } else { Some(name) }
}

/// Resolve a session ref to a live session. Accepts a raw tmux name,
/// `<project>/<task>/<session>`, or `<project>/<task>` (its main session).
fn resolve_session(reference: &str, sessions: &[TmuxSession]) -> Result<TmuxSession> {
    let reference = reference.trim().trim_matches('/');
    if reference.is_empty() {
        bail!("a session ref is required");
    }

    let found = if reference.starts_with("cm__") {
        sessions.iter().find(|s| s.name == reference)
    } else {
        let parts: Vec<&str> = reference.split('/').collect();
        let (project, task, session) = match parts.as_slice() {
            [project, task, session] => (*project, *task, *session),
            [project, task] => (*project, *task, tmux::MAIN_SESSION),
            _ => bail!(
                "'{reference}' is not a session ref (expected <project>/<task>[/<session>] or a tmux name)"
            ),
        };
        sessions.iter().find(|s| {
            s.project_name == tmux::sanitize(project)
                && s.task_name == tmux::sanitize(task)
                && s.session_name == tmux::sanitize(session)
        })
    };

    found.cloned().ok_or_else(|| {
        anyhow::anyhow!("no live session matching '{reference}' (see `showrunner list`)")
    })
}

/// Whether a session is still working on its turn. The pane can sit unchanged
/// for seconds mid-turn, so a stable pane alone doesn't mean the agent is done:
/// Claude Code marks work in flight with a spinner line ("✽ Gallivanting…", "⎿
/// Running…") and an "esc to interrupt" hint. Notifications can render below the
/// spinner, so look at the last few transcript lines rather than just the last.
fn session_looks_busy(session_name: &str) -> bool {
    let Some(text) = tmux::capture_output_plain(session_name, 100) else {
        return false;
    };
    if tail(&text, 6).contains("esc to interrupt") {
        return true;
    }

    let transcript = trim_pane(&text, tmux::session_agent(session_name));
    transcript
        .lines()
        .rev()
        .filter(|l| !l.trim().is_empty())
        .take(5)
        .any(is_spinner_line)
}

/// A spinner line is a glyph plus a gerund — "✽ Gallivanting…", "⎿ Running…",
/// pi's "⠏ Working..." — possibly with a counter, as opposed to prose that
/// happens to end in an ellipsis.
fn is_spinner_line(line: &str) -> bool {
    let line = line.trim();
    (line.ends_with('…') || line.ends_with("...")) && line.split_whitespace().count() <= 4
}

/// Sample session statuses the way the TUI's worker does: a session counts as
/// running while its pane keeps changing, so this probes twice.
fn sample_statuses(sessions: &[TmuxSession]) -> HashMap<String, SessionStatus> {
    let first: HashMap<String, Option<tmux::SessionProbe>> = sessions
        .iter()
        .map(|s| (s.name.clone(), tmux::probe_session(&s.name)))
        .collect();

    sleep(Duration::from_millis(800));

    sessions
        .iter()
        .map(|s| {
            let probe = tmux::probe_session(&s.name);
            let status = match (first.get(&s.name).and_then(|p| p.as_ref()), probe) {
                (_, None) => SessionStatus::Finished,
                (_, Some(p)) if !p.agent_alive => SessionStatus::Finished,
                (Some(before), Some(after)) if before.content_hash != after.content_hash => {
                    SessionStatus::Running
                }
                (_, Some(after)) if after.has_permission_prompt => {
                    SessionStatus::WaitingForPermission
                }
                _ if session_looks_busy(&s.name) => SessionStatus::Running,
                _ => SessionStatus::WaitingForInput,
            };
            (s.name.clone(), status)
        })
        .collect()
}

fn cmd_list(args: &[String]) -> Result<()> {
    let (positional, flags) = parse_args(args, &["project"], &["json"])?;
    if let Some(extra) = positional.first() {
        bail!("unexpected argument '{extra}' (usage: list [--json] [--project <name>])");
    }

    let cfg = Config::load()?;
    let sessions = tmux::list_sessions().unwrap_or_default();
    let statuses = sample_statuses(&sessions);
    let current = current_session_name();
    let project_filter = flags.get("project");

    let projects: Vec<&crate::config::Project> = cfg
        .projects
        .iter()
        .filter(|p| project_filter.is_none_or(|f| &p.name == f))
        .collect();
    if let Some(filter) = project_filter.filter(|_| projects.is_empty()) {
        bail!("project '{filter}' not found");
    }

    let session_value = |s: &TmuxSession| {
        json!({
            "ref": session_ref(s),
            "tmux_name": s.name,
            "name": s.session_name,
            "status": statuses.get(&s.name).map(|st| st.as_str()),
            "agent": tmux::session_agent(&s.name).id(),
            "current": current.as_deref() == Some(s.name.as_str()),
        })
    };

    if flags.contains_key("json") {
        let projects: Vec<Value> = projects
            .iter()
            .map(|p| {
                let stacks = p.stack_positions();
                let tasks: Vec<Value> = p
                    .tasks_stack_ordered()
                    .into_iter()
                    .map(|t| {
                        json!({
                            "name": t.name,
                            "branch": t.branch,
                            "base_branch": t.base_branch(),
                            "archived": t.archived,
                            "stack": stacks.get(&t.branch).map(|(pos, total)| {
                                json!({ "position": pos, "size": total })
                            }),
                            "sessions": tmux::sessions_for_task(&p.name, &t.name, &sessions)
                                .iter()
                                .map(session_value)
                                .collect::<Vec<_>>(),
                        })
                    })
                    .collect();
                json!({
                    "name": p.name,
                    "path": p.path,
                    "tasks": tasks,
                    "adhoc_sessions": tmux::adhoc_sessions_for_project(&p.name, &sessions)
                        .iter()
                        .map(session_value)
                        .collect::<Vec<_>>(),
                })
            })
            .collect();
        println!(
            "{}",
            serde_json::to_string_pretty(&json!({ "projects": projects }))?
        );
        return Ok(());
    }

    if projects.is_empty() {
        println!("no projects configured");
        return Ok(());
    }

    let print_session = |s: &TmuxSession| {
        let status = statuses
            .get(&s.name)
            .map(|st| st.as_str())
            .unwrap_or("unknown");
        let agent = tmux::session_agent(&s.name);
        let agent = if agent == AgentKind::default() {
            String::new()
        } else {
            format!("  agent={agent}")
        };
        let you = if current.as_deref() == Some(s.name.as_str()) {
            "  (this session)"
        } else {
            ""
        };
        println!("    {:<40} {status}{agent}{you}", session_ref(s));
    };

    for project in projects {
        println!("{}  ({})", project.name, project.path);
        let stacks = project.stack_positions();
        for task in project.tasks_stack_ordered() {
            let archived = if task.archived { "  [archived]" } else { "" };
            let stack = stacks
                .get(&task.branch)
                .map(|(pos, total)| format!("  stack={pos}/{total}"))
                .unwrap_or_default();
            println!(
                "  task {}  branch={} base={}{stack}{archived}",
                task.name,
                task.branch,
                task.base_branch()
            );
            let task_sessions = tmux::sessions_for_task(&project.name, &task.name, &sessions);
            if task_sessions.is_empty() {
                println!("    (no live sessions)");
            }
            for s in &task_sessions {
                print_session(s);
            }
        }
        let adhoc = tmux::adhoc_sessions_for_project(&project.name, &sessions);
        if !adhoc.is_empty() {
            println!("  adhoc sessions");
            for s in &adhoc {
                print_session(s);
            }
        }
    }

    Ok(())
}

fn cmd_task(args: &[String]) -> Result<()> {
    match args.first().map(String::as_str) {
        Some("create") => cmd_task_create(&args[1..]),
        Some("set-base") => cmd_task_set_base(&args[1..]),
        Some("delete") => cmd_task_delete(&args[1..]),
        Some(other) => {
            bail!("unknown task command '{other}' (expected create, set-base or delete)")
        }
        None => bail!(
            "usage: task create <project> <name> | task set-base <project> <task> <branch> | \
             task delete <project> <task> --yes"
        ),
    }
}

fn cmd_task_create(args: &[String]) -> Result<()> {
    let (positional, flags) = parse_args(args, &["branch", "base", "prompt", "agent"], &[])?;
    let [project_name, task_name] = positional.as_slice() else {
        bail!(
            "usage: task create <project> <name> [--branch <b>] [--base <b>] [--prompt <text>] \
             [--agent <id>]"
        );
    };

    let cfg = Config::load()?;
    let project = ops::find_project(&cfg, project_name)?;
    let agent = ops::resolve_agent(&cfg, flags.get("agent").map(String::as_str))?;
    let base = flags.get("base").map(String::as_str);
    let (branch, tmux_name) = ops::create_task(
        &cfg,
        project,
        task_name,
        flags.get("branch").map(String::as_str),
        base,
        flags
            .get("prompt")
            .map(String::as_str)
            .filter(|p| !p.trim().is_empty()),
        agent,
    )?;

    let session = TmuxSession::from_tmux_name(&tmux_name)
        .map(|s| session_ref(&s))
        .unwrap_or(tmux_name);
    match base.map(str::trim).filter(|b| !b.is_empty()) {
        Some(base) => println!("created task '{task_name}' on branch {branch} (base {base})"),
        None => println!("created task '{task_name}' on branch {branch}"),
    }
    println!("main session: {session}");
    Ok(())
}

fn cmd_task_set_base(args: &[String]) -> Result<()> {
    let (positional, _) = parse_args(args, &[], &[])?;
    let [project_name, task_name, base] = positional.as_slice() else {
        bail!("usage: task set-base <project> <task> <branch>   ('main' resets to the default)");
    };

    let cfg = Config::load()?;
    let project = ops::find_project(&cfg, project_name)?;
    if !project.tasks.iter().any(|t| t.name == *task_name) {
        bail!("task '{task_name}' not found in project '{project_name}'");
    }

    let base = base.trim();
    let new_base = Some(base.to_string()).filter(|b| !b.is_empty() && b != "main");
    if let Some(b) = &new_base
        && !tmux::branch_exists(&project.path, b)
    {
        bail!(
            "base branch '{b}' does not exist in {} (create it first, or check the name)",
            project.path
        );
    }

    let label = new_base.as_deref().unwrap_or("main").to_string();
    let (project_name, task_name) = (project_name.clone(), task_name.clone());
    let task_for_msg = task_name.clone();
    Config::modify(move |c| {
        c.set_task_base_branch(&project_name, &task_name, new_base);
    })?;
    println!("base branch for '{task_for_msg}' set to {label}");
    Ok(())
}

fn cmd_task_delete(args: &[String]) -> Result<()> {
    let (positional, flags) = parse_args(args, &[], &["yes"])?;
    let [project_name, task_name] = positional.as_slice() else {
        bail!("usage: task delete <project> <task> --yes");
    };
    if !flags.contains_key("yes") {
        bail!(
            "deleting a task kills its sessions and removes their worktrees and branches — \
             pass --yes to confirm"
        );
    }

    let cfg = Config::load()?;
    let project = ops::find_project(&cfg, project_name)?;
    ops::delete_task(project, task_name)?;
    println!("deleted task '{task_name}'");
    Ok(())
}

fn cmd_session(args: &[String]) -> Result<()> {
    match args.first().map(String::as_str) {
        Some("create") => cmd_session_create(&args[1..]),
        Some("kill") => cmd_session_kill(&args[1..]),
        Some(other) => bail!("unknown session command '{other}' (expected create or kill)"),
        None => bail!("usage: session create <project> <task> | session kill <session> --yes"),
    }
}

fn cmd_session_create(args: &[String]) -> Result<()> {
    let (positional, flags) = parse_args(args, &["prompt", "agent"], &["no-worktree"])?;
    let [project_name, task_name] = positional.as_slice() else {
        bail!(
            "usage: session create <project> <task> [--prompt <text>] [--no-worktree] [--agent <id>]"
        );
    };

    let cfg = Config::load()?;
    let project = ops::find_project(&cfg, project_name)?;
    let task = project
        .tasks
        .iter()
        .find(|t| t.name == *task_name)
        .ok_or_else(|| anyhow::anyhow!("task '{task_name}' not found"))?;

    let agent = ops::resolve_agent(&cfg, flags.get("agent").map(String::as_str))?;
    let tmux_name = ops::create_session(
        &cfg,
        project,
        &task.name,
        &task.branch,
        !flags.contains_key("no-worktree"),
        flags
            .get("prompt")
            .map(String::as_str)
            .filter(|p| !p.trim().is_empty()),
        agent,
    )?;

    let session = TmuxSession::from_tmux_name(&tmux_name)
        .map(|s| session_ref(&s))
        .unwrap_or(tmux_name);
    println!("created session: {session}");
    Ok(())
}

fn cmd_session_kill(args: &[String]) -> Result<()> {
    let (positional, flags) = parse_args(args, &[], &["yes"])?;
    let [reference] = positional.as_slice() else {
        bail!("usage: session kill <session> --yes");
    };
    if !flags.contains_key("yes") {
        bail!(
            "killing a session removes its worktree and branch, losing unmerged work — \
             pass --yes to confirm"
        );
    }

    let sessions = tmux::list_sessions().unwrap_or_default();
    let session = resolve_session(reference, &sessions)?;
    if current_session_name().as_deref() == Some(session.name.as_str()) {
        bail!("that is this session — ask the user to kill it instead");
    }

    ops::kill_session(&session.name)?;
    println!("killed session: {}", session_ref(&session));
    Ok(())
}

fn cmd_send(args: &[String]) -> Result<()> {
    let (positional, flags) = parse_args(args, &[], &["no-submit"])?;
    let (reference, text) = positional
        .split_first()
        .ok_or_else(|| anyhow::anyhow!("usage: send <session> <text> [--no-submit]"))?;
    let text = text.join(" ");
    if text.trim().is_empty() {
        bail!("nothing to send");
    }

    let sessions = tmux::list_sessions().unwrap_or_default();
    let session = resolve_session(reference, &sessions)?;
    tmux::send_text(&session.name, &text, !flags.contains_key("no-submit"))?;
    println!("sent to {}", session_ref(&session));
    Ok(())
}

fn cmd_output(args: &[String]) -> Result<()> {
    let (positional, flags) = parse_args(args, &["lines"], &[])?;
    let [reference] = positional.as_slice() else {
        bail!("usage: output <session> [--lines <n>]");
    };
    let lines: usize = match flags.get("lines") {
        Some(v) => v
            .parse()
            .map_err(|_| anyhow::anyhow!("--lines must be a number"))?,
        None => 200,
    };

    let sessions = tmux::list_sessions().unwrap_or_default();
    let session = resolve_session(reference, &sessions)?;
    let text = tmux::capture_output_plain(&session.name, lines.clamp(10, 5000))
        .ok_or_else(|| anyhow::anyhow!("could not capture output for {reference}"))?;
    println!("{}", trim_pane(&text, tmux::session_agent(&session.name)));
    Ok(())
}

/// How long a session's pane must stay unchanged, on top of showing no work in
/// flight, before its answer counts as complete.
const IDLE_POLLS: u32 = 6;
const POLL_INTERVAL: Duration = Duration::from_millis(500);
/// A session echoes a pasted message almost immediately; no change within this
/// window means it never received it.
const REACT_TIMEOUT: Duration = Duration::from_secs(20);

fn cmd_ask(args: &[String]) -> Result<()> {
    let (positional, flags) = parse_args(args, &["timeout"], &[])?;
    let (reference, question) = positional
        .split_first()
        .ok_or_else(|| anyhow::anyhow!("usage: ask <session> <question> [--timeout <secs>]"))?;
    let question = question.join(" ");
    if question.trim().is_empty() {
        bail!("a question is required");
    }
    let timeout = match flags.get("timeout") {
        Some(v) => Duration::from_secs(
            v.parse()
                .map_err(|_| anyhow::anyhow!("--timeout must be a number of seconds"))?,
        ),
        None => Duration::from_secs(300),
    };

    let sessions = tmux::list_sessions().unwrap_or_default();
    let session = resolve_session(reference, &sessions)?;
    let name = &session.name;
    if current_session_name().as_deref() == Some(name.as_str()) {
        bail!("that ref is this session — you can't ask yourself");
    }

    let token = ask_token();
    let message = format!(
        "{question}\n\n\
         (Asked by another Showrunner session via `showrunner ask`, which is waiting for \
         your reply. Answer concisely in your response text; don't change files unless the \
         question asks you to.)\n\
         [cm-ask {token}]"
    );

    let mut last_hash = tmux::probe_session(name)
        .ok_or_else(|| anyhow::anyhow!("session {} is not running claude", session_ref(&session)))?
        .content_hash;
    tmux::send_text(name, &message, true)?;

    let start = Instant::now();
    let mut idle_polls = 0;
    let mut reacted = false;

    loop {
        sleep(POLL_INTERVAL);

        let Some(probe) = tmux::probe_session(name) else {
            bail!("session {} died while answering", session_ref(&session));
        };

        if probe.content_hash != last_hash {
            last_hash = probe.content_hash;
            idle_polls = 0;
            reacted = true;
        } else {
            idle_polls += 1;
        }

        if reacted && idle_polls >= IDLE_POLLS && !session_looks_busy(name) {
            print_reply(name, &token);
            if probe.has_permission_prompt {
                bail!(
                    "session {} is waiting for the user (permission or question dialog) — \
                     its answer is incomplete",
                    session_ref(&session)
                );
            }
            return Ok(());
        }

        if !reacted && start.elapsed() > REACT_TIMEOUT {
            bail!(
                "session {} did not react to the question (is claude running there?)",
                session_ref(&session)
            );
        }

        if start.elapsed() > timeout {
            print_reply(name, &token);
            bail!(
                "session {} was still working after {}s — the answer above is partial \
                 (retry with --timeout, or read more with `showrunner output`)",
                session_ref(&session),
                timeout.as_secs()
            );
        }
    }
}

/// Token appended to an asked question so the reply can be sliced out of the
/// target pane's transcript.
fn ask_token() -> String {
    let nanos = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .map(|d| d.subsec_nanos() as u64)
        .unwrap_or(0);
    format!("{:x}", nanos ^ (std::process::id() as u64) << 32)
}

fn print_reply(session_name: &str, token: &str) {
    let agent = tmux::session_agent(session_name);
    match tmux::capture_output_plain(session_name, 2000) {
        Some(pane) => match extract_reply(&pane, token, agent) {
            Some(reply) => println!("{reply}"),
            None => {
                eprintln!("(could not locate the reply in the pane; showing its tail)");
                println!("{}", trim_pane(&tail(&pane, 60), agent));
            }
        },
        None => eprintln!("(could not capture the session's output)"),
    }
}

/// Everything the target printed after the echoed question, with the input box
/// and footer chrome stripped.
fn extract_reply(pane: &str, token: &str, agent: AgentKind) -> Option<String> {
    let after_marker = |idx: usize| -> Option<String> {
        let rest = pane[idx..].split_once('\n')?.1;
        let cleaned = trim_pane(rest, agent);
        (!cleaned.trim().is_empty()).then_some(cleaned)
    };

    // The token sits on the last line of the question, so the reply follows it.
    // A reply that quotes the token back would shadow it — fall back to the
    // first occurrence then.
    let last = pane.rfind(token)?;
    after_marker(last).or_else(|| after_marker(pane.find(token)?))
}

/// Drop the input area and status hints tmux captured below the transcript,
/// plus surrounding blank lines and trailing spaces. Chrome differs per agent:
/// Claude frames its prompt in a box or between horizontal rules, codex's
/// input line starts with `›` (its ─ rules are transcript content).
fn trim_pane(text: &str, agent: AgentKind) -> String {
    let mut lines: Vec<&str> = text.lines().map(str::trim_end).collect();

    let is_chrome = |line: &&str| agent.is_prompt_chrome(line);

    if let Some(last) = lines.iter().rposition(is_chrome) {
        let mut start = last;
        while start > 0 && (is_chrome(&lines[start - 1]) || lines[start - 1].trim().is_empty()) {
            start -= 1;
        }
        lines.truncate(start);
    }

    while lines.first().is_some_and(|l| l.trim().is_empty()) {
        lines.remove(0);
    }
    while lines.last().is_some_and(|l| l.trim().is_empty()) {
        lines.pop();
    }
    lines.join("\n")
}

fn tail(text: &str, lines: usize) -> String {
    let all: Vec<&str> = text.lines().collect();
    all[all.len().saturating_sub(lines)..].join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(items: &[&str]) -> Vec<String> {
        items.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn dispatch_falls_through_to_the_tui_without_arguments() {
        assert!(dispatch(&[]).is_none());
        assert!(dispatch(&args(&["nonsense"])).is_none());
    }

    #[test]
    fn parse_args_splits_positionals_flags_and_inline_values() {
        let (positional, flags) = parse_args(
            &args(&["myapp", "fix-auth", "--prompt", "do a thing", "--json"]),
            &["prompt"],
            &["json"],
        )
        .unwrap();
        assert_eq!(positional, vec!["myapp", "fix-auth"]);
        assert_eq!(flags.get("prompt").unwrap(), "do a thing");
        assert_eq!(flags.get("json").unwrap(), "true");

        let (_, flags) = parse_args(&args(&["--lines=50"]), &["lines"], &[]).unwrap();
        assert_eq!(flags.get("lines").unwrap(), "50");
    }

    #[test]
    fn parse_args_rejects_unknown_flags_and_missing_values() {
        assert!(parse_args(&args(&["--nope"]), &[], &[]).is_err());
        assert!(parse_args(&args(&["--prompt"]), &["prompt"], &[]).is_err());
        assert!(parse_args(&args(&["--json=1"]), &[], &["json"]).is_err());
    }

    fn sessions() -> Vec<TmuxSession> {
        ["cm__my-app__fix-auth__main", "cm__my-app__fix-auth__2"]
            .iter()
            .map(|n| TmuxSession::from_tmux_name(n).unwrap())
            .collect()
    }

    #[test]
    fn resolve_session_accepts_refs_tmux_names_and_task_shorthand() {
        let sessions = sessions();
        let resolve = |r: &str| resolve_session(r, &sessions).map(|s| s.name);

        assert_eq!(
            resolve("my-app/fix-auth/2").unwrap(),
            "cm__my-app__fix-auth__2"
        );
        assert_eq!(
            resolve("my-app/fix-auth").unwrap(),
            "cm__my-app__fix-auth__main"
        );
        assert_eq!(
            resolve("cm__my-app__fix-auth__main").unwrap(),
            "cm__my-app__fix-auth__main"
        );
        // Names as written in the config resolve too — they get sanitized the
        // same way the tmux session name was built.
        assert_eq!(
            resolve("my app/fix-auth/2").unwrap(),
            "cm__my-app__fix-auth__2"
        );

        assert!(resolve("my-app/other-task/2").is_err());
        assert!(resolve("my-app").is_err());
        assert!(resolve("").is_err());
    }

    #[test]
    fn extract_reply_takes_everything_after_the_asked_question() {
        let pane = "\
> what owns token refresh?
  [cm-ask beef]

⏺ src/auth/refresh.ts — TokenRefresher.

╭──────────────────────────────╮
│ >                            │
╰──────────────────────────────╯
  ? for shortcuts";

        assert_eq!(
            extract_reply(pane, "beef", AgentKind::Claude).unwrap(),
            "⏺ src/auth/refresh.ts — TokenRefresher."
        );
        assert!(extract_reply(pane, "cafe", AgentKind::Claude).is_none());
    }

    #[test]
    fn extract_reply_falls_back_when_the_reply_quotes_the_token() {
        let pane = "\
> question
  [cm-ask beef]

⏺ I saw the marker [cm-ask beef] in your message.

╭───────╮
│ >     │
╰───────╯";

        assert_eq!(
            extract_reply(pane, "beef", AgentKind::Claude).unwrap(),
            "⏺ I saw the marker [cm-ask beef] in your message."
        );
    }

    #[test]
    fn trim_pane_drops_a_boxed_input_area_and_surrounding_blanks() {
        let pane = "\n\nanswer   \n\n╭───╮\n│ > │\n╰───╯\n? for shortcuts\n";
        assert_eq!(trim_pane(pane, AgentKind::Claude), "answer");
    }

    #[test]
    fn trim_pane_drops_a_rule_framed_input_area_and_status_lines() {
        let pane = "\
⏺ README.md

────────────────────────────────
❯
────────────────────────────────
  Ben@host try-cli-main Fable 5
  -- INSERT -- ⏵⏵ bypass permissions on
";
        assert_eq!(trim_pane(pane, AgentKind::Claude), "⏺ README.md");
    }

    #[test]
    fn trim_pane_codex_cuts_at_input_line_but_keeps_turn_rules() {
        // Codex separates turns with long ─ rules inside the transcript; only
        // the trailing `›` input line and the status line below it are chrome.
        let pane = "\
• Ran sleep 8 && echo done-sleeping
  └ done-sleeping
────────────────────────────────────────
• It finished.
› Ask Codex to do anything
  gpt-5.6-sol default · /some/dir
";
        assert_eq!(
            trim_pane(pane, AgentKind::Codex),
            "• Ran sleep 8 && echo done-sleeping\n  └ done-sleeping\n────────────────────────────────────────\n• It finished."
        );
    }

    #[test]
    fn extract_reply_works_on_codex_panes() {
        let pane = "\
› what is 2+2?
  [cm-ask beef]
• 4
› Ask Codex to do anything
  gpt-5.6-sol default · /some/dir";
        assert_eq!(
            extract_reply(pane, "beef", AgentKind::Codex).unwrap(),
            "• 4"
        );
    }

    #[test]
    fn trim_pane_pi_cuts_input_box_and_footer() {
        // Pi frames its input box with full-width ─ rules; the cwd line and
        // stats footer render below the bottom rule (pi 0.84.2).
        let pane = "\
 what is 2+2?
 The answer is 4.

────────────────────────────────────────

────────────────────────────────────────
/some/work/dir (main)
0.0%/128k (auto)                    (openrouter) openai/gpt-4o-mini
";
        assert_eq!(
            trim_pane(pane, AgentKind::Pi),
            "what is 2+2?\n The answer is 4."
        );
    }

    #[test]
    fn extract_reply_works_on_pi_panes() {
        let pane = "\
 what is 2+2?
 [cm-ask beef]
 The answer is 4.
────────────────────────────────────────

────────────────────────────────────────
/some/work/dir (main)
0.0%/128k (auto)                    (openrouter) openai/gpt-4o-mini";
        assert_eq!(
            extract_reply(pane, "beef", AgentKind::Pi).unwrap(),
            " The answer is 4."
        );
    }

    #[test]
    fn spinner_lines_match_claude_and_pi_styles() {
        assert!(is_spinner_line("✽ Gallivanting…"));
        assert!(is_spinner_line("⠏ Working..."));
        assert!(!is_spinner_line(
            "This is a longer prose sentence that trails off..."
        ));
    }
}
