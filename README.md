# tmux-sessionizer

A TUI session manager plugin for [tmux](https://github.com/tmux/tmux) that replaces `choose-tree` with a cleaner, keyboard-driven interface.

## Features

- Flat session list with window preview
- Create, rename, and kill sessions
- Bottom status bar with keyboard shortcuts
- Pure bash - no external dependencies
- TPM compatible

## Installation

### With TPM

Add to your `~/.tmux.conf`:

```tmux
set -g @plugin 'user/tmux-sessionizer'
run '~/.tmux/plugins/tpm/tpm'
```

Press `prefix + I` to install.

### Manual

```
git clone https://github.com/user/tmux-sessionizer ~/.tmux/plugins/tmux-sessionizer
```

Then add to `~/.tmux.conf`:

```tmux
run '~/.tmux/plugins/tmux-sessionizer/sessionizer.tmux'
```

## Keybindings

| Key | Action |
|-----|--------|
| `Alt+s` | Open session manager |
| `Alt+n` | Quick new session (prompt) |

### Inside the TUI

| Key | Action |
|-----|--------|
| `Up/Down` | Navigate sessions |
| `Enter` | Switch to session |
| `n` | Create new session |
| `r` | Rename session |
| `x` | Kill session |
| `h` | Toggle help |
| `q` / `Esc` | Quit |

## Requirements

- tmux 3.2+ (for `display-popup`)
- bash 4+

## License

MIT
