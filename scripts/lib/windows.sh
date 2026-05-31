# List windows for a session
# Args: session_name
# Returns: one line per window, format: "index|name|active"
window_list() {
    tmux list-windows -t "$1" -F '#{window_index}|#{window_name}|#{window_active}' 2>/dev/null || true
}

# Count windows for a session
# Args: session_name
window_count() {
    local count
    count=$(tmux list-windows -t "$1" 2>/dev/null | wc -l || echo "0")
    echo "$count" || true
}

# Get formatted window preview for a session
# Args: session_name
# Returns: single line like "0:zsh* 1:vim 2:htop"
window_preview() {
    local session="$1"
    local preview=""
    while IFS='|' read -r idx name active; do
        [ -z "$idx" ] && continue
        if [ "$active" = "1" ]; then
            preview="$preview ${idx}:${name}*"
        else
            preview="$preview ${idx}:${name}"
        fi
    done <<< "$(window_list "$session")"
    echo "${preview# }"  # Remove leading space
}

# Get active window name for a session
# Args: session_name
# Returns: "index:name" of the active window
window_active_name() {
    local session="$1"
    tmux list-windows -t "$session" -F '#{window_index}|#{window_name}|#{window_active}' 2>/dev/null |         grep '|1$' | cut -d'|' -f1,2 || true
}


