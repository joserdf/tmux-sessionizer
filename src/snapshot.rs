//! Snapshot of every tmux session, window and pane (restored after a reboot,
//! see `crate::restore`) and the history of closed windows and sessions
//! (reopened on demand).
//!
//! Built from the pane inventory each worker tick, so a window closed between
//! two ticks is still in the previous snapshot and lands in the history.

use std::collections::{HashMap, HashSet};
use std::fs;
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use crate::autoclose::Target;

use crate::inventory::{self, Pane, PaneState};

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Snapshot {
    /// Unix time it was taken.
    pub taken_at: u64,
    pub sessions: Vec<SessionSnap>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SessionSnap {
    pub name: String,
    #[serde(default)]
    pub category: String,
    /// `CM_*` session environment of showrunner (`cm__…`) sessions.
    #[serde(default)]
    pub env: Vec<(String, String)>,
    pub windows: Vec<WindowSnap>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct WindowSnap {
    /// Window id when snapshotted (`@3`); only tells closed windows apart.
    pub id: String,
    pub index: u32,
    pub name: String,
    /// Named by tmux, not by the user or auto-naming.
    pub automatic_rename: bool,
    #[serde(default)]
    pub autoname_off: bool,
    #[serde(default)]
    pub last_autoname: String,
    pub layout: String,
    pub active: bool,
    pub panes: Vec<PaneSnap>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PaneSnap {
    pub index: u32,
    pub cwd: String,
    pub active: bool,
    /// Command line of the pane's program; `None` at an idle shell.
    pub argv: Option<Vec<String>>,
    pub agent: Option<AgentResume>,
}

/// What resumes the agent conversation running in a pane.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AgentResume {
    /// `AgentKind::id`.
    pub kind: String,
    pub session_id: Option<String>,
}

/// What a restored pane ran before, kept in its `@sessionizer_restore`
/// option while the pasted command sits unrun, so the next snapshot still
/// has it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Pending {
    pub argv: Option<Vec<String>>,
    pub agent: Option<AgentResume>,
}

/// Build a snapshot from the inventory (`list-panes -a` order: grouped by
/// session, then window). `argv` reads a pane's command line.
pub fn build(
    panes: &[Pane],
    states: &HashMap<String, PaneState>,
    env: &HashMap<String, Vec<(String, String)>>,
    argv: impl Fn(&Pane) -> Option<Vec<String>>,
    redact: &[String],
    now: u64,
) -> Snapshot {
    let mut sessions: Vec<SessionSnap> = Vec::new();
    for pane in panes {
        if sessions.last().is_none_or(|s| s.name != pane.session) {
            sessions.push(SessionSnap {
                name: pane.session.clone(),
                category: pane.category.clone(),
                env: env.get(&pane.session).cloned().unwrap_or_default(),
                windows: Vec::new(),
            });
        }
        let Some(session) = sessions.last_mut() else {
            continue;
        };
        if session
            .windows
            .last()
            .is_none_or(|w| w.id != pane.window_id)
        {
            session.windows.push(WindowSnap {
                id: pane.window_id.clone(),
                index: pane.window_index,
                name: pane.window_name.clone(),
                automatic_rename: pane.automatic_rename,
                autoname_off: pane.autoname_off,
                last_autoname: pane.last_autoname.clone(),
                layout: pane.window_layout.clone(),
                active: pane.window_active,
                panes: Vec::new(),
            });
        }
        let Some(window) = session.windows.last_mut() else {
            continue;
        };
        let state = states.get(&pane.id);
        let mut agent = state.and_then(|s| s.agent.clone()).map(|kind| AgentResume {
            kind,
            session_id: state.and_then(|s| s.agent_session_id.clone()),
        });
        let mut argv = argv(pane).map(|a| redact_argv(&a, redact));
        if argv.is_none()
            && agent.is_none()
            && let Ok(pending) = serde_json::from_str::<Pending>(&pane.pending_restore)
        {
            (argv, agent) = (pending.argv, pending.agent);
        }
        window.panes.push(PaneSnap {
            index: pane.pane_index,
            cwd: pane.cwd.clone(),
            active: pane.pane_active,
            argv,
            agent,
        });
    }
    Snapshot {
        taken_at: now,
        sessions,
    }
}

fn is_shell(arg0: &str) -> bool {
    let name = arg0
        .rsplit('/')
        .next()
        .unwrap_or(arg0)
        .trim_start_matches('-');
    matches!(
        name,
        "sh" | "bash" | "zsh" | "fish" | "dash" | "ksh" | "tcsh" | "csh" | "nu"
    )
}

/// The command line to re-run in a pane: its foreground program's, or the
/// pane's own process when that isn't a shell (a pane started as `claude`).
pub fn pane_argv(pane: &Pane) -> Option<Vec<String>> {
    if let Some(argv) = inventory::foreground_argv(pane.pid) {
        return Some(argv);
    }
    let own = inventory::parse_cmdline(&fs::read(format!("/proc/{}/cmdline", pane.pid)).ok()?)?;
    (!is_shell(&own[0])).then_some(own)
}

/// `argv` without secrets. A `--flag` pattern drops that flag and its value
/// (or `--flag=value`); a `*SUFFIX=` pattern drops `NAME=value` arguments
/// whose NAME ends in SUFFIX.
pub fn redact_argv(argv: &[String], patterns: &[String]) -> Vec<String> {
    let mut out = Vec::new();
    let mut skip_value = false;
    for arg in argv {
        if std::mem::take(&mut skip_value) {
            continue;
        }
        let mut secret = false;
        for p in patterns {
            if let Some(suffix) = p.strip_prefix('*').and_then(|s| s.strip_suffix('=')) {
                secret |= arg
                    .split_once('=')
                    .is_some_and(|(name, _)| name.ends_with(suffix));
            } else if arg == p {
                secret = true;
                skip_value = true;
            } else {
                secret |= arg
                    .strip_prefix(p.as_str())
                    .is_some_and(|r| r.starts_with('='));
            }
        }
        if !secret {
            out.push(arg.clone());
        }
    }
    out
}

// ---- Files ---------------------------------------------------------------

/// Previous snapshots kept as `snapshot.1.json` … `snapshot.5.json`.
const ROTATIONS: usize = 5;
/// Rewrite a snapshot whose structure is unchanged at most this often.
const WRITE_EVERY: u64 = 30;

pub fn snapshot_path() -> PathBuf {
    crate::config::base_dir().join("snapshot.json")
}

/// The `n`-th previous snapshot (`n >= 1`).
pub fn rotated_path(n: usize) -> PathBuf {
    crate::config::base_dir().join(format!("snapshot.{n}.json"))
}

fn closed_path() -> PathBuf {
    crate::config::base_dir().join("closed.json")
}

fn restoring_path() -> PathBuf {
    crate::config::base_dir().join("restoring")
}

/// Write via a temp file + rename, readable only by the user (argv may hold
/// secrets the redaction missed).
fn write_private(path: &Path, content: &str) -> Result<()> {
    if let Some(dir) = path.parent() {
        fs::create_dir_all(dir)?;
    }
    let tmp = path.with_extension("tmp");
    let mut opts = fs::OpenOptions::new();
    opts.write(true).create(true).truncate(true);
    #[cfg(unix)]
    std::os::unix::fs::OpenOptionsExt::mode(&mut opts, 0o600);
    let mut file = opts
        .open(&tmp)
        .with_context(|| format!("writing {}", tmp.display()))?;
    file.write_all(content.as_bytes())?;
    fs::rename(&tmp, path).with_context(|| format!("replacing {}", path.display()))
}

pub fn load(path: &Path) -> Option<Snapshot> {
    serde_json::from_str(&fs::read_to_string(path).ok()?).ok()
}

fn save(snapshot: &Snapshot, rotate: bool) -> Result<()> {
    let path = snapshot_path();
    if rotate && path.exists() {
        for n in (1..ROTATIONS).rev() {
            let _ = fs::rename(rotated_path(n), rotated_path(n + 1));
        }
        let _ = fs::rename(&path, rotated_path(1));
    }
    write_private(&path, &serde_json::to_string_pretty(snapshot)?)
}

/// Held while a restore runs: snapshots of a half-restored server must not
/// replace the one being restored. Removed on drop.
pub struct RestoreLock(());

impl RestoreLock {
    pub fn acquire() -> Result<Self> {
        write_private(&restoring_path(), &std::process::id().to_string())?;
        Ok(Self(()))
    }
}

impl Drop for RestoreLock {
    fn drop(&mut self) {
        let _ = fs::remove_file(restoring_path());
    }
}

/// A restore is running (a lock older than two minutes is stale).
fn restoring() -> bool {
    fs::metadata(restoring_path())
        .and_then(|m| m.modified())
        .is_ok_and(|t| t.elapsed().is_ok_and(|age| age.as_secs() < 120))
}

// ---- Closed history ------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ClosedKind {
    Session,
    Window,
}

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CloseReason {
    Manual,
    AutoClose,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ClosedEntry {
    pub id: u64,
    pub kind: ClosedKind,
    pub reason: CloseReason,
    pub closed_at: u64,
    /// The closed session, or its session holding only the closed window.
    pub session: SessionSnap,
}

const CLOSED_MAX: usize = 100;
const CLOSED_MAX_AGE: u64 = 7 * 86400;

/// Windows of `prev` that are gone from `cur` (by id, so a window moved to
/// another session doesn't count). A session that vanished with all its
/// windows is one `Session` entry; otherwise each window is its own entry.
pub fn closed_between(prev: &Snapshot, cur: &Snapshot) -> Vec<(ClosedKind, SessionSnap)> {
    let live_windows: HashSet<&str> = cur
        .sessions
        .iter()
        .flat_map(|s| s.windows.iter().map(|w| w.id.as_str()))
        .collect();
    let live_sessions: HashSet<&str> = cur.sessions.iter().map(|s| s.name.as_str()).collect();
    let mut out = Vec::new();
    for s in &prev.sessions {
        let gone: Vec<&WindowSnap> = s
            .windows
            .iter()
            .filter(|w| !live_windows.contains(w.id.as_str()))
            .collect();
        if gone.is_empty() {
            continue;
        }
        if gone.len() == s.windows.len() && !live_sessions.contains(s.name.as_str()) {
            out.push((ClosedKind::Session, s.clone()));
            continue;
        }
        for w in gone {
            let mut one = s.clone();
            one.windows = vec![w.clone()];
            out.push((ClosedKind::Window, one));
        }
    }
    out
}

/// Append closed targets to the history, then keep the newest
/// [`CLOSED_MAX`] entries younger than [`CLOSED_MAX_AGE`]. Returns the new
/// entries' ids, in order.
pub fn record_closed(
    history: &mut Vec<ClosedEntry>,
    closed: Vec<(ClosedKind, SessionSnap)>,
    reason: CloseReason,
    now: u64,
) -> Vec<u64> {
    let last = history.iter().map(|e| e.id).max().unwrap_or(0);
    let ids: Vec<u64> = (last + 1..).take(closed.len()).collect();
    for (&id, (kind, session)) in ids.iter().zip(closed) {
        history.push(ClosedEntry {
            id,
            kind,
            reason,
            closed_at: now,
            session,
        });
    }
    history.retain(|e| now.saturating_sub(e.closed_at) < CLOSED_MAX_AGE);
    let excess = history.len().saturating_sub(CLOSED_MAX);
    history.drain(..excess);
    ids
}

/// Take `target` out of `snap`, as a closed-history entry.
fn take_target(snap: &mut Snapshot, target: &Target) -> Option<(ClosedKind, SessionSnap)> {
    match target {
        Target::Session(name) => {
            let i = snap.sessions.iter().position(|s| &s.name == name)?;
            Some((ClosedKind::Session, snap.sessions.remove(i)))
        }
        Target::Window { session, window_id } => {
            let s = snap.sessions.iter_mut().find(|s| &s.name == session)?;
            let i = s.windows.iter().position(|w| &w.id == window_id)?;
            let mut one = s.clone();
            one.windows = vec![s.windows.remove(i)];
            Some((ClosedKind::Window, one))
        }
    }
}

/// `tmux` arguments closing `target`.
fn kill_args(target: &Target) -> Vec<String> {
    match target {
        Target::Session(name) => vec!["kill-session".into(), "-t".into(), format!("={name}")],
        Target::Window { window_id, .. } => {
            vec!["kill-window".into(), "-t".into(), window_id.clone()]
        }
    }
}

pub fn load_closed() -> Vec<ClosedEntry> {
    fs::read_to_string(closed_path())
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default()
}

pub fn save_closed(history: &[ClosedEntry]) -> Result<()> {
    write_private(&closed_path(), &serde_json::to_string_pretty(history)?)
}

// ---- Tracker -------------------------------------------------------------

/// `(session, window indexes)`: a change rotates the snapshot files.
fn structure(s: &Snapshot) -> Vec<(&str, Vec<u32>)> {
    s.sessions
        .iter()
        .map(|s| (s.name.as_str(), s.windows.iter().map(|w| w.index).collect()))
        .collect()
}

#[derive(Debug, PartialEq)]
enum WriteAction {
    Skip,
    Overwrite,
    Rotate,
}

/// Agent session ids in a snapshot, in order.
fn agent_ids(s: &Snapshot) -> Vec<Option<&str>> {
    s.sessions
        .iter()
        .flat_map(|s| &s.windows)
        .flat_map(|w| &w.panes)
        .map(|p| p.agent.as_ref().and_then(|a| a.session_id.as_deref()))
        .collect()
}

/// Rotate on a structural change; otherwise rewrite changed content at most
/// every [`WRITE_EVERY`] seconds, except a new agent session id (what a
/// restore resumes), which is written at once. A tracker's first snapshot is
/// always written, so `taken_at` shows the daemon saw the current tmux server.
fn write_action(written: Option<&Snapshot>, cur: &Snapshot, first: bool) -> WriteAction {
    let Some(written) = written else {
        return WriteAction::Rotate;
    };
    if structure(written) != structure(cur) {
        return WriteAction::Rotate;
    }
    let due = cur.taken_at.saturating_sub(written.taken_at) >= WRITE_EVERY;
    if first || agent_ids(written) != agent_ids(cur) || (due && written.sessions != cur.sessions) {
        WriteAction::Overwrite
    } else {
        WriteAction::Skip
    }
}

/// A snapshot taken now, outside the tracker (the MCP server's `close`).
pub fn take(
    panes: &[Pane],
    states: &HashMap<String, PaneState>,
    redact: &[String],
    now: u64,
) -> Snapshot {
    let mut env = HashMap::new();
    for pane in panes {
        if pane.session.starts_with("cm__") && !env.contains_key(&pane.session) {
            env.insert(pane.session.clone(), session_env(&pane.session));
        }
    }
    build(panes, states, &env, pane_argv, redact, now)
}

/// `CM_*` variables of a session's tmux environment.
fn session_env(session: &str) -> Vec<(String, String)> {
    let Ok(out) = Command::new("tmux")
        .args(["show-environment", "-t", &format!("={session}:")])
        .output()
    else {
        return Vec::new();
    };
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .filter_map(|l| l.split_once('='))
        .filter(|(k, _)| k.starts_with("CM_"))
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect()
}

/// After a daemon restart, agent session ids (learned from hooks) are only
/// in the last snapshot: copy them to the same panes (same window, index,
/// cwd and agent).
fn seed_session_ids(written: &Snapshot, panes: &[Pane], states: &mut HashMap<String, PaneState>) {
    for pane in panes {
        let Some(st) = states.get_mut(&pane.id) else {
            continue;
        };
        if st.agent.is_none() || st.agent_session_id.is_some() {
            continue;
        }
        st.agent_session_id = written
            .sessions
            .iter()
            .filter(|s| s.name == pane.session)
            .flat_map(|s| &s.windows)
            .filter(|w| w.id == pane.window_id)
            .flat_map(|w| &w.panes)
            .find(|p| p.index == pane.pane_index && p.cwd == pane.cwd)
            .and_then(|p| p.agent.as_ref())
            .filter(|a| st.agent.as_ref() == Some(&a.kind))
            .and_then(|a| a.session_id.clone());
    }
}

/// Worker-side state: the previous snapshot (to find closed windows) and the
/// last one written.
#[derive(Default)]
pub struct Tracker {
    prev: Option<Snapshot>,
    written: Option<Snapshot>,
    first: bool,
    env: HashMap<String, Vec<(String, String)>>,
}

impl Tracker {
    pub fn new() -> Self {
        Self {
            written: load(&snapshot_path()),
            first: true,
            ..Self::default()
        }
    }

    /// Snapshot the current inventory: record closed windows and sessions,
    /// and write the snapshot when due. Does nothing without a tmux server
    /// (a reboot or a killed server is what the last snapshot covers) and
    /// while a restore runs.
    pub fn tick(
        &mut self,
        panes: &[Pane],
        states: &mut HashMap<String, PaneState>,
        redact: &[String],
        now: u64,
    ) {
        if panes.is_empty() || restoring() {
            self.prev = None;
            return;
        }
        self.env
            .retain(|name, _| panes.iter().any(|p| &p.session == name));
        for pane in panes {
            if pane.session.starts_with("cm__") && !self.env.contains_key(&pane.session) {
                let env = session_env(&pane.session);
                if !env.is_empty() {
                    self.env.insert(pane.session.clone(), env);
                }
            }
        }
        if self.first
            && let Some(written) = &self.written
        {
            seed_session_ids(written, panes, states);
        }
        // A restored pane ran its pasted command (or another): forget what
        // it ran before the restore.
        for pane in panes.iter().filter(|p| !p.pending_restore.is_empty()) {
            let agent = states.get(&pane.id).is_some_and(|s| s.agent.is_some());
            if agent || pane_argv(pane).is_some() {
                let _ = Command::new("tmux")
                    .args([
                        "set-option",
                        "-p",
                        "-u",
                        "-t",
                        &pane.id,
                        "@sessionizer_restore",
                    ])
                    .output();
            }
        }
        let cur = build(panes, states, &self.env, pane_argv, redact, now);
        if let Some(prev) = &self.prev {
            let closed = closed_between(prev, &cur);
            if !closed.is_empty() {
                let mut history = load_closed();
                let closed = not_yet_recorded(closed, &history, now);
                if !closed.is_empty() {
                    record_closed(&mut history, closed, CloseReason::Manual, now);
                    let _ = save_closed(&history);
                }
            }
        }
        let action = write_action(self.written.as_ref(), &cur, self.first);
        if action != WriteAction::Skip && save(&cur, action == WriteAction::Rotate).is_ok() {
            self.written = Some(cur.clone());
            self.first = false;
        }
        self.prev = Some(cur);
    }

    /// Close `targets`, archived first (see [`archive_and_close`]) from the
    /// last snapshot; a target missing from it (none yet, or a restore is
    /// running) is left open. Taken out of that snapshot, so the next tick
    /// doesn't record them again.
    pub fn close(
        &mut self,
        targets: Vec<Target>,
        reason: CloseReason,
        now: u64,
    ) -> Vec<(Target, u64)> {
        match self.prev.as_mut() {
            Some(prev) => archive_and_close(prev, targets, reason, now),
            None => Vec::new(),
        }
    }
}

/// Close `targets`, each taken out of `snap` and archived in the closed
/// history first so it can always be reopened. A target not in `snap` is left
/// open; one tmux fails to close is dropped from the history again. Returns
/// each closed target with its history id.
pub fn archive_and_close(
    snap: &mut Snapshot,
    targets: Vec<Target>,
    reason: CloseReason,
    now: u64,
) -> Vec<(Target, u64)> {
    let (targets, entries): (Vec<Target>, Vec<_>) = targets
        .into_iter()
        .filter_map(|t| take_target(snap, &t).map(|e| (t, e)))
        .unzip();
    if targets.is_empty() {
        return Vec::new();
    }
    let mut history = load_closed();
    let ids = record_closed(&mut history, entries, reason, now);
    if save_closed(&history).is_err() {
        return Vec::new();
    }
    let mut closed = Vec::new();
    let mut failed = Vec::new();
    for (target, id) in targets.into_iter().zip(ids) {
        let ok = Command::new("tmux")
            .args(kill_args(&target))
            .output()
            .is_ok_and(|o| o.status.success());
        if ok {
            closed.push((target, id));
        } else {
            failed.push(id);
        }
    }
    if !failed.is_empty() {
        history.retain(|e| !failed.contains(&e.id));
        let _ = save_closed(&history);
    }
    closed
}

/// Seconds during which a window archived by another process (an MCP
/// `close`) counts as already recorded when the tracker sees it vanish.
const RECENTLY_ARCHIVED: u64 = 120;

/// `closed` without the windows already archived in `history` within
/// [`RECENTLY_ARCHIVED`] (same session name and window id).
fn not_yet_recorded(
    closed: Vec<(ClosedKind, SessionSnap)>,
    history: &[ClosedEntry],
    now: u64,
) -> Vec<(ClosedKind, SessionSnap)> {
    let recent: HashSet<(&str, &str)> = history
        .iter()
        .filter(|e| now.saturating_sub(e.closed_at) < RECENTLY_ARCHIVED)
        .flat_map(|e| {
            e.session
                .windows
                .iter()
                .map(|w| (e.session.name.as_str(), w.id.as_str()))
        })
        .collect();
    closed
        .into_iter()
        .filter_map(|(kind, mut s)| {
            let name = s.name.clone();
            s.windows
                .retain(|w| !recent.contains(&(name.as_str(), w.id.as_str())));
            (!s.windows.is_empty()).then_some((kind, s))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pane(session: &str, window: &str, index: u32, pane_index: u32, id: &str) -> Pane {
        let mut p = inventory::parse_panes(&format!(
            "{id}\t{session}\t{window}\t{index}\tw\t{pane_index}\t1\t1\t1\t1\t0\tzsh\t/src\t1\t\t\twork\tL\t\t\t"
        ))
        .remove(0);
        p.window_active = index == 0;
        p
    }

    fn snap(panes: &[Pane]) -> Snapshot {
        build(panes, &HashMap::new(), &HashMap::new(), |_| None, &[], 100)
    }

    #[test]
    fn build_groups_sessions_windows_panes() {
        let panes = [
            pane("a", "@1", 0, 0, "%1"),
            pane("a", "@1", 0, 1, "%2"),
            pane("a", "@2", 1, 0, "%3"),
            pane("b", "@3", 0, 0, "%4"),
        ];
        let mut states = HashMap::new();
        let mut st = PaneState::default();
        st.agent = Some("claude".into());
        st.agent_session_id = Some("abc".into());
        states.insert("%2".to_string(), st);
        let argv = |p: &Pane| (p.id == "%2").then(|| vec!["claude".to_string()]);
        let s = build(&panes, &states, &HashMap::new(), argv, &[], 7);
        assert_eq!(s.taken_at, 7);
        assert_eq!(s.sessions.len(), 2);
        let a = &s.sessions[0];
        assert_eq!((a.name.as_str(), a.category.as_str()), ("a", "work"));
        assert_eq!(a.windows.len(), 2);
        assert_eq!(a.windows[0].panes.len(), 2);
        assert!(a.windows[0].active && !a.windows[1].active);
        let p = &a.windows[0].panes[1];
        assert_eq!(p.argv.as_deref(), Some(&["claude".to_string()][..]));
        assert_eq!(
            p.agent,
            Some(AgentResume {
                kind: "claude".into(),
                session_id: Some("abc".into())
            })
        );
        assert_eq!(a.windows[0].panes[0].agent, None);
    }

    #[test]
    fn pending_restore_fills_idle_panes_only() {
        let pending = r#"{"argv":["btop"],"agent":null}"#;
        let mut idle = pane("a", "@1", 0, 0, "%1");
        idle.pending_restore = pending.into();
        let mut busy = pane("a", "@1", 0, 1, "%2");
        busy.pending_restore = pending.into();
        let argv = |p: &Pane| (p.id == "%2").then(|| vec!["vim".to_string()]);
        let s = build(
            &[idle, busy],
            &HashMap::new(),
            &HashMap::new(),
            argv,
            &[],
            1,
        );
        let panes = &s.sessions[0].windows[0].panes;
        assert_eq!(panes[0].argv.as_deref(), Some(&["btop".to_string()][..]));
        assert_eq!(panes[1].argv.as_deref(), Some(&["vim".to_string()][..]));
    }

    #[test]
    fn seeds_session_ids_from_the_same_pane() {
        let panes = [pane("a", "@1", 0, 0, "%1"), pane("a", "@1", 0, 1, "%2")];
        let mut states: HashMap<String, PaneState> = HashMap::new();
        for id in ["%1", "%2"] {
            let mut st = PaneState::default();
            st.agent = Some("claude".into());
            states.insert(id.into(), st);
        }
        let mut written = snap(&panes);
        written.sessions[0].windows[0].panes[0].agent = Some(AgentResume {
            kind: "claude".into(),
            session_id: Some("old".into()),
        });
        written.sessions[0].windows[0].panes[1].agent = Some(AgentResume {
            kind: "codex".into(),
            session_id: Some("other-agent".into()),
        });
        seed_session_ids(&written, &panes, &mut states);
        assert_eq!(states["%1"].agent_session_id.as_deref(), Some("old"));
        assert_eq!(states["%2"].agent_session_id, None);
    }

    #[test]
    fn redact_drops_secret_flags_and_assignments() {
        let argv: Vec<String> = [
            "cmd",
            "--token",
            "s3cr3t",
            "--token=x",
            "--tokenizer",
            "y",
            "API_KEY=k",
            "KEY=ok",
            "--model",
            "m",
        ]
        .map(String::from)
        .into();
        let patterns: Vec<String> = ["--token", "*_KEY="].map(String::from).into();
        assert_eq!(
            redact_argv(&argv, &patterns),
            ["cmd", "--tokenizer", "y", "KEY=ok", "--model", "m"]
        );
    }

    #[test]
    fn closed_windows_and_sessions() {
        let prev = snap(&[
            pane("a", "@1", 0, 0, "%1"),
            pane("a", "@2", 1, 0, "%2"),
            pane("b", "@3", 0, 0, "%3"),
            pane("c", "@4", 0, 0, "%4"),
        ]);
        // @2 closed; session b gone; c's window moved into a new session d.
        let cur = snap(&[pane("a", "@1", 0, 0, "%1"), pane("d", "@4", 0, 0, "%4")]);
        let closed = closed_between(&prev, &cur);
        assert_eq!(closed.len(), 2);
        assert_eq!(closed[0].0, ClosedKind::Window);
        assert_eq!(closed[0].1.name, "a");
        assert_eq!(closed[0].1.windows.len(), 1);
        assert_eq!(closed[0].1.windows[0].id, "@2");
        assert_eq!(closed[1].0, ClosedKind::Session);
        assert_eq!(closed[1].1.name, "b");
        assert!(closed_between(&cur, &cur).is_empty());
    }

    #[test]
    fn record_closed_numbers_and_prunes() {
        let s = snap(&[pane("a", "@1", 0, 0, "%1")]).sessions.remove(0);
        let mut h = Vec::new();
        record_closed(
            &mut h,
            vec![(ClosedKind::Session, s.clone())],
            CloseReason::Manual,
            10,
        );
        let many = (0..CLOSED_MAX)
            .map(|_| (ClosedKind::Window, s.clone()))
            .collect();
        record_closed(&mut h, many, CloseReason::Manual, 20);
        assert_eq!(h.len(), CLOSED_MAX);
        assert_eq!(h[0].id, 2);
        assert_eq!(h.last().unwrap().id, CLOSED_MAX as u64 + 1);
        record_closed(&mut h, Vec::new(), CloseReason::Manual, 20 + CLOSED_MAX_AGE);
        assert!(h.is_empty());
    }

    #[test]
    fn take_target_splits_windows_and_sessions() {
        let panes = [
            pane("a", "@1", 0, 0, "%1"),
            pane("a", "@2", 1, 0, "%2"),
            pane("b", "@3", 0, 0, "%3"),
        ];
        let mut s = snap(&panes);
        let window = Target::Window {
            session: "a".into(),
            window_id: "@2".into(),
        };
        let (kind, one) = take_target(&mut s, &window).unwrap();
        assert_eq!(kind, ClosedKind::Window);
        assert_eq!((one.name.as_str(), one.windows.len()), ("a", 1));
        assert_eq!(one.windows[0].id, "@2");
        assert_eq!(s.sessions[0].windows.len(), 1);
        assert!(take_target(&mut s, &window).is_none());
        let (kind, b) = take_target(&mut s, &Target::Session("b".into())).unwrap();
        assert_eq!((kind, b.name.as_str()), (ClosedKind::Session, "b"));
        assert_eq!(s.sessions.len(), 1);
        // Nothing was closed manually: the next diff records nothing.
        assert!(closed_between(&s, &snap(&panes[..1])).is_empty());
        assert_eq!(kill_args(&window).join(" "), "kill-window -t @2");
        assert_eq!(
            kill_args(&Target::Session("b".into())).join(" "),
            "kill-session -t =b"
        );
    }

    #[test]
    fn recently_archived_windows_are_not_recorded_twice() {
        let panes = [
            pane("a", "@1", 0, 0, "%1"),
            pane("a", "@2", 1, 0, "%2"),
            pane("b", "@3", 0, 0, "%3"),
        ];
        let prev = snap(&panes);
        let mut history = Vec::new();
        let mut archived = prev.sessions[0].clone();
        archived.windows.truncate(1);
        record_closed(
            &mut history,
            vec![(ClosedKind::Window, archived)],
            CloseReason::Manual,
            100,
        );
        // @1 (archived) and the whole of b vanished.
        let gone = closed_between(&prev, &snap(&panes[1..2]));
        let left = not_yet_recorded(gone.clone(), &history, 150);
        let names: Vec<_> = left
            .iter()
            .map(|(k, s)| (*k, s.name.as_str(), s.windows[0].id.as_str()))
            .collect();
        assert_eq!(names, [(ClosedKind::Session, "b", "@3")]);
        // Long after, the same ids (a new tmux server) are new closes.
        assert_eq!(
            not_yet_recorded(gone, &history, 100 + RECENTLY_ARCHIVED).len(),
            2
        );
    }

    #[test]
    fn write_action_rules() {
        let one = snap(&[pane("a", "@1", 0, 0, "%1")]);
        let two = snap(&[pane("a", "@1", 0, 0, "%1"), pane("a", "@2", 1, 0, "%2")]);
        assert_eq!(write_action(None, &one, false), WriteAction::Rotate);
        assert_eq!(write_action(Some(&one), &two, false), WriteAction::Rotate);
        assert_eq!(write_action(Some(&one), &one, true), WriteAction::Overwrite);
        assert_eq!(write_action(Some(&one), &one, false), WriteAction::Skip);
        let mut changed = one.clone();
        changed.sessions[0].windows[0].panes[0].cwd = "/elsewhere".into();
        assert_eq!(write_action(Some(&one), &changed, false), WriteAction::Skip);
        changed.taken_at += WRITE_EVERY;
        assert_eq!(
            write_action(Some(&one), &changed, false),
            WriteAction::Overwrite
        );
        let mut resumed = one.clone();
        resumed.sessions[0].windows[0].panes[0].agent = Some(AgentResume {
            kind: "claude".into(),
            session_id: Some("new".into()),
        });
        assert_eq!(
            write_action(Some(&one), &resumed, false),
            WriteAction::Overwrite
        );
    }

    #[test]
    fn shells_are_not_commands() {
        assert!(is_shell("-zsh") && is_shell("/bin/bash") && is_shell("zsh"));
        assert!(!is_shell("claude") && !is_shell("/usr/bin/btop"));
    }
}
