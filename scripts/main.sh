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
WINDOW_AUTO_CLOSE_TIMEOUT=${SESSIONIZER_WINDOW_TIMEOUT:-10}

while true; do
    ui_render

    # Read a single key (500ms timeout for periodic re-render)
    key=""
    IFS= read -r -N1 -t 0.5 key 2>/dev/null || true

    # Auto-close on inactivity timeout
    if [ -z "$key" ]; then
        NOW=$(date +%s)
        local timeout=$AUTO_CLOSE_TIMEOUT
        [ "$MODE" = "windows" ] && timeout=$WINDOW_AUTO_CLOSE_TIMEOUT
        [ $((NOW - LAST_ACTIVITY)) -ge $timeout ] && break
        continue
    fi

    LAST_ACTIVITY=$(date +%s)

    # Route by mode
    case "$MODE" in
        sessions)
            case "$key" in
                $'\e')
                    c1=""; c2=""
                    IFS= read -r -t 0.05 -N1 c1 2>/dev/null || true
                    IFS= read -r -t 0.05 -N1 c2 2>/dev/null || true
                    case "$c1$c2" in
                        '[A') ui_cursor_up ;;
                        '[B') ui_cursor_down ;;
                        '[C') # Right arrow -> window mode
                            if [ ${#SESSIONS[@]} -gt 0 ]; then
                                MODE="windows"
                                WIN_SELECTED=0
                                ui_refresh_windows
                            fi
                            ;;
                        *)    break ;;
                    esac
                    ;;
                $'\n'|$'\r'|' ') ui_select ;;
                n|c)  ui_create_session ;;
                r)    ui_rename_session ;;
                x)    ui_kill_session ;;
                h)    ui_toggle_help ;;
                q|Q)  break ;;
            esac
            ;;
        windows)
            case "$key" in
                $'\e')
                    c1=""; c2=""
                    IFS= read -r -t 0.05 -N1 c1 2>/dev/null || true
                    IFS= read -r -t 0.05 -N1 c2 2>/dev/null || true
                    case "$c1$c2" in
                        '[A') ui_window_cursor_up ;;
                        '[B') ui_window_cursor_down ;;
                        '[D') # Left arrow -> back to session mode
                            MODE="sessions"
                            WIN_SELECTED=0
                            HELP_VISIBLE=false
                            ;;
                        *)    break ;;  # standalone Escape = quit all
                    esac
                    ;;
                $'\n'|$'\r'|' ') ui_window_select ;;
                h)    ui_toggle_help ;;
                q|Q)  break ;;
            esac
            ;;
    esac
    LAST_ACTIVITY=$(date +%s)
done
