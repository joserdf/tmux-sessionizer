//! Name agent windows after the agent's session title.
//!
//! Claude Code sets its pane title to `✳ <session name>` (a spinner glyph
//! while working) while tmux auto-names the window `claude`. The daemon
//! renames such windows to the cleaned title, and stops for good once the
//! user renames a window by hand.

use std::collections::HashMap;
use std::process::Command;

use serde::{Deserialize, Serialize};

use crate::inventory::{Pane, PaneState};

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct NamingConfig {
    #[serde(default = "def_enabled")]
    pub enabled: bool,
    /// Longest window name, in characters (longer titles end in `…`).
    #[serde(default = "def_max_len")]
    pub max_len: usize,
}

fn def_enabled() -> bool {
    true
}

fn def_max_len() -> usize {
    32
}

impl Default for NamingConfig {
    fn default() -> Self {
        Self {
            enabled: def_enabled(),
            max_len: def_max_len(),
        }
    }
}

/// Titles that say nothing about the session (agents' defaults; tmux's
/// default pane title is the hostname).
const GENERIC: &[&str] = &["claude", "claude code", "opencode", "codex", "pi"];

fn is_glyph(c: char) -> bool {
    matches!(
        c,
        '\u{2800}'..='\u{28FF}' | '✳' | '✻' | '✢' | '✶' | '✽' | '⬡' | '⬢' | '·'
    ) || c.is_whitespace()
}

/// The window name for a pane title: leading spinner/glyph characters and
/// control characters removed, `None` for generic titles, truncated to
/// `max_len` characters. A trailing `;` is dropped because tmux would take it
/// as a command separator.
pub fn clean_title(raw: &str, max_len: usize, host: &str) -> Option<String> {
    let text: String = raw.chars().filter(|c| !c.is_control()).collect();
    let text = text
        .trim_start_matches(is_glyph)
        .trim_end()
        .trim_end_matches(';')
        .trim_end();
    let lower = text.to_lowercase();
    if text.is_empty() || GENERIC.contains(&lower.as_str()) || lower == host.to_lowercase() {
        return None;
    }
    if text.chars().count() <= max_len {
        return Some(text.to_string());
    }
    let mut cut: String = text.chars().take(max_len.saturating_sub(1)).collect();
    cut.truncate(cut.trim_end().len());
    cut.push('…');
    Some(cut)
}

/// What to do with one window.
#[derive(Debug, PartialEq)]
enum Action {
    Rename(String),
    /// The user named the window: stop auto-naming it.
    Disable,
}

fn action(window: &Pane, title: Option<String>) -> Option<Action> {
    if window.autoname_off {
        return None;
    }
    let renamed_by_user = if window.last_autoname.is_empty() {
        // Never named by us; automatic-rename off means it was named by hand.
        !window.automatic_rename
    } else {
        window.window_name != window.last_autoname
    };
    if renamed_by_user {
        return Some(Action::Disable);
    }
    title
        .filter(|t| *t != window.window_name)
        .map(Action::Rename)
}

/// `tmux` arguments renaming every agent window whose title changed, chained
/// with `;` into one invocation (empty when nothing to do). With several agent
/// panes in a window, the active pane's title wins, then the first one with a
/// usable title.
pub fn rename_args(
    panes: &[Pane],
    states: &HashMap<String, PaneState>,
    cfg: &NamingConfig,
    host: &str,
) -> Vec<String> {
    // Per window: the pane naming it and its cleaned title.
    let mut windows: Vec<(&Pane, Option<String>)> = Vec::new();
    for pane in panes {
        if states.get(&pane.id).is_none_or(|s| s.agent.is_none()) {
            continue;
        }
        let title = clean_title(&pane.title, cfg.max_len, host);
        match windows.iter_mut().find(|w| w.0.window_id == pane.window_id) {
            Some(w) => {
                if title.is_some() && (pane.active || (w.1.is_none() && !w.0.active)) {
                    *w = (pane, title);
                }
            }
            None => windows.push((pane, title)),
        }
    }
    let mut args: Vec<String> = Vec::new();
    for (pane, title) in windows {
        let window = &pane.window_id;
        let action = action(pane, title);
        let cmd: Vec<&str> = match &action {
            None => continue,
            Some(Action::Disable) => {
                vec![
                    "set-option",
                    "-w",
                    "-t",
                    window,
                    "@sessionizer_autoname",
                    "off",
                ]
            }
            // rename-window also turns automatic-rename off for the window.
            Some(Action::Rename(name)) => vec![
                "rename-window",
                "-t",
                window,
                "--",
                name,
                ";",
                "set-option",
                "-w",
                "-t",
                window,
                "@sessionizer_lastname",
                name,
            ],
        };
        if !args.is_empty() {
            args.push(";".into());
        }
        args.extend(cmd.into_iter().map(String::from));
    }
    args
}

