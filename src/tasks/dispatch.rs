//! The board dispatcher: turns cards in the `Ready` column into agent
//! sessions, up to a per-board and global concurrency cap, and keeps the
//! card's column in step with its session.
//!
//! Only the `native` provider is wired up today; Plane/GitHub/Jira boards are
//! ignored. The pure helpers here are unit-tested; the side-effecting path
//! (creating tmux sessions, probing them, persisting) runs only for real
//! (non-`dry_run`) dispatches against a config and session map the caller
//! supplies, so tests never touch disk or tmux.

use std::collections::HashMap;
use std::time::Duration;

use anyhow::{Context, Result};

use crate::config::{self, Config, Project, SessionRecord};
use crate::inventory::AgentStatus;
use crate::ops;
use crate::tasks::{
    BoardConfig, Card, CardId, Column, Link, provider_for,
};
use crate::tmux;

/// Status of a dispatched card's session: the daemon's hook-derived
/// `@agent_status` (via a tmux probe) plus whether the session is alive.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DispatchSessionStatus {
    /// Agent actively working — card stays (or returns to) Running.
    Alive,
    /// Agent idle: not producing. Finished only once its branch has commits.
    Idle,
    /// Agent is asking the user (question / permission / error).
    NeedsAttention,
    /// Agent finished its turn (`done`): the result is ready to review.
    Finished,
    /// Session or agent process is gone.
    Gone,
}

/// Map a probe result onto a status, using the rich hook status when the
/// session is alive and falling back to the coarse dialog check otherwise.
fn probe_to_status(
    probe: Option<&tmux::SessionProbe>,
    agent: Option<AgentStatus>,
) -> DispatchSessionStatus {
    match probe {
        None => DispatchSessionStatus::Gone,
        Some(p) if !p.agent_alive => DispatchSessionStatus::Gone,
        Some(p) if p.has_permission_prompt => DispatchSessionStatus::NeedsAttention,
        Some(_) => agent_status_to_dispatch(agent),
    }
}

/// The rich status the hook-derived `@agent_status` implies for a card.
/// An unset option (no agent / daemon down) keeps the card Running rather
/// than disrupting a live session.
fn agent_status_to_dispatch(status: Option<AgentStatus>) -> DispatchSessionStatus {
    use AgentStatus::*;
    match status {
        Some(Question | Permission | Error) => DispatchSessionStatus::NeedsAttention,
        Some(Working) => DispatchSessionStatus::Alive,
        Some(Done) => DispatchSessionStatus::Finished,
        Some(Idle) => DispatchSessionStatus::Idle,
        None => DispatchSessionStatus::Alive,
    }
}

/// Count Running cards whose session is alive — any status except Gone — for
/// the per-board and global concurrency cap. A card with no status entry (no
/// live session) is not counted.
fn assess_running(
    cards: &[Card],
    statuses: &HashMap<String, DispatchSessionStatus>,
) -> usize {
    cards
        .iter()
        .filter(|c| c.column == Column::Running)
        .filter(|c| {
            statuses
                .get(&c.id.0)
                .is_some_and(|s| *s != DispatchSessionStatus::Gone)
        })
        .count()
}

/// Drift moves between Running and NeedsYou: a session asking the user sends
/// its card to NeedsYou, and one working again brings it back to Running.
fn status_moves(
    cards: &[Card],
    statuses: &HashMap<String, DispatchSessionStatus>,
) -> Vec<(CardId, Column)> {
    let mut moves = Vec::new();
    for card in cards {
        let Some(status) = statuses.get(&card.id.0) else {
            continue;
        };
        let target = match status {
            DispatchSessionStatus::NeedsAttention => Column::NeedsYou,
            DispatchSessionStatus::Alive => Column::Running,
            _ => continue,
        };
        if target != card.column {
            moves.push((card.id.clone(), target));
        }
    }
    moves
}

/// Whether any live or archived session record for `project` already owns the
/// card, so it must never be dispatched twice (including after a restart).
pub fn card_already_dispatched(
    sessions: &HashMap<String, SessionRecord>,
    project: &str,
    card_id: &str,
) -> bool {
    sessions
        .values()
        .any(|r| r.project_name == project && r.board_card.as_deref() == Some(card_id))
}

/// The tmux session name (map key) and record serving a card, if any.
fn session_record_for_card<'a>(
    sessions: &'a HashMap<String, SessionRecord>,
    project: &str,
    card: &CardId,
) -> Option<(&'a str, &'a SessionRecord)> {
    sessions
        .iter()
        .find(|(_, r)| r.project_name == project && r.board_card.as_deref() == Some(&card.0))
        .map(|(name, r)| (name.as_str(), r))
}

