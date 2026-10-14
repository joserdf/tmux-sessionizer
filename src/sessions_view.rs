//! `showrunner sessions`: a light popup listing every tmux session and
//! window by category, with each agent's status, acting on them through
//! [`control`] (the same code the MCP tools run). Panes and the daemon's
//! agent states are re-read once a second on a background thread, so the
//! first frame never waits on tmux or the daemon.

use std::sync::Arc;
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::thread;
use std::time::Duration;

use anyhow::Result;
use crossterm::event::{self, Event, KeyCode, KeyEventKind, KeyModifiers};
use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, List, ListItem, ListState, Paragraph};
use serde_json::Value;

use crate::config::Config;
use crate::control::{self, Inventory};
use crate::inventory::AgentStatus;
use crate::snapshot::{self, ClosedEntry};

/// Width from which the selected window's output is shown on the right.
const PREVIEW_MIN_WIDTH: u16 = 140;
const REFRESH: Duration = Duration::from_secs(1);

pub fn run() -> Result<()> {
    let (tx, rx) = mpsc::channel();
    let (wake, woken) = mpsc::channel();
    let refresh_tx = tx.clone();
    thread::spawn(move || refresher(refresh_tx, woken));

    let mut view = View::new(tx, wake);
    let mut terminal = ratatui::init();
    let result = view.event_loop(&mut terminal, &rx);
    ratatui::restore();
    result?;

    if let Some((session, index)) = view.goto {
        crate::tmux::attach_session_window(&session, index as usize)?;
    } else if view.projects {
        std::process::Command::new(std::env::current_exe()?).status()?;
    }
    Ok(())
}

enum Msg {
    Inventory(Inventory),
    /// An action finished: its outcome for the status line.
    Done(String),
}

fn refresher(tx: Sender<Msg>, woken: Receiver<()>) {
    let Ok(rt) = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
    else {
        return;
    };
    loop {
        let states = rt.block_on(crate::mcp::daemon_states());
        if tx.send(Msg::Inventory(Inventory::load(states))).is_err() {
            return;
        }
        if let Err(RecvTimeoutError::Disconnected) = woken.recv_timeout(REFRESH) {
            return;
        }
    }
}

// ---- Rows ------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq)]
struct WinRow {
    session: String,
    window_id: String,
    index: u32,
    pane: String,
    name: String,
    agent: Option<String>,
    status: Option<AgentStatus>,
    idle: u64,
}

impl WinRow {
    fn target(&self) -> String {
        format!("{}:{}", self.session, self.index)
    }
}

#[derive(Debug, Clone, PartialEq)]
enum Row {
    Category(String),
    Session {
        name: String,
        category: String,
        attached: bool,
        active_index: u32,
    },
    Window(WinRow),
}

impl Row {
    /// What identifies the row across refreshes.
    fn key(&self) -> Option<String> {
        match self {
            Row::Category(_) => None,
            Row::Session { name, .. } => Some(format!("={name}")),
            Row::Window(w) => Some(w.window_id.clone()),
        }
    }
}

fn needs_attention(status: Option<AgentStatus>) -> bool {
    matches!(
        status,
        Some(
            AgentStatus::Question
                | AgentStatus::Permission
                | AgentStatus::Done
                | AgentStatus::Error
        )
    )
}

