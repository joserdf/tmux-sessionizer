//! `showrunner board`: a full-screen kanban board over the configured task
//! boards. Cards are grouped into the six canonical columns and rendered side
//! by side; Enter switches the tmux client to the session serving the selected
//! card, `m` moves a card between columns, `D` runs one dispatch pass and `p`
//! toggles the dispatcher pause. The board re-reads config and cards from disk
//! every [`REFRESH`] and on `r`.
//!
//! Rendering and IO live apart: the pure helpers (column keys, selection
//! clamping, card grouping, session lookup) are unit-tested and never touch
//! disk or tmux.

use std::collections::HashMap;
use std::time::{Duration, Instant};

use anyhow::{Result, bail};
use crossterm::event::{self, Event, KeyCode, KeyEventKind, KeyModifiers};
use ratatui::Frame;
use ratatui::layout::{Constraint, Layout};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;

use crate::config::{Config, SessionRecord};
use crate::tasks::dispatch;
use crate::tasks::{Card, CardId, Column, ProviderKind, provider_for};

/// How often the board re-reads config and cards from disk.
const REFRESH: Duration = Duration::from_secs(2);

/// The canonical column order, left to right.
const COLUMNS: [Column; 6] = [
    Column::Backlog,
    Column::Ready,
    Column::Running,
    Column::NeedsYou,
    Column::Review,
    Column::Done,
];

/// Full column label for wide terminals.
fn column_label(col: Column) -> &'static str {
    match col {
        Column::Backlog => "Backlog",
        Column::Ready => "Ready",
        Column::Running => "Running",
        Column::NeedsYou => "Needs you",
        Column::Review => "Review",
        Column::Done => "Done",
    }
}

/// Abbreviated column label for narrow terminals.
fn column_short(col: Column) -> &'static str {
    match col {
        Column::Backlog => "Back",
        Column::Ready => "Ready",
        Column::Running => "Run",
        Column::NeedsYou => "Need",
        Column::Review => "Rev",
        Column::Done => "Done",
    }
}

/// The single key that selects a column in the `m` move prompt.
fn column_key(col: Column) -> char {
    match col {
        Column::Backlog => 'b',
        Column::Ready => 'r',
        Column::Running => 'u',
        Column::NeedsYou => 'n',
        Column::Review => 'v',
        Column::Done => 'd',
    }
}

/// Parse a move-prompt key into a column, or `None` for an unknown key.
fn column_from_key(key: char) -> Option<Column> {
    COLUMNS.iter().copied().find(|&c| column_key(c) == key)
}

/// Clamp a selected card index to a valid position in a column of `len` cards.
fn clamp_card_index(len: usize, sel: usize) -> usize {
    if len == 0 { 0 } else { sel.min(len - 1) }
}

/// Move the column selection by `delta`, clamped to the column range.
fn step_col(col: usize, delta: isize) -> usize {
    if delta > 0 {
        (col + 1).min(COLUMNS.len() - 1)
    } else {
        col.saturating_sub(1)
    }
}

/// Move the card selection by `delta` within a column of `total` cards.
fn step_card(sel: usize, total: usize, delta: isize) -> usize {
    if total == 0 {
        0
    } else if delta > 0 {
        (sel + 1).min(total - 1)
    } else {
        sel.saturating_sub(1)
    }
}

/// The cards in `cards` sitting in `col`, in board order.
fn cards_in_column(cards: &[Card], col: Column) -> Vec<&Card> {
    cards.iter().filter(|c| c.column == col).collect()
}

/// The live tmux session serving a card for a project, if any. Matches the
/// session record whose `board_card` equals the card id and whose project
/// matches the board's project.
fn session_for_card(
    sessions: &HashMap<String, SessionRecord>,
    project: &str,
    id: &CardId,
) -> Option<String> {
    sessions
        .iter()
        .find(|(_, r)| r.project_name == project && r.board_card.as_deref() == Some(&id.0))
        .map(|(name, _)| name.clone())
}

/// One board's cards for display: the board's name and project plus its cards.
struct BoardCards {
    name: String,
    project: String,
    cards: Vec<Card>,
}

/// Load every native board's cards. Non-native boards are skipped; a board
/// whose provider can't be built (e.g. a missing project) is skipped too.
fn native_boards(cfg: &mut Config) -> Vec<BoardCards> {
    let boards = cfg.boards.clone();
    let mut out = Vec::new();
    for board in &boards {
        if board.provider != ProviderKind::Native {
            continue;
        }
        let Ok(provider) = provider_for(cfg, board) else { continue };
        let Ok(cards) = provider.list() else { continue };
        out.push(BoardCards {
            name: board.name.clone(),
            project: board.project.clone(),
            cards,
        });
    }
    out
}

