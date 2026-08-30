use std::collections::HashMap;
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use anyhow::{Result, bail};

use crate::agent::AgentKind;

const SESSION_SEP: &str = "__";

/// Sentinel placed in the task slot of a tmux session name to mark an adhoc session.
/// Adhoc sessions belong to a project but no task; they run in the project dir.
pub const ADHOC_MARKER: &str = "adhoc";

pub fn is_adhoc_marker(s: &str) -> bool {
    sanitize(s).eq_ignore_ascii_case(ADHOC_MARKER)
}

#[derive(Debug, Clone, Copy, PartialEq, serde::Serialize, serde::Deserialize)]
pub enum SessionStatus {
    Running,
    WaitingForInput,
    WaitingForPermission,
    Finished,
}

impl SessionStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            SessionStatus::Running => "running",
            SessionStatus::WaitingForInput => "waiting_input",
            SessionStatus::WaitingForPermission => "waiting_permission",
            SessionStatus::Finished => "finished",
        }
    }
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct TmuxSession {
    pub name: String,
    pub project_name: String,
    pub task_name: String,
    pub session_name: String,
}

impl TmuxSession {
    /// Parse a tmux session name like `cm__project__task__session`.
    pub fn from_tmux_name(name: &str) -> Option<Self> {
        let rest = name.strip_prefix("cm")?;
        let rest = rest.strip_prefix(SESSION_SEP)?;
        let (project_name, rest) = rest.split_once(SESSION_SEP)?;
        let (task_name, session_name) = rest.split_once(SESSION_SEP)?;
        Some(TmuxSession {
            name: name.to_string(),
            project_name: project_name.to_string(),
            task_name: task_name.to_string(),
            session_name: session_name.to_string(),
        })
    }

    /// Returns the worktree path if this session has one.
    pub fn worktree_path(&self) -> Option<PathBuf> {
        let path = worktree_dir(&self.project_name, &self.task_name, &self.session_name);
        if path.exists() { Some(path) } else { None }
    }
}

/// Sanitize a name for use in tmux session names.
/// Replaces problematic characters and ensures no double underscores.
pub fn sanitize(s: &str) -> String {
    let s: String = s
        .chars()
        .map(|c| {
            if c.is_alphanumeric() || c == '-' {
                c
            } else {
                '-'
            }
        })
        .collect();
    // Collapse multiple hyphens
    let mut result = String::new();
    let mut prev_hyphen = false;
    for c in s.chars() {
        if c == '-' {
            if !prev_hyphen {
                result.push(c);
            }
            prev_hyphen = true;
        } else {
            result.push(c);
            prev_hyphen = false;
        }
    }
    result.trim_matches('-').replace("__", "_").to_string()
}

/// Generate a branch name from a task name.
pub fn to_branch_name(task_name: &str) -> String {
    let s: String = task_name
        .to_lowercase()
        .chars()
        .map(|c| if c.is_alphanumeric() { c } else { '-' })
        .collect();
    let mut result = String::new();
    let mut prev_hyphen = true; // skip leading hyphens
    for c in s.chars() {
        if c == '-' {
            if !prev_hyphen {
                result.push(c);
            }
            prev_hyphen = true;
        } else {
            result.push(c);
            prev_hyphen = false;
        }
    }
    result.trim_end_matches('-').to_string()
}

fn shell_escape(s: &str) -> String {
    format!("'{}'", s.replace('\'', "'\\''"))
}

fn build_tmux_name(project: &str, task: &str, session: &str) -> String {
    format!(
        "cm{sep}{}{sep}{}{sep}{}",
        sanitize(project),
        sanitize(task),
        sanitize(session),
        sep = SESSION_SEP
    )
}

/// The always-present first session of a task. Unlike other sessions it works
/// directly on the task branch instead of a `<task-branch>-<session>` branch.
pub const MAIN_SESSION: &str = "main";

pub fn is_main_session(session_name: &str) -> bool {
    session_name == MAIN_SESSION
}

pub fn worktree_dir(project_name: &str, task: &str, session: &str) -> PathBuf {
    crate::config::base_dir()
        .join("worktrees")
        .join(sanitize(project_name))
        .join(format!("{}-{}", sanitize(task), sanitize(session)))
}

pub fn list_sessions() -> Result<Vec<TmuxSession>> {
    let output = Command::new("tmux")
        .args(["list-sessions", "-F", "#{session_name}"])
        .output();

    let output = match output {
        Ok(o) => o,
        Err(_) => return Ok(vec![]),
    };

    if !output.status.success() {
        return Ok(vec![]);
    }

    let stdout = String::from_utf8_lossy(&output.stdout);
    Ok(stdout
        .lines()
        .filter_map(TmuxSession::from_tmux_name)
        .collect())
}

pub fn branch_exists(project_path: &str, branch: &str) -> bool {
    Command::new("git")
        .args([
            "-C",
            project_path,
            "rev-parse",
            "--verify",
            &format!("refs/heads/{branch}"),
        ])
        .output()
        .is_ok_and(|o| o.status.success())
}

/// List checkout-able branches for `project_path`: local branches first, then
/// remote-tracking branches reduced to their short name (so `git checkout <name>`
/// creates a local tracking branch). Remote duplicates of local branches and
/// the symbolic `origin/HEAD` are skipped. Ordering otherwise follows git's
/// most-recently-committed-first.
pub fn list_branches(project_path: &str) -> Vec<String> {
    let mut branches: Vec<String> = Vec::new();
    let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();

    // Local branches, most recent commit first.
    if let Ok(out) = Command::new("git")
        .args([
            "-C",
            project_path,
            "for-each-ref",
            "--sort=-committerdate",
            "--format=%(refname:short)",
            "refs/heads",
        ])
        .output()
    {
        for line in String::from_utf8_lossy(&out.stdout).lines() {
            let name = line.trim();
            if !name.is_empty() && seen.insert(name.to_string()) {
                branches.push(name.to_string());
            }
        }
    }

    // Remote branches, reduced to their short name (origin/foo -> foo).
    if let Ok(out) = Command::new("git")
        .args([
            "-C",
            project_path,
            "for-each-ref",
            "--sort=-committerdate",
            "--format=%(refname:short)",
            "refs/remotes",
        ])
        .output()
    {
        for line in String::from_utf8_lossy(&out.stdout).lines() {
            let full = line.trim();
            // Skip symbolic refs like `origin/HEAD`.
            if full.is_empty() || full.ends_with("/HEAD") {
                continue;
            }
            let short = full.split_once('/').map(|(_, rest)| rest).unwrap_or(full);
            if !short.is_empty() && seen.insert(short.to_string()) {
                branches.push(short.to_string());
            }
        }
    }

    branches
}

/// Fetch every remote (pruning deleted branches), then fast-forward the
/// currently checked-out branch. The fetch updates all remote-tracking refs;
/// the pull is best-effort (a detached HEAD, missing upstream, or diverged
/// branch leaves the fetch intact and is reported, not treated as failure).
pub fn fetch_pull_all(project_path: &str) -> Result<String> {
    let fetch = Command::new("git")
        .args(["-C", project_path, "fetch", "--all", "--prune"])
        .output()?;
    if !fetch.status.success() {
        let stderr = String::from_utf8_lossy(&fetch.stderr);
        bail!("Fetch failed: {}", stderr.trim());
    }

    let pull = Command::new("git")
        .args(["-C", project_path, "pull", "--ff-only"])
        .output();
    match pull {
        Ok(o) if o.status.success() => {
            Ok("Fetched all remotes; fast-forwarded current branch".to_string())
        }
        _ => Ok("Fetched all remotes (current branch not fast-forwarded)".to_string()),
    }
}

/// Pull the latest base branch (default "main") and create a task branch from it.
pub fn create_task_branch(project_path: &str, branch_name: &str, base: Option<&str>) -> Result<()> {
    let base = base.unwrap_or("main");
    // Try to fetch the latest base from origin
    let _ = Command::new("git")
        .args(["-C", project_path, "fetch", "origin", base])
        .output();

    // Try creating from origin/<base> first, fall back to the local base.
    // `--no-track` prevents inheriting origin/<base> as the upstream — once the
    // branch is pushed, `push -u` will set it to track origin/<branch_name>.
    let status = Command::new("git")
        .args([
            "-C",
            project_path,
            "branch",
            "--no-track",
            branch_name,
            &format!("origin/{base}"),
        ])
        .output()?;

    if !status.status.success() {
        let output = Command::new("git")
            .args([
                "-C",
                project_path,
                "branch",
                "--no-track",
                branch_name,
                base,
            ])
            .output()?;
        if !output.status.success() {
            bail!("Failed to create branch {branch_name} from {base}");
        }
    }

    Ok(())
}

pub fn create_session(
    project_name: &str,
    project_path: &str,
    task_name: &str,
    task_branch: &str,
    session_name: &str,
    use_worktree: bool,
    copy_patterns: &[String],
    setup_commands: &[String],
    initial_prompt: Option<&str>,
    startup_skills: &[String],
    agent: AgentKind,
) -> Result<String> {
    let tmux_name = build_tmux_name(project_name, task_name, session_name);

    let work_dir;
    let mut worktree_path_str = String::new();

    if use_worktree {
        let wt_path = worktree_dir(project_name, task_name, session_name);
        worktree_path_str = wt_path.to_string_lossy().to_string();

        // Create parent directories
        if let Some(parent) = wt_path.parent() {
            fs::create_dir_all(parent)?;
        }

        // The main session works on the task branch itself; every other session
        // gets its own branch forked off it.
        let mut args = vec!["-C", project_path, "worktree", "add"];
        let session_branch = format!("{task_branch}-{}", sanitize(session_name));
        if !is_main_session(session_name) {
            args.extend(["-b", &session_branch]);
        }
        args.extend([worktree_path_str.as_str(), task_branch]);
        let status = Command::new("git").args(&args).output()?;

        if !status.status.success() {
            let stderr = String::from_utf8_lossy(&status.stderr);
            bail!("Failed to create worktree: {stderr}");
        }

        // Always copy .claude/ folder, plus any configured patterns (sync, before hooks setup)
        let mut all_patterns = vec![".claude/***".to_string()];
        all_patterns.extend_from_slice(copy_patterns);
        copy_patterns_to_worktree(project_path, &worktree_path_str, &all_patterns);

        // Run setup commands in the new worktree if configured
        for cmd in setup_commands {
            let output = Command::new("sh")
                .args(["-c", cmd])
                .current_dir(&worktree_path_str)
                .output()?;

            if !output.status.success() {
                let stderr = String::from_utf8_lossy(&output.stderr);
                bail!("Setup command failed: {stderr}\nCommand: {cmd}");
            }
        }

        work_dir = worktree_path_str.clone();
    } else {
        work_dir = project_path.to_string();
    }

    // Always install the showrunner skills (Claude plugin / .agents/skills)
    install_agent_skills(agent, &work_dir);

    let session_branch = if use_worktree && !is_main_session(session_name) {
        Some(format!("{task_branch}-{}", sanitize(session_name)))
    } else {
        None
    };

    let system_prompt = build_base_system_prompt(
        project_name,
        task_branch,
        session_branch.as_deref(),
        is_main_session(session_name),
    );

    let agent_cmd = build_agent_command(
        agent,
        &work_dir,
        Some(&system_prompt),
        build_initial_prompt(startup_skills, initial_prompt, agent).as_deref(),
        false,
    );

    let output = Command::new("tmux")
        .args([
            "new-session",
            "-d",
            "-s",
            &tmux_name,
            "-c",
            &work_dir,
            &agent_cmd,
        ])
        .output()?;

    if !output.status.success() {
        bail!("Failed to create tmux session");
    }

    // Store metadata in tmux environment for cleanup
    let _ = Command::new("tmux")
        .args([
            "set-environment",
            "-t",
            &tmux_name,
            "CM_PROJECT_PATH",
            project_path,
        ])
        .output();

    // Store the task branch so we can diff against it later
    let _ = Command::new("tmux")
        .args([
            "set-environment",
            "-t",
            &tmux_name,
            "CM_TASK_BRANCH",
            task_branch,
        ])
        .output();

    set_session_env(&tmux_name, agent);

    if use_worktree {
        let _ = Command::new("tmux")
            .args([
                "set-environment",
                "-t",
                &tmux_name,
                "CM_WORKTREE_PATH",
                &worktree_path_str,
            ])
            .output();
    }

    Ok(tmux_name)
}