/// Build the status map for a board's Running/Review cards by probing their
/// sessions.
fn running_statuses(
    cards: &[Card],
    sessions: &HashMap<String, SessionRecord>,
    project: &str,
) -> HashMap<String, DispatchSessionStatus> {
    let mut statuses = HashMap::new();
    for card in cards {
        if card.column != Column::Running
            && card.column != Column::Review
            && card.column != Column::NeedsYou
        {
            continue;
        }
        if let Some((tmux_name, _)) = session_record_for_card(sessions, project, &card.id) {
            let probe = tmux::probe_session(tmux_name);
            let agent = tmux::session_agent_status(tmux_name);
            statuses.insert(
                card.id.0.clone(),
                probe_to_status(probe.as_ref(), agent),
            );
        }
    }
    statuses
}

/// The branch a card is dispatched onto: `sr/<key-or-id slug>`.
fn branch_for_card(card: &Card) -> String {
    let slug = if card.key.trim().is_empty() {
        tmux::to_branch_name(&card.id.0)
    } else {
        tmux::to_branch_name(&card.key)
    };
    format!("sr/{slug}")
}

/// A branch name not already in `existing`, suffixing with `-2`, `-3`, …
fn unique_branch(existing: &[String], base: &str) -> String {
    let taken = |b: &str| existing.iter().any(|e| e == b);
    if !taken(base) {
        return base.to_string();
    }
    let mut n = 2;
    loop {
        let candidate = format!("{base}-{n}");
        if !taken(&candidate) {
            return candidate;
        }
        n += 1;
    }
}

/// A task name not already in the project, suffixing with ` 2`, ` 3`, …
fn unique_task_name(project: &Project, base: &str) -> String {
    let taken = |n: &str| project.tasks.iter().any(|t| t.name == n);
    if !taken(base) {
        return base.to_string();
    }
    let mut n = 2;
    loop {
        let candidate = format!("{base} {n}");
        if !taken(&candidate) {
            return candidate;
        }
        n += 1;
    }
}

/// The human-facing name a dispatched working task is derived from.
fn card_base_name(card: &Card) -> String {
    if card.key.trim().is_empty() {
        card.title.clone()
    } else {
        card.key.clone()
    }
}

/// The first message a dispatched agent sees: the template with `{{title}}`,
/// `{{body}}` and `{{key}}` filled, or a default of title + body.
fn render_prompt(board: &BoardConfig, card: &Card) -> Result<String> {
    let Some(path) = &board.prompt_template else {
        return Ok(default_prompt(card));
    };
    let template = std::fs::read_to_string(path)
        .with_context(|| format!("board '{}': read prompt template {path}", board.name))?;
    Ok(template
        .replace("{{title}}", &card.title)
        .replace("{{body}}", &card.body)
        .replace("{{key}}", &card.key))
}

fn default_prompt(card: &Card) -> String {
    if card.body.trim().is_empty() {
        card.title.clone()
    } else {
        format!("{}\n\n{}", card.title, card.body)
    }
}

/// The high-level move a finished card needs, decided purely from its status
/// and whether its branch has commits.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum FinishedAction {
    /// Push the branch, open/link the PR, and move to Review.
    OpenReview,
    /// Comment the agent's last output and move to NeedsYou.
    NeedsYouComment,
}

/// Decide what a finished card needs. An idle agent is only finished once its
/// branch has commits; idle without commits right after dispatch does nothing.
fn plan_finished_action(
    status: DispatchSessionStatus,
    has_commits: bool,
) -> Option<FinishedAction> {
    match status {
        DispatchSessionStatus::Gone | DispatchSessionStatus::Finished => {
            Some(if has_commits {
                FinishedAction::OpenReview
            } else {
                FinishedAction::NeedsYouComment
            })
        }
        DispatchSessionStatus::Idle if has_commits => Some(FinishedAction::OpenReview),
        DispatchSessionStatus::Idle => None,
        DispatchSessionStatus::Alive | DispatchSessionStatus::NeedsAttention => None,
    }
}

/// A PR-on-done action for a card whose session has finished.
#[derive(Debug, Clone, PartialEq, Eq)]
enum PrAction {
    /// Link the PR the agent already opened and move the card to Review.
    Review { url: String },
    /// Push the branch, open a PR, link it, and move to Review.
    OpenPr,
    /// Move the card back to the user with the session's last output.
    NeedsYouComment(String),
}

