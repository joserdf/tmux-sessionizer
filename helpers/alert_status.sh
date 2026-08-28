#!/usr/bin/env bash
# alert_status.sh — fixed-width alert indicator for the tmux status bar.
# Used via #() in a status-right/left format string.
#
# Reads $CACHE_DIR/status.cache (a single integer written by the showrunner
# Rust daemon) and prints a fixed-width string so the status bar doesn't shift.
# Falls back to "0" when the daemon hasn't written anything yet.

CACHE_DIR="${SESSIONIZER_CACHE_DIR:-$HOME/.cache/tmux-sessionizer}"
status_file="$CACHE_DIR/status.cache"

n=0
if [ -f "$status_file" ]; then
    n=$(cat "$status_file" 2>/dev/null || echo 0)
    case "$n" in
        *[!0-9]*) n=0 ;;
    esac
fi

printf '#[fg=red]⚠ %-3s #[default]' "$n"
