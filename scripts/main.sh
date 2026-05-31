#!/usr/bin/env bash
set -euo pipefail

SESSIONIZER_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"

source "$SESSIONIZER_DIR/helpers/cleanup.sh"
source "$SESSIONIZER_DIR/scripts/lib/sessions.sh"
source "$SESSIONIZER_DIR/scripts/lib/windows.sh"
source "$SESSIONIZER_DIR/scripts/lib/ui.sh"

trap 'sessionizer_cleanup' EXIT INT TERM

ui_init

# Auto-close: exit after N seconds of inactivity
LAST_ACTIVITY=$(date +%s)
AUTO_CLOSE_TIMEOUT=${SESSIONIZER_TIMEOUT:-5}

while true; do
    ui_render

    # Read a single key (500ms timeout for periodic re-render)
    key=""
    IFS= read -r -N1 -t 0.5 key 2>/dev/null || true

    # Auto-close on inactivity timeout
    if [ -z "$key" ]; then
        NOW=$(date +%s)
        [ $((NOW - LAST_ACTIVITY)) -ge $AUTO_CLOSE_TIMEOUT ] && break
        continue
    fi

    LAST_ACTIVITY=$(date +%s)

    case "$key" in
        $'\e')
            # Escape sequence — read remaining bytes for arrow keys
            c1=""; c2=""
            IFS= read -r -t 0.05 -N1 c1 2>/dev/null || true
            IFS= read -r -t 0.05 -N1 c2 2>/dev/null || true
            case "$c1$c2" in
                '[A') ui_cursor_up ;;
                '[B') ui_cursor_down ;;
                *)    break ;;  # standalone Escape = quit
            esac
            ;;
        $'\n'|$'\r'|' ') ui_select ;;
        n|c)  ui_create_session ;;
        r)    ui_rename_session ;;
        x)    ui_kill_session ;;
        h)    ui_toggle_help ;;
        q|Q)  break ;;
    esac
done
