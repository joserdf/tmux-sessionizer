# List all tmux sessions
# Returns: one line per session, format: "name|windows|created"
session_list() {
    tmux list-sessions -F '#{session_name}|#{session_windows}|#{session_created}' 2>/dev/null || true
}

# Get number of sessions
session_count() {
    tmux list-sessions 2>/dev/null | wc -l || echo "0"
}

# Create a new session
# Args: session_name
session_create() {
    tmux new-session -d -s "$1" 2>/dev/null || true
}

# Rename a session
# Args: old_name new_name
session_rename() {
    tmux rename-session -t "$1" "$2" 2>/dev/null || true
}

# Kill a session
# Args: session_name
session_kill() {
    tmux kill-session -t "$1" 2>/dev/null || true
}

# Switch to a session
# Args: session_name
session_switch() {
    tmux switch-client -t "$1" 2>/dev/null || true
}

# Get current session name
session_current() {
    tmux display-message -p '#{session_name}' 2>/dev/null || echo ""
}
