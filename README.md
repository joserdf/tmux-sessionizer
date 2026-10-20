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
- Daemon with HTTP + SSE on localhost, plus a small web UI
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
- **Act:**
  - `send_message`, `send_keys`, `approve(yes|always|no)`, `answer(choice)`
  - `create_session`, `create_window`, `rename`, `set_category`
  - `close` (archived first, so it's reopenable), `reopen(id)`
  - `suspend` (stop the agent, paste its resume command), `resume`

Agent statuses come from the daemon. Without the daemon, only which agent runs
where is known.

The caller's own pane is protected: `send_message`, `send_keys`, `close` and
`suspend` refuse it. `send_message` also refuses a working agent unless
`force`, and `close` needs `confirm: true`. Set `[mcp] read_only = true` in
`~/.showrunner/config.toml` to allow only the reading tools.

## Configuration

- State: `~/.showrunner/` (projects, tasks, session records, worktrees)
- Cache + daemon runtime: `~/.cache/tmux-sessionizer/` (status-bar cache,
  `server.pid`, `server.log`)
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

### Token usage & pricing

Each session's token usage is parsed from its transcript (Claude and Codex;
OpenCode and Pi aren't supported yet) and shown in `showrunner sessions` and
`get_status`. Costing is opt-in: showrunner never guesses prices. Add a price
per model (USD per million tokens) to `~/.showrunner/config.toml`:

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