/// Rename agent windows after their session titles (one tmux call, none when
/// nothing changed).
pub fn apply_names(
    panes: &[Pane],
    states: &HashMap<String, PaneState>,
    cfg: &NamingConfig,
    host: &str,
) {
    if !cfg.enabled {
        return;
    }
    let args = rename_args(panes, states, cfg, host);
    if !args.is_empty() {
        let _ = Command::new("tmux").args(&args).output();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn clean_title_table() {
        let cases: &[(&str, Option<&str>)] = &[
            ("✳ gates-presentation", Some("gates-presentation")),
            (
                "✳ Pesquisa sobre brmodelos",
                Some("Pesquisa sobre brmodelos"),
            ),
            ("⠐ gates", Some("gates")),
            ("✻ · home  ", Some("home")),
            ("✳ Claude Code", None),
            ("claude", None),
            ("OpenCode", None),
            ("✳", None),
            ("", None),
            ("myhost", None),
            ("MyHost", None),
            ("✳ fix bug;", Some("fix bug")),
            ("✳ a\x07b", Some("ab")),
        ];
        for (raw, want) in cases {
            assert_eq!(
                clean_title(raw, 32, "myhost").as_deref(),
                *want,
                "title {raw:?}"
            );
        }
    }

    #[test]
    fn clean_title_truncates_to_max_len() {
        let cases: &[(&str, &str)] = &[
            ("✳ abcdefghij", "abcdefghij"),
            ("✳ abcdefghijk", "abcdefghi…"),
            ("✳ abcd efghijk", "abcd efgh…"),
            ("✳ abcdefgh ijk", "abcdefgh…"),
        ];
        for (raw, want) in cases {
            assert_eq!(
                clean_title(raw, 10, "h").as_deref(),
                Some(*want),
                "title {raw:?}"
            );
        }
    }

    fn pane(id: &str, window: &str, name: &str, title: &str) -> Pane {
        Pane {
            id: id.into(),
            session: "s".into(),
            window_id: window.into(),
            window_index: 0,
            window_name: name.into(),
            pane_index: 0,
            pid: 1,
            active: false,
            attached: false,
            activity: 0,
            command: "claude".into(),
            cwd: "/".into(),
            automatic_rename: true,
            autoname_off: false,
            last_autoname: String::new(),
            category: String::new(),
            title: title.into(),
        }
    }

    fn agents(ids: &[&str]) -> HashMap<String, PaneState> {
        ids.iter()
            .map(|id| {
                let mut state = PaneState::default();
                state.agent = Some("claude".into());
                (id.to_string(), state)
            })
            .collect()
    }

    fn args(panes: &[Pane], states: &HashMap<String, PaneState>) -> String {
        rename_args(panes, states, &NamingConfig::default(), "host").join(" ")
    }

    #[test]
    fn renames_agent_windows_in_one_call() {
        let panes = [
            pane("%1", "@1", "claude", "✳ gates"),
            pane("%2", "@2", "claude", "✳ home"),
            pane("%3", "@3", "zsh", "✳ not an agent"),
        ];
        assert_eq!(
            args(&panes, &agents(&["%1", "%2"])),
            "rename-window -t @1 -- gates ; set-option -w -t @1 @sessionizer_lastname gates ; \
             rename-window -t @2 -- home ; set-option -w -t @2 @sessionizer_lastname home"
        );
    }

    #[test]
    fn leaves_up_to_date_and_generic_windows_alone() {
        let mut named = pane("%1", "@1", "gates", "✳ gates");
        named.last_autoname = "gates".into();
        named.automatic_rename = false;
        let generic = pane("%2", "@2", "claude", "✳ Claude Code");
        assert_eq!(args(&[named, generic], &agents(&["%1", "%2"])), "");
    }

    #[test]
    fn follows_a_title_change() {
        let mut p = pane("%1", "@1", "gates", "✳ gates v2");
        p.last_autoname = "gates".into();
        p.automatic_rename = false;
        assert!(args(&[p], &agents(&["%1"])).starts_with("rename-window -t @1 -- gates v2 ;"));
    }

    #[test]
    fn user_rename_disables_and_off_is_final() {
        let mut renamed = pane("%1", "@1", "mine", "✳ gates");
        renamed.last_autoname = "gates".into();
        renamed.automatic_rename = false;
        let mut named_by_hand = pane("%2", "@2", "work", "✳ home");
        named_by_hand.automatic_rename = false;
        let mut off = pane("%3", "@3", "x", "✳ other");
        off.autoname_off = true;
        assert_eq!(
            args(&[renamed, named_by_hand, off], &agents(&["%1", "%2", "%3"])),
            "set-option -w -t @1 @sessionizer_autoname off ; \
             set-option -w -t @2 @sessionizer_autoname off"
        );
    }

    #[test]
    fn active_agent_pane_names_a_shared_window() {
        let first = pane("%1", "@1", "claude", "✳ first");
        let mut active = pane("%2", "@1", "claude", "✳ second");
        active.active = true;
        let late = pane("%3", "@1", "claude", "✳ third");
        let out = args(&[first, active, late], &agents(&["%1", "%2", "%3"]));
        assert!(out.starts_with("rename-window -t @1 -- second ;"), "{out}");
        assert_eq!(out.matches("rename-window").count(), 1);
    }

    #[test]
    fn first_usable_title_when_no_pane_is_active() {
        let generic = pane("%1", "@1", "claude", "✳ Claude Code");
        let named = pane("%2", "@1", "claude", "✳ named");
        let out = args(&[generic, named], &agents(&["%1", "%2"]));
        assert!(out.starts_with("rename-window -t @1 -- named ;"), "{out}");
    }
}
