//! Suspend coding agents left idle for a long time: stop the agent process
//! and paste the command that resumes the same conversation at the shell
//! prompt, never run. The window and every other process stay.

use std::collections::{HashMap, HashSet};
use std::process::Command;
use std::thread::sleep;
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};

use crate::agent::AgentKind;
use crate::inventory::{self, AgentStatus, Pane, PaneState};
use crate::restore::{self, Paste};
use crate::snapshot::{self, AgentResume, PaneSnap, Pending};

fn def_idle_secs() -> u64 {
    7 * 86400
}
fn def_exclude_categories() -> Vec<String> {
    vec!["pinned".to_string()]
}
fn def_respect_dirty() -> bool {
    true
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SuspendConfig {
    #[serde(default)]
    pub enabled: bool,
    /// Seconds without output before an idle agent is suspended.
    #[serde(default = "def_idle_secs")]
    pub idle_secs: u64,
    /// Sessions in these categories are never suspended.
    #[serde(default = "def_exclude_categories")]
    pub exclude_categories: Vec<String>,
    /// Leave agents whose cwd has uncommitted git changes running.
    #[serde(default = "def_respect_dirty")]
    pub respect_dirty: bool,
}

impl Default for SuspendConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            idle_secs: def_idle_secs(),
            exclude_categories: def_exclude_categories(),
            respect_dirty: def_respect_dirty(),
        }
    }
}

/// Agent panes to suspend now, with the agent and its session id. An agent
/// qualifies when it is idle with its last result read (not working, waiting
/// on the user, errored or done-unread), its window has been quiet for
/// `idle_secs`, the conversation can be resumed (a known session id, not Pi),
/// and the pane is not on screen, kept (`@sessionizer_keep`), in an excluded
/// category or holding an unrun restored command.
pub fn candidates<'a>(
    cfg: &SuspendConfig,
    panes: &'a [Pane],
    states: &HashMap<String, PaneState>,
    now: u64,
) -> Vec<(&'a Pane, AgentKind, String)> {
    if !cfg.enabled {
        return Vec::new();
    }
    panes
        .iter()
        .filter(|p| {
            now.saturating_sub(p.activity) >= cfg.idle_secs
                && !(p.active && p.attached)
                && !p.keep
                && p.pending_restore.is_empty()
                && !cfg.exclude_categories.contains(&p.category)
        })
        .filter_map(|p| {
            let st = states.get(&p.id)?;
            let kind = AgentKind::from_id(st.agent.as_deref()?)?;
            let id = st.agent_session_id.clone()?;
            (st.status == Some(AgentStatus::Idle) && kind != AgentKind::Pi).then_some((p, kind, id))
        })
        .collect()
}

/// Uncommitted changes in `cwd`'s git work tree. Outside a repository there
/// is nothing to lose; a status that can't be read counts as dirty.
fn dirty(cwd: &str) -> bool {
    let inside = Command::new("git")
        .args(["-C", cwd, "rev-parse", "--is-inside-work-tree"])
        .output()
        .is_ok_and(|o| o.status.success());
    inside && crate::tmux::worktree_dirty_failsafe(cwd)
}

/// How long a stopped agent has to give the prompt back.
const STOP_WAIT: Duration = Duration::from_secs(10);

/// Stop the agent in `pane`, leaving a shell at its prompt. An agent started
/// under the pane's shell gets SIGTERM (its process group only, so other
/// jobs keep running); an agent that is the pane's own process is replaced
/// by a fresh shell in the same cwd.
fn stop_agent(pane: &Pane) -> bool {
    let fg = std::fs::read_to_string(format!("/proc/{}/stat", pane.pid))
        .ok()
        .and_then(|s| inventory::parse_tpgid(&s));
    match fg {
        None => false,
        Some(fg) if fg == pane.pid => {
            // Without a command, respawn-pane would rerun the agent.
            let Some(shell) = Command::new("tmux")
                .args(["show-options", "-gv", "default-shell"])
                .output()
                .ok()
                .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
                .filter(|s| !s.is_empty())
            else {
                return false;
            };
            Command::new("tmux")
                .args([
                    "respawn-pane",
                    "-k",
                    "-t",
                    &pane.id,
                    "-c",
                    &pane.cwd,
                    &shell,
                ])
                .output()
                .is_ok_and(|o| o.status.success())
        }
        Some(fg) => {
            let _ = Command::new("kill")
                .args(["-TERM", "--", &format!("-{fg}")])
                .output();
            let start = Instant::now();
            while start.elapsed() < STOP_WAIT {
                if inventory::foreground_argv(pane.pid).is_none() {
                    return true;
                }
                sleep(Duration::from_millis(200));
            }
            false
        }
    }
}

/// Suspend one agent: stop it, mark the pane with what it ran (so snapshots
/// keep it and it isn't suspended again) and paste the resume command.
/// Returns the pasted command.
pub fn suspend(
    pane: &Pane,
    kind: AgentKind,
    session_id: &str,
    redact: &[String],
) -> Option<String> {
    let argv = snapshot::pane_argv(pane).map(|a| snapshot::redact_argv(&a, redact));
    let agent = AgentResume {
        kind: kind.id().to_string(),
        session_id: Some(session_id.to_string()),
    };
    let snap = PaneSnap {
        index: pane.pane_index,
        cwd: pane.cwd.clone(),
        active: pane.pane_active,
        argv: argv.clone(),
        agent: Some(agent.clone()),
    };
    let (text, _) = restore::pane_command(&snap)?;
    let pending = serde_json::to_string(&Pending {
        argv,
        agent: Some(agent),
    })
    .ok()?;
    if !stop_agent(pane) {
        return None;
    }
    let _ = Command::new("tmux")
        .args([
            "set-option",
            "-p",
            "-t",
            &pane.id,
            "@sessionizer_restore",
            &pending,
        ])
        .output();
    restore::paste_when_ready(vec![Paste {
        pane: pane.id.clone(),
        text: text.clone(),
        submit: false,
    }]);
    Some(text)
}

