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

- [ ] Organized project view (Projects→Tasks→Sessions, worktrees) — inherited
- [ ] Active agent sessions view (claude/opencode/codex) with status — extend
- [ ] Diff visualization via hunk (kept) — inherited
- [ ] Fast project switching — extend
- [ ] Open/close/manage sessions — inherited
- [ ] Compute resource management (CPU/mem/GPU, all sessions) — **new**
- [ ] Auto-close sessions (idle + finished, safeguarded) — **new**
- [ ] Send messages via tmux send-keys — adapt
- [ ] Notifications — extend

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

## Next step
Start **Phase 0**: fork/adapt showrunner into this repo and stand up the Rust workspace
with the daemon + SSE skeleton. Confirm before implementation begins.
