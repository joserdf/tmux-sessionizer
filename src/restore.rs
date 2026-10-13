//! Rebuild tmux sessions and windows from a [`Snapshot`]: after a reboot,
//! on `showrunner restore`, or to reopen a closed window or session.
//!
//! Each pane gets its previous command pasted at the prompt, never executed
//! (unless `run_agents` is on, for agent panes). Agent panes get the command
//! that resumes the same agent conversation.

use std::collections::HashSet;
use std::process::Command;
use std::thread::sleep;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};

use crate::agent::AgentKind;
use crate::snapshot::{self, PaneSnap, SessionSnap, Snapshot, WindowSnap};

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RestoreConfig {
    /// Restore the last snapshot when the daemon starts on a tmux server
    /// that is newer than the snapshot and wasn't restored yet.
    #[serde(default = "def_on_start")]
    pub on_start: bool,
    /// Also press Enter on agent resume commands (others are only pasted).
    #[serde(default)]
    pub run_agents: bool,
    /// Arguments kept out of snapshots: `--flag` drops the flag and its
    /// value, `*SUFFIX=` drops `NAME=value` with NAME ending in SUFFIX.
    #[serde(default = "def_redact")]
    pub redact: Vec<String>,
}

fn def_on_start() -> bool {
    true
}

fn def_redact() -> Vec<String> {
    [
        "--token",
        "--password",
        "--api-key",
        "*_KEY=",
        "*_TOKEN=",
        "*PASSWORD=",
    ]
    .map(String::from)
    .into()
}

impl Default for RestoreConfig {
    fn default() -> Self {
        Self {
            on_start: def_on_start(),
            run_agents: false,
            redact: def_redact(),
        }
    }
}

// ---- Commands to paste ---------------------------------------------------

#[derive(Clone, Copy, PartialEq)]
enum Takes {
    Nothing,
    Value,
    /// Claude's `--resume [id]`: the next token, unless it is a flag.
    OptionalValue,
}

/// Flags that pick the conversation, removed before adding the resume flag.
fn resume_flag(kind: AgentKind, flag: &str) -> Option<Takes> {
    use Takes::*;
    match (kind, flag) {
        (AgentKind::Claude, "--resume" | "-r") => Some(OptionalValue),
        (AgentKind::Claude, "--continue" | "-c") => Some(Nothing),
        (AgentKind::Claude, "--session-id") => Some(Value),
        (AgentKind::OpenCode, "--session" | "-s") => Some(Value),
        (AgentKind::OpenCode, "--continue" | "-c") => Some(Nothing),
        (AgentKind::Codex, "--last") => Some(Nothing),
        _ => None,
    }
}

/// Flags that take no value, so a word after them is a positional.
fn boolean_flag(flag: &str) -> bool {
    matches!(
        flag,
        "--dangerously-skip-permissions"
            | "--dangerously-bypass-approvals-and-sandbox"
            | "--verbose"
            | "--full-auto"
            | "--yolo"
    )
}

fn is_runtime(arg0: &str) -> bool {
    matches!(
        arg0.rsplit('/').next().unwrap_or(arg0),
        "node" | "bun" | "deno"
    )
}

