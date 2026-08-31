use std::collections::HashMap;

use ratatui::Frame;
use ratatui::layout::{Alignment, Constraint, Layout, Margin, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{
    Block, BorderType, Borders, Clear, List, ListItem, ListState, Paragraph, Wrap,
};

use crate::app::{self, App, InputMode};
use crate::theme::current;
use crate::tmux::{self, SessionStatus};

const PAD_LEFT: u16 = 1;
const PAD_TOP: u16 = 1;

/// Master-detail split: the project/task/session tree is the sidebar (master),
/// and the selected session's output + diff fill the detail pane on the right.
const SIDEBAR_PCT: u16 = 40;

const SPINNER: &[&str] = &["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];

/// Inline "Run" indicator for a list item, when a run session is live: an
/// animated green spinner while the command is executing, a static green marker
/// once it has finished (shell still open). `None` when there's no run session.
fn run_indicator(app: &App, item: &app::ListItem) -> Option<Span<'static>> {
    match app.run_state_for(item) {
        Some(true) => {
            let frame = SPINNER[app.tick % SPINNER.len()];
            Some(Span::styled(
                format!("  {frame}"),
                Style::default()
                    .fg(current().green)
                    .add_modifier(Modifier::BOLD),
            ))
        }
        Some(false) => Some(Span::styled(
            "  ▷".to_string(),
            Style::default().fg(current().green),
        )),
        None => None,
    }
}

/// Red "✗" error attention for a session row, shown when the detail pane (the
/// selected session's fetched output) looks like an error. `None` otherwise, so
/// unselected sessions — whose output isn't fetched — carry no badge.
fn detail_error_span(app: &App, name: &str) -> Option<Span<'static>> {
    let has_error = app
        .detail
        .as_ref()
        .map_or(false, |d| d.session == name && d.has_error);
    has_error.then(|| {
        Span::styled(
            "  ✗",
            Style::default()
                .fg(current().red)
                .add_modifier(Modifier::BOLD),
        )
    })
}

/// Status icon + colour for a session, used both inline and for the status rail.

/// Dim glyph identifying the harness a session runs (✻ claude, ⬡ codex, π pi).
/// Nothing until the worker has resolved the session's agent.
fn agent_icon_span(app: &App, tmux_name: &str) -> Option<Span<'static>> {
    let agent = app
        .session_agents
        .get(tmux_name)
        .and_then(|id| crate::agent::AgentKind::from_id(id))?;
    Some(Span::styled(
        format!(" {}", agent.icon()),
        Style::default().fg(current().muted),
    ))
}

fn status_glyph(status: SessionStatus, tick: usize) -> (&'static str, Color) {
    match status {
        SessionStatus::Running => (SPINNER[tick % SPINNER.len()], current().yellow),
        SessionStatus::WaitingForInput => ("●", current().green),
        SessionStatus::WaitingForPermission => ("!", current().magenta),
        SessionStatus::Finished => ("●", current().red),
    }
}

pub fn draw(f: &mut Frame, app: &App) {
    crate::theme::set(crate::theme::THEMES[app.theme_index % crate::theme::THEMES.len()]);

    let outer = f.area().inner(Margin {
        horizontal: PAD_LEFT,
        vertical: 0,
    });

    let chunks = Layout::vertical([
        Constraint::Length(PAD_TOP),
        Constraint::Length(1), // dashboard header
        Constraint::Min(5),    // project cards
        Constraint::Length(1), // help
        Constraint::Length(1), // status
    ])
    .split(outer);

    let list_area = chunks[2];

    draw_dashboard(f, app, chunks[1]);
    // Master-detail view: the project/task/session tree is the sidebar (master);
    // the selected session's output + diff render in the detail pane. With no
    // projects there is nothing to detail, so the empty state spans full width.
    if app.config.projects.is_empty() {
        draw_empty_state(f, app, list_area);
    } else {
        let md = Layout::horizontal([
            Constraint::Percentage(SIDEBAR_PCT),
            Constraint::Percentage(100 - SIDEBAR_PCT),
        ])
        .split(list_area);
        draw_list(f, app, md[0]);
        draw_detail(f, app, md[1]);
    }

    draw_help(f, app, chunks[3]);
    draw_status(f, app, chunks[4]);

    if matches!(
        app.input_mode,
        InputMode::ContextMenu | InputMode::RunMenu | InputMode::AgentPicker
    ) {
        draw_context_menu(f, app, list_area);
    }

    if is_text_input_mode(app.input_mode) {
        draw_floating_input(f, app, list_area);
    }

    if app.input_mode == InputMode::CheckoutBranch {
        draw_branch_picker(f, app, list_area);
    }

    if app.input_mode == InputMode::ReviewSessionPicker {
        draw_review_session_picker(f, app, list_area);
    }

    if app.show_resources {
        draw_resource_panel(f, app, list_area);
    }
}

/// Top dashboard strip: app name + live counts of session states.
fn draw_dashboard(f: &mut Frame, app: &App, area: Rect) {
    let mut running = 0usize;
    let mut waiting = 0usize;
    let mut perm = 0usize;
    for st in app.session_statuses.values() {
        match st {
            SessionStatus::Running => running += 1,
            SessionStatus::WaitingForInput => waiting += 1,
            SessionStatus::WaitingForPermission => perm += 1,
            SessionStatus::Finished => {}
        }
    }
    let projects = app.config.projects.len();

    // Narrow terminals get a tighter strip: short title, thin separators, and
    // icon-only counts (colour + glyph carry the meaning, words are dropped).
    let compact = area.width < COMPACT_WIDTH;
    // ▞ is the logo's low-res twin: two offset blocks, same slant.
    let (title, sep_str) = if compact {
        ("▞", " · ")
    } else {
        ("▞ showrunner", "   ·   ")
    };
    let label = |glyph: &str, n: usize, word: &str| {
        if compact {
            format!("{glyph} {n}")
        } else {
            format!("{glyph} {n} {word}")
        }
    };

    let mut spans = vec![
        Span::styled(
            title,
            Style::default()
                .fg(current().accent)
                .add_modifier(Modifier::BOLD),
        ),
        Span::raw(" "),
        Span::styled(
            format!("@{}", app.hostname),
            Style::default().fg(current().muted),
        ),
        Span::raw(if compact { "  " } else { "   " }),
        Span::styled(
            label("\u{25c6}", projects, "proj"),
            Style::default().fg(current().muted),
        ),
    ];
    let sep = Style::default().fg(current().border);
    if waiting > 0 {
        spans.push(Span::styled(sep_str, sep));
        spans.push(Span::styled(
            label("●", waiting, "waiting"),
            Style::default().fg(current().green),
        ));
    }
    if perm > 0 {
        spans.push(Span::styled(sep_str, sep));
        spans.push(Span::styled(
            label("!", perm, "needs you"),
            Style::default().fg(current().magenta),
        ));
    }
    if running > 0 {
        let frame = SPINNER[app.tick % SPINNER.len()];
        spans.push(Span::styled(sep_str, sep));
        spans.push(Span::styled(
            label(frame, running, "running"),
            Style::default().fg(current().yellow),
        ));
    }
    // Remote mode + dropped daemon stream: the dashboard is now fed by the
    // local fallback prober. Say so, so frozen-vs-live data is never ambiguous.
    if app.worker.remote
        && !app
            .worker
            .connected
            .load(std::sync::atomic::Ordering::Relaxed)
    {
        spans.push(Span::styled(sep_str, sep));
        spans.push(Span::styled(
            if compact { "⚠ daemon" } else { "⚠ daemon down · local mode" },
            Style::default()
                .fg(current().red)
                .add_modifier(Modifier::BOLD),
        ));
    }
    f.render_widget(Paragraph::new(Line::from(spans)), area);
}

fn is_text_input_mode(mode: InputMode) -> bool {
    matches!(
        mode,
        InputMode::AddProjectPath
            | InputMode::AddProjectName
            | InputMode::AddSessionName
            | InputMode::AddSessionPrompt
            | InputMode::AddAdhocSessionName
            | InputMode::AddTaskName
            | InputMode::AddTaskBranch
            | InputMode::AddTaskPrompt
            |          InputMode::MergeCommitMessage
            | InputMode::SetBaseBranch
            | InputMode::Search
            | InputMode::RunCommand
            | InputMode::SendMessage
    )
}

