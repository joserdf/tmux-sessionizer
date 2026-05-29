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
    local max_visible=$(( TERM_ROWS - 5 ))  # Leave space for header, preview, status
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
            echo -n "${C_CLR}${C_REVERSE} ${C_BOLD}> ${name}${C_RESET}  ${C_SELECT}[${win_count} windows]${C_RESET}"
        else
            echo -n "${C_CLR}   ${name}  ${C_BLUE}[${win_count} windows]${C_RESET}"
        fi
        row=$(( row + 1 ))
    done

    # Clear remaining lines
    tput cup $row 0 2>/dev/null || true
    echo -n "${C_CLR}"
}

ui_draw_preview() {
    local preview_row=$(( TERM_ROWS - 4 ))

    # Draw separator
    tput cup $preview_row 0 2>/dev/null || true
    printf '%*s' "$TERM_COLS" '' | tr ' ' '─' 2>/dev/null || true

    # Draw preview content
    local content_row=$(( preview_row + 1 ))
    tput cup $content_row 0 2>/dev/null || true

    if [ ${#SESSIONS[@]} -gt 0 ] && [ $SELECTED -lt ${#SESSIONS[@]} ]; then
        local name="${SESSIONS[$SELECTED]}"
        local preview=$(window_preview "$name")
        local win_count=$(window_count "$name")
        echo -n "${C_CLR}${C_BOLD}${C_PREVIEW} ${name}${C_RESET}  ${C_CYAN}${win_count} windows:${C_RESET}  ${preview}"
    else
        echo -n "${C_CLR}${C_YELLOW} No sessions${C_RESET}"
    fi
}

ui_draw_status() {
    local status_row=$(( TERM_ROWS - 1 ))
    tput cup $status_row 0 2>/dev/null || true

    if [ "$HELP_VISIBLE" = true ]; then
        echo -n "${C_CLR}${C_RESET}${C_BOLD}HELP${C_RESET}  ${C_CYAN}n${C_RESET}:new  ${C_GREEN}r${C_RESET}:rename  ${C_RED}x${C_RESET}:kill  ${C_BOLD}Enter${C_RESET}:switch  ${C_YELLOW}h${C_RESET}:hide help  ${C_BOLD}q${C_RESET}:quit"
    else
        echo -n "${C_CLR}${C_RESET}${C_BOLD}n${C_RESET}:new  ${C_BOLD}r${C_RESET}:rename  ${C_BOLD}x${C_RESET}:kill  ${C_BOLD}Enter${C_RESET}:switch  ${C_YELLOW}h${C_RESET}:help  ${C_BOLD}q${C_RESET}:quit"
    fi
}

ui_render() {
    ui_update_size
    ui_draw_header
    ui_draw_session_list
    ui_draw_preview
    ui_draw_status
}

# ---- Cleanup ----
ui_cleanup() {
    sessionizer_cleanup
}

# ---- Actions ----
ui_cursor_up() {
    [ $SELECTED -gt 0 ] && SELECTED=$(( SELECTED - 1 )) || true
}

ui_cursor_down() {
    [ $SELECTED -lt $(( ${#SESSIONS[@]} - 1 )) ] && SELECTED=$(( SELECTED + 1 )) || true
}

ui_select() {
    if [ ${#SESSIONS[@]} -gt 0 ] && [ $SELECTED -lt ${#SESSIONS[@]} ]; then
        local name="${SESSIONS[$SELECTED]}"
        session_switch "$name"
        exit 0
    fi
}

ui_create_session() {
    # End TUI so we can show prompt
    ui_cleanup
    echo -n "New session name (leave empty for timestamp): "
    read -r name
    [ -z "$name" ] && name="session-$(date +%s)"
    session_create "$name"
    session_switch "$name"
    # Re-enter TUI
    ui_init
}

ui_rename_session() {
    if [ ${#SESSIONS[@]} -gt 0 ] && [ $SELECTED -lt ${#SESSIONS[@]} ]; then
        local old_name="${SESSIONS[$SELECTED]}"
        ui_cleanup
        echo -n "Rename session '${old_name}' to: "
        read -r new_name
        [ -n "$new_name" ] && session_rename "$old_name" "$new_name"
        ui_init
    fi
}

ui_kill_session() {
    if [ ${#SESSIONS[@]} -gt 0 ] && [ $SELECTED -lt ${#SESSIONS[@]} ]; then
        local name="${SESSIONS[$SELECTED]}"
        ui_cleanup
        echo -n "Kill session '${name}'? (y/N): "
        read -r confirm
        if [ "$confirm" = "y" ] || [ "$confirm" = "Y" ]; then
            session_kill "$name"
        fi
        ui_init
    fi
}

ui_toggle_help() {
    if [ "$HELP_VISIBLE" = true ]; then
        HELP_VISIBLE=false
    else
        HELP_VISIBLE=true
    fi
}