enum Mode {
    Normal,
    /// Awaiting a column key to move the selected card.
    Move,
}

struct BoardView {
    cfg: Config,
    boards: Vec<BoardCards>,
    board_idx: usize,
    col: usize,
    card: usize,
    mode: Mode,
    status: String,
    paused: bool,
    goto: Option<String>,
    quit: bool,
}

impl BoardView {
    fn current_board(&self) -> Option<&BoardCards> {
        self.boards.get(self.board_idx)
    }

    /// Cards in the selected column of the current board.
    fn current_cards(&self) -> Vec<&Card> {
        self.current_board()
            .map(|b| cards_in_column(&b.cards, COLUMNS[self.col]))
            .unwrap_or_default()
    }

    /// The selected card, or `None` when the column is empty.
    fn selected_card(&self) -> Option<Card> {
        self.current_cards().get(self.card).map(|c| (*c).clone())
    }

    /// The selected card and its board's project, or `None` when empty.
    fn selected_target(&self) -> Option<(String, String, Card)> {
        let board = self.current_board()?;
        let card = self.selected_card()?;
        Some((board.name.clone(), board.project.clone(), card))
    }

    /// Keep the card selection inside the current column after a column move.
    fn clamp_card(&mut self) {
        self.card = clamp_card_index(self.current_cards().len(), self.card);
    }

    /// Re-read config, cards and pause state from disk, preserving the
    /// selection as far as the new data allows.
    fn reload(&mut self) {
        match Config::load() {
            Ok(cfg) => {
                let mut cfg = cfg;
                self.paused = dispatch::is_paused();
                self.boards = native_boards(&mut cfg);
                self.cfg = cfg;
                self.board_idx = self.board_idx.min(self.boards.len().saturating_sub(1));
                self.clamp_card();
            }
            Err(e) => self.status = format!("reload failed: {e:#}"),
        }
    }

    fn move_col(&mut self, delta: isize) {
        self.col = step_col(self.col, delta);
        self.clamp_card();
    }

    fn move_card(&mut self, delta: isize) {
        let total = self.current_cards().len();
        self.card = step_card(self.card, total, delta);
    }

    fn next_board(&mut self) {
        if self.boards.len() > 1 {
            self.board_idx = (self.board_idx + 1) % self.boards.len();
            self.col = 0;
            self.card = 0;
        }
    }

    /// `Enter`: switch to the session serving the selected card, or report that
    /// none exists.
    fn enter(&mut self) {
        let Some((_, project, card)) = self.selected_target() else { return };
        let sessions = crate::config::load_sessions();
        match session_for_card(&sessions, &project, &card.id) {
            Some(name) => {
                self.goto = Some(name);
                self.quit = true;
            }
            None => self.status = "no session for this card".into(),
        }
    }

    /// `m` then a column key: move the selected card and persist.
    fn move_selected(&mut self, col: Column) -> Result<()> {
        let Some((board_name, _, card)) = self.selected_target() else {
            return Ok(());
        };
        let Some(board_cfg) = self
            .cfg
            .boards
            .iter()
            .find(|b| b.name == board_name)
            .cloned()
        else {
            return Ok(());
        };
        // Apply to a freshly loaded config so a dispatcher write since our
        // last reload isn't clobbered by saving the stale `self.cfg`.
        let mut result = Ok(());
        Config::modify(|cfg| {
            result = provider_for(cfg, &board_cfg).and_then(|mut p| p.move_to(&card.id, col));
        })?;
        result
    }

    /// `D`: run one real dispatch pass and surface its summary.
    fn dispatch(&mut self) {
        match dispatch::dispatch_once(false) {
            Ok(s) => {
                self.status = if s.dispatched > 0 {
                    format!("dispatched {} card(s)", s.dispatched)
                } else {
                    "dispatch: nothing to do".into()
                };
                self.reload();
            }
            Err(e) => self.status = format!("dispatch failed: {e:#}"),
        }
    }

    /// `p`: flip the dispatcher pause flag.
    fn toggle_pause(&mut self) {
        let paused = dispatch::is_paused();
        match dispatch::set_paused(!paused) {
            Ok(()) => {
                self.paused = !paused;
                self.status = format!(
                    "dispatch {}",
                    if self.paused { "paused" } else { "active" }
                );
            }
            Err(e) => self.status = format!("pause failed: {e:#}"),
        }
    }

