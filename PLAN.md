# Plan: agent-aware tmux workspace — status, naming, categories, persistence, MCP, light TUI

## Context

The Rust TUI only models `cm__project__task__session` sessions (`TmuxSession::from_tmux_name`,
`src/tmux.rs:159`), so plain sessions like `main`, `home`, `gates` get no status, and the
Projects → Tasks model is mandatory. Status is per session, not per window, and hooks are
correlated by `cwd` only (`src/worker.rs:433-460`), which breaks with several agents in one
directory. Agents started outside showrunner send no hooks at all (no global hooks in
`~/.claude/settings.json`), and the daemon is not running because TPM still loads the old
bash plugin. The user wants: categories, inactivity control with reopen, clear per-window agent
status, windows named after the agent session, an MCP to drive everything, full restore after a
reboot, and a lighter TUI.

## Approach

Move the core model from "showrunner sessions" to **every tmux pane**: one `tmux list-panes -a`
per tick builds an inventory, agent panes are detected by process (existing
`AgentKind::process_name`), and hook events are matched by `$TMUX_PANE` (cwd as fallback). The
daemon (`showrunner serve`) stays the single source of truth and becomes always-on via a systemd
user unit; tmux, TUI, web UI and MCP all read from it. Status is also published as tmux window
options (`@agent_status`), so even the old `Alt+s` picker and the status bar benefit before the new
TUI lands. Persistence is a periodic JSON snapshot plus a "closed" history derived by diffing
snapshots, restored with each agent's native resume flag.

Rejected alternatives:
- **tmux-resurrect + continuum** for persistence: they restore panes but not agent conversations
  (no session ids), and they'd be a second state store next to the daemon.
- **Pure bash + state files**: re-implements hooks/status/API the daemon already has.
- **MCP talking to tmux directly**: duplicates daemon logic and races with it; the MCP is a thin
  client of the daemon HTTP API instead.

## Preparation

- `git checkout -b feat/agent-workspace` (one PR per phase below, rebased on `main`).
- `cargo build --release && cargo test` green on `main` (190 pass, 1 ignored at `ec7442d`).
- Phase 6 only: add `rmcp = "3.5.1"` (latest on crates.io as of 2026-10-13); confirm the
  server/tool macro API via Context7 before coding.

## Verified facts this plan relies on

- Claude Code sets the pane title to `✳ <session name>` (live: `gates:1 → "✳ gates-presentation"`)
  while tmux auto-names the window `claude`.
- Resume flags: `claude --resume <session-id>`, `opencode -s <session-id>`, `codex resume <id>`.
- Claude hook payloads carry `session_id` and `cwd` (`src/hooks.rs` tests, `src/server.rs:614-621`).
- `swap-pane -d` keeps the active pane (verified on an isolated tmux 3.2a server).
- `/proc/<pane_pid>/stat` field 8 (`tpgid`) → `/proc/<tpgid>/cmdline` recovers the full foreground
  command line (live: `main:0 → btop`, `home:0 → claude --resume home`, `gates:0 → claude --resume`).

## Implementation steps

### Phase 0 — Foundation: pane inventory, global hooks, always-on daemon

1. **`contrib/systemd/showrunner.service`** (new): user unit, `ExecStart=%h/tmux-sessionizer/target/release/showrunner serve`,
   `Restart=on-failure`, `WantedBy=default.target`. `scripts/daemon.sh` stays for TPM users; it
   already no-ops when the port is in use (`_port_in_use`).
2. **`src/cli.rs:54`**: add `showrunner hooks install|uninstall|status`. Merges idempotently into
   `~/.claude/settings.json` (backup to `settings.json.bak-<ts>` first), installs the OpenCode
   plugin globally (`~/.config/opencode/plugins/`) and the Codex `notify` entry. Prints a diff
   and asks before writing.
3. **`showrunner-plugin/hooks/post-event.sh`** (+ OpenCode/Codex clients): add
   `ev["tmux_pane"] = os.environ.get("TMUX_PANE")`.
4. **`src/inventory.rs`** (new): `Pane { id, session, window_id, window_index, window_name, cwd,
   command, title, pid, activity_ts, active }` parsed from a single
   `tmux list-panes -a -F …` call (absorbs `list_all_pane_pids`, `src/tmux.rs:988`).
   `PaneState { agent: Option<AgentKind>, status, status_since, agent_session_id, unread }`
   keyed by pane id. Pure parser + unit tests on fixture output.
5. **`src/server.rs:621`** and **`src/worker.rs:433-460`**: correlate hook events by `tmux_pane`
   first, `cwd` second. Store `session_id` from `SessionStart` into `PaneState.agent_session_id`.