/// The command resuming agent session `session_id`: `argv` without the
/// flags that pick a conversation, then `claude … --resume <id>`,
/// `opencode … -s <id>` or `codex resume <id> …`. Other flags (`--model`,
/// `--dangerously-skip-permissions`) are kept. A positional word that doesn't
/// follow a flag is the initial prompt and is dropped, so pressing Enter
/// doesn't send it again. Without a session id (or for Pi, which has no
/// resume id) `argv` is returned unchanged.
pub fn resume_command(kind: AgentKind, argv: &[String], session_id: Option<&str>) -> Vec<String> {
    let (Some(id), false) = (session_id, kind == AgentKind::Pi || argv.is_empty()) else {
        return argv.to_vec();
    };
    let prog_len = (1 + usize::from(is_runtime(&argv[0]))).min(argv.len());
    let (prog, args) = argv.split_at(prog_len);
    let mut i = 0;
    if kind == AgentKind::Codex && args.first().is_some_and(|a| a == "resume") {
        i = 1 + usize::from(args.get(1).is_some_and(|a| !a.starts_with('-')));
    }
    let mut kept: Vec<String> = Vec::new();
    let mut after_value_flag = false;
    while i < args.len() {
        let arg = &args[i];
        i += 1;
        let (flag, inline) = match arg.split_once('=') {
            Some((f, _)) if arg.starts_with("--") => (f, true),
            _ => (arg.as_str(), false),
        };
        if let Some(takes) = resume_flag(kind, flag) {
            let next_is_value = args.get(i).is_some_and(|n| !n.starts_with('-'));
            if !inline
                && (takes == Takes::Value || (takes == Takes::OptionalValue && next_is_value))
            {
                i += 1;
            }
            after_value_flag = false;
            continue;
        }
        if !arg.starts_with('-') && !after_value_flag {
            continue;
        }
        after_value_flag = arg.starts_with('-') && !inline && !boolean_flag(arg);
        kept.push(arg.clone());
    }
    let mut out = prog.to_vec();
    match kind {
        AgentKind::Codex => {
            out.extend(["resume".to_string(), id.to_string()]);
            out.extend(kept);
        }
        AgentKind::OpenCode => {
            out.extend(kept);
            out.extend(["-s".to_string(), id.to_string()]);
        }
        _ => {
            out.extend(kept);
            out.extend(["--resume".to_string(), id.to_string()]);
        }
    }
    out
}

