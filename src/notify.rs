//! Best-effort OS notifications and terminal bell.
//!
//! The decision logic ([`pick_notifier`], [`notifier_args`]) is pure so it
//! can be unit-tested without touching the system. [`send`] and
//! [`tmux_bell`] are the thin side-effecting wrappers: both are
//! best-effort and never panic, so callers can invoke them unconditionally
//! without error handling.

use std::process::Command;

/// Notifier candidates in precedence order.
const NOTIFIER_CANDIDATES: [&str; 2] = ["terminal-notifier", "notify-send"];

/// A desktop notification payload, independent of which notifier delivers it.
#[derive(Debug, Clone, PartialEq)]
pub struct Notification {
    pub title: String,
    pub body: String,
    pub urgent: bool,
}

/// Pick the notifier binary to use from the commands present on `PATH`.
///
/// `available` holds command names the caller has already checked exist
/// (e.g. via `which`). Precedence is `terminal-notifier` (macOS) over
/// `notify-send` (libnotify, Linux); `None` when neither is available.
pub fn pick_notifier(available: &[&str]) -> Option<&'static str> {
    for candidate in NOTIFIER_CANDIDATES {
        if available.contains(&candidate) {
            return Some(candidate);
        }
    }
    None
}

/// Build the argument vector for `binary` — everything after the program
/// name — to deliver `n`.
///
/// Pure: same inputs, same output, no I/O. Unknown binaries yield an empty
/// vec; do not spawn with those.
pub fn notifier_args(binary: &str, n: &Notification) -> Vec<String> {
    match binary {
        "terminal-notifier" => {
            let mut args = vec![
                "-title".to_string(),
                n.title.clone(),
                "-message".to_string(),
                n.body.clone(),
            ];
            if n.urgent {
                args.extend(["-sound".to_string(), "SOS".to_string()]);
            }
            args
        }
        "notify-send" => {
            let urgency = if n.urgent { "critical" } else { "normal" };
            vec![
                "-u".to_string(),
                urgency.to_string(),
                n.title.clone(),
                n.body.clone(),
            ]
        }
        _ => vec![],
    }
}

/// Deliver `n` to the desktop, best-effort.
///
/// Tries `terminal-notifier` first, then `notify-send`, stopping at the
/// first binary that actually spawns successfully. Missing binaries, spawn
/// errors, and non-zero exit statuses are all silently ignored; this never
/// panics.
pub fn send(n: &Notification) {
    for binary in NOTIFIER_CANDIDATES {
        if ran(binary, n) {
            return;
        }
    }
}

/// Spawn `binary` with `n`'s args; `true` if the process launched and ran
/// (exit status irrelevant), `false` on spawn error.
fn ran(binary: &str, n: &Notification) -> bool {
    Command::new(binary)
        .args(notifier_args(binary, n))
        .output()
        .is_ok()
}

/// Ring the user's terminal bell.
///
/// Intentionally a no-op. This process is often a headless daemon with no
/// terminal of its own, so there is no bell to ring; writing `\x07` when a
/// terminal *is* attached would risk interleaving with the TUI's output.
/// Under tmux the daemon's stdout is not a user pane, and tmux typically
/// swallows or redirects pane bells (`bell-action`), so no command run from
/// here (e.g. `tmux display-message`) could reliably reach the user's
/// terminal. Callers may invoke this unconditionally; it never panics.
pub fn tmux_bell() {}

#[cfg(test)]
mod tests {
    use super::*;

    fn note(urgent: bool) -> Notification {
        Notification {
            title: "Task done".to_string(),
            body: "demo finished on branch feat".to_string(),
            urgent,
        }
    }

    #[test]
    fn pick_notifier_falls_back_to_notify_send() {
        assert_eq!(pick_notifier(&["notify-send"]), Some("notify-send"));
    }

    #[test]
    fn pick_notifier_prefers_terminal_notifier() {
        assert_eq!(
            pick_notifier(&["terminal-notifier", "notify-send"]),
            Some("terminal-notifier")
        );
    }

    #[test]
    fn pick_notifier_is_none_when_nothing_available() {
        assert_eq!(pick_notifier(&["x"]), None);
        assert_eq!(pick_notifier(&[]), None);
    }

    #[test]
    fn terminal_notifier_args_without_sound() {
        assert_eq!(
            notifier_args("terminal-notifier", &note(false)),
            vec![
                "-title",
                "Task done",
                "-message",
                "demo finished on branch feat"
            ]
        );
    }

    #[test]
    fn terminal_notifier_args_with_sound() {
        assert_eq!(
            notifier_args("terminal-notifier", &note(true)),
            vec![
                "-title",
                "Task done",
                "-message",
                "demo finished on branch feat",
                "-sound",
                "SOS"
            ]
        );
    }

    #[test]
    fn notify_send_args_critical() {
        assert_eq!(
            notifier_args("notify-send", &note(true)),
            vec!["-u", "critical", "Task done", "demo finished on branch feat"]
        );
    }

    #[test]
    fn notify_send_args_normal() {
        assert_eq!(
            notifier_args("notify-send", &note(false)),
            vec!["-u", "normal", "Task done", "demo finished on branch feat"]
        );
    }

    #[test]
    fn unknown_binary_has_no_args() {
        assert!(notifier_args("unknown", &note(true)).is_empty());
    }
}
