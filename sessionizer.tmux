#!/usr/bin/env bash

# tmux-sessionizer: TPM plugin bootstrap
# Thin launcher that delegates to the Rust `showrunner` binary (TUI + daemon).
# Key handling, session/agent management, and alerting all live in Rust now;
# this file only wires tmux keybindings and manages the daemon lifecycle.

SESSIONIZER_PATH="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
CACHE_DIR="${SESSIONIZER_CACHE_DIR:-$HOME/.cache/tmux-sessionizer}"
mkdir -p "$CACHE_DIR"

# Locate the showrunner binary: repo build first, then PATH.
if [ -x "$SESSIONIZER_PATH/target/release/showrunner" ]; then
    SESSIONIZER_BIN="$SESSIONIZER_PATH/target/release/showrunner"
elif [ -x "$SESSIONIZER_PATH/target/debug/showrunner" ]; then
    SESSIONIZER_BIN="$SESSIONIZER_PATH/target/debug/showrunner"
elif command -v showrunner >/dev/null 2>&1; then
    SESSIONIZER_BIN="$(command -v showrunner)"
else
    SESSIONIZER_BIN="showrunner"  # rely on PATH; errors at runtime if missing
fi
export SESSIONIZER_BIN
export SESSIONIZER_CACHE_DIR="$CACHE_DIR"

# Mark loaded so bindings + auto-start only run once per server (idempotent).
tmux set-option -g @tmux_sessionizer_loaded true 2>/dev/null || true

# Alt+s: open the TUI in a popup (popup exits when the TUI exits).
tmux bind-key -n M-s display-popup -w 90% -h 90% -E "$SESSIONIZER_BIN" 2>/dev/null || true

# Alt+n: quick access to the TUI (new/switch sessions) in a popup.
tmux bind-key -n M-n display-popup -w 90% -h 90% -E "$SESSIONIZER_BIN" 2>/dev/null || true

# Alt+a: toggle the showrunner daemon on/off.
tmux bind-key -n M-a run-shell -b "source '$SESSIONIZER_PATH/scripts/daemon.sh'; if daemon_status; then daemon_stop; tmux display-message 'showrunner daemon: stopped'; else daemon_start && tmux display-message 'showrunner daemon: started' || tmux display-message 'showrunner daemon: failed to start'; fi" 2>/dev/null || true

# Auto-start the daemon on plugin load (once).
if ! tmux show-option -gv @tmux_sessionizer_daemon_autostart 2>/dev/null | grep -q true; then
    tmux set-option -g @tmux_sessionizer_daemon_autostart true 2>/dev/null || true
    tmux run-shell -b "source '$SESSIONIZER_PATH/scripts/daemon.sh'; daemon_start" 2>/dev/null || true
fi
