# wv demo — render with `vhs demo/demo.tape` from the repo root.
#
# Needs vhs (https://github.com/charmbracelet/vhs) and wv on PATH.
# Produces demo/demo.gif for the README and demo/demo.mp4 for anywhere that
# takes video — the mp4 keeps the sub-cell colour blending that GIF's
# 256-colour palette flattens.

Require wv

Output demo/demo.gif
Output demo/demo.mp4

# A Nerd Font is needed for the powerline status bar. Without one, set
# `status_powerline = false` in the config below and any monospace font works.
Set FontFamily "JetBrainsMono Nerd Font"
Set FontSize 16
Set Width 1200
Set Height 700
Set Padding 20
Set Theme "Nord"
Set WindowBar Colorful
Set BorderRadius 8

# wv tweens at up to 160 fps; 50 is the most a GIF can actually carry.
Set Framerate 50
Set TypingSpeed 60ms

# ---------------------------------------------------------------- setup ---
# Off camera: an isolated config so the recording never depends on whatever
# is in ~/.config/weave, and a clean session name.
Hide
Type "export XDG_CONFIG_HOME=$(mktemp -d)"
Enter
Type "mkdir -p $XDG_CONFIG_HOME/weave"
Enter
Type "printf '[theme]\npreset = \"nord\"\n[ui]\ntarget_fps = 60\n' > $XDG_CONFIG_HOME/weave/config.toml"
Enter
Type "wv kill-server >/dev/null 2>&1; clear"
Enter
Type "wv --session demo"
Enter
Sleep 2s
Show

# ------------------------------------------------------------- the demo ---
Sleep 1s

# One pane, doing something real.
Type "git log --oneline --graph -12"
Enter
Sleep 2s

# Split right — the new pane grows from the split line, the sibling shrinks.
Alt+Enter
Sleep 1500ms
Type "cargo test --quiet"
Enter
Sleep 3s

# Split down from here.
Alt+V
Sleep 1500ms
Type "watch -n1 -t 'uptime; free -h | head -2'"
Enter
Sleep 3s

# Focus moves — watch the active border cross-fade.
Alt+H
Sleep 1200ms
Alt+L
Sleep 800ms
Alt+K
Sleep 1200ms

# Move a pane around the tree.
Alt+Shift+L
Sleep 2s

# The goto picker: every window in every live session, fuzzy-filtered.
Alt+;
Sleep 1500ms
Escape
Sleep 800ms

# Close a pane — it collapses to a line and the sibling expands to fill.
Alt+Q
Sleep 2s

# Detach: the server keeps every pane alive.
Alt+D
Sleep 1500ms
Type "wv ls"
Enter
Sleep 2s

# ...and reattach, exactly where it was left.
Type "wv attach demo"
Enter
Sleep 2500ms

# ------------------------------------------------------------- teardown ---
Hide
Alt+Shift+Q
Sleep 1s
Type "wv kill-server >/dev/null 2>&1"
Enter
