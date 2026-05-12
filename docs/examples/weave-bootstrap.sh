#!/usr/bin/env bash
# weave-bootstrap.sh — populate a weave-flavoured tmux session with a 4-window
# layout, then hand it over to `wv attach`.
#
# Ported from a vanilla tmux.sh-style script. The only weave-specific lines are
# the `wv --bare` line at the top and the `exec wv attach` line at the bottom;
# everything in between is plain tmux.
#
# Run:   ./weave-bootstrap.sh
# Re-run: `tmux kill-session -t weave-demo` first, or pass a different session
#         name as $1.

set -euo pipefail

SESSION="${1:-weave-demo}"
CWD="${CWD:-$HOME}"

# 1. create an empty, weave-marked tmux session and exit. --bare requires
#    --backend tmux and an explicit --session name.
wv --backend tmux --session "$SESSION" --bare

# 2. populate it with plain tmux commands. Every split / new-window emits a
#    %layout-change that weave will reconcile when we attach in step 3.

tmux new-window   -t "$SESSION:1" -n "code"     -c "$CWD"
tmux split-window -t "$SESSION:1" -v -l 10%     -c "$CWD"

tmux new-window   -t "$SESSION:2" -n "terminal" -c "$CWD"
tmux split-window -t "$SESSION:2" -h            -c "$CWD"

tmux new-window   -t "$SESSION:3" -n "agents1"  -c "$CWD"
tmux split-window -t "$SESSION:3" -h            -c "$CWD"
tmux split-window -t "$SESSION:3" -h            -c "$CWD"
tmux select-layout -t "$SESSION:3" even-horizontal

tmux new-window   -t "$SESSION:4" -n "agents2"  -c "$CWD"
tmux split-window -t "$SESSION:4" -h            -c "$CWD"
tmux split-window -t "$SESSION:4" -h            -c "$CWD"
tmux select-layout -t "$SESSION:4" even-horizontal

tmux select-window -t "$SESSION:1"

# 3. take over the session. wv hydrates the BSP forest from list-windows /
#    list-panes on the first frame (no open-animations), then animates anything
#    that happens after.
exec wv attach "$SESSION"