/// Create an adhoc session: tmux session running Claude in the project directory
/// on whatever branch is currently checked out, with no task or worktree.
/// Applies `startup_skills` if any, but sends no user prompt.
pub fn create_adhoc_session(
    project_name: &str,
    project_path: &str,
    session_name: &str,
    startup_skills: &[String],
    agent: AgentKind,
) -> Result<String> {
    let tmux_name = build_tmux_name(project_name, ADHOC_MARKER, session_name);

    let agent_cmd = build_agent_command(
        agent,
        project_path,
        None,
        build_initial_prompt(startup_skills, None, agent).as_deref(),
        false,
    );

    let output = Command::new("tmux")
        .args([
            "new-session",
            "-d",
            "-s",
            &tmux_name,
            "-c",
            project_path,
            &agent_cmd,
        ])
        .output()?;

    if !output.status.success() {
        bail!("Failed to create tmux session");
    }

    let _ = Command::new("tmux")
        .args([
            "set-environment",
            "-t",
            &tmux_name,
            "CM_PROJECT_PATH",
            project_path,
        ])
        .output();

    set_session_env(&tmux_name, agent);

    Ok(tmux_name)
}

/// Recreate an adhoc tmux session from a saved record (e.g. after tmux dies).
pub fn recreate_adhoc_session(
    tmux_name: &str,
    record: &crate::config::SessionRecord,
) -> Result<String> {
    let agent = record.agent_kind();
    let agent_cmd = build_agent_command(agent, &record.project_path, None, None, true);

    let output = Command::new("tmux")
        .args([
            "new-session",
            "-d",
            "-s",
            tmux_name,
            "-c",
            &record.project_path,
            &agent_cmd,
        ])
        .output()?;

    if !output.status.success() {
        bail!("Failed to recreate adhoc tmux session");
    }

    let _ = Command::new("tmux")
        .args([
            "set-environment",
            "-t",
            tmux_name,
            "CM_PROJECT_PATH",
            &record.project_path,
        ])
        .output();

    set_session_env(tmux_name, agent);

    Ok(tmux_name.to_string())
}

/// Recreate a tmux session from a saved record (e.g. after tmux dies) under the
/// record's own `tmux_name`. Reuses the existing worktree if present; does NOT
/// send an initial prompt.
pub fn recreate_session(tmux_name: &str, record: &crate::config::SessionRecord) -> Result<String> {
    let work_dir = if record.use_worktree {
        let wt_path = worktree_dir(
            &record.project_name,
            &record.task_name,
            &record.session_name,
        );
        if wt_path.exists() {
            wt_path.to_string_lossy().to_string()
        } else {
            // Worktree is gone — cannot recreate this session
            bail!(
                "Worktree no longer exists for session {}",
                record.session_name
            );
        }
    } else {
        record.project_path.clone()
    };

    let agent = record.agent_kind();

    // Always install the showrunner skills
    install_agent_skills(agent, &work_dir);

    let session_branch = if record.use_worktree && !is_main_session(&record.session_name) {
        Some(format!(
            "{}-{}",
            record.task_branch,
            sanitize(&record.session_name)
        ))
    } else {
        None
    };

    let system_prompt = build_base_system_prompt(
        &record.project_name,
        &record.task_branch,
        session_branch.as_deref(),
        is_main_session(&record.session_name),
    );

    let agent_cmd = build_agent_command(agent, &work_dir, Some(&system_prompt), None, true);

    let output = Command::new("tmux")
        .args([
            "new-session",
            "-d",
            "-s",
            tmux_name,
            "-c",
            &work_dir,
            &agent_cmd,
        ])
        .output()?;

    if !output.status.success() {
        bail!("Failed to create tmux session for recreation");
    }

    // Restore environment variables
    let _ = Command::new("tmux")
        .args([
            "set-environment",
            "-t",
            tmux_name,
            "CM_PROJECT_PATH",
            &record.project_path,
        ])
        .output();

    let _ = Command::new("tmux")
        .args([
            "set-environment",
            "-t",
            tmux_name,
            "CM_TASK_BRANCH",
            &record.task_branch,
        ])
        .output();

    set_session_env(tmux_name, agent);

    if record.use_worktree {
        let _ = Command::new("tmux")
            .args([
                "set-environment",
                "-t",
                tmux_name,
                "CM_WORKTREE_PATH",
                &work_dir,
            ])
            .output();
    }

    Ok(tmux_name.to_string())
}

/// Insert text into the claude pane's input buffer as a bracketed paste.
/// If `submit` is true, also presses Enter afterwards.
pub fn send_text(session_name: &str, text: &str, submit: bool) -> Result<()> {
    let target = format!("{session_name}:0");
    let buf_name = "cm_comment_paste";

    let mut child = Command::new("tmux")
        .args(["load-buffer", "-b", buf_name, "-"])
        .stdin(Stdio::piped())
        .spawn()?;
    if let Some(mut stdin) = child.stdin.take() {
        stdin.write_all(text.as_bytes())?;
    }
    let status = child.wait()?;
    if !status.success() {
        bail!("tmux load-buffer failed");
    }

    // -p: bracketed paste, -d: delete buffer after pasting.
    let out = Command::new("tmux")
        .args(["paste-buffer", "-d", "-p", "-b", buf_name, "-t", &target])
        .output()?;
    if !out.status.success() {
        let stderr = String::from_utf8_lossy(&out.stderr);
        bail!("tmux paste-buffer failed: {}", stderr.trim());
    }

    if submit {
        let out = Command::new("tmux")
            .args(["send-keys", "-t", &target, "Enter"])
            .output()?;
        if !out.status.success() {
            let stderr = String::from_utf8_lossy(&out.stderr);
            bail!("tmux send-keys Enter failed: {}", stderr.trim());
        }
    }
    Ok(())
}

/// Capture the last `lines` lines (including scrollback) of a session's Claude
/// pane (window 0) with ANSI escape sequences, plus the pane width in columns.
pub fn capture_output(session_name: &str, lines: usize) -> Option<(String, usize)> {
    let target = format!("{session_name}:0");
    let output = Command::new("tmux")
        .args([
            "capture-pane",
            "-p",
            "-e",
            "-t",
            &target,
            "-S",
            &format!("-{lines}"),
        ])
        .output()
        .ok()?;

    if !output.status.success() {
        return None;
    }

    let width = Command::new("tmux")
        .args(["display-message", "-p", "-t", &target, "#{pane_width}"])
        .output()
        .ok()
        .filter(|o| o.status.success())
        .and_then(|o| String::from_utf8_lossy(&o.stdout).trim().parse().ok())
        .unwrap_or(80);

    Some((String::from_utf8_lossy(&output.stdout).to_string(), width))
}

/// Capture the last `lines` lines (including scrollback) of a session's Claude
/// pane as plain text, without ANSI escape sequences.
pub fn capture_output_plain(session_name: &str, lines: usize) -> Option<String> {
    let target = format!("{session_name}:0");
    let output = Command::new("tmux")
        .args([
            "capture-pane",
            "-p",
            "-t",
            &target,
            "-S",
            &format!("-{lines}"),
        ])
        .output()
        .ok()?;

    if !output.status.success() {
        return None;
    }

    Some(String::from_utf8_lossy(&output.stdout).to_string())
}

/// Send a single tmux key name (e.g. "Enter", "Escape", "Up", "1") to the
/// Claude pane.
pub fn send_key(session_name: &str, key: &str) -> Result<()> {
    let target = format!("{session_name}:0");
    let out = Command::new("tmux")
        .args(["send-keys", "-t", &target, key])
        .output()?;
    if !out.status.success() {
        let stderr = String::from_utf8_lossy(&out.stderr);
        bail!("tmux send-keys failed: {}", stderr.trim());
    }
    Ok(())
}

pub fn attach_session(name: &str) -> Result<()> {
    // Select window 0 (claude) before attaching
    let _ = Command::new("tmux")
        .args(["select-window", "-t", &format!("{name}:0")])
        .output();

    let status = Command::new("tmux")
        .args(["attach-session", "-t", name])
        .status()?;

    if !status.success() {
        bail!("Failed to attach to tmux session");
    }

    Ok(())
}

/// Attach to a specific window of a session (selects it first).
pub fn attach_session_window(session_name: &str, window_idx: usize) -> Result<()> {
    let _ = Command::new("tmux")
        .args([
            "select-window",
            "-t",
            &format!("{session_name}:{window_idx}"),
        ])
        .output();

    let status = Command::new("tmux")
        .args(["attach-session", "-t", session_name])
        .status()?;

    if !status.success() {
        bail!("Failed to attach to tmux session");
    }
    Ok(())
}

pub fn count_terminal_windows(session_name: &str) -> usize {
    let output = Command::new("tmux")
        .args(["list-windows", "-t", session_name, "-F", "#{window_index}"])
        .output();
    match output {
        Ok(o) if o.status.success() => String::from_utf8_lossy(&o.stdout)
            .lines()
            .filter(|line| line.trim().parse::<usize>().is_ok_and(|i| i > 0))
            .count(),
        _ => 0,
    }
}

/// All tmux session names on the current server (not only showrunner-managed
/// `cm__*` sessions), used for per-session resource monitoring.
pub fn list_all_tmux_sessions() -> Vec<String> {
    let output = Command::new("tmux")
        .args(["list-sessions", "-F", "#{session_name}"])
        .output();

    let output = match output {
        Ok(o) => o,
        Err(_) => return vec![],
    };

    if !output.status.success() {
        return vec![];
    }

    let stdout = String::from_utf8_lossy(&output.stdout);
    stdout
        .lines()
        .map(|line| line.trim().to_string())
        .filter(|name| !name.is_empty())
        .collect()
}

