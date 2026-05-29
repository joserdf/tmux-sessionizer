# Restore terminal on exit
sessionizer_cleanup() {
    # Restore terminal settings
    stty echo icanon 2>/dev/null || true
    # Show cursor
    tput cnorm 2>/dev/null || true
    # Exit alternate screen
    tput rmcup 2>/dev/null || true
    tput sgr0 2>/dev/null || true
}