6. **`src/worker.rs:30` `WorkerUpdate`**: add `panes: Vec<Pane>` and `pane_states:
   HashMap<String, PaneState>`. Keep the existing `sessions`/`statuses` fields (web UI and the
   Projects view keep working).

### Phase 1 — Richer status, visible in tmux (depends on 0)

1. **`src/tmux.rs:23`**: replace `SessionStatus` with `AgentStatus { Working, Question,
   Permission, Done, Idle, Error, Exited }`. Keep `as_str()` wire values backward compatible for
   the web UI (`Question → "waiting_input"`, `Permission → "waiting_permission"`, …) and add a new
   `code()` for v2 consumers.
2. **`src/hooks.rs:17,40,63`**: add `AgentEvent::Question`; Claude `PreToolUse` with
   `tool_name == "AskUserQuestion"` and the `agent_needs_input` notification map to it.
   Mapping: `Stop → Done (unread)`, `idle_prompt → Idle`, `PermissionRequest → Permission`,
   `SessionEnd → Exited`, `Error → Error`. Add the `PreToolUse` matcher to
   `showrunner-plugin/hooks/hooks.json`.
3. **Read tracking** (`src/worker.rs`): a `Done` pane becomes `Idle` once it has been the active
   pane of an attached client for ≥2 s (`#{pane_active}` + `#{session_attached}` from the
   inventory), so "finished, result unread" is distinct from "idle".
4. **Publish to tmux** (`src/inventory.rs` → `apply_window_options`): per window, aggregate the
   worst pane status (`Permission > Question > Error > Done > Working > Idle`) into
   `@agent_status` and `@agent_idle` ("12m"). Only on change, all changes chained into **one**
   `tmux` invocation with `\;`.
5. **`contrib/tmux/status.conf`** (new): `window-status-format` snippet that renders the glyph
   from `@agent_status`; documented, not auto-installed.
6. **`src/notify.rs`**: notify on transitions into `Question`, `Permission`, `Done`.

**As built (deviations):**
- `AgentStatus` was added per pane in `src/inventory.rs` instead of replacing `SessionStatus`;
  the existing TUI/web/autoclose pipeline is unchanged and Phase 7 consumes `AgentStatus`.
  `Exited` was dropped: a pane whose agent exited has no status.
- "Working" comes from async Claude hooks `UserPromptSubmit`/`PostToolUse` and OpenCode
  `session.status busy`; agents without turn-start hooks (Codex, Pi) fall back to
  `window_activity` after their last event.
- `AskUserQuestion` also fires `PermissionRequest` + a `permission_prompt` notification; a
  permission event while a question is pending keeps `Question`.
- `idle_prompt` now maps to `Idle`; `elicitation_dialog` to `Question`.

### Phase 2 — Windows named after the agent session (depends on 0; parallel with 3)

1. **`src/naming.rs`** (new): `clean_title(raw) -> Option<String>` strips leading spinner/glyph
   chars (Braille U+2800–U+28FF, `✳ ✻ ⬡ ⬢ ·`), trims, rejects generic titles (`""`, `claude`,
   `Claude Code`, `opencode`, hostname), truncates to `max_len`. Pure + table tests.
2. **Daemon apply**: for each window with an agent pane (the active one if several), when
   `@sessionizer_autoname != "off"`: `set-window-option automatic-rename off` + `rename-window`,
   recording the name in `@sessionizer_lastname`. If `window_name != @sessionizer_lastname`
   (the user renamed it), set `@sessionizer_autoname off` and never touch it again.
3. **`src/config.rs:514` `Config`**: `[naming] enabled = true, max_len = 32`.

### Phase 3 — Manual categories (depends on 0; parallel with 2)

1. Category lives in the tmux session option `@category` (so plain tmux tools see it) and is
   captured by the snapshot (Phase 4) for restore.
2. **`src/cli.rs`**: `showrunner category set <session> <name>` / `clear` / `list`.
3. **`src/server.rs:60-79`**: `POST /api/v2/sessions/{name}/category`; inventory exposes it.

### Phase 4 — Persistence: snapshot, closed history, restore after reboot (depends on 0, 3)

1. **`src/snapshot.rs`** (new): `Snapshot { taken_at, sessions: [SessionSnap { name, category,
   windows: [WindowSnap { index, name, autoname, layout, active, panes: [PaneSnap { index, cwd,
   active, argv: Option<Vec<String>>, agent: Option<AgentResume { kind, session_id }> }] }] }] }`.
   Atomic write (tmp + rename) to `~/.showrunner/snapshot.json` every 30 s and on structural
   change; keep 5 rotations.
   - `argv` is the **full command line of the pane's foreground process**, not just
     `#{pane_current_command}`: read `tpgid` from `/proc/<pane_pid>/stat`, then
     `/proc/<tpgid>/cmdline` (NUL-separated). `None` when the foreground process is the shell
     itself (an idle prompt). Pure parser for `stat`/`cmdline` with unit tests.