pub fn list_pane_pids(session_name: &str) -> Vec<u32> {
    let output = Command::new("tmux")
        .args(["list-panes", "-t", session_name, "-F", "#{pane_pid}"])
        .output();

    let output = match output {
        Ok(o) => o,
        Err(_) => return vec![],
    };

    if !output.status.success() {
        return vec![];
    }

    let stdout = String::from_utf8_lossy(&output.stdout);
    stdout
        .lines()
        .filter_map(|line| line.trim().parse::<u32>().ok())
        .collect()
}

/// Pane PIDs for every tmux session in ONE `tmux list-panes -a` call (keyed by
/// session name). Avoids the N process spawns of calling `list_pane_pids` per
/// session — used by the batched resource sampler.
pub fn list_all_pane_pids() -> HashMap<String, Vec<u32>> {
    let output = Command::new("tmux")
        .args(["list-panes", "-a", "-F", "#{session_name}\t#{pane_pid}"])
        .output();
    let output = match output {
        Ok(o) => o,
        Err(_) => return HashMap::new(),
    };
    if !output.status.success() {
        return HashMap::new();
    }
    let stdout = String::from_utf8_lossy(&output.stdout);
    let mut map: HashMap<String, Vec<u32>> = HashMap::new();
    for line in stdout.lines() {
        let mut it = line.splitn(2, '\t');
        let session = it.next().unwrap_or_default();
        let pid = it.next().and_then(|p| p.trim().parse::<u32>().ok());
        if let Some(pid) = pid {
            map.entry(session.to_string()).or_default().push(pid);
        }
    }
    map
}

/// Create a terminal window in the session rooted at `work_dir`. Returns its
/// window index.
pub fn create_terminal_window(session_name: &str, work_dir: &str) -> Result<usize> {
    let output = Command::new("tmux")
        .args([
            "new-window",
            "-t",
            session_name,
            "-c",
            work_dir,
            "-P",
            "-F",
            "#{window_index}",
        ])
        .output()?;

    if !output.status.success() {
        bail!("Failed to create terminal window");
    }

    let idx = String::from_utf8_lossy(&output.stdout)
        .trim()
        .parse::<usize>()
        .unwrap_or(1);
    Ok(idx)
}

/// Launch `command` in a dedicated, detached tmux session rooted at `work_dir`
/// and return its tmux session name. Any prior run session with the same name
/// is replaced so "Run" always starts fresh. The shell stays alive after the
/// command exits so its output (and any error) remains visible on attach. The
/// `cmrun-` name prefix is deliberately unparseable by `from_tmux_name`, so run
/// sessions never appear in the managed session list.
/// tmux session name hosting the run command for `label`. Shared by the
/// launcher and the UI so an item can find its own run session.
pub fn run_session_name(label: &str) -> String {
    format!("cmrun-{}", sanitize(label))
}

/// Live run sessions (`cmrun-*`) mapped to whether their command is still
/// executing (`true`) versus having dropped to an interactive shell (`false`,
/// i.e. the command finished). Uses one `list-panes` call across all sessions.
pub fn list_run_sessions() -> HashMap<String, bool> {
    let mut map = HashMap::new();
    let output = Command::new("tmux")
        .args([
            "list-panes",
            "-a",
            "-F",
            "#{session_name}\t#{pane_current_command}",
        ])
        .output();
    if let Ok(o) = output
        && o.status.success()
    {
        for line in String::from_utf8_lossy(&o.stdout).lines() {
            if let Some((name, cmd)) = line.split_once('\t')
                && name.starts_with("cmrun-")
            {
                let active = !is_shell_command(cmd);
                map.entry(name.to_string())
                    .and_modify(|v| *v |= active)
                    .or_insert(active);
            }
        }
    }
    map
}

/// Whether `cmd` (a tmux `pane_current_command`) is an interactive shell, i.e.
/// the run command has finished and left the keep-alive shell in the foreground.
fn is_shell_command(cmd: &str) -> bool {
    matches!(
        cmd.trim_start_matches('-'),
        "sh" | "bash" | "zsh" | "fish" | "dash" | "ksh" | "tcsh" | "csh"
    )
}

pub fn run_command_session(label: &str, work_dir: &str, command: &str) -> Result<String> {
    let tmux_name = run_session_name(label);

    // Replace any existing run session so re-running restarts the command.
    let _ = Command::new("tmux")
        .args(["kill-session", "-t", &tmux_name])
        .output();

    let shell_cmd = format!("{command}; exec \"${{SHELL:-/bin/sh}}\"");
    let output = Command::new("tmux")
        .args([
            "new-session",
            "-d",
            "-s",
            &tmux_name,
            "-c",
            work_dir,
            "sh",
            "-c",
            &shell_cmd,
        ])
        .output()?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        bail!("Failed to start run session: {}", stderr.trim());
    }

    Ok(tmux_name)
}

/// Record which agent runs in the session, for status probing and transcript
/// trimming after the fact.
/// Set the per-session showrunner environment: the agent id, and the daemon
/// port so the plugin's hook script (`post-event.sh`) can reach the daemon even
/// when `SESSIONIZER_PORT` isn't otherwise in the pane environment.
fn set_session_env(tmux_name: &str, agent: AgentKind) {
    let _ = Command::new("tmux")
        .args(["set-environment", "-t", tmux_name, "CM_AGENT", agent.id()])
        .output();
    let port = std::env::var("SESSIONIZER_PORT").unwrap_or_else(|_| "7878".to_string());
    let _ = Command::new("tmux")
        .args(["set-environment", "-t", tmux_name, "SESSIONIZER_PORT", port.as_str()])
        .output();
}

/// The agent running in a session. Sessions created before agents were
/// tracked have no `CM_AGENT` and default to Claude.
pub fn session_agent(session_name: &str) -> AgentKind {
    get_session_env(session_name, "CM_AGENT")
        .and_then(|id| AgentKind::from_id(id.trim()))
        .unwrap_or_default()
}

/// Assemble the shell command that launches (or resumes) the agent for a
/// session. `system_prompt` is the session-context briefing — present for task
/// sessions, absent for adhoc ones; for Claude it also implies loading the
/// showrunner plugin. Agents without a system-prompt flag get it prepended to
/// the first message instead.
fn build_agent_command(
    agent: AgentKind,
    work_dir: &str,
    system_prompt: Option<&str>,
    initial_prompt: Option<&str>,
    resume: bool,
) -> String {
    match agent {
        AgentKind::Claude => {
            let mut cmd = String::from("claude --dangerously-skip-permissions");
            if resume {
                cmd.push_str(" --continue");
            }
            if system_prompt.is_some() {
                cmd.push_str(&format!(
                    " --plugin-dir {}",
                    shell_escape(&showrunner_plugin_path(work_dir))
                ));
            }
            if let Some(sp) = system_prompt {
                cmd.push_str(&format!(" --append-system-prompt {}", shell_escape(sp)));
            }
            if let Some(prompt) = initial_prompt {
                cmd.push(' ');
                cmd.push_str(&shell_escape(prompt));
            }
            cmd
        }
        AgentKind::Codex => {
            // Codex shows a per-directory trust dialog on first launch, even
            // in yolo mode — pre-trust the work dir so sessions never stall.
            ensure_codex_trust(work_dir);
            let mut cmd = String::from("codex");
            if resume {
                // cwd-scoped: resumes the most recent session for this work dir.
                cmd.push_str(" resume --last");
            }
            cmd.push_str(" --dangerously-bypass-approvals-and-sandbox");
            if !resume {
                let prompt = match (system_prompt, initial_prompt) {
                    (Some(sp), Some(p)) => Some(format!("{sp}\n\n{p}")),
                    (Some(sp), None) => Some(sp.to_string()),
                    (None, p) => p.map(str::to_string),
                };
                if let Some(prompt) = prompt {
                    cmd.push(' ');
                    cmd.push_str(&shell_escape(&prompt));
                }
            }
            cmd
        }
        AgentKind::Pi => {
            // Pi has no per-command approvals; `--approve` pre-trusts the
            // project-local files we inject (.agents/skills) for this run so
            // the one-time trust dialog never blocks the session.
            let mut cmd = String::from("pi --approve");
            if resume {
                // cwd-scoped: continues the most recent session for this work dir.
                cmd.push_str(" --continue");
            }
            if let Some(sp) = system_prompt {
                cmd.push_str(&format!(" --append-system-prompt {}", shell_escape(sp)));
            }
            if let Some(prompt) = initial_prompt {
                cmd.push(' ');
                cmd.push_str(&shell_escape(prompt));
            }
            cmd
        }
        AgentKind::OpenCode => {
            // OpenCode's TUI positional is the *project directory*, not a
            // message — the prompt goes through `--prompt`. `--auto` mirrors the
            // other agents' yolo launch; without it opencode stalls on every
            // permission dialog.
            let mut cmd = String::from("opencode --auto");
            if resume {
                // cwd-scoped: continues the last session for this work dir.
                cmd.push_str(" --continue");
            }
            if !resume {
                let prompt = match (system_prompt, initial_prompt) {
                    (Some(sp), Some(p)) => Some(format!("{sp}\n\n{p}")),
                    (Some(sp), None) => Some(sp.to_string()),
                    (None, p) => p.map(str::to_string),
                };
                if let Some(prompt) = prompt {
                    cmd.push_str(&format!(" --prompt {}", shell_escape(&prompt)));
                }
            }
            cmd
        }
    }
}

/// Ensure `work_dir` is marked trusted in `~/.codex/config.toml`, appending a
/// `[projects."<dir>"]` entry if missing (a `-c` override does not skip the
/// trust dialog; the config file entry does).
fn ensure_codex_trust(work_dir: &str) {
    let Some(home) = dirs::home_dir() else {
        return;
    };
    let config_path = home.join(".codex").join("config.toml");
    let content = fs::read_to_string(&config_path).unwrap_or_default();
    let header = format!("[projects.\"{work_dir}\"]");
    if content.contains(&header) {
        return;
    }
    let _ = fs::create_dir_all(config_path.parent().unwrap());
    let mut updated = content;
    if !updated.is_empty() && !updated.ends_with('\n') {
        updated.push('\n');
    }
    updated.push_str(&format!("\n{header}\ntrust_level = \"trusted\"\n"));
    let _ = fs::write(&config_path, updated);
}

fn get_session_env(session_name: &str, var: &str) -> Option<String> {
    let output = Command::new("tmux")
        .args(["show-environment", "-t", session_name, var])
        .output()
        .ok()?;

    if !output.status.success() {
        return None;
    }

    let line = String::from_utf8_lossy(&output.stdout);
    line.trim().split_once('=').map(|(_, v)| v.to_string())
}

/// Fallback paths for cleaning up a session when the tmux session is already dead
/// and environment variables are unavailable.
pub struct SessionCleanupInfo {
    pub project_path: String,
    pub worktree_path: String,
    /// The branch checked out in the worktree (e.g. "task-branch-session-name").
    /// If not provided, it will be derived from the worktree before removal.
    pub branch_name: Option<String>,
    /// The task branch, which is never deleted (the main session has it checked out).
    pub task_branch: Option<String>,
}

