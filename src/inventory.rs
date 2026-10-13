//! Pane-level inventory of the whole tmux server.
//!
//! Unlike [`crate::tmux::list_sessions`], which only sees showrunner's own
//! `cm__project__task__session` sessions, this covers every pane of every
//! session, so plain sessions (`main`, `home`, …) are first-class too. One
//! `tmux list-panes -a` call per tick feeds it.
//!
//! Parsers are pure (fixture-tested); process and tmux I/O is isolated in the
//! thin wrappers [`list_panes`] and [`foreground_argv`].

use std::collections::{HashMap, HashSet};
use std::process::Command;

use crate::agent::AgentKind;
use crate::hooks::AgentEvent;
use crate::tmux::TmuxSession;

/// One tmux pane, as reported by `list-panes -a`.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct Pane {
    /// Pane id, e.g. `%3` — stable for the pane's lifetime, unlike indexes.
    pub id: String,
    pub session: String,
    /// Window id, e.g. `@2`.
    pub window_id: String,
    pub window_index: u32,
    pub window_name: String,
    pub pane_index: u32,
    /// PID of the pane's initial process (usually the shell).
    pub pid: u32,
    /// Active pane of the active window of its session.
    pub active: bool,
    /// Its session has at least one client attached.
    pub attached: bool,
    /// Unix time of the window's last output.
    pub activity: u64,
    /// Foreground process name (`pane_current_command`).
    pub command: String,
    pub cwd: String,
    /// The window's `automatic-rename` option (off once it was named).
    #[serde(default)]
    pub automatic_rename: bool,
    /// `@sessionizer_autoname` is `off`: never auto-name this window.
    #[serde(default)]
    pub autoname_off: bool,
    /// `@sessionizer_lastname`: the name auto-naming last gave the window.
    #[serde(default)]
    pub last_autoname: String,
    /// The session's `@category` (empty when uncategorized).
    #[serde(default)]
    pub category: String,
    /// `window_layout`, as `select-layout` takes it.
    #[serde(default)]
    pub window_layout: String,
    /// Active window of its session (`window_active`).
    #[serde(default)]
    pub window_active: bool,
    /// Active pane of its window (`pane_active`), whatever window is active.
    #[serde(default)]
    pub pane_active: bool,
    /// `@sessionizer_restore` pane option: what a restored pane ran before
    /// (`snapshot::Pending` JSON), until something runs in it.
    #[serde(default)]
    pub pending_restore: String,
    /// Title set by the program in the pane (Claude sets `✳ <session name>`).
    pub title: String,
}

/// What an agent pane needs from the user, ordered by urgency: the `max`
/// over a window's panes is the window's status.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, serde::Serialize, serde::Deserialize,
)]
#[serde(rename_all = "snake_case")]
pub enum AgentStatus {
    Idle,
    Working,
    /// Finished a turn, result not seen yet.
    Done,
    Error,
    Question,
    Permission,
}

impl AgentStatus {
    /// Value of the `@agent_status` tmux window option.
    pub fn code(self) -> &'static str {
        match self {
            AgentStatus::Idle => "idle",
            AgentStatus::Working => "working",
            AgentStatus::Done => "done",
            AgentStatus::Error => "error",
            AgentStatus::Question => "question",
            AgentStatus::Permission => "permission",
        }
    }
}

/// Seconds a finished pane must be on screen before its result counts as read.
const READ_AFTER: u64 = 2;
/// Output within this many seconds counts as "producing output now".
const ACTIVITY_WINDOW: u64 = 2;

/// Per-pane agent state, carried across worker ticks and keyed by pane id.
#[derive(Debug, Clone, Default, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct PaneState {
    /// Agent id (`AgentKind::id`) running in the pane, if any.
    pub agent: Option<String>,
    /// The agent's own session id, learned from its hooks (what `--resume` takes).
    pub agent_session_id: Option<String>,
    /// Most recent hook event for this pane.
    pub last_event: Option<AgentEvent>,
    /// Unix time `last_event` arrived.
    pub last_event_at: Option<u64>,
    /// Derived each tick by [`update_statuses`]; `None` for non-agent panes.
    pub status: Option<AgentStatus>,
    /// The last finished turn has been on screen for [`READ_AFTER`] seconds.
    pub read: bool,
    /// Since when the pane has been continuously on screen.
    #[serde(skip)]
    viewed_since: Option<u64>,
}