2. **Closed history** `~/.showrunner/closed.json`: diff previous vs. current inventory; a vanished
   window or session becomes `{ id, kind, snap, closed_at, reason: auto_close|manual }`. Ring of
   100 entries / 7 days. If the tmux server itself is gone, record nothing (reboot or crash is
   what the snapshot covers).
3. **`src/restore.rs`** (new): `restore_session(&SessionSnap)` / `restore_window(session,
   &WindowSnap)`: `new-session -d` / `new-window` / `split-window` with each pane's `-c cwd`,
   then `select-layout <layout>`, restore names, `@category`, `@sessionizer_autoname`.
   Existing sessions are merged (missing windows added), never duplicated. Reuses
   `recreate_session` (`src/tmux.rs:552`) for `cm__` sessions that need worktree metadata.
   - **Each pane gets its previous command pasted at the prompt, not executed**:
     `tmux send-keys -t %pane -l "<cmd>"` with no `Enter`. The user reviews it and presses
     Enter. So no command allowlist is needed and nothing runs by surprise.
   - **Agent panes** get the command that resumes *the same agent session*, built by
     `restore::resume_command(kind, argv, session_id)`: the original argv with any
     `--resume/-r/--continue/-c/-s/--session` (and their values) removed, then
     `claude … --resume <id>` / `opencode … -s <id>` / `codex resume <id> …`. So flags like
     `--dangerously-skip-permissions` or `--model` are kept. Unit-tested per agent.
     `--resume` takes an *optional* value, so it drops the next token only when that token
     doesn't start with `-` (live cases: `claude --resume`, `claude --resume home`).
     If no `session_id` was captured (e.g. a pane started before `hooks install`), paste the
     original argv unchanged.
   - **Other panes** get `argv` re-quoted with the existing `shell_escape` (`src/tmux.rs:124`).
     Idle shells get nothing.
   - Paste only after the shell is ready: poll `capture-pane` until the prompt line is non-empty
     (max 3 s), so typeahead isn't lost to zsh/p10k init.
   - `[restore] run_agents = false` (default): when true, agent resume commands also get `Enter`.
4. **Boot restore**: on daemon start, if `[restore] on_start = true` and none of the snapshot's
   sessions exist, restore everything and set the server option `@sessionizer_restored 1`. The
   unit starts at login, so `.zshrc`'s `tmux new-session -A -s main` attaches to the restored
   `main`. **Snapshotting is paused during a restore** so a half-restored state can't overwrite
   the snapshot.
5. **CLI**: `showrunner restore [--dry-run]`, `showrunner closed list`, `showrunner reopen <id>`.

### Phase 5 — Inactivity and auto-close for windows and any session (depends on 1, 4)

1. Idle time per window: agent panes `now - status_since` when `Idle`/`Done`; shell panes
   `now - activity_ts`. Exposed in the inventory and in `@agent_idle`.
2. **`src/autoclose.rs:73,90`**: add `blocked` (any pane in `Question`/`Permission`) to
   `SessionCloseState`. A blocked target never closes. Keep the existing `respect_dirty → Confirm`.
3. Scope: `[auto_close] scope = "agents" | "all"`, `exclude_categories = ["pinned"]`, and a
   per-session `@sessionizer_keep 1` opt-out. Still `enabled = false` by default.
4. Every auto-close first writes a closed-history entry with `reason = auto_close`, so it's always
   reopenable.

### Phase 6 — MCP server (depends on 0, 1, 3, 4)

1. **`src/server.rs`** v2 endpoints, with generic targets (`session`, `session:window`, `%pane`):
   `GET /api/v2/inventory`, `POST /api/v2/sessions`, `POST /api/v2/windows`,
   `POST /api/v2/targets/{t}/{send,keys,rename,close}`, `GET /api/v2/targets/{t}/output?lines=`,
   `GET /api/v2/closed`, `POST /api/v2/closed/{id}/reopen`.
