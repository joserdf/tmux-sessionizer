#!/bin/bash
# Strip ANSI escape sequences from stdin to stdout
# Used by tmux pipe-pane to capture clean pane content
exec sed "$(printf 's/\x1b\\[[0-9;]*[a-zA-Z]//g')"