    fn key(&mut self, code: KeyCode) {
        match self.mode {
            Mode::Normal => self.key_normal(code),
            Mode::Move => match code {
                KeyCode::Esc => self.mode = Mode::Normal,
                KeyCode::Char(c) => {
                    if let Some(col) = column_from_key(c) {
                        self.mode = Mode::Normal;
                        match self.move_selected(col) {
                            Ok(()) => {
                                self.status = format!("moved to {}", column_label(col));
                                self.reload();
                            }
                            Err(e) => self.status = format!("move failed: {e:#}"),
                        }
                    }
                }
                _ => {}
            },
        }
    }

    fn key_normal(&mut self, code: KeyCode) {
        match code {
            KeyCode::Char('q') | KeyCode::Esc => self.quit = true,
            KeyCode::Char('h') | KeyCode::Left => self.move_col(-1),
            KeyCode::Char('l') | KeyCode::Right => self.move_col(1),
            KeyCode::Char('j') | KeyCode::Down => self.move_card(1),
            KeyCode::Char('k') | KeyCode::Up => self.move_card(-1),
            KeyCode::Tab => self.next_board(),
            KeyCode::Enter => self.enter(),
            KeyCode::Char('m') => self.mode = Mode::Move,
            KeyCode::Char('D') => self.dispatch(),
            KeyCode::Char('p') => self.toggle_pause(),
            KeyCode::Char('r') => self.reload(),
            _ => {}
        }
    }

    fn event_loop(&mut self, terminal: &mut ratatui::DefaultTerminal) -> Result<()> {
        let mut last_reload = Instant::now();
        while !self.quit {
            if last_reload.elapsed() >= REFRESH {
                self.reload();
                last_reload = Instant::now();
            }
            terminal.draw(|f| self.draw(f))?;
            if event::poll(Duration::from_millis(200))?
                && let Event::Key(k) = event::read()?
                && k.kind == KeyEventKind::Press
            {
                if k.modifiers
                    .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT)
                {
                    if k.code == KeyCode::Char('c') {
                        break;
                    }
                    continue;
                }
                self.key(k.code);
            }
        }
        Ok(())
    }

    // ---- Drawing -----------------------------------------------------------

    fn draw(&mut self, f: &mut Frame) {
        let [header, body, footer] = Layout::vertical([
            Constraint::Length(1),
            Constraint::Min(1),
            Constraint::Length(2),
        ])
        .areas(f.area());

        let dim = Style::default().fg(Color::DarkGray);
        let mut head = vec![Span::styled(
            "Board",
            Style::default().add_modifier(Modifier::BOLD),
        )];
        if let Some(board) = self.current_board() {
            head.push(Span::styled(format!("  {}", board.name), Style::default().bold()));
            if self.boards.len() > 1 {
                head.push(Span::styled(
                    format!("  [{}/{}]", self.board_idx + 1, self.boards.len()),
                    dim,
                ));
            }
        }
        let (word, color) = if self.paused {
            ("paused", Color::Yellow)
        } else {
            ("active", Color::Green)
        };
        head.push(Span::styled(format!("  dispatch {word}"), Style::default().fg(color)));
        f.render_widget(Paragraph::new(Line::from(head)), header);

        let cols = Layout::horizontal(vec![Constraint::Ratio(1, COLUMNS.len() as u32); COLUMNS.len()])
            .split(body);
        for (i, col) in COLUMNS.iter().enumerate() {
            let cards = self.current_board().map_or(vec![], |b| cards_in_column(&b.cards, *col));
            let selected = i == self.col;
            self.draw_column(f, cols[i], *col, &cards, selected);
        }

        let hint = if !self.status.is_empty() {
            Line::from(Span::raw(self.status.clone()))
        } else if matches!(self.mode, Mode::Move) {
            Line::from(Span::styled(
                "move to: b backlog  r ready  u running  n needs-you  v review  d done  esc cancel",
                Style::default().fg(Color::Cyan),
            ))
        } else {
            Line::from(Span::styled(
                "h/l columns  j/k cards  enter switch  m move  D dispatch  p pause  r reload  q quit",
                dim,
            ))
        };
        let keys = Line::from(Span::styled("tab next board", dim));
        f.render_widget(Paragraph::new(vec![hint, keys]), footer);
    }

    fn draw_column(&self, f: &mut Frame, area: ratatui::layout::Rect, col: Column, cards: &[&Card], selected: bool) {
        let narrow = area.width < 12;
        let label = if narrow { column_short(col) } else { column_label(col) };
        let title = format!("{label} ({})", cards.len());
        let mut lines = vec![Line::from(Span::styled(
            title,
            Style::default()
                .fg(if selected { Color::Cyan } else { Color::DarkGray })
                .add_modifier(Modifier::BOLD),
        ))];
        for (i, card) in cards.iter().enumerate() {
            let is_sel = selected && i == self.card;
            let mut style = Style::default();
            if is_sel {
                style = style.bg(Color::Rgb(49, 50, 68));
            }
            let text = if narrow {
                card.key.clone()
            } else {
                format!("{}  {}", card.key, card.title)
            };
            lines.push(Line::from(Span::styled(text, style)));
        }
        f.render_widget(Paragraph::new(lines), area);
    }
}

