# wv

<p align="center">
  <img src="assets/wv-logo.png" alt="wv" width="180" />
</p>

**Elevator pitch:** An animated tiling terminal multiplexer in Rust, with modern refinements such as agent status at a glance and a fuzzy-filtered picker to quickly navigate panes, windows and sessions.

https://github.com/user-attachments/assets/2ef50d88-3c2b-4500-9476-76b673094fce

## Features

- **Smooth animated layout.** Topology changes are interpolated frame-by-frame with sub-cell precision. PTYs are resized only at tween completion, so children never see per-frame `SIGWINCH` storms.
- **Detach and reattach.** A session server owns the PTYs, terminal state and layout; the client owns only your terminal.
- **BSP splits** with geometric focus navigation (`h/j/k/l`).
- **Configurable themes.** Hex overrides for borders, status bar and accent; `nord` (default) and `tokyonight` presets.
- **Window names from pane titles.** OSC 0/2 sequences name the window they are in. Per-pane title labels are off by default (`pane_titles = true`).
- **Agent status in the bar.** Panes running `claude`, `codex` or `opencode` are listed on the right, coloured by state across the whole session. See [Agent status](#agent-status).
- **Truecolor with graceful degradation.** Detects `COLORTERM=truecolor`; otherwise quantizes RGB to the xterm-256 cube.
- **Panic-safe.** Terminal state is always restored on crash; panic info goes to the log file.
- **`#![forbid(unsafe_code)]`** at the crate root.

## Install

```sh
curl -fsSL https://raw.githubusercontent.com/codedsleep/wv/main/install.sh | sh
```

Downloads the latest release binary, verifies its checksum and installs it to `/usr/local/bin` (asking for sudo) or `~/.local/bin` if root isn't available. The binaries are statically linked against musl, so no Rust toolchain and no runtime dependencies — x86_64 and aarch64.

```sh
... | sh -s -- --dir ~/.local/bin   # install somewhere else
... | sh -s -- --version v0.1.0     # pin a release
... | sh -s -- --uninstall          # remove it again
```

Prebuilt archives and `SHA256SUMS` are on the [releases page](https://github.com/codedsleep/wv/releases).

### From source

Requires Rust 1.75+.

```sh
cargo install --git https://github.com/codedsleep/wv --locked
```

## Quickstart

`wv` enters the alternate screen with a single shell (`$SHELL`, falling back to `/bin/sh`). Resize the host terminal and panes reflow. `wv --debug` overlays live frame stats: fps, frame time, in-flight tweens, dirty cells.

### Default keybindings

Every command uses an `Alt` chord; unbound keys pass through to the focused pane.

| Key                             | Action                                                 |
| ------------------------------- | ------------------------------------------------------ |
| `Alt+Enter`                     | Split horizontal                                       |
| `Alt+V`                         | Split vertical                                         |
| `Alt+H` / `J` / `K` / `L`       | Focus left / down / up / right                         |
| `Alt+Shift+H` / `J` / `K` / `L` | Move the focused pane                                  |
| `Alt+;`                         | Goto picker (all sessions and windows, fuzzy-filtered) |
| `Alt+R` / `Alt+Shift+R`         | Rename window / session                                |
| `Alt+Q`                         | Close focused pane                                     |
| `Alt+D`                         | Detach, leaving the session running                    |
| `Alt+Shift+Q`                   | Quit                                                   |

### Nested weave

Two weaves in one key stream cannot both own `Alt`. A weave that detects it is the inner one moves its leader from `Alt` to `Ctrl+Alt`, and its prefix from `C-b` to `C-a`, keeping every chord's letter. `Ctrl+Alt` rather than `Ctrl` because `C-d`, `C-h`, `C-l`, `C-r`, `C-v` and `C-q` belong to the shell; nothing sends `Ctrl+Alt`, and the outer weave (whose root table holds bare `Alt`) passes it through.

| Local               | Nested                      |
| ------------------- | --------------------------- |
| `Alt+Enter`         | `Ctrl+Alt+Enter`            |
| `Alt+V`             | `Ctrl+Alt+V`                |
| `Alt+H`/`J`/`K`/`L` | `Ctrl+Alt+H`/`J`/`K`/`L`    |
| `Alt+1` … `Alt+9`   | `Ctrl+Alt+1` … `Ctrl+Alt+9` |
| `Alt+;`             | `Ctrl+Alt+;`                |
| `C-b` (prefix)      | `C-a` (`nested-prefix`)     |

Detection is per-terminal and dynamic: the client reports whether its terminal is across an SSH connection, so `ssh box` then `wv` is nested and reattaching locally puts `Alt` back. One remote client among several attached terminals is enough to nest the session.

```sh
set -g nested-keys auto   # default: nested when the attached terminal is remote
set -g nested-keys on     # always
set -g nested-keys off    # never
set -g nested-prefix C-a  # empty keeps `prefix`
```

## Animation

Interpolated at `[ui] target_fps` (160 Hz default):

- **Open** — new pane grows from the split line, `EaseOutBack` (220 ms); sibling shrinks, `EaseOutCubic` (180 ms).
- **Close** — collapses to a line, `EaseOutCubic` (180 ms); sibling expands to fill.
- **Focus** — active border color cross-fades, `EaseOutCubic` (120 ms).
- **Sub-cell edges** — fractional offsets painted with half-block glyphs (`▌▐▀▄`) and sRGB-blended colors.

Curves tuned against [Hyprland's](https://wiki.hyprland.org/Configuring/Animations/) reference profiles.

## Config

`wv` reads `$XDG_CONFIG_HOME/weave/config.toml` (or `~/.config/weave/config.toml`). A missing or malformed file falls back to defaults.

```toml
[keymap.bindings]
"Alt+h"     = "focus-left"
"Alt+j"     = "focus-down"
"Alt+k"     = "focus-up"
"Alt+l"     = "focus-right"
"Alt+H"     = "move-left"
"Alt+J"     = "move-down"
"Alt+K"     = "move-up"
"Alt+L"     = "move-right"
"Alt+v"     = "split-v"
"Alt+Enter" = "split-h"
"Alt+q"     = "close"
"Alt+Q"     = "quit"

[ui]
border_color = "cyan"   # legacy override; takes a back seat to [theme]
status_bar   = true
status_powerline = true   # off falls back to plain separators
pane_titles  = false
target_fps   = 160

[theme]
preset = "nord"   # or "tokyonight"
# Per-key overrides win over the preset:
# border_focused   = "#81a1c1"
# border_unfocused = "#4c566a"
# status_fg        = "#e5e9f0"
# status_bg        = "#3b4252"
# status_segment   = "#4c566a"   # other windows, the clock
# status_session   = "#81a1c1"
# accent           = "#88c0d0"   # current window, host
# agent_working    = "#a3be8c"
# agent_waiting    = "#ebcb8b"
# agent_idle       = "#4c566a"
```

The TOML parser accepts a single modifier (`Ctrl+` or `Alt+`).

### Agent status

Each pane's foreground job is read from `/proc`. Agent panes are listed on the right of the status bar, grouped by kind and numbered within it:

Kinds appear in `agent-commands` order, not pane-creation order, so the layout holds still and only colour moves. State comes from the pane's own output — the agent is not asked and needs no setup:

| Colour | State   | Meaning                                                                                                                       |
| ------ | ------- | ----------------------------------------------------------------------------------------------------------------------------- |
| green  | working | printed within `agent-activity-time`, bottom of pane matches a working pattern, or the pane title carries the agent's spinner |
| amber  | waiting | quiet, and the bottom of the pane matches a waiting pattern                                                                   |
| grey   | idle    | quiet, asking for nothing                                                                                                     |

```sh
set -g agent-status on
set -g agent-commands 'claude,codex,opencode'
set -g agent-activity-time 2000
set -g agent-waiting-patterns 'do you want,(y/n),proceed?,continue?'
set -g agent-working-patterns 'to interrupt,esc to stop'
set -g agent-viewer-patterns 'showing detailed transcript,home/end to jump'
set -g agent-bell on
set -g agent-minimum-run 3000
```

## Sessions

The client owns your terminal — raw mode, alternate screen, input events — and nothing else. Everything that survives a detach lives in the session server: PTYs, per-pane terminal state, layout tree, animation timeline and the renderer. The server ships rendered frames down a unix socket.

```sh
wv                     # start a session (auto-named weave-<uid>) and attach
wv --session main      # start or attach to a session by name
wv --bare              # create a session without attaching; prints its name
wv attach [-d] [name]  # reattach; -d detaches everyone else first
wv ls                  # list live sessions
wv has-session [name]  # exit 0 if live, 1 if not
wv kill-server         # end every session
```

### The goto picker

`Alt+;` (also `C-b s` / `C-b w`) opens a single fuzzy-filtered list of every window in every live session.

`↑`/`↓` (or `C-p`/`C-n`, or `Tab`) move; `C-u` clears the filter; `Esc`, `C-c` or `C-g` closes. The list is gathered once when the picker opens.

Picking a window in the current session is an ordinary animated window change. Picking one in another session hands the client over — it detaches and reattaches without giving the terminal back, so there is no flash of shell. That is also what `switch-client -t other[:2]` does, and what tmux's `choose-tree`/`choose-session`/`choose-window` map onto. A session that won't answer over its socket is still listed as a bare row.

## Scripting weave

`wv exec` sends a command to a running session over its socket — the same command enum the keybindings produce, so scripted changes animate exactly like typed ones. With no `--session`, the most recent live session is used.

```sh
wv --bare --session main                      # create an empty session, print its name
wv exec --session main split-window -h
wv exec --session main select-pane -t %1
wv exec --session main select-window -t :2
exec wv attach main
```

Commands use tmux's names and flags, including `-t session:window.pane` targets:

- `split-window [-h|-v] [-d] [-p percent|-l size] [-c dir] [-t target] [command...]`
- `select-pane [-L|-R|-U|-D] [-l] [-t target]`
- `select-window [-t target] [-n|-p|-l]`, `next-window`, `previous-window`, `last-window`
- `new-window [-d] [-n name] [-c dir] [-t target] [command...]`
- `kill-window [-t target]`, `rename-window [-t target] <name>`
- `rename-session [-t target] <name>`
- `command-prompt [-p label] [-I initial] "<command> %%"`
- `kill-pane [-t target]`, `detach-client`, `kill-session`
- `display-message -p [-t target] [text]`
- `send-keys [-l] [-t target] <keys...>`
- `respawn-pane [-k] [-c dir] [-t target] [command...]`
- `resize-pane [-L|-R|-U|-D [n]] [-x n] [-y n] [-Z] [-t target]`
- `swap-pane [-U|-D|-s src|-t dst] [-d]`, `rotate-window [-U|-D]`
- `select-layout even-horizontal|even-vertical|main-vertical|main-horizontal|tiled`
- `capture-pane [-p] [-S n] [-E n] [-t target]`
- `list-panes [-a] [-F fmt]`, `list-windows [-F fmt]`, `list-sessions [-F fmt]`
- `break-pane [-s src] [-t dst] [-n name] [-d]`, `join-pane [-s src] [-t dst] [-h|-v] [-d]`
- `run-shell [-b] <command>`, `if-shell [-b] <test> <then> [else]`
- `wait-for [-S] <channel>`
- `bind-key`, `unbind-key`, `list-keys`, `set-option`, `show-options`

Targets accept pane ids (`%1`), pane indices (`.0`), window indices (`:2`), window names (`:build`), and the relative forms `+`, `-`, `!`, `{last}`, `{top}`, `{bottom}`, `{left}`, `{right}`.

Windows are nine numbered slots. An unnamed window takes its name from its focused pane's title, so `-t :vim` finds the window running vim; `rename-window` pins a name against that.

The older weave names still work: `split-h`, `split-v`, `focus-left`, `focus-right`, `focus-up`, `focus-down`, `close`, `detach`, `quit`, and `workspace-1` .. `workspace-9`.

New panes open in the focused pane's current directory, and `send-keys` puts exactly the bytes on the PTY that pressing the keys would:

```sh
wv exec --session main split-window -h -d -- npm run dev
wv exec --session main send-keys -t %1 'cargo test' Enter
wv exec --session main send-keys -t %2 C-c
```

Scripts read state back with format strings, and `wv exec` reports output on stdout and failures on stderr with a non-zero exit:

```sh
wv exec --session main list-panes -F '#{pane_id} #{pane_current_path}'
wv exec --session main capture-pane -t build.1 -p
wv exec --session main display-message -p '#{window_name}'
```

## Logs

`~/.local/state/weave/weave.log`, rotating at 10 MB with 3 archives (`weave.log.1`–`.3`). Set `WEAVE_LOG=debug` (or `trace` / `warn` / `error`).

## Development

```sh
cargo test                     # unit + integration + insta snapshots
cargo bench                    # criterion benches for the diff and compose hot paths
cargo clippy -- -D warnings    # pedantic-clean
```

## Acknowledgements

- [Hyprland](https://hypr.land) — motion-feel reference for the animation curves.
- [Zellij](https://zellij.dev) — client/server session architecture reference.
- [nord-tmux](https://github.com/nordtheme/tmux) — status bar design for the Nord theme.
- [vt100](https://crates.io/crates/vt100) — VT escape sequence parser.
- [portable-pty](https://crates.io/crates/portable-pty) — PTY abstraction.