/// A card is eligible for the finish path only while it's Running; a Review
/// card already finished (its PR is open) and must never be re-finished, or
/// every poll would re-link the PR.
fn finish_eligible(card: &Card) -> bool {
    card.column == Column::Running
}

/// Plan the finish of Running cards whose session is done, idle with commits,
/// or gone. Only cards with a live session record are considered; a failing
/// card must never abort the pass.
fn plan_pr_done(
    cards: &[Card],
    statuses: &HashMap<String, DispatchSessionStatus>,
    sessions: &HashMap<String, SessionRecord>,
    project: &str,
    project_path: &str,
    base_branches: &HashMap<String, String>,
) -> Vec<(CardId, PrAction)> {
    let mut out = Vec::new();
    for card in cards {
        if !finish_eligible(card) {
            continue;
        }
        let Some(status) = statuses.get(&card.id.0).copied() else {
            continue;
        };
        let Some((tmux_name, rec)) = session_record_for_card(sessions, project, &card.id) else {
            continue;
        };
        let branch = &rec.task_branch;
        let base = base_branches
            .get(branch)
            .map(String::as_str)
            .unwrap_or("main");
        let has_commits = tmux::get_branch_diff(project_path, branch, base)
            .is_some_and(|d| d.added + d.removed > 0);
        let Some(action) = plan_finished_action(status, has_commits) else {
            continue;
        };
        match action {
            FinishedAction::OpenReview => {
                if let Some(url) = tmux::get_pr_url(project_path, branch) {
                    out.push((card.id.clone(), PrAction::Review { url }));
                } else {
                    out.push((card.id.clone(), PrAction::OpenPr));
                }
            }
            FinishedAction::NeedsYouComment => {
                let tail = tmux::capture_output_plain(tmux_name, 40).unwrap_or_default();
                out.push((card.id.clone(), PrAction::NeedsYouComment(tail)));
            }
        }
    }
    out
}

/// Review cards whose PR has been merged, to move to Done and close the
/// session. A card with no session record can't be acted on and is skipped.
fn plan_merged(
    cards: &[Card],
    sessions: &HashMap<String, SessionRecord>,
    project: &str,
    project_path: &str,
) -> Vec<CardId> {
    let mut out = Vec::new();
    for card in cards {
        if card.column != Column::Review {
            continue;
        }
        let Some((_, rec)) = session_record_for_card(sessions, project, &card.id) else {
            continue;
        };
        if tmux::pr_is_merged(project_path, &rec.task_branch) {
            out.push(card.id.clone());
        }
    }
    out
}

/// Archive-and-close a merged card's session via the existing snapshot path,
/// then mark the record auto-closed so the TUI's startup restore doesn't
/// resurrect it. The record stays, so the card is never dispatched twice.
fn close_card_session(cfg: &Config, tmux_name: &str) {
    let now = crate::control::now();
    let panes = crate::inventory::list_panes();
    let mut snap = crate::snapshot::take(
        &panes,
        &HashMap::new(),
        &cfg.restore.redact,
        now,
    );
    let target = crate::autoclose::Target::Session(tmux_name.to_string());
    let _ = crate::snapshot::archive_and_close(
        &mut snap,
        vec![target],
        crate::snapshot::CloseReason::AutoClose,
        now,
    );
    config::set_session_auto_closed(tmux_name, true);
}

