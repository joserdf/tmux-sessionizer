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
    local max_visible=$(( TERM_ROWS - 11 ))  # Leave space for header, preview, status
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
    local sep_row=$(( TERM_ROWS - 8 ))
    local header_row=$(( sep_row + 1 ))
    local content_start=$(( header_row + 1 ))
    local num_lines=$(( TERM_ROWS - 2 - content_start ))  # lines available for content

    # Draw separator
    tput cup $sep_row 0 2>/dev/null || true
    printf '%*s' "$TERM_COLS" '' | tr ' ' '─' 2>/dev/null || true

    # Clear entire preview area first
    local r=$header_row
    while [ $r -lt $(( TERM_ROWS - 1 )) ]; do
        tput cup $r 0 2>/dev/null || true
        echo -n "${C_CLR}"
        r=$(( r + 1 ))
    done

    if [ ${#SESSIONS[@]} -eq 0 ] || [ $SELECTED -ge ${#SESSIONS[@]} ]; then
        tput cup $header_row 0 2>/dev/null || true
        echo -n "${C_CLR}${C_YELLOW} No sessions${C_RESET}"
        return
    fi

    local name="${SESSIONS[$SELECTED]}"
    local sep=" | "
    local col_width=$(( (TERM_COLS - ${#sep} * 2) / 3 ))
    local cap=$(( num_lines + 3 ))  # capture a few extra lines

    # Get first 3 windows: idx|name|active
    local wins=()
    local wcount=0
    while IFS='|' read -r widx wname active; do
        [ -z "$widx" ] && continue
        wins+=("${widx}|${wname}|${active}")
        wcount=$(( wcount + 1 ))
        [ $wcount -ge 3 ] && break
    done <<< "$(window_list "$name")"

    [ $wcount -eq 0 ] && return

    # --- Draw column headers (row = header_row) ---
    tput cup $header_row 0 2>/dev/null || true
    local header_line=""
    for ((i = 0; i < wcount; i++)); do
        local win_entry="${wins[$i]}"
        local widx=$(echo "$win_entry" | cut -d'|' -f1)
        local wname=$(echo "$win_entry" | cut -d'|' -f2)
        local wactive=$(echo "$win_entry" | cut -d'|' -f3)

        local label="${widx}:${wname}"
        [ "$wactive" = "1" ] && label="${label}*"

        # Colored label
        local style="${C_CLR}${C_BOLD}${C_CYAN}${label}${C_RESET}"

        # Pad to col_width (label may have color codes, approximate)
        local plain_len=$(( ${#widx} + 1 + ${#wname} ))
        [ "$wactive" = "1" ] && plain_len=$(( plain_len + 1 ))

        local pad=$(( col_width - plain_len ))
        [ $pad -lt 0 ] && pad=0
        local left_pad=$(( pad / 2 ))
        local right_pad=$(( pad - left_pad ))

        if [ "$i" -gt 0 ]; then
            header_line="${header_line}${sep}"
        fi
        header_line="${header_line}$(printf '%*s' $left_pad '')${style}$(printf '%*s' $right_pad '')"
    done
    echo -n "${header_line}"

    # --- Capture content for each window ---
    local cap_wins=()
    for ((i = 0; i < wcount; i++)); do
        local win_entry="${wins[$i]}"
        local widx=$(echo "$win_entry" | cut -d'|' -f1)
        local captured
        captured=$(window_capture_specific "$name" "$widx" "$cap" 2>/dev/null || true)
        # Trim trailing blanks
        if [ -n "$captured" ]; then
            captured=$(echo "$captured" | sed -e :a -e '/^[[:space:]]*$/{$d;N;ba}' || true)
        fi
        cap_wins[$i]="$captured"
    done

    # --- Draw content lines side by side ---
    # Pre-count lines per window
    local line_counts=()
    local starts=()
    for ((i = 0; i < wcount; i++)); do
        local captured="${cap_wins[$i]}"
        local total=0
        if [ -n "$captured" ]; then
            total=$(echo "$captured" | wc -l)
        fi
        line_counts[$i]=$total
        local s=$(( total - num_lines ))
        [ $s -lt 1 ] && s=1
        starts[$i]=$s
    done

    local line_idx=0
    local done=false
    while [ $line_idx -lt $num_lines ] && [ "$done" = false ]; do
        tput cup $(( content_start + line_idx )) 0 2>/dev/null || true
        local row_line=""
        local any_data=false

        for ((i = 0; i < wcount; i++)); do
            local captured="${cap_wins[$i]}"
            local total=${line_counts[$i]}
            local start=${starts[$i]}
            local src=$(( start + line_idx ))

            local text=""
            if [ -n "$captured" ] && [ $src -le $total ]; then
                text=$(echo "$captured" | sed -n "${src}p" || true)
                any_data=true
            fi

            # Format: truncate to col_width and right-pad
            local truncated="${text:0:$col_width}"
            local tlen=${#truncated}
            local pad=$(( col_width - tlen ))
            [ $pad -lt 0 ] && pad=0

            if [ "$i" -gt 0 ]; then
                row_line="${row_line}${sep}"
            fi
            row_line="${row_line}${C_CLR}${truncated}$(printf '%*s' $pad '')"
        done

        echo -n "${row_line}"
        line_idx=$(( line_idx + 1 ))

        # Stop if all columns have no data and we've shown at least 1 row
        if [ "$any_data" = false ] && [ $line_idx -gt 1 ]; then
            break
        fi
    done
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

ui_select() {
    if [ ${#SESSIONS[@]} -gt 0 ] && [ $SELECTED -lt ${#SESSIONS[@]} ]; then
        local name="${SESSIONS[$SELECTED]}"
        session_switch "$name"
        exit 0
    fi
}


ui_create_session() {
    ui_cleanup
    echo -n "New session name (empty for timestamp): "
    read -r name
    [ -z "$name" ] && name="session-$(date +%s)"
    session_create "$name"
    session_switch "$name"
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