impl PaneState {
    /// Record a hook event delivered for this pane. A session end forgets the
    /// agent session id, so a later agent in the same pane can't inherit it.
    /// A permission request while a question is pending is the question's own
    /// prompt (Claude's `AskUserQuestion` also fires `PermissionRequest` and a
    /// `permission_prompt` notification), so it keeps the question.
    pub fn record(&mut self, event: &AgentEvent, session_id: Option<&str>, now: u64) {
        if *event == AgentEvent::PermissionRequest && self.last_event == Some(AgentEvent::Question)
        {
            return;
        }
        if *event == AgentEvent::SessionStopped {
            self.agent_session_id = None;
        } else if let Some(id) = session_id {
            self.agent_session_id = Some(id.to_string());
        }
        if *event == AgentEvent::Finished {
            self.read = false;
            self.viewed_since = None;
        }
        self.last_event = Some(event.clone());
        self.last_event_at = Some(now);
    }

    fn update_view(&mut self, viewing: bool, now: u64) {
        if !viewing {
            self.viewed_since = None;
            return;
        }
        let since = *self.viewed_since.get_or_insert(now);
        if now.saturating_sub(since) >= READ_AFTER {
            self.read = true;
        }
    }

    /// Status from the last hook event. Without one (no hooks, or none yet),
    /// and for agents whose hooks don't report turn starts, recent pane
    /// output means working. `window_activity` is per window, so output in
    /// a sibling pane counts too.
    fn derive_status(&self, pane: &Pane, now: u64) -> Option<AgentStatus> {
        use AgentStatus::*;
        let outputting = now.saturating_sub(pane.activity) <= ACTIVITY_WINDOW;
        let from_event = match &self.last_event {
            None => None,
            Some(AgentEvent::SessionStopped) => return None,
            Some(AgentEvent::PermissionRequest) => Some(Permission),
            Some(AgentEvent::Question | AgentEvent::NeedsInput) => Some(Question),
            Some(AgentEvent::Error(_)) => Some(Error),
            Some(AgentEvent::Working) => Some(Working),
            Some(AgentEvent::Finished) => Some(if self.read { Idle } else { Done }),
            Some(AgentEvent::Idle | AgentEvent::SessionStarted) => Some(Idle),
        };
        let Some(status) = from_event else {
            return self
                .agent
                .is_some()
                .then_some(if outputting { Working } else { Idle });
        };
        let guesses_turns = self
            .agent
            .as_deref()
            .and_then(AgentKind::from_id)
            .is_some_and(|k| !k.reports_turn_start());
        let output_since_event = pane.activity > self.last_event_at.unwrap_or(0);
        if matches!(status, Idle | Done) && guesses_turns && outputting && output_since_event {
            Some(Working)
        } else {
            Some(status)
        }
    }
}

/// Recompute each pane's status (after this tick's hooks were recorded) and
/// return the panes whose status changed, with the new status.
pub fn update_statuses<'a>(
    states: &mut HashMap<String, PaneState>,
    panes: &'a [Pane],
    now: u64,
) -> Vec<(&'a Pane, AgentStatus)> {
    let mut changed = Vec::new();
    for pane in panes {
        let Some(st) = states.get_mut(&pane.id) else {
            continue;
        };
        st.update_view(pane.active && pane.attached, now);
        let status = st.derive_status(pane, now);
        if status != st.status {
            st.status = status;
            if let Some(s) = status {
                changed.push((pane, s));
            }
        }
    }
    changed
}