/// Push the card's branch and open a PR for it (title = card title, body =
/// card body plus its URL). Reuses the TUI's push + `gh pr create` flow.
fn open_pr(project_path: &str, branch: &str, card: &Card) -> Result<String> {
    tmux::push_branch(project_path, branch)?;
    let mut body = card.body.clone();
    if let Some(url) = &card.url {
        body.push_str(&format!("\n\nCard: {url}"));
    }
    let output = std::process::Command::new("gh")
        .args([
            "pr", "create", "--fill", "--head", branch,
            "--title", &card.title, "--body", &body,
        ])
        .current_dir(project_path)
        .output()?;
    if !output.status.success() {
        anyhow::bail!(
            "gh pr create failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
}

/// Carry out one planned finish action on a card: link or open the PR, or
/// comment the agent's last output and send the card back to the user.
fn apply_pr_action(
    cards: &[Card],
    sessions: &HashMap<String, SessionRecord>,
    project: &str,
    project_path: &str,
    provider: &mut dyn crate::tasks::TaskProvider,
    id: &CardId,
    action: &PrAction,
) -> Result<()> {
    match action {
        PrAction::Review { url } => {
            provider.link(id, Link::PullRequest, url)?;
            provider.move_to(id, Column::Review)?;
        }
        PrAction::OpenPr => {
            let Some((_, rec)) = session_record_for_card(sessions, project, id) else {
                return Ok(());
            };
            let Some(card) = cards.iter().find(|c| c.id == *id) else {
                return Ok(());
            };
            let url = open_pr(project_path, &rec.task_branch, card)?;
            provider.link(id, Link::PullRequest, &url)?;
            provider.move_to(id, Column::Review)?;
        }
        PrAction::NeedsYouComment(tail) => {
            provider.comment(
                id,
                &format!("finished without commits; last output:\n{tail}"),
            )?;
            provider.move_to(id, Column::NeedsYou)?;
        }
    }
    Ok(())
}

/// Human-readable descriptions of every planned action, for dry-run reports.
fn plan_descriptions(
    moves: &[(CardId, Column)],
    pr_actions: &[(CardId, PrAction)],
    merged: &[CardId],
) -> Vec<String> {
    let mut out = Vec::new();
    out.extend(moves.iter().map(|(id, col)| format!("move {} -> {col:?}", id.0)));
    out.extend(pr_actions.iter().map(|(id, a)| match a {
        PrAction::Review { .. } => format!("link PR {} -> review", id.0),
        PrAction::OpenPr => format!("push + open PR {} -> review", id.0),
        PrAction::NeedsYouComment(_) => format!("comment {} -> needs you", id.0),
    }));
    out.extend(merged.iter().map(|id| format!("PR merged {} -> done", id.0)));
    out
}

/// Result of one dispatch pass, per board.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct DispatchSummary {
    /// Number of cards newly dispatched across all boards.
    pub dispatched: usize,
    /// Per-board detail, in config order.
    pub boards: Vec<BoardSummary>,
}

/// Per-board dispatch detail.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct BoardSummary {
    pub name: String,
    pub dispatched: Vec<String>,
    pub running: usize,
    pub ready: usize,
    /// Actions a dry-run pass *would* take, human-readable.
    pub planned: Vec<String>,
}

/// Run one dispatch pass against an in-memory config and session map. When
/// `dry_run` is set it only reports what *would* be dispatched: no sessions
/// are created and nothing is persisted.
pub fn dispatch_cfg(
    cfg: &mut Config,
    sessions: &HashMap<String, SessionRecord>,
    dry_run: bool,
) -> Result<DispatchSummary> {
    let mut summary = DispatchSummary::default();
    if cfg.dispatch.max_running == 0 || cfg.boards.is_empty() {
        return Ok(summary);
    }

    let mut global_running = 0;
    let boards = cfg.boards.clone();
    for board in &boards {
        // A remote board that can't be reached must not stop the others.
        let cards = match provider_for(cfg, board).and_then(|p| p.list()) {
            Ok(cards) => cards,
            Err(e) => {
                eprintln!("dispatcher: board {}: {e:#}", board.name);
                continue;
            }
        };
        let project = ops::find_project(&*cfg, &board.project)?;
        let project_path = project.path.clone();
        // Base branch per task branch, so a PR/diff compares against the
        // task's own base rather than hardcoding "main".
        let base_branches: HashMap<String, String> = project
            .tasks
            .iter()
            .map(|t| {
                (
                    t.branch.clone(),
                    t.base_branch.clone().unwrap_or_else(|| "main".to_string()),
                )
            })
            .collect();
        let statuses = running_statuses(&cards, sessions, &board.project);
        let running = assess_running(&cards, &statuses);
        global_running += running;

        // Status drift: NeedsYou <-> Running as the agent asks / works again.
        let moves = status_moves(&cards, &statuses);
        // Finish: done / idle-with-commits / gone cards open (or link) a PR.
        let pr_actions =
            plan_pr_done(&cards, &statuses, sessions, &board.project, &project_path, &base_branches);
        // Merged PRs close the card's session and move it to Done.
        let merged = plan_merged(&cards, sessions, &board.project, &project_path);
        let planned = plan_descriptions(&moves, &pr_actions, &merged);

        if !dry_run {
            let mut provider = provider_for(cfg, board)?;
            for (id, col) in &moves {
                // One card failing must not abort the pass: log it and move on.
                if let Err(e) = provider.move_to(id, *col) {
                    eprintln!("dispatcher: card {}: {e:#}", id.0);
                }
            }
            for (id, action) in &pr_actions {
                if let Err(e) = apply_pr_action(
                    &cards,
                    sessions,
                    &board.project,
                    &project_path,
                    provider.as_mut(),
                    id,
                    action,
                ) {
                    eprintln!("dispatcher: card {}: {e:#}", id.0);
                }
            }
            for id in &merged {
                if let Err(e) = provider.move_to(id, Column::Done) {
                    eprintln!("dispatcher: card {}: move to done: {e:#}", id.0);
                }
            }
        }
        // Archive-and-close merged sessions once the provider is dropped.
        if !dry_run && !merged.is_empty() {
            for id in &merged {
                if let Some((tmux_name, _)) =
                    session_record_for_card(sessions, &board.project, id)
                {
                    close_card_session(cfg, tmux_name);
                }
            }
        }

        // Ready cards not already dispatched, highest priority first.
        let mut ready: Vec<Card> = cards
            .iter()
            .filter(|c| {
                c.column == Column::Ready
                    && !card_already_dispatched(sessions, &board.project, &c.id.0)
            })
            .cloned()
            .collect();
        ready.sort_by_key(|c| std::cmp::Reverse(c.priority.unwrap_or(0)));

        let mut board_summary = BoardSummary {
            name: board.name.clone(),
            dispatched: vec![],
            running,
            ready: ready.len(),
            planned,
        };
        let mut running = running;
        for card in ready {
            if running >= board.max_running || global_running >= cfg.dispatch.max_running {
                break;
            }
            if dry_run {
                board_summary.dispatched.push(card.key.clone());
                summary.dispatched += 1;
                running += 1;
                global_running += 1;
                continue;
            }

            // One card failing must not abort the pass: log it and move on.
            if let Err(e) = dispatch_card(cfg, board, &card) {
                eprintln!("dispatcher: card {}: {e:#}", card.key);
                continue;
            }
            // Persist each dispatch so a later failure can't lose the move.
            cfg.save()?;

            board_summary.dispatched.push(card.key.clone());
            summary.dispatched += 1;
            running += 1;
            global_running += 1;
        }
        summary.boards.push(board_summary);
    }

    if !dry_run {
        cfg.save()?;
    }
    Ok(summary)
}