/// Category ▸ session ▸ window rows: named categories alphabetically, then
/// uncategorized sessions; sessions and windows in tmux order. `filter`
/// keeps windows whose session, name, category, agent or cwd contains it;
/// `attention` keeps only windows that need the user.
fn build_rows(inv: &Inventory, now: u64, filter: &str, attention: bool) -> Vec<Row> {
    let needle = filter.trim().to_lowercase();
    let windows = inv.windows();
    let mut sessions: Vec<&str> = Vec::new();
    for p in &windows {
        if !sessions.contains(&p.session.as_str()) {
            sessions.push(&p.session);
        }
    }
    let category_of = |s: &str| {
        windows
            .iter()
            .find(|p| p.session == s)
            .map_or("", |p| p.category.as_str())
    };
    let mut categories: Vec<&str> = sessions.iter().map(|s| category_of(s)).collect();
    categories.sort_by_key(|c| (c.is_empty(), c.to_lowercase()));
    categories.dedup();

    let mut rows = Vec::new();
    for category in categories {
        let mut group = Vec::new();
        for &session in sessions.iter().filter(|s| category_of(s) == category) {
            let kept: Vec<WinRow> = windows
                .iter()
                .filter(|p| p.session == session)
                .map(|p| WinRow {
                    session: p.session.clone(),
                    window_id: p.window_id.clone(),
                    index: p.window_index,
                    pane: p.id.clone(),
                    name: p.window_name.clone(),
                    agent: inv.state(p).and_then(|s| s.agent.clone()),
                    status: inv.window_status(&p.window_id),
                    idle: now.saturating_sub(p.activity),
                })
                .filter(|w| !attention || needs_attention(w.status))
                .filter(|w| {
                    needle.is_empty()
                        || [
                            w.session.as_str(),
                            w.name.as_str(),
                            category,
                            w.agent.as_deref().unwrap_or(""),
                            inv.panes
                                .iter()
                                .find(|p| p.id == w.pane)
                                .map_or("", |p| p.cwd.as_str()),
                        ]
                        .iter()
                        .any(|f| f.to_lowercase().contains(&needle))
                })
                .collect();
            if kept.is_empty() {
                continue;
            }
            let first = windows.iter().find(|p| p.session == session).unwrap();
            let active_index = windows
                .iter()
                .find(|p| p.session == session && p.window_active)
                .map_or(first.window_index, |p| p.window_index);
            group.push(Row::Session {
                name: session.to_string(),
                category: category.to_string(),
                attached: first.attached,
                active_index,
            });
            group.extend(kept.into_iter().map(Row::Window));
        }
        if !group.is_empty() {
            let label = if category.is_empty() {
                "uncategorized"
            } else {
                category
            };
            rows.push(Row::Category(label.to_string()));
            rows.extend(group);
        }
    }
    rows
}

fn ago(secs: u64) -> String {
    match secs {
        s if s < 60 => "now".into(),
        s if s < 3600 => format!("{}m", s / 60),
        s if s < 86400 => format!("{}h", s / 3600),
        s => format!("{}d", s / 86400),
    }
}

fn status_span(status: Option<AgentStatus>) -> Span<'static> {
    let (glyph, color) = match status {
        None => return Span::raw(format!("{:<STATUS_W$}", "")),
        Some(AgentStatus::Working) => ("●", Color::Yellow),
        Some(AgentStatus::Question) => ("?", Color::Magenta),
        Some(AgentStatus::Permission) => ("!", Color::Red),
        Some(AgentStatus::Done) => ("✓", Color::Green),
        Some(AgentStatus::Idle) => ("○", Color::DarkGray),
        Some(AgentStatus::Error) => ("✗", Color::Red),
    };
    let s = status.unwrap();
    let mut style = Style::default().fg(color);
    // Done means "finished, not seen yet".
    if s == AgentStatus::Done {
        style = style.add_modifier(Modifier::BOLD);
    }
    Span::styled(
        format!("{:<STATUS_W$}", format!("{glyph} {}", s.code())),
        style,
    )
}

/// Columns of the status cell: glyph, space, the longest code, a gap.
const STATUS_W: usize = 14;

fn draw_closed(f: &mut Frame, area: Rect, entries: &[ClosedEntry], sel: usize) {
    let now = control::now();
    let items: Vec<ListItem> = entries
        .iter()
        .map(|e| {
            let windows: Vec<&str> = e.session.windows.iter().map(|w| w.name.as_str()).collect();
            ListItem::new(format!(
                "{:>4}  {}  {}",
                ago(now.saturating_sub(e.closed_at)),
                e.session.name,
                windows.join(", ")
            ))
        })
        .collect();
    let mut state = ListState::default().with_selected(Some(sel));
    f.render_stateful_widget(
        List::new(items)
            .block(Block::default().title("Recently closed"))
            .highlight_style(Style::default().bg(Color::Rgb(49, 50, 68))),
        area,
        &mut state,
    );
}

// ---- View ------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq)]
enum Input {
    Reply,
    Category,
    Rename,
}

