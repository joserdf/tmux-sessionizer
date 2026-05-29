#!/usr/bin/env bash

# tmux-sessionizer: TPM plugin entry point
# Sets up keybindings for the session manager TUI

SESSIONIZER_PATH="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
MAIN_SCRIPT="$SESSIONIZER_PATH/scripts/main.sh"

# Ensure main.sh is executable
chmod +x "$MAIN_SCRIPT" 2>/dev/null || true

# Bind Alt+s to open the session manager in a popup
tmux bind-key -n M-s display-popup -w 90% -h 80% -E "$MAIN_SCRIPT"

# Bind Alt+n for quick new session (prompt-based, no TUI)
tmux bind-key -n M-n command-prompt -p "New session:" "new-session -d -s '%1'; switch-client -t '%1'"
