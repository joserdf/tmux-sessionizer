#!/usr/bin/env bash

# tmux-sessionizer: TPM plugin entry point
# Opens the sessionizer TUI in a popup. Keybindings inside the popup
# are managed dynamically by scripts/main.sh via bind/unbind_sessionizer_keys.

SESSIONIZER_PATH="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
MAIN_SCRIPT="$SESSIONIZER_PATH/scripts/main.sh"

chmod +x "$MAIN_SCRIPT" 2>/dev/null || true

# Alt+s: Open sessionizer in popup (popup exits when main.sh finishes)
tmux bind-key -n M-s display-popup -w 90% -h 80% -E "$MAIN_SCRIPT"
