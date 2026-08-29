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

# True if $1 is a numeric PID that is alive AND is the showrunner daemon.
# Guards against a recycled PID now belonging to an unrelated process (which
# must never be trusted by status or force-killed by stop).
_pid_is_daemon() {
    local pid="${1:-}"
    [[ "$pid" =~ ^[0-9]+$ ]] || return 1
    kill -0 "$pid" 2>/dev/null || return 1
    ps -p "$pid" -o comm= 2>/dev/null | grep -qx "showrunner"
}

# True if something is already listening on the daemon port — covers the case
# where a daemon is up but the PID file was lost (e.g. a lost double-start race).
_port_in_use() {
    local port="${SESSIONIZER_PORT:-7878}"
    if command -v ss >/dev/null 2>&1; then
        ss -ltn 2>/dev/null | grep -Eq "[:.]$port\b"
    elif command -v lsof >/dev/null 2>&1; then
        lsof -iTCP:"$port" -sTCP:LISTEN >/dev/null 2>&1
    else
        return 1
    fi
}

# Start the daemon if not already running. Returns 0 if running (started or
# already up), 1 on failure.
daemon_start() {
    mkdir -p "$CACHE_DIR"
    if daemon_status; then
        return 0
    fi
    if _port_in_use; then
        # A daemon is already serving; adopt its PID so status/stop work.
        local pid
        pid=$(ss -ltnp 2>/dev/null | grep -E "[:.]${SESSIONIZER_PORT:-7878}\b" | grep -oE 'pid=[0-9]+' | head -1 | cut -d= -f2)
        [ -n "$pid" ] && echo "$pid" > "$PID_FILE"
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
        if _pid_is_daemon "$pid"; then
            kill "$pid" 2>/dev/null || true
            # Wait up to ~1s for graceful exit, then force kill.
            local i
            for i in 1 2 3 4 5; do
                kill -0 "$pid" 2>/dev/null || break
                sleep 0.2
            done
            kill -9 "$pid" 2>/dev/null || true
        fi
        # Clean up the PID file whether it was valid or stale.
        rm -f "$PID_FILE"
    fi
    return 0
}

# Returns 0 if the daemon is running, 1 otherwise.
daemon_status() {
    [ -f "$PID_FILE" ] || return 1
    local pid
    pid=$(cat "$PID_FILE" 2>/dev/null || true)
    if _pid_is_daemon "$pid"; then
        return 0
    fi
    # Stale or recycled PID: drop the file rather than trust it.
    rm -f "$PID_FILE"
    return 1
}
