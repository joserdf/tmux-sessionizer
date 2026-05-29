#!/usr/bin/env bash
set -euo pipefail

# Determine plugin directory (script lives in scripts/)
SESSIONIZER_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"

# Source libraries
source "$SESSIONIZER_DIR/helpers/cleanup.sh"
source "$SESSIONIZER_DIR/scripts/lib/sessions.sh"
source "$SESSIONIZER_DIR/scripts/lib/windows.sh"
source "$SESSIONIZER_DIR/scripts/lib/ui.sh"

# Create FIFO for IPC (fixed path — only one sessionizer at a time)
FIFO_PATH="/tmp/tmux-sessionizer.fifo"
rm -f "$FIFO_PATH"
mkfifo "$FIFO_PATH" 2>/dev/null || true

# Open FIFO read-write so read does not block waiting for a writer
exec 3<>"$FIFO_PATH"

# Cleanup on exit
trap 'sessionizer_cleanup; rm -f "$FIFO_PATH"; exec 3>&-' EXIT INT TERM

# Initialize TUI
ui_init

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