pub fn kill_session(name: &str) -> Result<()> {
    kill_session_with_fallback(name, None)
}

/// Kill the tmux session only — leave worktrees, branches, and session records intact.
/// Used when archiving a task so the session can be recreated later.
pub fn kill_session_only(name: &str) -> Result<()> {
    let _ = Command::new("tmux")
        .args(["kill-session", "-t", name])
        .output();
    Ok(())
}

pub fn kill_session_with_fallback(name: &str, fallback: Option<SessionCleanupInfo>) -> Result<()> {
    // Try to get paths from tmux env vars first, fall back to provided info
    let project_path = get_session_env(name, "CM_PROJECT_PATH")
        .or_else(|| fallback.as_ref().map(|f| f.project_path.clone()));
    let worktree_path = get_session_env(name, "CM_WORKTREE_PATH")
        .or_else(|| fallback.as_ref().map(|f| f.worktree_path.clone()));
    let task_branch_fallback = fallback.as_ref().and_then(|f| f.task_branch.clone());

    // Kill the tmux session (ignore errors — it may already be dead)
    let _ = Command::new("tmux")
        .args(["kill-session", "-t", name])
        .output();

    // Clean up worktree and its branch if applicable
    if let (Some(proj_path), Some(wt_path)) = (project_path, worktree_path) {
        // Get the branch name before removing the worktree
        let branch = Command::new("git")
            .args(["-C", &wt_path, "rev-parse", "--abbrev-ref", "HEAD"])
            .output()
            .ok()
            .filter(|o| o.status.success())
            .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
            .or_else(|| fallback.and_then(|f| f.branch_name));

        if Path::new(&wt_path).exists() {
            let _ = Command::new("git")
                .args(["-C", &proj_path, "worktree", "remove", "--force", &wt_path])
                .output();
        }

        // Prune stale worktree references so git no longer considers the branch checked out
        let _ = Command::new("git")
            .args(["-C", &proj_path, "worktree", "prune"])
            .output();

        // Delete the worktree branch — never the task branch, which the main
        // session has checked out.
        let task_branch = get_session_env(name, "CM_TASK_BRANCH").or(task_branch_fallback);
        if let Some(branch_name) = branch {
            if !branch_name.is_empty()
                && branch_name != "main"
                && branch_name != "master"
                && Some(&branch_name) != task_branch.as_ref()
            {
                let _ = Command::new("git")
                    .args(["-C", &proj_path, "branch", "-D", &branch_name])
                    .output();
            }
        }
    }

    Ok(())
}

/// Copy specific file patterns from the project into a new worktree.
/// Patterns can be files (`.env`) or directories (`build/`).
fn copy_patterns_to_worktree(project_path: &str, worktree_path: &str, patterns: &[String]) {
    let src = if project_path.ends_with('/') {
        project_path.to_string()
    } else {
        format!("{project_path}/")
    };

    let dst = if worktree_path.ends_with('/') {
        worktree_path.to_string()
    } else {
        format!("{worktree_path}/")
    };

    let mut args = vec!["-a".to_string()];
    for pattern in patterns {
        args.push("--include".to_string());
        args.push(pattern.to_string());
    }
    // Exclude everything not matched
    args.push("--exclude".to_string());
    args.push("*".to_string());
    args.push(src);
    args.push(dst);

    let _ = Command::new("rsync")
        .args(&args)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .output();
}

/// Build the base system prompt that all sessions receive. `worktree_branch` is
/// the session's own branch, or `None` for the main session (which works on the
/// task branch) and for sessions running without a worktree.
fn build_base_system_prompt(
    project_name: &str,
    task_branch: &str,
    worktree_branch: Option<&str>,
    is_main: bool,
) -> String {
    let mut prompt = format!(
        "You have been spawned as a session agent by Showrunner, a multi-agent task management tool.\n\
         \n\
         - Project: {project_name}\n\
         - Task branch: {task_branch}\n"
    );
    if let Some(wt_branch) = worktree_branch {
        prompt.push_str(&format!(
            "- Worktree branch: {wt_branch}\n\
             - You are on your own branch; the task branch is checked out in the task's\n  \
             main session worktree, so never check it out here — merge into it instead\n  \
             (the `commit-push-task` skill does this correctly)\n"
        ));
    } else if is_main {
        prompt.push_str(
            "- You are the task's main session and work on the task branch directly:\n  \
             commit here, there is no merge step\n",
        );
    }
    prompt.push_str(&format!(
        "- PRs should always be opened from the task branch: {task_branch}\n\
         - Other agents may be working on the same task in parallel\n\
         - The `showrunner` CLI lets you list, create and manage other tasks and\n  \
         sessions, and ask an agent in another session a question — see the\n  \
         `manage-sessions` skill"
    ));
    if worktree_branch.is_some() {
        prompt.push_str("\n- NEVER push the worktree branch unless explicitly told to do so");
    }
    prompt
}

/// Build the combined initial prompt from startup skills and optional user prompt.
/// Returns `None` if both are empty.
fn build_initial_prompt(
    startup_skills: &[String],
    user_prompt: Option<&str>,
    agent: AgentKind,
) -> Option<String> {
    let has_skills = !startup_skills.is_empty();
    let has_prompt = user_prompt.is_some_and(|p| !p.is_empty());

    if !has_skills && !has_prompt {
        return None;
    }

    if !has_skills {
        return user_prompt.map(String::from);
    }

    let skills_list: String = startup_skills
        .iter()
        .enumerate()
        .map(|(i, s)| format!("{}. {s}", i + 1))
        .collect::<Vec<_>>()
        .join("\n");

    // Claude runs these via its Skill tool; other agents get the list as-is
    // (skills they discover are invoked by name, unknown ones are skipped).
    let how = match agent {
        AgentKind::Claude => "(one at a time, using the Skill tool)",
        _ => "(one at a time; skip any that aren't available to you)",
    };

    if !has_prompt {
        return Some(format!(
            "Run these startup skills first {how}:\n{skills_list}"
        ));
    }

    Some(format!(
        "Run these startup skills first {how}, then proceed with the task below:\n\
         {skills_list}\n\n\
         Task: {}",
        user_prompt.unwrap()
    ))
}

// Embedded showrunner plugin files (see showrunner-plugin/ at repo root).
const PLUGIN_MANIFEST: &str = include_str!("../showrunner-plugin/.claude-plugin/plugin.json");
const PLUGIN_SKILL_COMMIT_PUSH_TASK: &str =
    include_str!("../showrunner-plugin/skills/commit-push-task/SKILL.md");
const PLUGIN_SKILL_MANAGE_SESSIONS: &str =
    include_str!("../showrunner-plugin/skills/manage-sessions/SKILL.md");
const PLUGIN_HOOKS_JSON: &str = include_str!("../showrunner-plugin/hooks/hooks.json");
const PLUGIN_HOOKS_POST_EVENT: &str =
    include_str!("../showrunner-plugin/hooks/post-event.sh");

/// Filesystem path to the installed showrunner plugin directory inside `work_dir`.
/// This is the path passed to `claude --plugin-dir`.
fn showrunner_plugin_path(work_dir: &str) -> String {
    Path::new(work_dir)
        .join(".claude")
        .join("plugins")
        .join("showrunner")
        .to_string_lossy()
        .to_string()
}

/// Install the showrunner skills in whatever form the agent discovers them:
/// a Claude Code plugin for Claude, plain SKILL.md folders under
/// `.agents/skills/` (the cross-agent location) for everyone else.
fn install_agent_skills(agent: AgentKind, work_dir: &str) {
    if agent.supports_plugin_dir() {
        install_showrunner_plugin(work_dir);
    } else {
        install_agents_dir_skills(work_dir);
    }
}

/// Install the two showrunner skills as plain SKILL.md folders under
/// `<work_dir>/.agents/skills/`, which codex (and pi) scan from the cwd up.
fn install_agents_dir_skills(work_dir: &str) {
    let skills_dir = Path::new(work_dir).join(".agents").join("skills");
    for (name, content) in [
        ("commit-push-task", PLUGIN_SKILL_COMMIT_PUSH_TASK),
        ("manage-sessions", PLUGIN_SKILL_MANAGE_SESSIONS),
    ] {
        let dir = skills_dir.join(name);
        let _ = fs::create_dir_all(&dir);
        let _ = fs::write(dir.join("SKILL.md"), content);
    }
    // Exclude only our own skill folders so a repo's own .agents/skills stay
    // visible to git.
    add_git_excludes(
        work_dir,
        &[
            ".agents/skills/commit-push-task/",
            ".agents/skills/manage-sessions/",
        ],
    );
}

/// Install the bundled showrunner plugin into the work directory's
/// `.claude/plugins/showrunner/`. The plugin is loaded at session start via
/// `claude --plugin-dir <path>` (see `showrunner_plugin_path`).
fn install_showrunner_plugin(work_dir: &str) {
    // Remove the update-task-context skill that older versions installed
    // (standalone under `.claude/skills/` and inside the plugin) — the shared
    // task context concept no longer exists.
    let legacy_skill_dir = Path::new(work_dir)
        .join(".claude")
        .join("skills")
        .join("update-task-context");
    let _ = fs::remove_dir_all(&legacy_skill_dir);

    // Remove the plugin installed under its pre-rename name (claude-manager).
    let _ = fs::remove_dir_all(
        Path::new(work_dir)
            .join(".claude")
            .join("plugins")
            .join("claude-manager"),
    );

    let plugin_dir = PathBuf::from(showrunner_plugin_path(work_dir));
    // Also drop skills removed in later versions.
    for stale in ["update-task-context", "stacked-pr"] {
        let _ = fs::remove_dir_all(plugin_dir.join("skills").join(stale));
    }

    let _ = fs::create_dir_all(plugin_dir.join(".claude-plugin"));
    let _ = fs::create_dir_all(plugin_dir.join("skills").join("commit-push-task"));
    let _ = fs::create_dir_all(plugin_dir.join("skills").join("manage-sessions"));
    let _ = fs::create_dir_all(plugin_dir.join("hooks"));

    let _ = fs::write(
        plugin_dir.join(".claude-plugin").join("plugin.json"),
        PLUGIN_MANIFEST,
    );
    // The manifest references ./hooks/hooks.json — both files must be present or
    // Claude Code fails to load the plugin (which would also drop the skills).
    let _ = fs::write(plugin_dir.join("hooks").join("hooks.json"), PLUGIN_HOOKS_JSON);
    let _ = fs::write(
        plugin_dir.join("hooks").join("post-event.sh"),
        PLUGIN_HOOKS_POST_EVENT,
    );
    let _ = fs::write(
        plugin_dir
            .join("skills")
            .join("commit-push-task")
            .join("SKILL.md"),
        PLUGIN_SKILL_COMMIT_PUSH_TASK,
    );
    let _ = fs::write(
        plugin_dir
            .join("skills")
            .join("manage-sessions")
            .join("SKILL.md"),
        PLUGIN_SKILL_MANAGE_SESSIONS,
    );

    // Git-ignore the locally installed plugin via .git/info/exclude.
    add_git_excludes(work_dir, &[".claude/plugins/showrunner/"]);
}

