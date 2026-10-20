# tmux-sessionizer

A tmux-native TUI (Rust + Ratatui) for AI-assisted development. It organizes
your work into Projects → Tasks → Sessions, launches AI coding agents
(Claude Code, OpenCode, Codex, Pi) in tmux sessions backed by git worktrees,
shows live per-agent status, lets you review diffs, and watches per-session
CPU/mem/GPU.

Built on the [showrunner](https://github.com/Bendzae/showrunner) base (MIT).
The old pure-bash session picker is gone; bash is now a thin tmux bootstrap.

## Architecture

- **Daemon** — `showrunner serve` (Rust/tokio/axum) owns the session/agent
  state, serves HTTP + SSE on localhost (default `127.0.0.1:7878`), and writes
  the status-bar alert count. It also serves a small web UI.
- **TUI** — `showrunner` (Ratatui). It consumes the daemon's SSE
  (`/events/worker`) as its live data source, so the daemon's worker is the
  single source of truth across TUI, web and mobile. When the daemon isn't
  running (e.g. a standalone TUI), it falls back to its own in-process worker.
- **Bootstrap** — `sessionizer.tmux` (bash) wires tmux keybindings and the
  daemon lifecycle. That is its entire job.

## Requirements

- Rust toolchain (to build), `tmux` 3.2+ (for `display-popup`), `git`
- The agent CLIs you want to use: `claude`, `opencode`, `codex`, `pi`
- `python3` (the Claude and Codex hook clients use it to forward events to the
  daemon; without it the hooks no-op and status falls back to pane-scrape)
- `nvidia-smi` for GPU metrics (optional)
- `hunk` for diff review (falls back to `npx -y hunkdiff` if absent)

## Build & test

```bash
cargo build --release   # binary: target/release/showrunner
cargo test
```

## Install (TPM)

Add the repo to your `~/.tmux.conf`:

```tmux
set -g @plugin 'joserdf/tmux-sessionizer'
run '~/.tmux/plugins/tpm/tpm'
```

Press `prefix + I`, then build the binary once — the bootstrap looks for
`target/release/showrunner` in the plugin directory, then `target/debug`,
then `$PATH`:

```bash
cd ~/.tmux/plugins/tmux-sessionizer && cargo build --release
```

The bootstrap binds `Alt+s` / `Alt+n` (open the TUI in a popup), `Alt+a`
(daemon on/off), and auto-starts the daemon on load.

Manual install: clone the repo, build it, then add
`run '~/.tmux/plugins/tmux-sessionizer/sessionizer.tmux'` to your
`.tmux.conf`.

## Keybindings

| Key | Action |
|-----|--------|
| `Alt+s` | Open the TUI in a popup |
| `Alt+n` | Open the TUI (new/switch sessions) |
| `Alt+a` | Toggle the daemon on/off |

Inside the TUI (defaults):

| Key | Action |
|-----|--------|
| `↑/↓`, `j`/`k` | Navigate |
 | `Enter` | Open / switch to the selected session |
 | `/` | Search |
 | `a` | Context menu (new session, task, review, push, PR, merge, …) |
 | `h` | Resource panel (per-session CPU / mem / GPU) |
 | `p` | Add project |
 | ` ` | Toggle collapse |
 | `t` / `Z` | Cycle theme / toggle archive view |
 | `q`, `Esc` | Quit / cancel |

On a session, the context menu (`a`) also offers **Send message** (`s`, types
and submits a reply to the agent), **Approve** (`a`, sends `y` to accept a
pending permission prompt — only offered while the session is actually waiting
for one), and **Restart** (`R`, kills and relaunches the session, resuming the
conversation).

Keybindings are configurable in `~/.showrunner/keybindings.toml` — see
`keybindings.example.toml` in this repo.

## Features

### Done

- Projects → Tasks → Sessions, each session in its own git worktree
- Per-agent status: running / waiting-for-input / waiting-for-permission /
  finished (Claude Code, OpenCode, Codex, Pi)
- OpenCode harness
- Diff review via `hunk`
- Per-session CPU/mem + GPU usage (`RES` column) and a resource panel overlay
  (per-session CPU / mem / GPU with totals, `h`)
- Quick-reply and approval from the TUI (send a message, or accept a permission
  prompt)
- Restart an agent session from the TUI (kill + relaunch, resuming the
  conversation; preserves worktree/branch)
- Auto-close sessions (idle + finished, dirty-worktree safeguard; opt-in)
- OS notifications when a session needs a decision (permission prompt, etc.)
- Authoritative agent hooks via the daemon — Claude Code (plugin `hooks.json`),
  OpenCode (auto-loaded plugin), and Codex (`notify` on turn-complete); Pi uses
  the pane-scrape fallback
- Project auto-discovery (`showrunner discover` — git / zoxide / ghq)
- Daemon with HTTP + SSE on localhost, plus a small web UI with a kanban board
  at `/board` (live via `/events` SSE + a 5s poll fallback)
- Board API: `GET /api/v2/boards`, `POST /api/v2/boards/{name}/cards/move`
  (`{id, column}`), `POST /api/v2/boards/dispatch` (`{dry_run}`), and
  `POST /api/v2/boards/pause` (`{paused}`); mutations are loopback-only
- TUI consumes the daemon's SSE (`/events/worker`) as its live data source. When
  the daemon isn't running at startup, or dies mid-session, the TUI falls back to
  a local in-process prober (and shows a "daemon down" indicator in the header),
  so the dashboard keeps updating either way
- tmux status-bar alert badge (count of sessions waiting on you)
- `showrunner` CLI to manage and talk to sessions from inside one
  (`list`, `task`, `session`, `ask`, `send`, `output`, `discover`)

## Sessions view

`showrunner sessions` lists every tmux session and window as a tree,
*category ▸ session ▸ window*, with each window's agent, status and idle time.
Statuses are `● working`, `? question`, `! permission`, `✓ done` (finished, not
seen yet), `○ idle` and `✗ error`. Uncategorized sessions go last. The plugin
binds it to `Alt+S` as a popup.

- **Move:** `j`/`k`, `Enter` switches to the window, `n` jumps to the next one
  needing you, `/` filters, `a` toggles attention-only.
- **Act:**
  - `r` reply, `y` approve a permission, `s` suspend the agent
  - `c` set the category, `R` rename
  - `x` close (asks first; reopenable), `u` reopen from the recently closed list
- **Other:** `p` opens the projects TUI, `q` quits.

When the terminal is wider than 140 columns, the selected window's output is
shown on the right. The actions are the MCP tools' (below), so the same rules
apply.

Agent token usage is read from the transcript (Claude, Codex) and shown per
window as a compact count (`1.2M tok`), with a dollar cost when the model is
priced (below). `get_status` reports it as `tokens` and `cost_usd`.

## MCP server

`showrunner mcp` is an MCP server on stdio. It lets a coding agent see and
drive every tmux session on the machine, other agents included. Register it
in each agent:

```sh
claude mcp add showrunner -- showrunner mcp
codex mcp add showrunner -- showrunner mcp
```

For OpenCode, add this to `~/.config/opencode/opencode.json`:

```json
{ "mcp": { "showrunner": { "type": "local", "command": ["showrunner", "mcp"] } } }
```

Targets are `session`, `session:window` (index, name or `@id`) or `%pane`.
A window resolves to its agent pane.

- **Read:**
  - `whoami`, `list_sessions(category?, status?)`, `list_windows(session?)`
  - `get_status(target)`: includes the options of a pending question or permission.
  - `read_output(target, lines)`, `read_diff(target)`, `list_closed`
  - `wait_for_status(target, statuses?, timeout?)`
  - `search_sessions(query)`: ranks windows by the query's words in their names,
    agent title, category, branch, cwd, recent output and the agent's transcript
    (Claude, Codex).
  - `list_cards(board?, column?)`: kanban cards across all boards (or one board /
    column) with id, key, title, priority, labels and url.
  - `children(session)`: the live sessions forked or spawned from a session.
  - `collect(session, lines?)`: a worker's status, last output and the commits
    its worktree has beyond the task branch.
- **Act:**
  - `send_message`, `send_keys`, `approve(yes|always|no)`, `answer(choice)`
  - `create_session`, `create_window`, `rename`, `set_category`
  - `close` (archived first, so it's reopenable), `reopen(id)`
  - `suspend` (stop the agent, paste its resume command), `resume`
  - `move_card(board, id, column)`: move a card between kanban columns.
  - `dispatch(card?, dry_run?)`: run a dispatch pass, or dispatch one Ready card.
  - `fork(session)`: fork the session's agent conversation into a new session in
    the same worktree (Claude, Codex, OpenCode).
  - `spawn_worker(parent, prompt, agent?)`: start a worker session of the
    parent's task in its own worktree; `children` and `collect` follow it.

Agent statuses come from the daemon. Without the daemon, only which agent runs
where is known.

The caller's own pane is protected: `send_message`, `send_keys`, `close` and
`suspend` refuse it. `send_message` to a working agent is queued (delivered
when the agent next becomes idle or done) unless `force` pastes immediately,
and `close` needs `confirm: true`. Queued messages survive daemon restarts
(but not tmux server restarts). Set `[mcp] read_only = true` in
`~/.showrunner/config.toml` to allow only the reading tools.

### Streamable HTTP (on the daemon)

The daemon (`showrunner serve`) also exposes the same MCP server over streamable
HTTP at `http://127.0.0.1:7878/mcp`, so a local program (e.g. a Telegram
assistant) can poll statuses and call `approve` / `answer` over HTTP instead of
spawning stdio. It is protected by a bearer token:

- The token lives in `~/.showrunner/mcp_token` (32 random bytes, hex-encoded,
  permissions `0600`). It is created on daemon start if absent.
- `showrunner mcp token` prints that file's path (never the token itself);
  `showrunner mcp token --rotate` regenerates it.
- Every request must carry `Authorization: Bearer <token>`; a missing or wrong
  token gets a `401`. The token is compared in constant time and never logged.

Point an HTTP MCP client at the endpoint and send the header, e.g.:

```sh
curl http://127.0.0.1:7878/mcp \
  -H "Authorization: Bearer $(cat ~/.showrunner/mcp_token)" \
  -H "Content-Type: application/json" \
  -H "Accept: application/json, text/event-stream" \
  -d '{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-03-26","capabilities":{},"clientInfo":{"name":"curl","version":"1"}}}'
```

## Configuration

- State: `~/.showrunner/` (projects, tasks, session records, worktrees)
- Cache + daemon runtime: `~/.cache/tmux-sessionizer/` (status-bar cache,
  `server.pid`, `server.log`)
- Files copied into new worktrees: per-project `copy_patterns` in the config,
  plus a repo-level `.worktreeinclude` file (gitignore syntax) at the project
  root listing untracked/ignored files (e.g. `.env`) to copy as well; a
  trailing-slash directory pattern (e.g. `build/`) there copies the directory
  recursively
- Keybindings: `~/.showrunner/keybindings.toml` (start from
  `keybindings.example.toml`)

### Task boards (in progress)

Kanban boards whose cards get dispatched to agent sessions. A board maps a
provider (native, plane, github, jira) to a showrunner project; `native` uses
showrunner's own tasks that have a board column set.

```toml
[dispatch]
max_running = 2   # global cap
poll_secs = 60

[[boards]]
name = "sessionizer"
provider = "native"
project = "tmux-sessionizer"
agent = "claude"
max_running = 2   # per-board cap

# [boards.plane]      url / workspace / project_id / api_key_env
# [boards.github]     owner / project_number / status_field
# [boards.jira]       url / project_key / email_env / api_token_env
# [boards.columns]    provider state name -> canonical column
```

#### Plane boards

A self-hosted Plane (plane.so) project becomes a board; cards are its work
items, dispatched the same way as native cards. The API key is read from the
env var named by `api_key_env` (never stored in the config):

```toml
[[boards]]
name = "roadmap"
provider = "plane"
project = "tmux-sessionizer"
agent = "claude"
max_running = 2

[boards.plane]
url = "https://plane.gmmsb.lncc.br"
workspace = "joserdf"
project_id = "abc-123"
api_key_env = "PLANE_API_KEY"

# Plane state name -> canonical column (optional; unlisted states fall back
# by group: backlog/unstarted -> Backlog, started -> Running, completed -> Done,
# cancelled -> skipped).
[boards.columns]
"Backlog" = "backlog"
"Ready for agent" = "ready"
"In Progress" = "running"
"Done" = "done"
```

**Board view**: `showrunner board` (or `board view`, or `b` from the sessions
view) opens a full-screen kanban. Cards sit in six columns; `h`/`l` and `j`/`k`
navigate, `Tab` cycles boards. `Enter` switches the tmux client to the session
serving a card, `m` moves it to another column, `D` runs one dispatch pass, `p`
toggles dispatch pause and `r` reloads (it also auto-reloads every ~2s).

### Token usage & pricing

Each session's token usage is read from its transcript and shown per window in
`showrunner sessions`, the resource panel (CPU/mem/GPU/tokens) and `get_status`:
- **Claude** — summed from the JSONL transcript, deduped by message.
- **Codex** — the transcript's last cumulative `token_count`.
- **OpenCode** — the aggregated token columns in its SQLite db (via the
  `sqlite3` CLI). This also covers Carcará: `carcara-coder` is OpenCode pointed
  at the Carcará endpoint, so its sessions report tokens (cost `0`).
- **Pi** — summed from the JSONL session file under its agent dir.

Costing is opt-in: showrunner never guesses prices. Add a price per model (USD
per million tokens) to `~/.showrunner/config.toml`:

```toml
[pricing."claude-opus-5-5"]
input = 15.0
output = 75.0
cache_write = 18.75
cache_read = 1.5
```

Sessions whose model has no price show only the token count, with no cost.

## License

MIT