/// Quiet time as shown in `@agent_idle`: empty under a minute, else the
/// largest whole unit (`12m`, `3h`, `2d`).
pub fn format_idle(secs: u64) -> String {
    match secs {
        0..60 => String::new(),
        60..3600 => format!("{}m", secs / 60),
        3600..86400 => format!("{}h", secs / 3600),
        _ => format!("{}d", secs / 86400),
    }
}

/// `(@agent_status, @agent_idle)` for every window: its most urgent pane
/// status, and how long it has been quiet when nothing is pending (`Idle`
/// or `Done`). Windows without agents get empty values, which clears options
/// left over from a previous daemon run.
pub fn window_options(
    panes: &[Pane],
    states: &HashMap<String, PaneState>,
    now: u64,
) -> HashMap<String, (String, String)> {
    let mut worst: HashMap<&str, (Option<AgentStatus>, u64)> = HashMap::new();
    for pane in panes {
        let entry = worst
            .entry(&pane.window_id)
            .or_insert((None, pane.activity));
        entry.0 = entry.0.max(states.get(&pane.id).and_then(|s| s.status));
    }
    worst
        .into_iter()
        .map(|(window, (status, activity))| {
            let idle = match status {
                Some(AgentStatus::Idle | AgentStatus::Done) => {
                    format_idle(now.saturating_sub(activity))
                }
                _ => String::new(),
            };
            let code = status.map_or("", AgentStatus::code).to_string();
            (window.to_string(), (code, idle))
        })
        .collect()
}

/// `tmux` arguments bringing each window's options from `published` to
/// `wanted`, chained with `;` into one invocation (empty when nothing
/// changed). `published` is updated, and windows that are gone are dropped.
pub fn window_option_args(
    published: &mut HashMap<String, (String, String)>,
    wanted: HashMap<String, (String, String)>,
) -> Vec<String> {
    let live: HashSet<&String> = wanted.keys().collect();
    published.retain(|w, _| live.contains(w));
    let mut args: Vec<String> = Vec::new();
    let mut windows: Vec<_> = wanted.into_iter().collect();
    windows.sort();
    for (window, value) in windows {
        if published.get(&window) == Some(&value) {
            continue;
        }
        for (opt, v) in [("@agent_status", &value.0), ("@agent_idle", &value.1)] {
            if !args.is_empty() {
                args.push(";".into());
            }
            args.extend(["set-option", "-w", "-t", &window, opt, v].map(String::from));
        }
        published.insert(window, value);
    }
    args
}

/// Publish `@agent_status`/`@agent_idle` window options for changed windows
/// (one tmux call, none when nothing changed).
pub fn apply_window_options(
    published: &mut HashMap<String, (String, String)>,
    panes: &[Pane],
    states: &HashMap<String, PaneState>,
    now: u64,
) {
    let args = window_option_args(published, window_options(panes, states, now));
    if !args.is_empty() {
        let _ = Command::new("tmux").args(&args).output();
    }
}

/// `list-panes -F` format matching [`parse_panes`]. The title goes last so a
/// tab inside it can't shift the other fields.
const FORMAT: &str = "#{pane_id}\t#{session_name}\t#{window_id}\t#{window_index}\t\
#{window_name}\t#{pane_index}\t#{pane_pid}\t#{pane_active}\t#{window_active}\t\
#{session_attached}\t#{window_activity}\t#{pane_current_command}\t\
#{pane_current_path}\t#{automatic-rename}\t#{@sessionizer_autoname}\t\
#{@sessionizer_lastname}\t#{@category}\t#{window_layout}\t#{@sessionizer_restore}\t#{pane_title}";

/// Parse `list-panes -a -F FORMAT` output. Malformed lines are skipped.
pub fn parse_panes(stdout: &str) -> Vec<Pane> {
    stdout.lines().filter_map(parse_pane_line).collect()
}