/// Add entries to the repo's `.git/info/exclude`, skipping any already
/// present. Git only reads `info/exclude` from the COMMON git dir — a linked
/// worktree's own `worktrees/<name>/info/exclude` is ignored — so resolve the
/// common dir; the slash-anchored patterns then apply to the project dir and
/// every worktree alike.
fn add_git_excludes(work_dir: &str, exclude_entries: &[&str]) {
    let real_git_dir = Command::new("git")
        .args([
            "-C",
            work_dir,
            "rev-parse",
            "--path-format=absolute",
            "--git-common-dir",
        ])
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| PathBuf::from(String::from_utf8_lossy(&o.stdout).trim()));
    if let Some(gd) = real_git_dir {
        let info_dir = gd.join("info");
        let _ = fs::create_dir_all(&info_dir);
        let exclude_path = info_dir.join("exclude");
        let mut content = fs::read_to_string(&exclude_path).unwrap_or_default();
        let mut changed = false;
        for entry in exclude_entries {
            if !content.lines().any(|l| l.trim() == *entry) {
                if !content.ends_with('\n') && !content.is_empty() {
                    content.push('\n');
                }
                content.push_str(entry);
                content.push('\n');
                changed = true;
            }
        }
        if changed {
            let _ = fs::write(&exclude_path, content);
        }
    }
}

/// Check if a worktree has uncommitted changes.
pub fn worktree_is_dirty(worktree_path: &str) -> bool {
    Command::new("git")
        .args(["-C", worktree_path, "status", "--porcelain"])
        .output()
        .map(|o| o.status.success() && !String::from_utf8_lossy(&o.stdout).trim().is_empty())
        .unwrap_or(false)
}

/// Like [`worktree_is_dirty`], but fail-safe: any error (spawn failure, git
/// non-zero exit, not a repo, transient index lock) is treated as *dirty*.
/// Used by auto-close, where the cost of a false "clean" is destroying a
/// session's uncommitted work — so when in doubt, keep the session.
pub fn worktree_dirty_failsafe(worktree_path: &str) -> bool {
    match Command::new("git")
        .args(["-C", worktree_path, "status", "--porcelain"])
        .output()
    {
        Ok(o) if o.status.success() => !String::from_utf8_lossy(&o.stdout).trim().is_empty(),
        _ => true,
    }
}

/// Generate a default commit message: "<session_name>-<N>" where N increments.
pub fn next_commit_message(worktree_path: &str, session_name: &str) -> String {
    let count = Command::new("git")
        .args(["-C", worktree_path, "rev-list", "--count", "HEAD"])
        .output()
        .ok()
        .filter(|o| o.status.success())
        .and_then(|o| {
            String::from_utf8_lossy(&o.stdout)
                .trim()
                .parse::<u32>()
                .ok()
        })
        .unwrap_or(0);

    format!("{session_name}-{count}")
}

/// Stage all changes and commit.
pub fn commit_all(worktree_path: &str, message: &str) -> Result<()> {
    let output = Command::new("git")
        .args(["-C", worktree_path, "add", "-A"])
        .output()?;
    if !output.status.success() {
        bail!("Failed to stage changes");
    }

    let output = Command::new("git")
        .args(["-C", worktree_path, "commit", "-m", message])
        .output()?;
    if !output.status.success() {
        bail!("Failed to commit");
    }

    Ok(())
}

/// Rebase a session's worktree branch onto the task branch to pull in latest changes.
/// Pull latest main and rebase the task branch onto it.
pub fn push_branch(project_path: &str, branch: &str) -> Result<String> {
    if branch.is_empty() || branch == "main" || branch == "master" {
        bail!("Refusing to push protected branch '{branch}'");
    }

    let output = Command::new("git")
        .args([
            "-C",
            project_path,
            "push",
            "--force-with-lease",
            "-u",
            "origin",
            branch,
        ])
        .output()?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        bail!("Push failed: {stderr}");
    }

    Ok(format!("Pushed {branch} to origin"))
}

pub fn update_task_branch(project_path: &str, branch: &str, base_branch: &str) -> Result<String> {
    // Fetch latest base branch from origin (always updates origin/<base>).
    let _ = Command::new("git")
        .args(["-C", project_path, "fetch", "origin", base_branch])
        .output();

    // Pick the rebase target: prefer origin/<base> if it resolves, else local <base>.
    let remote_ref = format!("origin/{base_branch}");
    let has_remote = Command::new("git")
        .args(["-C", project_path, "rev-parse", "--verify", &remote_ref])
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false);

    // Fast-forward the local <base> ref to origin/<base>. Worktrees share refs, so
    // this keeps every worktree's view of the base branch (e.g. `main`) current.
    if has_remote {
        update_local_base_branch(project_path, base_branch, &remote_ref);
    }

    let target = if has_remote {
        remote_ref
    } else {
        base_branch.to_string()
    };

    // A dedicated worktree already has the branch checked out — git refuses to
    // check it out again in the project dir, so rebase in place there. Otherwise
    // rebase in the project dir, which checks the branch out and back again.
    let worktree = other_worktree_for_branch(project_path, branch);
    let work_dir = worktree.as_deref().unwrap_or(project_path);

    let original_branch = match &worktree {
        Some(_) => None,
        None => {
            let head = Command::new("git")
                .args(["-C", project_path, "rev-parse", "--abbrev-ref", "HEAD"])
                .output()?;
            Some(String::from_utf8_lossy(&head.stdout).trim().to_string())
        }
    };

    let mut args = vec!["-C", work_dir, "rebase", &target];
    if worktree.is_none() {
        args.push(branch);
    }
    let output = Command::new("git").args(&args).output()?;

    if !output.status.success() {
        // Leave the branch checked out so the user can resolve conflicts
        let stderr = String::from_utf8_lossy(&output.stderr);
        bail!(
            "Rebase has conflicts. Resolve them in {work_dir} then run `git rebase --continue`.\n{stderr}"
        );
    }

    // Restore original branch only on success
    if let Some(original_branch) = original_branch {
        let _ = Command::new("git")
            .args(["-C", project_path, "checkout", &original_branch])
            .output();
    }

    let stdout = String::from_utf8_lossy(&output.stdout).trim().to_string();
    if stdout.contains("is up to date") {
        Ok(format!(
            "Branch {branch} is already up to date with {base_branch}"
        ))
    } else {
        Ok(format!("Rebased {branch} onto latest {base_branch}"))
    }
}

fn current_branch(project_path: &str) -> Option<String> {
    let out = Command::new("git")
        .args(["-C", project_path, "rev-parse", "--abbrev-ref", "HEAD"])
        .output()
        .ok()?;
    let name = String::from_utf8_lossy(&out.stdout).trim().to_string();
    if name.is_empty() || name == "HEAD" {
        None
    } else {
        Some(name)
    }
}

/// A worktree other than the project dir that has `branch` checked out, if any.
fn other_worktree_for_branch(project_path: &str, branch: &str) -> Option<String> {
    find_worktree_for_branch(project_path, branch).filter(|p| p.as_str() != project_path)
}

pub fn rebase_session_on_task(
    project_path: &str,
    task_branch: &str,
    worktree_path: &str,
) -> Result<String> {
    // Check for uncommitted changes
    if worktree_is_dirty(worktree_path) {
        bail!("Worktree has uncommitted changes. Commit or stash first.");
    }

    // Get the session branch name
    let output = Command::new("git")
        .args(["-C", worktree_path, "rev-parse", "--abbrev-ref", "HEAD"])
        .output()?;
    if !output.status.success() {
        bail!("Failed to determine worktree branch");
    }
    let session_branch = String::from_utf8_lossy(&output.stdout).trim().to_string();

    // Check if already up to date
    let is_ancestor = Command::new("git")
        .args([
            "-C",
            project_path,
            "merge-base",
            "--is-ancestor",
            task_branch,
            &session_branch,
        ])
        .output()?
        .status
        .success();

    if is_ancestor {
        return Ok(format!(
            "{session_branch} is already up to date with {task_branch}"
        ));
    }

    // Rebase onto task branch
    let output = Command::new("git")
        .args(["-C", worktree_path, "rebase", task_branch])
        .output()?;

    if !output.status.success() {
        let _ = Command::new("git")
            .args(["-C", worktree_path, "rebase", "--abort"])
            .output();
        let stderr = String::from_utf8_lossy(&output.stderr);
        bail!("Rebase conflict. Aborted. Resolve manually.\n{stderr}");
    }

    Ok(format!("Rebased {session_branch} onto {task_branch}"))
}

/// Merge a session's worktree branch into the task branch.
pub fn merge_session_to_task(
    project_path: &str,
    task_branch: &str,
    _session_name: &str,
    worktree_path: &str,
) -> Result<String> {
    // Get the session branch name from the worktree
    let output = Command::new("git")
        .args(["-C", worktree_path, "rev-parse", "--abbrev-ref", "HEAD"])
        .output()?;
    if !output.status.success() {
        bail!("Failed to determine worktree branch");
    }
    let session_branch = String::from_utf8_lossy(&output.stdout).trim().to_string();

    if session_branch.is_empty() {
        bail!("Could not determine session branch");
    }

    // Find a worktree that has the task branch checked out
    let task_wt = find_worktree_for_branch(project_path, task_branch);

    if let Some(task_wt_path) = task_wt {
        // Merge in the worktree that has the task branch — this naturally updates
        // its index and working tree, and respects uncommitted changes.
        let output = Command::new("git")
            .args(["-C", &task_wt_path, "merge", "--ff-only", &session_branch])
            .output()?;

        if output.status.success() {
            let stdout = String::from_utf8_lossy(&output.stdout);
            return Ok(format!(
                "Merged {session_branch} into {task_branch} (ff)\n{}",
                stdout.trim()
            ));
        }

        // ff-only failed — try a real merge
        let output = Command::new("git")
            .args([
                "-C",
                &task_wt_path,
                "merge",
                &session_branch,
                "-m",
                &format!("Merge {session_branch} into {task_branch}"),
            ])
            .output()?;

        if !output.status.success() {
            let _ = Command::new("git")
                .args(["-C", &task_wt_path, "merge", "--abort"])
                .output();
            let stderr = String::from_utf8_lossy(&output.stderr);
            bail!("Merge conflict. Aborted. Resolve manually.\n{stderr}");
        }

        Ok(format!("Merged {session_branch} into {task_branch}"))
    } else {
        // No worktree has the task branch — safe to use update-ref
        let is_ancestor = Command::new("git")
            .args([
                "-C",
                project_path,
                "merge-base",
                "--is-ancestor",
                task_branch,
                &session_branch,
            ])
            .output()?
            .status
            .success();

        if is_ancestor {
            let output = Command::new("git")
                .args(["-C", project_path, "rev-parse", &session_branch])
                .output()?;
            if !output.status.success() {
                bail!("Failed to resolve {session_branch}");
            }
            let session_sha = String::from_utf8_lossy(&output.stdout).trim().to_string();

            let output = Command::new("git")
                .args([
                    "-C",
                    project_path,
                    "rev-list",
                    "--count",
                    &format!("{task_branch}..{session_branch}"),
                ])
                .output()?;
            let count = String::from_utf8_lossy(&output.stdout).trim().to_string();

            let output = Command::new("git")
                .args([
                    "-C",
                    project_path,
                    "update-ref",
                    &format!("refs/heads/{task_branch}"),
                    &session_sha,
                ])
                .output()?;
            if !output.status.success() {
                bail!("Failed to fast-forward {task_branch}");
            }

            Ok(format!(
                "Fast-forwarded {task_branch} ({count} commit(s) from {session_branch})"
            ))
        } else {
            // Non-ff merge without a worktree: do it in the session worktree temporarily
            let output = Command::new("git")
                .args(["-C", worktree_path, "checkout", task_branch])
                .output()?;
            if !output.status.success() {
                bail!("Failed to checkout {task_branch} in worktree");
            }

            let output = Command::new("git")
                .args([
                    "-C",
                    worktree_path,
                    "merge",
                    &session_branch,
                    "-m",
                    &format!("Merge {session_branch} into {task_branch}"),
                ])
                .output()?;

            if !output.status.success() {
                let _ = Command::new("git")
                    .args(["-C", worktree_path, "merge", "--abort"])
                    .output();
                let _ = Command::new("git")
                    .args(["-C", worktree_path, "checkout", &session_branch])
                    .output();
                let stderr = String::from_utf8_lossy(&output.stderr);
                bail!("Merge conflict. Aborted. Resolve manually.\n{stderr}");
            }

            let _ = Command::new("git")
                .args(["-C", worktree_path, "checkout", &session_branch])
                .output();

            Ok(format!("Merged {session_branch} into {task_branch}"))
        }
    }
}

