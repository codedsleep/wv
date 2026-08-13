# weave

An animated tiling terminal multiplexer in Rust.

`wv` is a terminal-native tiling window manager + multiplexer with smooth, sub-cell-accurate motion at 60–160 FPS, recursive BSP splits, and its own client/server session layer for detach/reattach. No external multiplexer, no dependencies beyond the binary. Linux only.

> _Demo cast: place a recording at `docs/demo.cast` (asciinema) — embed when the public release lands._
>
> Until then, run `wv --debug` and split / close panes — the top-right HUD shows fps, frame time, in-flight tweens, and dirty-cell count per frame.

**Status:** v0.1.0 — Phase 5 polish (themes, truecolor + 256-color fallback, rotating log file), plus tmux parity for scripting and configuration.

## Features

- **Smooth animated layout.** Every topology change is interpolated frame-by-frame with sub-cell precision. PTYs are resized only at tween completion, so children never see per-frame `SIGWINCH` storms.
- **Detach and reattach.** A session server owns the PTYs, the terminal state and the layout; the client owns only your terminal. Close the terminal and the session keeps running.
- **BSP splits.** Recursive horizontal/vertical splits with geometric focus navigation (`h/j/k/l`).
- **Configurable themes.** Hex color overrides for borders, status bar, and accent; ships with `nord` and `tokyonight` presets (default `nord`).
- **Window names from pane titles.** OSC 0/2 sequences (`printf '\e]2;hello\a'`) name the window they are in, so the status bar reads `1 ❯ vim`. Per-pane title labels are off by default — a caption over every pane is noise when the pane's contents already say what it is — but `pane_titles = true` brings them back.
- **Agent status in the bar.** Panes running a coding agent (`claude`, `codex`, `opencode`) are listed on the right of the status bar, grouped by kind and coloured green while the agent is producing output, amber when it has stopped at a question, and grey when it is done. The whole session is covered, not just the window on screen, so an agent that has finished in another window is still visible. See [Agent status](#agent-status).
- **Truecolor with graceful degradation.** Detects `COLORTERM=truecolor`; otherwise quantizes RGB to the xterm-256 cube.
- **Panic-safe.** Terminal state is always restored on crash; panic info goes to the log file.
- **`#![forbid(unsafe_code)]`** at the crate root.

## Install

### cargo install (recommended)

```sh
cargo install --git https://github.com/<owner>/weave --locked
```

There are no prebuilt binaries: weave has no release pipeline, so builds are
from source.

### Build from source

Requires Rust 1.75 or newer.

```sh
git clone https://github.com/<owner>/weave
cd weave
cargo build --release
./target/release/wv
```

## Quickstart

`wv` enters the alternate screen with a single shell (`$SHELL`, falling back to `/bin/sh`). Type to interact with the focused pane; resize the host terminal and panes reflow.

Run with `--debug` to overlay live frame stats:

```sh
wv --debug
```

### Default keybindings

`wv` follows tiling-window-manager conventions: every command uses an `Alt` chord, and any unbound key passes straight through to the focused pane.

| Key                       | Action                       |
|---------------------------|------------------------------|
| `Alt+Enter`               | Split horizontal             |
| `Alt+V`                   | Split vertical               |
| `Alt+H` / `J` / `K` / `L` | Focus left / down / up / right |
| `Alt+Shift+H` / `J` / `K` / `L` | Move the focused pane left / down / up / right |
| `Alt+;`                   | Goto picker: every session and window, fuzzy-filtered |
| `Alt+R`                   | Rename the current window (prompts) |
| `Alt+Shift+R`             | Rename the session (prompts) |
| `Alt+Q`                   | Close focused pane           |
| `Alt+D`                   | Detach, leaving the session running |
| `Alt+Shift+Q`             | Quit                         |

### Nested weave

Two weaves in one key stream cannot both own `Alt`: the outer one matches the
chord and the inner one never sees the key — the trap a nested tmux falls into
with its prefix. So a weave that detects it is the inner one moves its leader
from `Alt` to `Ctrl+Alt`, and its prefix from `C-b` to `C-a`. Every chord keeps
its letter: `Alt+V` becomes `Ctrl+Alt+V`, `Alt+Shift+H` becomes
`Ctrl+Alt+Shift+H`. The outer session keeps the `Alt` chords it always had, and
the status line says when the move happens.

`Ctrl+Alt` rather than plain `Ctrl` because `Ctrl` is not weave's to take:
`C-d`, `C-h`, `C-l`, `C-r`, `C-v` and `C-q` all belong to the shell in the
pane, and a nested session that ate them would cost you your shell to save its
own keys. Nothing sends `Ctrl+Alt`, so it is free, and the outer weave passes
it through — its root table holds bare `Alt`, which a key carrying `Ctrl` as
well does not match.

| Local            | Nested                 |
|------------------|------------------------|
| `Alt+Enter`      | `Ctrl+Alt+Enter`       |
| `Alt+V`          | `Ctrl+Alt+V`           |
| `Alt+H`/`J`/`K`/`L` | `Ctrl+Alt+H`/`J`/`K`/`L` |
| `Alt+1` … `Alt+9`| `Ctrl+Alt+1` … `Ctrl+Alt+9` |
| `Alt+;`          | `Ctrl+Alt+;`           |
| `C-b` (prefix)   | `C-a` (`nested-prefix`)|

Detection is per-terminal and dynamic: the client tells the server whether its
terminal is on the far side of an SSH connection, so `ssh box` then `wv` is
nested, and detaching and reattaching locally puts `Alt` back. With several
terminals watching one session, one remote client is enough to nest it —
`Ctrl+Alt` reaches everyone, `Alt` would reach only the local ones.

```sh
set -g nested-keys auto   # default: nested when the attached terminal is remote
set -g nested-keys on     # always, for weave inside weave on one machine
set -g nested-keys off    # never; keep Alt no matter what
set -g nested-prefix C-a  # the prefix to use while nested; empty keeps `prefix`
```

Bindings you added yourself move too — a `bind -n M-s` answers to `C-M-s` while
nested — since the rule is by modifier: every root binding carrying the
leader's. One bound to something else, a bare `C-t`, stays where you put it.

The one requirement is that your terminal can *send* `Ctrl+Alt` chords. Letters
survive any terminal (they go as the meta prefix plus a control byte), but
`Ctrl+Alt+1` and `Ctrl+Alt+Enter` need the kitty keyboard protocol — as
`Ctrl+1` would, with or without weave. weave negotiates it with the pane it is
nested inside, so the inner hop is covered; it is the outermost terminal that
has to support it. kitty, foot, ghostty, WezTerm and recent Alacritty do.

## Animation

`wv` interpolates every topology change at the monitor's refresh rate (160 Hz target by default, configurable via `[ui] target_fps`):

- **Open** — a new pane grows from the split line with an `EaseOutBack` overshoot (220 ms); the sibling shrinks in step (`EaseOutCubic`, 180 ms).
- **Close** — the closing pane collapses to a line (`EaseOutCubic`, 180 ms) before being removed; its sibling expands to fill.
- **Focus** — the active border color cross-fades (`EaseOutCubic`, 120 ms).
- **Sub-cell edges** — fractional pane offsets are painted with the half-block glyphs (`▌▐▀▄`) and sRGB-blended colors so motion looks smooth between cell boundaries.

Bezier curves were tuned against [Hyprland's](https://wiki.hyprland.org/Configuring/Animations/) reference profiles.

## Config

`wv` reads `$XDG_CONFIG_HOME/weave/config.toml` (or `~/.config/weave/config.toml`). A missing or malformed file falls back to defaults — `wv` will not crash on bad config.

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
pane_titles  = false  # a title label on every pane's top border
target_fps   = 160

[theme]
preset = "nord"   # or "tokyonight"
# Per-key overrides win over the preset:
# border_focused   = "#81a1c1"   # active pane border
# border_unfocused = "#4c566a"
# status_fg        = "#e5e9f0"
# status_bg        = "#3b4252"   # the bar itself
# status_segment   = "#4c566a"   # the quiet blocks: other windows, the clock
# status_session   = "#81a1c1"   # the session block on the far left
# accent           = "#88c0d0"   # the current window, and the host
# agent_working    = "#a3be8c"
# agent_waiting    = "#ebcb8b"
# agent_idle       = "#4c566a"
```

The TOML parser accepts a single modifier (`Ctrl+` or `Alt+`). Multi-modifier chords aren't expressible there — use the tmux-syntax file below, which takes `M-C-x`.

### Agent status

Each pane's foreground job is read from `/proc`, so a shell sitting at a prompt
reports `fish` and one running an agent reports the agent. Agent panes are then
listed on the right of the status bar, grouped so agents of one kind sit
together and numbered within that kind:

```
[weave-2e5478c7] 1:main 2:api     ● 1:claude  ● 2:claude  ● 1:codex  02:34:44
```

Kinds appear in the order `agent-commands` names them rather than the order the
panes were made, so the bar's layout holds still as agents start and stop and
colour is the only thing that moves.

The colour is the state, and the state comes from the pane's own output — the
agent is not asked and needs no setup:

| Colour | State   | Meaning                                                   |
| ------ | ------- | --------------------------------------------------------- |
| green  | working | printed something within `agent-activity-time`, or the bottom of the pane matches a working pattern |
| amber  | waiting | quiet, and the bottom of the pane matches a waiting pattern |
| grey   | idle    | quiet, asking for nothing                                  |

```sh
set -g agent-status on
set -g agent-commands 'claude,codex,opencode'
set -g agent-activity-time 2000
set -g agent-waiting-patterns 'do you want,(y/n),proceed?,continue?'
set -g agent-working-patterns 'to interrupt,esc to stop'
set -g agent-bell on
set -g agent-minimum-run 3000
```

When an agent leaves the working state — finished, or stopped at a question —
weave rings the terminal bell once, so a run that ends in a window you aren't
looking at still reaches you. Your terminal decides what a bell is: a sound, a
flash, a desktop notification. Turn it off with `set -g agent-bell off`. Agents
finishing together ring once, and nothing rings for agents that were already
stopped when weave started watching.

The bell is for the pane you cannot see, so the focused one never rings. Your
own keystrokes move an agent's screen exactly like its output does, and without
that rule typing a message read as a turn and sending it read as that turn
ending. That holds after you move on, too: a run whose last screen change was
your own echo never rings, so typing into a pane and then leaving it to think
stays silent, while an agent that answers — printing long after the message
that set it off — still gets through. Nor does a screen that moves once and
settles: an idle agent still
repaints its footer, and a clock ticking over would otherwise be a whole turn
beginning and ending every minute. `agent-minimum-run` is how long a run has to
have lasted before stopping is worth hearing about — lower it if a quick answer
should ring too, raise it if a busy footer still gets through.

An agent that ends by exiting rings the same way — a one-shot run that stops
when its answer does, or an agent started as its pane's own command, which
takes the pane with it. Closing a pane yourself does not ring, and neither does
quitting an agent that had already stopped.

A turn is not a stream of output. An agent that hands a long command to a tool
prints nothing at all until it comes back, and a screen that has not moved for
a couple of seconds is otherwise indistinguishable from one whose turn is over
— so a single run rang once per tool call. `agent-working-patterns` is the way
out: while the bottom of the pane still says the turn can be interrupted, the
agent counts as working however still its screen is, and nothing rings until
the footer goes. Set it empty to go back to the activity window alone.

`agent-commands` is matched against the file name, so an agent started by
absolute path still counts. `agent-waiting-patterns` and
`agent-working-patterns` are matched case-insensitively against the last few
non-blank lines, so a question that has scrolled away no longer counts as one.

Weave itself only ever writes a `BEL`; it never raises, focuses, or otherwise
takes over your terminal's window. Some terminals do that on your behalf — in
kitty, `window_alert_on_bell yes` requests attention for the window, which a
Wayland compositor may honour by pulling it to the front. If the bell should be
heard and not seen, set `window_alert_on_bell no` and keep the sound
(`enable_audio_bell yes`, or `command_on_bell`).

### tmux-syntax config

`wv` also reads `$XDG_CONFIG_HOME/weave/weave.conf` (or `~/.config/weave/weave.conf`), applied after the TOML so it wins:

```sh
set -g prefix C-a
unbind C-b
bind C-a send-keys C-a

bind -n M-Left  select-pane -L
bind '|' split-window -h
bind '-' split-window -v
bind -r H resize-pane -L 5

source-file extra.conf
```

Lines that can't be honoured are logged with their file and line number rather than aborting the file. Options weave accepts but doesn't act on say so explicitly — `history-limit` warns that there's no scrollback rather than silently doing nothing. See [`docs/TMUX_PARITY.md`](docs/TMUX_PARITY.md).

`C-b` is the prefix, with tmux's default bindings behind it (`c`, `%`, `"`, `x`, `z`, `&`, `d`, `n`/`p`/`l`, digits, arrows). The `Alt` chords below still work with no prefix.

## Sessions

`wv` is a client/server program. The client owns your terminal — raw mode, the alternate screen, input events — and nothing else. Everything that has to survive a detach lives in a session server: the PTYs, each pane's terminal state, the layout tree, the animation timeline and the renderer itself. The server ships rendered frames down a unix socket and the client writes them to stdout.

```
wv (client)                          wv --server --session NAME (daemon)
  stdin  ── input / resize ────────►   keymap → command → layout → tween
  stdout ◄── rendered frames ───────   owns every PTY, never dies with the client
```

```sh
wv                     # start a session (auto-named weave-<uid>) and attach
wv --session main      # start or attach to a session by name
wv --bare              # create a session without attaching; prints its name
wv attach [-d] [name]  # reattach; -d detaches everyone else first
wv ls                  # list live sessions
wv has-session [name]  # exit 0 if it is live, 1 if not
wv kill-server         # end every session
```

The status bar shows the session name on the left, then the windows, then the
date, clock and host on the right — powerline blocks, laid out like tmux's nord
theme: ` dev  1 ❯ editor   2 ❯ build *      2026-05-11 ❮ 14:23  localhost `.
Set `status-powerline off` if your font is not a patched one.

### The goto picker

`Alt+;` opens the goto picker: every window in this session and in every other
live one, in a single fuzzy-filtered list. Type to narrow it, `Enter` to go.

```
┌─┤ goto ├─────────────────────────────────────────────────┐
│ > dev                                                    │
├──────────────────────────────────────────────────────────┤
│* main                                         2 windows  │
│▸ main:1 editor                                  2 panes  │
│  main:2 dev-server                               1 pane  │
│  scratch                                       1 window  │
│  scratch:1 shell                                 1 pane  │
└──────────────────────────────────────────────────────────┘
```

`↑`/`↓` (or `C-p`/`C-n`, or `Tab`) move; `C-u` clears the filter; `Esc`, `C-c`
or `C-g` closes it without going anywhere. The list is gathered once when the
picker opens, so filtering costs nothing.

Picking a window in the session you are already in is an ordinary window
change, animated like `Alt+2`. Picking one in *another* session hands your
client over: it detaches from this server and attaches to that one without ever
giving the terminal back, so there is no flash of shell in between. That is
also what `switch-client -t other` and `switch-client -t other:2` do, and what
tmux's `choose-tree`, `choose-session` and `choose-window` all map onto —
weave has one picker, and the filter line is how you narrow it. `C-b s` and
`C-b w` open it too.

A session that will not answer over its socket is still listed, as a bare
session row with no windows under it: one wedged session must not blank out the
rest of the picker.

`Alt+D` detaches: the server keeps running with every pane alive, and the client restores your terminal and prints `[detached from NAME]`. `Alt+Shift+Q` quits, which shuts the session down and kills its panes.

Sockets live in `$XDG_RUNTIME_DIR/weave/<name>.sock` (falling back to a private directory under `/tmp`). A socket whose server has gone away is unlinked automatically, so a name is never stuck.

Several terminals can watch one session at once, each with its own diff stream. The session renders at the size of the smallest attached terminal; larger ones see it in their top-left corner. `wv attach -d` joins and detaches everyone else.

The server ignores `SIGINT` and `SIGHUP`, so neither `Ctrl-C` nor closing the terminal can take a session down; `SIGTERM` shuts it down cleanly.

## Scripting weave

`wv exec` sends a command to a running session over its socket. It is the same command enum the keybindings produce, so scripted changes animate exactly like typed ones.

```sh
wv --bare --session main                      # create an empty session, print its name
wv exec --session main split-window -h        # ... drive its layout from anywhere
wv exec --session main select-pane -t %1
wv exec --session main select-window -t :2
exec wv attach main                           # ... then take it over interactively
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

Targets accept pane ids (`%1`), pane indices (`.0`), window indices (`:2`), window
names (`:build`), and the relative forms `+`, `-`, `!`, `{last}`, `{top}`,
`{bottom}`, `{left}`, `{right}`.

Windows are nine numbered slots. A window with no name of its own takes one from
its focused pane's title, so `-t :vim` finds the window running vim;
`rename-window` pins a name against that. See
[`docs/TMUX_PARITY.md`](docs/TMUX_PARITY.md) for what that trades away.

The older weave names still work: `split-h`, `split-v`, `focus-left`, `focus-right`,
`focus-up`, `focus-down`, `close`, `detach`, `quit`, and `workspace-1` .. `workspace-9`.
With no `--session`, the most recent live session is used.

New panes open in the focused pane's current directory, and `send-keys` puts
exactly the bytes on the PTY that pressing the keys would:

```sh
wv exec --session main split-window -h -d -- npm run dev
wv exec --session main send-keys -t %1 'cargo test' Enter
wv exec --session main send-keys -t %2 C-c
```

Scripts read state back out with format strings:

```sh
wv exec --session main list-panes -F '#{pane_id} #{pane_current_path}'
wv exec --session main capture-pane -t build.1 -p
wv exec --session main display-message -p '#{window_name}'
```

`wv exec` reports what the command produced: output on stdout, failures on
stderr with a non-zero exit, so a script can branch on it.

```sh
if ! wv exec --session main kill-pane -t %9; then
  echo "no such pane" >&2
fi
```

**Coming from tmux?** [`docs/TMUX_PARITY.md`](docs/TMUX_PARITY.md) is the full matrix of
what is supported, what is planned, and the one trap worth knowing: `split-window -h`
and weave's `split-h` mean opposite things, and both keep their original meaning.

## Logs

Tracing output is written to `~/.local/state/weave/weave.log`. The file rotates at 10 MB and keeps 3 numbered archives (`weave.log.1`–`weave.log.3`). Set `WEAVE_LOG=debug` (or `trace` / `warn` / `error`) for level control.

## Development

```sh
cargo test                     # unit + integration + insta snapshots
cargo bench                    # criterion benches for the diff and compose hot paths
cargo clippy -- -D warnings    # pedantic-clean
```

## Non-goals

`wv` is deliberately small. The following are **not** on the roadmap:

- **No graphical WM.** It's a terminal multiplexer, not a Wayland compositor.
- **No plugin system.** Behavior is defined in Rust; configuration is data, not code.
- **No floating windows.** Pure tiling.
- **No GPU.** Pure cell grid + diff. The "animation" is interpolated cell coordinates plus half-block sub-cell shading.
- **No Windows or macOS in v1.** Linux only.
- **No scrollback in v1.** Punted until later.

## FAQ

**Why not just a tmux fork?**

tmux is a process supervisor with a rendering layer bolted on. weave starts from the rendering layer (animated, sub-cell-accurate compositor) and treats process supervision as a backend behind the `PaneBackend` trait. Forking tmux would mean fighting decades of assumptions about how the screen is drawn.

**Why Linux only?**

Realistically, this is what the author runs and tests. macOS and Windows could work later but aren't in v1. The codebase is `#![forbid(unsafe_code)]` and the OS-specific surface area is small (PTY spawning, signal handlers), so a port is plausible — just not on the v1 critical path.

**Why animate at all?**

Because instantaneous layout changes are jarring once you have more than two panes — your eye can't track which pane went where. Smooth interpolation lets you actually see the topology change happen, the way a tiling Wayland WM (Hyprland) does for windows.

**Why no GPU?**

The animation budget is dominated by terminal output bandwidth (escape sequences to your host terminal), not by interpolation math. A GPU buys nothing when your bottleneck is `write(stdout)` on the other end of a Unicode-aware emulator.

**Does this work over SSH?**

Yes — run `wv` on the remote host and the session lives there, so a dropped connection is just a detach. Latency shows up as input lag and less smooth animation; the renderer is otherwise transport-agnostic.

## License

Copyright (C) 2026 weave contributors.

weave is free software: you can redistribute it and/or modify it under the terms
of the [GNU General Public License](./LICENSE) as published by the Free Software
Foundation, either version 3 of the License, or (at your option) any later
version.

It is distributed in the hope that it will be useful, but WITHOUT ANY WARRANTY;
without even the implied warranty of MERCHANTABILITY or FITNESS FOR A PARTICULAR
PURPOSE. See the GNU General Public License for more details.

## Acknowledgements

- [Hyprland](https://hypr.land) — motion-feel reference for the animation curves.
- [Zellij](https://zellij.dev) — client/server session architecture reference.
- [vt100](https://crates.io/crates/vt100) — VT escape sequence parser.
- [portable-pty](https://crates.io/crates/portable-pty) — PTY abstraction.