/// Open the full-screen board view. Fails loudly when no boards are configured.
pub fn run() -> Result<()> {
    let mut cfg = Config::load()?;
    let boards = native_boards(&mut cfg);
    if boards.is_empty() {
        bail!("no native boards configured (add a [[boards]] entry to config)");
    }
    let paused = dispatch::is_paused();
    let mut view = BoardView {
        cfg,
        boards,
        board_idx: 0,
        col: 0,
        card: 0,
        mode: Mode::Normal,
        status: String::new(),
        paused,
        goto: None,
        quit: false,
    };
    let mut terminal = ratatui::init();
    let result = view.event_loop(&mut terminal);
    ratatui::restore();
    result?;
    if let Some(name) = view.goto {
        crate::tmux::attach_session(&name)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn card(id: &str, col: Column) -> Card {
        Card {
            id: CardId(id.to_string()),
            key: id.to_string(),
            title: id.to_string(),
            body: String::new(),
            column: col,
            labels: vec![],
            priority: None,
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
    fn column_keys_round_trip() {
        for col in COLUMNS {
            assert_eq!(column_from_key(column_key(col)), Some(col));
        }
        assert_eq!(column_from_key('x'), None);
        assert_eq!(column_from_key(' '), None);
    }

    #[test]
    fn selection_clamps_to_column_range() {
        assert_eq!(clamp_card_index(0, 0), 0);
        assert_eq!(clamp_card_index(3, 5), 2);
        assert_eq!(clamp_card_index(3, 1), 1);
        assert_eq!(step_col(0, -1), 0);
        assert_eq!(step_col(COLUMNS.len() - 1, 1), COLUMNS.len() - 1);
        assert_eq!(step_col(1, 1), 2);
        assert_eq!(step_card(0, 0, 1), 0);
        assert_eq!(step_card(0, 4, 1), 1);
        assert_eq!(step_card(3, 4, 1), 3);
        assert_eq!(step_card(0, 4, -1), 0);
        assert_eq!(step_card(2, 4, -1), 1);
    }

    #[test]
    fn groups_cards_by_column() {
        let cards = vec![
            card("a", Column::Ready),
            card("b", Column::Running),
            card("c", Column::Ready),
        ];
        assert_eq!(cards_in_column(&cards, Column::Ready).len(), 2);
        assert_eq!(cards_in_column(&cards, Column::Running).len(), 1);
        assert!(cards_in_column(&cards, Column::Done).is_empty());
    }

    #[test]
    fn finds_session_serving_card_by_project() {
        let mut sessions = HashMap::new();
        sessions.insert("s1".to_string(), session("proj", Some("fix/bug")));
        sessions.insert("s2".to_string(), session("other", Some("fix/bug")));
        assert_eq!(
            session_for_card(&sessions, "proj", &CardId("fix/bug".to_string())),
            Some("s1".to_string())
        );
        // Wrong project or wrong card id: no match.
        assert_eq!(
            session_for_card(&sessions, "nope", &CardId("fix/bug".to_string())),
            None
        );
        assert_eq!(
            session_for_card(&sessions, "proj", &CardId("other".to_string())),
            None
        );
    }

    #[test]
    fn native_boards_collects_only_cards_with_columns() {
        use crate::config::{Project, Task};
        let task = |name: &str, col: Column| Task {
            name: name.to_string(),
            branch: name.to_string(),
            base_branch: None,
            archived: false,
            column: Some(col),
            body: None,
            notes: vec![],
            links: vec![],
        };
        let mut cfg = Config::default();
        cfg.projects.push(Project {
            name: "proj".to_string(),
            path: "/tmp/p".to_string(),
            tasks: vec![task("a", Column::Ready), task("b", Column::Backlog)],
            copy_patterns: vec![],
            setup_commands: vec![],
            run_command: None,
        });
        cfg.boards.push(crate::tasks::BoardConfig {
            name: "b".to_string(),
            provider: ProviderKind::Native,
            project: "proj".to_string(),
            agent: None,
            max_running: 1,
            prompt_template: None,
            plane: None,
            github: None,
            jira: None,
            columns: HashMap::new(),
        });
        let boards = native_boards(&mut cfg);
        assert_eq!(boards.len(), 1);
        assert_eq!(boards[0].name, "b");
        assert_eq!(boards[0].cards.len(), 2);
    }
}