fn draw_floating_input(f: &mut Frame, app: &App, area: Rect) {
    let width = (area.width.saturating_mul(2) / 3).max(40).min(area.width);
    // Inner text width: minus borders (2) and horizontal padding (2)
    let text_width = width.saturating_sub(4).max(1) as usize;

    // Count wrapped visual lines. Append cursor char for accurate wrap counting.
    let mut visual_lines = 0usize;
    for line in app.input_buffer.split('\n') {
        let chars = line.chars().count();
        visual_lines += chars.div_ceil(text_width).max(1);
    }
    // Cursor wraps to a new row if last line exactly fills width
    let last_line_len = app
        .input_buffer
        .split('\n')
        .next_back()
        .map(|s| s.chars().count())
        .unwrap_or(0);
    if last_line_len > 0 && last_line_len % text_width == 0 {
        visual_lines += 1;
    }

    let max_height = (area.height.saturating_mul(2) / 3).max(3);
    let height = (visual_lines as u16 + 2).clamp(3, max_height);

    // Center within the content area (excludes top padding, help, status bars)
    let x = area.x + area.width.saturating_sub(width) / 2;
    let y = area.y + area.height.saturating_sub(height) / 2;
    let rect = Rect {
        x,
        y,
        width,
        height,
    };

    let title = app
        .status_message
        .as_deref()
        .map(|s| s.trim_end().trim_end_matches(':').trim_end().to_string())
        .unwrap_or_else(|| "Input".to_string());

    f.render_widget(Clear, rect);
    let block = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(current().accent))
        .title(Span::styled(
            format!(" {title} "),
            Style::default()
                .fg(current().accent)
                .add_modifier(Modifier::BOLD),
        ));
    let inner = block.inner(rect);
    f.render_widget(block, rect);

    let segments: Vec<&str> = app.input_buffer.split('\n').collect();
    let last_idx = segments.len().saturating_sub(1);
    let lines: Vec<Line> = segments
        .iter()
        .enumerate()
        .map(|(i, seg)| {
            let mut spans = vec![Span::styled(
                (*seg).to_string(),
                Style::default().fg(current().white),
            )];
            if i == last_idx {
                spans.push(Span::styled("▌", Style::default().fg(current().accent)));
            }
            Line::from(spans)
        })
        .collect();
    let paragraph = Paragraph::new(lines).wrap(Wrap { trim: false });

    let inner_padded = inner.inner(Margin {
        horizontal: 1,
        vertical: 0,
    });
    f.render_widget(paragraph, inner_padded);
}

/// Resource overlay: per-session CPU / memory / GPU with a totals row. GPU
/// attribution is computed on demand when the panel opens (see
/// `App::toggle_resources`).
fn draw_resource_panel(f: &mut Frame, app: &App, area: Rect) {
    let width = (area.width * 75 / 100).max(50);
    // Size the panel to its content (header + one row per session + blank +
    // total) like the other modal overlays, instead of a fixed 85% height. A
    // fixed large box leaves a big `Clear`-ed (terminal default-bg) region when
    // few sessions exist, and clips the TOTAL row when many do.
    let content_rows = app.sessions.len() as u16 + 3;
    let height = (content_rows + 2).clamp(6, area.height);
    let x = area.x + area.width.saturating_sub(width) / 2;
    let y = area.y + area.height.saturating_sub(height) / 2;
    let rect = Rect { x, y, width, height };
    f.render_widget(Clear, rect);

    let block = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(current().accent))
        .title(Span::styled(
            " Resources — per session  (h / q to close) ",
            Style::default().fg(current().accent).add_modifier(Modifier::BOLD),
        ));
    let inner = block.inner(rect);
    f.render_widget(block, rect);

    let name_w = (inner.width.saturating_sub(24)).max(10) as usize;
    let bold = Style::default().add_modifier(Modifier::BOLD);
    let dim = Style::default().fg(current().muted);

    let mut lines: Vec<Line> = Vec::new();
    lines.push(Line::from(vec![
        Span::styled(format!("{:<name_w$}  ", "SESSION"), bold),
        Span::styled(format!("{:>5}  ", "CPU%"), bold),
        Span::styled(format!("{:>7}  ", "MEM"), bold),
        Span::styled(format!("{:>7}", "GPU"), bold),
    ]));

    let mut total_cpu = 0f32;
    let mut total_mem_kb: u64 = 0;
    let mut total_gpu_mib: u64 = 0;
    let mut shown = 0usize;

    for session in &app.sessions {
        let res = app.resources.get(&session.name);
        let cpu = res.map(|r| r.cpu_percent).unwrap_or(0.0);
        let mem_kb = res.map(|r| r.mem_kb).unwrap_or(0);
        let gpu_mib = app.gpu_by_session.get(&session.name).copied().unwrap_or(0);
        total_cpu += cpu;
        total_mem_kb += mem_kb;
        total_gpu_mib += gpu_mib;
        shown += 1;

        let name: String = session.name.chars().take(name_w).collect();
        lines.push(Line::from(vec![
            Span::raw(format!("{name:<name_w$}  ")),
            Span::raw(format!("{:>5}  ", cpu as u32)),
            Span::raw(format!("{:>7}  ", mem_human(mem_kb))),
            Span::styled(
                format!("{:>7}", if gpu_mib > 0 { format!("{gpu_mib}M") } else { "-".into() }),
                if gpu_mib > 0 { Style::default().fg(current().cyan) } else { dim },
            ),
        ]));
    }

    if shown == 0 {
        lines.push(Line::from(Span::styled("  (no sessions)", dim)));
    }

    lines.push(Line::from(Span::raw("")));
    lines.push(Line::from(vec![
        Span::styled(format!("{:<name_w$}  ", "TOTAL"), bold),
        Span::styled(format!("{:>5}  ", total_cpu as u32), bold),
        Span::styled(format!("{:>7}  ", mem_human(total_mem_kb)), bold),
        Span::styled(
            format!("{:>7}", if total_gpu_mib > 0 { format!("{total_gpu_mib}M") } else { "-".into() }),
            bold,
        ),
    ]));

    let para = Paragraph::new(lines).wrap(Wrap { trim: false });
    f.render_widget(para, inner);
}

fn is_project_collapsed(app: &App, name: &str) -> bool {
    app.collapsed.contains(&format!("p:{name}"))
}

/// Check if a task is the last task in its project (looking past child sessions).
fn is_last_task(items: &[app::ListItem], i: usize, project_name: &str) -> bool {
    for j in (i + 1)..items.len() {
        match &items[j] {
            app::ListItem::Session { .. } => continue,
            app::ListItem::Task {
                project_name: pn, ..
            } => return pn != project_name,
            _ => return true,
        }
    }
    true
}

/// Check if a session is the last session under its task.
fn is_last_session(items: &[app::ListItem], i: usize, project_name: &str, task_name: &str) -> bool {
    match items.get(i + 1) {
        Some(app::ListItem::Session {
            project_name: pn,
            task: t,
            ..
        }) => pn != project_name || t.name != task_name,
        _ => true,
    }
}

/// True if no Task of the same project follows the AdhocGroup at index `i`.
/// (Adhoc group is rendered before tasks, so it's "last" only when no tasks come after.)
fn is_last_adhoc_group(items: &[app::ListItem], i: usize, project_name: &str) -> bool {
    for item in items.iter().skip(i + 1) {
        match item {
            app::ListItem::Task {
                project_name: pn, ..
            } if pn == project_name => return false,
            app::ListItem::Project { .. } => return true,
            _ => continue,
        }
    }
    true
}

/// True if the AdhocSession at index `i` is the last in its group.
fn is_last_adhoc_session(items: &[app::ListItem], i: usize, project_name: &str) -> bool {
    match items.get(i + 1) {
        Some(app::ListItem::AdhocSession {
            project_name: pn, ..
        }) => pn != project_name,
        _ => true,
    }
}

/// For an AdhocSession at `i`, look back to find its parent AdhocGroup and
/// return whether that group is the last in the project.
fn is_last_adhoc_group_lookup(
    items: &[app::ListItem],
    session_idx: usize,
    project_name: &str,
) -> bool {
    for j in (0..session_idx).rev() {
        if let app::ListItem::AdhocGroup {
            project_name: pn, ..
        } = &items[j]
            && pn == project_name
        {
            return is_last_adhoc_group(items, j, project_name);
        }
    }
    true
}

/// Find whether the parent task of a session is the last task in the project.
fn parent_task_is_last(
    items: &[app::ListItem],
    session_idx: usize,
    project_name: &str,
    task_name: &str,
) -> bool {
    for j in (0..session_idx).rev() {
        if let app::ListItem::Task {
            project_name: pn,
            task: t,
            ..
        } = &items[j]
        {
            if pn == project_name && t.name == task_name {
                return is_last_task(items, j, project_name);
            }
        }
    }
    true
}

