# ---- Terminal Capabilities ----
# Cache tput values for performance
if tput setaf 0 &>/dev/null 2>&1; then
    HAS_COLORS=true
    # Basic colors
    C_RESET=$(tput sgr0 2>/dev/null || echo '\033[0m')
    C_BOLD=$(tput bold 2>/dev/null || echo '\033[1m')
    C_REVERSE=$(tput rev 2>/dev/null || echo '\033[7m')
    C_BLACK=$(tput setaf 0 2>/dev/null || true)
    C_WHITE=$(tput setaf 7 2>/dev/null || true)
    C_CYAN=$(tput setaf 6 2>/dev/null || true)
    C_YELLOW=$(tput setaf 3 2>/dev/null || true)
    C_GREEN=$(tput setaf 2 2>/dev/null || true)
    C_RED=$(tput setaf 1 2>/dev/null || true)
    C_BLUE=$(tput setaf 4 2>/dev/null || true)
    C_MAGENTA=$(tput setaf 5 2>/dev/null || true)
else
    HAS_COLORS=false
    C_RESET=""; C_BOLD=""; C_REVERSE=""
    C_BLACK=""; C_WHITE=""; C_CYAN=""; C_YELLOW=""
    C_GREEN=""; C_RED=""; C_BLUE=""; C_MAGENTA=""
fi

# Terminal clear line
C_CLR=$(tput el 2>/dev/null || echo '\033[K')
# Cursor save/restore
C_SAVE=$(tput sc 2>/dev/null || echo '\0337')
C_RESTORE=$(tput rc 2>/dev/null || echo '\0338')

# Customizable colors (defaults match standard colors above)
C_TITLE=$C_CYAN
C_SELECT=$C_YELLOW
C_PREVIEW=$C_GREEN

# ---- State ----
SELECTED=0         # Currently selected session index
SESSIONS=()        # Array of session names
SESSION_DATA=()    # Array of "name|windows|created"
HELP_VISIBLE=false # Whether help is shown
TERM_ROWS=24       # Terminal height
TERM_COLS=80       # Terminal width

MODE="sessions"    # "sessions" or "windows"
WIN_SELECTED=0     # Selected window index in window mode
WINDOWS=()         # Array of "index|name|active" for current session's windows

# Inline editing state
EDIT_MODE="none"    # "none", "rename", "create", "kill_confirm", "rename_window", "kill_window_confirm"
EDIT_TEXT=""

# ---- Color Customization ----
# Load colors from tmux @sessionizer_color_* options
_load_color() {
    local var_name="$1" option_name="$2"
    local opt_val

    opt_val=$(tmux show-option -gv "$option_name" 2>/dev/null || true)
    [ -z "$opt_val" ] && return

    case "$opt_val" in
        black|red|green|yellow|blue|magenta|cyan|white)
            local color_num
            case "$opt_val" in
                black) color_num=0 ;; red) color_num=1 ;; green) color_num=2 ;;
                yellow) color_num=3 ;; blue) color_num=4 ;; magenta) color_num=5 ;;
                cyan) color_num=6 ;; white) color_num=7 ;;
            esac
            eval "$var_name=\$(tput setaf \$color_num 2>/dev/null || echo \"\$C_RESET\")"
            ;;
        bold)
            eval "$var_name=\$C_BOLD"
            ;;
        reverse)
            eval "$var_name=\$C_REVERSE"
            ;;
    esac
}

_load_colors() {
    _load_color C_TITLE    @sessionizer_color_title
    _load_color C_SELECT   @sessionizer_color_select
    _load_color C_PREVIEW  @sessionizer_color_preview
}

# ---- Init ----
ui_init() {
    # Save terminal state
    stty -echo -icanon 2>/dev/null || true
    # Enter alternate screen
    tput smcup 2>/dev/null || true
    # Clear alternate screen
    tput clear 2>/dev/null || true
    # Hide cursor
    tput civis 2>/dev/null || true

    # Get terminal size
    ui_update_size

    # Load customizable colors
    _load_colors

    # Load initial data
    ui_refresh_sessions
}

