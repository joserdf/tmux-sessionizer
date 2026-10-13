//! Pane-level inventory of the whole tmux server.
//!
//! Unlike [`crate::tmux::list_sessions`], which only sees showrunner's own
//! `cm__project__task__session` sessions, this covers every pane of every
//! session, so plain sessions (`main`, `home`, …) are first-class too. One
//! `tmux list-panes -a` call per tick feeds it.
//!
//! Parsers are pure (fixture-tested); process and tmux I/O is isolated in the
//! thin wrappers [`list_panes`] and [`foreground_argv`].

use std::collections::HashMap;
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
    /// Title set by the program in the pane (Claude sets `✳ <session name>`).
    pub title: String,
}

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
}

impl PaneState {
    /// Record a hook event delivered for this pane. A session end forgets the
    /// agent session id, so a later agent in the same pane can't inherit it.
    pub fn record(&mut self, event: &AgentEvent, session_id: Option<&str>, now: u64) {
        if *event == AgentEvent::SessionStopped {
            self.agent_session_id = None;
        } else if let Some(id) = session_id {
            self.agent_session_id = Some(id.to_string());
        }
        self.last_event = Some(event.clone());
        self.last_event_at = Some(now);
    }
}

/// `list-panes -F` format matching [`parse_panes`]. The title goes last so a
/// tab inside it can't shift the other fields.
const FORMAT: &str = "#{pane_id}\t#{session_name}\t#{window_id}\t#{window_index}\t\
#{window_name}\t#{pane_index}\t#{pane_pid}\t#{pane_active}\t#{window_active}\t\
#{session_attached}\t#{window_activity}\t#{pane_current_command}\t\
#{pane_current_path}\t#{pane_title}";

/// Parse `list-panes -a -F FORMAT` output. Malformed lines are skipped.
pub fn parse_panes(stdout: &str) -> Vec<Pane> {
    stdout.lines().filter_map(parse_pane_line).collect()
}

fn parse_pane_line(line: &str) -> Option<Pane> {
    let f: Vec<&str> = line.splitn(14, '\t').collect();
    if f.len() != 14 {
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
        attached: f[9].parse::<u32>().is_ok_and(|n| n > 0),
        activity: f[10].parse().unwrap_or(0),
        command: f[11].to_string(),
        cwd: f[12].to_string(),
        title: f[13].to_string(),
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
            "{id}\t{session}\t@1\t0\twin\t0\t100\t1\t1\t1\t1700000000\t{command}\t{cwd}\t{title}"
        )
    }

    fn pane(id: &str, session: &str, command: &str, cwd: &str) -> Pane {
        parse_pane_line(&line(id, session, command, cwd, "")).unwrap()
    }

    #[test]
    fn parses_all_fields() {
        let p = parse_pane_line(
            "%3\tgates\t@2\t1\tclaude\t0\t23685\t1\t0\t2\t1791900000\tclaude\t/home/u/gates\t✳ gates-presentation",
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
            "garbage\n{}\n%2\ts\t@1\tX\tw\t0\t1\t1\t1\t0\t0\tzsh\t/\t",
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
