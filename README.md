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
- **TUI** — `showrunner` (Ratatui). It currently runs its own in-process
  background worker; consuming the daemon's SSE directly is on the roadmap.
- **Bootstrap** — `sessionizer.tmux` (bash) wires tmux keybindings and the
  daemon lifecycle. That is its entire job.

## Requirements

- Rust toolchain (to build), `tmux` 3.2+ (for `display-popup`), `git`
- The agent CLIs you want to use: `claude`, `opencode`, `codex`, `pi`
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
| `p` | Add project |
| ` ` | Toggle collapse |
| `t` / `Z` | Cycle theme / toggle archive view |
| `q`, `Esc` | Quit / cancel |

Keybindings are configurable in `~/.showrunner/keybindings.toml` — see
`keybindings.example.toml` in this repo.

## Features

### Done

- Projects → Tasks → Sessions, each session in its own git worktree
- Per-agent status: running / waiting-for-input / waiting-for-permission /
  finished (Claude Code, OpenCode, Codex, Pi)
- OpenCode harness
- Diff review via `hunk`
- Per-session CPU/mem + GPU usage (`RES` column)
- Daemon with HTTP + SSE on localhost, plus a small web UI
- tmux status-bar alert badge (count of sessions waiting on you)
- `showrunner` CLI to manage and talk to sessions from inside one
  (`list`, `task`, `session`, `ask`, `send`, `output`)

### Roadmap

- TUI consuming the daemon's SSE directly (instead of its own worker)
- Auto-close sessions (idle + finished, dirty-worktree safeguard)
- OS notifications + bell
- Authoritative agent hooks (Claude / OpenCode / Codex)
- Quick-reply and approval flows from the TUI
- Resource panel in a tmux popup
- Project auto-discovery (git / zoxide / ghq)

## Configuration

- State: `~/.showrunner/` (projects, tasks, session records, worktrees)
- Cache + daemon runtime: `~/.cache/tmux-sessionizer/` (status-bar cache,
  `server.pid`, `server.log`)
- Keybindings: `~/.showrunner/keybindings.toml` (start from
  `keybindings.example.toml`)

## License

MIT