ui_update_size() {
    TERM_ROWS=$(tput lines 2>/dev/null || echo "24")
    TERM_COLS=$(tput cols 2>/dev/null || echo "80")
}

ui_refresh_sessions() {
    SESSION_DATA=()
    SESSIONS=()
    while IFS='|' read -r name windows created; do
        [ -z "$name" ] && continue
        SESSIONS+=("$name")
        SESSION_DATA+=("$name|$windows|$created")
    done < <(session_list)

    # Virtual "New session" item at end of list
    SESSIONS+=("+ New session")
    SESSION_DATA+=("+|0|")

    # Clamp selection
    [ ${#SESSIONS[@]} -eq 0 ] && SELECTED=0
    [ $SELECTED -ge ${#SESSIONS[@]} ] && SELECTED=$(( ${#SESSIONS[@]} - 1 ))
    [ $SELECTED -lt 0 ] && SELECTED=0 || true
}

# ---- Drawing ----
ui_draw_header() {
    # Line 1: Title
    tput cup 0 0 2>/dev/null || true
    echo -n "${C_CLR}${C_BOLD}${C_TITLE} tmux-sessionizer${C_RESET}"
    echo -n "  ${C_BLACK}${C_WHITE}[arrows: navigate]${C_RESET}"

    # Line 2: Separator
    tput cup 1 0 2>/dev/null || true
    printf '%*s' "$TERM_COLS" '' | tr ' ' '─' 2>/dev/null || true
}

ui_draw_session_list() {
    local start_row=2
    local max_visible=$(( TERM_ROWS - 3 ))  # Leave space for header + separator + status
    local scroll=0

    # Calculate scroll offset
    if [ $SELECTED -ge $(( max_visible + scroll )) ]; then
        scroll=$(( SELECTED - max_visible + 1 ))
    fi
    [ $scroll -lt 0 ] && scroll=0

    local end_idx=$(( scroll + max_visible ))
    [ $end_idx -gt ${#SESSIONS[@]} ] && end_idx=${#SESSIONS[@]}

    local row=$start_row
    for (( i = scroll; i < end_idx; i++ )); do
        tput cup $row 0 2>/dev/null || true
        local name="${SESSIONS[$i]}"
        local win_count=0
        local data_line="${SESSION_DATA[$i]}"
        if [ -n "$data_line" ]; then
            win_count=$(echo "$data_line" | cut -d'|' -f2)
        fi

        if [ $i -eq $SELECTED ]; then
            if [ "$EDIT_MODE" = "rename" ] || { [ "$EDIT_MODE" = "create" ] && [ "$name" = "+ New session" ]; }; then
                echo -n "${C_CLR}${C_REVERSE} ${C_BOLD}> ${EDIT_TEXT}_${C_RESET}"
            else
                echo -n "${C_CLR}${C_REVERSE} ${C_BOLD}> ${name}${C_RESET}  ${C_SELECT}[${win_count} windows]${C_RESET}"
            fi
        else
            echo -n "${C_CLR}   ${name}  ${C_BLUE}[${win_count} windows]${C_RESET}"
        fi
        row=$(( row + 1 ))
    done

    # Clear remaining lines
    tput cup $row 0 2>/dev/null || true
    echo -n "${C_CLR}"
}


ui_draw_status() {
    local status_row=$(( TERM_ROWS - 1 ))
    tput cup $status_row 0 2>/dev/null || true

    # Edit mode status
    case "$EDIT_MODE" in
        rename)
            echo -n "${C_CLR}${C_RESET}${C_YELLOW}RENAME: ${EDIT_TEXT}_${C_RESET}  ${C_BOLD}Enter${C_RESET}:ok  ${C_BOLD}Esc${C_RESET}:cancel"
            return
            ;;
        create)
            echo -n "${C_CLR}${C_RESET}${C_GREEN}NEW: ${EDIT_TEXT}_${C_RESET}  ${C_BOLD}Enter${C_RESET}:ok  ${C_BOLD}Esc${C_RESET}:cancel"
            return
            ;;
        kill_confirm)
            echo -n "${C_CLR}${C_RESET}${C_RED}Kill '${EDIT_TEXT}'? (y/N)${C_RESET}"
            return
            ;;
        rename_window)
            echo -n "${C_CLR}${C_RESET}${C_YELLOW}RENAME WINDOW: ${EDIT_TEXT}_${C_RESET}  ${C_BOLD}Enter${C_RESET}:ok  ${C_BOLD}Esc${C_RESET}:cancel"
            return
            ;;
        kill_window_confirm)
            echo -n "${C_CLR}${C_RESET}${C_RED}Kill window '${EDIT_TEXT}'? (y/N)${C_RESET}"
            return
            ;;
    esac

    if [ "$MODE" = "windows" ]; then
        if [ "$HELP_VISIBLE" = true ]; then
            echo -n "${C_CLR}${C_RESET}${C_BOLD}HELP${C_RESET}  ${C_CYAN}↑↓${C_RESET}:navigate  ${C_BOLD}Enter${C_RESET}:open  ${C_GREEN}r${C_RESET}:rename  ${C_RED}x${C_RESET}:kill  ${C_YELLOW}←${C_RESET}:back  ${C_YELLOW}h${C_RESET}:hide  ${C_BOLD}q${C_RESET}:quit"
        else
            echo -n "${C_CLR}${C_RESET}${C_BOLD}↑↓${C_RESET}:navigate  ${C_BOLD}Enter${C_RESET}:open  ${C_BOLD}r${C_RESET}:rename  ${C_BOLD}x${C_RESET}:kill  ${C_YELLOW}←${C_RESET}:back  ${C_YELLOW}h${C_RESET}:help  ${C_BOLD}q${C_RESET}:quit"
        fi
    else
        if [ "$HELP_VISIBLE" = true ]; then
            echo -n "${C_CLR}${C_RESET}${C_BOLD}HELP${C_RESET}  ${C_CYAN}n${C_RESET}:new  ${C_GREEN}r${C_RESET}:rename  ${C_RED}x${C_RESET}:kill  ${C_BOLD}Enter${C_RESET}:switch  ${C_YELLOW}h${C_RESET}:hide help  ${C_BOLD}q${C_RESET}:quit"
        else
            echo -n "${C_CLR}${C_RESET}${C_BOLD}n${C_RESET}:new  ${C_BOLD}r${C_RESET}:rename  ${C_BOLD}x${C_RESET}:kill  ${C_BOLD}Enter${C_RESET}:switch  ${C_YELLOW}h${C_RESET}:help  ${C_BOLD}q${C_RESET}:quit"
        fi
    fi
}

ui_render() {
    ui_update_size
    if [ "$MODE" = "sessions" ]; then
        ui_draw_header
        ui_draw_session_list
    else
        ui_draw_window_header
        ui_draw_window_list
    fi
    ui_draw_status
}

# ---- Cleanup ----
# Terminal restoration is handled by main.sh's EXIT trap (via sessionizer_cleanup).
# Individual actions (create/rename/kill) call ui_cleanup before showing prompts,
# then ui_init to re-enter TUI mode.
ui_cleanup() {
    stty echo icanon 2>/dev/null || true
    tput cnorm 2>/dev/null || true
    tput rmcup 2>/dev/null || true
    tput clear 2>/dev/null || true
    tput sgr0 2>/dev/null || true
}

# ---- Actions ----
ui_cursor_up() {
    [ $SELECTED -gt 0 ] && SELECTED=$(( SELECTED - 1 )) || true
}

ui_cursor_down() {
    [ $SELECTED -lt $(( ${#SESSIONS[@]} - 1 )) ] && SELECTED=$(( SELECTED + 1 )) || true
}

# ---- Window Mode ----
ui_refresh_windows() {
    WINDOWS=()
    local session="${SESSIONS[$SELECTED]:-}"
    [ -z "$session" ] && return
    while IFS='|' read -r idx name active; do
        [ -z "$idx" ] && continue
        WINDOWS+=("${idx}|${name}|${active}")
    done <<< "$(window_list "$session")"
}

ui_draw_window_header() {
    tput cup 0 0 2>/dev/null || true
    local session="${SESSIONS[$SELECTED]:-}"
    local win_count=${#WINDOWS[@]}
    echo -n "${C_CLR}${C_BOLD}${C_TITLE} ${session}${C_RESET}  ${C_SELECT}[${win_count} windows]${C_RESET}"

    tput cup 1 0 2>/dev/null || true
    printf '%*s' "$TERM_COLS" '' | tr ' ' '─' 2>/dev/null || true
}

ui_draw_window_list() {
    local start_row=2
    local max_visible=$(( TERM_ROWS - 3 ))
    local scroll=0

    [ $WIN_SELECTED -ge $(( max_visible + scroll )) ] && scroll=$(( WIN_SELECTED - max_visible + 1 ))
    [ $scroll -lt 0 ] && scroll=0

    local end_idx=$(( scroll + max_visible ))
    [ $end_idx -gt ${#WINDOWS[@]} ] && end_idx=${#WINDOWS[@]}

    local row=$start_row
    for (( i = scroll; i < end_idx; i++ )); do
        tput cup $row 0 2>/dev/null || true
        local entry="${WINDOWS[$i]}"
        local widx="${entry%%|*}"
        local rest="${entry#*|}"
        local wname="${rest%|*}"
        local active="${rest##*|}"

        if [ $i -eq $WIN_SELECTED ]; then
            echo -n "${C_CLR}${C_REVERSE} ${C_BOLD}> ${widx}: ${wname}${C_RESET}"
            [ "$active" = "1" ] && echo -n " ${C_GREEN}(active)${C_RESET}"
        else
            echo -n "${C_CLR}   ${widx}: ${wname}"
            [ "$active" = "1" ] && echo -n " ${C_GREEN}(active)${C_RESET}"
        fi
        row=$(( row + 1 ))
    done

    # Clear remaining list lines
    while [ $row -lt $(( TERM_ROWS - 1 )) ]; do
        tput cup $row 0 2>/dev/null || true
        echo -n "${C_CLR}"
        row=$(( row + 1 ))
    done
}

ui_window_cursor_up() {
    [ $WIN_SELECTED -gt 0 ] && WIN_SELECTED=$(( WIN_SELECTED - 1 )) || true
}

ui_window_cursor_down() {
    [ $WIN_SELECTED -lt $(( ${#WINDOWS[@]} - 1 )) ] && WIN_SELECTED=$(( WIN_SELECTED + 1 )) || true
}

ui_window_select() {
    if [ ${#WINDOWS[@]} -gt 0 ] && [ $WIN_SELECTED -lt ${#WINDOWS[@]} ]; then
        local session="${SESSIONS[$SELECTED]}"
        local entry="${WINDOWS[$WIN_SELECTED]}"
        local widx="${entry%%|*}"

        # Exit TUI mode to free terminal for nested tmux
        ui_cleanup

        # Make the selected window active in its session
        tmux select-window -t "${session}:${widx}" 2>/dev/null || true

        # Open the session inside the popup via nested tmux (no TMUX= to allow nesting)
        # User interacts with the session. Detach with prefix+d to return to TUI.
        env -u TMUX tmux attach-session -t "${session}" 2>/dev/null || true

        # Re-enter TUI mode
        MODE="sessions"
        WIN_SELECTED=0
        HELP_VISIBLE=false
        LAST_ACTIVITY=$(date +%s)
        ui_init
    fi
}

ui_select() {
    if [ ${#SESSIONS[@]} -gt 0 ] && [ $SELECTED -lt ${#SESSIONS[@]} ]; then
        local name="${SESSIONS[$SELECTED]}"
        if [ "$name" = "+ New session" ]; then
            ui_create_session
            return
        fi
        session_switch "$name"
        exit 0
    fi
}


ui_create_session() {
    EDIT_MODE="create"
    EDIT_TEXT="session-$(date +%s)"
}

ui_rename_session() {
    if [ ${#SESSIONS[@]} -gt 0 ] && [ $SELECTED -lt ${#SESSIONS[@]} ]; then
        local name="${SESSIONS[$SELECTED]}"
        if [ "$name" != "+ New session" ]; then
            EDIT_MODE="rename"
            EDIT_TEXT="$name"
        fi
    fi
}

ui_kill_session() {
    if [ ${#SESSIONS[@]} -gt 0 ] && [ $SELECTED -lt ${#SESSIONS[@]} ]; then
        local name="${SESSIONS[$SELECTED]}"
        if [ "$name" != "+ New session" ]; then
            EDIT_MODE="kill_confirm"
            EDIT_TEXT="$name"
        fi
    fi
}

# ---- Window Inline Actions ----
ui_window_rename() {
    if [ ${#WINDOWS[@]} -gt 0 ] && [ $WIN_SELECTED -lt ${#WINDOWS[@]} ]; then
        local entry="${WINDOWS[$WIN_SELECTED]}"
        local rest="${entry#*|}"
        local wname="${rest%|*}"
        EDIT_MODE="rename_window"
        EDIT_TEXT="$wname"
    fi
}

ui_window_kill() {
    if [ ${#WINDOWS[@]} -gt 0 ] && [ $WIN_SELECTED -lt ${#WINDOWS[@]} ]; then
        local entry="${WINDOWS[$WIN_SELECTED]}"
        local rest="${entry#*|}"
        local wname="${rest%|*}"
        EDIT_MODE="kill_window_confirm"
        EDIT_TEXT="$wname"
    fi
}

ui_toggle_help() {
    if [ "$HELP_VISIBLE" = true ]; then
        HELP_VISIBLE=false
    else
        HELP_VISIBLE=true
    fi
}

# ---- Inline Edit Actions ----
ui_edit_commit() {
    local mode="$EDIT_MODE"
    local text="$EDIT_TEXT"
    EDIT_MODE="none"
    EDIT_TEXT=""

    case "$mode" in
        create)
            [ -n "$text" ] && session_create "$text" || true
            ui_refresh_sessions
            # Move selection to the new session (second to last)
            [ ${#SESSIONS[@]} -ge 2 ] && SELECTED=$(( ${#SESSIONS[@]} - 2 ))
            ;;
        rename)
            local old_name="${SESSIONS[$SELECTED]}"
            if [ -n "$text" ] && [ "$old_name" != "+ New session" ]; then
                session_rename "$old_name" "$text" || true
            fi
            ui_refresh_sessions
            ;;
        kill_confirm)
            if [ -n "$text" ] && [ "$text" != "+ New session" ]; then
                session_kill "$text" || true
            fi
            ui_refresh_sessions
            [ $SELECTED -ge ${#SESSIONS[@]} ] && SELECTED=$(( ${#SESSIONS[@]} - 1 ))
            [ $SELECTED -lt 0 ] && SELECTED=0
            ;;
        rename_window)
            local session="${SESSIONS[$SELECTED]}"
            local entry="${WINDOWS[$WIN_SELECTED]}"
            local widx="${entry%%|*}"
            if [ -n "$text" ]; then
                tmux rename-window -t "${session}:${widx}" "$text" 2>/dev/null || true
            fi
            ui_refresh_windows
            ;;
        kill_window_confirm)
            local session="${SESSIONS[$SELECTED]}"
            local entry="${WINDOWS[$WIN_SELECTED]}"
            local widx="${entry%%|*}"
            tmux kill-window -t "${session}:${widx}" 2>/dev/null || true
            ui_refresh_windows
            [ $WIN_SELECTED -ge ${#WINDOWS[@]} ] && WIN_SELECTED=$(( ${#WINDOWS[@]} - 1 ))
            [ $WIN_SELECTED -lt 0 ] && WIN_SELECTED=0
            ;;
    esac
}

ui_edit_cancel() {
    EDIT_MODE="none"
    EDIT_TEXT=""
}