/// Find a worktree path that has the given branch checked out.
/// Fast-forward the local `base_branch` ref to `remote_ref` (origin/<base>) so that
/// every worktree — which share a single ref store — sees the latest base branch.
///
/// Git refuses to update a checked-out branch via `fetch origin base:base`, so when
/// the base branch is checked out somewhere we fast-forward it in that worktree
/// instead. If it isn't checked out anywhere, the ref is updated directly. All steps
/// are best-effort and no-ops when already up to date.
fn update_local_base_branch(project_path: &str, base_branch: &str, remote_ref: &str) {
    match find_worktree_for_branch(project_path, base_branch) {
        Some(wt) => {
            // Checked out — fast-forward its working tree. Skip if dirty so we never
            // touch uncommitted work; a non-ff history is left untouched by --ff-only.
            if !worktree_is_dirty(&wt) {
                let _ = Command::new("git")
                    .args(["-C", &wt, "merge", "--ff-only", remote_ref])
                    .output();
            }
        }
        None => {
            // Not checked out anywhere — safe to update the local ref directly.
            let _ = Command::new("git")
                .args([
                    "-C",
                    project_path,
                    "fetch",
                    "origin",
                    &format!("{base_branch}:{base_branch}"),
                ])
                .output();
        }
    }
}

fn find_worktree_for_branch(project_path: &str, branch: &str) -> Option<String> {
    // Check main repo first
    let output = Command::new("git")
        .args(["-C", project_path, "rev-parse", "--abbrev-ref", "HEAD"])
        .output()
        .ok()?;
    if output.status.success() {
        let current = String::from_utf8_lossy(&output.stdout).trim().to_string();
        if current == branch {
            return Some(project_path.to_string());
        }
    }

    // Check worktrees
    let output = Command::new("git")
        .args(["-C", project_path, "worktree", "list", "--porcelain"])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }

    let stdout = String::from_utf8_lossy(&output.stdout);
    let mut current_path = None;

    for line in stdout.lines() {
        if let Some(path) = line.strip_prefix("worktree ") {
            current_path = Some(path.to_string());
        } else if let Some(b) = line.strip_prefix("branch refs/heads/") {
            if b == branch {
                return current_path;
            }
        } else if line.is_empty() {
            current_path = None;
        }
    }

    None
}

#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
pub struct DiffStats {
    pub added: usize,
    pub removed: usize,
}

impl DiffStats {
    pub fn is_empty(&self) -> bool {
        self.added == 0 && self.removed == 0
    }
}

/// The branch currently checked out in the session's worktree (or the project
/// directory for no-worktree sessions).
pub fn get_session_branch(session_name: &str) -> Option<String> {
    let path = get_session_env(session_name, "CM_WORKTREE_PATH")
        .or_else(|| get_session_env(session_name, "CM_PROJECT_PATH"))?;
    current_branch(&path)
}

fn count_diff(diff: &str) -> DiffStats {
    let mut added = 0;
    let mut removed = 0;
    for line in diff.lines() {
        if line.starts_with('+') && !line.starts_with("+++") {
            added += 1;
        } else if line.starts_with('-') && !line.starts_with("---") {
            removed += 1;
        }
    }
    DiffStats { added, removed }
}

/// Full unified diff of a session's worktree against its task branch
/// (includes committed + uncommitted changes).
pub fn get_session_diff_text(session_name: &str) -> Option<String> {
    let worktree_path = get_session_env(session_name, "CM_WORKTREE_PATH")
        .or_else(|| get_session_env(session_name, "CM_PROJECT_PATH"))?;

    // Try task branch first, fall back to base commit for older sessions
    let diff_target = get_session_env(session_name, "CM_TASK_BRANCH")
        .or_else(|| get_session_env(session_name, "CM_BASE_COMMIT"))?;

    if !std::path::Path::new(&worktree_path).exists() {
        return None;
    }

    // Stage intent-to-add for untracked files so they show up in diff
    let _ = Command::new("git")
        .args(["-C", &worktree_path, "add", "-N", "."])
        .output();

    let output = Command::new("git")
        .args(["-C", &worktree_path, "--no-pager", "diff", &diff_target])
        .output()
        .ok()?;

    if !output.status.success() {
        return None;
    }

    Some(String::from_utf8_lossy(&output.stdout).to_string())
}

/// Compute diff stats for a session's worktree against its base commit.
pub fn get_diff_stats(session_name: &str) -> Option<DiffStats> {
    Some(count_diff(&get_session_diff_text(session_name)?))
}

/// Compute diff stats for a task branch against its base branch.
/// Resolve a diff base ref, preferring `origin/<base>` when it exists (matching
/// `get_branch_diff`), else the local branch name.
pub fn resolve_base_ref(project_path: &str, base_branch: &str) -> String {
    let remote = format!("origin/{base_branch}");
    let has_remote = Command::new("git")
        .args([
            "-C",
            project_path,
            "rev-parse",
            "--verify",
            "--quiet",
            &remote,
        ])
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false);
    if has_remote {
        remote
    } else {
        base_branch.to_string()
    }
}

/// Full unified diff of a task branch against its base branch.
pub fn get_branch_diff_text(project_path: &str, branch: &str, base_branch: &str) -> Option<String> {
    let base = resolve_base_ref(project_path, base_branch);

    let output = Command::new("git")
        .args([
            "-C",
            project_path,
            "--no-pager",
            "diff",
            &format!("{base}...{branch}"),
        ])
        .output()
        .ok()?;

    if !output.status.success() {
        return None;
    }

    Some(String::from_utf8_lossy(&output.stdout).to_string())
}

pub fn get_branch_diff(project_path: &str, branch: &str, base_branch: &str) -> Option<DiffStats> {
    Some(count_diff(&get_branch_diff_text(
        project_path,
        branch,
        base_branch,
    )?))
}

/// Raw signals from a tmux session for status detection.
pub struct SessionProbe {
    pub agent_alive: bool,
    pub content_hash: u64,
    pub has_permission_prompt: bool,
}

/// Probe a session for raw status signals.
pub fn probe_session(session_name: &str) -> Option<SessionProbe> {
    let target = format!("{session_name}:0");
    // Check pane_pid and pane_dead
    let output = Command::new("tmux")
        .args([
            "display-message",
            "-t",
            &target,
            "-p",
            "#{pane_pid} #{pane_dead}",
        ])
        .output()
        .ok()?;

    if !output.status.success() {
        return None;
    }

    let info = String::from_utf8_lossy(&output.stdout);
    let parts: Vec<&str> = info.trim().split(' ').collect();

    if parts.len() >= 2 && parts[1] == "1" {
        return None; // pane is dead
    }

    let pane_pid = parts.first().and_then(|p| p.parse::<u32>().ok())?;

    let agent = session_agent(session_name);
    let process_name = agent.process_name();

    // Check if the pane process itself is the agent, or if the agent is a
    // child (e.g. codex runs under a node wrapper).
    let pane_comm = Command::new("ps")
        .args(["-o", "comm=", "-p", &pane_pid.to_string()])
        .output()
        .ok()
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .unwrap_or_default();

    let mut agent_alive = pane_comm == process_name
        || Command::new("pgrep")
            .args(["-P", &pane_pid.to_string(), "-x", process_name])
            .output()
            .map(|o| o.status.success())
            .unwrap_or(false);

    // Fallback for agents that run as a `node` script (comm is `node`, so
    // `pgrep -x <name>` misses them): match the command line of the pane's
    // direct children.
    if !agent_alive {
        if let Some(term) = agent.node_argv_term() {
            agent_alive = Command::new("pgrep")
                .args(["-P", &pane_pid.to_string(), "-f", term])
                .output()
                .map(|o| o.status.success())
                .unwrap_or(false);
        }
    }

    let content = capture_pane_plain(&target).unwrap_or_default();
    let content_hash = hash_content(&content);
    let has_permission_prompt = detect_attention_dialog(&content, agent);

    Some(SessionProbe {
        agent_alive,
        content_hash,
        has_permission_prompt,
    })
}

/// Whether the pane shows an active dialog needing the user (permission
/// prompt or question selector).
///
/// Two guards against false positives from dialog text merely echoed in the
/// transcript: markers must appear near the bottom of the pane (active
/// dialogs render there), and — for Claude — the pane must not currently show
/// the regular input prompt (a bare `❯`/`>` line), which Claude Code replaces
/// while a dialog is open. Codex has no such guard: its selector line (`› 1.`)
/// uses the same glyph as its input line, and its idle placeholder never
/// matches the marker.
fn detect_attention_dialog(content: &str, agent: AgentKind) -> bool {
    let nonempty: Vec<&str> = content.lines().filter(|l| !l.trim().is_empty()).collect();

    if agent == AgentKind::Claude {
        let at_input_prompt = nonempty[nonempty.len().saturating_sub(8)..]
            .iter()
            .any(|l| matches!(l.trim(), "❯" | ">"));
        if at_input_prompt {
            return false;
        }
    }

    let tail = nonempty[nonempty.len().saturating_sub(12)..].join("\n");
    agent.attention_markers().iter().any(|p| tail.contains(p))
}

fn hash_content(s: &str) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    s.hash(&mut hasher);
    hasher.finish()
}