/// Join words into a shell command line, quoting only where needed.
fn shell_join(words: &[String]) -> String {
    words
        .iter()
        .map(|w| {
            let plain = !w.is_empty()
                && w.chars()
                    .all(|c| c.is_ascii_alphanumeric() || "-_./=:@%+,".contains(c));
            if plain {
                w.clone()
            } else {
                crate::tmux::shell_escape(w)
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
}

/// The command to paste into a restored pane, and whether it resumes an
/// agent. `None` for an idle shell.
pub fn pane_command(pane: &PaneSnap) -> Option<(String, bool)> {
    let agent = pane.agent.as_ref();
    let argv = match (&pane.argv, agent) {
        (Some(argv), _) => argv.clone(),
        (None, Some(a)) => vec![a.kind.clone()],
        (None, None) => return None,
    };
    let kind = agent.and_then(|a| AgentKind::from_id(&a.kind));
    let words = match kind {
        Some(k) => resume_command(k, &argv, agent.and_then(|a| a.session_id.as_deref())),
        None => argv,
    };
    Some((shell_join(&words), kind.is_some()))
}

// ---- tmux ----------------------------------------------------------------

fn tmux(args: &[&str]) -> Result<String> {
    let out = Command::new("tmux").args(args).output()?;
    if !out.status.success() {
        bail!("{}", String::from_utf8_lossy(&out.stderr).trim());
    }
    Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
}

/// Indexes of a session's windows; `None` when the session doesn't exist.
fn existing_windows(session: &str) -> Option<HashSet<u32>> {
    let out = tmux(&[
        "list-windows",
        "-t",
        &format!("={session}:"),
        "-F",
        "#{window_index}",
    ])
    .ok()?;
    Some(out.lines().filter_map(|l| l.parse().ok()).collect())
}

/// `WxH` of a layout string (`csum,WxH,x,y…`).
fn layout_size(layout: &str) -> Option<(&str, &str)> {
    layout.split(',').nth(1)?.split_once('x')
}

struct Paste {
    pane: String,
    text: String,
    submit: bool,
}

/// Panes, layout, names and options of a window whose first pane exists.
fn fill_window(w: &WindowSnap, first: String, run_agents: bool, pastes: &mut Vec<Paste>) {
    let mut ids = vec![first];
    for p in w.panes.iter().skip(1) {
        // Split the newest pane so tmux keeps the panes in snapshot order.
        let last = ids.last().cloned().unwrap_or_default();
        let args = [
            "split-window",
            "-d",
            "-t",
            &last,
            "-c",
            &p.cwd,
            "-P",
            "-F",
            "#{pane_id}",
        ];
        match tmux(&args) {
            Ok(id) => ids.push(id),
            Err(_) => break,
        }
    }
    let first = ids[0].clone();
    let _ = tmux(&["select-layout", "-t", &first, &w.layout]);
    if !w.automatic_rename {
        let _ = tmux(&["rename-window", "-t", &first, "--", &w.name]);
    }
    if !w.last_autoname.is_empty() {
        let _ = tmux(&[
            "set-option",
            "-w",
            "-t",
            &first,
            "@sessionizer_lastname",
            &w.last_autoname,
        ]);
    }
    if w.autoname_off {
        let _ = tmux(&[
            "set-option",
            "-w",
            "-t",
            &first,
            "@sessionizer_autoname",
            "off",
        ]);
    }
    for (snap, id) in w.panes.iter().zip(&ids) {
        if snap.active {
            let _ = tmux(&["select-pane", "-t", id]);
        }
        if let Some((text, agent)) = pane_command(snap) {
            let pending = snapshot::Pending {
                argv: snap.argv.clone(),
                agent: snap.agent.clone(),
            };
            if let Ok(json) = serde_json::to_string(&pending) {
                let _ = tmux(&["set-option", "-p", "-t", id, "@sessionizer_restore", &json]);
            }
            pastes.push(Paste {
                pane: id.clone(),
                text,
                submit: agent && run_agents,
            });
        }
    }
}

/// Restore a session's missing windows, creating the session if needed.
/// A window whose index is taken is skipped, or with `force` (reopen) added
/// at the next free index. Returns the number of windows created.
fn restore_session(
    s: &SessionSnap,
    force: bool,
    run_agents: bool,
    pastes: &mut Vec<Paste>,
) -> Result<usize> {
    let existing = existing_windows(&s.name);
    let mut exists = existing.is_some();
    let taken = existing.unwrap_or_default();
    let target = format!("={}:", s.name);
    let mut created = 0;
    for w in &s.windows {
        let Some(first) = w.panes.first() else {
            continue;
        };
        let free = !taken.contains(&w.index);
        if !free && !force {
            continue;
        }
        let pane = if !exists {
            let mut args = vec!["new-session", "-d", "-s", &s.name, "-c", &first.cwd];
            if let Some((x, y)) = layout_size(&w.layout) {
                args.extend(["-x", x, "-y", y]);
            }
            args.extend(["-P", "-F", "#{window_index}\t#{pane_id}"]);
            let out = tmux(&args)?;
            let (index, pane) = out.split_once('\t').context("new-session output")?;
            if index != w.index.to_string() {
                let from = format!("{target}{index}");
                let _ = tmux(&[
                    "move-window",
                    "-s",
                    &from,
                    "-t",
                    &format!("{target}{}", w.index),
                ]);
            }
            exists = true;
            pane.to_string()
        } else {
            let at = if free {
                format!("{target}{}", w.index)
            } else {
                target.clone()
            };
            let args = [
                "new-window",
                "-d",
                "-t",
                &at,
                "-c",
                &first.cwd,
                "-P",
                "-F",
                "#{pane_id}",
            ];
            tmux(&args)?
        };
        fill_window(w, pane, run_agents, pastes);
        created += 1;
        if w.active {
            let _ = tmux(&["select-window", "-t", &format!("{target}{}", w.index)]);
        }
    }
    if exists && !s.category.is_empty() {
        let _ = tmux(&["set-option", "-t", &target, "@category", &s.category]);
    }
    for (k, v) in &s.env {
        let _ = tmux(&["set-environment", "-t", &target, k, v]);
    }
    Ok(created)
}

/// How long to wait for restored shells to show a prompt before pasting.
const PROMPT_WAIT: Duration = Duration::from_secs(5);

/// The shell drew its prompt, so pasted input lands in its line editor.
/// A prompt leaves the cursor after it; start-up banners (`/etc/bash.bashrc`'s
/// sudo hint, a MOTD) end in a newline and leave it at column 0.
fn prompt_ready(pane: &str) -> bool {
    tmux(&["display-message", "-p", "-t", pane, "#{cursor_x}"])
        .is_ok_and(|x| x.parse::<u32>().is_ok_and(|x| x > 0))
}

fn paste_when_ready(mut pending: Vec<Paste>) {
    let start = Instant::now();
    while !pending.is_empty() {
        let timed_out = start.elapsed() >= PROMPT_WAIT;
        pending.retain(|p| {
            if !timed_out && !prompt_ready(&p.pane) {
                return true;
            }
            let _ = tmux(&["send-keys", "-t", &p.pane, "-l", "--", &p.text]);
            if p.submit {
                let _ = tmux(&["send-keys", "-t", &p.pane, "Enter"]);
            }
            false
        });
        if !pending.is_empty() {
            sleep(Duration::from_millis(100));
        }
    }
}

/// What a restore did.
#[derive(Debug, Default)]
pub struct Report {
    pub windows: usize,
    pub pasted: usize,
    pub errors: Vec<String>,
}

/// Restore every session of `snap`, merging into sessions that exist (by
/// session name and window index, never duplicating). Snapshots are paused
/// meanwhile.
pub fn restore(snap: &Snapshot, cfg: &RestoreConfig, force: bool) -> Result<Report> {
    let _lock = snapshot::RestoreLock::acquire()?;
    let mut report = Report::default();
    let mut pastes = Vec::new();
    for s in &snap.sessions {
        match restore_session(s, force, cfg.run_agents, &mut pastes) {
            Ok(n) => report.windows += n,
            Err(e) => report.errors.push(format!("{}: {e}", s.name)),
        }
    }
    report.pasted = pastes.len();
    paste_when_ready(pastes);
    Ok(report)
}

/// The plan for `restore --dry-run`: one line per window to create and per
/// command to paste.
pub fn describe(snap: &Snapshot) -> Vec<String> {
    let mut lines = Vec::new();
    for s in &snap.sessions {
        let taken = existing_windows(&s.name).unwrap_or_default();
        for w in s.windows.iter().filter(|w| !taken.contains(&w.index)) {
            let category = if s.category.is_empty() {
                String::new()
            } else {
                format!(" [{}]", s.category)
            };
            lines.push(format!("{}:{} {}{category}", s.name, w.index, w.name));
            for p in &w.panes {
                let cmd = pane_command(p).map_or_else(String::new, |(c, _)| format!("  $ {c}"));
                lines.push(format!("  pane {} {}{cmd}", p.index, p.cwd));
            }
        }
    }
    lines
}

// ---- Boot ----------------------------------------------------------------

/// A server started after the snapshot was taken (so the daemon never saw it)
/// and not restored yet; no server at all counts too.
fn server_needs_restore(taken_at: u64) -> bool {
    let Ok(out) = tmux(&[
        "display-message",
        "-p",
        "#{start_time}\t#{@sessionizer_restored}",
    ]) else {
        return true;
    };
    let (start, restored) = out.split_once('\t').unwrap_or((&out, ""));
    restored != "1" && start.parse::<u64>().is_ok_and(|t| t > taken_at)
}

/// On daemon start: restore the last snapshot if the tmux server is new.
pub fn boot(cfg: &RestoreConfig) {
    if !cfg.on_start {
        return;
    }
    let Some(snap) = snapshot::load(&snapshot::snapshot_path()) else {
        return;
    };
    if !server_needs_restore(snap.taken_at) {
        return;
    }
    match restore(&snap, cfg, false) {
        Ok(r) => eprintln!(
            "showrunner: restored {} windows, pasted {} commands{}",
            r.windows,
            r.pasted,
            if r.errors.is_empty() {
                String::new()
            } else {
                format!(" ({})", r.errors.join("; "))
            }
        ),
        Err(e) => eprintln!("showrunner: restore failed: {e}"),
    }
    let _ = tmux(&["set-option", "-g", "@sessionizer_restored", "1"]);
}

/// Reopen closed-history entry `id` and drop it from the history.
pub fn reopen(id: u64, cfg: &RestoreConfig) -> Result<Report> {
    let mut history = snapshot::load_closed();
    let pos = history
        .iter()
        .position(|e| e.id == id)
        .with_context(|| format!("no closed entry {id} (see `showrunner closed list`)"))?;
    let entry = history.remove(pos);
    let snap = Snapshot {
        taken_at: entry.closed_at,
        sessions: vec![entry.session],
    };
    let report = restore(&snap, cfg, true)?;
    snapshot::save_closed(&history)?;
    Ok(report)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::snapshot::AgentResume;

    fn words(s: &str) -> Vec<String> {
        s.split_whitespace().map(String::from).collect()
    }

    fn resume(kind: AgentKind, argv: &str, id: Option<&str>) -> String {
        resume_command(kind, &words(argv), id).join(" ")
    }

    #[test]
    fn claude_resume_table() {
        use AgentKind::Claude;
        let cases = [
            ("claude", "claude --resume ID"),
            ("claude --resume", "claude --resume ID"),
            ("claude --resume home", "claude --resume ID"),
            (
                "claude -r old --model sonnet",
                "claude --model sonnet --resume ID",
            ),
            (
                "claude --continue --dangerously-skip-permissions",
                "claude --dangerously-skip-permissions --resume ID",
            ),
            (
                "claude --resume --model opus",
                "claude --model opus --resume ID",
            ),
            (
                "claude --session-id X --verbose",
                "claude --verbose --resume ID",
            ),
            ("claude --resume=old", "claude --resume ID"),
            ("claude fix-the-bug", "claude --resume ID"),
            (
                "claude --append-system-prompt SP initial",
                "claude --append-system-prompt SP --resume ID",
            ),
            (
                "claude --dangerously-skip-permissions prompt",
                "claude --dangerously-skip-permissions --resume ID",
            ),
            ("node /x/cli.js --resume a", "node /x/cli.js --resume ID"),
        ];
        for (argv, want) in cases {
            assert_eq!(resume(Claude, argv, Some("ID")), want, "argv {argv:?}");
        }
    }

    #[test]
    fn other_agents_resume() {
        assert_eq!(
            resume(AgentKind::OpenCode, "opencode -s old -m x", Some("ID")),
            "opencode -m x -s ID"
        );
        assert_eq!(
            resume(AgentKind::OpenCode, "opencode --continue", Some("ID")),
            "opencode -s ID"
        );
        assert_eq!(
            resume(
                AgentKind::Codex,
                "codex resume --last --dangerously-bypass-approvals-and-sandbox",
                Some("ID")
            ),
            "codex resume ID --dangerously-bypass-approvals-and-sandbox"
        );
        assert_eq!(
            resume(AgentKind::Codex, "codex resume old -m o3", Some("ID")),
            "codex resume ID -m o3"
        );
        assert_eq!(
            resume(
                AgentKind::Codex,
                "codex --dangerously-bypass-approvals-and-sandbox do-it",
                Some("ID")
            ),
            "codex resume ID --dangerously-bypass-approvals-and-sandbox"
        );
        assert_eq!(resume(AgentKind::Pi, "pi --x", Some("ID")), "pi --x");
    }

    #[test]
    fn without_session_id_argv_is_kept() {
        assert_eq!(
            resume(AgentKind::Claude, "claude --resume home", None),
            "claude --resume home"
        );
    }

    #[test]
    fn pane_command_quotes_and_flags_agents() {
        let shell = PaneSnap {
            index: 0,
            cwd: "/".into(),
            active: true,
            argv: None,
            agent: None,
        };
        assert_eq!(pane_command(&shell), None);
        let btop = PaneSnap {
            argv: Some(vec!["btop".into()]),
            ..shell.clone()
        };
        assert_eq!(pane_command(&btop), Some(("btop".into(), false)));
        let quoted = PaneSnap {
            argv: Some(vec!["grep".into(), "a b".into(), "it's".into()]),
            ..shell.clone()
        };
        assert_eq!(pane_command(&quoted).unwrap().0, r"grep 'a b' 'it'\''s'");
        let agent = PaneSnap {
            argv: None,
            agent: Some(AgentResume {
                kind: "claude".into(),
                session_id: Some("abc-1".into()),
            }),
            ..shell
        };
        assert_eq!(
            pane_command(&agent),
            Some(("claude --resume abc-1".into(), true))
        );
    }

    #[test]
    fn layout_size_parses() {
        assert_eq!(
            layout_size("21be,200x50,0,0{100x50,0,0,0}"),
            Some(("200", "50"))
        );
        assert_eq!(layout_size("L"), None);
    }
}
