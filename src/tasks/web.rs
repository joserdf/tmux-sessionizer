//! JSON shaping for the web kanban board.
//!
//! Pure functions that turn a [`Config`] (cards) plus a session map into the
//! `/api/v2/boards` DTO. They never touch disk or tmux, so the shaping is
//! unit-tested in isolation; the handlers that call them run inside
//! `spawn_blocking`.

use std::collections::HashMap;

use anyhow::Result;
use serde_json::{Value, json};

use crate::config::{Config, SessionRecord};
use crate::tasks::{
    BoardConfig, Card, CardId, Column, ProviderKind, provider_for,
};

/// Canonical column order, left to right across the board.
const COLUMN_ORDER: [Column; 6] = [
    Column::Backlog,
    Column::Ready,
    Column::Running,
    Column::NeedsYou,
    Column::Review,
    Column::Done,
];

/// The tmux session name whose record serves `card` for `project`, if any.
/// A card with a live or archived session record is never shown as idle.
pub fn card_session<'a>(
    sessions: &'a HashMap<String, SessionRecord>,
    project: &str,
    card: &CardId,
) -> Option<&'a str> {
    sessions
        .iter()
        .find(|(_, r)| r.project_name == project && r.board_card.as_deref() == Some(&card.0))
        .map(|(name, _)| name.as_str())
}

/// One card's JSON, with its owning tmux session (or null when undispatched).
pub fn card_json(card: &Card, session: Option<&str>) -> Value {
    json!({
        "id": card.id.0,
        "key": card.key,
        "title": card.title,
        "body": card.body,
        "column": card.column,
        "labels": card.labels,
        "priority": card.priority,
        "url": card.url,
        "session": session,
    })
}

/// One board's JSON: its columns (all six, in board order) each holding that
/// column's cards.
pub fn board_json(
    cfg: &mut Config,
    board: &BoardConfig,
    sessions: &HashMap<String, SessionRecord>,
) -> Result<Value> {
    let cards = {
        let provider = provider_for(cfg, board)?;
        provider.list()?
    };
    let columns: Vec<Value> = COLUMN_ORDER
        .iter()
        .map(|col| {
            let column_cards: Vec<Value> = cards
                .iter()
                .filter(|c| c.column == *col)
                .map(|c| card_json(c, card_session(sessions, &board.project, &c.id)))
                .collect();
            json!({ "column": col, "cards": column_cards })
        })
        .collect();
    Ok(json!({
        "name": board.name,
        "project": board.project,
        "paused": crate::tasks::dispatch::is_paused(),
        "columns": columns,
    }))
}

/// Every native board as a JSON array, in config order.
pub fn boards_json(
    cfg: &mut Config,
    sessions: &HashMap<String, SessionRecord>,
) -> Result<Vec<Value>> {
    let boards = cfg.boards.clone();
    let mut out = Vec::new();
    for board in &boards {
        if board.provider != ProviderKind::Native {
            continue;
        }
        out.push(board_json(cfg, board, sessions)?);
    }
    Ok(out)
}

/// A dispatch summary as JSON, so the web UI can show what ran / would run.
pub fn dispatch_summary_json(summary: &crate::tasks::dispatch::DispatchSummary) -> Value {
    json!({
        "dispatched": summary.dispatched,
        "boards": summary.boards.iter().map(|b| json!({
            "name": b.name,
            "dispatched": b.dispatched,
            "running": b.running,
            "ready": b.ready,
        })).collect::<Vec<_>>(),
    })
}

/// The canonical column name (snake_case) at `index`, or None past the end.
pub fn column_at(index: usize) -> Option<Column> {
    COLUMN_ORDER.get(index).copied()
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

    fn board(name: &str, project: &str) -> BoardConfig {
        BoardConfig {
            name: name.to_string(),
            provider: ProviderKind::Native,
            project: project.to_string(),
            agent: None,
            max_running: 1,
            prompt_template: None,
            plane: None,
            github: None,
            jira: None,
            columns: HashMap::new(),
        }
    }

    fn cfg_with_board() -> Config {
        let mut cfg = Config::default();
        cfg.dispatch.max_running = 2;
        cfg.projects.push(crate::config::Project {
            name: "proj".to_string(),
            path: "/tmp/p".to_string(),
            tasks: vec![
                card_task("Fix bug", "fix/bug", Column::Backlog),
                card_task("Ship", "feat/x", Column::Running),
            ],
            copy_patterns: vec![],
            setup_commands: vec![],
            run_command: None,
        });
        cfg.boards.push(board("b", "proj"));
        cfg
    }

    #[test]
    fn card_session_matches_project_and_id() {
        let mut sessions = HashMap::new();
        sessions.insert("s1".to_string(), session("proj", Some("fix/bug")));
        sessions.insert("s2".to_string(), session("other", Some("fix/bug")));
        assert_eq!(
            card_session(&sessions, "proj", &CardId("fix/bug".to_string())),
            Some("s1")
        );
        assert_eq!(
            card_session(&sessions, "proj", &CardId("nope".to_string())),
            None
        );
    }

    #[test]
    fn board_dto_shapes_cards_into_columns() {
        let mut cfg = cfg_with_board();
        let mut sessions = HashMap::new();
        sessions.insert("s1".to_string(), session("proj", Some("fix/bug")));

        let boards = boards_json(&mut cfg, &sessions).unwrap();
        assert_eq!(boards.len(), 1);
        let b = &boards[0];
        assert_eq!(b["name"], "b");
        assert_eq!(b["project"], "proj");
        assert_eq!(b["paused"], false);

        let columns = b["columns"].as_array().unwrap();
        assert_eq!(columns.len(), 6);
        // Column order: backlog, ready, running, needs_you, review, done.
        assert_eq!(columns[0]["column"], "backlog");
        assert_eq!(columns[2]["column"], "running");

        let backlog_cards = columns[0]["cards"].as_array().unwrap();
        assert_eq!(backlog_cards.len(), 1);
        assert_eq!(backlog_cards[0]["id"], "fix/bug");
        assert_eq!(backlog_cards[0]["column"], "backlog");
        assert_eq!(backlog_cards[0]["session"], "s1");

        // A card with no session record reports null.
        let running_cards = columns[2]["cards"].as_array().unwrap();
        assert_eq!(running_cards.len(), 1);
        assert_eq!(running_cards[0]["session"], Value::Null);

        // Empty columns still appear with an empty cards array.
        assert_eq!(columns[1]["cards"].as_array().unwrap().len(), 0);
    }

    #[test]
    fn board_dto_skips_non_native_boards() {
        let mut cfg = Config::default();
        cfg.boards.push(BoardConfig {
            name: "remote".to_string(),
            provider: ProviderKind::Github,
            project: "proj".to_string(),
            agent: None,
            max_running: 1,
            prompt_template: None,
            plane: None,
            github: Some(crate::tasks::GithubConfig {
                owner: "o".to_string(),
                project_number: 1,
                status_field: "Status".to_string(),
            }),
            jira: None,
            columns: HashMap::new(),
        });
        assert!(boards_json(&mut cfg, &HashMap::new()).unwrap().is_empty());
    }

    #[test]
    fn column_order_is_canonical() {
        assert_eq!(column_at(0), Some(Column::Backlog));
        assert_eq!(column_at(3), Some(Column::NeedsYou));
        assert_eq!(column_at(5), Some(Column::Done));
        assert_eq!(column_at(6), None);
    }
}
