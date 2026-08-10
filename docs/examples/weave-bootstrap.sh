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

run() { wv exec --session "$session" "$1"; }

# Workspace 1: editor on the left, two stacked shells on the right.
run split-v
run focus-right
run split-h
run focus-left

# Workspace 2: a single wide pane for logs.
run workspace-2

# Back to the editor before attaching.
run workspace-1

exec wv attach "$session"