/// Worker-side state: panes already tried, so an agent that won't stop (or
/// sits in a dirty tree) isn't retried until it does something again.
#[derive(Default)]
pub struct Suspender {
    tried: HashSet<String>,
}

impl Suspender {
    /// Suspend every candidate not tried yet and notify about the ones that
    /// were. Blocks while agents stop (up to [`STOP_WAIT`] each), so the
    /// caller runs it on a slow cadence.
    pub fn step(
        &mut self,
        cfg: &SuspendConfig,
        panes: &[Pane],
        states: &HashMap<String, PaneState>,
        redact: &[String],
        now: u64,
    ) {
        let found = candidates(cfg, panes, states, now);
        self.tried
            .retain(|id| found.iter().any(|(p, ..)| &p.id == id));
        let mut done = Vec::new();
        for (pane, kind, session_id) in found {
            if !self.tried.insert(pane.id.clone()) || (cfg.respect_dirty && dirty(&pane.cwd)) {
                continue;
            }
            if let Some(text) = suspend(pane, kind, &session_id, redact) {
                done.push(format!(
                    "{}:{} {}: {text}",
                    pane.session, pane.window_index, pane.window_name
                ));
            }
        }
        if !done.is_empty() {
            crate::notify::send(&crate::notify::Notification {
                title: "showrunner: idle agents suspended".to_string(),
                body: done.join("\n"),
                urgent: false,
            });
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const NOW: u64 = 1_000_000;
    const WEEK: u64 = 7 * 86400;

    /// A pane quiet for `quiet` seconds, not on screen.
    fn pane(id: &str, quiet: u64) -> Pane {
        inventory::parse_panes(&format!(
            "{id}\ts\t@1\t0\tw\t0\t1\t0\t0\t0\t{}\tclaude\t/\t1\t\t\t\tL\t\t\t",
            NOW - quiet
        ))
        .remove(0)
    }

    fn agent(kind: &str, status: AgentStatus, session_id: Option<&str>) -> PaneState {
        let mut st = PaneState::default();
        st.agent = Some(kind.into());
        st.status = Some(status);
        st.agent_session_id = session_id.map(String::from);
        st
    }

    fn on() -> SuspendConfig {
        SuspendConfig {
            enabled: true,
            ..SuspendConfig::default()
        }
    }

    fn ids(cfg: &SuspendConfig, panes: &[Pane], states: &[(&str, PaneState)]) -> Vec<String> {
        let states: HashMap<String, PaneState> = states
            .iter()
            .map(|(id, st)| (id.to_string(), st.clone()))
            .collect();
        candidates(cfg, panes, &states, NOW)
            .into_iter()
            .map(|(p, kind, id)| format!("{} {} {id}", p.id, kind.id()))
            .collect()
    }

    #[test]
    fn only_idle_resumable_agents_quiet_for_the_timeout() {
        let panes = [
            pane("%1", WEEK),
            pane("%2", WEEK - 60),
            pane("%3", WEEK),
            pane("%4", WEEK),
            pane("%5", WEEK),
            pane("%6", WEEK),
        ];
        let states = [
            ("%1", agent("claude", AgentStatus::Idle, Some("a"))),
            ("%2", agent("claude", AgentStatus::Idle, Some("b"))),
            ("%3", agent("claude", AgentStatus::Idle, None)),
            ("%4", agent("pi", AgentStatus::Idle, Some("d"))),
            ("%5", agent("opencode", AgentStatus::Idle, Some("e"))),
            // %6 is a shell: no state.
        ];
        assert_eq!(
            ids(&on(), &panes, &states),
            ["%1 claude a", "%5 opencode e"]
        );
        assert!(ids(&SuspendConfig::default(), &panes, &states).is_empty());
    }

    #[test]
    fn busy_waiting_unread_or_errored_agents_stay() {
        for status in [
            AgentStatus::Working,
            AgentStatus::Question,
            AgentStatus::Permission,
            AgentStatus::Done,
            AgentStatus::Error,
        ] {
            let states = [("%1", agent("claude", status, Some("a")))];
            assert!(
                ids(&on(), &[pane("%1", WEEK)], &states).is_empty(),
                "{status:?}"
            );
        }
    }

    #[test]
    fn viewed_kept_excluded_and_pending_panes_stay() {
        let states = [("%1", agent("claude", AgentStatus::Idle, Some("a")))];
        let base = pane("%1", WEEK);
        let mut viewed = base.clone();
        (viewed.active, viewed.attached) = (true, true);
        let mut kept = base.clone();
        kept.keep = true;
        let mut pinned = base.clone();
        pinned.category = "pinned".into();
        let mut pending = base.clone();
        pending.pending_restore = "{}".into();
        for p in [viewed, kept, pinned, pending] {
            assert!(ids(&on(), &[p.clone()], &states).is_empty(), "{p:?}");
        }
        // Attached, but another window is on screen.
        let mut background = base;
        background.attached = true;
        assert_eq!(ids(&on(), &[background], &states).len(), 1);
    }

    #[test]
    fn partial_table_parses_with_defaults() {
        let cfg: SuspendConfig = toml::from_str("enabled = true").unwrap();
        assert_eq!(cfg, on());
        assert_eq!(cfg.idle_secs, WEEK);
        let cfg: SuspendConfig =
            toml::from_str("idle_secs = 60\nexclude_categories = [\"system\"]").unwrap();
        assert!(!cfg.enabled);
        assert_eq!(cfg.exclude_categories, ["system"]);
    }
}
