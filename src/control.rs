//! What the MCP server (`showrunner mcp`) does, as plain functions over a
//! fresh inventory: resolve targets, describe sessions, windows and agents,
//! and act on them. Panes are listed per call; agent state (status, session
//! id) comes from the daemon, which alone sees the agents' hook events.
//!
//! Targets are `session` (its active window), `session:window` (index, name
//! or `@id`) or `%pane`. A window's main pane is its agent pane, else its
//! active pane.

use std::collections::HashMap;
use std::process::Command;
use std::thread::sleep;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use serde_json::{Value, json};

use crate::agent::{AGENT_IDS, AgentKind};
use crate::autoclose::Target as CloseTarget;
use crate::config::Config;
use crate::inventory::{self, AgentStatus, Pane, PaneState};
use crate::restore::{self, Paste};
use crate::snapshot::{self, CloseReason};

pub fn now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// The pane the caller runs in (`$TMUX_PANE`, inherited from the agent).
pub fn own_pane() -> Option<String> {
    std::env::var("TMUX_PANE").ok().filter(|p| !p.is_empty())
}

/// What a target names: `close` and `rename` act on sessions or windows.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Scope {
    Session,
    Window,
    Pane,
}

/// A resolved target and its main pane.
#[derive(Debug, Clone, Copy)]
pub struct Target<'a> {
    pub scope: Scope,
    pub pane: &'a Pane,
}

pub struct Inventory {
    pub panes: Vec<Pane>,
    pub states: HashMap<String, PaneState>,
    /// `states` came from the daemon (with statuses), not local detection.
    pub live: bool,
}

impl Inventory {
    /// `states` from the daemon; without it, only which agent runs where.
    pub fn new(panes: Vec<Pane>, states: Option<HashMap<String, PaneState>>) -> Self {
        let live = states.is_some();
        let mut states = states.unwrap_or_default();
        if !live {
            inventory::refresh_states(&mut states, &panes);
        }
        Self {
            panes,
            states,
            live,
        }
    }

    pub fn load(states: Option<HashMap<String, PaneState>>) -> Self {
        Self::new(inventory::list_panes(), states)
    }

    pub(crate) fn state(&self, pane: &Pane) -> Option<&PaneState> {
        self.states.get(&pane.id)
    }

    fn is_agent(&self, pane: &Pane) -> bool {
        self.state(pane).is_some_and(|s| s.agent.is_some())
    }

    fn status(&self, pane: &Pane) -> Option<AgentStatus> {
        self.state(pane).and_then(|s| s.status)
    }

    /// A window's status: the most urgent of its panes'.
    pub(crate) fn window_status(&self, window_id: &str) -> Option<AgentStatus> {
        self.window_panes(window_id)
            .filter_map(|p| self.status(p))
            .max()
    }

    /// The main agent pane's token usage, when its transcript is readable.
    fn window_token_usage(&self, pane: &Pane) -> Option<crate::usage::TokenUsage> {
        let kind = AgentKind::from_id(self.state(pane)?.agent.as_deref()?)?;
        crate::usage::token_usage(kind, self.state(pane)?.agent_session_id.as_deref()?)
    }

    fn window_panes<'a>(&'a self, window_id: &'a str) -> impl Iterator<Item = &'a Pane> {
        self.panes.iter().filter(move |p| p.window_id == window_id)
    }

    /// The agent pane of a window (the active one if several), else its
    /// active pane.
    fn main_pane(&self, window_id: &str) -> Option<&Pane> {
        let panes: Vec<&Pane> = self
            .panes
            .iter()
            .filter(|p| p.window_id == window_id)
            .collect();
        let agents: Vec<&Pane> = panes.iter().copied().filter(|p| self.is_agent(p)).collect();
        agents
            .iter()
            .find(|p| p.pane_active)
            .or(agents.first())
            .or(panes.iter().find(|p| p.pane_active))
            .or(panes.first())
            .copied()
    }

    /// One main pane per window, in tmux order.
    pub fn windows(&self) -> Vec<&Pane> {
        let mut seen = Vec::new();
        for p in &self.panes {
            if !seen.contains(&p.window_id.as_str()) {
                seen.push(&p.window_id);
            }
        }
        seen.into_iter().filter_map(|w| self.main_pane(w)).collect()
    }

