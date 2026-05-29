#!/usr/bin/env bash

# tmux-sessionizer: TPM plugin entry point
# Opens the sessionizer TUI in a popup. M-prefix keybindings write to a FIFO
# that is read by scripts/main.sh while the popup is active.

SESSIONIZER_PATH="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
MAIN_SCRIPT="$SESSIONIZER_PATH/scripts/main.sh"

# Ensure main.sh is executable
chmod +x "$MAIN_SCRIPT" 2>/dev/null || true

# Alt+s: Open sessionizer in popup (popup exits when main.sh finishes)
tmux bind-key -n M-s display-popup -w 90% -h 80% -E "$MAIN_SCRIPT"

# Navigation keys — write to FIFO (only active while sessionizer popup is open)
tmux bind-key -n M-Up     run-shell 'printf "%s\n" "up" > /tmp/tmux-sessionizer.fifo 2>/dev/null || true'
tmux bind-key -n M-Down   run-shell 'printf "%s\n" "down" > /tmp/tmux-sessionizer.fifo 2>/dev/null || true'
tmux bind-key -n M-Enter  run-shell 'printf "%s\n" "select" > /tmp/tmux-sessionizer.fifo 2>/dev/null || true'

# Action keys
tmux bind-key -n M-c      run-shell 'printf "%s\n" "new" > /tmp/tmux-sessionizer.fifo 2>/dev/null || true'
tmux bind-key -n M-r      run-shell 'printf "%s\n" "rename" > /tmp/tmux-sessionizer.fifo 2>/dev/null || true'
tmux bind-key -n M-x      run-shell 'printf "%s\n" "kill" > /tmp/tmux-sessionizer.fifo 2>/dev/null || true'
tmux bind-key -n M-h      run-shell 'printf "%s\n" "help" > /tmp/tmux-sessionizer.fifo 2>/dev/null || true'
tmux bind-key -n M-q      run-shell 'printf "%s\n" "quit" > /tmp/tmux-sessionizer.fifo 2>/dev/null || true'
