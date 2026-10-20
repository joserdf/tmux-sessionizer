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
use crate::ops;
use crate::tasks::{
    BoardConfig, Card, CardId, Column, Link, ProviderKind, provider_for,
};
use crate::tmux;

/// Coarse status of a dispatched card's session, derived from a tmux probe.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DispatchSessionStatus {
    /// Agent alive and not blocked on a prompt.
    Alive,
    /// Agent is asking the user something (permission / question / error).
    NeedsAttention,
    /// Session or agent is gone.
    Gone,
}

/// Map a probe result onto a coarse status.
fn probe_to_status(probe: Option<&tmux::SessionProbe>) -> DispatchSessionStatus {
    match probe {
        None => DispatchSessionStatus::Gone,
        Some(p) if !p.agent_alive => DispatchSessionStatus::Gone,
        Some(p) if p.has_permission_prompt => DispatchSessionStatus::NeedsAttention,
        Some(_) => DispatchSessionStatus::Alive,
    }
}

/// The canonical column a session status implies for a Running card, or None
/// when the status shouldn't change the card (e.g. the session is gone).
fn column_for_status(status: DispatchSessionStatus) -> Option<Column> {
    match status {
        DispatchSessionStatus::Alive => Some(Column::Running),
        DispatchSessionStatus::NeedsAttention => Some(Column::NeedsYou),
        DispatchSessionStatus::Gone => None,
    }
}

/// Count Running cards whose session is alive, and the Running→NeedsYou moves
/// implied by sessions that need the user. Only cards in `Running` are
/// considered; a card with no status entry (no live session) is neither
/// counted nor moved.
fn assess_running(
    cards: &[Card],
    statuses: &HashMap<String, DispatchSessionStatus>,
) -> (usize, Vec<(CardId, Column)>) {
    let mut running = 0;
    let mut changes = Vec::new();
    for card in cards {
        if card.column != Column::Running {
            continue;
        }
        let Some(status) = statuses.get(&card.id.0) else { continue };
        match column_for_status(*status) {
            Some(Column::Running) => running += 1,
            Some(col) => changes.push((card.id.clone(), col)),
            None => {}
        }
    }
    (running, changes)
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
        if card.column != Column::Running && card.column != Column::Review {
            continue;
        }
        if let Some((tmux_name, _)) = session_record_for_card(sessions, project, &card.id) {
            let probe = tmux::probe_session(tmux_name);
            statuses.insert(card.id.0.clone(), probe_to_status(probe.as_ref()));
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

/// A PR-on-done action for a card whose session has finished.
#[derive(Debug, Clone, PartialEq, Eq)]
enum PrAction {
    /// Link the PR the agent already opened and move the card to Review.
    Review { url: String },
    /// Move the card back to the user with the session's last output.
    NeedsYouComment(String),
}

/// Best-effort PR handling for finished Running/Review cards. Reacts only to
/// PRs the agent already opened (via `gh`); it never pushes or opens a PR, to
/// avoid network side effects. Merged→Done and archive-and-close are deferred
/// to a later wave.
fn plan_pr_done(
    cards: &[Card],
    statuses: &HashMap<String, DispatchSessionStatus>,
    sessions: &HashMap<String, SessionRecord>,
    project: &str,
    project_path: &str,
) -> Vec<(CardId, PrAction)> {
    let mut out = Vec::new();
    for card in cards {
        if card.column != Column::Running && card.column != Column::Review {
            continue;
        }
        // Only react once the session's agent has finished.
        if statuses.get(&card.id.0) != Some(&DispatchSessionStatus::Gone) {
            continue;
        }
        let Some((tmux_name, rec)) = session_record_for_card(sessions, project, &card.id) else {
            continue;
        };
        let branch = &rec.task_branch;
        let has_commits = tmux::get_branch_diff(project_path, branch, "main")
            .is_some_and(|d| d.added + d.removed > 0);
        if has_commits {
            if let Some(url) = tmux::get_pr_url(project_path, branch) {
                out.push((card.id.clone(), PrAction::Review { url }));
            }
            // A PR isn't open yet: leave the card Running.
        } else {
            let tail = tmux::capture_output_plain(tmux_name, 40).unwrap_or_default();
            out.push((card.id.clone(), PrAction::NeedsYouComment(tail)));
        }
    }
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
        if board.provider != ProviderKind::Native {
            continue;
        }

        let cards = {
            let provider = provider_for(cfg, board)?;
            provider.list()?
        };
        let project_path = ops::find_project(&*cfg, &board.project)?.path.clone();
        let statuses = running_statuses(&cards, sessions, &board.project);
        let (running, needs_you) = assess_running(&cards, &statuses);
        global_running += running;

        // Status mapping: move Running cards whose session needs the user.
        if !dry_run && !needs_you.is_empty() {
            let mut provider = provider_for(cfg, board)?;
            for (id, col) in &needs_you {
                provider.move_to(id, *col)?;
            }
        }

        // PR-on-done for finished sessions (best-effort, no push/open).
        let pr_actions = plan_pr_done(&cards, &statuses, sessions, &board.project, &project_path);
        if !dry_run && !pr_actions.is_empty() {
            let mut provider = provider_for(cfg, board)?;
            for (id, action) in &pr_actions {
                match action {
                    PrAction::Review { url } => {
                        provider.link(id, Link::PullRequest, url)?;
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
        if board.provider != ProviderKind::Native {
            continue;
        }
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
        if board.provider != ProviderKind::Native {
            continue;
        }
        let cards = {
            let provider = provider_for(cfg, board)?;
            provider.list()?
        };
        let statuses = running_statuses(&cards, sessions, &board.project);
        let (running, _) = assess_running(&cards, &statuses);
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
    fn assess_running_counts_alive_and_moves_needs_attention() {
        let cards = vec![
            card("a", Column::Running, None),
            card("b", Column::Running, None),
            card("c", Column::Ready, None),
        ];
        let mut statuses = HashMap::new();
        statuses.insert("a".to_string(), DispatchSessionStatus::Alive);
        statuses.insert("b".to_string(), DispatchSessionStatus::NeedsAttention);
        let (running, changes) = assess_running(&cards, &statuses);
        assert_eq!(running, 1);
        assert_eq!(changes.len(), 1);
        assert_eq!(changes[0].0, CardId("b".to_string()));
        assert_eq!(changes[0].1, Column::NeedsYou);
    }

    #[test]
    fn gone_session_neither_counts_nor_moves() {
        let cards = vec![card("a", Column::Running, None)];
        let mut statuses = HashMap::new();
        statuses.insert("a".to_string(), DispatchSessionStatus::Gone);
        let (running, changes) = assess_running(&cards, &statuses);
        assert_eq!(running, 0);
        assert!(changes.is_empty());
    }

    #[test]
    fn probe_status_mapping() {
        assert_eq!(column_for_status(DispatchSessionStatus::Alive), Some(Column::Running));
        assert_eq!(
            column_for_status(DispatchSessionStatus::NeedsAttention),
            Some(Column::NeedsYou)
        );
        assert_eq!(column_for_status(DispatchSessionStatus::Gone), None);
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
