use std::collections::{HashMap, HashSet};
use std::io::Write as _;
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, mpsc};
use std::thread;
use std::time::Duration;

use anyhow::Result;

use crate::agent::AgentKind;
use crate::config::{self, Config, KeyBindings, Project, ReviewTool, Task};
use crate::resources::SessionResources;
use crate::tmux::{self, DiffStats, SessionStatus, TmuxSession};
use crate::worker::{TaskInfo, Worker};

#[derive(Debug, Clone)]
pub enum ListItem {
    Project {
        project: Project,
    },
    Task {
        project_name: String,
        project_path: String,
        task: Task,
    },
    Session {
        project_name: String,
        project_path: String,
        task: Task,
        session: TmuxSession,
    },
    AdhocGroup {
        project_name: String,
        project_path: String,
        session_count: usize,
    },
    AdhocSession {
        project_name: String,
        project_path: String,
        session: TmuxSession,
    },
}

#[derive(Debug, PartialEq, Clone, Copy)]
pub enum InputMode {
    Normal,
    ContextMenu,
    AddProjectPath,
    AddProjectName,
    AddTaskName,
    AddTaskBranch,
    AddTaskPrompt,
    AddSessionName,
    AddSessionPrompt,
    AddAdhocSessionName,
    ConfirmDelete,
    MergeCommitMessage,
    ConfirmCreatePr,
    SetBaseBranch,
    Search,
    /// Fuzzy branch picker for project-level checkout.
    CheckoutBranch,
    /// Prompt for a project's run command (first use) before running it.
    RunCommand,
    /// Attach / Restart / Kill menu shown when an item's run session is live.
    RunMenu,
    /// Pick which of a task's sessions to forward difit review comments to,
    /// shown after a task review closes with comments and more than one session
    /// exists.
    ReviewSessionPicker,
    /// Pick the agent harness before an add-task / new-session flow.
    AgentPicker,
    /// Type a message to send to the selected session's agent (quick reply /
    /// answer to a question it asked).
    SendMessage,
}

/// What the agent picker feeds into once an agent is chosen.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum AgentPickerTarget {
    AddTask,
    NewSession,
}