/// Extract the trailing PR number from a GitHub PR URL (`.../pull/123` → `123`).
fn pr_number(url: &str) -> Option<&str> {
    let last = url.trim_end_matches('/').rsplit('/').next()?;
    if !last.is_empty() && last.bytes().all(|b| b.is_ascii_digit()) {
        Some(last)
    } else {
        None
    }
}

// --- Overview row layout ---------------------------------------------------
// Each row keeps the name/tree on the left and packs its metadata into
// columns flush against the right edge, so churn / status / branch line up
// vertically across every row. Column widths are autoscaled per render to the
// widest cell in each column (see `ColWidths`), so nothing is truncated or
// padded more than it needs to be.
const COL_GAP: usize = 2;
// Branch column is autoscaled to the widest branch in view, but capped here so
// a single long branch can't crowd the name column off the screen.
const BRANCH_MAX: usize = 80;

// Responsive breakpoint. At or below this list width (roughly a phone terminal
// in portrait, e.g. Terminus on an iPhone) the UI drops secondary metadata so
// item names get the full width and nothing runs off-screen. Recomputed every
// frame from the actual area, so it adapts live to rotating/resizing.
const COMPACT_WIDTH: u16 = 60;

fn spans_width(spans: &[Span]) -> usize {
    spans.iter().map(|s| s.width()).sum()
}

/// Left-align `spans` within a `width`-column field (right-padded with spaces).
fn col_left<'a>(spans: Vec<Span<'a>>, width: usize) -> Vec<Span<'a>> {
    let pad = width.saturating_sub(spans_width(&spans));
    let mut out = spans;
    if pad > 0 {
        out.push(Span::raw(" ".repeat(pad)));
    }
    out
}

/// Truncate to `max` display columns, appending '…' when cut.
fn truncate_ellipsis(s: &str, max: usize) -> String {
    let len = s.chars().count();
    if len <= max {
        s.to_string()
    } else if max <= 1 {
        "…".to_string()
    } else {
        let mut t: String = s.chars().take(max - 1).collect();
        t.push('…');
        t
    }
}

fn churn_spans(added: usize, removed: usize) -> Vec<Span<'static>> {
    if added == 0 && removed == 0 {
        return Vec::new();
    }
    vec![
        Span::styled(format!("+{added}"), Style::default().fg(current().green)),
        Span::raw(" "),
        Span::styled(format!("-{removed}"), Style::default().fg(current().red)),
    ]
}

/// Per-column display widths, sized to the widest cell in each column.
#[derive(Default)]
struct ColWidths {
    name: usize,
    churn: usize,
    badge: usize,
    branch: usize,
    resources: usize,
}

/// A pre-measured overview row. Metadata cells are kept separate so the second
/// pass can right-align each within its autoscaled column width.
enum Row<'a> {
    /// Rounded top border of a project card (title + branch).
    CardTop {
        chevron: &'static str,
        name: Span<'a>,
        meta: Vec<Span<'a>>,
        branch: Option<String>,
        collapsed: bool,
        selected: bool,
    },
    /// A content row drawn inside the current card.
    Body {
        left: Vec<Span<'a>>,
        churn: Vec<Span<'a>>,
        badge: Vec<Span<'a>>,
        branch: Option<String>,
        /// Per-session resource readout (`CPU 12% · 1.2G`), when sampled.
        resources: Option<String>,
        /// Colour for the status rail (`None` → blank gutter).
        rail: Option<Color>,
        selected: bool,
        /// Whether this row's name/metadata size the shared columns.
        has_meta: bool,
    },
}

/// Build the right-hand metadata block: churn | badge | branch | resources,
/// each left-aligned within its autoscaled column. Columns nothing populates
/// (width 0) are dropped along with their gap.
fn meta_block<'a>(
    churn: Vec<Span<'a>>,
    badge: Vec<Span<'a>>,
    branch: Option<String>,
    resources: Option<String>,
    w: &ColWidths,
) -> Vec<Span<'a>> {
    let mut cells: Vec<Vec<Span<'a>>> = Vec::new();
    if w.churn > 0 {
        cells.push(col_left(churn, w.churn));
    }
    if w.badge > 0 {
        cells.push(col_left(badge, w.badge));
    }
    if w.branch > 0 {
        let text = branch
            .map(|b| truncate_ellipsis(&b, w.branch))
            .unwrap_or_default();
        cells.push(col_left(
            vec![Span::styled(text, Style::default().fg(current().muted))],
            w.branch,
        ));
    }
    if w.resources > 0 {
        let text = resources
            .map(|r| truncate_ellipsis(&r, w.resources))
            .unwrap_or_default();
        cells.push(col_left(
            vec![Span::styled(text, Style::default().fg(current().muted))],
            w.resources,
        ));
    }
    let mut out = Vec::new();
    for (i, cell) in cells.into_iter().enumerate() {
        if i > 0 {
            out.push(Span::raw(" ".repeat(COL_GAP)));
        }
        out.extend(cell);
    }
    out
}

/// Compact per-session resource readout: `CPU 12% · 1.2G`.
///
/// `round()` (half away from zero) rather than `{:.0}` (half-to-even), so
/// `12.5` displays as `13` — the intuitive reading of a percentage.
fn resource_label(r: &crate::resources::SessionResources) -> String {
    format!("CPU {}% · {}", r.cpu_percent.round() as u32, mem_human(r.mem_kb))
}

/// KiB as a short human string: `512K`, `1.2M`, `3.4G`.
fn mem_human(kb: u64) -> String {
    // KiB thresholds: 1 GiB = 1024^2 KiB, 1 TiB = 1024^3 KiB.
    const KIB_PER_GIB: u64 = 1024 * 1024;
    const KIB_PER_TIB: u64 = 1024 * 1024 * 1024;
    if kb < 1024 {
        format!("{kb}K")
    } else if kb < KIB_PER_GIB {
        let mb = kb as f64 / 1024.0;
        // Promote to G when the value would otherwise display as "1024.0M".
        if mb >= 1023.95 {
            format!("{:.1}G", mb / 1024.0)
        } else {
            format!("{:.1}M", mb)
        }
    } else if kb < KIB_PER_TIB {
        format!("{:.1}G", kb as f64 / KIB_PER_GIB as f64)
    } else {
        format!("{:.1}T", kb as f64 / KIB_PER_TIB as f64)
    }
}

/// Compose a row: `left` (name/tree) padded to the shared `name_w` column, then
/// the `right` metadata block left-aligned a `COL_GAP` further right, so the
/// metadata sits just past the longest name instead of at the screen edge.
fn row_line<'a>(left: Vec<Span<'a>>, right: Vec<Span<'a>>, name_w: usize) -> Line<'a> {
    if right.is_empty() {
        return Line::from(left);
    }
    let pad = name_w.saturating_sub(spans_width(&left)) + COL_GAP;
    let mut spans = left;
    spans.push(Span::raw(" ".repeat(pad)));
    spans.extend(right);
    Line::from(spans)
}

/// Interior offset of card body content: `│` + space + rail + space.
const CARD_INDENT: usize = 4;

/// Dim column-header row, indented to line up with card body columns.
fn header_line<'a>(w: &ColWidths) -> ListItem<'a> {
    let dim = Style::default()
        .fg(current().muted)
        .add_modifier(Modifier::BOLD);
    let churn = if w.churn > 0 {
        vec![Span::styled("CHANGES", dim)]
    } else {
        Vec::new()
    };
    let badge = if w.badge > 0 {
        vec![Span::styled("PR", dim)]
    } else {
        Vec::new()
    };
    let branch = (w.branch > 0).then(|| "BRANCH".to_string());
    let resources = (w.resources > 0).then(|| "RES".to_string());
    let right = meta_block(churn, badge, branch, resources, w);
    let inner = row_line(vec![Span::styled("NAME", dim)], right, w.name);
    let mut spans = vec![Span::raw(" ".repeat(CARD_INDENT))];
    spans.extend(inner.spans);
    ListItem::new(Line::from(spans))
}

