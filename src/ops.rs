//! Task and session operations shared by the HTTP server and the CLI, so both
//! headless entry points create and delete things exactly like the TUI does.

use anyhow::{Context, Result, bail};

use crate::agent::AgentKind;
use crate::config::{self, Config, Project};
use crate::tmux;

/// The agent new sessions run: an explicit `--agent` value wins, else the
/// config's `default_agent`, else Claude.
pub fn resolve_agent(cfg: &Config, explicit: Option<&str>) -> Result<AgentKind> {
    match explicit {
        Some(id) => crate::agent::parse_agent_id(id),
        None => Ok(AgentKind::from_id(&cfg.default_agent).unwrap_or_default()),
    }
}

/// Create a session for a task and persist its record.
pub fn create_task_session(
    cfg: &Config,
    project: &Project,
    task_name: &str,
    branch: &str,
    session_name: String,
    use_worktree: bool,
    prompt: Option<&str>,
    agent: AgentKind,
) -> Result<String> {
    let tmux_name = tmux::create_session(
        &project.name,
        &project.path,
        task_name,
        branch,
        &session_name,
        use_worktree,
        &project.copy_patterns,
        &project.setup_commands,
        prompt,
        &cfg.startup_skills,
        agent,
    )?;

    config::add_session_record(
        &tmux_name,
        config::SessionRecord {
            project_name: project.name.clone(),
            project_path: project.path.clone(),
            task_name: task_name.to_string(),
            task_branch: branch.to_string(),
            session_name,
            use_worktree,
            archived: false,
            auto_closed: false,
            agent: agent.id().to_string(),
            board_card: None,
            parent: None,
        },
    );

    Ok(tmux_name)
}

/// Create a task (branch + config entry) and its main session.
/// `branch` defaults to a branch name derived from the task name.
/// `base` sets the task's base branch (for stacking); a newly created task
/// branch starts from it instead of main.
/// Returns the task's branch and the main session's tmux name.
pub fn create_task(
    cfg: &Config,
    project: &Project,
    task_name: &str,
    branch: Option<&str>,
    base: Option<&str>,
    prompt: Option<&str>,
    agent: AgentKind,
) -> Result<(String, String)> {
    let task_name = task_name.trim();
    if task_name.is_empty() {
        bail!("task name is required");
    }
    if tmux::is_adhoc_marker(task_name) {
        bail!("'adhoc' is a reserved task name");
    }
    if project.tasks.iter().any(|t| t.name == task_name) {
        bail!(
            "task '{task_name}' already exists in project '{}'",
            project.name
        );
    }

    let branch = branch
        .map(str::trim)
        .filter(|b| !b.is_empty())
        .map(str::to_string)
        .unwrap_or_else(|| tmux::to_branch_name(task_name));
    if branch.is_empty() {
        bail!("task name produces an empty branch name");
    }
    if branch == "main" || branch == "master" {
        bail!("'{branch}' cannot be used as a task branch");
    }

    let base = base.map(str::trim).filter(|b| !b.is_empty());
    if let Some(base) = base {
        if base == branch {
            bail!("base branch can't be the task branch itself");
        }
        if !tmux::branch_exists(&project.path, base) {
            bail!(
                "base branch '{base}' does not exist in {} (create it first, or check the name)",
                project.path
            );
        }
    }

    if !tmux::branch_exists(&project.path, &branch) {
        tmux::create_task_branch(&project.path, &branch, base)?;
    }

    let (project_name, task, branch_for_config) =
        (project.name.clone(), task_name.to_string(), branch.clone());
    let base_for_config = base.map(str::to_string);
    Config::modify(move |c| {
        c.add_task(&project_name, task.clone(), branch_for_config);
        c.set_task_base_branch(&project_name, &task, base_for_config);
    })?;

    let tmux_name = create_task_session(
        cfg,
        project,
        task_name,
        &branch,
        tmux::MAIN_SESSION.to_string(),
        true,
        prompt,
        agent,
    )?;

    Ok((branch, tmux_name))
}

/// Create an additional session on an existing task, numbered like the TUI does.
pub fn create_session(
    cfg: &Config,
    project: &Project,
    task_name: &str,
    branch: &str,
    use_worktree: bool,
    prompt: Option<&str>,
    agent: AgentKind,
) -> Result<String> {
    let sessions = tmux::list_sessions()?;
    let session_name = tmux::next_session_number(&project.name, task_name, &sessions).to_string();
    create_task_session(
        cfg,
        project,
        task_name,
        branch,
        session_name,
        use_worktree,
        prompt,
        agent,
    )
}

/// Delete a task: kill its sessions, remove their worktrees/branches, drop the
/// session records and the config entry.
pub fn delete_task(project: &Project, task_name: &str) -> Result<()> {
    let task = project
        .tasks
        .iter()
        .find(|t| t.name == task_name)
        .ok_or_else(|| anyhow::anyhow!("task '{task_name}' not found"))?;

    let sessions = tmux::list_sessions().unwrap_or_default();
    tmux::delete_task(
        &project.name,
        &project.path,
        &task.name,
        &task.branch,
        &sessions,
    );
    config::remove_task_session_records(&project.name, &task.name);

    let (project_name, task_name) = (project.name.clone(), task_name.to_string());
    Config::modify(move |c| {
        c.remove_task(&project_name, &task_name);
    })?;

    Ok(())
}

