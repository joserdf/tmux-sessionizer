#!/usr/bin/env bash
set -euo pipefail

# Determine plugin directory (script lives in scripts/)
SESSIONIZER_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"

# Source libraries
source "$SESSIONIZER_DIR/helpers/cleanup.sh"
source "$SESSIONIZER_DIR/scripts/lib/sessions.sh"
source "$SESSIONIZER_DIR/scripts/lib/windows.sh"
source "$SESSIONIZER_DIR/scripts/lib/ui.sh"

# Cleanup on exit
trap 'sessionizer_cleanup' EXIT INT TERM

# Initialize TUI
ui_init

# Main loop
while true; do
    ui_render
    key=$(ui_read_key)

    case "$key" in
        "up")       ui_cursor_up ;;
        "down")     ui_cursor_down ;;
        "enter")    ui_select ;;
        "")         ;;  # timeout or no input
        "n")        ui_create_session ;;
        "r")        ui_rename_session ;;
        "x")        ui_kill_session ;;
        "h")        ui_toggle_help ;;
        "q")        break ;;
        # Ctrl-c, escape, or any other key exits
        $'\003')    break ;;  # Ctrl-c
        "escape")   break ;;
        *)          break ;;
    esac
done
