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
set -g @plugin 'joserdf/tmux-sessionizer'
run '~/.tmux/plugins/tpm/tpm'
```

Press `prefix + I` to install.

### Manual

```
git clone https://github.com/joserdf/tmux-sessionizer ~/.tmux/plugins/tmux-sessionizer
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

## Customization

### Keybindings

Customize TUI keybindings by adding to `~/.tmux.conf`:

```tmux
set -g @sessionizer_key_new "n"
set -g @sessionizer_key_rename "r"
set -g @sessionizer_key_kill "x"
set -g @sessionizer_key_help "h"
set -g @sessionizer_key_quit "q"
```

### Colors

Customize TUI colors by name (black, red, green, yellow, blue, magenta, cyan, white, bold, reverse):

```tmux
set -g @sessionizer_color_title "cyan"
set -g @sessionizer_color_select "yellow"
set -g @sessionizer_color_preview "green"
```

The default is `#006B3F` (deep green) for the background, with white text and cyan accents.

## Requirements

- tmux 3.2+ (for `display-popup`)
- bash 4+

## License

MIT