    pub fn resolve(&self, target: &str) -> Result<Target<'_>> {
        let t = target.trim();
        if t.starts_with('%') {
            let pane = self
                .panes
                .iter()
                .find(|p| p.id == t)
                .with_context(|| format!("no pane {t}"))?;
            return Ok(Target {
                scope: Scope::Pane,
                pane,
            });
        }
        let (session, window) = match t.split_once(':') {
            Some((s, w)) if !w.is_empty() => (s, Some(w)),
            Some((s, _)) => (s, None),
            None => (t, None),
        };
        let in_session: Vec<&Pane> = self.panes.iter().filter(|p| p.session == session).collect();
        if in_session.is_empty() {
            bail!("no session {session:?} (see list_sessions)");
        }
        let window_id = match window {
            None => in_session
                .iter()
                .find(|p| p.window_active)
                .unwrap_or(&in_session[0])
                .window_id
                .as_str(),
            Some(w) => {
                let by_index = w.parse::<u32>().ok();
                let mut ids: Vec<&str> = in_session
                    .iter()
                    .filter(|p| match by_index {
                        Some(i) => p.window_index == i,
                        None => p.window_id == w || p.window_name == w,
                    })
                    .map(|p| p.window_id.as_str())
                    .collect();
                ids.dedup();
                match ids[..] {
                    [id] => id,
                    [] => bail!("no window {w:?} in session {session:?} (see list_windows)"),
                    _ => bail!("window name {w:?} is ambiguous in {session:?}; use its index"),
                }
            }
        };
        let pane = self.main_pane(window_id).context("window has no panes")?;
        let scope = if window.is_some() {
            Scope::Window
        } else {
            Scope::Session
        };
        Ok(Target { scope, pane })
    }

    /// A window (through its main pane), as the tools report it.
    pub fn window_json(&self, p: &Pane, now: u64) -> Value {
        let st = self.state(p);
        let mut v = json!({
            "target": format!("{}:{}", p.session, p.window_index),
            "session": p.session,
            "window": p.window_index,
            "window_id": p.window_id,
            "name": p.window_name,
            "pane": p.id,
            "panes": self.window_panes(&p.window_id).count(),
            "cwd": p.cwd,
            "command": p.command,
            "agent": st.and_then(|s| s.agent.clone()),
            "status": self.window_status(&p.window_id).map(|s| s.code()),
            "agent_session_id": st.and_then(|s| s.agent_session_id.clone()),
            "idle_secs": now.saturating_sub(p.activity),
        });
        if self.is_agent(p) && !p.title.is_empty() {
            v["title"] = json!(p.title);
        }
        if !p.pending_restore.is_empty() {
            v["pending_resume"] = json!(true);
        }
        if p.keep {
            v["keep"] = json!(true);
        }
        if p.active && p.attached {
            v["on_screen"] = json!(true);
        }
        strip_nulls(v)
    }

    pub fn list_sessions(&self, category: Option<&str>, status: Option<&str>, now: u64) -> Value {
        let mut out: Vec<Value> = Vec::new();
        let mut names: Vec<&str> = Vec::new();
        for p in &self.panes {
            if !names.contains(&p.session.as_str()) {
                names.push(&p.session);
            }
        }
        for name in names {
            let windows: Vec<&Pane> = self
                .windows()
                .into_iter()
                .filter(|p| p.session == name)
                .collect();
            let first = windows[0];
            if category.is_some_and(|c| c != first.category) {
                continue;
            }
            let statuses: Vec<Option<AgentStatus>> = windows
                .iter()
                .map(|p| self.window_status(&p.window_id))
                .collect();
            if status.is_some_and(|s| !statuses.iter().any(|st| st.map(|st| st.code()) == Some(s)))
            {
                continue;
            }
            let summary: Vec<Value> = windows
                .iter()
                .zip(&statuses)
                .map(|(p, st)| {
                    strip_nulls(json!({
                        "window": p.window_index,
                        "name": p.window_name,
                        "agent": self.state(p).and_then(|s| s.agent.clone()),
                        "status": st.map(|s| s.code()),
                        "idle_secs": now.saturating_sub(p.activity),
                    }))
                })
                .collect();
            out.push(strip_nulls(json!({
                "session": name,
                "category": (!first.category.is_empty()).then_some(&first.category),
                "attached": first.attached,
                "windows": summary,
            })));
        }
        json!(out)
    }

    pub fn list_windows(&self, session: Option<&str>, now: u64) -> Result<Value> {
        if let Some(s) = session
            && !self.panes.iter().any(|p| p.session == s)
        {
            bail!("no session {s:?}");
        }
        Ok(json!(
            self.windows()
                .into_iter()
                .filter(|p| session.is_none_or(|s| p.session == s))
                .map(|p| self.window_json(p, now))
                .collect::<Vec<_>>()
        ))
    }

    pub fn whoami(&self, now: u64) -> Value {
        match own_pane().and_then(|id| self.panes.iter().find(|p| p.id == id)) {
            Some(p) => {
                let mut v = self.window_json(p, now);
                v["pane"] = json!(p.id);
                v["note"] = json!("your own pane: send_message, close and suspend refuse it");
                v
            }
            None => json!({"in_tmux": false}),
        }
    }

    pub fn get_status(&self, target: &str, now: u64, cfg: &Config) -> Result<Value> {
        let t = self.resolve(target)?;
        let p = t.pane;
        let mut v = self.window_json(p, now);
        if let Some(usage) = self.window_token_usage(p) {
            add_tokens(&mut v, &usage, &cfg.pricing);
        }
        if let Some(branch) = git(&p.cwd, &["rev-parse", "--abbrev-ref", "HEAD"]) {
            v["branch"] = json!(branch.trim());
        }
        if let Some(e) = self.state(p).and_then(|s| s.last_event.as_ref()) {
            v["last_event"] = json!(e);
        }
        if matches!(
            self.status(p),
            Some(AgentStatus::Question | AgentStatus::Permission)
        ) && let Ok(screen) = capture(&p.id, 60)
        {
            let options = parse_options(&screen);
            if !options.is_empty() {
                v["options"] = json!(
                    options
                        .iter()
                        .map(|(n, l)| format!("{n}. {l}"))
                        .collect::<Vec<_>>()
                );
            }
        }
        if !self.live {
            v["note"] = json!("daemon not reachable: agent status unknown");
        }
        Ok(v)
    }

    pub fn read_output(&self, target: &str, lines: usize) -> Result<Value> {
        let t = self.resolve(target)?;
        Ok(json!({
            "pane": t.pane.id,
            "status": self.status(t.pane).map(|s| s.code()),
            "output": capture(&t.pane.id, lines.clamp(1, 500))?,
        }))
    }

    pub fn send_message(
        &self,
        target: &str,
        text: &str,
        submit: bool,
        force: bool,
    ) -> Result<Value> {
        let t = self.resolve(target)?;
        refuse_own(t.pane)?;
        if !force && self.status(t.pane) == Some(AgentStatus::Working) {
            bail!("{target} is working; wait_for_status first or pass force=true");
        }
        crate::tmux::paste_text(&t.pane.id, text, submit)?;
        Ok(json!({"sent_to": t.pane.id, "submitted": submit}))
    }

    pub fn send_keys(&self, target: &str, keys: &[String]) -> Result<Value> {
        let t = self.resolve(target)?;
        refuse_own(t.pane)?;
        if keys.is_empty() {
            bail!("no keys");
        }
        tmux(&[&["send-keys", "-t", &t.pane.id][..], &str_refs(keys)].concat())?;
        Ok(json!({"sent_to": t.pane.id, "keys": keys}))
    }

    /// Pick option `pick(options)` of the dialog the agent shows and return
    /// the screen after it.
    fn choose(
        &self,
        target: &str,
        pick: impl Fn(&[(u32, String)]) -> Result<u32>,
    ) -> Result<Value> {
        let t = self.resolve(target)?;
        refuse_own(t.pane)?;
        if self.live
            && !matches!(
                self.status(t.pane),
                Some(AgentStatus::Question | AgentStatus::Permission)
            )
        {
            bail!("{target} is not waiting on a question or permission");
        }
        let options = parse_options(&capture(&t.pane.id, 60)?);
        if options.is_empty() {
            bail!(
                "no numbered options on screen (OpenCode dialogs are unsupported); use send_keys"
            );
        }
        let n = pick(&options)?;
        tmux(&["send-keys", "-t", &t.pane.id, &n.to_string()])?;
        sleep(Duration::from_millis(800));
        Ok(json!({
            "chose": options.iter().find(|(m, _)| *m == n).map(|(m, l)| format!("{m}. {l}")),
            "screen": tail(&capture(&t.pane.id, 40)?, 15),
        }))
    }

    pub fn approve(&self, target: &str, decision: &str) -> Result<Value> {
        self.choose(target, |opts| approve_option(opts, decision))
    }

    pub fn answer(&self, target: &str, choice: &str) -> Result<Value> {
        self.choose(target, |opts| answer_option(opts, choice))
    }

    pub fn read_diff(&self, target: &str) -> Result<Value> {
        let t = self.resolve(target)?;
        let cwd = &t.pane.cwd;
        let root = git(cwd, &["rev-parse", "--show-toplevel"])
            .with_context(|| format!("{cwd} is not in a git repository"))?;
        let diff = git(cwd, &["diff", "HEAD"])
            .or_else(|| git(cwd, &["diff"]))
            .unwrap_or_default();
        let untracked =
            git(cwd, &["ls-files", "--others", "--exclude-standard"]).unwrap_or_default();
        Ok(strip_nulls(json!({
            "repo": root.trim(),
            "branch": git(cwd, &["rev-parse", "--abbrev-ref", "HEAD"]).map(|b| b.trim().to_string()),
            "stat": git(cwd, &["diff", "HEAD", "--stat"]).map(|s| s.trim_end().to_string()),
            "untracked": untracked.lines().collect::<Vec<_>>(),
            "diff": truncate(&diff, DIFF_MAX),
        })))
    }

    pub fn create_session(
        &self,
        name: &str,
        cwd: &str,
        agent: Option<&str>,
        prompt: Option<&str>,
        category: Option<&str>,
    ) -> Result<Value> {
        check_session_name(name)?;
        if self.panes.iter().any(|p| p.session == name) {
            bail!("session {name:?} exists");
        }
        let cwd = existing_dir(cwd)?;
        let command = agent_command(agent, &cwd, prompt)?;
        if let Some(c) = category.filter(|c| !c.is_empty()) {
            crate::category::validate(c)?;
        }
        let pane = tmux(&[
            "new-session",
            "-d",
            "-s",
            name,
            "-c",
            &cwd,
            "-P",
            "-F",
            "#{pane_id}",
        ])?;
        if let Some(c) = category.filter(|c| !c.is_empty()) {
            crate::category::set(name, Some(c))?;
        }
        launch(&pane, command.as_deref());
        Ok(strip_nulls(
            json!({"target": name, "pane": pane, "cwd": cwd, "command": command}),
        ))
    }

    pub fn create_window(
        &self,
        session: &str,
        cwd: Option<&str>,
        agent: Option<&str>,
        prompt: Option<&str>,
        name: Option<&str>,
    ) -> Result<Value> {
        let base = self.resolve(session.split(':').next().unwrap_or(session))?;
        let session = base.pane.session.as_str();
        let cwd = existing_dir(cwd.unwrap_or(&base.pane.cwd))?;
        let command = agent_command(agent, &cwd, prompt)?;
        let target = format!("={session}:");
        let mut args = vec![
            "new-window",
            "-d",
            "-t",
            &target,
            "-c",
            &cwd,
            "-P",
            "-F",
            "#{pane_id}\t#{window_id}\t#{window_index}",
        ];
        if let Some(n) = name.filter(|n| !n.is_empty()) {
            args.extend(["-n", n]);
        }
        let out = tmux(&args)?;
        let [pane, window_id, index] = out.split('\t').collect::<Vec<_>>()[..] else {
            bail!("unexpected new-window output {out:?}");
        };
        if name.is_some_and(|n| !n.is_empty()) {
            tmux(&[
                "set-option",
                "-w",
                "-t",
                window_id,
                "@sessionizer_autoname",
                "off",
            ])?;
        }
        launch(pane, command.as_deref());
        Ok(strip_nulls(json!({
            "target": format!("{session}:{index}"),
            "pane": pane,
            "cwd": cwd,
            "command": command,
        })))
    }

    pub fn rename(&self, target: &str, name: &str) -> Result<Value> {
        let t = self.resolve(target)?;
        let p = t.pane;
        if t.scope == Scope::Session {
            check_session_name(name)?;
            if self.panes.iter().any(|q| q.session == name) {
                bail!("session {name:?} exists");
            }
            tmux(&["rename-session", "-t", &format!("={}", p.session), name])?;
            return Ok(json!({"session": name}));
        }
        if name.trim().is_empty() {
            bail!("empty name");
        }
        tmux(&["rename-window", "-t", &p.window_id, name])?;
        // Keep auto-naming from renaming it back.
        tmux(&[
            "set-option",
            "-w",
            "-t",
            &p.window_id,
            "@sessionizer_autoname",
            "off",
        ])?;
        Ok(json!({"target": format!("{}:{}", p.session, p.window_index), "name": name}))
    }

    pub fn set_category(&self, target: &str, category: Option<&str>) -> Result<Value> {
        let t = self.resolve(target)?;
        let category = category.filter(|c| !c.is_empty());
        crate::category::set(&t.pane.session, category)?;
        Ok(json!({"session": t.pane.session, "category": category}))
    }

    /// Close a session or a window (a pane target closes its window),
    /// archived first so `reopen` brings it back.
    pub fn close(&self, cfg: &Config, target: &str, confirm: bool, now: u64) -> Result<Value> {
        let t = self.resolve(target)?;
        let p = t.pane;
        if !confirm {
            bail!("pass confirm=true to close {target} (it can be reopened with reopen)");
        }
        let close = match t.scope {
            Scope::Session => CloseTarget::Session(p.session.clone()),
            _ => CloseTarget::Window {
                session: p.session.clone(),
                window_id: p.window_id.clone(),
            },
        };
        if let Some(own) = own_pane().and_then(|id| self.panes.iter().find(|q| q.id == id)) {
            let hits = match &close {
                CloseTarget::Session(s) => &own.session == s,
                CloseTarget::Window { window_id, .. } => &own.window_id == window_id,
            };
            if hits {
                bail!("{target} holds your own pane");
            }
        }
        let mut snap = snapshot::take(&self.panes, &self.states, &cfg.restore.redact, now);
        let closed = snapshot::archive_and_close(&mut snap, vec![close], CloseReason::Manual, now);
        let (_, id) = closed
            .first()
            .with_context(|| format!("could not close {target}"))?;
        Ok(json!({"closed": target, "reopen_id": id}))
    }

    /// Stop the agent and paste the command that resumes its conversation.
    pub fn suspend(&self, cfg: &Config, target: &str, force: bool) -> Result<Value> {
        let t = self.resolve(target)?;
        let p = t.pane;
        refuse_own(p)?;
        let st = self.state(p).context("no agent state")?;
        let kind = st
            .agent
            .as_deref()
            .and_then(AgentKind::from_id)
            .with_context(|| format!("no agent runs in {target}"))?;
        if kind == AgentKind::Pi {
            bail!("Pi conversations can't be resumed by id");
        }
        let id = st.agent_session_id.as_deref().context(
            "agent session id not known yet (it arrives with the agent's first hook event)",
        )?;
        if !force
            && matches!(
                st.status,
                Some(AgentStatus::Working | AgentStatus::Question | AgentStatus::Permission)
            )
        {
            bail!("{target} is busy or waiting; pass force=true to stop it anyway");
        }
        let text = crate::suspend::suspend(p, kind, id, &cfg.restore.redact)
            .context("the agent did not stop")?;
        Ok(json!({"pane": p.id, "pasted": text}))
    }

    /// Run the resume command `suspend` (or a restore) pasted.
    pub fn resume(&self, target: &str) -> Result<Value> {
        let t = self.resolve(target)?;
        if t.pane.pending_restore.is_empty() {
            bail!("nothing pasted to resume in {target}");
        }
        tmux(&["send-keys", "-t", &t.pane.id, "Enter"])?;
        Ok(json!({"resumed": t.pane.id}))
    }
}

