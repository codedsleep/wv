#!/usr/bin/env bash
# Build a weave layout from a script, then hand it to an interactive client.
#
# `wv --bare` creates a session server without attaching and prints its name.
# `wv exec` sends commands to that session over its socket -- the same commands
# the keybindings produce, so every change animates exactly as if you had typed
# it. The final `exec wv attach` replaces this shell with the client.
set -euo pipefail

session="${1:-dev}"

# Create the session (a single shell pane) unless it is already running.
if ! wv ls | awk '{print $1}' | grep -qx "$session"; then
  wv --bare --session "$session" >/dev/null
fi

run() { wv exec --session "$session" "$@"; }

# Window 1: editor on the left, two stacked shells on the right.
# `-h` is tmux's spelling: it puts the panes side by side.
run split-window -h
run select-pane -R
run split-window -v
run select-pane -L

# Window 2: a single wide pane for logs.
run select-window -t :2

# Back to the editor before attaching.
run select-window -t :1

exec wv attach "$session"