/// Rounded top border of a project card: `╭─ ▼ name … (branch) ─╮`.
fn card_top<'a>(
    width: u16,
    chevron: &str,
    name: Span<'a>,
    meta: Vec<Span<'a>>,
    branch: Option<String>,
    selected: bool,
    rounded: bool,
) -> ListItem<'a> {
    let border = Style::default().fg(if selected {
        current().accent
    } else {
        current().border
    });
    // Rounded corners open a box (project with content); a flat line is just a
    // header (collapsed/empty project).
    let (lead, tail) = if rounded {
        ("╭─ ", "─╮")
    } else {
        ("── ", "──")
    };
    let mut left = vec![
        Span::styled(lead, border),
        Span::styled(chevron.to_string(), Style::default().fg(current().muted)),
        name,
    ];
    left.extend(meta);
    left.push(Span::raw(" "));
    let right = match branch {
        Some(b) => vec![
            Span::styled(format!("({b})"), Style::default().fg(current().muted)),
            Span::styled(format!(" {tail}"), border),
        ],
        None => vec![Span::styled(tail.to_string(), border)],
    };
    let fill = (width as usize).saturating_sub(spans_width(&left) + spans_width(&right));
    let mut spans = left;
    spans.push(Span::styled("─".repeat(fill), border));
    spans.extend(right);
    ListItem::new(Line::from(spans))
}

/// Rounded bottom border of a project card.
fn card_bottom<'a>(width: u16) -> ListItem<'a> {
    let s = format!("╰{}╯", "─".repeat((width as usize).saturating_sub(2)));
    ListItem::new(Line::from(Span::styled(
        s,
        Style::default().fg(current().border),
    )))
}

/// Wrap a body `inner` line in card borders with a status rail, padding to the
/// full width and tinting the interior when selected.
fn wrap_body<'a>(width: u16, rail: Option<Color>, inner: Line<'a>, selected: bool) -> ListItem<'a> {
    let border = Style::default().fg(current().border);
    let rail_span = match rail {
        Some(c) => Span::styled("▎", Style::default().fg(c)),
        None => Span::raw(" "),
    };
    let mut mid = vec![Span::raw(" "), rail_span, Span::raw(" ")];
    mid.extend(inner.spans);
    // Pad the interior out to the right border.
    let used = 1 + spans_width(&mid); // left border + interior
    let pad = (width as usize).saturating_sub(used + 1); // +1 right border
    mid.push(Span::raw(" ".repeat(pad)));
    if selected {
        for s in &mut mid {
            s.style = s.style.bg(current().select_bg);
        }
    }
    let mut spans = vec![Span::styled("│", border)];
    spans.extend(mid);
    spans.push(Span::styled("│", border));
    ListItem::new(Line::from(spans))
}

