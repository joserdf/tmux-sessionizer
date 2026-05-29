#!/usr/bin/env bash
set -euo pipefail

# Determine plugin directory (script lives in scripts/)
SESSIONIZER_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"

# Source libraries
source "$SESSIONIZER_DIR/helpers/cleanup.sh"
source "$SESSIONIZER_DIR/scripts/lib/sessions.sh"
source "$SESSIONIZER_DIR/scripts/lib/windows.sh"
source "$SESSIONIZER_DIR/scripts/lib/ui.sh"

# Set up temporary M-prefix keybindings that write to the FIFO
bind_sessionizer_keys() {
    local fifo="$FIFO_PATH"
    tmux bind-key -n M-Up     run-shell "printf '%s\\n' up     > '$fifo' 2>/dev/null || true" || true
    tmux bind-key -n M-Down   run-shell "printf '%s\\n' down   > '$fifo' 2>/dev/null || true" || true
    tmux bind-key -n M-Enter  run-shell "printf '%s\\n' select > '$fifo' 2>/dev/null || true" || true
    tmux bind-key -n M-c      run-shell "printf '%s\\n' new    > '$fifo' 2>/dev/null || true" || true
    tmux bind-key -n M-r      run-shell "printf '%s\\n' rename > '$fifo' 2>/dev/null || true" || true
    tmux bind-key -n M-x      run-shell "printf '%s\\n' kill   > '$fifo' 2>/dev/null || true" || true
    tmux bind-key -n M-h      run-shell "printf '%s\\n' help   > '$fifo' 2>/dev/null || true" || true
    tmux bind-key -n M-q      run-shell "printf '%s\\n' quit   > '$fifo' 2>/dev/null || true" || true
}

# Remove temporary M-prefix keybindings
unbind_sessionizer_keys() {
    tmux unbind-key -n M-Up    2>/dev/null || true
    tmux unbind-key -n M-Down  2>/dev/null || true
    tmux unbind-key -n M-Enter 2>/dev/null || true
    tmux unbind-key -n M-c     2>/dev/null || true
    tmux unbind-key -n M-r     2>/dev/null || true
    tmux unbind-key -n M-x     2>/dev/null || true
    tmux unbind-key -n M-h     2>/dev/null || true
    tmux unbind-key -n M-q     2>/dev/null || true
}

# Create FIFO for IPC (fixed path — only one sessionizer at a time)
FIFO_PATH="/tmp/tmux-sessionizer.fifo"
rm -f "$FIFO_PATH"
mkfifo "$FIFO_PATH" 2>/dev/null || true

# Open FIFO read-write so read does not block waiting for a writer
exec 3<>"$FIFO_PATH"

# Cleanup on exit
trap 'sessionizer_cleanup; unbind_sessionizer_keys; rm -f "$FIFO_PATH"; exec 3>&-' EXIT INT TERM

# Initialize TUI
ui_init

# Bind temporary M-prefix keys for the popup session
bind_sessionizer_keys

# Main loop
while true; do
    ui_render

    # Read action from FIFO (1s timeout for periodic re-render)
    action=""
    IFS= read -r -t 1.0 action <&3 2>/dev/null || true

    case "$action" in
        "up")       ui_cursor_up || true ;;
        "down")     ui_cursor_down || true ;;
        "select")   ui_select || true ;;
        "new")      ui_create_session || true ;;
        "rename")   ui_rename_session || true ;;
        "kill")     ui_kill_session || true ;;
        "help")     ui_toggle_help || true ;;
        "quit")     break ;;
        "")         ;;  # timeout — re-render
        *)          ;;  # ignore unknown
    esac
done