fn capture_pane_plain(session_name: &str) -> Option<String> {
    let output = Command::new("tmux")
        .args(["capture-pane", "-t", session_name, "-p"])
        .output()
        .ok()?;

    if !output.status.success() {
        return None;
    }

    Some(String::from_utf8_lossy(&output.stdout).to_string())
}

/// Get the PR URL for a branch using the `gh` CLI.
pub fn get_pr_url(project_path: &str, branch: &str) -> Option<String> {
    let output = Command::new("gh")
        .args(["pr", "view", branch, "--json", "url", "-q", ".url"])
        .current_dir(project_path)
        .output()
        .ok()?;

    if !output.status.success() {
        return None;
    }

    let url = String::from_utf8_lossy(&output.stdout).trim().to_string();
    if url.is_empty() { None } else { Some(url) }
}

pub fn next_session_number(project_name: &str, task_name: &str, sessions: &[TmuxSession]) -> u32 {
    let max = sessions
        .iter()
        .filter(|s| s.project_name == project_name && s.task_name == task_name)
        .filter_map(|s| s.session_name.parse::<u32>().ok())
        .max()
        .unwrap_or(0);
    max + 1
}

pub fn sessions_for_task(
    project_name: &str,
    task_name: &str,
    sessions: &[TmuxSession],
) -> Vec<TmuxSession> {
    let mut task_sessions: Vec<TmuxSession> = sessions
        .iter()
        .filter(|s| s.project_name == sanitize(project_name) && s.task_name == sanitize(task_name))
        .cloned()
        .collect();
    // The main session always sorts first.
    task_sessions.sort_by_key(|s| (!is_main_session(&s.session_name), s.session_name.clone()));
    task_sessions
}

/// All adhoc sessions belonging to a project.
pub fn adhoc_sessions_for_project(
    project_name: &str,
    sessions: &[TmuxSession],
) -> Vec<TmuxSession> {
    sessions
        .iter()
        .filter(|s| s.project_name == sanitize(project_name) && is_adhoc_marker(&s.task_name))
        .cloned()
        .collect()
}

/// Delete a task and all its sessions, worktrees, branches, and config files.
/// Returns a description of what was cleaned up.
pub fn delete_task(
    project_name: &str,
    project_path: &str,
    task_name: &str,
    task_branch: &str,
    sessions: &[TmuxSession],
) -> String {
    let task_sessions = sessions_for_task(project_name, task_name, sessions);
    let session_count = task_sessions.len();

    // Collect tmux names of live sessions so we can identify orphaned records
    let live_names: std::collections::HashSet<&str> =
        task_sessions.iter().map(|s| s.name.as_str()).collect();

    // Kill all live tmux sessions (this also removes worktrees + session branches)
    for session in &task_sessions {
        let _ = kill_session(&session.name);
    }

    // Also clean up any orphaned session records (tmux session already dead)
    let records = crate::config::load_sessions();
    for (tmux_name, record) in &records {
        if record.project_name == sanitize(project_name)
            && record.task_name == sanitize(task_name)
            && !live_names.contains(tmux_name.as_str())
        {
            // This record's tmux session is dead — clean up its worktree and branch
            let wt_path = worktree_dir(
                &record.project_name,
                &record.task_name,
                &record.session_name,
            );
            if record.use_worktree {
                let session_branch = format!(
                    "{}-{}",
                    sanitize(task_branch),
                    sanitize(&record.session_name)
                );
                let _ = kill_session_with_fallback(
                    tmux_name,
                    Some(SessionCleanupInfo {
                        project_path: record.project_path.clone(),
                        worktree_path: wt_path.to_string_lossy().to_string(),
                        branch_name: Some(session_branch),
                        // The task branch is deleted explicitly further down.
                        task_branch: Some(task_branch.to_string()),
                    }),
                );
            }
        }
    }

    // Clean up any remaining worktree directories for this task that weren't covered above
    // (e.g. if session records were also lost)
    let task_wt_prefix = format!("{}-", sanitize(task_name));
    let project_wt_dir = crate::config::base_dir()
        .join("worktrees")
        .join(sanitize(project_name));
    if project_wt_dir.exists() {
        if let Ok(entries) = std::fs::read_dir(&project_wt_dir) {
            for entry in entries.flatten() {
                let name = entry.file_name();
                let name_str = name.to_string_lossy();
                if name_str.starts_with(&task_wt_prefix) && entry.path().is_dir() {
                    // Derive branch name from worktree before removing
                    let wt_path_str = entry.path().to_string_lossy().to_string();
                    let branch = Command::new("git")
                        .args(["-C", &wt_path_str, "rev-parse", "--abbrev-ref", "HEAD"])
                        .output()
                        .ok()
                        .filter(|o| o.status.success())
                        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string());

                    let _ = Command::new("git")
                        .args([
                            "-C",
                            project_path,
                            "worktree",
                            "remove",
                            "--force",
                            &wt_path_str,
                        ])
                        .output();

                    // If git refused (e.g. the dir was never registered as a
                    // worktree), drop the directory so nothing is left behind.
                    if entry.path().exists() {
                        let _ = std::fs::remove_dir_all(entry.path());
                    }

                    if let Some(branch_name) = branch {
                        if !branch_name.is_empty()
                            && branch_name != "main"
                            && branch_name != "master"
                        {
                            let _ = Command::new("git")
                                .args(["-C", project_path, "branch", "-D", &branch_name])
                                .output();
                        }
                    }
                }
            }
        }
    }

    // Prune any stale worktree references
    let _ = Command::new("git")
        .args(["-C", project_path, "worktree", "prune"])
        .output();

    // Delete cached task files (pr_url.txt)
    let _ = std::fs::remove_dir_all(crate::config::task_dir(project_name, task_branch));

    // Delete the task branch itself (session branches are already cleaned up above)
    if !task_branch.is_empty() && task_branch != "main" && task_branch != "master" {
        let _ = Command::new("git")
            .args(["-C", project_path, "branch", "-D", task_branch])
            .output();
    }

    if session_count > 0 {
        format!(
            "Deleted task '{}' and {} session(s)",
            task_name, session_count
        )
    } else {
        format!("Deleted task '{}'", task_name)
    }
}

/// Reap an orphaned session record whose task no longer exists in config.
///
/// Removes the worktree directory and cached task files, but deliberately
/// PRESERVES the git branch so any committed work stays recoverable. This is
/// meant for automatic startup reconciliation, where silently deleting branches
/// would be unsafe. Explicit, user-initiated deletion still goes through
/// [`delete_task`], which also removes the branch.
pub fn cleanup_orphan_session(record: &crate::config::SessionRecord) {
    // Remove the worktree directory if this session used one (committed work
    // remains on the branch, which we keep).
    if record.use_worktree {
        let wt_path = worktree_dir(
            &record.project_name,
            &record.task_name,
            &record.session_name,
        );
        let wt_str = wt_path.to_string_lossy().to_string();
        if wt_path.exists() {
            let _ = Command::new("git")
                .args([
                    "-C",
                    &record.project_path,
                    "worktree",
                    "remove",
                    "--force",
                    &wt_str,
                ])
                .output();
        }
        // Prune stale worktree references regardless, in case the dir was
        // already removed but git still tracks it.
        let _ = Command::new("git")
            .args(["-C", &record.project_path, "worktree", "prune"])
            .output();
    }

    // Remove the cached task directory (pr_url.txt). The dir is
    // shared by all sessions of a task; since orphan status is per-task, every
    // session of this task is being reaped together.
    let _ = std::fs::remove_dir_all(crate::config::task_dir(
        &record.project_name,
        &record.task_branch,
    ));
}

