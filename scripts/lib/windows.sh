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

# Capture last N lines from the active pane of a session
# Args: session_name, max_lines
# Returns: plain text lines stripped of trailing blanks
window_capture_preview() {
    local session="$1"
    local max_lines="$2"
    tmux capture-pane -t "$session" -p -S -"${max_lines}" -J 2>/dev/null || true
}

# Capture last N lines from a specific window in a session
# Args: session_name, window_index, max_lines
window_capture_specific() {
    local session="$1"
    local window="$2"
    local max_lines="$3"
    tmux capture-pane -t "${session}:${window}" -p -S -"${max_lines}" -J 2>/dev/null || true
}

# ---- Preview Pipe Management ----
PREVIEW_FILE=""
PIPED_TARGET=""

window_preview_init() {
    PREVIEW_FILE="/tmp/tmux-sessionizer-$$.preview"
    : > "$PREVIEW_FILE"
}

window_preview_session() {
    local session="$1"
    window_preview_stop_pipe || true
    : > "$PREVIEW_FILE"

    # Get active pane for this session
    local target
    target=$(tmux list-panes -t "$session" -F '#{pane_id}' -f '#{==:#{pane_active},1}' 2>/dev/null | head -1) || true
    [ -z "$target" ] && target="${session}:0"

    # Capture current screen content (plain text, no -e to avoid raw escapes)
    tmux capture-pane -t "$target" -p -S -15 >> "$PREVIEW_FILE" 2>/dev/null || true

    # Start pipe for ongoing output, strip ANSI codes via helper
    local strip_helper
    strip_helper="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)/helpers/strip_ansi.sh"
    tmux pipe-pane -t "$target" "$strip_helper >> \"$PREVIEW_FILE\"" 2>/dev/null || true
    PIPED_TARGET="$target"
}

window_preview_stop_pipe() {
    [ -n "$PIPED_TARGET" ] && {
        tmux pipe-pane -t "$PIPED_TARGET" 2>/dev/null || true
        PIPED_TARGET=""
    }
}

window_preview_get_content() {
    local max_lines="${1:-5}"
    if [ -f "$PREVIEW_FILE" ] && [ -s "$PREVIEW_FILE" ]; then
        tail -n "$max_lines" "$PREVIEW_FILE" 2>/dev/null || true
    fi
}

window_preview_cleanup() {
    window_preview_stop_pipe || true
    rm -f "$PREVIEW_FILE" 2>/dev/null || true
    PREVIEW_FILE=""
}