/// Create the working task (branch, worktree, session) for one Ready card,
/// record which card it serves, then move the card to Running.
fn dispatch_card(cfg: &mut Config, board: &BoardConfig, card: &Card) -> Result<()> {
    let (task_name, branch, prompt, agent) = {
        let project = ops::find_project(&*cfg, &board.project)?;
        let task_name = unique_task_name(project, &card_base_name(card));
        let branch = unique_branch(&task_branches(project), &branch_for_card(card));
        let prompt = render_prompt(board, card)?;
        let agent = ops::resolve_agent(&*cfg, board.agent.as_deref())?;
        (task_name, branch, prompt, agent)
    };
    cfg.add_task(&board.project, task_name.clone(), branch.clone());
    let tmux_name = {
        let project = ops::find_project(&*cfg, &board.project)?;
        tmux::create_task_branch(&project.path, &branch, None)?;
        ops::create_task_session(
            &*cfg,
            project,
            &task_name,
            &branch,
            tmux::MAIN_SESSION.to_string(),
            true,
            Some(&prompt),
            agent,
        )?
    };
    config::set_session_record_board_card(&tmux_name, card.id.0.clone());
    let mut provider = provider_for(cfg, board)?;
    provider.move_to(&card.id, Column::Running)?;
    provider.comment(&card.id, &format!("session {tmux_name} on branch {branch}"))?;
    provider.link(&card.id, Link::Branch, &format!("branch:{branch}"))?;
    Ok(())
}

fn task_branches(project: &Project) -> Vec<String> {
    project.tasks.iter().map(|t| t.branch.clone()).collect()
}

/// Load the config and sessions and run one dispatch pass against the real
/// on-disk state. A no-op when dispatch is paused.
pub fn dispatch_once(dry_run: bool) -> Result<DispatchSummary> {
    if is_paused() {
        return Ok(DispatchSummary::default());
    }
    let mut cfg = Config::load()?;
    let sessions = config::load_sessions();
    dispatch_cfg(&mut cfg, &sessions, dry_run)
}

/// Whether the dispatcher is paused (a `dispatch.paused` marker in the data
/// dir). A no-op pass while paused.
pub fn is_paused() -> bool {
    crate::config::base_dir().join("dispatch.paused").exists()
}

/// Pause or resume the dispatcher.
pub fn set_paused(paused: bool) -> Result<()> {
    let path = crate::config::base_dir().join("dispatch.paused");
    if paused {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(&path, "")?;
    } else {
        let _ = std::fs::remove_file(&path);
    }
    Ok(())
}

/// Per-board card listing for `board list`.
pub struct BoardCards {
    pub name: String,
    pub cards: Vec<Card>,
}