/// Clean up worktree and task config directories for a project.
pub fn cleanup_project_dirs(project_name: &str) {
    let sanitized = sanitize(project_name);
    let base = crate::config::base_dir();

    // Remove worktree directory for this project
    let wt_dir = base.join("worktrees").join(&sanitized);
    if wt_dir.exists() {
        let _ = std::fs::remove_dir_all(&wt_dir);
    }

    // Remove task config directory for this project
    let task_dir = base.join("tasks").join(&sanitized);
    if task_dir.exists() {
        let _ = std::fs::remove_dir_all(&task_dir);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // --- sanitize ---

    #[test]
    fn sanitize_alphanumeric_unchanged() {
        assert_eq!(sanitize("hello123"), "hello123");
    }

    #[test]
    fn sanitize_replaces_special_chars() {
        assert_eq!(sanitize("hello world!"), "hello-world");
    }

    #[test]
    fn sanitize_collapses_hyphens() {
        assert_eq!(sanitize("a--b---c"), "a-b-c");
    }

    #[test]
    fn sanitize_trims_leading_trailing_hyphens() {
        assert_eq!(sanitize("-hello-"), "hello");
    }

    #[test]
    fn sanitize_replaces_dots_and_slashes() {
        assert_eq!(sanitize("my.project/path"), "my-project-path");
    }

    #[test]
    fn sanitize_replaces_underscores_with_hyphens() {
        // Underscores are not alphanumeric or '-', so they become hyphens
        assert_eq!(sanitize("a__b"), "a-b");
    }

    // --- run sessions ---

    #[test]
    fn run_session_name_is_prefixed_and_sanitized() {
        assert_eq!(run_session_name("My App"), "cmrun-My-App");
        // The prefix is intentionally unparseable as a managed session.
        assert!(TmuxSession::from_tmux_name(&run_session_name("App")).is_none());
    }

    #[test]
    fn is_shell_command_detects_shells_and_processes() {
        assert!(is_shell_command("zsh"));
        assert!(is_shell_command("bash"));
        assert!(is_shell_command("-zsh")); // login shell form
        assert!(!is_shell_command("node"));
        assert!(!is_shell_command("npm"));
        assert!(!is_shell_command("cargo"));
    }

    // --- to_branch_name ---

    #[test]
    fn branch_name_lowercases() {
        assert_eq!(to_branch_name("Fix Bug"), "fix-bug");
    }

    #[test]
    fn branch_name_strips_special_chars() {
        assert_eq!(to_branch_name("Add feature #123!"), "add-feature-123");
    }

    #[test]
    fn branch_name_collapses_hyphens() {
        assert_eq!(to_branch_name("a   b"), "a-b");
    }

    #[test]
    fn branch_name_trims_edges() {
        assert_eq!(to_branch_name(" hello "), "hello");
    }

    // --- TmuxSession::from_tmux_name ---

    #[test]
    fn parse_valid_session_name() {
        let session = TmuxSession::from_tmux_name("cm__myproject__mytask__mysession").unwrap();
        assert_eq!(session.project_name, "myproject");
        assert_eq!(session.task_name, "mytask");
        assert_eq!(session.session_name, "mysession");
        assert_eq!(session.name, "cm__myproject__mytask__mysession");
    }

    #[test]
    fn parse_session_with_hyphens() {
        let session = TmuxSession::from_tmux_name("cm__my-project__my-task__my-session").unwrap();
        assert_eq!(session.project_name, "my-project");
        assert_eq!(session.task_name, "my-task");
        assert_eq!(session.session_name, "my-session");
    }

    #[test]
    fn parse_rejects_no_prefix() {
        assert!(TmuxSession::from_tmux_name("myproject__task__session").is_none());
    }

    #[test]
    fn parse_rejects_too_few_parts() {
        assert!(TmuxSession::from_tmux_name("cm__project__task").is_none());
    }

    #[test]
    fn parse_rejects_unrelated_session() {
        assert!(TmuxSession::from_tmux_name("random-session").is_none());
    }

    // --- build_tmux_name ---

    #[test]
    fn build_tmux_name_basic() {
        assert_eq!(
            build_tmux_name("proj", "task", "sess"),
            "cm__proj__task__sess"
        );
    }

    #[test]
    fn build_tmux_name_sanitizes_parts() {
        let name = build_tmux_name("my project", "my task", "my session");
        assert_eq!(name, "cm__my-project__my-task__my-session");
    }

    #[test]
    fn build_tmux_name_roundtrips() {
        let name = build_tmux_name("proj", "task", "sess");
        let parsed = TmuxSession::from_tmux_name(&name).unwrap();
        assert_eq!(parsed.project_name, "proj");
        assert_eq!(parsed.task_name, "task");
        assert_eq!(parsed.session_name, "sess");
    }

    // --- adhoc helpers ---

    #[test]
    fn adhoc_marker_recognises_canonical() {
        assert!(is_adhoc_marker("adhoc"));
        assert!(is_adhoc_marker("Adhoc"));
        assert!(is_adhoc_marker("ADHOC"));
    }

    #[test]
    fn adhoc_marker_rejects_other_names() {
        assert!(!is_adhoc_marker("ad-hoc"));
        assert!(!is_adhoc_marker("adhocs"));
        assert!(!is_adhoc_marker("explore"));
    }

    #[test]
    fn adhoc_tmux_name_uses_marker_slot() {
        let name = build_tmux_name("proj", ADHOC_MARKER, "explore");
        assert_eq!(name, "cm__proj__adhoc__explore");
        let parsed = TmuxSession::from_tmux_name(&name).unwrap();
        assert!(is_adhoc_marker(&parsed.task_name));
    }

    #[test]
    fn adhoc_sessions_for_project_filters() {
        let sessions = vec![
            TmuxSession::from_tmux_name("cm__proj__adhoc__a").unwrap(),
            TmuxSession::from_tmux_name("cm__proj__task1__1").unwrap(),
            TmuxSession::from_tmux_name("cm__other__adhoc__a").unwrap(),
        ];
        let adhoc = adhoc_sessions_for_project("proj", &sessions);
        assert_eq!(adhoc.len(), 1);
        assert_eq!(adhoc[0].session_name, "a");
    }

    // --- shell_escape ---

    #[test]
    fn shell_escape_simple() {
        assert_eq!(shell_escape("hello"), "'hello'");
    }

    #[test]
    fn shell_escape_with_single_quotes() {
        assert_eq!(shell_escape("it's"), "'it'\\''s'");
    }

    #[test]
    fn shell_escape_with_spaces() {
        assert_eq!(shell_escape("hello world"), "'hello world'");
    }

    // --- DiffStats ---

    // --- build_initial_prompt ---

    #[test]
    fn initial_prompt_none_when_empty() {
        assert!(build_initial_prompt(&[], None, AgentKind::Claude).is_none());
        assert!(build_initial_prompt(&[], Some(""), AgentKind::Claude).is_none());
    }

    #[test]
    fn initial_prompt_passthrough_no_skills() {
        assert_eq!(
            build_initial_prompt(&[], Some("do stuff"), AgentKind::Claude),
            Some("do stuff".into())
        );
    }

    #[test]
    fn initial_prompt_skills_only() {
        let result = build_initial_prompt(&["/prime".into()], None, AgentKind::Claude).unwrap();
        assert!(result.contains("/prime"));
        assert!(!result.contains("Task:"));
    }

    #[test]
    fn initial_prompt_skills_and_prompt() {
        let result = build_initial_prompt(
            &["/prime".into(), "/caveman ultra".into()],
            Some("fix bug"),
            AgentKind::Claude,
        )
        .unwrap();
        assert!(result.contains("1. /prime"));
        assert!(result.contains("2. /caveman ultra"));
        assert!(result.contains("Task: fix bug"));
    }

    // --- DiffStats ---

    #[test]
    fn diff_stats_empty() {
        let stats = DiffStats {
            added: 0,
            removed: 0,
        };
        assert!(stats.is_empty());
    }

    #[test]
    fn diff_stats_not_empty() {
        let stats = DiffStats {
            added: 5,
            removed: 3,
        };
        assert!(!stats.is_empty());
    }

    // --- detect_attention_dialog ---

    #[test]
    fn attention_for_active_question_dialog() {
        let pane = "⏺ Some earlier output\n\n\
                    Which approach should we take?\n\
                    ❯ 1. Option A\n  \
                    2. Option B\n  \
                    3. Option C\n\n  \
                    Enter to confirm";
        assert!(detect_attention_dialog(pane, AgentKind::Claude));
    }

    #[test]
    fn attention_for_active_permission_prompt() {
        let pane = "⏺ Bash(rm -rf build)\n\n\
                    Do you want to proceed?\n\
                    ❯ 1. Yes\n  \
                    2. Yes, allow all edits during this session\n  \
                    3. No, and tell Claude what to do differently";
        assert!(detect_attention_dialog(pane, AgentKind::Claude));
    }

    #[test]
    fn no_attention_when_idle_at_input_prompt() {
        // Dialog-like text echoed in the transcript above an idle input box
        // (bare ❯ line) must not count as an active dialog.
        let pane = "⏺ Added \"❯ 1.\" to the marker list\n\n\
                    Do you want to test this?\n\
                    ────────────\n\
                    ❯ \n\
                    ────────────\n  \
                    -- INSERT -- ⏵⏵ bypass permissions on";
        assert!(!detect_attention_dialog(pane, AgentKind::Claude));
    }

    #[test]
    fn no_attention_when_marker_scrolled_far_up() {
        let mut pane = String::from("❯ 1. Old answered dialog\n");
        for i in 0..20 {
            pane.push_str(&format!("output line {i}\n"));
        }
        assert!(!detect_attention_dialog(&pane, AgentKind::Claude));
    }

    #[test]
    fn no_attention_on_plain_output() {
        assert!(!detect_attention_dialog(
            "⏺ Done. All tests pass.\n",
            AgentKind::Claude
        ));
    }

    // --- codex (captured from codex-cli 0.148.0 panes) ---

    #[test]
    fn codex_attention_for_trust_dialog() {
        let pane = "  Do you trust the contents of this directory?\n\n\
                    › 1. Yes, continue\n  \
                    2. No, quit\n\n  \
                    Press enter to continue";
        assert!(detect_attention_dialog(pane, AgentKind::Codex));
    }

    #[test]
    fn codex_no_attention_when_idle() {
        let pane = "• 4\n  beef42\n\n\
                    › Ask Codex to do anything\n  \
                    gpt-5.6-sol default · /some/dir";
        assert!(!detect_attention_dialog(pane, AgentKind::Codex));
    }

    #[test]
    fn codex_no_attention_while_working() {
        let pane = "• Running the command exactly as provided.\n\
                    • Working (2s • esc to interrupt)\n\
                    › Ask Codex to do anything\n  \
                    gpt-5.6-sol default · /some/dir";
        assert!(!detect_attention_dialog(pane, AgentKind::Codex));
    }

    // --- pi (captured from pi 0.84.2 panes) ---

    #[test]
    fn pi_attention_for_trust_dialog() {
        let pane = "\
────────────────────────────\n \
Trust project folder?\n \
/some/work/dir\n \
This allows pi to load .pi settings and resources, install missing project packages, and execute project extensions.\n \
→ Trust\n   \
Trust parent folder (/some/work)\n   \
Trust (this session only)\n   \
Do not trust\n   \
Do not trust (this session only)\n \
↑↓ navigate  enter select  escape/ctrl+c cancel\n\
────────────────────────────";
        assert!(detect_attention_dialog(pane, AgentKind::Pi));
    }

    #[test]
    fn pi_no_attention_when_idle() {
        let pane = "\
Some transcript output.\n\
────────────────────────────\n\
\n\
────────────────────────────\n\
/some/work/dir (main)\n\
0.0%/128k (auto)                    (openrouter) openai/gpt-4o-mini";
        assert!(!detect_attention_dialog(pane, AgentKind::Pi));
    }

    #[test]
    fn pi_no_attention_while_working() {
        let pane = "\
 ⠏ Working...\n\
────────────────────────────\n\
\n\
────────────────────────────\n\
/some/work/dir (main)\n\
0.0%/128k (auto)                    (openrouter) openai/gpt-4o-mini";
        assert!(!detect_attention_dialog(pane, AgentKind::Pi));
    }

    #[test]
    fn pi_command_uses_approve_system_prompt_flag_and_positional_prompt() {
        let cmd = build_agent_command(AgentKind::Pi, "/w", Some("briefing"), Some("do it"), false);
        assert_eq!(
            cmd,
            "pi --approve --append-system-prompt 'briefing' 'do it'"
        );

        let resumed = build_agent_command(AgentKind::Pi, "/w", Some("briefing"), None, true);
        assert_eq!(
            resumed,
            "pi --approve --continue --append-system-prompt 'briefing'"
        );
    }

    #[test]
    fn codex_initial_prompt_avoids_skill_tool_wording() {
        let result =
            build_initial_prompt(&["/prime".into()], Some("fix bug"), AgentKind::Codex).unwrap();
        assert!(result.contains("1. /prime"));
        assert!(result.contains("Task: fix bug"));
        assert!(!result.contains("Skill tool"));
    }

    /// Regression for C1: the installed plugin must include the hooks files its
    /// manifest references, or Claude Code fails to load the whole plugin.
    #[test]
    fn install_showrunner_plugin_writes_hooks_files() {
        let dir = std::env::temp_dir().join(format!("sr_plugin_test_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let work_dir = dir.to_string_lossy().to_string();

        install_showrunner_plugin(&work_dir);

        let plugin_dir = dir.join(".claude/plugins/showrunner");
        assert!(
            plugin_dir.join(".claude-plugin/plugin.json").exists(),
            "manifest missing"
        );
        assert!(
            plugin_dir.join("hooks/hooks.json").exists(),
            "C1: hooks.json was not installed into the plugin"
        );
        assert!(
            plugin_dir.join("hooks/post-event.sh").exists(),
            "C1: post-event.sh was not installed into the plugin"
        );
        // The manifest's hooks reference must point at a file that exists.
        let manifest =
            std::fs::read_to_string(plugin_dir.join(".claude-plugin/plugin.json")).unwrap();
        assert!(
            manifest.contains("./hooks/hooks.json"),
            "manifest should reference hooks.json"
        );
        assert!(
            plugin_dir.join("hooks/hooks.json").exists(),
            "referenced hooks.json must exist (no dangling ref)"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }
}