/// Centered placeholder shown when no projects are configured yet.
fn draw_empty_state(f: &mut Frame, app: &App, area: Rect) {
    let key = key_display(app.keybindings.add_project);
    let lines = vec![
        Line::from(Span::styled(
            "No projects yet",
            Style::default()
                .fg(current().white)
                .add_modifier(Modifier::BOLD),
        )),
        Line::from(""),
        Line::from(vec![
            Span::styled("Press ", Style::default().fg(current().muted)),
            Span::styled(
                key,
                Style::default()
                    .fg(current().accent)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled(
                " to add your first project",
                Style::default().fg(current().muted),
            ),
        ]),
    ];

    // Vertically center the block within the available area.
    let height = lines.len() as u16;
    let top_pad = area.height.saturating_sub(height) / 2;
    let target = Rect {
        x: area.x,
        y: area.y + top_pad,
        width: area.width,
        height: height.min(area.height),
    };

    let paragraph = Paragraph::new(lines).alignment(Alignment::Center);
    f.render_widget(paragraph, target);
}

/// Compute the first visible row so the selection stays on screen. `prev` is
/// last frame's offset (for stability), `sel` the selected row, `total` the row
/// count, `height` the viewport height. Rows are one terminal line each. The
/// offset only moves when the selection would fall outside the viewport, so the
/// view is stable while navigating within the visible window and scrolls just
/// enough — down (selection pinned to the bottom) or up (pinned to the top) —
/// when it wouldn't be.
fn scroll_offset(prev: usize, sel: usize, total: usize, height: usize) -> usize {
    let max_offset = total.saturating_sub(height);
    let mut offset = prev.min(max_offset);
    if sel < offset {
        offset = sel;
    } else if height > 0 && sel >= offset + height {
        offset = sel + 1 - height;
    }
    offset
}

fn draw_list(f: &mut Frame, app: &App, area: Rect) {
    // Narrow (phone) terminals drop the right-hand metadata columns and card
    // branch labels so names get the full width. See COMPACT_WIDTH.
    let compact = area.width < COMPACT_WIDTH;
    let mut rows: Vec<Row> = Vec::new();
    let indicator_style = Style::default()
        .fg(current().accent)
        .add_modifier(Modifier::BOLD);
    let tree_style = Style::default().fg(current().border);

    // Stacked-task positions (⧉ n/m) per project, keyed by task branch.
    let stacks: HashMap<&str, HashMap<String, (usize, usize)>> = app
        .config
        .projects
        .iter()
        .map(|p| (p.name.as_str(), p.stack_positions()))
        .collect();

    for (i, item) in app.items.iter().enumerate() {
        let is_selected = i == app.selected;

        match item {
            app::ListItem::Project { project } => {
                let collapsed = is_project_collapsed(app, &project.name);
                let chevron = if collapsed { "▶ " } else { "▼ " };
                let name_style = if is_selected {
                    Style::default()
                        .fg(current().accent)
                        .add_modifier(Modifier::BOLD)
                } else {
                    Style::default()
                        .fg(current().white)
                        .add_modifier(Modifier::BOLD)
                };
                let name = Span::styled(project.name.as_str(), name_style);

                // Show task/session counts when project is collapsed.
                let mut meta: Vec<Span> = Vec::new();
                if collapsed {
                    let task_count = project.tasks.len();
                    let sanitized = tmux::sanitize(&project.name);
                    let active_sessions = app
                        .sessions
                        .iter()
                        .filter(|s| {
                            s.project_name == sanitized
                                && app
                                    .session_statuses
                                    .get(&s.name)
                                    .map_or(false, |st| *st != SessionStatus::Finished)
                        })
                        .count();
                    if task_count > 0 || active_sessions > 0 {
                        let mut parts = Vec::new();
                        if task_count > 0 {
                            parts.push(format!(
                                "{task_count} task{}",
                                if task_count == 1 { "" } else { "s" }
                            ));
                        }
                        if active_sessions > 0 {
                            parts.push(format!("{active_sessions} active"));
                        }
                        meta.push(Span::styled(
                            format!("  [{}]", parts.join(", ")),
                            Style::default().fg(current().green),
                        ));
                    }
                }

                if let Some(ind) = run_indicator(app, item) {
                    meta.push(ind);
                }

                let branch = app.project_branches.get(&project.name).cloned();
                rows.push(Row::CardTop {
                    chevron,
                    name,
                    meta,
                    branch,
                    collapsed,
                    selected: is_selected,
                });
            }
            app::ListItem::Task {
                project_name, task, ..
            } => {
                let indicator = if is_selected { " ▸ " } else { "   " };
                let last = is_last_task(&app.items, i, project_name);
                let branch_char = if last { "└─ " } else { "├─ " };
                let base_color = if task.archived {
                    current().muted
                } else {
                    current().yellow
                };
                let style = if is_selected {
                    Style::default().fg(base_color).add_modifier(Modifier::BOLD)
                } else {
                    Style::default().fg(base_color)
                };
                let mut left = vec![
                    Span::styled(indicator, indicator_style),
                    Span::styled(branch_char, tree_style),
                    Span::styled(&task.name, style),
                ];

                if let Some((pos, total)) = stacks
                    .get(project_name.as_str())
                    .and_then(|s| s.get(&task.branch))
                {
                    left.push(Span::styled(
                        format!("  ⧉ {pos}/{total}"),
                        Style::default().fg(current().cyan),
                    ));
                }

                if task.archived {
                    left.push(Span::styled(
                        "  [archived]",
                        Style::default().fg(current().muted),
                    ));
                }

                // Show active session count when task is collapsed
                if app
                    .collapsed
                    .contains(&format!("t:{project_name}:{}", task.name))
                {
                    let sessions = tmux::sessions_for_task(project_name, &task.name, &app.sessions);
                    let active = sessions
                        .iter()
                        .filter(|s| {
                            app.session_statuses
                                .get(&s.name)
                                .map_or(false, |st| *st != SessionStatus::Finished)
                        })
                        .count();
                    if active > 0 {
                        left.push(Span::styled(
                            format!("  [{active} active]"),
                            Style::default().fg(current().green),
                        ));
                    }
                }

                if let Some(ind) = run_indicator(app, item) {
                    left.push(ind);
                }

                // --- right-hand metadata columns: churn | badge | branch ---
                let (added, removed) = app
                    .task_diff_stats
                    .get(&task.branch)
                    .map(|s| (s.added, s.removed))
                    .unwrap_or((0, 0));

                // Badge column: PR icon.
                let badge = if let Some(url) = app.pr_urls.get(&task.branch) {
                    // Show "#<number>" instead of a bare icon; fall back to "PR"
                    // when the URL has no numeric id.
                    let label = pr_number(url)
                        .map(|n| format!("#{n}"))
                        .unwrap_or_else(|| "PR".to_string());
                    vec![Span::styled(label, Style::default().fg(current().magenta))]
                } else {
                    Vec::new()
                };

                // Branch column: branch name, with "← base" suffix for non-main bases.
                let branch_label = match task.base_branch.as_deref() {
                    Some(base) if !base.is_empty() && base != "main" => {
                        format!("{} \u{2190} {base}", task.branch)
                    }
                    _ => task.branch.clone(),
                };

                rows.push(Row::Body {
                    left,
                    churn: churn_spans(added, removed),
                    badge,
                    branch: Some(branch_label),
                    resources: None,
                    rail: None,
                    selected: is_selected,
                    has_meta: true,
                });
            }
            app::ListItem::AdhocGroup {
                project_name,
                session_count,
                ..
            } => {
                let indicator = if is_selected { " ▸ " } else { "   " };
                let last = is_last_adhoc_group(&app.items, i, project_name);
                let branch_char = if last { "└─ " } else { "├─ " };
                let collapsed = app.collapsed.contains(&format!("a:{project_name}"));
                let chevron = if collapsed { "▶ " } else { "▼ " };
                let style = if is_selected {
                    Style::default()
                        .fg(current().accent)
                        .add_modifier(Modifier::BOLD)
                } else {
                    Style::default().fg(current().accent)
                };
                let mut spans = vec![
                    Span::styled(indicator, indicator_style),
                    Span::styled(branch_char, tree_style),
                    Span::styled(chevron, Style::default().fg(current().muted)),
                    Span::styled("⌂ adhoc", style),
                ];
                if collapsed && *session_count > 0 {
                    spans.push(Span::styled(
                        format!("  [{session_count}]"),
                        Style::default().fg(current().green),
                    ));
                }
                rows.push(Row::Body {
                    left: spans,
                    churn: Vec::new(),
                    badge: Vec::new(),
                    branch: None,
                    resources: None,
                    rail: None,
                    selected: is_selected,
                    has_meta: false,
                });
            }
            app::ListItem::AdhocSession {
                project_name,
                session,
                ..
            } => {
                let indicator = if is_selected { " ▸ " } else { "   " };
                // The main session sits on the task branch itself — mark it out.
                let is_main = tmux::is_main_session(&session.session_name);
                let name_color = if is_main {
                    current().accent
                } else {
                    current().green
                };
                let mut style = Style::default().fg(name_color);
                if is_selected || is_main {
                    style = style.add_modifier(Modifier::BOLD);
                }

                let status = app
                    .session_statuses
                    .get(&session.name)
                    .copied()
                    .unwrap_or(SessionStatus::Finished);
                let (status_icon, status_color) = status_glyph(status, app.tick);

                let group_last = is_last_adhoc_group_lookup(&app.items, i, project_name);
                let session_last = is_last_adhoc_session(&app.items, i, project_name);
                let continuation = if group_last { "   " } else { "│  " };
                let branch_char = if session_last { "└─ " } else { "├─ " };

                let mut spans = vec![
                    Span::styled(indicator, indicator_style),
                    Span::styled(continuation, tree_style),
                    Span::styled(branch_char, tree_style),
                    Span::styled(format!("{status_icon} "), Style::default().fg(status_color)),
                    Span::styled("⌂ ", Style::default().fg(current().accent)),
                    Span::styled(&session.session_name, style),
                ];
                if let Some(icon) = agent_icon_span(app, &session.name) {
                    spans.push(icon);
                }
                if let Some(ind) = run_indicator(app, item) {
                    spans.push(ind);
                }
                if let Some(err) = detail_error_span(app, &session.name) {
                    spans.push(err);
                }
                let resources = app
                    .resources
                    .get(&session.name)
                    .map(resource_label);
                rows.push(Row::Body {
                    left: spans,
                    churn: Vec::new(),
                    badge: Vec::new(),
                    branch: None,
                    resources,
                    rail: Some(status_color),
                    selected: is_selected,
                    has_meta: false,
                });
            }
            app::ListItem::Session {
                project_name,
                task,
                session,
                ..
            } => {
                let indicator = if is_selected { " ▸ " } else { "   " };
                // The main session sits on the task branch itself — mark it out.
                let is_main = tmux::is_main_session(&session.session_name);
                let name_color = if is_main {
                    current().accent
                } else {
                    current().green
                };
                let mut style = Style::default().fg(name_color);
                if is_selected || is_main {
                    style = style.add_modifier(Modifier::BOLD);
                }

                let status = app
                    .session_statuses
                    .get(&session.name)
                    .copied()
                    .unwrap_or(SessionStatus::Finished);
                let (status_icon, status_color) = status_glyph(status, app.tick);

                let parent_last = parent_task_is_last(&app.items, i, project_name, &task.name);
                let session_last = is_last_session(&app.items, i, project_name, &task.name);
                let continuation = if parent_last { "   " } else { "│  " };
                let branch_char = if session_last { "└─ " } else { "├─ " };

                let wt = session.worktree_path();
                let mut left = vec![
                    Span::styled(indicator, indicator_style),
                    Span::styled(continuation, tree_style),
                    Span::styled(branch_char, tree_style),
                    Span::styled(format!("{status_icon} "), Style::default().fg(status_color)),
                ];
                if is_main {
                    left.push(Span::styled("◆ ", Style::default().fg(current().accent)));
                } else if wt.is_some() {
                    left.push(Span::styled(
                        "\u{e0a0} ",
                        Style::default().fg(current().border),
                    ));
                } else {
                    left.push(Span::styled("⌂ ", Style::default().fg(current().accent)));
                }
                left.push(Span::styled(&session.session_name, style));
                if let Some(icon) = agent_icon_span(app, &session.name) {
                    left.push(icon);
                }
                if let Some(ind) = run_indicator(app, item) {
                    left.push(ind);
                }
                if let Some(err) = detail_error_span(app, &session.name) {
                    left.push(err);
                }

                // --- right-hand metadata columns: churn | branch | resources ---
                let churn = app
                    .diff_stats
                    .get(&session.name)
                    .filter(|s| !s.is_empty())
                    .map(|s| churn_spans(s.added, s.removed))
                    .unwrap_or_default();
                let branch = app.session_branches.get(&session.name).cloned();
                let resources = app
                    .resources
                    .get(&session.name)
                    .map(resource_label);
                rows.push(Row::Body {
                    left,
                    churn,
                    badge: Vec::new(),
                    branch,
                    resources,
                    rail: Some(status_color),
                    selected: is_selected,
                    has_meta: true,
                });
            }
        }
    }

    // Pass 2: size columns to content (and to the column-header labels), then
    // emit cards with the column header on top.
    let mut widths = ColWidths::default();
    for row in &rows {
        if let Row::Body {
            left,
            churn,
            badge,
            branch,
            resources,
            has_meta,
            ..
        } = row
        {
            if *has_meta {
                widths.name = widths.name.max(spans_width(left));
            }
            widths.churn = widths.churn.max(spans_width(churn));
            widths.badge = widths.badge.max(spans_width(badge));
            if let Some(b) = branch {
                widths.branch = widths.branch.max(b.chars().count());
            }
            if let Some(r) = resources {
                widths.resources = widths.resources.max(r.chars().count());
            }
        }
    }
    widths.branch = widths.branch.min(BRANCH_MAX);
    if compact {
        // Zero-width columns are dropped (with their header labels) by
        // meta_block / header_line, collapsing every card to just its names.
        widths.churn = 0;
        widths.badge = 0;
        widths.branch = 0;
        widths.resources = 0;
    }
    if widths.churn > 0 {
        widths.churn = widths.churn.max("CHANGES".len());
    }
    if widths.badge > 0 {
        widths.badge = widths.badge.max("PR".len());
    }
    if widths.branch > 0 {
        widths.branch = widths.branch.max("BRANCH".len());
    }
    if widths.resources > 0 {
        widths.resources = widths.resources.max("RES".len());
    }

    let mut lines: Vec<ListItem> = vec![header_line(&widths)];
    // A card has content (→ full rounded box) when a body row follows it;
    // otherwise it's a collapsed/empty project rendered as a flat header line.
    let has_body: Vec<bool> = (0..rows.len())
        .map(|i| matches!(rows.get(i + 1), Some(Row::Body { .. })))
        .collect();

    let mut card_open = false;
    let mut card_has_body = false;
    let mut seen_card = false;
    let mut sel_row = 0u16;
    for (i, row) in rows.into_iter().enumerate() {
        match row {
            Row::CardTop {
                chevron,
                name,
                meta,
                branch,
                collapsed,
                selected,
            } => {
                if card_open && card_has_body {
                    lines.push(card_bottom(area.width));
                }
                card_open = false;
                card_has_body = false;
                if seen_card {
                    lines.push(ListItem::new(Line::raw("")));
                }
                seen_card = true;
                if selected {
                    sel_row = lines.len() as u16;
                }
                let rounded = has_body[i];
                let branch = if compact { None } else { branch };
                lines.push(card_top(
                    area.width, chevron, name, meta, branch, selected, rounded,
                ));
                if !collapsed {
                    card_open = true;
                }
            }
            Row::Body {
                left,
                churn,
                badge,
                branch,
                resources,
                rail,
                selected,
                ..
            } => {
                let right = meta_block(churn, badge, branch, resources, &widths);
                let inner = row_line(left, right, widths.name);
                if selected {
                    sel_row = lines.len() as u16;
                }
                card_has_body = true;
                lines.push(wrap_body(area.width, rail, inner, selected));
            }
        }
    }
    if card_open && card_has_body {
        lines.push(card_bottom(area.width));
    }
    // Scroll so the selected row stays visible. Each ListItem is exactly one
    // terminal row, so `sel_row` and offsets are line indices. The offset is
    // persisted across frames (App::list_offset) so scrolling feels stable in
    // both directions: it only moves when the selection would leave the viewport.
    let offset = scroll_offset(
        app.list_offset.get() as usize,
        sel_row as usize,
        lines.len(),
        area.height as usize,
    );
    app.list_offset.set(offset as u16);
    // Record the selection's on-screen row (relative to the list area top) so
    // popups anchor to it even when the list is scrolled.
    app.selected_row.set(sel_row.saturating_sub(offset as u16));

    let list = List::new(lines).block(Block::default().borders(Borders::NONE));
    let mut state = ListState::default().with_offset(offset);
    f.render_stateful_widget(list, area, &mut state);
}

/// The master-detail "detail" pane: an attention/status header for the selected
/// session, its latest output, and its worktree diff. Backed by `app.detail`,
/// which `maybe_refresh_detail` keeps fresh on a short timer.
fn draw_detail(f: &mut Frame, app: &App, area: Rect) {
    // Very thin/short terminals: collapse to a single hint line.
    if area.width < 24 || area.height < 5 {
        let hint = Paragraph::new(Line::from(Span::styled(
            "◧ detail",
            Style::default().fg(current().muted),
        )));
        f.render_widget(hint, area);
        return;
    }

    let Some(name) = app.selected_session_name() else {
        let hint = Paragraph::new(Line::from(Span::styled(
            "Select a session to see its output and diff",
            Style::default().fg(current().muted),
        )));
        f.render_widget(hint, area);
        return;
    };

    let status = app
        .session_statuses
        .get(&name)
        .copied()
        .unwrap_or(SessionStatus::Finished);
    let (status_icon, status_color) = status_glyph(status, app.tick);
    let status_label = match status {
        SessionStatus::Running => "running",
        SessionStatus::WaitingForInput => "awaiting input",
        SessionStatus::WaitingForPermission => "needs permission",
        SessionStatus::Finished => "finished",
    };

    let mut header = vec![
        Span::styled(format!("{status_icon} "), Style::default().fg(status_color)),
        Span::styled(
            name.clone(),
            Style::default()
                .fg(current().accent)
                .add_modifier(Modifier::BOLD),
        ),
    ];
    if let Some(icon) = agent_icon_span(app, &name) {
        header.push(icon);
    }
    header.push(Span::styled(
        format!("  {status_label}"),
        Style::default().fg(status_color),
    ));

    let detail = app.detail.as_ref().filter(|d| d.session == name);
    // Error attention: a red "✗ error" badge when the recent output looks like
    // an error, surfacing the "erro" attention alongside permission/idle.
    if detail.map_or(false, |d| d.has_error) {
        header.push(Span::styled(
            "  ✗ error",
            Style::default()
                .fg(current().red)
                .add_modifier(Modifier::BOLD),
        ));
    }
    if let Some(branch) = app.session_branches.get(&name) {
        header.push(Span::styled(
            format!("  ⎇ {branch}"),
            Style::default().fg(current().muted),
        ));
    }
    if let Some(res) = app.resources.get(&name).map(resource_label) {
        header.push(Span::styled(
            format!("  {res}"),
            Style::default().fg(current().muted),
        ));
    }

    let dl = Layout::vertical([
        Constraint::Length(1),
        Constraint::Min(2),
        Constraint::Min(2),
    ])
    .split(area);

    f.render_widget(Paragraph::new(Line::from(header)), dl[0]);

    let focus = app.detail_focus.get();
    let output_focused = focus == app::DetailFocus::Output;
    let output_text = detail
        .and_then(|d| d.output.as_deref())
        .unwrap_or("(no output yet)");
    let output_block = detail_block(" output ", output_focused);
    let output = Paragraph::new(output_text)
        .block(output_block)
        .scroll((app.detail_output_scroll.get(), 0))
        .wrap(Wrap { trim: false });
    f.render_widget(output, dl[1]);

    let diff_focused = focus == app::DetailFocus::Diff;
    let diff_text = detail
        .and_then(|d| d.diff.as_deref())
        .unwrap_or("(no diff)");
    let diff_block = detail_block(" diff ", diff_focused);
    let diff = Paragraph::new(diff_text)
        .block(diff_block)
        .scroll((app.detail_diff_scroll.get(), 0))
        .wrap(Wrap { trim: false });
    f.render_widget(diff, dl[2]);
}

/// Top-bordered block for a detail-pane section. The focused section gets a
/// brighter border + bold title so the user knows which one PageUp/Down scrolls.
fn detail_block(title: &str, focused: bool) -> Block<'_> {
    let (border, title_style) = if focused {
        (
            current().accent,
            Style::default()
                .fg(current().accent)
                .add_modifier(Modifier::BOLD),
        )
    } else {
        (current().border, Style::default().fg(current().muted))
    };
    Block::default()
        .borders(Borders::TOP)
        .border_style(Style::default().fg(border))
        .title(title)
        .title_style(title_style)
}