/// Whether a tmux session name belongs to a task's main session.
pub fn is_main_session_name(tmux_name: &str) -> bool {
    tmux_name.ends_with(&format!("__{}", tmux::MAIN_SESSION))
}

/// Kill a session, removing its worktree and per-session branch.
/// Main sessions are owned by their task — delete the task instead.
pub fn kill_session(tmux_name: &str) -> Result<()> {
    if is_main_session_name(tmux_name) {
        bail!("the main session can't be killed — delete the task instead");
    }

    let fallback = config::load_sessions()
        .get(tmux_name)
        .filter(|r| r.use_worktree)
        .map(|r| tmux::SessionCleanupInfo {
            project_path: r.project_path.clone(),
            worktree_path: tmux::worktree_dir(&r.project_name, &r.task_name, &r.session_name)
                .to_string_lossy()
                .to_string(),
            branch_name: Some(format!(
                "{}-{}",
                tmux::sanitize(&r.task_branch),
                tmux::sanitize(&r.session_name)
            )),
            task_branch: Some(r.task_branch.clone()),
        });

    tmux::kill_session_with_fallback(tmux_name, fallback)?;
    config::remove_session_record(tmux_name);
    Ok(())
}

/// The agent pane of a session in the last snapshot: its argv and the agent's
/// own session id (what a fork resumes). Falls back to the bare agent binary
/// when the snapshot has no argv for the pane.
fn agent_pane(source_tmux_name: &str) -> Result<(Vec<String>, Option<String>)> {
    let snap = crate::snapshot::load(&crate::snapshot::snapshot_path())
        .ok_or_else(|| anyhow::anyhow!("no snapshot yet — the daemon must run once to fork"))?;
    let session = snap
        .sessions
        .iter()
        .find(|s| s.name == source_tmux_name)
        .ok_or_else(|| anyhow::anyhow!("no snapshot for '{source_tmux_name}'"))?;
    let pane = session
        .windows
        .iter()
        .flat_map(|w| &w.panes)
        .find(|p| p.agent.is_some())
        .ok_or_else(|| anyhow::anyhow!("no agent pane in '{source_tmux_name}'"))?;
    let agent = pane.agent.as_ref().context("no agent in pane")?;
    let argv = pane
        .argv
        .clone()
        .unwrap_or_else(|| vec![agent.kind.clone()]);
    Ok((argv, agent.session_id.clone()))
}

/// Fork a session: a new tmux session in the same project/task/worktree dir
/// (the fork shares the worktree, no new branch) running the agent's fork
/// command. The new session's record links back to `source_tmux_name` as its
/// `parent`.
pub fn fork_session(source_tmux_name: &str) -> Result<String> {
    let record = config::load_sessions()
        .get(source_tmux_name)
        .cloned()
        .ok_or_else(|| anyhow::anyhow!("no session record for '{source_tmux_name}'"))?;
    let kind = record.agent_kind();
    let (argv, session_id) = agent_pane(source_tmux_name)?;
    let cmd = crate::restore::fork_command(kind, &argv, session_id.as_deref())
        .ok_or_else(|| anyhow::anyhow!("{} has no fork support", kind.id()))?;

    let work_dir = if record.use_worktree {
        tmux::worktree_dir(&record.project_name, &record.task_name, &record.session_name)
            .to_string_lossy()
            .to_string()
    } else {
        record.project_path.clone()
    };

    let sessions = tmux::list_sessions().unwrap_or_default();
    let session_name = tmux::next_session_number(
        &record.project_name,
        &record.task_name,
        &sessions,
    )
    .to_string();
    let tmux_name = tmux::build_tmux_name(&record.project_name, &record.task_name, &session_name);

    let cmd_line = crate::restore::shell_join(&cmd);
    let output = std::process::Command::new("tmux")
        .args([
            "new-session",
            "-d",
            "-s",
            &tmux_name,
            "-c",
            &work_dir,
            &cmd_line,
        ])
        .output()?;
    if !output.status.success() {
        bail!(
            "failed to create tmux session: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }

    let _ = std::process::Command::new("tmux")
        .args([
            "set-environment",
            "-t",
            &tmux_name,
            "CM_PROJECT_PATH",
            &record.project_path,
        ])
        .output();
    let _ = std::process::Command::new("tmux")
        .args([
            "set-environment",
            "-t",
            &tmux_name,
            "CM_TASK_BRANCH",
            &record.task_branch,
        ])
        .output();
    if record.use_worktree {
        let _ = std::process::Command::new("tmux")
            .args([
                "set-environment",
                "-t",
                &tmux_name,
                "CM_WORKTREE_PATH",
                &work_dir,
            ])
            .output();
    }
    crate::tmux::set_session_env(&tmux_name, kind);

    config::add_session_record(
        &tmux_name,
        config::SessionRecord {
            project_name: record.project_name.clone(),
            project_path: record.project_path.clone(),
            task_name: record.task_name.clone(),
            task_branch: record.task_branch.clone(),
            session_name,
            use_worktree: record.use_worktree,
            archived: false,
            auto_closed: false,
            agent: kind.id().to_string(),
            board_card: None,
            parent: Some(source_tmux_name.to_string()),
        },
    );

    Ok(tmux_name)
}

/// Look up a project by name in the loaded config.
pub fn find_project<'a>(cfg: &'a Config, name: &str) -> Result<&'a Project> {
    cfg.projects
        .iter()
        .find(|p| p.name == name)
        .ok_or_else(|| anyhow::anyhow!("project '{name}' not found"))
}