2. **`src/mcp.rs`** (new) + `showrunner mcp` (stdio, `rmcp`): a thin client of the v2 API.
   Tools: `list_sessions(category?, status?)`, `list_windows(session)`, `get_status(target)`,
   `read_output(target, lines ≤ 500)`, `send_message(target, text, submit = true)`,
   `send_keys(target, keys[])`, `create_session(name, cwd, agent?, prompt?, category?)`,
   `create_window(session, cwd?, agent?, prompt?, name?)`, `rename(target, name)`,
   `set_category(session, category)`, `close(target, confirm)`, `list_closed()`, `reopen(id)`,
   `wait_for_status(target, statuses[], timeout_s ≤ 600)`.
3. Safety: `send_message` refuses a `Working` target unless `force`, and refuses the caller's own
   pane (`TMUX_PANE` of the MCP process). `close` requires `confirm: true`.
   `[mcp] read_only = false`. The daemon stays bound to `127.0.0.1`.
4. Docs: `claude mcp add sessionizer -- showrunner mcp` (+ OpenCode/Codex equivalents).

### Phase 7 — Light TUI: the "Sessions" view (depends on 1–5)

1. **`src/ui_sessions.rs`** (new, keeps `src/ui.rs` from growing past 2120 lines): a tree
   *Category ▸ Session ▸ Window* rendered from the inventory. Rows show the glyph, window name,
   agent and idle time: `● working`, `? question`, `! permission`, `✓ done` (bold = unread),
   `○ idle`, `✗ error`. Uncategorized sessions go last.
2. **`src/app.rs:18` `ListItem`**: add `Category`, `Window`. The view becomes the default; the
   current Projects/Tasks view stays behind `p`.
3. Minimal keys: `Enter` switch to window, `j/k`, `/` filter, `a` attention-only, `n` next
   needing attention, `r` reply inline, `y` approve permission, `c` category, `R` rename,
   `x` close (reopenable), `u` reopen list, `p` projects view, `q` quit.
4. Layout: no live split by default (`[ui] live_pane = false`). A right-hand preview
   (capture-pane tail) only when width > 140 columns. The popup only.
5. Startup reads the daemon snapshot over SSE (already `Worker::connect_remote`); no blocking tmux
   calls before the first frame.
6. Binding: ship the Rust TUI on `Alt+S` while the old bash picker keeps `Alt+s`. Swap only when
   the user approves.

## Edge cases and risks

- **Several agents in one window**: the window aggregates the worst status; naming uses the
  active agent pane.
- **Generic or spinner-only pane titles**: `clean_title` returns `None` and the window keeps its
  name.
- **User renames a window**: detected through `@sessionizer_lastname` and auto-naming stops for
  that window.
- **tmux server killed (not a reboot)**: no closed-history flood (rule 4.2). A restore can be
  triggered manually.
- **Partial restore / existing sessions**: merge by session name + window index, never
  duplicate.
- **Stale agent session id** (conversation deleted): the agent errors and the pane stays at a
  shell in the right cwd. The failure is logged.
- **Codex session id**: confirm the `notify` payload field before Phase 4. Without it, Codex panes
  restore as a fresh `codex` in the cwd.
- **Restore executes commands**: it doesn't by default; commands are pasted without `Enter`.
  Secrets in argv (e.g. `--token=…`) would be written to `snapshot.json`, so the file is created
  `0600`. Argv values matching `[restore] redact = ["--token", "--password", "*_KEY="]` are
  dropped.
- **No foreground child / zombie / process exited between reads**: `argv = None`, just the cwd.
- **Wrapper processes** (`npx`, `node …/claude`, `bash -c`): agent detection already handles
  node argv (`AgentKind::node_argv_term`). The pasted command is the original argv, which re-runs
  the same wrapper.
- **tmux spawn storms**: one `list-panes` per tick, option writes only on change and batched.
- **Hook bursts**: per-pane debounce (latest event wins within a tick).
- **Names with `.`/`:`** break tmux targets: always address by `$id`/`@id`/`%id` internally.
- **MCP prompt injection**: `read_output` returns another agent's text, which a calling agent may
  obey. Document it; never auto-forward output into `send_message`.
- **Daemon down**: MCP and TUI show a clear "daemon not running" error. The old `Alt+s` picker
  keeps working since it doesn't depend on the daemon.

## Documentation updates

- **`README.md`**: daemon via systemd, `hooks install`, categories, restore, the MCP setup,
  `status.conf` snippet, new keys.
- **`ROADMAP.md`**: add these phases under a new "v2" section; mark the projects-only model
  as superseded.
- **`CLAUDE.md` / `.claude/`**: no changes.

## Out of scope

