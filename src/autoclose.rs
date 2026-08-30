//! Auto-close policy engine.
//!
//! Pure decision logic: given the configuration and the observed state of a
//! session, decide whether nothing should happen, the session is safe to
//! close, or a destructive close needs user confirmation. No IO is performed
//! here; callers observe state and act on the returned [`CloseAction`].

use serde::{Deserialize, Serialize};

/// Outcome of the auto-close evaluation for a session.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CloseAction {
    /// Do nothing.
    None,
    /// Safe to close the session now.
    Close,
    /// Closing would be destructive (dirty worktree); surface to the user
    /// for confirmation.
    Confirm,
}

/// Policy settings that control when a session may be auto-closed.
/// Auto-close is destructive, so it defaults to OFF. The other knobs are set to
/// the documented policy so that turning `enabled` on is safe. These fns are
/// the single source of truth for the per-field serde defaults, so a *partial*
/// `[auto_close]` table (e.g. just `enabled = true`) deserializes instead of
/// breaking the whole config file.
fn def_enabled() -> bool {
    false
}
fn def_idle_secs() -> Option<u64> {
    Some(900)
}
fn def_close_on_finish() -> bool {
    true
}
fn def_respect_dirty() -> bool {
    true
}

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct AutoCloseConfig {
    /// Master switch. When false, [`evaluate`] always returns
    /// [`CloseAction::None`].
    #[serde(default = "def_enabled")]
    pub enabled: bool,
    /// Idle timeout in seconds before auto-close; `None` disables the idle
    /// trigger.
    #[serde(default = "def_idle_secs")]
    pub idle_secs: Option<u64>,
    /// Close when the agent has finished.
    #[serde(default = "def_close_on_finish")]
    pub close_on_finish: bool,
    /// When true, a session with uncommitted changes yields
    /// [`CloseAction::Confirm`] instead of a silent [`CloseAction::Close`].
    #[serde(default = "def_respect_dirty")]
    pub respect_dirty: bool,
}

impl Default for AutoCloseConfig {
    fn default() -> Self {
        Self {
            enabled: def_enabled(),
            idle_secs: def_idle_secs(),
            close_on_finish: def_close_on_finish(),
            respect_dirty: def_respect_dirty(),
        }
    }
}

/// Observed state of a session, as seen by the caller.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct SessionCloseState {
    /// The agent has finished its work.
    pub finished: bool,
    /// The session has been idle past the configured timeout.
    pub idle: bool,
    /// The worktree has uncommitted changes.
    pub has_uncommitted_changes: bool,
}

/// Decide what to do with a session under the given policy.
///
/// Rules, in priority order:
/// 1. Disabled policy never closes.
/// 2. A dirty session with `respect_dirty` set always needs confirmation.
/// 3. A finished session closes when `close_on_finish` is set.
/// 4. An idle session closes when an idle timeout is configured.
/// 5. Otherwise, nothing happens.
pub fn evaluate(cfg: &AutoCloseConfig, st: &SessionCloseState) -> CloseAction {
    if !cfg.enabled {
        return CloseAction::None;
    }
    if st.has_uncommitted_changes && cfg.respect_dirty {
        return CloseAction::Confirm;
    }
    if st.finished && cfg.close_on_finish {
        return CloseAction::Close;
    }
    if st.idle && cfg.idle_secs.is_some() {
        return CloseAction::Close;
    }
    CloseAction::None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config(
        enabled: bool,
        idle_secs: Option<u64>,
        close_on_finish: bool,
        respect_dirty: bool,
    ) -> AutoCloseConfig {
        AutoCloseConfig {
            enabled,
            idle_secs,
            close_on_finish,
            respect_dirty,
        }
    }

    fn state(finished: bool, idle: bool, dirty: bool) -> SessionCloseState {
        SessionCloseState {
            finished,
            idle,
            has_uncommitted_changes: dirty,
        }
    }

    #[test]
    fn disabled_never_closes_even_when_finished_idle_and_dirty() {
        let cfg = config(false, Some(900), true, true);
        let st = state(true, true, true);
        assert_eq!(evaluate(&cfg, &st), CloseAction::None);
    }

    #[test]
    fn dirty_with_respect_dirty_confirms_even_when_finished() {
        let cfg = config(true, Some(900), true, true);
        let st = state(true, false, true);
        assert_eq!(evaluate(&cfg, &st), CloseAction::Confirm);
    }

    #[test]
    fn dirty_without_respect_dirty_closes_when_finished() {
        let cfg = config(true, None, true, false);
        let st = state(true, false, true);
        assert_eq!(evaluate(&cfg, &st), CloseAction::Close);
    }

    #[test]
    fn dirty_without_respect_dirty_closes_when_idle() {
        let cfg = config(true, Some(900), true, false);
        let st = state(false, true, true);
        assert_eq!(evaluate(&cfg, &st), CloseAction::Close);
    }

    #[test]
    fn finished_clean_closes_when_close_on_finish() {
        let cfg = config(true, None, true, true);
        let st = state(true, false, false);
        assert_eq!(evaluate(&cfg, &st), CloseAction::Close);
    }

    #[test]
    fn finished_without_close_on_finish_falls_through_to_idle() {
        let cfg = config(true, Some(900), false, true);
        let st = state(true, true, false);
        assert_eq!(evaluate(&cfg, &st), CloseAction::Close);
    }

    #[test]
    fn finished_without_close_on_finish_and_not_idle_does_nothing() {
        let cfg = config(true, Some(900), false, true);
        let st = state(true, false, false);
        assert_eq!(evaluate(&cfg, &st), CloseAction::None);
    }

    #[test]
    fn idle_without_timeout_does_nothing_when_not_finished() {
        let cfg = config(true, None, true, true);
        let st = state(false, true, false);
        assert_eq!(evaluate(&cfg, &st), CloseAction::None);
    }

    #[test]
    fn not_finished_and_not_idle_does_nothing() {
        let cfg = config(true, Some(900), true, true);
        let st = state(false, false, false);
        assert_eq!(evaluate(&cfg, &st), CloseAction::None);
    }

    #[test]
    fn default_config_matches_spec() {
        let cfg = AutoCloseConfig::default();
        // Auto-close is opt-in: the default is disabled.
        assert_eq!(
            cfg,
            config(false, Some(900), true, true)
        );
    }

    #[test]
    fn partial_table_deserializes_with_defaults() {
        // A user who writes only `[auto_close]\nenabled = true` must not break
        // the whole config file: the other fields fall back to their defaults.
        let cfg: AutoCloseConfig =
            serde_json::from_str(r#"{"enabled": true}"#).expect("partial config parses");
        assert_eq!(cfg, config(true, Some(900), true, true));
        // And an empty object is fully default (disabled).
        let cfg: AutoCloseConfig = serde_json::from_str("{}").unwrap();
        assert_eq!(cfg, config(false, Some(900), true, true));
    }
}
