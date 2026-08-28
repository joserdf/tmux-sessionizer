#!/usr/bin/env bash
# daemon.sh — start/stop/status for the showrunner HTTP/SSE daemon.
# Sourced by sessionizer.tmux; also usable standalone.
#
# The daemon is `showrunner serve` (axum HTTP + SSE server). It owns the
# session/agent state and streams events to TUI/web clients. This helper
# manages its lifecycle (background process + PID file).

CACHE_DIR="${SESSIONIZER_CACHE_DIR:-$HOME/.cache/tmux-sessionizer}"
PID_FILE="$CACHE_DIR/server.pid"
LOG_FILE="$CACHE_DIR/server.log"
BIN="${SESSIONIZER_BIN:-showrunner}"

# Start the daemon if not already running. Returns 0 if running (started or
# already up), 1 on failure.
daemon_start() {
    mkdir -p "$CACHE_DIR"
    if daemon_status; then
        return 0
    fi

    if [ ! -x "$BIN" ] && ! command -v "$BIN" >/dev/null 2>&1; then
        echo "showrunner binary not found: $BIN" >&2
        return 1
    fi

    nohup "$BIN" serve >"$LOG_FILE" 2>&1 &
    echo $! > "$PID_FILE"
    return 0
}

# Stop the daemon if running. Always returns 0.
daemon_stop() {
    if [ -f "$PID_FILE" ]; then
        local pid
        pid=$(cat "$PID_FILE" 2>/dev/null || true)
        if [ -n "$pid" ]; then
            kill "$pid" 2>/dev/null || true
            # Wait up to ~1s for graceful exit, then force kill.
            local i
            for i in 1 2 3 4 5; do
                kill -0 "$pid" 2>/dev/null || break
                sleep 0.2
            done
            kill -9 "$pid" 2>/dev/null || true
        fi
        rm -f "$PID_FILE"
    fi
    return 0
}

# Returns 0 if the daemon is running, 1 otherwise.
daemon_status() {
    [ -f "$PID_FILE" ] || return 1
    local pid
    pid=$(cat "$PID_FILE" 2>/dev/null || true)
    [ -z "$pid" ] && { rm -f "$PID_FILE"; return 1; }
    kill -0 "$pid" 2>/dev/null || { rm -f "$PID_FILE"; return 1; }
    return 0
}
