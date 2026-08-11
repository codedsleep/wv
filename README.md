# weave

An animated tiling terminal multiplexer in Rust.

`wv` is a terminal-native tiling window manager + multiplexer with smooth, sub-cell-accurate motion at 60–160 FPS, recursive BSP splits, and its own client/server session layer for detach/reattach. No external multiplexer, no dependencies beyond the binary. Linux only.

> _Demo cast: place a recording at `docs/demo.cast` (asciinema) — embed when the public release lands._
>
> Until then, run `wv --debug` and split / close panes — the top-right HUD shows fps, frame time, in-flight tweens, and dirty-cell count per frame.

**Status:** v0.1.0 — Phase 5 polish (themes, OSC 0/2 pane titles, truecolor + 256-color fallback, rotating log file), plus tmux parity for scripting and configuration.

## Features

- **Smooth animated layout.** Every topology change is interpolated frame-by-frame with sub-cell precision. PTYs are resized only at tween completion, so children never see per-frame `SIGWINCH` storms.
- **Detach and reattach.** A session server owns the PTYs, the terminal state and the layout; the client owns only your terminal. Close the terminal and the session keeps running.
- **BSP splits.** Recursive horizontal/vertical splits with geometric focus navigation (`h/j/k/l`).
- **Configurable themes.** Hex color overrides for borders, status bar, and accent; ships with `nord` and `tokyonight` presets (default `tokyonight`).
- **Pane titles.** OSC 0/2 sequences (`printf '\e]2;hello\a'`) surface as centered top-border labels.
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
| `Alt+Q`                   | Close focused pane           |
| `Alt+D`                   | Detach, leaving the session running |
| `Alt+Shift+Q`             | Quit                         |

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
"Alt+v"     = "split-v"
"Alt+Enter" = "split-h"
"Alt+q"     = "close"
"Alt+Q"     = "quit"

[ui]
border_color = "cyan"   # legacy override; takes a back seat to [theme]
status_bar   = true
pane_titles  = true
target_fps   = 160

[theme]
preset = "tokyonight"   # or "nord"
# Per-key overrides win over the preset:
# border_focused   = "#7dcfff"
# border_unfocused = "#414868"
# status_fg        = "#c0caf5"
# status_bg        = "#1a1b26"
# accent           = "#f7768e"
```

The TOML parser accepts a single modifier (`Ctrl+` or `Alt+`). Multi-modifier chords aren't expressible there — use the tmux-syntax file below, which takes `M-C-x`.

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

Dual-licensed under either of:

- [Apache License, Version 2.0](./LICENSE-APACHE)
- [MIT license](./LICENSE-MIT)

at your option.

## Acknowledgements

- [Hyprland](https://hypr.land) — motion-feel reference for the animation curves.
- [Zellij](https://zellij.dev) — client/server session architecture reference.
- [vt100](https://crates.io/crates/vt100) — VT escape sequence parser.
- [portable-pty](https://crates.io/crates/portable-pty) — PTY abstraction.