fn draw_context_menu(f: &mut Frame, app: &App, area: Rect) {
    let items = &app.context_menu_items;
    if items.is_empty() {
        return;
    }

    let max_label_width = items.iter().map(|i| i.label.len()).max().unwrap_or(0);
    // "  label   key  " — padding + label + gap + key + padding
    let menu_width = (max_label_width + 8).max(16) as u16;
    let menu_height = items.len() as u16 + 2; // +2 for border

    // Position: anchored to the selected item's row, nudged one row down so it
    // drops from the item; clamped to stay within the list area.
    let x = area.x + 4;
    let y = area.y + app.selected_row.get().saturating_add(1);
    let max_y = (area.y + area.height).saturating_sub(menu_height);

    let menu_area = Rect {
        x: x.min(area.x + area.width - menu_width),
        y: y.min(max_y),
        width: menu_width.min(area.width),
        height: menu_height.min(area.height),
    };

    // Clear background
    let clear = ratatui::widgets::Clear;
    f.render_widget(clear, menu_area);

    let block = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(current().accent));

    let inner = block.inner(menu_area);
    f.render_widget(block, menu_area);

    for (i, item) in items.iter().enumerate() {
        if i as u16 >= inner.height {
            break;
        }
        let is_selected = i == app.context_menu_selected;
        let row_area = Rect {
            x: inner.x,
            y: inner.y + i as u16,
            width: inner.width,
            height: 1,
        };

        let label_style = if is_selected {
            Style::default()
                .fg(current().accent)
                .add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(current().white)
        };
        let key_style = Style::default().fg(current().muted);

        let key_str = if item.key.is_uppercase() {
            format!("S-{}", item.key.to_lowercase())
        } else {
            item.key.to_string()
        };
        let padding = inner.width as usize - key_str.len() - 2; // 1 left pad + 1 right pad
        let line = Line::from(vec![
            Span::styled(" ", Style::default()),
            Span::styled(
                format!("{:<width$}", item.label, width = padding),
                label_style,
            ),
            Span::styled(key_str, key_style),
            Span::styled(" ", Style::default()),
        ]);
        f.render_widget(Paragraph::new(line), row_area);
    }
}