- Web/mobile UI changes beyond keeping the v1 wire format working.
- Restoring in-program state (vim buffers, shell history, running servers' state).
- Pi agent naming and resume (no title or resume id available).
- Multi-machine or remote tmux servers.
- Removing the Projects/Tasks/worktree features (kept, just not the default).

## Commit plan

| Phase | Commits |
|---|---|
| 0 | `feat(daemon): systemd user unit` · `feat(cli): hooks install/uninstall/status` · `feat(hooks): forward TMUX_PANE` · `feat(inventory): pane-level tmux inventory` · `feat(worker): correlate hooks by pane` |
| 1 | `feat(status): AgentStatus with question/done/unread` · `feat(hooks): AskUserQuestion → Question` · `feat(tmux): publish @agent_status window options` · `docs: status.conf snippet` |
| 2 | `feat(naming): clean_title` · `feat(naming): auto-name agent windows` |
| 3 | `feat(category): @category + CLI + API` |
| 4 | `feat(snapshot): periodic snapshot` · `feat(snapshot): closed history` · `feat(restore): restore + reopen` · `feat(restore): restore on daemon start` |
| 5 | `feat(autoclose): windows, any session, blocked guard, archive before close` |
| 6 | `feat(api): v2 target endpoints` · `feat(mcp): stdio MCP server` |
| 7 | `feat(ui): sessions view` · `feat(ui): attention navigation + inline reply` · `feat(ui): reopen list` |

## Files to modify

- `src/tmux.rs:23`: `SessionStatus` → `AgentStatus`
- `src/tmux.rs:552`: `recreate_session`, reused by restore
- `src/tmux.rs:988`: `list_all_pane_pids` absorbed by the inventory
- `src/hooks.rs:17,40,63`: `Question` event, new mapping, `PreToolUse`
- `src/worker.rs:30,433-460`: `WorkerUpdate` panes, pane correlation, read tracking
- `src/server.rs:60-79,591-640`: v2 routes, `tmux_pane` in `api_hook`
- `src/autoclose.rs:73,90`: `blocked`, scope, archive before close
- `src/config.rs:514`: `[naming]`, `[restore]`, `[mcp]`, `[ui]` sections
- `src/cli.rs:54`: `hooks`, `category`, `restore`, `closed`, `reopen`, `mcp`
- `src/app.rs:18`: `ListItem::{Category, Window}`, default view
- `src/notify.rs`: new transitions
- `showrunner-plugin/hooks/{hooks.json,post-event.sh}` (+ OpenCode, Codex clients)
- New: `src/inventory.rs`, `src/naming.rs`, `src/snapshot.rs`, `src/restore.rs`, `src/mcp.rs`,
  `src/ui_sessions.rs`, `contrib/systemd/showrunner.service`, `contrib/tmux/status.conf`
- `Cargo.toml`: `rmcp`
- `README.md`, `ROADMAP.md`

## How to test

1. **Setup**: `cargo build --release`; `systemctl --user enable --now showrunner`;
   `showrunner hooks install`; open three windows running `claude`, plus one plain shell.
2. **Happy path**
   - Status: ask Claude for something that needs a tool, then check `@agent_status` turns
     `!`; answer it, check `●`; when it finishes, check `✓` (unread). Focus the window for 2 s and
     check `○`. Ask a question with AskUserQuestion and check `?`.
   - Naming: a new Claude conversation renames its window to the session title within one tick.
   - Category: `showrunner category set gates work` and check the TUI groups it under *work*.
   - Persistence: run `btop` in one pane and `claude --model sonnet` in another, then
     `tmux kill-server` (or reboot) and `systemctl --user restart showrunner`. Check that every
     session/window/layout comes back; the `btop` pane shows `btop` pasted and not running; the
     Claude pane shows `claude --model sonnet --resume <same id>` pasted. Pressing Enter reopens
     the same conversation. Categories and names are kept.
   - Reopen: close a window with `x`, then reopen it with `u`.
   - MCP: `claude mcp add sessionizer -- showrunner mcp`, then from another Claude list sessions,
     `send_message` to an idle agent, and `wait_for_status` until it reaches `done`.
3. **Edge cases**: two Claude panes in the same cwd get separate statuses; rename a window by
   hand and check auto-naming stops; auto-close never closes a pane waiting on permission;
   restore with `main` already present merges instead of duplicating.
4. **Regression**: `cargo test`; the old `Alt+s` bash picker still works; the web UI still
   shows statuses (v1 wire values); `cm__` task sessions still recreate their worktrees.
5. **Visual**: the TUI at 80 and 160 columns; status bar glyphs in light and dark themes.

## Open questions

- Should `Alt+s` move to the Rust TUI after Phase 7, or should both pickers stay?
- Auto-close defaults once enabled: is 15 min idle (current `def_idle_secs`) right for windows?