enum Mode {
    Normal,
    Filter,
    Input(Input, String),
    /// Close this target (session or `session:index`)?
    Confirm(String),
    /// The recently closed list and its selection.
    Closed(Vec<ClosedEntry>, usize),
}

struct View {
    inv: Option<Arc<Inventory>>,
    rows: Vec<Row>,
    sel: usize,
    filter: String,
    attention: bool,
    mode: Mode,
    status: String,
    /// The selected window's pane and its captured tail.
    preview: Option<(String, String)>,
    show_preview: bool,
    tx: Sender<Msg>,
    wake: Sender<()>,
    goto: Option<(String, u32)>,
    projects: bool,
    quit: bool,
}

impl View {
    fn new(tx: Sender<Msg>, wake: Sender<()>) -> Self {
        Self {
            inv: None,
            rows: Vec::new(),
            sel: 0,
            filter: String::new(),
            attention: false,
            mode: Mode::Normal,
            status: String::new(),
            preview: None,
            show_preview: false,
            tx,
            wake,
            goto: None,
            projects: false,
            quit: false,
        }
    }

    fn event_loop(
        &mut self,
        terminal: &mut ratatui::DefaultTerminal,
        rx: &Receiver<Msg>,
    ) -> Result<()> {
        while !self.quit {
            while let Ok(msg) = rx.try_recv() {
                match msg {
                    Msg::Inventory(inv) => {
                        self.inv = Some(Arc::new(inv));
                        self.rebuild();
                        self.preview = None;
                    }
                    Msg::Done(s) => self.status = s,
                }
            }
            if self.show_preview {
                self.refresh_preview();
            }
            terminal.draw(|f| self.draw(f))?;
            if event::poll(Duration::from_millis(200))?
                && let Event::Key(k) = event::read()?
                && k.kind == KeyEventKind::Press
            {
                // No Ctrl/Alt bindings: keep them out of the text inputs.
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

    fn rebuild(&mut self) {
        let Some(inv) = &self.inv else { return };
        let key = self.rows.get(self.sel).and_then(Row::key);
        self.rows = build_rows(inv, control::now(), &self.filter, self.attention);
        self.sel = key
            .and_then(|k| self.rows.iter().position(|r| r.key().as_ref() == Some(&k)))
            .unwrap_or(self.sel.min(self.rows.len().saturating_sub(1)));
        if matches!(self.rows.get(self.sel), Some(Row::Category(_))) {
            self.step(1);
        }
    }

    /// Move the selection by `delta` rows, skipping category headers.
    fn step(&mut self, delta: isize) {
        let mut i = self.sel as isize;
        loop {
            i += delta;
            match self.rows.get(i as usize) {
                _ if i < 0 => return,
                None => return,
                Some(Row::Category(_)) => continue,
                Some(_) => {
                    self.sel = i as usize;
                    return;
                }
            }
        }
    }

    fn selected(&self) -> Option<&Row> {
        self.rows
            .get(self.sel)
            .filter(|r| !matches!(r, Row::Category(_)))
    }

    fn selected_window(&self) -> Option<&WinRow> {
        match self.selected()? {
            Row::Window(w) => Some(w),
            _ => None,
        }
    }

    /// The control target of the selection: a session or `session:index`.
    fn target(&self) -> Option<String> {
        match self.selected()? {
            Row::Session { name, .. } => Some(name.clone()),
            Row::Window(w) => Some(w.target()),
            Row::Category(_) => None,
        }
    }

    fn refresh_preview(&mut self) {
        let Some(pane) = self.selected_window().map(|w| w.pane.clone()) else {
            self.preview = None;
            return;
        };
        if self.preview.as_ref().is_some_and(|(p, _)| *p == pane) {
            return;
        }
        let text = control::capture(&pane, 200).unwrap_or_default();
        // A pane is captured to its full height; show its last output.
        let text = text.trim_end().to_string();
        self.preview = Some((pane, text));
    }

    /// Run `f` on a fresh thread over the current inventory; its outcome
    /// lands on the status line and triggers a refresh.
    fn act<F>(&mut self, what: &str, f: F)
    where
        F: FnOnce(&Inventory, &Config) -> Result<Value> + Send + 'static,
    {
        let Some(inv) = self.inv.clone() else { return };
        self.status = format!("{what}…");
        let (tx, wake, what) = (self.tx.clone(), self.wake.clone(), what.to_string());
        thread::spawn(move || {
            let r = Config::load().and_then(|cfg| f(&inv, &cfg));
            let msg = match r {
                Ok(_) => format!("{what}: done"),
                Err(e) => format!("{what}: {e:#}"),
            };
            let _ = tx.send(Msg::Done(msg));
            let _ = wake.send(());
        });
    }

    fn key(&mut self, code: KeyCode) {
        match std::mem::replace(&mut self.mode, Mode::Normal) {
            Mode::Normal => self.key_normal(code),
            Mode::Filter => match code {
                KeyCode::Enter => {}
                KeyCode::Esc => {
                    self.filter.clear();
                    self.rebuild();
                }
                KeyCode::Backspace => {
                    self.filter.pop();
                    self.rebuild();
                    self.mode = Mode::Filter;
                }
                KeyCode::Char(c) => {
                    self.filter.push(c);
                    self.rebuild();
                    self.mode = Mode::Filter;
                }
                _ => self.mode = Mode::Filter,
            },
            Mode::Input(kind, mut buf) => match code {
                KeyCode::Enter => self.submit(kind, buf),
                KeyCode::Esc => {}
                KeyCode::Backspace => {
                    buf.pop();
                    self.mode = Mode::Input(kind, buf);
                }
                KeyCode::Char(c) => {
                    buf.push(c);
                    self.mode = Mode::Input(kind, buf);
                }
                _ => self.mode = Mode::Input(kind, buf),
            },
            Mode::Confirm(target) => {
                if code == KeyCode::Char('y') {
                    let label = format!("close {target}");
                    self.act(&label, move |inv, cfg| {
                        inv.close(cfg, &target, true, control::now())
                    });
                } else {
                    self.status = "close cancelled".into();
                }
            }
            Mode::Closed(entries, mut i) => match code {
                KeyCode::Char('j') | KeyCode::Down => {
                    i = (i + 1).min(entries.len().saturating_sub(1));
                    self.mode = Mode::Closed(entries, i);
                }
                KeyCode::Char('k') | KeyCode::Up => {
                    self.mode = Mode::Closed(entries, i.saturating_sub(1));
                }
                KeyCode::Enter => {
                    if let Some(e) = entries.get(i) {
                        let id = e.id;
                        self.act(&format!("reopen {}", e.session.name), move |_, cfg| {
                            control::reopen(cfg, id)
                        });
                    }
                }
                KeyCode::Esc | KeyCode::Char('q') | KeyCode::Char('u') => {}
                _ => self.mode = Mode::Closed(entries, i),
            },
        }
    }

    fn key_normal(&mut self, code: KeyCode) {
        match code {
            KeyCode::Char('q') | KeyCode::Esc => self.quit = true,
            KeyCode::Char('j') | KeyCode::Down => self.step(1),
            KeyCode::Char('k') | KeyCode::Up => self.step(-1),
            KeyCode::Enter => {
                self.goto = match self.selected() {
                    Some(Row::Window(w)) => Some((w.session.clone(), w.index)),
                    Some(Row::Session {
                        name, active_index, ..
                    }) => Some((name.clone(), *active_index)),
                    _ => None,
                };
                self.quit = self.goto.is_some();
            }
            KeyCode::Char('/') => self.mode = Mode::Filter,
            KeyCode::Char('a') => {
                self.attention = !self.attention;
                self.rebuild();
            }
            KeyCode::Char('n') => self.next_attention(),
            KeyCode::Char('p') => {
                self.projects = true;
                self.quit = true;
            }
            KeyCode::Char('u') => {
                let mut entries = snapshot::load_closed();
                // Newest first, as `showrunner closed list` shows them.
                entries.reverse();
                if entries.is_empty() {
                    self.status = "nothing closed recently".into();
                } else {
                    self.mode = Mode::Closed(entries, 0);
                }
            }
            KeyCode::Char('r') if self.selected_window().is_some() => {
                self.mode = Mode::Input(Input::Reply, String::new());
            }
            KeyCode::Char('c') => {
                if let Some(Row::Session { category, .. }) = self.session_row() {
                    let current = category.clone();
                    self.mode = Mode::Input(Input::Category, current);
                }
            }
            KeyCode::Char('R') => {
                let current = match self.selected() {
                    Some(Row::Session { name, .. }) => name.clone(),
                    Some(Row::Window(w)) => w.name.clone(),
                    _ => return,
                };
                self.mode = Mode::Input(Input::Rename, current);
            }
            KeyCode::Char('y') => {
                if let Some(t) = self.selected_window().map(WinRow::target) {
                    self.act(&format!("approve {t}"), move |inv, _| {
                        inv.approve(&t, "yes")
                    });
                }
            }
            KeyCode::Char('s') => {
                if let Some(t) = self.selected_window().map(WinRow::target) {
                    self.act(&format!("suspend {t}"), move |inv, cfg| {
                        inv.suspend(cfg, &t, false)
                    });
                }
            }
            KeyCode::Char('x') => {
                if let Some(t) = self.target() {
                    self.mode = Mode::Confirm(t);
                }
            }
            _ => {}
        }
    }

    /// The session row the selection belongs to.
    fn session_row(&self) -> Option<&Row> {
        self.rows[..=self.sel.min(self.rows.len().checked_sub(1)?)]
            .iter()
            .rev()
            .find(|r| matches!(r, Row::Session { .. }))
    }

    fn next_attention(&mut self) {
        let n = self.rows.len();
        let hit = (1..=n)
            .map(|d| (self.sel + d) % n)
            .find(|&i| matches!(&self.rows[i], Row::Window(w) if needs_attention(w.status)));
        match hit {
            Some(i) => self.sel = i,
            None => self.status = "nothing needs you".into(),
        }
    }

    fn submit(&mut self, kind: Input, text: String) {
        let Some(target) = self.target() else { return };
        match kind {
            Input::Reply if !text.trim().is_empty() => {
                self.act(&format!("reply to {target}"), move |inv, _| {
                    inv.send_message(&target, &text, true, true)
                });
            }
            Input::Category => {
                let session = target.split(':').next().unwrap_or(&target).to_string();
                self.act(&format!("category of {session}"), move |inv, _| {
                    inv.set_category(&session, Some(text.trim()))
                });
            }
            Input::Rename if !text.trim().is_empty() => {
                self.act(&format!("rename {target}"), move |inv, _| {
                    inv.rename(&target, text.trim())
                });
            }
            _ => {}
        }
    }

    // ---- Drawing -----------------------------------------------------------

    fn draw(&mut self, f: &mut Frame) {
        let [header, body, footer] = Layout::vertical([
            Constraint::Length(1),
            Constraint::Min(1),
            Constraint::Length(2),
        ])
        .areas(f.area());
        self.show_preview = body.width > PREVIEW_MIN_WIDTH;

        let dim = Style::default().fg(Color::DarkGray);
        let mut head = vec![Span::styled(
            "Sessions",
            Style::default().add_modifier(Modifier::BOLD),
        )];
        match &self.inv {
            None => head.push(Span::styled("  loading…", dim)),
            Some(inv) => {
                let n = inv
                    .windows()
                    .iter()
                    .filter(|p| needs_attention(inv.window_status(&p.window_id)))
                    .count();
                if n > 0 {
                    head.push(Span::styled(
                        format!("  {n} need you"),
                        Style::default().fg(Color::Magenta),
                    ));
                }
                if !inv.live {
                    head.push(Span::styled("  daemon not running: no agent status", dim));
                }
            }
        }
        if self.attention {
            head.push(Span::styled("  [attention only]", dim));
        }
        if !self.filter.is_empty() || matches!(self.mode, Mode::Filter) {
            head.push(Span::styled(format!("  /{}", self.filter), dim));
        }
        f.render_widget(Paragraph::new(Line::from(head)), header);

        let (list_area, preview_area) = if self.show_preview {
            let [l, r] =
                Layout::horizontal([Constraint::Percentage(50), Constraint::Percentage(50)])
                    .areas(body);
            (l, Some(r))
        } else {
            (body, None)
        };
        match &self.mode {
            Mode::Closed(entries, i) => draw_closed(f, list_area, entries, *i),
            _ => self.draw_rows(f, list_area),
        }
        if let Some(area) = preview_area {
            let text = self.preview.as_ref().map_or("", |(_, t)| t.as_str());
            let height = area.height.saturating_sub(1) as usize;
            let lines: Vec<&str> = text.lines().collect();
            let tail = lines[lines.len().saturating_sub(height)..].join("\n");
            f.render_widget(
                Paragraph::new(tail)
                    .block(Block::default().borders(Borders::LEFT).border_style(dim)),
                area,
            );
        }

        let hint = match &self.mode {
            Mode::Normal | Mode::Filter if !self.status.is_empty() => {
                Line::from(Span::raw(self.status.clone()))
            }
            Mode::Normal => Line::from(Span::styled(
                "enter go  / filter  a attention  n next  r reply  y approve  s suspend",
                dim,
            )),
            Mode::Filter => Line::from(Span::styled("type to filter  enter keep  esc clear", dim)),
            Mode::Input(kind, buf) => {
                let label = match kind {
                    Input::Reply => "reply",
                    Input::Category => "category (empty clears)",
                    Input::Rename => "rename",
                };
                Line::from(vec![
                    Span::styled(format!("{label}: "), Style::default().fg(Color::Cyan)),
                    Span::raw(format!("{buf}▏")),
                ])
            }
            Mode::Confirm(t) => Line::from(Span::styled(
                format!("close {t}? it can be reopened with u  (y/n)"),
                Style::default().fg(Color::Yellow),
            )),
            Mode::Closed(..) => Line::from(Span::styled("enter reopen  esc back", dim)),
        };
        let keys = Line::from(Span::styled(
            "c category  R rename  x close  u reopen  p projects  q quit",
            dim,
        ));
        f.render_widget(Paragraph::new(vec![hint, keys]), footer);
    }

    fn draw_rows(&self, f: &mut Frame, area: Rect) {
        let dim = Style::default().fg(Color::DarkGray);
        let name_w = self
            .rows
            .iter()
            .filter_map(|r| match r {
                Row::Window(w) => Some(w.name.chars().count()),
                _ => None,
            })
            .max()
            .unwrap_or(0)
            .clamp(8, 28);
        let items: Vec<ListItem> = self
            .rows
            .iter()
            .map(|r| match r {
                Row::Category(c) => ListItem::new(Line::from(Span::styled(
                    c.clone(),
                    Style::default()
                        .fg(Color::Cyan)
                        .add_modifier(Modifier::BOLD),
                ))),
                Row::Session { name, attached, .. } => {
                    let mut spans = vec![Span::styled(
                        format!("  {name}"),
                        Style::default().add_modifier(Modifier::BOLD),
                    )];
                    if *attached {
                        spans.push(Span::styled("  attached", dim));
                    }
                    ListItem::new(Line::from(spans))
                }
                Row::Window(w) => {
                    let name: String = w.name.chars().take(name_w).collect();
                    ListItem::new(Line::from(vec![
                        Span::styled(format!("    {:>2} ", w.index), dim),
                        Span::raw(format!("{name:<name_w$}  ")),
                        Span::styled(format!("{:<9}", w.agent.as_deref().unwrap_or("-")), dim),
                        status_span(w.status),
                        Span::styled(format!("{:>4}", ago(w.idle)), dim),
                    ]))
                }
            })
            .collect();
        if items.is_empty() {
            let msg = if self.inv.is_none() {
                "loading…"
            } else {
                "no sessions match"
            };
            f.render_widget(Paragraph::new(Span::styled(msg, dim)), area);
            return;
        }
        let mut state = ListState::default().with_selected(Some(self.sel));
        f.render_stateful_widget(
            List::new(items).highlight_style(Style::default().bg(Color::Rgb(49, 50, 68))),
            area,
            &mut state,
        );
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use super::*;
    use crate::inventory::{self, Pane, PaneState};

    fn pane(id: &str, session: &str, window: &str, index: u32, name: &str) -> Pane {
        inventory::parse_panes(&format!(
            "{id}\t{session}\t{window}\t{index}\t{name}\t0\t1\t1\t0\t0\t0\tzsh\t/tmp/{session}\t1\t\t\t\tL\t\t\t",
        ))
        .remove(0)
    }

    fn inv() -> Inventory {
        let mut panes = vec![
            pane("%1", "main", "@1", 0, "btop"),
            pane("%2", "gates", "@2", 0, "gates"),
            pane("%3", "gates", "@3", 1, "slides"),
            pane("%4", "ga-009", "@4", 0, "pesquisa"),
        ];
        for (p, c) in panes.iter_mut().zip(["", "projects", "projects", "study"]) {
            p.category = c.into();
            p.activity = 1000;
        }
        let mut states = HashMap::new();
        for (id, status) in [("%2", AgentStatus::Question), ("%3", AgentStatus::Idle)] {
            let mut s = PaneState::default();
            s.agent = Some("claude".into());
            s.status = Some(status);
            states.insert(id.to_string(), s);
        }
        Inventory::new(panes, Some(states))
    }

    fn labels(rows: &[Row]) -> Vec<String> {
        rows.iter()
            .map(|r| match r {
                Row::Category(c) => format!("# {c}"),
                Row::Session { name, .. } => format!("s {name}"),
                Row::Window(w) => format!("w {}", w.target()),
            })
            .collect()
    }

    #[test]
    fn rows_group_by_category_uncategorized_last() {
        let rows = build_rows(&inv(), 1000, "", false);
        assert_eq!(
            labels(&rows),
            [
                "# projects",
                "s gates",
                "w gates:0",
                "w gates:1",
                "# study",
                "s ga-009",
                "w ga-009:0",
                "# uncategorized",
                "s main",
                "w main:0",
            ]
        );
        let Row::Window(w) = &rows[2] else { panic!() };
        assert_eq!(w.status, Some(AgentStatus::Question));
        assert_eq!(w.agent.as_deref(), Some("claude"));
    }

    #[test]
    fn filter_and_attention_narrow_rows() {
        let inv = inv();
        assert_eq!(
            labels(&build_rows(&inv, 1000, "SLIDE", false)),
            ["# projects", "s gates", "w gates:1"]
        );
        // The category and the cwd match too.
        assert_eq!(build_rows(&inv, 1000, "study", false).len(), 3);
        assert_eq!(build_rows(&inv, 1000, "/tmp/main", false).len(), 3);
        assert_eq!(
            labels(&build_rows(&inv, 1000, "", true)),
            ["# projects", "s gates", "w gates:0"]
        );
        assert!(build_rows(&inv, 1000, "nothing", false).is_empty());
    }

    #[test]
    fn navigation_skips_headers_and_finds_attention() {
        let (tx, _rx) = mpsc::channel();
        let (wake, _woken) = mpsc::channel();
        let mut v = View::new(tx, wake);
        v.inv = Some(Arc::new(inv()));
        v.rebuild();
        assert_eq!(v.target().as_deref(), Some("gates"));
        v.step(1);
        v.step(1);
        v.step(1);
        assert_eq!(v.target().as_deref(), Some("ga-009"));
        v.step(-1);
        v.step(-1);
        assert_eq!(v.target().as_deref(), Some("gates:0"));
        v.next_attention();
        assert_eq!(
            v.target().as_deref(),
            Some("gates:0"),
            "wraps to the only one"
        );
        v.step(-1);
        v.step(-1);
        assert_eq!(v.target().as_deref(), Some("gates"), "stops at the top");
        // The selection follows its window across a rebuild that shifts rows.
        v.step(1);
        v.step(1);
        v.filter = "slides".into();
        v.rebuild();
        assert_eq!(v.target().as_deref(), Some("gates:1"));
    }

    #[test]
    fn ago_units() {
        assert_eq!(ago(5), "now");
        assert_eq!(ago(300), "5m");
        assert_eq!(ago(7200), "2h");
        assert_eq!(ago(3 * 86400), "3d");
    }
}
