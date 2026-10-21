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

## Roadmap v2 (2026-10)

The agent-workspace work (status, naming, categories, restore, suspend, MCP,
sessions view) is tracked in `PLAN.md`. This section is what comes after it,
drawn from a survey of similar tools (see *Landscape* below).

**Guiding constraint:** showrunner lives inside the user's own tmux. Features
that need an owned multiplexer, a replaceable UI or native Windows are out of
scope (see *Not planned*).

### Landscape (surveyed 2026-10-19)

| Tool | Stars | Backend | What to learn from it |
|------|-------|---------|-----------------------|
| [Herdr](https://github.com/herdrdev/herdr) | 44.8k | own multiplexer | ~24 agents; "add Herdr support" protocol for agent authors; layout snapshots; plugins; mobile PWA (Collie) |
| [Vibe Kanban](https://github.com/BloopAI/vibe-kanban) | 28.4k | web app | kanban issues → agent workspaces; inline diff comments sent to the agent; PR + merge |
| [Claude Squad](https://github.com/smtg-ai/claude-squad) | 8.6k | tmux | `-y` auto-accept; pause/checkout |
| [workmux](https://github.com/raine/workmux) | 2.9k | user's tmux | closest in spirit: status in window names, `merge` cleans up worktree + window + branch, sandbox |
| [CCManager](https://github.com/kbwo/ccmanager) | 1.3k | own PTY | `.worktreeinclude`; AI-verified auto-approval; devcontainer |
| [Agent Deck](https://github.com/asheshgoplani/agent-deck) | 1.1k | tmux | message queue; fork + parent link; cost dashboard; Telegram/Slack conductor; MCP/skills manager; Docker sandbox |
| [thurbox](https://github.com/Thurbeen/thurbox) | 95 | own tmux server | multi-repo sessions; fork + lead/worker tree; agent mailbox; scheduled automations; agents as TOML; Lua UI |

What only showrunner has: a native MCP server with guards (own-pane
protection, `read_only`), six statuses including question vs permission with the
pending options, per-session CPU/mem/GPU, and suspend of idle agents.

### Phase 5 — Task board + agent dispatch

A kanban board over a pluggable task source. Cards moved to **Ready** are
spawned as agent sessions automatically, up to a concurrency cap, and the card
follows the session through review to done.

**Canonical columns.** Every provider maps its own states onto these:

| Column | Meaning | Set by |
|--------|---------|--------|
| Backlog | not for agents yet | human |
| Ready | eligible for dispatch | human |
| Running | a session is working on it | dispatcher |
| Needs you | session is `question`, `permission` or `error` | dispatcher (from session status) |
| Review | agent finished; PR open | dispatcher |
| Done | PR merged / card closed | dispatcher or human |

*Needs you* is derived from session status. Providers without a matching
state show it as a label or as a badge on the card.

**Providers.** Each implements one trait (`src/tasks/`), so the board, the
dispatcher and the UIs never know which one is behind a board:

```rust
trait TaskProvider {
    fn list(&self) -> Result<Vec<Card>>;                  // cards + their column
    fn move_to(&self, card: &CardId, col: Column) -> Result<()>;
    fn comment(&self, card: &CardId, body: &str) -> Result<()>;
    fn link(&self, card: &CardId, kind: Link, url: &str) -> Result<()>; // branch, PR
}
// Card { id, key ("SR-42"), title, body, column, labels, priority, url }
```

| Provider | API | Auth | Notes |
|----------|-----|------|-------|
| `native` | `~/.showrunner` | none | extends `Task` with `column` + `body`; built first, defines the trait |
| `plane` | REST `/api/v1/workspaces/{slug}/projects/{id}/…` (work items, states, comments) | `X-API-Key` | self-hosted on the LAN; check endpoint names against the instance's version |
| `github` | Projects v2 GraphQL (`updateProjectV2ItemFieldValue` on the Status field) | reuses `gh auth` | items are issues; PR link is native |
| `jira` | REST v3 `/rest/api/3/search/jql`, `/issue/{key}/transitions` | API token | comments are ADF |

**Config** (`~/.showrunner/config.toml`). One board per block, each bound
to one showrunner project (repo):

```toml
[[boards]]
name = "sessionizer"
provider = "plane"            # native | plane | github | jira
project = "tmux-sessionizer"  # showrunner project the cards run in
agent = "claude"
max_running = 2               # concurrency cap for this board
prompt_template = "~/.showrunner/prompts/task.md"

[boards.plane]
url = "http://plane.lan"
workspace = "joserdf"
project_id = "…"
api_key_env = "PLANE_API_KEY"  # secrets come from env, never the file

[boards.columns]              # provider state -> canonical column
"Backlog" = "backlog"
"Ready for agent" = "ready"
"In Progress" = "running"
"In Review" = "review"
"Done" = "done"
```

**Dispatcher** (daemon loop, poll every 60 s; webhooks later):
1. Per board, count Running cards whose session is alive. While below
   `max_running` (and below a global `[dispatch] max_running`), take the next
   Ready card by provider rank/priority.
2. Create the task: branch `sr/<key>-<slug>`, a worktree (existing
   `copy_patterns` / `setup_commands`), and a session with the board's agent.
   The first message is the prompt template rendered with the card's title,
   body and URL.
3. Move the card to Running and comment the branch and session name.
4. Map session status onto the card: `question`/`permission`/`error` → Needs
   you; back to `working` → Running.
5. On `done`: if the branch has commits, push and open the PR (existing ops),
   link it and move to Review. If not, move to Needs you with the agent's
   last output as a comment.
6. On PR merged → Done: archive-and-close the session (existing snapshot path).

Rules:
- **Idempotent.** The card id is stored on the `SessionRecord`. A card with a
  live or archived task is never dispatched twice, including after a daemon
  restart.
- **Pausable.** `showrunner board pause|resume`, and daemon off (`Alt+a`)
  stops dispatching.
- **No silent escalation.** Dispatched agents run in their normal permission
  mode. A board must opt in explicitly to any bypass flag.
- `--dry-run` prints what would be dispatched.

**UI (both):**
- **Web** (`/board` on the daemon): one column per canonical state, drag
  between columns (calls `move_to`), live session status on each card via the
  existing SSE, click a card for output and diff, plus a "dispatch now"
  button.
- **TUI**: `showrunner board` and a key in the sessions view. Compact columns
  with counts; `Enter` on a card switches to its session; `m` moves the card.
- **MCP**: `list_cards(board, column?)`, `move_card`, `dispatch(card)` (lays
  the groundwork for a lead agent, Phase 6).
- **CLI**: `showrunner board list|dispatch|pause|resume`.

**Order:** native provider + trait → dispatcher → TUI board → web board →
Plane → GitHub Projects → Jira.

**Definition of done:** a card moved to Ready in Plane becomes a running
session within one poll, shows Needs you when the agent asks something, opens a
PR when the agent finishes, and the cap holds with N Ready cards.

**Status (2026-10-20):** native, Plane, dispatcher (full status mapping,
auto push + PR, merged → Done + archive), TUI board, web board (status badge,
output/diff panel), CLI and MCP tools (`list_cards`, `move_card`, `dispatch`)
are in. The Plane provider passed a live round trip (list, move, comment)
against a private test project, and a live daemon run (2026-10-20) met the
definition of done: Ready → session within one poll, question → Needs you,
commits → push + PR → Review, PR merged → Done + session archived. Found on
that run: Claude's folder-trust dialog (fresh repo) left the card Running with
no signal (fixed: it is now an attention marker); a card in Needs you no
longer counts against `max_running`; and an agent can `cd` out of its worktree
and commit on the main checkout (the card then lands in Needs you with no
commits). GitHub Projects and Jira are not started.

Open questions:
- ~~Plane URL and version on the LAN~~ — Plane 1.4.2 serves both `issues/` and
  `work-items/`; the provider uses `work-items/`.
- Webhooks need the daemon reachable from the LAN (it binds `127.0.0.1:7878`
  today). Keep polling as the default; add webhooks with a secret as an opt-in.
- Boards spanning several repos: map card → repo by a label (`repo:<name>`) or
  keep one board per showrunner project.

### Phase 6 — Agent control

- **Message queue.** `send_message` to a `working` agent queues instead of
  refusing, and is delivered on the next `idle`/`done` (built on
  `wait_for_status`). `force` keeps today's interrupting behaviour. (Agent Deck)
- **Fork a session** (Claude `--fork-session`, Codex fork) with a parent link on
  the `SessionRecord`. The sessions view nests children under their parent.
  (thurbox, Agent Deck)
- **Orchestration MCP tools:** `spawn_worker(prompt, project?)`,
  `children(session)` and `collect(session)`, so a lead agent can fan out
  work and gather results. This shares the spawn path with the Phase 5
  dispatcher. (thurbox Fleet, Herdr agent-automation)

**Status (2026-10-20):** all three are in. The message queue landed earlier.
Fork: `showrunner fork`, the sessions view `F` key and the MCP `fork` tool; a
fork shares the source's worktree, and Pi has no fork. Orchestration takes
the parent task session instead of a project: `spawn_worker(parent, prompt,
agent?)` starts a session of the parent's task in its own worktree, so workers
merge back like any other session of the task.

### Phase 7 — Observability and reach

- **Token cost per session** in the `RES` column and the resource panel, read
  from the Claude/Codex transcripts that search already parses. (Agent Deck)
- **Phone notifications** via ntfy or Telegram on `question` / `permission` /
  board Needs you, sent from the daemon next to the OS notifier. (Agent Deck,
  Herdr plugin)

**Status (2026-10-20):** token cost is in. Phone notifications go through
carcara-duo (Telegram) rather than the daemon: the daemon serves MCP over
streamable HTTP at `/mcp` with a bearer token (`~/.showrunner/mcp_token`),
and duo polls status and turns permission and question prompts into Telegram
cards that call `approve` / `answer`. The duo side lives in the carcara-duo
repo.

### Phase 8 — Agents and review

- **Agents as data**: a TOML file with command, resume command, hook/scrape
  status strategy and window-name rules, so new agents need no recompile. Add
  Gemini, Copilot CLI, Cursor Agent and Amp. (thurbox `agents.toml`, Herdr)
- **Diff comments to the agent**: comment lines during hunk review, then send
  them to the session as one message. (Vibe Kanban)
- **`.worktreeinclude`**: also honour a repo-level `.worktreeinclude`
  (gitignore syntax) in addition to the per-project `copy_patterns`. (CCManager,
  Agent Deck)

**Status (2026-10-20):** diff comments and `.worktreeinclude` are in. The TUI
forwards hunk/difit comments, and the web diff view takes per-line notes sent
through `POST /api/sessions/{name}/review`. Agents as data is hybrid: the four
built-ins stay in code (hooks, transcripts, usage), and Gemini, Copilot CLI,
Cursor Agent and Amp are `agents.toml` entries (bundled, overridable from
`~/.showrunner/agents.toml`) with pane-scraped status. Cursor and Amp are
unverified (not installed).

### Not planned

- Own multiplexer / own tmux server (Herdr, thurbox): would give up living in the
  user's tmux.
- Replaceable Lua UI and a plugin marketplace (thurbox, Herdr).
- Native Windows; SSH hosts in one list (revisit if remote agents become a need).
- AI auto-approval of permission prompts (CCManager): risky. The board's
  Needs you column plus phone notifications cover the same pain.