pub fn board_cards(cfg: &mut Config) -> Result<Vec<BoardCards>> {
    let mut out = Vec::new();
    let boards = cfg.boards.clone();
    for board in &boards {
        let provider = provider_for(cfg, board)?;
        out.push(BoardCards {
            name: board.name.clone(),
            cards: provider.list()?,
        });
    }
    Ok(out)
}

/// Per-board running/ready/needs-you counts for `board status`.
pub struct BoardStatus {
    pub name: String,
    pub running: usize,
    pub ready: usize,
    pub needs_you: usize,
}

pub fn board_status(
    cfg: &mut Config,
    sessions: &HashMap<String, SessionRecord>,
) -> Result<Vec<BoardStatus>> {
    let mut out = Vec::new();
    let boards = cfg.boards.clone();
    for board in &boards {
        let cards = {
            let provider = provider_for(cfg, board)?;
            provider.list()?
        };
        let statuses = running_statuses(&cards, sessions, &board.project);
        let running = assess_running(&cards, &statuses);
        out.push(BoardStatus {
            name: board.name.clone(),
            running,
            ready: cards.iter().filter(|c| c.column == Column::Ready).count(),
            needs_you: cards.iter().filter(|c| c.column == Column::NeedsYou).count(),
        });
    }
    Ok(out)
}