#[derive(Debug, Clone)]
pub struct ContextMenuItem {
    pub key: char,
    pub label: &'static str,
    pub action: ContextAction,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum ContextAction {
    AddTask,
    /// Add task, picking the agent harness first.
    AddTaskWithAgent,
    NewSession,
    /// New worktree session, picking the agent harness first.
    NewSessionWithAgent,
    NewSessionNoWorktree,
    NewAdhocSession,
    Delete,
    Merge,
    Update,
    Push,
    OpenPr,
    Checkout,
    CopyWorktreePath,
    SetBaseBranch,
    Archive,
    Unarchive,
    Review,
    Terminal,
    /// Checkout a branch in the project dir via the fuzzy branch picker.
    CheckoutBranch,
    /// Copy the project's directory path to the clipboard.
    CopyProjectPath,
    /// Fetch all remotes and fast-forward the current branch for a project.
    FetchPull,
    /// Run the project's configured run command for the selected item.
    Run,
    /// Attach to the item's existing live run session.
    RunAttach,
    /// Restart the item's run session (kill + relaunch the run command).
    RunRestart,
    /// Kill the item's run session.
    RunKill,
    /// An agent chosen in the agent picker.
    PickAgent(AgentKind),
    /// Send a typed message to the selected session's agent.
    SendMessage,
    /// Approve the selected session's pending permission prompt (sends "y").
    Approve,
    /// Restart the selected session's agent (kill + relaunch, resuming the
    /// conversation).
    Restart,
}

/// Where a "Run" action should execute: the owning project (whose `run_command`
/// is read/saved), the working directory, and a label used to name the tmux run
/// session.
#[derive(Debug, Clone)]
pub struct RunContext {
    pub project_name: String,
    pub cwd: String,
    pub label: String,
}

/// Label identifying the run session for a list item (the basis of its tmux
/// run-session name). `None` for items that can't be run (e.g. adhoc groups).
/// Single source of truth so the launcher and the UI indicator agree.
pub fn run_label(item: &ListItem) -> Option<String> {
    match item {
        ListItem::Project { project } => Some(project.name.clone()),
        ListItem::Task {
            project_name, task, ..
        } => Some(format!("{project_name}-{}", task.name)),
        ListItem::Session { session, .. } | ListItem::AdhocSession { session, .. } => {
            Some(session.name.clone())
        }
        ListItem::AdhocGroup { .. } => None,
    }
}

/// True when a `difit` executable is found on `PATH`.
fn difit_on_path() -> bool {
    let Some(path) = std::env::var_os("PATH") else {
        return false;
    };
    std::env::split_paths(&path).any(|dir| dir.join("difit").is_file())
}

/// Build the command used to launch difit. Prefers a `difit` binary on `PATH`;
/// otherwise falls back to `npx -y difit` so review works without a global
/// install (npx fetches the package on first use).
fn difit_command(args: &[String]) -> std::process::Command {
    let mut cmd = if difit_on_path() {
        std::process::Command::new("difit")
    } else {
        let mut c = std::process::Command::new("npx");
        c.args(["-y", "difit"]);
        c
    };
    cmd.args(args);
    cmd
}

/// True when a `hunk` executable is found on `PATH`.
fn hunk_on_path() -> bool {
    let Some(path) = std::env::var_os("PATH") else {
        return false;
    };
    std::env::split_paths(&path).any(|dir| dir.join("hunk").is_file())
}

/// Build the command used to launch hunk. Prefers a `hunk` binary on `PATH`;
/// otherwise falls back to `npx -y hunkdiff` (hunk's npm package name) so review
/// works without a global install (npx fetches the package on first use).
pub fn hunk_command(args: &[String]) -> std::process::Command {
    let mut cmd = if hunk_on_path() {
        std::process::Command::new("hunk")
    } else {
        let mut c = std::process::Command::new("npx");
        c.args(["-y", "hunkdiff"]);
        c
    };
    cmd.args(args);
    cmd
}

/// A single inline review note from hunk's live session (`hunk session comment
/// list --json`). Only the fields needed to forward the note are deserialized.
#[derive(Clone, serde::Deserialize)]
struct HunkComment {
    #[serde(rename = "filePath")]
    file_path: Option<String>,
    body: Option<String>,
    /// `[start, end]` line range on the new side; absent for pure deletions.
    #[serde(rename = "newRange")]
    new_range: Option<Vec<i64>>,
    /// `[start, end]` line range on the old side.
    #[serde(rename = "oldRange")]
    old_range: Option<Vec<i64>>,
}

/// Query hunk's live session for the human's review comments (`--type user`).
/// Returns `None` on any failure (daemon down, session not yet registered, parse
/// error) so the poller simply keeps its previous snapshot.
fn query_hunk_user_comments(cwd: &str) -> Option<Vec<HunkComment>> {
    let args: Vec<String> = [
        "session", "comment", "list", "--repo", cwd, "--type", "user", "--json",
    ]
    .iter()
    .map(|s| s.to_string())
    .collect();
    // Null stdin so this background query can't contend for terminal input with
    // the foreground hunk TUI; `output()` already keeps stdout/stderr off-screen.
    let out = hunk_command(&args)
        .current_dir(cwd)
        .stdin(Stdio::null())
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    #[derive(serde::Deserialize)]
    struct List {
        comments: Vec<HunkComment>,
    }
    serde_json::from_slice::<List>(&out.stdout)
        .ok()
        .map(|l| l.comments)
}

/// Format hunk review comments into a raw comment block (`- file:line — body`
/// bullets). Wrapped by [`review_prompt`] before it's forwarded to the agent,
/// the same way difit's extracted comment block is.
fn format_hunk_comments(comments: &[HunkComment]) -> String {
    comments
        .iter()
        .map(|c| {
            let file = c.file_path.as_deref().unwrap_or("(unknown file)");
            let line = c
                .new_range
                .as_ref()
                .or(c.old_range.as_ref())
                .and_then(|r| r.first())
                .copied();
            let body = c.body.as_deref().unwrap_or("").trim();
            match line {
                Some(l) => format!("- {file}:{l} — {body}"),
                None => format!("- {file} — {body}"),
            }
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// Extract the review-comment block difit prints to stdout on exit. Returns
/// `None` when the session left no comments (difit prints nothing).
pub fn extract_difit_comments(stdout: &str) -> Option<String> {
    const MARKER: &str = "Comments from review session:";
    let idx = stdout.find(MARKER)?;
    // Back up to the start of the marker's line so the header is included.
    let line_start = stdout[..idx].rfind('\n').map(|n| n + 1).unwrap_or(0);
    Some(stdout[line_start..].trim_end().to_string())
}

/// A pending foreground hunk review: working dir, hunk CLI args, and the
/// candidate sessions its comments forward to (`(tmux name, display name)`).
type HunkReview = (String, Vec<String>, Vec<(String, String)>);

pub struct App {
    pub config: Config,
    pub keybindings: KeyBindings,
    pub sessions: Vec<TmuxSession>,
    pub items: Vec<ListItem>,
    pub selected: usize,
    pub input_mode: InputMode,
    pub input_buffer: String,
    pub use_worktree: bool,
    pub status_message: Option<String>,
    pub should_quit: bool,
    pub should_attach: Option<String>,
    /// Attach to a specific (session, window index) — used for terminals.
    pub should_attach_window: Option<(String, usize)>,
    /// Pending foreground hunk review: `(cwd, hunk args, candidate sessions)`.
    /// Set by the review action when the configured tool is `hunk`; the main loop
    /// suspends the TUI, runs hunk on the real terminal, then resumes. Unlike
    /// difit (browser, run in the background), hunk is a terminal TUI and needs
    /// the controlling terminal — so it can't run as a background op. While it
    /// runs, the review comments are polled from hunk's live session and, on
    /// exit, routed to the candidate sessions via the same picker difit uses.
    pub should_review_hunk: Option<HunkReview>,
    pub pending_project_path: Option<String>,
    pub pending_task_name: Option<String>,
    /// Target tmux session for an in-progress "Send message" prompt.
    pub pending_send_session: Option<String>,
    pub pending_task_branch: Option<String>,
    pub pending_session_name: Option<String>,
    /// Agent chosen in the agent picker, consumed by the next create flow.
    pub pending_agent: Option<AgentKind>,
    /// Flow the agent picker was opened for.
    pub agent_picker_target: Option<AgentPickerTarget>,
    pub collapsed: HashSet<String>,
    pub session_statuses: HashMap<String, SessionStatus>,
    pub diff_stats: HashMap<String, DiffStats>,
    pub session_branches: HashMap<String, String>,
    /// Agent harness id per session, keyed by tmux name (from the worker).
    pub session_agents: HashMap<String, String>,
    pub task_diff_stats: HashMap<String, DiffStats>,
    /// PR URLs keyed by branch name
    pub pr_urls: HashMap<String, String>,
    /// Current git branch for each project, keyed by project name
    pub project_branches: HashMap<String, String>,
    /// Last-seen modification time of config.toml, used to detect external edits.
    pub config_mtime: Option<std::time::SystemTime>,
    /// Number of in-flight async ops. UI stays interactive while ops run; the
    /// status bar shows a spinner when this is non-zero.
    pub op_count: usize,
    pub op_receiver: mpsc::Receiver<OpResult>,
    pub op_sender: mpsc::Sender<OpResult>,
    /// Channel carrying difit reviews that closed with comments and need the
    /// user to choose a target session (more than one candidate).
    pub review_receiver: mpsc::Receiver<PendingReview>,
    pub review_sender: mpsc::Sender<PendingReview>,
    /// Review comments awaiting forwarding once a session is picked.
    pub pending_review_comments: Option<String>,
    /// Candidate sessions shown in the picker as `(tmux name, display name)`.
    pub review_candidates: Vec<(String, String)>,
    /// Selected index into `review_candidates`.
    pub review_selected: usize,
    pub tick: usize,
    pub worker: Worker,
    pub context_menu_items: Vec<ContextMenuItem>,
    pub context_menu_selected: usize,
    /// When true, the task list shows only archived tasks instead of active ones.
    pub view_archived: bool,
    /// Active filter substring; tasks/projects/sessions are matched case-insensitively.
    pub search_query: String,
    /// Branches offered by the fuzzy checkout picker (project-level).
    pub branch_picker_all: Vec<String>,
    /// Project path the branch picker checks out into.
    pub branch_picker_project: String,
    /// Selected index into the *filtered* branch list.
    pub branch_picker_selected: usize,
    /// Context awaiting a run command entered via the `RunCommand` prompt.
    pub pending_run: Option<RunContext>,
    /// Live "Run" sessions keyed by tmux name; value true while the command is
    /// still executing. Populated by the background worker.
    pub run_sessions: HashMap<String, bool>,
    /// Per-session CPU/mem, keyed by tmux session name. Populated by the
    /// background worker (sampled every 8th tick).
    pub resources: HashMap<String, SessionResources>,
    /// Per-session GPU memory (MiB), computed on demand when the resource panel
    /// opens (not on the worker tick).
    pub gpu_by_session: HashMap<String, u64>,
    /// Whether the resource (CPU/mem/GPU) panel overlay is shown.
    pub show_resources: bool,
    /// Index into `theme::THEMES` of the active color theme.
    pub theme_index: usize,
    /// Screen row (relative to the list area top) of the selected item, recorded
    /// during rendering so popups can anchor to it. Interior-mutable since draw
    /// only borrows `&App`.
    pub selected_row: std::cell::Cell<u16>,
    /// Hostname of the machine cm is actually running on (resolved over SSH too).
    /// Shown in the dashboard header so it's clear which box a session lives on.
    pub hostname: String,
    /// First list row (absolute index into the rendered rows) currently scrolled
    /// into view. Persisted across frames so scrolling feels stable in both
    /// directions; updated during rendering to keep the selection visible.
    pub list_offset: std::cell::Cell<u16>,
}

pub struct OpResult {
    pub message: String,
    pub rebuild: bool,
    pub reload_config: bool,
}

/// A review that closed with comments still needing to be routed to a session.
/// Sent from the difit background thread to the main thread when the reviewed
/// task has more than one session, so the user can pick the target via a popup
/// (the single-session case is forwarded directly off-thread). hunk reuses the
/// same picker via [`App::open_review_session_picker`] from the main thread.
pub struct PendingReview {
    pub comments: String,
    /// Candidate sessions as `(tmux session name, display name)`.
    pub candidates: Vec<(String, String)>,
}

/// The prompt forwarded to an agent session carrying review comments (difit or
/// hunk); `comments` is the tool's raw comment block.
fn review_prompt(comments: &str) -> String {
    format!(
        "The following code review comments were left during review. \
         Please address them:\n\n{comments}"
    )
}

/// Modification time of config.toml, if it exists.
fn config_file_mtime() -> Option<std::time::SystemTime> {
    std::fs::metadata(Config::config_path())
        .ok()
        .and_then(|m| m.modified().ok())
}

/// Resolve the hostname of the machine cm is actually running on. When cm is
/// launched over SSH this is the remote box, not the user's laptop. Runs
/// `hostname` once at startup and falls back to the `HOSTNAME` env var, then to
/// "unknown". The short form (first dot-separated label) keeps the header tidy.
pub fn detect_hostname() -> String {
    let raw = std::process::Command::new("hostname")
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .filter(|s| !s.is_empty())
        .or_else(|| std::env::var("HOSTNAME").ok())
        .unwrap_or_else(|| "unknown".to_string());
    raw.split('.').next().unwrap_or(&raw).to_string()
}

fn project_key(name: &str) -> String {
    format!("p:{name}")
}

fn task_key(project: &str, task: &str) -> String {
    format!("t:{project}:{task}")
}

fn adhoc_group_key(project: &str) -> String {
    format!("a:{project}")
}

/// Expand a leading `~` to the user's home directory.
pub(crate) fn expand_tilde(path: &str) -> String {
    if let Some(rest) = path.strip_prefix('~') {
        if let Some(home) = dirs::home_dir() {
            return format!("{}{}", home.to_string_lossy(), rest);
        }
    }
    path.to_string()
}

/// Longest common prefix shared by all strings.
fn longest_common_prefix(items: &[String]) -> String {
    let first = match items.first() {
        Some(f) => f,
        None => return String::new(),
    };
    let mut len = first.chars().count();
    for item in &items[1..] {
        len = first
            .chars()
            .zip(item.chars())
            .take(len)
            .take_while(|(a, b)| a == b)
            .count();
    }
    first.chars().take(len).collect()
}

/// Tab-completion for a directory path: completes the final component to the
/// longest common prefix of matching subdirectories, preserving the typed
/// directory portion (including a leading `~`). Returns `None` if nothing matches.
fn complete_dir_path(input: &str) -> Option<String> {
    // Split the typed directory portion (kept verbatim) from the partial name.
    let (typed_dir, partial) = match input.rfind('/') {
        Some(i) => (&input[..=i], &input[i + 1..]),
        None => ("", input),
    };
    let listing_dir = match expand_tilde(typed_dir).as_str() {
        "" => ".".to_string(),
        d => d.to_string(),
    };

    let mut names: Vec<String> = std::fs::read_dir(&listing_dir)
        .ok()?
        .filter_map(|e| e.ok())
        .filter(|e| e.path().is_dir())
        .map(|e| e.file_name().to_string_lossy().to_string())
        .filter(|n| n.starts_with(partial))
        .collect();
    if names.is_empty() {
        return None;
    }
    names.sort();

    let mut completed = format!("{typed_dir}{}", longest_common_prefix(&names));
    // A single unambiguous match is a directory — append a slash to descend.
    if names.len() == 1 {
        completed.push('/');
    }
    Some(completed)
}

impl App {
    /// Agent new sessions run, from the config's `default_agent`.
    fn default_agent(&self) -> AgentKind {
        AgentKind::from_id(&self.config.default_agent).unwrap_or_default()
    }

    pub fn new() -> Result<Self> {
        let config = Config::load()?;
        let keybindings = KeyBindings::load();
        let mut sessions = tmux::list_sessions().unwrap_or_default();

        // Recreate any saved sessions that are no longer in tmux (e.g. tmux died)
        let saved = config::load_sessions();
        if !saved.is_empty() {
            let live_names: HashSet<_> = sessions.iter().map(|s| s.name.as_str()).collect();
            for (tmux_name, record) in &saved {
                if record.archived {
                    continue;
                }
                // Only act on records whose tmux session is gone. The decision
                // to recreate vs. prune is based on whether the task still
                // exists in config — NOT on tmux liveness — so legitimate
                // sessions are recovered after showrunner/tmux restarts,
                // while sessions whose task was deleted are reaped instead of
                // being resurrected on every startup.
                if live_names.contains(tmux_name.as_str()) {
                    continue;
                }

                if tmux::is_adhoc_marker(&record.task_name) {
                    // Adhoc sessions are project-scoped. Recreate while the
                    // project exists (unless auto-closed); otherwise the project
                    // is gone — prune. An auto-closed record with a live project
                    // is left for the user to restart from the TUI.
                    if !config.project_exists(&record.project_path) {
                        config::remove_session_record(tmux_name);
                    } else if !record.auto_closed
                        && tmux::recreate_adhoc_session(tmux_name, record).is_err()
                    {
                        config::remove_session_record(tmux_name);
                    }
                    continue;
                }

                // Task-scoped session: match by branch (+ project path) rather
                // than the display-name fields, which can drift when the config
                // is edited by hand.
                match config.find_task_by_branch(&record.project_path, &record.task_branch) {
                    Some(_) => {
                        // Recreate, unless the daemon auto-closed it (leave that
                        // record for the user to restart from the TUI).
                        if !record.auto_closed
                            && tmux::recreate_session(tmux_name, record).is_err()
                        {
                            // Could not recreate (e.g. worktree gone) — remove stale record
                            config::remove_session_record(tmux_name);
                        }
                    }
                    None => {
                        // The task no longer exists in config. Reap the orphan
                        // (worktree + cached context + record) so it isn't
                        // resurrected on every startup. The git branch is kept,
                        // preserving any committed work. (Reaped even if
                        // auto-closed: an auto-closed session whose task was
                        // deleted is just a stale record.)
                        tmux::cleanup_orphan_session(record);
                        config::remove_session_record(tmux_name);
                    }
                }
            }
            // Re-list sessions after recreation
            sessions = tmux::list_sessions().unwrap_or_default();
        }
        let (tx, rx) = mpsc::channel();
        let (review_tx, review_rx) = mpsc::channel();
        // Prefer the daemon's worker via SSE (single source of truth for TUI +
        // web + mobile); fall back to a local worker when the daemon isn't
        // running (standalone TUI use).
        let worker = if Worker::daemon_reachable() {
            Worker::connect_remote(&Worker::daemon_url())
        } else {
            Worker::spawn()
        };
        let mut app = App {
            config,
            keybindings,
            sessions,
            items: vec![],
            selected: 0,
            input_mode: InputMode::Normal,
            input_buffer: String::new(),
            use_worktree: true,
            status_message: None,
            should_quit: false,
            should_attach: None,
            should_attach_window: None,
            should_review_hunk: None,
            pending_project_path: None,
            pending_task_name: None,
            pending_send_session: None,
            pending_task_branch: None,
            pending_session_name: None,
            pending_agent: None,
            agent_picker_target: None,
            collapsed: HashSet::new(),
            session_statuses: HashMap::new(),
            diff_stats: HashMap::new(),
            session_branches: HashMap::new(),
            session_agents: HashMap::new(),
            task_diff_stats: HashMap::new(),
            pr_urls: HashMap::new(),
            project_branches: HashMap::new(),
            config_mtime: config_file_mtime(),
            op_count: 0,
            op_receiver: rx,
            op_sender: tx,
            review_receiver: review_rx,
            review_sender: review_tx,
            pending_review_comments: None,
            review_candidates: Vec::new(),
            review_selected: 0,
            tick: 0,
            worker,
            context_menu_items: vec![],
            context_menu_selected: 0,
            view_archived: false,
            search_query: String::new(),
            branch_picker_all: Vec::new(),
            branch_picker_project: String::new(),
            branch_picker_selected: 0,
            pending_run: None,
            run_sessions: HashMap::new(),
            resources: HashMap::new(),
            gpu_by_session: HashMap::new(),
            show_resources: false,
            theme_index: config::load_theme()
                .map(|n| crate::theme::by_name(&n))
                .unwrap_or(0),
            selected_row: std::cell::Cell::new(0),
            hostname: detect_hostname(),
            list_offset: std::cell::Cell::new(0),
        };
        // Start with all tasks collapsed, and projects with no tasks collapsed
        for project in &app.config.projects {
            if project.tasks.is_empty() {
                app.collapsed.insert(project_key(&project.name));
            }
            for task in &project.tasks {
                app.collapsed.insert(task_key(&project.name, &task.name));
            }
        }
        app.rebuild_items();
        app.check_cwd();
        Ok(app)
    }

    fn check_cwd(&mut self) {
        if let Ok(cwd) = std::env::current_dir() {
            let cwd_str = cwd.to_string_lossy().to_string();
            if cwd.join(".git").is_dir() && !self.config.has_project_at(&cwd_str) {
                self.pending_project_path = Some(cwd_str);
            }
        }
    }

    /// Apply any pending updates from the background worker.
    pub fn apply_worker_updates(&mut self) {
        let latest = self.worker.latest.lock().unwrap().take();
        if let Some(update) = latest {
            self.sessions = update.sessions;
            self.session_statuses = update.statuses;
            self.diff_stats = update.diff_stats;
            if !update.session_agents.is_empty() {
                self.session_agents = update.session_agents;
            }
            if !update.session_branches.is_empty() {
                self.session_branches = update.session_branches;
            }
            if !update.task_diff_stats.is_empty() {
                self.task_diff_stats = update.task_diff_stats;
            }
            if !update.pr_urls.is_empty() {
                self.pr_urls.extend(update.pr_urls);
            }
            if !update.project_branches.is_empty() {
                self.project_branches = update.project_branches;
            }
            self.run_sessions = update.run_sessions;
            self.resources = update.resources;
            self.rebuild_items();
        }
    }

    /// Poll for completed background operations.
    pub fn apply_op_results(&mut self) {
        while let Ok(result) = self.op_receiver.try_recv() {
            self.op_count = self.op_count.saturating_sub(1);
            self.status_message = Some(result.message);
            if result.reload_config {
                if let Ok(config) = Config::load() {
                    self.config = config;
                }
            }
            if result.rebuild {
                self.rebuild_items();
            }
        }
    }

    /// Pick up difit reviews that closed with comments and need a target
    /// session chosen. One is handled at a time; while the picker is open the
    /// rest stay queued in the channel until it closes.
    pub fn apply_review_requests(&mut self) {
        if self.input_mode == InputMode::ReviewSessionPicker {
            return;
        }
        if let Ok(req) = self.review_receiver.try_recv() {
            self.open_review_session_picker(req);
        }
    }

    /// Open the popup that asks which session a review's comments go to. The
    /// background thread only sends here when there is more than one candidate,
    /// but guard the degenerate cases too in case that ever changes.
    fn open_review_session_picker(&mut self, req: PendingReview) {
        match req.candidates.len() {
            0 => {
                self.status_message =
                    Some("Review finished; no session to forward comments to".into());
            }
            1 => {
                let (tmux_name, display) = req.candidates[0].clone();
                self.forward_review_comments(&req.comments, &tmux_name, &display);
            }
            _ => {
                self.pending_review_comments = Some(req.comments);
                self.review_candidates = req.candidates;
                self.review_selected = 0;
                self.input_mode = InputMode::ReviewSessionPicker;
            }
        }
    }

    /// Send the held review comments to the picked session, then close the
    /// picker. A no-op (closes the picker) if state is missing.
    pub fn confirm_review_session(&mut self) {
        let comments = self.pending_review_comments.take();
        let target = self.review_candidates.get(self.review_selected).cloned();
        self.reset_review_picker();
        match (comments, target) {
            (Some(comments), Some((tmux_name, display))) => {
                self.forward_review_comments(&comments, &tmux_name, &display);
            }
            _ => self.status_message = Some("No session to forward comments to".into()),
        }
    }

    /// Forward review comments to a session as a new prompt, reporting the
    /// outcome in the status bar.
    fn forward_review_comments(&mut self, comments: &str, tmux_name: &str, display: &str) {
        self.status_message = Some(
            match tmux::send_text(tmux_name, &review_prompt(comments), true) {
                Ok(()) => format!("Forwarded review comments to {display}"),
                Err(e) => format!("Failed to forward comments: {e}"),
            },
        );
    }

    pub fn cancel_review_picker(&mut self) {
        self.reset_review_picker();
        self.status_message = Some("Review comments discarded".into());
    }

    /// Clear picker state and return to Normal mode without touching the status.
    fn reset_review_picker(&mut self) {
        self.input_mode = InputMode::Normal;
        self.pending_review_comments = None;
        self.review_candidates.clear();
        self.review_selected = 0;
    }

    pub fn review_picker_move_up(&mut self) {
        if self.review_selected > 0 {
            self.review_selected -= 1;
        }
    }

    pub fn review_picker_move_down(&mut self) {
        if self.review_selected + 1 < self.review_candidates.len() {
            self.review_selected += 1;
        }
    }

    /// Pick up config edits made by another process. Only reloads when idle (no
    /// in-flight op, Normal mode) and the on-disk content actually differs from
    /// memory — so the app's own saves never trigger a spurious rebuild.
    pub fn maybe_reload_config(&mut self) {
        if self.op_count != 0 || self.input_mode != InputMode::Normal {
            return;
        }
        let mtime = config_file_mtime();
        if mtime == self.config_mtime {
            return;
        }
        self.config_mtime = mtime;
        let Ok(disk) = std::fs::read_to_string(Config::config_path()) else {
            return;
        };
        // Skip if disk matches our in-memory state (our own write).
        if toml::to_string_pretty(&self.config)
            .map(|s| s == disk)
            .unwrap_or(false)
        {
            return;
        }
        if let Ok(cfg) = toml::from_str::<Config>(&disk) {
            self.config = cfg;
            self.rebuild_items();
            self.sync_worker_hints();
        }
    }

    fn start_op<F>(&mut self, loading_msg: &str, f: F)
    where
        F: FnOnce() -> OpResult + Send + 'static,
    {
        self.op_count += 1;
        self.status_message = Some(loading_msg.into());
        let tx = self.op_sender.clone();
        thread::spawn(move || {
            let result = f();
            let _ = tx.send(result);
        });
    }

    /// Tell the worker what is selected.
    pub fn sync_worker_hints(&self) {
        // Dedup so the worker never computes the same branch diff / PR lookup
        // twice: branch names can collide across projects, and project names are
        // the identity of a project_path entry. No-op on well-formed configs.
        let mut seen_tasks: HashSet<(String, String)> = HashSet::new();
        let mut seen_projects: HashSet<String> = HashSet::new();
        let mut tasks: Vec<TaskInfo> = Vec::new();
        let mut project_paths: Vec<(String, String)> = Vec::new();
        for p in &self.config.projects {
            if seen_projects.insert(p.name.clone()) {
                project_paths.push((p.name.clone(), p.path.clone()));
            }
            for t in &p.tasks {
                if seen_tasks.insert((p.path.clone(), t.branch.clone())) {
                    tasks.push(TaskInfo {
                        project_name: p.name.clone(),
                        project_path: p.path.clone(),
                        branch: t.branch.clone(),
                        base_branch: t.base_branch().to_string(),
                    });
                }
            }
        }

        if let Ok(mut hints) = self.worker.hints.lock() {
            hints.tasks = tasks;
            hints.project_paths = project_paths;
        }
    }

    pub fn rebuild_items(&mut self) {
        self.items.clear();
        let needle = self.search_query.to_lowercase();
        let needle = needle.trim();
        let want_archived = self.view_archived;
        for project in &self.config.projects {
            // Determine which tasks of this project match the current view + filter.
            // Stack-ordered so chained tasks sit together instead of in creation order.
            let visible_tasks: Vec<&Task> = project
                .tasks_stack_ordered()
                .into_iter()
                .filter(|t| t.archived == want_archived)
                .filter(|t| {
                    needle.is_empty()
                        || project.name.to_lowercase().contains(needle)
                        || t.name.to_lowercase().contains(needle)
                        || t.branch.to_lowercase().contains(needle)
                })
                .collect();

            // Hide the project entirely when filtering and nothing matches under it.
            // Without a filter we still show empty projects so the user can add tasks.
            if !needle.is_empty() && visible_tasks.is_empty() {
                continue;
            }

            self.items.push(ListItem::Project {
                project: project.clone(),
            });

            if self.collapsed.contains(&project_key(&project.name)) {
                continue;
            }

            // Adhoc group: only rendered when the project has at least one adhoc session.
            let adhoc_sessions = tmux::adhoc_sessions_for_project(&project.name, &self.sessions);
            if !adhoc_sessions.is_empty() {
                self.items.push(ListItem::AdhocGroup {
                    project_name: project.name.clone(),
                    project_path: project.path.clone(),
                    session_count: adhoc_sessions.len(),
                });
                if !self.collapsed.contains(&adhoc_group_key(&project.name)) {
                    for session in adhoc_sessions {
                        self.items.push(ListItem::AdhocSession {
                            project_name: project.name.clone(),
                            project_path: project.path.clone(),
                            session,
                        });
                    }
                }
            }

            for task in visible_tasks {
                self.items.push(ListItem::Task {
                    project_name: project.name.clone(),
                    project_path: project.path.clone(),
                    task: task.clone(),
                });

                if self
                    .collapsed
                    .contains(&task_key(&project.name, &task.name))
                {
                    continue;
                }

                // Archived tasks have no live tmux sessions; skip session rendering.
                if task.archived {
                    continue;
                }

                for session in tmux::sessions_for_task(&project.name, &task.name, &self.sessions) {
                    self.items.push(ListItem::Session {
                        project_name: project.name.clone(),
                        project_path: project.path.clone(),
                        task: task.clone(),
                        session,
                    });
                }
            }
        }
        if self.selected >= self.items.len() && !self.items.is_empty() {
            self.selected = self.items.len() - 1;
        }
    }

    pub fn selected_item(&self) -> Option<&ListItem> {
        self.items.get(self.selected)
    }

    /// Get the project context for the currently selected item.
    fn selected_project_info(&self) -> Option<(&str, &str)> {
        match self.selected_item()? {
            ListItem::Project { project } => Some((&project.name, &project.path)),
            ListItem::Task {
                project_name,
                project_path,
                ..
            } => Some((project_name, project_path)),
            ListItem::Session {
                project_name,
                project_path,
                ..
            } => Some((project_name, project_path)),
            ListItem::AdhocGroup {
                project_name,
                project_path,
                ..
            } => Some((project_name, project_path)),
            ListItem::AdhocSession {
                project_name,
                project_path,
                ..
            } => Some((project_name, project_path)),
        }
    }

    /// Get the project/task info for the currently selected item.
    fn selected_task_info(&self) -> Option<(&str, &str, &Task)> {
        match self.selected_item()? {
            ListItem::Task {
                project_name,
                project_path,
                task,
            } => Some((project_name, project_path, task)),
            ListItem::Session {
                project_name,
                project_path,
                task,
                ..
            } => Some((project_name, project_path, task)),
            _ => None,
        }
    }

    pub fn move_up(&mut self) {
        if self.selected > 0 {
            self.selected -= 1;
            self.on_selection_changed();
        }
    }

    pub fn move_down(&mut self) {
        if self.selected + 1 < self.items.len() {
            self.selected += 1;
            self.on_selection_changed();
        }
    }

    fn on_selection_changed(&mut self) {
        self.sync_worker_hints();
    }

    pub fn toggle_collapse(&mut self) {
        match self.selected_item() {
            Some(ListItem::Project { project }) => {
                let key = project_key(&project.name);
                if !self.collapsed.remove(&key) {
                    self.collapsed.insert(key);
                }
                self.rebuild_items();
            }
            Some(ListItem::Task {
                project_name, task, ..
            }) => {
                let key = task_key(project_name, &task.name);
                if !self.collapsed.remove(&key) {
                    self.collapsed.insert(key);
                }
                self.rebuild_items();
            }
            Some(ListItem::AdhocGroup { project_name, .. }) => {
                let key = adhoc_group_key(project_name);
                if !self.collapsed.remove(&key) {
                    self.collapsed.insert(key);
                }
                self.rebuild_items();
            }
            _ => {}
        }
    }

    pub fn enter_selected(&mut self) {
        match self.selected_item() {
            // Enter on a session attaches to it.
            Some(ListItem::Session { session, .. })
            | Some(ListItem::AdhocSession { session, .. }) => {
                self.should_attach = Some(session.name.clone());
            }
            // Enter on a collapsible item (project/task/adhoc group) toggles it.
            _ => self.toggle_collapse(),
        }
    }

    /// Label for the review context-menu item, reflecting the configured tool.
    fn review_label(&self) -> &'static str {
        match self.config.review_tool {
            ReviewTool::Difit => "Review diff (difit)",
            ReviewTool::Hunk => "Review diff (hunk)",
        }
    }

    pub fn open_context_menu(&mut self) {
        let cm = self.keybindings.context_menu_keys.clone();
        let review_label = self.review_label();
        let items = match self.selected_item() {
            Some(ListItem::Project { .. }) => vec![
                ContextMenuItem {
                    key: cm.add_task,
                    label: "Add task",
                    action: ContextAction::AddTask,
                },
                ContextMenuItem {
                    key: cm.add_task_with_agent,
                    label: "Add task (choose agent)",
                    action: ContextAction::AddTaskWithAgent,
                },
                ContextMenuItem {
                    key: cm.new_adhoc_session,
                    label: "New adhoc session",
                    action: ContextAction::NewAdhocSession,
                },
                ContextMenuItem {
                    key: cm.run,
                    label: "Run",
                    action: ContextAction::Run,
                },
                ContextMenuItem {
                    key: cm.checkout,
                    label: "Checkout branch",
                    action: ContextAction::CheckoutBranch,
                },
                ContextMenuItem {
                    key: cm.fetch_pull,
                    label: "Fetch & pull all branches",
                    action: ContextAction::FetchPull,
                },
                ContextMenuItem {
                    key: cm.copy_path,
                    label: "Copy path",
                    action: ContextAction::CopyProjectPath,
                },
                ContextMenuItem {
                    key: cm.delete,
                    label: "Delete",
                    action: ContextAction::Delete,
                },
            ],
            Some(ListItem::AdhocGroup { .. }) => vec![ContextMenuItem {
                key: cm.new_adhoc_session,
                label: "New adhoc session",
                action: ContextAction::NewAdhocSession,
            }],
            Some(ListItem::AdhocSession { .. }) => vec![
                ContextMenuItem {
                    key: cm.run,
                    label: "Run",
                    action: ContextAction::Run,
                },
                ContextMenuItem {
                    key: cm.delete,
                    label: "Delete",
                    action: ContextAction::Delete,
                },
            ],
            Some(ListItem::Task { task, .. }) => {
                if task.archived {
                    vec![
                        ContextMenuItem {
                            key: cm.archive,
                            label: "Unarchive",
                            action: ContextAction::Unarchive,
                        },
                        ContextMenuItem {
                            key: cm.delete,
                            label: "Delete",
                            action: ContextAction::Delete,
                        },
                    ]
                } else {
                    let mut items = vec![
                        ContextMenuItem {
                            key: cm.new_session,
                            label: "New session",
                            action: ContextAction::NewSession,
                        },
                        ContextMenuItem {
                            key: cm.new_session_no_worktree,
                            label: "New session (no worktree)",
                            action: ContextAction::NewSessionNoWorktree,
                        },
                        ContextMenuItem {
                            key: cm.new_session_with_agent,
                            label: "New session (choose agent)",
                            action: ContextAction::NewSessionWithAgent,
                        },
                        ContextMenuItem {
                            key: cm.review,
                            label: review_label,
                            action: ContextAction::Review,
                        },
                        ContextMenuItem {
                            key: cm.run,
                            label: "Run",
                            action: ContextAction::Run,
                        },
                        ContextMenuItem {
                            key: cm.update,
                            label: "Update branch",
                            action: ContextAction::Update,
                        },
                        ContextMenuItem {
                            key: cm.set_base_branch,
                            label: "Set base branch",
                            action: ContextAction::SetBaseBranch,
                        },
                        ContextMenuItem {
                            key: cm.push,
                            label: "Push",
                            action: ContextAction::Push,
                        },
                        ContextMenuItem {
                            key: cm.checkout,
                            label: "Checkout",
                            action: ContextAction::Checkout,
                        },
                        ContextMenuItem {
                            key: cm.open_pr,
                            label: "Open PR",
                            action: ContextAction::OpenPr,
                        },
                    ];
                    items.extend([
                        ContextMenuItem {
                            key: cm.archive,
                            label: "Archive",
                            action: ContextAction::Archive,
                        },
                        ContextMenuItem {
                            key: cm.delete,
                            label: "Delete",
                            action: ContextAction::Delete,
                        },
                    ]);
                    items
                }
            }
            Some(ListItem::Session { session, .. }) => {
                // The main session is on the task branch itself, so merging into
                // it / rebasing onto it are no-ops, and it can't be deleted.
                let is_main = tmux::is_main_session(&session.session_name);
                let mut items = vec![ContextMenuItem {
                    key: cm.review,
                    label: review_label,
                    action: ContextAction::Review,
                }];
                if !is_main {
                    items.extend([
                        ContextMenuItem {
                            key: cm.merge,
                            label: "Merge",
                            action: ContextAction::Merge,
                        },
                        ContextMenuItem {
                            key: cm.update,
                            label: "Update",
                            action: ContextAction::Update,
                        },
                    ]);
                }
                items.extend([
                    ContextMenuItem {
                        key: cm.send_message,
                        label: "Send message",
                        action: ContextAction::SendMessage,
                    },
                    ContextMenuItem {
                        key: cm.approve,
                        label: "Approve (y)",
                        action: ContextAction::Approve,
                    },
                    ContextMenuItem {
                        key: cm.restart,
                        label: "Restart (resumes)",
                        action: ContextAction::Restart,
                    },
                    ContextMenuItem {
                        key: cm.terminal,
                        label: "Terminal",
                        action: ContextAction::Terminal,
                    },
                    ContextMenuItem {
                        key: cm.run,
                        label: "Run",
                        action: ContextAction::Run,
                    },
                    ContextMenuItem {
                        key: cm.copy_path,
                        label: "Copy worktree path",
                        action: ContextAction::CopyWorktreePath,
                    },
                ]);
                if !is_main {
                    items.push(ContextMenuItem {
                        key: cm.delete,
                        label: "Delete",
                        action: ContextAction::Delete,
                    });
                }
                items
            }
            None => return,
        };
        self.context_menu_items = items;
        self.context_menu_selected = 0;
        self.input_mode = InputMode::ContextMenu;
    }

    pub fn execute_context_action(&mut self, action: ContextAction) {
        self.input_mode = InputMode::Normal;
        // A stale picked agent must never leak into a later create flow.
        if !matches!(action, ContextAction::PickAgent(_)) {
            self.pending_agent = None;
        }
        match action {
            ContextAction::AddTask => self.start_add_task(),
            ContextAction::AddTaskWithAgent => self.open_agent_picker(AgentPickerTarget::AddTask),
            ContextAction::NewSession => self.start_new_session(true),
            ContextAction::NewSessionWithAgent => {
                self.open_agent_picker(AgentPickerTarget::NewSession)
            }
            ContextAction::NewSessionNoWorktree => self.start_new_session(false),
            ContextAction::NewAdhocSession => self.start_new_adhoc_session(),
            ContextAction::Delete => self.start_delete(),
            ContextAction::Merge => self.start_merge(),
            ContextAction::Update => self.update_session(),
            ContextAction::Push => self.push_task_branch(),
            ContextAction::OpenPr => self.open_pr(),
            ContextAction::Checkout => self.checkout_task_branch(),
            ContextAction::CopyWorktreePath => self.copy_worktree_path(),
            ContextAction::SetBaseBranch => self.start_set_base_branch(),
            ContextAction::Archive => self.archive_task(),
            ContextAction::Unarchive => self.unarchive_task(),
            ContextAction::Review => self.start_review(),
            ContextAction::Terminal => self.open_terminal(),
            ContextAction::CheckoutBranch => self.start_checkout_branch(),
            ContextAction::CopyProjectPath => self.copy_project_path(),
            ContextAction::FetchPull => self.fetch_pull_all_branches(),
            ContextAction::Run => self.start_run(),
            ContextAction::RunAttach => self.run_attach(),
            ContextAction::RunRestart => self.run_restart(),
            ContextAction::RunKill => self.run_kill(),
            ContextAction::SendMessage => self.start_send_message(),
            ContextAction::Approve => self.approve_session(),
            ContextAction::Restart => self.restart_session(),
            ContextAction::PickAgent(agent) => self.confirm_agent_picker(agent),
        }
    }

    /// Open a floating picker listing the available agent harnesses; the
    /// chosen one feeds into `target`'s create flow.
    fn open_agent_picker(&mut self, target: AgentPickerTarget) {
        let keys = ['1', '2', '3', '4', '5', '6', '7', '8', '9'];
        self.context_menu_items = AgentKind::ALL
            .iter()
            .zip(keys)
            .map(|(agent, key)| ContextMenuItem {
                key,
                label: agent.label(),
                action: ContextAction::PickAgent(*agent),
            })
            .collect();
        self.context_menu_selected = AgentKind::ALL
            .iter()
            .position(|a| *a == self.default_agent())
            .unwrap_or(0);
        self.agent_picker_target = Some(target);
        self.input_mode = InputMode::AgentPicker;
    }

    fn confirm_agent_picker(&mut self, agent: AgentKind) {
        self.pending_agent = Some(agent);
        match self.agent_picker_target.take() {
            Some(AgentPickerTarget::AddTask) => self.start_add_task(),
            Some(AgentPickerTarget::NewSession) => self.start_new_session(true),
            None => self.pending_agent = None,
        }
    }

    pub fn archive_task(&mut self) {
        let (project_name, task_name) = match self.selected_task_info() {
            Some((pn, _, t)) => (pn.to_string(), t.name.clone()),
            None => {
                self.status_message = Some("Select a task to archive".into());
                return;
            }
        };

        let task_sessions = tmux::sessions_for_task(&project_name, &task_name, &self.sessions);
        let live_names: Vec<String> = task_sessions.iter().map(|s| s.name.clone()).collect();
        let session_count = live_names.len();

        // Persist archived state on the task and its session records.
        self.config.reload();
        self.config
            .set_task_archived(&project_name, &task_name, true);
        let _ = self.config.save();
        config::set_task_session_records_archived(&project_name, &task_name, true);

        // Collapse so the archived task hides cleanly when the user toggles back to active view.
        self.collapsed.insert(task_key(&project_name, &task_name));

        self.start_op("Archiving task...", move || {
            for name in &live_names {
                let _ = tmux::kill_session_only(name);
            }
            OpResult {
                message: format!(
                    "Archived task '{task_name}' ({} session{} suspended)",
                    session_count,
                    if session_count == 1 { "" } else { "s" }
                ),
                rebuild: true,
                reload_config: true,
            }
        });
    }

    pub fn unarchive_task(&mut self) {
        let (project_name, task_name, task_branch) = match self.selected_task_info() {
            Some((pn, _, t)) => (pn.to_string(), t.name.clone(), t.branch.clone()),
            None => {
                self.status_message = Some("Select a task to unarchive".into());
                return;
            }
        };

        self.config.reload();
        self.config
            .set_task_archived(&project_name, &task_name, false);
        let _ = self.config.save();
        config::set_task_session_records_archived(&project_name, &task_name, false);

        let _ = task_branch;
        // Switch back to the active view so the unarchived task is visible.
        if self.view_archived {
            self.view_archived = false;
        }

        self.start_op("Unarchiving task...", move || {
            let records = config::load_sessions();
            let mut recreated = 0;
            let mut failed = 0;
            for (tmux_name, record) in &records {
                if record.project_name == project_name && record.task_name == task_name {
                    match tmux::recreate_session(tmux_name, record) {
                        Ok(_) => {
                            // Unarchiving re-activates the task: clear any
                            // auto-closed marker so the revived session is treated
                            // as a normal live session.
                            config::set_session_auto_closed(tmux_name, false);
                            recreated += 1;
                        }
                        Err(_) => {
                            failed += 1;
                            // Stale record (e.g. worktree removed externally) — drop it.
                            config::remove_session_record(tmux_name);
                        }
                    }
                }
            }
            let msg = if failed > 0 {
                format!(
                    "Unarchived '{task_name}' — {recreated} session(s) restored, {failed} dropped"
                )
            } else {
                format!("Unarchived '{task_name}' — {recreated} session(s) restored")
            };
            OpResult {
                message: msg,
                rebuild: true,
                reload_config: true,
            }
        });
    }

    pub fn toggle_archive_view(&mut self) {
        self.view_archived = !self.view_archived;
        self.search_query.clear();
        self.selected = 0;
        self.rebuild_items();
        self.status_message = Some(if self.view_archived {
            "Showing archived tasks".into()
        } else {
            "Showing active tasks".into()
        });
        self.sync_worker_hints();
    }

    pub fn cycle_theme(&mut self) {
        self.theme_index = (self.theme_index + 1) % crate::theme::THEMES.len();
        let name = crate::theme::THEMES[self.theme_index].name;
        crate::config::save_theme(name);
        self.status_message = Some(format!("Theme: {name}"));
    }

    /// Launch the configured review tool on a task (branch vs base) or a session
    /// (uncommitted changes). difit runs in the background so the TUI stays
    /// interactive; hunk is a terminal TUI, so it's deferred to the main loop
    /// which suspends the TUI and runs it on the real terminal. Both forward any
    /// review comments to the agent session on exit.
    pub fn start_review(&mut self) {
        // (cwd, difit args, hunk args, candidate sessions, description). Each
        // candidate is `(tmux session name, display name)`; comments forward to it.
        let (cwd, difit_args, hunk_args, candidates, description) =
            match self.selected_item().cloned() {
                Some(ListItem::Task {
                    project_name,
                    project_path,
                    task,
                }) => {
                    let base = task
                        .base_branch
                        .clone()
                        .unwrap_or_else(|| "main".to_string());
                    let base_ref = tmux::resolve_base_ref(&project_path, &base);
                    // All of the task's sessions are candidates: a lone one is
                    // forwarded to directly, several trigger the picker popup.
                    let candidates =
                        tmux::sessions_for_task(&project_name, &task.name, &self.sessions)
                            .into_iter()
                            .map(|s| (s.name, s.session_name))
                            .collect();
                    // difit: `difit <target> <base> --merge-base` resolves the base
                    // to merge-base(branch, base) so we see only the branch's changes
                    // — the GitHub PR diff, excluding main's commits since the fork
                    // point. hunk: the three-dot range `<base>...<branch>` is git's
                    // equivalent merge-base diff, passed straight through to git.
                    (
                        project_path,
                        vec![
                            task.branch.clone(),
                            base_ref.clone(),
                            "--merge-base".to_string(),
                        ],
                        vec!["diff".to_string(), format!("{base_ref}...{}", task.branch)],
                        candidates,
                        format!("{} vs {base}", task.branch),
                    )
                }
                Some(ListItem::Session {
                    project_path,
                    session,
                    ..
                }) => {
                    let cwd = session
                        .worktree_path()
                        .map(|p| p.to_string_lossy().to_string())
                        .unwrap_or(project_path);
                    // hunk's `diff` shows uncommitted working-tree changes and
                    // includes untracked files by default.
                    (
                        cwd,
                        vec![".".to_string(), "--include-untracked".to_string()],
                        vec!["diff".to_string()],
                        vec![(session.name.clone(), session.session_name.clone())],
                        format!("uncommitted changes in {}", session.session_name),
                    )
                }
                _ => {
                    self.status_message = Some("Select a task or session to review".into());
                    return;
                }
            };

        if let ReviewTool::Hunk = self.config.review_tool {
            // hunk needs the controlling terminal; hand it to the main loop,
            // along with the candidate sessions its comments forward to.
            self.status_message = Some(format!("Reviewing {description} in hunk…"));
            self.should_review_hunk = Some((cwd, hunk_args, candidates));
            return;
        }

        let review_tx = self.review_sender.clone();
        self.start_op(&format!("Reviewing {description} in difit…"), move || {
            // `.output()` captures difit's stdout/stderr (keeping them off the
            // TUI) and blocks this background thread until the browser closes.
            let message = match difit_command(&difit_args).current_dir(&cwd).output() {
                Err(e) => format!("difit failed to launch (need difit or npx on PATH): {e}"),
                Ok(out) => {
                    let stdout = String::from_utf8_lossy(&out.stdout);
                    match extract_difit_comments(&stdout) {
                        Some(comments) => match candidates.len() {
                            0 => "Review finished; no session to forward comments to".into(),
                            // Exactly one candidate: forward straight away.
                            1 => {
                                let (tmux_name, display) = &candidates[0];
                                match tmux::send_text(tmux_name, &review_prompt(&comments), true) {
                                    Ok(()) => format!("Forwarded review comments to {display}"),
                                    Err(e) => format!("Failed to forward comments: {e}"),
                                }
                            }
                            // Several candidates: hand off to the main thread so
                            // the user can pick the target via a popup.
                            _ => {
                                let _ = review_tx.send(PendingReview {
                                    comments,
                                    candidates,
                                });
                                "Review closed; select a session to forward comments to".into()
                            }
                        },
                        None => "Review closed with no comments".into(),
                    }
                }
            };
            OpResult {
                message,
                rebuild: false,
                reload_config: false,
            }
        });
    }

    /// Run hunk in the foreground (the TUI is already suspended by the main loop)
    /// and route the human's review comments to a `candidates` session on exit,
    /// reusing the same picker difit uses (direct forward for one candidate, a
    /// popup for several).
    ///
    /// hunk's review session is live-only — its comments are queryable via the
    /// session daemon while it runs but vanish the instant it exits, and hunk has
    /// no exit-time comment dump (unlike difit). So a background thread polls the
    /// live session and keeps the latest snapshot; once hunk exits we forward that
    /// snapshot. A comment added in the final fraction of a second before quitting
    /// may be missed — that's the best a poll-based capture can do.
    pub fn run_hunk_review(
        &mut self,
        cwd: String,
        args: Vec<String>,
        candidates: Vec<(String, String)>,
    ) {
        let stop = Arc::new(AtomicBool::new(false));
        let latest: Arc<Mutex<Vec<HunkComment>>> = Arc::new(Mutex::new(Vec::new()));

        let poller = {
            let stop = stop.clone();
            let latest = latest.clone();
            let cwd = cwd.clone();
            thread::spawn(move || {
                while !stop.load(Ordering::Relaxed) {
                    if let Some(comments) = query_hunk_user_comments(&cwd) {
                        *latest.lock().unwrap() = comments;
                    }
                    // Sleep in short slices so we stop promptly when hunk exits.
                    for _ in 0..3 {
                        if stop.load(Ordering::Relaxed) {
                            break;
                        }
                        thread::sleep(Duration::from_millis(100));
                    }
                }
            })
        };

        let status = hunk_command(&args).current_dir(&cwd).status();
        stop.store(true, Ordering::Relaxed);
        let _ = poller.join();

        match status {
            Err(e) => {
                self.status_message = Some(format!(
                    "hunk failed to launch (need hunk or npx on PATH): {e}"
                ));
            }
            Ok(_) => {
                let comments = latest.lock().unwrap().clone();
                if comments.is_empty() {
                    self.status_message = Some("Review closed with no comments".into());
                } else {
                    // Route through the shared picker: forwarded directly for a
                    // lone candidate, popup for several (shown when the TUI
                    // resumes), status message for none.
                    self.open_review_session_picker(PendingReview {
                        comments: format_hunk_comments(&comments),
                        candidates,
                    });
                }
            }
        }
    }

    /// Resolve the "Run" context for the selected item: the owning project plus
    /// the working directory and a label for the tmux run session. Sets a status
    /// message and returns `None` when the selection can't be run.
    fn resolve_run_context(&mut self) -> Option<RunContext> {
        let item = self.selected_item().cloned()?;
        let Some(label) = run_label(&item) else {
            self.status_message = Some("Select a project, task, or session to run".into());
            return None;
        };
        let (project_name, cwd) = match item {
            ListItem::Project { project } => (project.name, project.path),
            ListItem::Task {
                project_name,
                project_path,
                task,
            } => {
                // Prefer a session worktree so the command runs against the
                // task's code; fall back to the project dir when none exists.
                let cwd = tmux::sessions_for_task(&project_name, &task.name, &self.sessions)
                    .first()
                    .and_then(|s| s.worktree_path())
                    .map(|p| p.to_string_lossy().to_string())
                    .unwrap_or(project_path);
                (project_name, cwd)
            }
            ListItem::Session {
                project_name,
                project_path,
                session,
                ..
            }
            | ListItem::AdhocSession {
                project_name,
                project_path,
                session,
                ..
            } => {
                let cwd = session
                    .worktree_path()
                    .map(|p| p.to_string_lossy().to_string())
                    .unwrap_or(project_path);
                (project_name, cwd)
            }
            // run_label already returned None for any other variant.
            ListItem::AdhocGroup { .. } => return None,
        };
        Some(RunContext {
            project_name,
            cwd,
            label,
        })
    }

    /// Run-session state for `item`: `Some(true)` while its command is executing,
    /// `Some(false)` once it has finished (shell still open), `None` if no run
    /// session exists.
    pub fn run_state_for(&self, item: &ListItem) -> Option<bool> {
        let label = run_label(item)?;
        self.run_sessions
            .get(&tmux::run_session_name(&label))
            .copied()
    }

    /// Run the selected item. If a run session is already live for it, open the
    /// Attach / Restart / Kill menu; otherwise launch the project's run command
    /// (prompting for it on first use).
    pub fn start_run(&mut self) {
        let ctx = match self.resolve_run_context() {
            Some(c) => c,
            None => return,
        };
        if self
            .run_sessions
            .contains_key(&tmux::run_session_name(&ctx.label))
        {
            self.open_run_menu();
        } else {
            self.launch_or_prompt(ctx);
        }
    }

    /// Launch `ctx`'s run command, or prompt for one (saving it) if the project
    /// has none configured yet.
    fn launch_or_prompt(&mut self, ctx: RunContext) {
        self.config.reload();
        match self.config.project_run_command(&ctx.project_name) {
            Some(cmd) => {
                let cmd = cmd.to_string();
                self.launch_run(ctx, cmd);
            }
            None => {
                self.pending_run = Some(ctx);
                self.input_buffer.clear();
                self.input_mode = InputMode::RunCommand;
                self.status_message = Some("Run command (saved to project for reuse): ".into());
            }
        }
    }

    /// Populate and show the run-session menu (Attach / Restart / Kill).
    fn open_run_menu(&mut self) {
        self.context_menu_items = vec![
            ContextMenuItem {
                key: 'a',
                label: "Attach",
                action: ContextAction::RunAttach,
            },
            ContextMenuItem {
                key: 'r',
                label: "Restart",
                action: ContextAction::RunRestart,
            },
            ContextMenuItem {
                key: 'k',
                label: "Kill",
                action: ContextAction::RunKill,
            },
        ];
        self.context_menu_selected = 0;
        self.input_mode = InputMode::RunMenu;
        self.status_message = Some("Run session is live — choose an action".into());
    }

    /// Attach to the selected item's live run session.
    fn run_attach(&mut self) {
        let Some(ctx) = self.resolve_run_context() else {
            return;
        };
        let name = tmux::run_session_name(&ctx.label);
        if self.run_sessions.contains_key(&name) {
            self.should_attach = Some(name);
        } else {
            self.status_message = Some("Run session is no longer active".into());
        }
    }

    /// Restart the selected item's run session (kill + relaunch).
    fn run_restart(&mut self) {
        let Some(ctx) = self.resolve_run_context() else {
            return;
        };
        self.launch_or_prompt(ctx);
    }

    /// Kill the selected item's run session.
    fn run_kill(&mut self) {
        let Some(ctx) = self.resolve_run_context() else {
            return;
        };
        let name = tmux::run_session_name(&ctx.label);
        match tmux::kill_session_only(&name) {
            Ok(()) => {
                self.run_sessions.remove(&name);
                self.status_message = Some("Run session killed".into());
            }
            Err(e) => self.status_message = Some(format!("Failed to kill run session: {e}")),
        }
    }

    /// Save the entered run command on its project and run it.
    pub fn confirm_run_command(&mut self) {
        let command = self.input_buffer.trim().to_string();
        let ctx = match self.pending_run.take() {
            Some(c) => c,
            None => {
                self.cancel_input();
                return;
            }
        };
        self.input_mode = InputMode::Normal;
        self.input_buffer.clear();
        if command.is_empty() {
            self.status_message = Some("Run cancelled (no command entered)".into());
            return;
        }
        self.config.reload();
        if self
            .config
            .set_project_run_command(&ctx.project_name, command.clone())
        {
            let _ = self.config.save();
        }
        self.launch_run(ctx, command);
    }

    /// Launch `command` in a dedicated tmux run session for `ctx` and attach.
    fn launch_run(&mut self, ctx: RunContext, command: String) {
        match tmux::run_command_session(&ctx.label, &ctx.cwd, &command) {
            Ok(tmux_name) => {
                self.status_message = Some(format!("Running: {command}"));
                self.should_attach = Some(tmux_name);
            }
            Err(e) => self.status_message = Some(format!("Run failed: {e}")),
        }
    }

    /// Open a terminal in the session's worktree: create one if none exists,
    /// then attach to it (attaches directly if one already exists).
    pub fn open_terminal(&mut self) {
        let (name, cwd) = match self.selected_item().cloned() {
            Some(ListItem::Session {
                project_path,
                session,
                ..
            })
            | Some(ListItem::AdhocSession {
                project_path,
                session,
                ..
            }) => {
                let cwd = session
                    .worktree_path()
                    .map(|p| p.to_string_lossy().to_string())
                    .unwrap_or(project_path);
                (session.name.clone(), cwd)
            }
            _ => {
                self.status_message = Some("Select a session to open a terminal".into());
                return;
            }
        };
        if tmux::count_terminal_windows(&name) == 0 {
            if let Err(e) = tmux::create_terminal_window(&name, &cwd) {
                self.status_message = Some(format!("Error: {e}"));
                return;
            }
        }
        // Window 0 is the agent; the first terminal is window 1.
        self.should_attach_window = Some((name, 1));
    }

    /// The tmux name of the selected session, if the selection is a session.
    fn selected_session_name(&self) -> Option<String> {
        match self.selected_item() {
            Some(ListItem::Session { session, .. })
            | Some(ListItem::AdhocSession { session, .. }) => Some(session.name.clone()),
            _ => None,
        }
    }

    /// Open the "Send message" prompt targeting the selected session.
    pub fn start_send_message(&mut self) {
        match self.selected_session_name() {
            Some(name) => {
                self.input_buffer.clear();
                self.status_message = Some(format!("Send message to {name}:"));
                self.pending_send_session = Some(name);
                self.input_mode = InputMode::SendMessage;
            }
            None => self.status_message = Some("Select a session to send a message".into()),
        }
    }

    /// Send the buffered message to the pending session's agent and submit it.
    pub fn confirm_send_message(&mut self) {
        let Some(name) = self.pending_send_session.take() else {
            self.input_mode = InputMode::Normal;
            return;
        };
        let msg = self.input_buffer.clone();
        self.input_buffer.clear();
        self.input_mode = InputMode::Normal;
        if msg.trim().is_empty() {
            self.status_message = Some("Empty message — nothing sent".into());
            return;
        }
        self.start_op(&format!("Sending to {name}..."), move || {
            match crate::tmux::send_text(&name, &msg, true) {
                Ok(()) => OpResult {
                    message: format!("Sent message to '{name}'"),
                    rebuild: false,
                    reload_config: false,
                },
                Err(e) => OpResult {
                    message: format!("Send failed: {e}"),
                    rebuild: false,
                    reload_config: false,
                },
            }
        });
    }

    /// Approve the selected session's pending permission prompt (sends "y").
    /// Gated on the session actually being in `WaitingForPermission`: sending a
    /// bare "y" + Enter into a running or idle agent would inject a stray
    /// command into its input.
    pub fn approve_session(&mut self) {
        let Some(name) = self.selected_session_name() else {
            self.status_message = Some("Select a session to approve".into());
            return;
        };
        let Some(status) = self.session_statuses.get(&name) else {
            self.status_message = Some("No status available for that session".into());
            return;
        };
        if *status != SessionStatus::WaitingForPermission {
            self.status_message =
                Some("Session is not waiting for permission to approve".into());
            return;
        }
        self.start_op(&format!("Approving {name}..."), move || {
            match crate::tmux::send_text(&name, "y", true) {
                Ok(()) => OpResult {
                    message: format!("Sent approval to '{name}'"),
                    rebuild: false,
                    reload_config: false,
                },
                Err(e) => OpResult {
                    message: format!("Approve failed: {e}"),
                    rebuild: false,
                    reload_config: false,
                },
            }
        });
    }

    /// Restart the selected session's agent: kill the tmux session (preserving
    /// the worktree, branch, and record) and relaunch the agent from its
    /// record, resuming the conversation. Reuses the same recreation path the
    /// app uses for startup restore / unarchive.
    pub fn restart_session(&mut self) {
        let Some(name) = self.selected_session_name() else {
            self.status_message = Some("Select a session to restart".into());
            return;
        };
        // Resolve the record up front so a missing/stale one gives a clear
        // message instead of killing the session for no reason.
        let record = config::load_sessions().get(&name).cloned();
        let Some(record) = record else {
            self.status_message =
                Some(format!("Cannot restart '{name}' — no session record found"));
            return;
        };
        self.start_op(&format!("Restarting {name}..."), move || {
            // Kill the tmux session only — keep worktree, branch, record intact.
            let _ = crate::tmux::kill_session_only(&name);
            // The user is explicitly reviving this session: clear the
            // auto-closed marker so startup restore no longer skips it.
            config::set_session_auto_closed(&name, false);
            match crate::tmux::recreate_session(&name, &record) {
                Ok(_) => OpResult {
                    message: format!("Restarted '{name}' (conversation resumed)"),
                    rebuild: true,
                    reload_config: false,
                },
                Err(e) => OpResult {
                    message: format!("Restart failed for '{name}': {e}"),
                    rebuild: true,
                    reload_config: false,
                },
            }
        });
    }

    /// Toggle the per-session resource (CPU/mem/GPU) panel. GPU attribution is
    /// computed on demand when the panel opens.
    pub fn toggle_resources(&mut self) {
        self.show_resources = !self.show_resources;
        if self.show_resources {
            self.gpu_by_session = crate::resources::gpu_by_session();
        }
    }

    pub fn start_search(&mut self) {
        self.input_mode = InputMode::Search;
        self.input_buffer = self.search_query.clone();
        self.status_message = Some("Filter (Esc to clear): ".into());
    }

    pub fn update_search(&mut self) {
        self.search_query = self.input_buffer.clone();
        self.selected = 0;
        self.rebuild_items();
    }

    pub fn confirm_search(&mut self) {
        self.search_query = self.input_buffer.trim().to_string();
        self.input_buffer.clear();
        self.input_mode = InputMode::Normal;
        self.status_message = if self.search_query.is_empty() {
            None
        } else {
            Some(format!("Filter: {}", self.search_query))
        };
        self.selected = 0;
        self.rebuild_items();
        self.sync_worker_hints();
    }

    pub fn cancel_search(&mut self) {
        self.search_query.clear();
        self.input_buffer.clear();
        self.input_mode = InputMode::Normal;
        self.status_message = None;
        self.selected = 0;
        self.rebuild_items();
        self.sync_worker_hints();
    }

    pub fn start_set_base_branch(&mut self) {
        let task = match self.selected_item() {
            Some(ListItem::Task { task, .. }) => task.clone(),
            _ => {
                self.status_message = Some("Select a task to set its base branch".into());
                return;
            }
        };
        self.input_mode = InputMode::SetBaseBranch;
        self.input_buffer = task
            .base_branch
            .clone()
            .unwrap_or_else(|| task.base_branch().to_string());
        self.status_message = Some("Base branch (empty for main): ".into());
    }

    pub fn confirm_set_base_branch(&mut self) {
        let (project_name, task_name) = match self.selected_item() {
            Some(ListItem::Task {
                project_name, task, ..
            }) => (project_name.clone(), task.name.clone()),
            _ => {
                self.cancel_input();
                return;
            }
        };

        let raw = self.input_buffer.trim().to_string();
        let new_base = if raw.is_empty() { None } else { Some(raw) };

        self.config.reload();
        self.config
            .set_task_base_branch(&project_name, &task_name, new_base.clone());
        let _ = self.config.save();

        let label = new_base.as_deref().unwrap_or("main");
        self.status_message = Some(format!("Base branch for '{task_name}' set to {label}"));
        self.input_buffer.clear();
        self.input_mode = InputMode::Normal;
        self.rebuild_items();
        self.sync_worker_hints();
    }

    pub fn copy_worktree_path(&mut self) {
        let session = match self.selected_item() {
            Some(ListItem::Session { session, .. }) => session,
            _ => {
                self.status_message = Some("Select a session to copy its worktree path".into());
                return;
            }
        };
        let path = match session.worktree_path() {
            Some(p) => p.to_string_lossy().to_string(),
            None => {
                self.status_message = Some("Session has no worktree".into());
                return;
            }
        };
        match copy_to_clipboard(&path) {
            Ok(()) => self.status_message = Some(format!("Copied to clipboard: {path}")),
            Err(e) => self.status_message = Some(format!("Copy failed: {e}")),
        }
    }

    pub fn start_add_project(&mut self) {
        // Prefill with the current directory as a convenient starting point; the
        // user can edit it (Tab completes directory paths).
        self.input_buffer = std::env::current_dir()
            .map(|p| p.to_string_lossy().to_string())
            .unwrap_or_default();
        self.input_mode = InputMode::AddProjectPath;
        self.status_message = Some("Project directory (⇥ to complete): ".into());
    }

    /// Tab-complete the directory path currently in the input buffer.
    pub fn complete_project_path(&mut self) {
        if let Some(completed) = complete_dir_path(&self.input_buffer) {
            self.input_buffer = completed;
        }
    }

    /// Validate the entered project directory, then prompt for a name.
    pub fn confirm_add_project_path(&mut self) {
        let raw = self.input_buffer.trim();
        if raw.is_empty() {
            self.status_message = Some("Enter a directory path".into());
            return;
        }
        let path = std::path::PathBuf::from(expand_tilde(raw));
        if !path.is_dir() {
            self.status_message = Some("Not a directory".into());
            return;
        }
        let path = path.canonicalize().unwrap_or(path);
        let path_str = path.to_string_lossy().to_string();
        if !path.join(".git").is_dir() {
            self.status_message = Some("Not a git repository".into());
            return;
        }
        if self.config.has_project_at(&path_str) {
            self.status_message = Some("Project already registered".into());
            return;
        }

        let default_name = path
            .file_name()
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or_default();
        self.pending_project_path = Some(path_str);
        self.input_mode = InputMode::AddProjectName;
        self.input_buffer.clear();
        self.status_message = Some(format!("Enter project name (default: {default_name}): "));
    }

    pub fn confirm_add_project(&mut self) {
        if let Some(path) = self.pending_project_path.take() {
            let name = if self.input_buffer.trim().is_empty() {
                std::path::Path::new(&path)
                    .file_name()
                    .map(|n| n.to_string_lossy().to_string())
                    .unwrap_or_else(|| "unknown".into())
            } else {
                self.input_buffer.trim().to_string()
            };
            self.config.reload();
            self.config.add_project(name, path);
            let _ = self.config.save();
            self.input_buffer.clear();
            self.input_mode = InputMode::Normal;
            self.status_message = None;
            self.rebuild_items();
        }
    }

    pub fn start_add_task(&mut self) {
        if self.selected_project_info().is_some() {
            self.use_worktree = true;
            self.input_mode = InputMode::AddTaskName;
            self.input_buffer.clear();
            self.status_message = Some("Task name: ".into());
        }
    }

    pub fn confirm_add_task(&mut self) {
        let task_name = self.input_buffer.trim().to_string();
        if task_name.is_empty() {
            self.cancel_input();
            return;
        }
        if tmux::is_adhoc_marker(&task_name) {
            self.status_message = Some("'adhoc' is reserved — pick a different task name".into());
            return;
        }

        self.pending_task_name = Some(task_name.clone());
        self.input_buffer = tmux::to_branch_name(&task_name);
        self.input_mode = InputMode::AddTaskBranch;
        self.status_message = Some("Branch name (existing or new): ".into());
    }

    pub fn confirm_add_task_branch(&mut self) {
        let branch = self.input_buffer.trim().to_string();
        if branch.is_empty() {
            self.cancel_input();
            return;
        }
        if branch == "main" || branch == "master" {
            self.status_message = Some("Cannot use 'main' or 'master' as a task branch".into());
            return;
        }

        if self.pending_task_name.is_none() {
            self.cancel_input();
            return;
        }

        self.pending_task_branch = Some(branch);
        self.input_buffer.clear();
        self.input_mode = InputMode::AddTaskPrompt;
        self.status_message = Some("Initial session prompt (empty to skip): ".into());
    }

    pub fn confirm_add_task_with_prompt(&mut self) {
        let task_name = match self.pending_task_name.take() {
            Some(n) => n,
            None => {
                self.cancel_input();
                return;
            }
        };
        let branch = match self.pending_task_branch.take() {
            Some(b) => b,
            None => {
                self.cancel_input();
                return;
            }
        };

        let (project_name, project_path) = match self.selected_project_info() {
            Some((name, path)) => (name.to_string(), path.to_string()),
            None => {
                self.cancel_input();
                return;
            }
        };

        let prompt = if self.input_buffer.trim().is_empty() {
            None
        } else {
            Some(self.input_buffer.trim().to_string())
        };

        self.collapsed.remove(&project_key(&project_name));
        self.input_buffer.clear();
        self.input_mode = InputMode::Normal;

        let use_worktree = self.use_worktree;
        let startup_skills = self.config.startup_skills.clone();
        let agent = self
            .pending_agent
            .take()
            .unwrap_or_else(|| self.default_agent());
        let project = self.config.projects.iter().find(|p| p.name == project_name);
        let copy_patterns = project.map(|p| p.copy_patterns.clone()).unwrap_or_default();
        let setup_commands = project
            .map(|p| p.setup_commands.clone())
            .unwrap_or_default();

        self.start_op("Creating task...", move || {
            let branch_exists = tmux::branch_exists(&project_path, &branch);

            if !branch_exists {
                if let Err(e) = tmux::create_task_branch(&project_path, &branch, None) {
                    return OpResult {
                        message: format!("Error: {e}"),
                        rebuild: false,
                        reload_config: false,
                    };
                }
            }

            let task_name_for_modify = task_name.clone();
            let branch_for_modify = branch.clone();
            let project_name_for_modify = project_name.clone();
            if let Err(e) = Config::modify(move |c| {
                c.add_task(
                    &project_name_for_modify,
                    task_name_for_modify,
                    branch_for_modify,
                );
            }) {
                return OpResult {
                    message: format!("Error saving config: {e}"),
                    rebuild: false,
                    reload_config: false,
                };
            }

            let session_name = tmux::MAIN_SESSION.to_string();

            match tmux::create_session(
                &project_name,
                &project_path,
                &task_name,
                &branch,
                &session_name,
                use_worktree,
                &copy_patterns,
                &setup_commands,
                prompt.as_deref(),
                &startup_skills,
                agent,
            ) {
                Ok(tmux_name) => {
                    config::add_session_record(
                        &tmux_name,
                        config::SessionRecord {
                            project_name: project_name.clone(),
                            project_path: project_path.clone(),
                            task_name: task_name.clone(),
                            task_branch: branch.clone(),
                            session_name: session_name.clone(),
                            use_worktree,
                            archived: false,
                            auto_closed: false,
                            agent: agent.id().to_string(),
                        },
                    );
                    let task_msg = if branch_exists {
                        format!("task '{task_name}' on existing branch {branch}")
                    } else {
                        format!("task '{task_name}' on branch {branch}")
                    };
                    OpResult {
                        message: format!("Created {task_msg} and session {tmux_name}"),
                        rebuild: true,
                        reload_config: true,
                    }
                }
                Err(e) => OpResult {
                    message: format!("Task created but session failed: {e}"),
                    rebuild: true,
                    reload_config: true,
                },
            }
        });
    }

    pub fn start_new_adhoc_session(&mut self) {
        if self.selected_project_info().is_none() {
            self.status_message = Some("Select a project first".into());
            return;
        }
        self.input_mode = InputMode::AddAdhocSessionName;
        self.input_buffer.clear();
        self.status_message = Some("Adhoc session name: ".into());
    }

    pub fn confirm_new_adhoc_session(&mut self) {
        let name = self.input_buffer.trim().to_string();
        if name.is_empty() {
            self.cancel_input();
            return;
        }

        let (project_name, project_path) = match self.selected_project_info() {
            Some((n, p)) => (n.to_string(), p.to_string()),
            None => {
                self.cancel_input();
                return;
            }
        };

        let tmux_name_to_create = format!(
            "cm__{}__{}__{}",
            tmux::sanitize(&project_name),
            tmux::ADHOC_MARKER,
            tmux::sanitize(&name),
        );
        if self.sessions.iter().any(|s| s.name == tmux_name_to_create) {
            self.status_message = Some(format!("Adhoc session '{name}' already exists"));
            return;
        }

        self.collapsed.remove(&project_key(&project_name));
        self.collapsed.remove(&adhoc_group_key(&project_name));
        self.input_buffer.clear();
        self.input_mode = InputMode::Normal;

        let startup_skills = self.config.startup_skills.clone();
        let agent = self.default_agent();
        let proj_name_for_op = project_name.clone();
        let proj_path_for_op = project_path.clone();
        let session_name = name.clone();

        self.start_op(
            "Creating adhoc session...",
            move || match tmux::create_adhoc_session(
                &proj_name_for_op,
                &proj_path_for_op,
                &session_name,
                &startup_skills,
                agent,
            ) {
                Ok(tmux_name) => {
                    config::add_session_record(
                        &tmux_name,
                        config::SessionRecord {
                            project_name: proj_name_for_op.clone(),
                            project_path: proj_path_for_op.clone(),
                            task_name: tmux::ADHOC_MARKER.to_string(),
                            task_branch: String::new(),
                            session_name: session_name.clone(),
                            use_worktree: false,
                            archived: false,
                            auto_closed: false,
                            agent: agent.id().to_string(),
                        },
                    );
                    OpResult {
                        message: format!("Created adhoc session {tmux_name}"),
                        rebuild: true,
                        reload_config: false,
                    }
                }
                Err(e) => OpResult {
                    message: format!("Error: {e}"),
                    rebuild: false,
                    reload_config: false,
                },
            },
        );
    }

    pub fn start_new_session(&mut self, use_worktree: bool) {
        let info = self
            .selected_task_info()
            .map(|(pn, _, t)| (pn.to_string(), t.name.clone()));

        if let Some((project_name, task_name)) = info {
            self.use_worktree = use_worktree;
            self.input_mode = InputMode::AddSessionName;
            self.input_buffer.clear();
            let next = tmux::next_session_number(&project_name, &task_name, &self.sessions);
            self.status_message = Some(format!(
                "Session name (default: {next}){}:",
                if use_worktree { " [worktree]" } else { "" }
            ));
        } else {
            self.status_message = Some("Select a task first to create a session".into());
        }
    }

    pub fn confirm_new_session(&mut self) {
        let (project_name, _, task) = match self.selected_task_info() {
            Some((pn, pp, t)) => (pn.to_string(), pp.to_string(), t.clone()),
            None => {
                self.cancel_input();
                return;
            }
        };

        let session_name = if self.input_buffer.trim().is_empty() {
            tmux::next_session_number(&project_name, &task.name, &self.sessions).to_string()
        } else {
            self.input_buffer.trim().to_string()
        };

        if tmux::is_main_session(&session_name) {
            self.cancel_input();
            self.status_message = Some(format!(
                "'{}' is reserved for the task's own session",
                tmux::MAIN_SESSION
            ));
            return;
        }

        self.pending_session_name = Some(session_name);
        self.input_buffer.clear();
        self.input_mode = InputMode::AddSessionPrompt;
        self.status_message = Some("Initial prompt (empty to skip): ".into());
    }

    pub fn confirm_new_session_with_prompt(&mut self) {
        let (project_name, project_path, task) = match self.selected_task_info() {
            Some((pn, pp, t)) => (pn.to_string(), pp.to_string(), t.clone()),
            None => {
                self.cancel_input();
                return;
            }
        };

        let session_name = match self.pending_session_name.take() {
            Some(name) => name,
            None => {
                self.cancel_input();
                return;
            }
        };

        let prompt = if self.input_buffer.trim().is_empty() {
            None
        } else {
            Some(self.input_buffer.trim().to_string())
        };

        let use_worktree = self.use_worktree;
        let task_name = task.name.clone();
        let task_branch = task.branch.clone();
        let project = self.config.projects.iter().find(|p| p.name == project_name);
        let copy_patterns = project.map(|p| p.copy_patterns.clone()).unwrap_or_default();
        let setup_commands = project
            .map(|p| p.setup_commands.clone())
            .unwrap_or_default();
        let startup_skills = self.config.startup_skills.clone();
        let agent = self
            .pending_agent
            .take()
            .unwrap_or_else(|| self.default_agent());
        self.input_buffer.clear();
        self.input_mode = InputMode::Normal;

        self.start_op("Creating session...", move || {
            match tmux::create_session(
                &project_name,
                &project_path,
                &task_name,
                &task_branch,
                &session_name,
                use_worktree,
                &copy_patterns,
                &setup_commands,
                prompt.as_deref(),
                &startup_skills,
                agent,
            ) {
                Ok(tmux_name) => {
                    config::add_session_record(
                        &tmux_name,
                        config::SessionRecord {
                            project_name: project_name.clone(),
                            project_path: project_path.clone(),
                            task_name: task_name.clone(),
                            task_branch: task_branch.clone(),
                            session_name: session_name.clone(),
                            use_worktree,
                            archived: false,
                            auto_closed: false,
                            agent: agent.id().to_string(),
                        },
                    );
                    OpResult {
                        message: format!("Created session {tmux_name}"),
                        rebuild: true,
                        reload_config: false,
                    }
                }
                Err(e) => OpResult {
                    message: format!("Error: {e}"),
                    rebuild: false,
                    reload_config: false,
                },
            }
        });
    }

    pub fn start_delete(&mut self) {
        match self.selected_item() {
            Some(ListItem::Project { project }) => {
                let session_count = self
                    .sessions
                    .iter()
                    .filter(|s| s.project_name == tmux::sanitize(&project.name))
                    .count();
                let task_count = project.tasks.len();
                self.input_mode = InputMode::ConfirmDelete;
                if session_count > 0 || task_count > 0 {
                    self.status_message = Some(format!(
                        "Delete project and all {} task(s), {} session(s)? (y/n)",
                        task_count, session_count
                    ));
                } else {
                    self.status_message = Some("Delete this project? (y/n)".into());
                }
            }
            Some(ListItem::Session { session, .. }) => {
                if tmux::is_main_session(&session.session_name) {
                    self.status_message = Some(
                        "The main session can't be deleted — delete or archive the task instead"
                            .into(),
                    );
                    return;
                }
                self.input_mode = InputMode::ConfirmDelete;
                self.status_message = Some("Delete this session? (y/n)".into());
            }
            Some(ListItem::AdhocSession { .. }) => {
                self.input_mode = InputMode::ConfirmDelete;
                self.status_message = Some("Delete this adhoc session? (y/n)".into());
            }
            Some(ListItem::Task {
                project_name, task, ..
            }) => {
                let active = tmux::sessions_for_task(project_name, &task.name, &self.sessions);
                self.input_mode = InputMode::ConfirmDelete;
                if active.is_empty() {
                    self.status_message = Some("Delete this task? (y/n)".into());
                } else {
                    self.status_message = Some(format!(
                        "Delete task and kill {} active session(s)? (y/n)",
                        active.len()
                    ));
                }
            }
            _ => {}
        }
    }

    pub fn confirm_delete(&mut self) {
        match self.selected_item().cloned() {
            Some(ListItem::Project { project }) => {
                let project_name = project.name.clone();
                let project_path = project.path.clone();
                let tasks: Vec<_> = project.tasks.clone();
                let sessions = self.sessions.clone();
                self.input_mode = InputMode::Normal;
                self.start_op("Deleting project...", move || {
                    let mut total_sessions = 0;
                    for task in &tasks {
                        let msg = tmux::delete_task(
                            &project_name,
                            &project_path,
                            &task.name,
                            &task.branch,
                            &sessions,
                        );
                        // Count sessions from message
                        if msg.contains("session(s)") {
                            total_sessions +=
                                tmux::sessions_for_task(&project_name, &task.name, &sessions).len();
                        }
                    }
                    let _ = total_sessions;
                    // Clean up leftover worktree and task config directories
                    tmux::cleanup_project_dirs(&project_name);
                    config::remove_project_session_records(&project_name);
                    OpResult {
                        message: format!("Deleted project '{}'", project_name),
                        rebuild: true,
                        reload_config: true,
                    }
                });
                // Remove project from config (done here so it's saved even if op thread is slow)
                self.config.reload();
                self.config.remove_project(&project.path);
                let _ = self.config.save();
                return;
            }
            Some(ListItem::Session { session, .. }) => {
                let name = session.name.clone();
                let display_name = session.session_name.clone();
                self.input_mode = InputMode::Normal;
                self.start_op("Deleting session...", move || {
                    // Load session record for fallback cleanup info
                    let fallback = config::load_sessions()
                        .remove(&name)
                        .filter(|r| r.use_worktree)
                        .map(|r| {
                            let wt =
                                tmux::worktree_dir(&r.project_name, &r.task_name, &r.session_name);
                            tmux::SessionCleanupInfo {
                                project_path: r.project_path,
                                worktree_path: wt.to_string_lossy().to_string(),
                                branch_name: Some(format!(
                                    "{}-{}",
                                    tmux::sanitize(&r.task_branch),
                                    tmux::sanitize(&r.session_name),
                                )),
                                task_branch: Some(r.task_branch),
                            }
                        });
                    match tmux::kill_session_with_fallback(&name, fallback) {
                        Ok(()) => {
                            config::remove_session_record(&name);
                            OpResult {
                                message: format!("Killed session {display_name}"),
                                rebuild: true,
                                reload_config: false,
                            }
                        }
                        Err(e) => OpResult {
                            message: format!("Error: {e}"),
                            rebuild: false,
                            reload_config: false,
                        },
                    }
                });
                return;
            }
            Some(ListItem::AdhocSession { session, .. }) => {
                let name = session.name.clone();
                let display_name = session.session_name.clone();
                self.input_mode = InputMode::Normal;
                self.start_op("Deleting adhoc session...", move || {
                    match tmux::kill_session_with_fallback(&name, None) {
                        Ok(()) => {
                            config::remove_session_record(&name);
                            OpResult {
                                message: format!("Killed adhoc session {display_name}"),
                                rebuild: true,
                                reload_config: false,
                            }
                        }
                        Err(e) => OpResult {
                            message: format!("Error: {e}"),
                            rebuild: false,
                            reload_config: false,
                        },
                    }
                });
                return;
            }
            Some(ListItem::Task {
                project_name,
                project_path,
                task,
            }) => {
                let task_name = task.name.clone();
                let task_branch = task.branch.clone();
                let pname = project_name.clone();
                let ppath = project_path.clone();
                let sessions = self.sessions.clone();
                self.input_mode = InputMode::Normal;
                self.start_op("Deleting task...", move || {
                    let msg =
                        tmux::delete_task(&pname, &ppath, &task_name, &task_branch, &sessions);
                    config::remove_task_session_records(&pname, &task_name);
                    OpResult {
                        message: msg,
                        rebuild: true,
                        reload_config: true,
                    }
                });
                // Remove task from config immediately
                self.config.reload();
                self.config.remove_task(&project_name, &task.name);
                let _ = self.config.save();
                return;
            }
            _ => {}
        }
        self.input_mode = InputMode::Normal;
    }

    pub fn start_merge(&mut self) {
        let (project_path, task, session) = match self.selected_item().cloned() {
            Some(ListItem::Session {
                project_path,
                task,
                session,
                ..
            }) => (project_path, task, session),
            _ => {
                self.status_message = Some("Select a session to merge".into());
                return;
            }
        };

        let wt_path = match session.worktree_path() {
            Some(p) => p.to_string_lossy().to_string(),
            None => {
                self.status_message = Some("Cannot merge: session has no worktree".into());
                return;
            }
        };

        // Check if worktree has uncommitted changes
        if tmux::worktree_is_dirty(&wt_path) {
            self.input_mode = InputMode::MergeCommitMessage;
            self.input_buffer.clear();
            let default_msg = tmux::next_commit_message(&wt_path, &session.session_name);
            self.status_message = Some(format!("Commit message (default: {default_msg}): "));
        } else {
            self.do_merge(project_path, task.branch, session.session_name, wt_path);
        }
    }

    pub fn confirm_merge_commit(&mut self) {
        let (project_path, task, session) = match self.selected_item().cloned() {
            Some(ListItem::Session {
                project_path,
                task,
                session,
                ..
            }) => (project_path, task, session),
            _ => {
                self.cancel_input();
                return;
            }
        };

        let wt_path = match session.worktree_path() {
            Some(p) => p.to_string_lossy().to_string(),
            None => {
                self.cancel_input();
                return;
            }
        };

        let msg = if self.input_buffer.trim().is_empty() {
            tmux::next_commit_message(&wt_path, &session.session_name)
        } else {
            self.input_buffer.trim().to_string()
        };

        let task_branch = task.branch.clone();
        let session_display = session.session_name.clone();
        self.input_buffer.clear();
        self.input_mode = InputMode::Normal;

        self.start_op("Merging...", move || {
            if let Err(e) = tmux::commit_all(&wt_path, &msg) {
                return OpResult {
                    message: format!("Error committing: {e}"),
                    rebuild: false,
                    reload_config: false,
                };
            }
            match tmux::merge_session_to_task(
                &project_path,
                &task_branch,
                &session_display,
                &wt_path,
            ) {
                Ok(msg) => OpResult {
                    message: msg,
                    rebuild: false,
                    reload_config: false,
                },
                Err(e) => OpResult {
                    message: format!("Error: {e}"),
                    rebuild: false,
                    reload_config: false,
                },
            }
        });
    }

    fn do_merge(
        &mut self,
        project_path: String,
        task_branch: String,
        session_name: String,
        wt_path: String,
    ) {
        self.start_op("Merging...", move || {
            match tmux::merge_session_to_task(&project_path, &task_branch, &session_name, &wt_path)
            {
                Ok(msg) => OpResult {
                    message: msg,
                    rebuild: false,
                    reload_config: false,
                },
                Err(e) => OpResult {
                    message: format!("Error: {e}"),
                    rebuild: false,
                    reload_config: false,
                },
            }
        });
    }

    pub fn update_session(&mut self) {
        match self.selected_item().cloned() {
            Some(ListItem::Task {
                project_path, task, ..
            }) => {
                let branch = task.branch.clone();
                let base_branch = task.base_branch().to_string();
                self.start_op(
                    "Updating task branch...",
                    move || match tmux::update_task_branch(&project_path, &branch, &base_branch) {
                        Ok(msg) => OpResult {
                            message: msg,
                            rebuild: false,
                            reload_config: false,
                        },
                        Err(e) => OpResult {
                            message: format!("Error: {e}"),
                            rebuild: false,
                            reload_config: false,
                        },
                    },
                );
            }
            Some(ListItem::Session {
                project_path,
                task,
                session,
                ..
            }) => {
                let wt_path = match session.worktree_path() {
                    Some(p) => p.to_string_lossy().to_string(),
                    None => {
                        self.status_message = Some("Cannot update: session has no worktree".into());
                        return;
                    }
                };
                let task_branch = task.branch.clone();
                self.start_op(
                    "Updating session...",
                    move || match tmux::rebase_session_on_task(
                        &project_path,
                        &task_branch,
                        &wt_path,
                    ) {
                        Ok(msg) => OpResult {
                            message: msg,
                            rebuild: false,
                            reload_config: false,
                        },
                        Err(e) => OpResult {
                            message: format!("Error: {e}"),
                            rebuild: false,
                            reload_config: false,
                        },
                    },
                );
            }
            _ => {
                self.status_message = Some("Select a session or task to update".into());
            }
        }
    }

    pub fn push_task_branch(&mut self) {
        let (project_path, task) = match self.selected_item().cloned() {
            Some(ListItem::Task {
                project_path, task, ..
            }) => (project_path, task),
            _ => {
                self.status_message = Some("Select a task to push".into());
                return;
            }
        };

        let branch = task.branch.clone();
        self.start_op("Pushing...", move || {
            match tmux::push_branch(&project_path, &branch) {
                Ok(msg) => OpResult {
                    message: msg,
                    rebuild: false,
                    reload_config: false,
                },
                Err(e) => OpResult {
                    message: format!("Error: {e}"),
                    rebuild: false,
                    reload_config: false,
                },
            }
        });
    }

    /// Open the fuzzy branch picker for the selected project.
    pub fn start_checkout_branch(&mut self) {
        let project_path = match self.selected_item() {
            Some(ListItem::Project { project }) => project.path.clone(),
            _ => {
                self.status_message = Some("Select a project to checkout a branch".into());
                return;
            }
        };
        let branches = tmux::list_branches(&project_path);
        if branches.is_empty() {
            self.status_message = Some("No branches found in this project".into());
            return;
        }
        self.branch_picker_all = branches;
        self.branch_picker_project = project_path;
        self.branch_picker_selected = 0;
        self.input_buffer.clear();
        self.input_mode = InputMode::CheckoutBranch;
        self.status_message = Some("Checkout branch (type to filter)".into());
    }

    /// Branches matching the current picker query, best match first. With an
    /// empty query, returns every branch in list order.
    pub fn filtered_branches(&self) -> Vec<&String> {
        let query = self.input_buffer.trim();
        let mut scored: Vec<(i64, usize, &String)> = self
            .branch_picker_all
            .iter()
            .enumerate()
            .filter_map(|(i, b)| fuzzy_score(query, b).map(|s| (s, i, b)))
            .collect();
        // Sort by score, then original order to keep ranking stable.
        scored.sort_by(|a, b| a.0.cmp(&b.0).then(a.1.cmp(&b.1)));
        scored.into_iter().map(|(_, _, b)| b).collect()
    }

    /// Re-clamp the picker selection after the filter changes.
    pub fn update_branch_filter(&mut self) {
        let len = self.filtered_branches().len();
        if self.branch_picker_selected >= len {
            self.branch_picker_selected = len.saturating_sub(1);
        }
    }

    pub fn branch_picker_move_up(&mut self) {
        if self.branch_picker_selected > 0 {
            self.branch_picker_selected -= 1;
        }
    }

    pub fn branch_picker_move_down(&mut self) {
        let len = self.filtered_branches().len();
        if self.branch_picker_selected + 1 < len {
            self.branch_picker_selected += 1;
        }
    }

    /// Check out the branch highlighted in the picker.
    pub fn confirm_checkout_branch(&mut self) {
        let branch = match self.filtered_branches().get(self.branch_picker_selected) {
            Some(b) => (*b).clone(),
            None => {
                self.cancel_input();
                return;
            }
        };
        let project_path = self.branch_picker_project.clone();
        self.input_mode = InputMode::Normal;
        self.input_buffer.clear();
        self.branch_picker_all.clear();
        self.start_op(&format!("Checking out {branch}…"), move || {
            let output = std::process::Command::new("git")
                .args(["-C", &project_path, "checkout", &branch])
                .output();
            let message = match output {
                Ok(o) if o.status.success() => format!("Checked out {branch}"),
                Ok(o) => format!("Error: {}", String::from_utf8_lossy(&o.stderr).trim()),
                Err(e) => format!("Error: {e}"),
            };
            OpResult {
                message,
                rebuild: false,
                reload_config: false,
            }
        });
    }

    /// Copy the selected project's directory path to the clipboard.
    pub fn copy_project_path(&mut self) {
        let path = match self.selected_item() {
            Some(ListItem::Project { project }) => project.path.clone(),
            _ => {
                self.status_message = Some("Select a project to copy its path".into());
                return;
            }
        };
        match copy_to_clipboard(&path) {
            Ok(()) => self.status_message = Some(format!("Copied to clipboard: {path}")),
            Err(e) => self.status_message = Some(format!("Copy failed: {e}")),
        }
    }

    /// Fetch all remotes and fast-forward the current branch for the selected project.
    pub fn fetch_pull_all_branches(&mut self) {
        let (name, path) = match self.selected_item() {
            Some(ListItem::Project { project }) => (project.name.clone(), project.path.clone()),
            _ => {
                self.status_message = Some("Select a project to fetch".into());
                return;
            }
        };
        self.start_op(&format!("Fetching all branches for {name}…"), move || {
            let message = match tmux::fetch_pull_all(&path) {
                Ok(msg) => msg,
                Err(e) => format!("{e}"),
            };
            OpResult {
                message,
                rebuild: false,
                reload_config: false,
            }
        });
    }

    pub fn checkout_task_branch(&mut self) {
        let (project_path, task) = match self.selected_item().cloned() {
            Some(ListItem::Task {
                project_path, task, ..
            }) => (project_path, task),
            _ => {
                self.status_message = Some("Select a task to checkout".into());
                return;
            }
        };

        let branch = task.branch.clone();
        self.start_op("Checking out...", move || {
            let output = std::process::Command::new("git")
                .args(["-C", &project_path, "checkout", &branch])
                .output();

            match output {
                Ok(o) if o.status.success() => OpResult {
                    message: format!("Checked out {branch}"),
                    rebuild: false,
                    reload_config: false,
                },
                Ok(o) => {
                    let stderr = String::from_utf8_lossy(&o.stderr).trim().to_string();
                    OpResult {
                        message: format!("Error: {stderr}"),
                        rebuild: false,
                        reload_config: false,
                    }
                }
                Err(e) => OpResult {
                    message: format!("Error: {e}"),
                    rebuild: false,
                    reload_config: false,
                },
            }
        });
    }

    pub fn open_pr(&mut self) {
        if let Some(ListItem::Task { task, .. }) = self.selected_item() {
            if let Some(url) = self.pr_urls.get(&task.branch) {
                open_url(url);
            } else {
                self.input_mode = InputMode::ConfirmCreatePr;
                self.status_message = Some("No PR found. Create one? (y/n)".into());
            }
        }
    }

    pub fn confirm_create_pr(&mut self) {
        let (project_path, task) = match self.selected_item().cloned() {
            Some(ListItem::Task {
                project_path, task, ..
            }) => (project_path, task),
            _ => {
                self.cancel_input();
                return;
            }
        };

        let branch = task.branch.clone();
        let task_name = task.name.clone();
        // An explicit base (a stacked task, or a non-main base like develop)
        // must be the PR's base too; unset falls back to the repo default.
        let base = task.base_branch.clone().filter(|b| !b.trim().is_empty());
        self.input_mode = InputMode::Normal;

        self.start_op("Creating PR...", move || {
            // Push branch first
            if let Err(e) = tmux::push_branch(&project_path, &branch) {
                return OpResult {
                    message: format!("Error pushing: {e}"),
                    rebuild: false,
                    reload_config: false,
                };
            }

            let mut args = vec![
                "pr", "create", "--draft", "--title", &task_name, "--body", "", "--head", &branch,
            ];
            if let Some(base) = &base {
                args.extend(["--base", base]);
            }
            let output = std::process::Command::new("gh")
                .args(&args)
                .current_dir(&project_path)
                .output();

            match output {
                Ok(o) if o.status.success() => {
                    let url = String::from_utf8_lossy(&o.stdout).trim().to_string();
                    open_url(&url);
                    OpResult {
                        message: format!("Created PR: {url}"),
                        rebuild: false,
                        reload_config: false,
                    }
                }
                Ok(o) => {
                    let stderr = String::from_utf8_lossy(&o.stderr).trim().to_string();
                    OpResult {
                        message: format!("Error creating PR: {stderr}"),
                        rebuild: false,
                        reload_config: false,
                    }
                }
                Err(e) => OpResult {
                    message: format!("Error: {e}"),
                    rebuild: false,
                    reload_config: false,
                },
            }
        });
    }

    pub fn cancel_input(&mut self) {
        self.input_mode = InputMode::Normal;
        self.input_buffer.clear();
        self.status_message = None;
        self.pending_task_name = None;
        self.pending_task_branch = None;
        self.pending_session_name = None;
        self.pending_agent = None;
        self.agent_picker_target = None;
        self.pending_run = None;
    }
}

/// Case-insensitive subsequence fuzzy match used by the branch picker. Returns
/// a score (lower = better) when every char of `query` appears in `candidate`
/// in order, otherwise `None`. An empty query matches everything with score 0.
/// Scoring favours an early first match, then tight (low-gap) matches, then
/// shorter candidates.
fn fuzzy_score(query: &str, candidate: &str) -> Option<i64> {
    let query = query.to_lowercase();
    if query.is_empty() {
        return Some(0);
    }
    let cand: Vec<char> = candidate.to_lowercase().chars().collect();
    let mut ci = 0usize;
    let mut first: Option<usize> = None;
    let mut prev: Option<usize> = None;
    let mut gaps: i64 = 0;
    for qc in query.chars() {
        let mut found = false;
        while ci < cand.len() {
            let matched = cand[ci] == qc;
            ci += 1;
            if matched {
                let at = ci - 1;
                if first.is_none() {
                    first = Some(at);
                }
                if let Some(p) = prev {
                    gaps += (at - p - 1) as i64;
                }
                prev = Some(at);
                found = true;
                break;
            }
        }
        if !found {
            return None;
        }
    }
    let first = first.unwrap_or(0) as i64;
    Some(first * 4 + gaps * 2 + candidate.chars().count() as i64 / 10)
}

/// Open `url` (or a file path) in the platform's default handler. Uses `open`
/// on macOS and `xdg-open` on Linux/other Unixes. Errors are swallowed by
/// callers, matching the previous best-effort behavior.
fn open_url(url: &str) {
    let opener = if cfg!(target_os = "macos") {
        "open"
    } else {
        "xdg-open"
    };
    let _ = Command::new(opener).arg(url).output();
}

/// Copy `text` to the system clipboard. Tries `pbcopy` (macOS), `wl-copy`
/// (Wayland), then `xclip -selection clipboard` (X11).
fn copy_to_clipboard(text: &str) -> std::result::Result<(), String> {
    let candidates: &[(&str, &[&str])] = &[
        ("pbcopy", &[]),
        ("wl-copy", &[]),
        ("xclip", &["-selection", "clipboard"]),
    ];
    let mut last_err = String::from("no clipboard tool found (pbcopy / wl-copy / xclip)");
    for (cmd, args) in candidates {
        match Command::new(cmd)
            .args(*args)
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
        {
            Ok(mut child) => {
                if let Some(mut stdin) = child.stdin.take() {
                    if let Err(e) = stdin.write_all(text.as_bytes()) {
                        last_err = format!("{cmd}: write failed: {e}");
                        let _ = child.wait();
                        continue;
                    }
                }
                match child.wait() {
                    Ok(status) if status.success() => return Ok(()),
                    Ok(status) => last_err = format!("{cmd} exited with {status}"),
                    Err(e) => last_err = format!("{cmd}: {e}"),
                }
            }
            Err(_) => continue,
        }
    }
    Err(last_err)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fuzzy_empty_query_matches_everything() {
        assert_eq!(fuzzy_score("", "any-branch"), Some(0));
    }

    #[test]
    fn fuzzy_non_subsequence_does_not_match() {
        assert_eq!(fuzzy_score("xyz", "feature/login"), None);
        // Out-of-order chars are not a subsequence.
        assert_eq!(fuzzy_score("ba", "abc"), None);
    }

    #[test]
    fn fuzzy_matches_subsequence_case_insensitively() {
        assert!(fuzzy_score("FL", "feature/login").is_some());
        assert!(fuzzy_score("ftr", "feature").is_some());
    }

    #[test]
    fn fuzzy_ranks_earlier_and_tighter_matches_better() {
        // Contiguous prefix beats a scattered match.
        let prefix = fuzzy_score("feat", "feature/x").unwrap();
        let scattered = fuzzy_score("feat", "x-fix-east").unwrap();
        assert!(prefix < scattered, "{prefix} should beat {scattered}");
    }

    #[test]
    fn extract_difit_comments_returns_none_without_marker() {
        assert_eq!(extract_difit_comments("server started\nbye"), None);
    }

    #[test]
    fn extract_difit_comments_captures_from_marker() {
        let out = "noise\nComments from review session:\n- fix this\n";
        let got = extract_difit_comments(out).unwrap();
        assert!(got.starts_with("Comments from review session:"));
        assert!(got.contains("- fix this"));
    }

    // Parse the exact shape `hunk session comment list --json` emits, then format
    // it into the comment block. Guards both the serde field renames and the
    // file:line rendering (incl. the oldRange-only / missing-line fallbacks).
    fn parse_hunk_comments(json: &str) -> Vec<HunkComment> {
        #[derive(serde::Deserialize)]
        struct List {
            comments: Vec<HunkComment>,
        }
        serde_json::from_str::<List>(json).unwrap().comments
    }

    #[test]
    fn format_hunk_comments_renders_file_line_and_body() {
        let json = r#"{"comments":[
            {"noteId":"u:1","source":"user","filePath":"src/f.txt","hunkIndex":0,
             "newRange":[2,2],"body":"Please rename this variable","editable":true},
            {"noteId":"u:2","source":"user","filePath":"src/g.txt",
             "oldRange":[10,12],"body":"Removed too much","editable":true}
        ]}"#;
        let comments = parse_hunk_comments(json);
        let out = format_hunk_comments(&comments);
        assert!(out.contains("- src/f.txt:2 — Please rename this variable"));
        // Falls back to oldRange when newRange is absent.
        assert!(out.contains("- src/g.txt:10 — Removed too much"));
        // Wrapped by the shared review prompt when forwarded.
        assert!(review_prompt(&out).contains("Please address them"));
    }

    #[test]
    fn format_hunk_comments_tolerates_missing_fields() {
        // A note with neither range nor file still renders without panicking.
        let comments = parse_hunk_comments(r#"{"comments":[{"body":"general note"}]}"#);
        let out = format_hunk_comments(&comments);
        assert!(out.contains("(unknown file) — general note"));
    }

    #[test]
    fn review_prompt_embeds_the_comments() {
        let prompt = review_prompt("- rename foo\n- drop bar");
        assert!(prompt.contains("- rename foo"));
        assert!(prompt.contains("- drop bar"));
        assert!(prompt.contains("Please address them"));
    }
}
