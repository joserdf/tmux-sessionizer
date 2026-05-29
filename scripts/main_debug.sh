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

# Load customizable keybindings from tmux options
KEY_NEW=$(tmux show-option -gv @sessionizer_key_new 2>/dev/null || echo "n")
KEY_RENAME=$(tmux show-option -gv @sessionizer_key_rename 2>/dev/null || echo "r")
KEY_KILL=$(tmux show-option -gv @sessionizer_key_kill 2>/dev/null || echo "x")
KEY_HELP=$(tmux show-option -gv @sessionizer_key_help 2>/dev/null || echo "h")
KEY_QUIT=$(tmux show-option -gv @sessionizer_key_quit 2>/dev/null || echo "q")

# Main loop
while true; do
    ui_render

    # Read key directly (no command substitution to avoid subshell issues)
    key=""
    IFS= read -r -n1 -t 0.5 key 2>/dev/null || true

    # Handle escape sequences for arrow keys
    if [ "$key" = $'\033' ]; then
        seq=""
        IFS= read -r -n2 -t 0.1 seq 2>/dev/null || true
        case "$seq" in
            '[A') key="up" ;;
            '[B') key="down" ;;
            *)    key="escape" ;;
        esac
    fi

    case "$key" in
        "up")       ui_cursor_up || true ;;
        "down")     ui_cursor_down || true ;;
        $'\n'|$'\r') ui_select || true ;;
        "")         ;;  # timeout or no input — just re-render
        "$KEY_NEW")     ui_create_session || true ;;
        "$KEY_RENAME")  ui_rename_session || true ;;
        "$KEY_KILL")    ui_kill_session || true ;;
        "$KEY_HELP")    ui_toggle_help || true ;;
        "$KEY_QUIT")    break ;;
        "escape")   break ;;
        *)          ;;  # ignore unknown keys (don't exit)
    esac
done
