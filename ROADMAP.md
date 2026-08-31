# tmux-sessionizer — Plan & Roadmap

Turning this repo into a TUI for AI-assisted development: a tmux-native dashboard to
organize projects, monitor and manage multiple AI coding agents (Claude Code, OpenCode,
Codex), review file changes, manage compute resources, and auto-close sessions.

## Decisions (locked)

| Topic | Decision |
|-------|----------|
| **Stack** | Rust + Ratatui (+ crossterm + tokio + axum/SSE) |
| **Base tool** | Adopt/adapt **showrunner** (Bendzae/showrunner) — Rust/Ratatui, tmux-backed, Projects/Tasks/Sessions with worktrees, diff review, `send`/`ask` CLI, mobile web UI |
| **Bash role** | Becomes a lightweight **bootstrap plugin**: TPM entry (`sessionizer.tmux`) binds `Alt+s` (open TUI), `Alt+a` (daemon on/off), `Alt+n` (new session), auto-starts the daemon. All heavy logic moves to Rust. |
| **Event transport** | **Daemon + HTTP/SSE on localhost** (ccmux-style). Daemon owns state; TUI and web UI are SSE clients. |
| **Milestone 1** | Projects + Agents + fast switch |
| **Diff viewer** | Keep **hunk** (showrunner's default) |
| **Resource scope** | **All tmux sessions + GPU** (nvidia-smi) |
| **Auto-close policy** | **Idle + agent-finished**, with safeguards; both configurable and disable-able |
| **Rendering** | Main dashboard in Ratatui; diff and resource panel open via **tmux `display-popup`** (external viewers) |

## Architecture

```
tmux (sessions hold agent panes + run sessions)
   ▲  hooks (Claude/OpenCode/Codex) + process scan + tmux -C
   │  write state events
DAEMON  showrunnerd  (Rust/tokio)
   • discover agents; correlate pane ↔ session ↔ project
   • collect CPU/mem/GPU per session
   • auto-close policy
   • HTTP + SSE on localhost
   ▲  SSE
TUI  showrunner  (Ratatui)  — and web UI
   views: Projects · Agents · Diff · Resources · Notifications
   ▲
bash bootstrap (sessionizer.tmux): Alt+s / Alt+a / Alt+n
```

## Data model (inherited from showrunner)

- **Project** — a git repo added by path.
- **Task** — a unit of work tied to a git branch. Main session works on the task branch; extra sessions get per-session worktree branches.
- **Session** — an agent instance (claude / codex / pi; **add opencode**) running in a tmux session, by default in its own git worktree.
- **Adhoc session** — project-scoped, no task/worktree.
- **Run session** — per-project command (`npm run dev`, etc.) in a dedicated tmux session.

State lives in `~/.showrunner/`.

## Components to build / adapt

### A. Daemon (`showrunnerd`) — new
- Owns the authoritative session/agent state.
- Exposes `GET /events` (SSE) and a small JSON API. showrunner already has an HTTP server (`serve`) for the web UI — extend it for SSE and make the TUI subscribe.
- Correlates a pane to a session/project via `pane_current_path` + hook markers + process name.

### B. Agent detection & status — extend
- **Hooks (authoritative):** wire each agent to emit events to the daemon.
  - **Claude Code**: `Notification` (matcher `permission_prompt`/`idle_prompt`/`agent_needs_input`/`agent_completed`), `SessionStart/End`, `Stop` in `~/.claude/settings.json`. (Your `helpers/claude-hook.sh` is the prototype.)
  - **OpenCode**: plugin subscribing to event bus (`session.idle`, `session.error`, `session.diff`, …). (Your repo already reads `opencode-hook.state`.)
  - **Codex**: `hooks.json`/`[hooks]` — `SessionStart`, `SessionEnd`, `PermissionRequest`, `Stop`; plus `notify` for `agent-turn-complete`.
- **Fallback detection:** scan panes by process name + parse visible prompt (bosun's prompt-box approach) when hooks are absent.
- **States:** `idle` · `working` · `waiting` (permission/question) · `error` · `done`.

### C. Resource monitoring — new
- Per-session CPU/mem: `tmux list-panes -F '#{pane_pid}'` → walk process tree → sum usage. (Your `ui.sh` already does the tree-walk pattern.)
- GPU per process: `nvidia-smi` (pattern from tmux-cpu).
- Applies to **all tmux sessions**, not just agents.
- Rendered via tmux `display-popup` (external viewer) per decision.

### D. Auto-close — new
- Policy: close when **idle** (no input + low CPU) OR agent **finished** (`Stop`/`SessionEnd` hook).
- Safeguard: never auto-close a worktree with uncommitted changes without confirmation.
- Both triggers configurable and disable-able.

### E. Messaging — adapt
- showrunner already has `send`/`ask` (CLI). Expose **quick-reply in the TUI** that sends via `tmux send-keys -t pane "text" Enter`.

### F. Notifications — extend
- Unify hooks from all three agents → daemon → TUI badge + OS notify (`notify-send`/`terminal-notifier`) + bell. Your `alerts.sh`/`alert_status.sh` already cover the indicator + status-right.

### G. Projects / discovery — extend
- showrunner adds projects by path manually. Add discovery by scanning configured dirs (tmux-sessionizer pattern) + zoxide/ghq.
- Fast switch via fuzzy palette (`/` already exists in showrunner).

## Feature checklist (your requirements)

- [x] Organized project view (Projects→Tasks→Sessions, worktrees) — inherited
- [x] Active agent sessions view (claude/opencode/codex) with status — extend
- [x] Diff visualization via hunk (kept) — inherited
- [x] Fast project switching — extend
- [x] Open/close/manage sessions — inherited
- [x] Compute resource management (CPU/mem/GPU, all sessions) — **new**
- [x] Auto-close sessions (idle + finished, safeguarded) — **new** (opt-in; off by default)
- [x] Send messages via tmux send-keys — adapt
- [x] Notifications — extend (tmux status-bar badge + OS notify; the "bell" was dropped as non-functional)

## Roadmap

### Phase 0 — Foundation
Fork showrunner, build from source, understand the codebase. Set up the Rust workspace
(crates: `daemon`, `tui`, `tmux`, `agents`, `resources`). Wire bash bootstrap
(`sessionizer.tmux`) to launch the binary + daemon. Verify build/test.

### Phase 1 — Milestone: Projects + Agents + Switch
- Daemon with SSE (`/events`) + JSON API; TUI subscribes instead of polling.
- Projects view (inherited) + Agents view with status (hooks + scan fallback).
- Add **OpenCode** harness.
- Fast project/session switching.
- *Definition of done:* dashboard shows projects and live agent states; switching works; daemon runs detached.

### Phase 2 — Messaging & control
- Quick-reply in TUI (`send-keys`).
- Approval flows (y/n, numbered choices).
- Spawn sessions by agent type; kill/restart from TUI.

### Phase 3 — Resources + auto-close
- Per-session CPU/mem (+ GPU) collection.
- Resource panel in tmux popup.
- Auto-close policy engine with safeguards + config.

### Phase 4 — Diff & polish
- Keep hunk diff (already integrated); wire into project/task/session contexts.
- Unify notifications (badge + OS notify + bell).
- Themes, full config, keybindings, docs, TPM install docs.

## Status (2026-08) & next up

**Phases 0–4 are complete.** It all ships as a single Rust
binary (`showrunner`) — daemon + TUI + web UI + CLI in one crate (not the
 multi-crate workspace Phase 0 sketched). `cargo build` + `cargo test` (180 pass)
 are the gates; the legacy bash TUI was dropped in favour of the Rust one.

Actuals vs. the locked decisions above:
- **Rendering:** the diff and resource panels are **in-TUI overlays** (Ratatui),
  not `tmux display-popup` external viewers — the popup decision was superseded.
- **Notifications:** unified to the tmux status-bar badge (daemon-written
  `status.cache`) + OS notify. The "bell" was never functional and was removed.
- **Authoritative hooks:** wired for **Claude, OpenCode, and Codex** — Claude via the
  plugin `hooks.json`, OpenCode via an auto-loaded `.opencode/plugins/` file, Codex via
  the legacy `notify` (turn-complete). Each POSTs to `/api/hook`; the worker correlates
  by `cwd` and applies the status. Pi uses the pane-scrape fallback.

Remaining / deferred:
- **display-popup** diff/resource viewers — superseded by the in-TUI overlays
  (better UX); not reimplementing.
- **Tighter idle/finished policy** — already satisfied: hook events apply as a
  one-shot status override on the next tick (zero pane-scrape latency), and the
  3-tick stability + finished guards cover the hook-less cases.