/// The daemon's dispatcher loop: poll every `poll_secs`, cheap when no boards
/// are configured, and never panics (errors are logged and the loop continues).
pub fn spawn_dispatcher() -> std::thread::JoinHandle<()> {
    std::thread::spawn(|| loop {
        // Load fresh each tick so edits and the pause flag are picked up.
        let (poll_secs, has_boards) = match Config::load() {
            Ok(c) => (c.dispatch.poll_secs.max(1), !c.boards.is_empty()),
            Err(e) => {
                eprintln!("dispatcher: failed to load config: {e:#}");
                (60, false)
            }
        };
        if !has_boards || is_paused() {
            std::thread::sleep(Duration::from_secs(poll_secs));
            continue;
        }
        match dispatch_once(false) {
            Ok(s) if s.dispatched > 0 => {
                eprintln!("dispatcher: dispatched {} card(s)", s.dispatched)
            }
            Ok(_) => {}
            Err(e) => eprintln!("dispatcher: {e:#}"),
        }
        std::thread::sleep(Duration::from_secs(poll_secs));
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tasks::ProviderKind;
    use crate::config::Task;

    fn card_task(name: &str, branch: &str, col: Column) -> Task {
        Task {
            name: name.to_string(),
            branch: branch.to_string(),
            base_branch: None,
            archived: false,
            column: Some(col),
            body: None,
            notes: vec![],
            links: vec![],
        }
    }

    fn board(name: &str, project: &str, max_running: usize) -> BoardConfig {
        BoardConfig {
            name: name.to_string(),
            provider: ProviderKind::Native,
            project: project.to_string(),
            agent: None,
            max_running,
            prompt_template: None,
            plane: None,
            github: None,
            jira: None,
            columns: HashMap::new(),
        }
    }

    fn card(id: &str, col: Column, priority: Option<u8>) -> Card {
        Card {
            id: CardId(id.to_string()),
            key: id.to_string(),
            title: id.to_string(),
            body: String::new(),
            column: col,
            labels: vec![],
            priority,
            url: None,
        }
    }

    fn session(project: &str, board_card: Option<&str>) -> SessionRecord {
        SessionRecord {
            project_name: project.to_string(),
            project_path: "/tmp/p".to_string(),
            task_name: "t".to_string(),
            task_branch: "sr/x".to_string(),
            session_name: "main".to_string(),
            use_worktree: true,
            archived: false,
            auto_closed: false,
            agent: "claude".to_string(),
            board_card: board_card.map(str::to_string),
        }
    }

    #[test]
    fn already_dispatched_matches_board_card() {
        let mut sessions = HashMap::new();
        sessions.insert("s1".to_string(), session("proj", Some("fix/bug")));
        assert!(card_already_dispatched(&sessions, "proj", "fix/bug"));
        // Archived records still own the card.
        sessions.get_mut("s1").unwrap().archived = true;
        assert!(card_already_dispatched(&sessions, "proj", "fix/bug"));
        // Other project or other card id: not owned.
        assert!(!card_already_dispatched(&sessions, "proj", "other"));
        assert!(!card_already_dispatched(&sessions, "other", "fix/bug"));
        // Records without a board card never own anything.
        let mut sessions = HashMap::new();
        sessions.insert("s2".to_string(), session("proj", None));
        assert!(!card_already_dispatched(&sessions, "proj", "fix/bug"));
    }

    #[test]
    fn assess_running_counts_alive_sessions_only() {
        let cards = vec![
            card("a", Column::Running, None),
            card("b", Column::Running, None),
            card("c", Column::Ready, None),
        ];
        let mut statuses = HashMap::new();
        statuses.insert("a".to_string(), DispatchSessionStatus::Alive);
        statuses.insert("b".to_string(), DispatchSessionStatus::NeedsAttention);
        // Any status except Gone counts toward the cap.
        assert_eq!(assess_running(&cards, &statuses), 2);
        // A card with no status entry (no live session) isn't counted.
        assert_eq!(assess_running(&cards[..1], &HashMap::new()), 0);
    }

    #[test]
    fn gone_session_does_not_count() {
        let cards = vec![card("a", Column::Running, None)];
        let mut statuses = HashMap::new();
        statuses.insert("a".to_string(), DispatchSessionStatus::Gone);
        assert_eq!(assess_running(&cards, &statuses), 0);
    }

    #[test]
    fn status_moves_needs_you_and_back_to_running() {
        let cards = vec![
            card("ask", Column::Running, None),
            card("back", Column::NeedsYou, None),
            card("idle", Column::Running, None),
        ];
        let mut statuses = HashMap::new();
        statuses.insert("ask".to_string(), DispatchSessionStatus::NeedsAttention);
        statuses.insert("back".to_string(), DispatchSessionStatus::Alive);
        statuses.insert("idle".to_string(), DispatchSessionStatus::Idle);
        let moves = status_moves(&cards, &statuses);
        // Asking card goes to NeedsYou; working card returns to Running; an
        // idle card isn't drifted.
        assert_eq!(moves.len(), 2);
        assert!(moves.contains(&(CardId("ask".to_string()), Column::NeedsYou)));
        assert!(moves.contains(&(CardId("back".to_string()), Column::Running)));
    }

    #[test]
    fn finished_action_maps_status_and_commits() {
        use DispatchSessionStatus::*;
        // Done with commits -> review; done without commits -> needs you.
        assert_eq!(
            plan_finished_action(Finished, true),
            Some(FinishedAction::OpenReview)
        );
        assert_eq!(
            plan_finished_action(Finished, false),
            Some(FinishedAction::NeedsYouComment)
        );
        // Gone follows the same rule.
        assert_eq!(
            plan_finished_action(Gone, true),
            Some(FinishedAction::OpenReview)
        );
        assert_eq!(
            plan_finished_action(Gone, false),
            Some(FinishedAction::NeedsYouComment)
        );
        // Idle only finishes once its branch has commits.
        assert_eq!(
            plan_finished_action(Idle, true),
            Some(FinishedAction::OpenReview)
        );
        assert_eq!(plan_finished_action(Idle, false), None);
        // Working / asking never finish here.
        assert_eq!(plan_finished_action(Alive, true), None);
        assert_eq!(plan_finished_action(NeedsAttention, true), None);
    }

    #[test]
    fn agent_status_to_dispatch_mapping() {
        use AgentStatus::*;
        assert_eq!(
            agent_status_to_dispatch(Some(Question)),
            DispatchSessionStatus::NeedsAttention
        );
        assert_eq!(
            agent_status_to_dispatch(Some(Permission)),
            DispatchSessionStatus::NeedsAttention
        );
        assert_eq!(
            agent_status_to_dispatch(Some(Error)),
            DispatchSessionStatus::NeedsAttention
        );
        assert_eq!(
            agent_status_to_dispatch(Some(Working)),
            DispatchSessionStatus::Alive
        );
        assert_eq!(
            agent_status_to_dispatch(Some(Done)),
            DispatchSessionStatus::Finished
        );
        assert_eq!(
            agent_status_to_dispatch(Some(Idle)),
            DispatchSessionStatus::Idle
        );
        // Unset status keeps a live session Running, never disrupting it.
        assert_eq!(
            agent_status_to_dispatch(None),
            DispatchSessionStatus::Alive
        );
    }

    #[test]
    fn probe_to_status_combines_probe_and_agent() {
        let probe = tmux::SessionProbe {
            agent_alive: true,
            content_hash: 0,
            has_permission_prompt: false,
        };
        assert_eq!(
            probe_to_status(Some(&probe), Some(AgentStatus::Working)),
            DispatchSessionStatus::Alive
        );
        // A permission dialog wins even if the hook status is stale.
        let dialog = tmux::SessionProbe {
            agent_alive: true,
            content_hash: 0,
            has_permission_prompt: true,
        };
        assert_eq!(
            probe_to_status(Some(&dialog), Some(AgentStatus::Working)),
            DispatchSessionStatus::NeedsAttention
        );
        // A dead or missing session is gone.
        assert_eq!(
            probe_to_status(None, Some(AgentStatus::Working)),
            DispatchSessionStatus::Gone
        );
        let dead = tmux::SessionProbe {
            agent_alive: false,
            content_hash: 0,
            has_permission_prompt: false,
        };
        assert_eq!(
            probe_to_status(Some(&dead), Some(AgentStatus::Done)),
            DispatchSessionStatus::Gone
        );
    }

    #[test]
    fn review_cards_are_never_re_finished() {
        let cards = vec![card("r", Column::Review, None)];
        let mut statuses = HashMap::new();
        statuses.insert("r".to_string(), DispatchSessionStatus::Finished);
        let mut sessions = HashMap::new();
        sessions.insert("s1".to_string(), session("proj", Some("r")));
        let actions = plan_pr_done(
            &cards,
            &statuses,
            &sessions,
            "proj",
            "/tmp/p",
            &HashMap::new(),
        );
        // A Review card is already finished; re-running the finish path would
        // spam a new PR link every poll.
        assert!(actions.is_empty());
    }

    #[test]
    fn branch_for_card_slugs_and_prefixes() {
        let c = card("fix/bug", Column::Ready, None);
        assert_eq!(branch_for_card(&c), "sr/fix-bug");
        let c2 = card("SR-42", Column::Ready, None);
        assert_eq!(branch_for_card(&c2), "sr/sr-42");
    }

    #[test]
    fn unique_branch_suffixes_collisions() {
        let existing = vec!["sr/fix-bug".to_string(), "sr/fix-bug-2".to_string()];
        assert_eq!(unique_branch(&existing, "sr/fix-bug"), "sr/fix-bug-3");
        assert_eq!(unique_branch(&existing, "sr/other"), "sr/other");
    }

    #[test]
    fn dispatch_dry_run_reports_ready_cards() {
        let mut cfg = Config::default();
        cfg.dispatch.max_running = 2;
        let mut project = Project {
            name: "proj".to_string(),
            path: "/tmp/p".to_string(),
            tasks: vec![
                card_task("Fix bug", "fix/bug", Column::Ready),
                card_task("Other", "other", Column::Ready),
            ],
            copy_patterns: vec![],
            setup_commands: vec![],
            run_command: None,
        };
        project.tasks[0].body = Some("do it".to_string());
        project.tasks[0].notes = vec![];
        cfg.projects.push(project);
        cfg.boards.push(board("b", "proj", 1));

        let sessions = HashMap::new();
        let summary = dispatch_cfg(&mut cfg, &sessions, true).unwrap();
        assert_eq!(summary.dispatched, 1);
        assert_eq!(summary.boards.len(), 1);
        assert_eq!(summary.boards[0].name, "b");
        assert_eq!(summary.boards[0].ready, 2);
        assert_eq!(summary.boards[0].dispatched, vec!["Fix bug".to_string()]);
        // Dry run leaves the config untouched.
        assert_eq!(cfg.projects[0].tasks[0].column, Some(Column::Ready));
    }

    #[test]
    fn dispatch_skips_already_dispatched_cards() {
        let mut cfg = Config::default();
        cfg.dispatch.max_running = 2;
        let project = Project {
            name: "proj".to_string(),
            path: "/tmp/p".to_string(),
            tasks: vec![card_task("Fix bug", "fix/bug", Column::Ready)],
            copy_patterns: vec![],
            setup_commands: vec![],
            run_command: None,
        };
        cfg.projects.push(project);
        cfg.boards.push(board("b", "proj", 1));

        let mut sessions = HashMap::new();
        sessions.insert("s1".to_string(), session("proj", Some("fix/bug")));
        let summary = dispatch_cfg(&mut cfg, &sessions, true).unwrap();
        assert_eq!(summary.dispatched, 0);
        assert_eq!(summary.boards[0].ready, 0);
    }

    #[test]
    fn dispatch_noop_when_cap_zero() {
        let mut cfg = Config::default();
        cfg.dispatch.max_running = 0;
        cfg.boards.push(board("b", "proj", 1));
        let summary = dispatch_cfg(&mut cfg, &HashMap::new(), true).unwrap();
        assert_eq!(summary.dispatched, 0);
        assert!(summary.boards.is_empty());
    }
}