fn parse_pane_line(line: &str) -> Option<Pane> {
    let f: Vec<&str> = line.splitn(20, '\t').collect();
    if f.len() != 20 {
        return None;
    }
    Some(Pane {
        id: f[0].to_string(),
        session: f[1].to_string(),
        window_id: f[2].to_string(),
        window_index: f[3].parse().ok()?,
        window_name: f[4].to_string(),
        pane_index: f[5].parse().ok()?,
        pid: f[6].parse().ok()?,
        active: f[7] == "1" && f[8] == "1",
        pane_active: f[7] == "1",
        window_active: f[8] == "1",
        attached: f[9].parse::<u32>().is_ok_and(|n| n > 0),
        activity: f[10].parse().unwrap_or(0),
        command: f[11].to_string(),
        cwd: f[12].to_string(),
        automatic_rename: f[13] == "1",
        autoname_off: f[14] == "off",
        last_autoname: f[15].to_string(),
        category: f[16].to_string(),
        window_layout: f[17].to_string(),
        pending_restore: f[18].to_string(),
        title: f[19].to_string(),
    })
}

/// Every pane on the tmux server; empty when no server is running.
pub fn list_panes() -> Vec<Pane> {
    Command::new("tmux")
        .args(["list-panes", "-a", "-F", FORMAT])
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| parse_panes(&String::from_utf8_lossy(&o.stdout)))
        .unwrap_or_default()
}

/// The showrunner (`cm__…`) sessions among `panes`, once each, in tmux order.
/// Equivalent to [`crate::tmux::list_sessions`] without another tmux call.
pub fn cm_sessions(panes: &[Pane]) -> Vec<TmuxSession> {
    let mut out: Vec<TmuxSession> = Vec::new();
    for pane in panes {
        if out.last().is_some_and(|s| s.name == pane.session) {
            continue;
        }
        if let Some(s) = TmuxSession::from_tmux_name(&pane.session)
            && !out.iter().any(|o| o.name == s.name)
        {
            out.push(s);
        }
    }
    out
}

/// The agent running in a pane, from its foreground command. Node-wrapped
/// installs (`pane_current_command` is `node`) are resolved from the argv,
/// which `argv` reads only when needed.
pub fn detect_agent(
    command: &str,
    argv: impl FnOnce() -> Option<Vec<String>>,
) -> Option<AgentKind> {
    if let Some(kind) = AgentKind::ALL
        .iter()
        .copied()
        .find(|k| k.process_name() == command)
    {
        return Some(kind);
    }
    if command != "node" {
        return None;
    }
    let argv = argv()?;
    AgentKind::ALL.iter().copied().find(|k| {
        argv.iter().skip(1).any(|arg| {
            let base = arg.rsplit('/').next().unwrap_or(arg);
            base.split('.').next() == Some(k.id())
        })
    })
}

/// Foreground process group of the terminal, from `/proc/<pid>/stat`
/// (field 8, `tpgid`). The command name (field 2) may contain spaces and
/// parens, so fields are counted after its closing paren.
pub fn parse_tpgid(stat: &str) -> Option<u32> {
    let rest = &stat[stat.rfind(')')? + 1..];
    // After ")": state(3) ppid(4) pgrp(5) session(6) tty_nr(7) tpgid(8).
    let tpgid: i64 = rest.split_whitespace().nth(5)?.parse().ok()?;
    u32::try_from(tpgid).ok().filter(|&p| p > 0)
}

/// Split a NUL-separated `/proc/<pid>/cmdline`. `None` when empty (zombie or
/// kernel thread).
pub fn parse_cmdline(raw: &[u8]) -> Option<Vec<String>> {
    let args: Vec<String> = raw
        .split(|&b| b == 0)
        .filter(|a| !a.is_empty())
        .map(|a| String::from_utf8_lossy(a).into_owned())
        .collect();
    (!args.is_empty()).then_some(args)
}

/// Full command line of the pane's foreground process. `None` when the
/// foreground process is the pane's shell itself (an idle prompt) or it can't
/// be read (exited between reads).
pub fn foreground_argv(pane_pid: u32) -> Option<Vec<String>> {
    let stat = std::fs::read_to_string(format!("/proc/{pane_pid}/stat")).ok()?;
    let fg = parse_tpgid(&stat)?;
    if fg == pane_pid {
        return None;
    }
    parse_cmdline(&std::fs::read(format!("/proc/{fg}/cmdline")).ok()?)
}

