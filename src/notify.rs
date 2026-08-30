//! Best-effort OS notifications.
//!
//! [`notifier_args`] is pure so it can be unit-tested without touching the
//! system. [`send`] is the thin side-effecting wrapper: it's best-effort and
//! never blocks the caller (notifier processes are spawned and detached, so a
//! wedged notifier can't stall the worker), so callers can invoke it
//! unconditionally without error handling.

use std::process::Command;
use std::thread;

/// Notifier candidates in precedence order.
const NOTIFIER_CANDIDATES: [&str; 2] = ["terminal-notifier", "notify-send"];

/// A desktop notification payload, independent of which notifier delivers it.
#[derive(Debug, Clone, PartialEq)]
pub struct Notification {
    pub title: String,
    pub body: String,
    pub urgent: bool,
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
        if spawned(binary, n) {
            return;
        }
    }
}

/// Launch `binary` with `n`'s args and return `true` if it spawned, `false` on
/// spawn error.
///
/// We deliberately do NOT wait for the notifier on the caller's thread: `send`
/// runs on the worker's single polling thread, and a wedged notifier (e.g. a
/// stuck D-Bus session bus) must not be able to stall status polling,
/// auto-close, or permission handling. Delivery is best-effort; exit status is
/// irrelevant.
///
/// The child IS reaped — by a one-shot background thread — because a dropped
/// `Child` that is never waited on becomes a zombie for the lifetime of this
/// long-lived process (daemon/TUI), leaking a pid slot per notification.
fn spawned(binary: &str, n: &Notification) -> bool {
    match Command::new(binary)
        .args(notifier_args(binary, n))
        .spawn()
    {
        Ok(mut child) => {
            thread::spawn(move || {
                let _ = child.wait();
            });
            true
        }
        Err(_) => false,
    }
}

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