/// Floating fuzzy branch picker: a filter line plus a scrolling, highlighted
/// list of matching branches. Centered within the list area.
fn draw_branch_picker(f: &mut Frame, app: &App, area: Rect) {
    let matches = app.filtered_branches();

    let width = (area.width.saturating_mul(2) / 3).max(40).min(area.width);
    // Reserve rows for: border (2) + filter line (1) + separator (1).
    let max_list_rows = area.height.saturating_sub(6).max(1) as usize;
    let list_rows = matches.len().clamp(1, max_list_rows);
    let height = (list_rows as u16 + 4).min(area.height);

    let x = area.x + area.width.saturating_sub(width) / 2;
    let y = area.y + area.height.saturating_sub(height) / 2;
    let rect = Rect {
        x,
        y,
        width,
        height,
    };

    f.render_widget(Clear, rect);
    let block = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(current().accent))
        .title(Span::styled(
            format!(" Checkout branch ({} matches) ", matches.len()),
            Style::default()
                .fg(current().accent)
                .add_modifier(Modifier::BOLD),
        ));
    let inner = block.inner(rect);
    f.render_widget(block, rect);

    // Filter line at the top: "› <query>▌".
    let filter_area = Rect {
        x: inner.x + 1,
        y: inner.y,
        width: inner.width.saturating_sub(1),
        height: 1,
    };
    let filter_line = Line::from(vec![
        Span::styled("› ", Style::default().fg(current().muted)),
        Span::styled(
            app.input_buffer.clone(),
            Style::default().fg(current().white),
        ),
        Span::styled("▌", Style::default().fg(current().accent)),
    ]);
    f.render_widget(Paragraph::new(filter_line), filter_area);

    let list_top = inner.y + 1;
    let visible = inner.height.saturating_sub(1) as usize;

    if matches.is_empty() {
        let none_area = Rect {
            x: inner.x + 1,
            y: list_top,
            width: inner.width.saturating_sub(1),
            height: 1,
        };
        f.render_widget(
            Paragraph::new(Span::styled(
                "no matching branches",
                Style::default().fg(current().muted),
            )),
            none_area,
        );
        return;
    }

    // Scroll so the selection stays visible.
    let selected = app.branch_picker_selected.min(matches.len() - 1);
    let offset = if selected >= visible {
        selected + 1 - visible
    } else {
        0
    };

    for (row, branch) in matches.iter().skip(offset).take(visible).enumerate() {
        let idx = offset + row;
        let is_selected = idx == selected;
        let row_area = Rect {
            x: inner.x,
            y: list_top + row as u16,
            width: inner.width,
            height: 1,
        };
        let (marker, style) = if is_selected {
            (
                "▌ ",
                Style::default()
                    .fg(current().accent)
                    .add_modifier(Modifier::BOLD),
            )
        } else {
            ("  ", Style::default().fg(current().white))
        };
        let line = Line::from(vec![
            Span::styled(marker, Style::default().fg(current().accent)),
            Span::styled((*branch).clone(), style),
        ]);
        f.render_widget(Paragraph::new(line), row_area);
    }
}

/// Floating picker for choosing which session a difit review's comments are
/// forwarded to. Shown when a reviewed task has more than one session. Centered
/// within the list area; modeled on the branch picker but without a filter line.
fn draw_review_session_picker(f: &mut Frame, app: &App, area: Rect) {
    let candidates = &app.review_candidates;
    if candidates.is_empty() {
        return;
    }

    let title = " Send review comments to ";
    let widest_name = candidates
        .iter()
        .map(|(_, display)| display.len())
        .max()
        .unwrap_or(0);
    // marker (2) + name + padding (2), at least wide enough for the title.
    let width = ((widest_name + 6).max(title.len() + 2) as u16)
        .max(28)
        .min(area.width);

    let max_list_rows = area.height.saturating_sub(2).max(1) as usize;
    let list_rows = candidates.len().clamp(1, max_list_rows);
    let height = (list_rows as u16 + 2).min(area.height);

    let x = area.x + area.width.saturating_sub(width) / 2;
    let y = area.y + area.height.saturating_sub(height) / 2;
    let rect = Rect {
        x,
        y,
        width,
        height,
    };

    f.render_widget(Clear, rect);
    let block = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(current().accent))
        .title(Span::styled(
            title,
            Style::default()
                .fg(current().accent)
                .add_modifier(Modifier::BOLD),
        ));
    let inner = block.inner(rect);
    f.render_widget(block, rect);

    let visible = inner.height as usize;
    let selected = app.review_selected.min(candidates.len() - 1);
    let offset = if selected >= visible {
        selected + 1 - visible
    } else {
        0
    };

    for (row, (_, display)) in candidates.iter().skip(offset).take(visible).enumerate() {
        let idx = offset + row;
        let is_selected = idx == selected;
        let row_area = Rect {
            x: inner.x,
            y: inner.y + row as u16,
            width: inner.width,
            height: 1,
        };
        let (marker, style) = if is_selected {
            (
                "▌ ",
                Style::default()
                    .fg(current().accent)
                    .add_modifier(Modifier::BOLD),
            )
        } else {
            ("  ", Style::default().fg(current().white))
        };
        let line = Line::from(vec![
            Span::styled(marker, Style::default().fg(current().accent)),
            Span::styled(display.clone(), style),
        ]);
        f.render_widget(Paragraph::new(line), row_area);
    }
}

/// Format a keybinding char for display in hints (e.g. ' ' → "␣", uppercase → "S-x").
fn key_display(c: char) -> String {
    match c {
        ' ' => "␣".to_string(),
        c if c.is_uppercase() => format!("S-{}", c.to_lowercase()),
        c => c.to_string(),
    }
}

fn draw_help(f: &mut Frame, app: &App, area: Rect) {
    // On narrow terminals the single-line help bar would overflow, so show only
    // the essential hints (the rest stay reachable, just unlisted).
    let compact = area.width < COMPACT_WIDTH;
    let help_spans = match app.input_mode {
        InputMode::Normal => {
            let kb = &app.keybindings;
            let enter_label = if matches!(
                app.selected_item(),
                Some(app::ListItem::Session { .. }) | Some(app::ListItem::AdhocSession { .. })
            ) {
                "attach"
            } else {
                "collapse"
            };
            if compact {
                help_bar(&[
                    ("⏎", enter_label),
                    (&key_display(kb.context_menu), "actions"),
                    (&key_display(kb.search), "filter"),
                    (&key_display(kb.quit), "quit"),
                ])
            } else {
                help_bar(&[
                    ("⏎", enter_label),
                    (&key_display(kb.toggle_collapse), "collapse"),
                    (&key_display(kb.context_menu), "actions"),
                    (&key_display(kb.search), "filter"),
                    (
                        &key_display(kb.toggle_archive_view),
                        if app.view_archived {
                            "active"
                        } else {
                            "archived"
                        },
                    ),
                    (&key_display(kb.add_project), "add project"),
                    (&key_display(kb.resources), "resources"),
                    (&key_display(kb.quit), "quit"),
                ])
            }
        }
        InputMode::ContextMenu => {
            let kb = &app.keybindings;
            let nav_keys = format!("{}/{}", key_display(kb.move_down), key_display(kb.move_up));
            help_bar(&[("⏎", "select"), (&nav_keys, "navigate"), ("Esc", "close")])
        }
        InputMode::RunMenu => help_bar(&[
            ("a", "attach"),
            ("r", "restart"),
            ("k", "kill"),
            ("Esc", "close"),
        ]),
        InputMode::AgentPicker => help_bar(&[
            ("⏎", "select agent"),
            ("↑/↓", "navigate"),
            ("Esc", "cancel"),
        ]),
        InputMode::AddTaskPrompt
        | InputMode::AddSessionPrompt
        | InputMode::MergeCommitMessage
        | InputMode::SendMessage => {
            help_bar(&[("⏎", "confirm"), ("⌥⏎", "newline"), ("Esc", "cancel")])
        }
        InputMode::AddProjectName
        | InputMode::AddSessionName
        | InputMode::AddAdhocSessionName
        | InputMode::AddTaskName
        | InputMode::AddTaskBranch
        | InputMode::SetBaseBranch
        | InputMode::RunCommand => help_bar(&[("⏎", "confirm"), ("Esc", "cancel")]),
        InputMode::ConfirmDelete | InputMode::ConfirmCreatePr => {
            help_bar(&[("y", "confirm"), ("n/Esc", "cancel")])
        }
        InputMode::AddProjectPath => {
            help_bar(&[("⏎", "confirm"), ("⇥", "complete"), ("Esc", "cancel")])
        }
        InputMode::Search => help_bar(&[("⏎", "apply"), ("Esc", "clear")]),
        InputMode::CheckoutBranch => {
            help_bar(&[("⏎", "checkout"), ("↑/↓", "navigate"), ("Esc", "cancel")])
        }
        InputMode::ReviewSessionPicker => {
            help_bar(&[("⏎", "send"), ("↑/↓", "navigate"), ("Esc", "cancel")])
        }
    };

    let help = Paragraph::new(Line::from(help_spans));
    f.render_widget(help, area);
}