/// Refresh `states` for the current `panes`: drop vanished panes and
/// re-detect each pane's agent. Hook-learned fields are kept while the same
/// agent keeps running, and reset when it exits or another agent takes over.
/// A shell→agent switch keeps them: the agent's `SessionStart` hook can land
/// before `pane_current_command` changes.
pub fn refresh_states(states: &mut HashMap<String, PaneState>, panes: &[Pane]) {
    states.retain(|id, _| panes.iter().any(|p| &p.id == id));
    for pane in panes {
        let agent =
            detect_agent(&pane.command, || foreground_argv(pane.pid)).map(|k| k.id().to_string());
        let state = states.entry(pane.id.clone()).or_default();
        if state.agent != agent {
            if state.agent.is_some() {
                *state = PaneState::default();
            }
            state.agent = agent;
        }
    }
}

/// The pane a hook event came from: its `$TMUX_PANE` when that pane exists,
/// else the single agent pane whose cwd matches (ambiguous matches resolve to
/// nothing rather than to the wrong pane).
pub fn resolve_hook_pane<'a>(
    panes: &'a [Pane],
    states: &HashMap<String, PaneState>,
    tmux_pane: Option<&str>,
    cwd: Option<&str>,
) -> Option<&'a Pane> {
    if let Some(id) = tmux_pane
        && let Some(p) = panes.iter().find(|p| p.id == id)
    {
        return Some(p);
    }
    let cwd = cwd?;
    let mut matches = panes
        .iter()
        .filter(|p| p.cwd == cwd && states.get(&p.id).is_some_and(|s| s.agent.is_some()));
    let first = matches.next()?;
    matches.next().is_none().then_some(first)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn line(id: &str, session: &str, command: &str, cwd: &str, title: &str) -> String {
        format!(
            "{id}\t{session}\t@1\t0\twin\t0\t100\t1\t1\t1\t1700000000\t{command}\t{cwd}\t1\t\t\t\tL\t\t{title}"
        )
    }

    fn pane(id: &str, session: &str, command: &str, cwd: &str) -> Pane {
        parse_pane_line(&line(id, session, command, cwd, "")).unwrap()
    }

    #[test]
    fn parses_all_fields() {
        let p = parse_pane_line(
            "%3\tgates\t@2\t1\tclaude\t0\t23685\t1\t0\t2\t1791900000\tclaude\t/home/u/gates\t0\toff\tgates\twork\tab12,80x24,0,0,3\t{}\t✳ gates-presentation",
        )
        .unwrap();
        assert_eq!(p.id, "%3");
        assert_eq!(p.session, "gates");
        assert_eq!(p.window_id, "@2");
        assert_eq!(p.window_index, 1);
        assert_eq!(p.pid, 23685);
        // pane_active=1 but window_active=0 → not the visible pane.
        assert!(!p.active);
        assert!(p.attached);
        assert_eq!(p.activity, 1791900000);
        assert_eq!(p.command, "claude");
        assert_eq!(p.cwd, "/home/u/gates");
        assert!(!p.automatic_rename);
        assert!(p.autoname_off);
        assert_eq!(p.last_autoname, "gates");
        assert_eq!(p.category, "work");
        assert_eq!(p.window_layout, "ab12,80x24,0,0,3");
        assert!(p.pane_active && !p.window_active);
        assert_eq!(p.pending_restore, "{}");
        assert_eq!(p.title, "✳ gates-presentation");
    }

    #[test]
    fn tab_in_title_is_kept() {
        let p = parse_pane_line(&line("%1", "s", "zsh", "/", "a\tb")).unwrap();
        assert_eq!(p.title, "a\tb");
    }

    #[test]
    fn malformed_lines_are_skipped() {
        let out = format!(
            "garbage\n{}\n%2\ts\t@1\tX\tw\t0\t1\t1\t1\t0\t0\tzsh\t/\t1\t\t\t\t\t\t",
            line("%1", "s", "zsh", "/", "")
        );
        let panes = parse_panes(&out);
        assert_eq!(panes.len(), 1);
        assert_eq!(panes[0].id, "%1");
    }

    #[test]
    fn cm_sessions_dedups_and_skips_plain_sessions() {
        let panes = vec![
            pane("%1", "cm__proj__task__main", "claude", "/a"),
            pane("%2", "cm__proj__task__main", "zsh", "/a"),
            pane("%3", "main", "btop", "/"),
            pane("%4", "cm__proj__task__1", "claude", "/b"),
        ];
        let names: Vec<String> = cm_sessions(&panes).into_iter().map(|s| s.name).collect();
        assert_eq!(names, ["cm__proj__task__main", "cm__proj__task__1"]);
    }

    #[test]
    fn detect_agent_by_command() {
        assert_eq!(detect_agent("claude", || None), Some(AgentKind::Claude));
        assert_eq!(detect_agent("opencode", || None), Some(AgentKind::OpenCode));
        assert_eq!(
            detect_agent("zsh", || panic!("argv read for non-node")),
            None
        );
    }

    #[test]
    fn detect_agent_node_wrapper_via_argv() {
        let argv = || {
            Some(vec![
                "node".into(),
                "/usr/lib/node_modules/codex/bin/codex.js".into(),
            ])
        };
        assert_eq!(detect_agent("node", argv), Some(AgentKind::Codex));
        let plain = || Some(vec!["node".into(), "server.js".into()]);
        assert_eq!(detect_agent("node", plain), None);
    }

    #[test]
    fn tpgid_survives_spaces_and_parens_in_comm() {
        let stat = "4242 (my (weird) cmd) S 1 4242 4242 34817 5000 4194560 0 0";
        assert_eq!(parse_tpgid(stat), Some(5000));
        // No controlling terminal: tpgid is -1.
        assert_eq!(parse_tpgid("1 (init) S 0 1 1 0 -1 0"), None);
    }

    #[test]
    fn cmdline_splits_on_nul() {
        assert_eq!(
            parse_cmdline(b"claude\0--resume\0home\0"),
            Some(vec!["claude".into(), "--resume".into(), "home".into()])
        );
        assert_eq!(parse_cmdline(b""), None);
    }

    #[test]
    fn refresh_states_keeps_hook_data_while_agent_unchanged() {
        let panes = vec![
            pane("%1", "s", "claude", "/a"),
            pane("%2", "s", "zsh", "/a"),
        ];
        let mut states = HashMap::new();
        refresh_states(&mut states, &panes);
        assert_eq!(states["%1"].agent.as_deref(), Some("claude"));
        assert_eq!(states["%2"].agent, None);

        states.get_mut("%1").unwrap().agent_session_id = Some("abc".into());
        refresh_states(&mut states, &panes);
        assert_eq!(states["%1"].agent_session_id.as_deref(), Some("abc"));

        // Agent exits (back to the shell): learned id is dropped.
        let panes = vec![pane("%1", "s", "zsh", "/a")];
        refresh_states(&mut states, &panes);
        assert_eq!(states["%1"].agent_session_id, None);
        assert!(!states.contains_key("%2"));
    }

    #[test]
    fn refresh_states_keeps_hook_data_that_beat_agent_detection() {
        let mut states = HashMap::new();
        refresh_states(&mut states, &[pane("%1", "s", "zsh", "/a")]);
        states.get_mut("%1").unwrap().agent_session_id = Some("abc".into());
        refresh_states(&mut states, &[pane("%1", "s", "claude", "/a")]);
        assert_eq!(states["%1"].agent.as_deref(), Some("claude"));
        assert_eq!(states["%1"].agent_session_id.as_deref(), Some("abc"));
    }

    #[test]
    fn permission_while_question_pending_keeps_question() {
        let mut st = PaneState::default();
        st.record(&AgentEvent::Question, None, 10);
        st.record(&AgentEvent::PermissionRequest, None, 11);
        assert_eq!(st.last_event, Some(AgentEvent::Question));
        // Once answered (PostToolUse -> Working), a permission counts again.
        st.record(&AgentEvent::Working, None, 12);
        st.record(&AgentEvent::PermissionRequest, None, 13);
        assert_eq!(st.last_event, Some(AgentEvent::PermissionRequest));
    }

    #[test]
    fn record_forgets_session_id_on_stop() {
        let mut st = PaneState::default();
        st.record(&AgentEvent::SessionStarted, Some("abc"), 10);
        st.record(&AgentEvent::Finished, None, 11);
        assert_eq!(st.agent_session_id.as_deref(), Some("abc"));
        assert_eq!(st.last_event_at, Some(11));
        st.record(&AgentEvent::SessionStopped, Some("abc"), 12);
        assert_eq!(st.agent_session_id, None);
        assert_eq!(st.last_event, Some(AgentEvent::SessionStopped));
    }

    fn at(pane_activity: u64) -> Pane {
        let mut p = pane("%1", "s", "claude", "/a");
        p.activity = pane_activity;
        p
    }

    fn state(agent: &str, event: Option<AgentEvent>, at: u64) -> PaneState {
        PaneState {
            agent: Some(agent.into()),
            last_event: event,
            last_event_at: Some(at),
            ..PaneState::default()
        }
    }

    #[test]
    fn status_from_events() {
        use AgentStatus::*;
        let p = at(0);
        let cases = [
            (AgentEvent::PermissionRequest, Permission),
            (AgentEvent::Question, Question),
            (AgentEvent::NeedsInput, Question),
            (AgentEvent::Error("x".into()), Error),
            (AgentEvent::Working, Working),
            (AgentEvent::Finished, Done),
            (AgentEvent::Idle, Idle),
            (AgentEvent::SessionStarted, Idle),
        ];
        for (ev, want) in cases {
            assert_eq!(
                state("claude", Some(ev.clone()), 50).derive_status(&p, 100),
                Some(want),
                "{ev:?}"
            );
        }
        assert_eq!(
            state("claude", Some(AgentEvent::SessionStopped), 50).derive_status(&p, 100),
            None
        );
    }

    #[test]
    fn status_without_events_follows_output() {
        let st = state("claude", None, 0);
        assert_eq!(st.derive_status(&at(99), 100), Some(AgentStatus::Working));
        assert_eq!(st.derive_status(&at(50), 100), Some(AgentStatus::Idle));
        // Not an agent pane, no events: no status.
        assert_eq!(PaneState::default().derive_status(&at(99), 100), None);
    }

    #[test]
    fn output_after_done_means_working_only_for_agents_without_turn_hooks() {
        // Codex only reports finished turns: new output after it is a new turn.
        let codex = state("codex", Some(AgentEvent::Finished), 50);
        assert_eq!(
            codex.derive_status(&at(99), 100),
            Some(AgentStatus::Working)
        );
        assert_eq!(codex.derive_status(&at(40), 100), Some(AgentStatus::Done));
        // Claude reports UserPromptSubmit, so output (e.g. typing) doesn't flip it.
        let claude = state("claude", Some(AgentEvent::Finished), 50);
        assert_eq!(claude.derive_status(&at(99), 100), Some(AgentStatus::Done));
    }

    #[test]
    fn done_becomes_idle_after_being_viewed() {
        let mut p = at(0);
        p.active = true;
        p.attached = true;
        let mut states = HashMap::from([("%1".to_string(), PaneState::default())]);
        states.get_mut("%1").unwrap().agent = Some("claude".into());
        states
            .get_mut("%1")
            .unwrap()
            .record(&AgentEvent::Finished, None, 100);
        let panes = [p];

        let changed = update_statuses(&mut states, &panes, 100);
        assert_eq!(changed.len(), 1);
        assert_eq!(changed[0].1, AgentStatus::Done);
        assert!(update_statuses(&mut states, &panes, 101).is_empty());
        let changed = update_statuses(&mut states, &panes, 102);
        assert_eq!(changed[0].1, AgentStatus::Idle);
        // A new finished turn is unread again.
        states
            .get_mut("%1")
            .unwrap()
            .record(&AgentEvent::Finished, None, 110);
        assert_eq!(
            update_statuses(&mut states, &panes, 110)[0].1,
            AgentStatus::Done
        );
    }

    #[test]
    fn format_idle_units() {
        assert_eq!(format_idle(59), "");
        assert_eq!(format_idle(60 * 12), "12m");
        assert_eq!(format_idle(3600 * 3 + 5), "3h");
        assert_eq!(format_idle(86400 * 2), "2d");
    }

    #[test]
    fn window_takes_most_urgent_pane() {
        let mut a = pane("%1", "s", "claude", "/a");
        let mut b = pane("%2", "s", "claude", "/a");
        let mut c = pane("%3", "s", "zsh", "/a");
        c.window_id = "@9".into();
        a.activity = 0;
        b.activity = 0;
        let states = HashMap::from([
            (
                "%1".to_string(),
                PaneState {
                    status: Some(AgentStatus::Working),
                    ..Default::default()
                },
            ),
            (
                "%2".to_string(),
                PaneState {
                    status: Some(AgentStatus::Permission),
                    ..Default::default()
                },
            ),
        ]);
        let opts = window_options(&[a.clone(), b.clone(), c.clone()], &states, 1000);
        assert_eq!(opts["@1"], ("permission".to_string(), String::new()));
        assert_eq!(opts["@9"], (String::new(), String::new()));

        let states = HashMap::from([(
            "%1".to_string(),
            PaneState {
                status: Some(AgentStatus::Idle),
                ..Default::default()
            },
        )]);
        let opts = window_options(&[a], &states, 600);
        assert_eq!(opts["@1"], ("idle".to_string(), "10m".to_string()));
    }

    #[test]
    fn window_option_args_only_for_changes_in_one_call() {
        let mut published = HashMap::new();
        let wanted = |s: &str| {
            HashMap::from([
                ("@1".to_string(), (s.to_string(), String::new())),
                ("@2".to_string(), (String::new(), String::new())),
            ])
        };
        let args = window_option_args(&mut published, wanted("done"));
        assert_eq!(args.iter().filter(|a| *a == ";").count(), 3);
        assert_eq!(
            &args[..6],
            ["set-option", "-w", "-t", "@1", "@agent_status", "done"]
        );
        // Unchanged: nothing to run.
        assert!(window_option_args(&mut published, wanted("done")).is_empty());
        // One window changed: only its two options.
        let args = window_option_args(&mut published, wanted("idle"));
        assert_eq!(args.len(), 13);
        assert!(args.iter().all(|a| a != "@2"));
        // A vanished window is forgotten.
        window_option_args(&mut published, HashMap::new());
        assert!(published.is_empty());
    }

    #[test]
    fn resolve_hook_pane_prefers_tmux_pane_then_unique_cwd() {
        let panes = vec![
            pane("%1", "s", "claude", "/a"),
            pane("%2", "s", "claude", "/a"),
            pane("%3", "s", "claude", "/b"),
            pane("%4", "s", "zsh", "/b"),
        ];
        let mut states = HashMap::new();
        refresh_states(&mut states, &panes);

        let id = |p: Option<&Pane>| p.map(|p| p.id.clone());
        assert_eq!(
            id(resolve_hook_pane(&panes, &states, Some("%2"), Some("/b"))),
            Some("%2".into())
        );
        // Unknown pane id falls back to cwd; only the agent pane in /b counts.
        assert_eq!(
            id(resolve_hook_pane(&panes, &states, Some("%9"), Some("/b"))),
            Some("%3".into())
        );
        // Two agents share /a: ambiguous → none.
        assert_eq!(
            id(resolve_hook_pane(&panes, &states, None, Some("/a"))),
            None
        );
        assert_eq!(id(resolve_hook_pane(&panes, &states, None, None)), None);
    }
}