pub fn list_closed(now: u64) -> Value {
    json!(
        snapshot::load_closed()
            .iter()
            .map(|e| json!({
                "id": e.id,
                "kind": e.kind,
                "reason": e.reason,
                "closed_secs_ago": now.saturating_sub(e.closed_at),
                "session": e.session.name,
                "windows": e.session.windows.iter().map(|w| strip_nulls(json!({
                    "index": w.index,
                    "name": w.name,
                    "cwd": w.panes.first().map(|p| &p.cwd),
                    "agent": w.panes.iter().find_map(|p| p.agent.as_ref().map(|a| &a.kind)),
                }))).collect::<Vec<_>>(),
            }))
            .collect::<Vec<_>>()
    )
}

pub fn reopen(cfg: &Config, id: u64) -> Result<Value> {
    let r = restore::reopen(id, &cfg.restore)?;
    Ok(json!({"windows": r.windows, "pasted": r.pasted, "errors": r.errors}))
}

/// Largest diff `read_diff` returns, in bytes.
const DIFF_MAX: usize = 60_000;

fn refuse_own(pane: &Pane) -> Result<()> {
    if own_pane().as_deref() == Some(pane.id.as_str()) {
        bail!("{} is your own pane", pane.id);
    }
    Ok(())
}

/// Run tmux; its trimmed stdout, or its stderr as the error.
fn tmux(args: &[&str]) -> Result<String> {
    let out = Command::new("tmux").args(args).output()?;
    if !out.status.success() {
        bail!(
            "tmux {}: {}",
            args[0],
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    Ok(String::from_utf8_lossy(&out.stdout).trim_end().to_string())
}

fn str_refs(v: &[String]) -> Vec<&str> {
    v.iter().map(String::as_str).collect()
}

fn git(cwd: &str, args: &[&str]) -> Option<String> {
    Command::new("git")
        .arg("-C")
        .arg(cwd)
        .args(args)
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| String::from_utf8_lossy(&o.stdout).into_owned())
}

/// The last `lines` lines of a pane (wrapped lines joined), without the
/// blank rows below the cursor.
pub fn capture(pane: &str, lines: usize) -> Result<String> {
    let out = tmux(&[
        "capture-pane",
        "-p",
        "-J",
        "-t",
        pane,
        "-S",
        &format!("-{lines}"),
    ])?;
    Ok(tail(&out, lines))
}

fn tail(text: &str, lines: usize) -> String {
    let all: Vec<&str> = text.trim_end().lines().map(str::trim_end).collect();
    all[all.len().saturating_sub(lines)..].join("\n")
}

fn truncate(s: &str, max: usize) -> String {
    if s.len() <= max {
        return s.to_string();
    }
    let mut end = max;
    while !s.is_char_boundary(end) {
        end -= 1;
    }
    format!(
        "{}\n[... truncated, {} more bytes]",
        &s[..end],
        s.len() - end
    )
}

fn strip_nulls(mut v: Value) -> Value {
    if let Value::Object(m) = &mut v {
        m.retain(|_, x| !x.is_null());
    }
    v
}

/// Add a window's `tokens` object and `cost_usd` (when the model is priced) to
/// its status JSON.
fn add_tokens(
    v: &mut Value,
    usage: &crate::usage::TokenUsage,
    pricing: &HashMap<String, crate::config::Pricing>,
) {
    v["tokens"] = json!({
        "input": usage.input,
        "output": usage.output,
        "cache_write": usage.cache_write,
        "cache_read": usage.cache_read,
        "total": usage.total(),
    });
    if let Some(c) = crate::usage::cost(usage, pricing) {
        v["cost_usd"] = json!(c);
    }
}

fn check_session_name(name: &str) -> Result<()> {
    if name.trim().is_empty() || name.contains([':', '.']) {
        bail!("session names must be non-empty without ':' or '.'");
    }
    Ok(())
}

fn existing_dir(path: &str) -> Result<String> {
    let path = crate::app::expand_tilde(path);
    if !std::path::Path::new(&path).is_dir() {
        bail!("{path} is not a directory");
    }
    Ok(path)
}

/// The command that starts `agent` in `cwd` with an optional first prompt.
fn agent_command(agent: Option<&str>, cwd: &str, prompt: Option<&str>) -> Result<Option<String>> {
    let prompt = prompt.filter(|p| !p.trim().is_empty());
    match agent.filter(|a| !a.is_empty()) {
        Some(a) => {
            let kind = AgentKind::from_id(a).with_context(|| {
                format!("unknown agent {a:?} (one of {})", AGENT_IDS.join(", "))
            })?;
            Ok(Some(crate::tmux::build_agent_command(
                kind, cwd, None, prompt, false,
            )))
        }
        None if prompt.is_some() => bail!("a prompt needs an agent"),
        None => Ok(None),
    }
}

/// Run `command` at the new pane's shell prompt, so the shell stays when it
/// exits.
fn launch(pane: &str, command: Option<&str>) {
    if let Some(text) = command {
        restore::paste_when_ready(vec![Paste {
            pane: pane.to_string(),
            text: text.to_string(),
            submit: true,
        }]);
    }
}

/// The numbered options of the last dialog on `screen` (`1. Yes`, `❯ 2. No`,
/// framed in `│`): the last run of lines numbered 1, 2, 3…
pub fn parse_options(screen: &str) -> Vec<(u32, String)> {
    let mut last = Vec::new();
    let mut cur: Vec<(u32, String)> = Vec::new();
    for line in screen.lines() {
        let s = line
            .trim_start_matches(|c: char| c.is_whitespace() || "│┃|❯›>".contains(c))
            .trim_end_matches(|c: char| c.is_whitespace() || "│┃|".contains(c));
        let Some((n, label)) = s.split_once(". ") else {
            continue;
        };
        let (Ok(n), label) = (n.parse::<u32>(), label.trim()) else {
            continue;
        };
        if label.is_empty() {
            continue;
        }
        if n == 1 {
            if !cur.is_empty() {
                last = std::mem::take(&mut cur);
            }
            cur.push((n, label.to_string()));
        } else if cur.last().is_some_and(|(m, _)| m + 1 == n) {
            cur.push((n, label.to_string()));
        }
    }
    if cur.is_empty() { last } else { cur }
}

fn listing(options: &[(u32, String)]) -> String {
    options
        .iter()
        .map(|(n, l)| format!("{n}. {l}"))
        .collect::<Vec<_>>()
        .join("; ")
}

/// The option for a permission decision: `yes` (once), `always` (don't ask
/// again) or `no`.
pub fn approve_option(options: &[(u32, String)], decision: &str) -> Result<u32> {
    let lower = |l: &str| l.to_lowercase();
    let always = |l: &str| {
        let l = lower(l);
        l.contains("don't ask") || l.contains("always") || l.contains("allow all")
    };
    let found = options.iter().find(|(_, l)| match decision {
        "yes" => lower(l).starts_with("yes") && !always(l),
        "always" => always(l),
        "no" => lower(l).starts_with("no"),
        _ => false,
    });
    match (found, decision) {
        (Some((n, _)), _) => Ok(*n),
        (None, "yes" | "always" | "no") => {
            bail!("no {decision:?} option among: {}", listing(options))
        }
        _ => bail!("decision must be yes, always or no"),
    }
}

/// The option `choice` names: its number, its label, or a unique part of it.
pub fn answer_option(options: &[(u32, String)], choice: &str) -> Result<u32> {
    let c = choice.trim().trim_end_matches('.');
    if let Ok(n) = c.parse::<u32>()
        && options.iter().any(|(m, _)| *m == n)
    {
        return Ok(n);
    }
    let c = c.to_lowercase();
    if let Some((n, _)) = options.iter().find(|(_, l)| l.to_lowercase() == c) {
        return Ok(*n);
    }
    let hits: Vec<u32> = options
        .iter()
        .filter(|(_, l)| l.to_lowercase().contains(&c))
        .map(|(n, _)| *n)
        .collect();
    match hits[..] {
        [n] => Ok(n),
        [] => bail!("no option matches {choice:?} among: {}", listing(options)),
        _ => bail!("{choice:?} matches several options: {}", listing(options)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pane(id: &str, session: &str, window: &str, index: u32, name: &str, active: bool) -> Pane {
        inventory::parse_panes(&format!(
            "{id}\t{session}\t{window}\t{index}\t{name}\t0\t1\t{}\t{}\t0\t0\tzsh\t/tmp\t1\t\t\t\tL\t\t\t",
            u8::from(active),
            u8::from(index == 0),
        ))
        .remove(0)
    }

    fn inv() -> Inventory {
        let panes = vec![
            pane("%1", "a", "@1", 0, "shell", true),
            pane("%2", "a", "@2", 1, "work", true),
            pane("%3", "a", "@2", 1, "work", false),
            pane("%4", "b", "@3", 0, "dup", true),
            pane("%5", "b", "@4", 1, "dup", true),
        ];
        let mut states = HashMap::new();
        let mut agent = PaneState::default();
        agent.agent = Some("claude".into());
        agent.status = Some(AgentStatus::Question);
        states.insert("%3".to_string(), agent);
        Inventory::new(panes, Some(states))
    }

    fn resolved(inv: &Inventory, t: &str) -> (Scope, String) {
        let t = inv.resolve(t).unwrap();
        (t.scope, t.pane.id.clone())
    }

    #[test]
    fn targets_resolve_to_main_panes() {
        let inv = inv();
        assert_eq!(resolved(&inv, "a"), (Scope::Session, "%1".into()));
        assert_eq!(resolved(&inv, "a:"), (Scope::Session, "%1".into()));
        // The agent pane wins over the active pane.
        assert_eq!(resolved(&inv, "a:1"), (Scope::Window, "%3".into()));
        assert_eq!(resolved(&inv, "a:work"), (Scope::Window, "%3".into()));
        assert_eq!(resolved(&inv, "a:@2"), (Scope::Window, "%3".into()));
        assert_eq!(resolved(&inv, "%2"), (Scope::Pane, "%2".into()));
        assert_eq!(resolved(&inv, "b:1"), (Scope::Window, "%5".into()));
        for bad in ["c", "a:9", "a:nope", "%9", "b:dup"] {
            assert!(inv.resolve(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn sessions_filter_by_status_and_category() {
        let inv = inv();
        let names = |v: Value| -> Vec<String> {
            v.as_array()
                .unwrap()
                .iter()
                .map(|s| s["session"].as_str().unwrap().to_string())
                .collect()
        };
        assert_eq!(names(inv.list_sessions(None, None, 0)), ["a", "b"]);
        assert_eq!(names(inv.list_sessions(None, Some("question"), 0)), ["a"]);
        assert!(names(inv.list_sessions(Some("work"), None, 0)).is_empty());
        let windows = inv.list_windows(Some("a"), 0).unwrap();
        assert_eq!(windows[1]["status"], "question");
        assert_eq!(windows[1]["panes"], 2);
        assert!(windows[0].get("status").is_none());
    }

    const PERMISSION: &str = "\
Earlier output:
1. not an option list
2. still not

╭──────────────────────────────────────────╮
│ Bash command                             │
│   rm -rf build                           │
│ Do you want to proceed?                  │
│ ❯ 1. Yes                                 │
│   2. Yes, and don't ask again for rm     │
│   3. No, and tell Claude what to do      │
╰──────────────────────────────────────────╯";

    #[test]
    fn options_come_from_the_last_dialog() {
        let opts = parse_options(PERMISSION);
        assert_eq!(
            opts,
            [
                (1, "Yes".to_string()),
                (2, "Yes, and don't ask again for rm".to_string()),
                (3, "No, and tell Claude what to do".to_string()),
            ]
        );
        assert!(parse_options("no dialog here\n2. orphan").is_empty());
        let codex = "  › 1. Yes, proceed (y)\n    2. No, and tell Codex what to do (esc)";
        assert_eq!(parse_options(codex).len(), 2);
    }

    #[test]
    fn decisions_and_answers_pick_options() {
        let opts = parse_options(PERMISSION);
        assert_eq!(approve_option(&opts, "yes").unwrap(), 1);
        assert_eq!(approve_option(&opts, "always").unwrap(), 2);
        assert_eq!(approve_option(&opts, "no").unwrap(), 3);
        assert!(approve_option(&opts, "maybe").is_err());
        assert!(approve_option(&opts[..1], "no").is_err());

        assert_eq!(answer_option(&opts, "3").unwrap(), 3);
        assert_eq!(answer_option(&opts, "yes").unwrap(), 1);
        assert_eq!(answer_option(&opts, "don't ask").unwrap(), 2);
        assert!(answer_option(&opts, "9").is_err());
        assert!(answer_option(&opts, "and").is_err());
    }

    #[test]
    fn long_diffs_are_cut_on_a_char_boundary() {
        assert_eq!(truncate("abc", 5), "abc");
        let cut = truncate("aé", 2);
        assert!(cut.starts_with("a\n[... truncated, 2 more bytes]"), "{cut}");
    }
}