fn help_bar(items: &[(&str, &str)]) -> Vec<Span<'static>> {
    let key_style = Style::default().fg(Color::Rgb(140, 140, 150));
    let desc_style = Style::default().fg(current().muted);
    let sep_style = Style::default().fg(Color::Rgb(50, 50, 60));

    let mut spans = Vec::new();
    for (i, (key, desc)) in items.iter().enumerate() {
        if i > 0 {
            spans.push(Span::styled("  ", sep_style));
        }
        spans.push(Span::styled(key.to_string(), key_style));
        spans.push(Span::styled(format!(" {desc}"), desc_style));
    }
    spans
}

fn draw_status(f: &mut Frame, app: &App, area: Rect) {
    // Text input modes render in the floating overlay instead of the status bar
    if is_text_input_mode(app.input_mode) {
        return;
    }
    // Show PR URL when a task with a PR is selected and no other status message
    if app.status_message.is_none() && app.input_mode == InputMode::Normal {
        if let Some(app::ListItem::Task { task, .. }) = app.selected_item() {
            if let Some(url) = app.pr_urls.get(&task.branch) {
                let pr_line = Paragraph::new(Line::from(vec![
                    Span::styled("\u{e728} ", Style::default().fg(current().magenta)),
                    Span::styled(url.as_str(), Style::default().fg(current().muted)),
                ]));
                f.render_widget(pr_line, area);
                return;
            }
        }
    }
    // Show archived/filter indicator when nothing more important to display.
    if app.status_message.is_none() && app.input_mode == InputMode::Normal {
        let mut spans: Vec<Span> = Vec::new();
        if app.view_archived {
            spans.push(Span::styled(
                "[archived view]",
                Style::default().fg(current().yellow),
            ));
        }
        if !app.search_query.is_empty() {
            if !spans.is_empty() {
                spans.push(Span::raw("  "));
            }
            spans.push(Span::styled(
                format!("filter: {}", app.search_query),
                Style::default().fg(current().cyan),
            ));
        }
        if !spans.is_empty() {
            f.render_widget(Paragraph::new(Line::from(spans)), area);
            return;
        }
    }
    if let Some(msg) = &app.status_message {
        let style = if msg.starts_with("Error") {
            Style::default().fg(current().red)
        } else {
            Style::default().fg(current().yellow)
        };

        let content = if app.op_count > 0 {
            let spinner = SPINNER[app.tick % SPINNER.len()];
            if app.op_count > 1 {
                format!("{spinner} {msg} ({} running)", app.op_count)
            } else {
                format!("{spinner} {msg}")
            }
        } else if matches!(
            app.input_mode,
            InputMode::AddProjectName
                | InputMode::AddSessionName
                | InputMode::AddSessionPrompt
                | InputMode::AddTaskName
                | InputMode::AddTaskBranch
                | InputMode::AddTaskPrompt
                | InputMode::MergeCommitMessage
        ) {
            format!("{}{}", msg, app.input_buffer)
        } else {
            msg.clone()
        };

        let status = Paragraph::new(Span::styled(content, style));
        f.render_widget(status, area);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn truncate_ellipsis_keeps_short_strings() {
        assert_eq!(truncate_ellipsis("feat/auth", 24), "feat/auth");
    }

    #[test]
    fn scroll_offset_no_scroll_when_everything_fits() {
        // 5 rows, viewport of 10: never scrolls regardless of selection.
        assert_eq!(scroll_offset(0, 0, 5, 10), 0);
        assert_eq!(scroll_offset(0, 4, 5, 10), 0);
    }

    #[test]
    fn scroll_offset_pins_selection_to_bottom_when_below_viewport() {
        // 50 rows, viewport of 10. Selecting row 20 scrolls so it's the last visible.
        assert_eq!(scroll_offset(0, 20, 50, 10), 11);
        // Selection already visible in the current window: offset unchanged.
        assert_eq!(scroll_offset(11, 15, 50, 10), 11);
    }

    #[test]
    fn scroll_offset_pins_selection_to_top_when_above_viewport() {
        // Scrolled down to offset 30, then select row 5 (above the window).
        assert_eq!(scroll_offset(30, 5, 50, 10), 5);
    }

    #[test]
    fn scroll_offset_clamps_stale_offset_when_list_shrinks() {
        // Was scrolled to 30 but the list is now short enough to fit; snap back.
        assert_eq!(scroll_offset(30, 2, 8, 10), 0);
        // Near the end: offset never leaves a blank gap past the last row.
        assert_eq!(scroll_offset(99, 49, 50, 10), 40);
    }

    #[test]
    fn truncate_ellipsis_cuts_long_strings_with_marker() {
        let t = truncate_ellipsis("feature/really-long-branch-name-here", 10);
        assert_eq!(t.chars().count(), 10);
        assert!(t.ends_with('…'));
    }

    #[test]
    fn meta_block_pads_every_column_to_its_width() {
        // A fully populated and a fully empty row must span the same width, so
        // columns line up vertically regardless of which cells a row fills.
        let w = ColWidths {
            churn: 10,
            badge: 4,
            branch: 9,
            ..Default::default()
        };
        let full = meta_block(
            churn_spans(1234, 567),
            vec![Span::raw("PR")],
            Some("feat/auth".to_string()),
            None,
            &w,
        );
        let empty = meta_block(Vec::new(), Vec::new(), None, None, &w);
        let expected = w.churn + COL_GAP + w.badge + COL_GAP + w.branch;
        assert_eq!(spans_width(&full), expected);
        assert_eq!(spans_width(&empty), expected);
    }

    #[test]
    fn meta_block_omits_zero_width_columns() {
        // Columns no row populates collapse away (no stray gaps).
        let w = ColWidths {
            churn: 8,
            ..Default::default()
        };
        let only_churn = meta_block(churn_spans(10, 2), Vec::new(), None, None, &w);
        assert_eq!(spans_width(&only_churn), w.churn);
    }

    #[test]
    fn mem_human_formats_kib_compactly() {
        assert_eq!(mem_human(0), "0K");
        assert_eq!(mem_human(512), "512K");
        assert_eq!(mem_human(1024), "1.0M");
        assert_eq!(mem_human(1256), "1.2M");
        // Just under 1 GiB must promote to G, not display as "1024.0M".
        assert_eq!(mem_human(1048575), "1.0G");
        assert_eq!(mem_human(1300 * 1024), "1.3G");
        assert_eq!(mem_human(1600 * 1024 * 1024), "1.6T");
    }

    #[test]
    fn row_line_left_aligns_metadata_after_name_column() {
        let left = vec![Span::raw("  ├─ my-task")]; // 12 cols wide
        let right = vec![Span::raw("+10 -2")]; // 6 cols
        let line = row_line(left, right, 20); // name column padded to 20
        // name_w (20) + COL_GAP + right (6); independent of terminal width.
        assert_eq!(line.width(), 20 + COL_GAP + 6);
    }

    #[test]
    fn row_line_without_metadata_is_just_the_name() {
        let left = vec![Span::raw("  ├─ my-task")];
        let line = row_line(left, Vec::new(), 40);
        assert_eq!(line.width(), spans_width(&[Span::raw("  ├─ my-task")]));
    }
}
