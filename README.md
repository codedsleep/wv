# weave

An animated tiling terminal multiplexer in Rust.

`wv` is a terminal-native tiling window manager + multiplexer with smooth, sub-cell-accurate motion at 60–160 FPS, recursive BSP splits, and both a native PTY backend and a `tmux -CC` backend for detach/reattach. Linux only.

> _Demo cast: place a recording at `docs/demo.cast` (asciinema) — embed when the public release lands._
>
> Until then, run `wv --debug` and split / close panes — the top-right HUD shows fps, frame time, in-flight tweens, and dirty-cell count per frame.

**Status:** v0.1.0 — Phase 5 polish (themes, OSC 0/2 pane titles, truecolor + 256-color fallback, rotating log file, cargo-dist Linux release pipeline).

## Features

- **Smooth animated layout.** Every topology change is interpolated frame-by-frame with sub-cell precision. PTYs are resized only at tween completion, so children never see per-frame `SIGWINCH` storms.
- **Two backends.** Native via [`portable-pty`](https://crates.io/crates/portable-pty), or `tmux -CC` for detach/reattach with state preserved.
- **BSP splits.** Recursive horizontal/vertical splits with geometric focus navigation (`h/j/k/l`).
- **Configurable themes.** Hex color overrides for borders, status bar, and accent; ships with `nord` and `tokyonight` presets (default `tokyonight`).
- **Pane titles.** OSC 0/2 sequences (`printf '\e]2;hello\a'`) surface as centered top-border labels.
- **Truecolor with graceful degradation.** Detects `COLORTERM=truecolor`; otherwise quantizes RGB to the xterm-256 cube.
- **Panic-safe.** Terminal state is always restored on crash; panic info goes to the log file.
- **`#![forbid(unsafe_code)]`** at the crate root.

## Install

### Prebuilt binary (recommended)

Pre-built Linux binaries (x86_64 and aarch64, `.tar.xz`) are attached to each GitHub Release once published. After the first tagged release:

```sh
curl -L https://github.com/<owner>/weave/releases/latest/download/wv-x86_64-unknown-linux-gnu.tar.xz \
  | tar -xJ
sudo install -m 755 wv /usr/local/bin/
```

### cargo install

```sh
cargo install --git https://github.com/<owner>/weave --locked
```

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
| `Alt+D`                   | Detach (tmux backend only)   |
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

The config parser currently accepts a single modifier (`Ctrl+` or `Alt+`). Multi-modifier chords aren't expressible yet.

## Backends

`wv` ships with two pane backends, selected at launch:

```sh
wv                     # default: --backend native (portable-pty)
wv --backend tmux      # tmux -CC control-mode backend
```

The native backend is the simplest path. The tmux backend drives a `tmux -CC` control-mode session, which adds:

- **Detach.** `Alt+D` detaches the current `wv` session — tmux keeps it running in the background with all panes alive.
- **Reattach.** `wv attach [name]` reconnects to a previously detached weave session. With no name, picks the most recent `weave-*` session.
- **List.** `wv ls` enumerates `weave-*` tmux sessions so you can pick which to reattach.

Each tmux session is auto-named `weave-<8-hex-uid>` so multiple parallel sessions coexist. Internal tmux chrome (`status`, `pane-border-status`) is disabled at startup — `wv` draws its own.

The tmux backend parser is `#![forbid(unsafe_code)]` and covered by both unit tests and [proptest](https://crates.io/crates/proptest)-driven randomized robustness tests (`src/backend/tmux/parser.rs`). The protocol surface is modelled on [iTerm2's tmux integration](https://iterm2.com/documentation-tmux-integration.html).

## Scripting weave with tmux

When the tmux backend is active, external `tmux` commands drive the layout and weave reconciles + animates the result. tmux is the source of truth; both internal keybinds and external scripts flow through the same `%layout-change` path.

```sh
wv --session main --bare &              # create empty session, set @weave-instance marker, exit
tmux new-window  -t main -n code        # workspace 0 (window 1)
tmux split-window -h -t main:code
tmux new-window  -t main -n agents      # workspace 1 (window 2)
exec wv attach main                     # weave takes over and animates into the built layout
```

- `wv --session <name>` picks a stable, scriptable name (default is `weave-<uid>`).
- `wv --bare` creates the session, sets options, and exits without spawning a pane — leaving room for a script to populate.
- `wv exec <tmux-args...>` resolves the active weave session and runs a tmux subcommand against it.
- `wv ls --windows` lists windows including overflow windows 10+ (addressable via `Command::GotoWindow`).

tmux windows `1..9` map 1:1 to weave workspaces `0..8`; windows `10+` are overflow. Layouts produced by `select-layout main-horizontal | tiled | even-vertical` are accepted and normalized into a right-leaning BSP — exact round-trip is not guaranteed.

Full safe-command contract and a worked example are in [`docs/tmux-scripting.md`](docs/tmux-scripting.md) and [`docs/examples/weave-bootstrap.sh`](docs/examples/weave-bootstrap.sh).

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
- **No 1:1 tmux replacement.** The tmux backend exists so that detach/reattach come free; `wv` is not a tmux fork.
- **No scrollback in v1.** Punted until later.

## FAQ

**Why not just a tmux fork?**

tmux is a process supervisor with a rendering layer bolted on. weave starts from the rendering layer (animated, sub-cell-accurate compositor) and treats process supervision as a backend (`PaneBackend` trait, with `NativeBackend` and `TmuxBackend` implementations). Forking tmux would mean fighting decades of assumptions about how the screen is drawn.

**Why Linux only?**

Realistically, this is what the author runs and tests. macOS and Windows could work later but aren't in v1. The codebase is `#![forbid(unsafe_code)]` and the OS-specific surface area is small (PTY spawning, signal handlers), so a port is plausible — just not on the v1 critical path.

**Why animate at all?**

Because instantaneous layout changes are jarring once you have more than two panes — your eye can't track which pane went where. Smooth interpolation lets you actually see the topology change happen, the way a tiling Wayland WM (Hyprland) does for windows.

**Why no GPU?**

The animation budget is dominated by terminal output bandwidth (escape sequences to your host terminal), not by interpolation math. A GPU buys nothing when your bottleneck is `write(stdout)` on the other end of a Unicode-aware emulator.

**Is the tmux backend supported on remote sessions?**

It should work over SSH or in any environment where you can run `tmux -CC` and pump bytes back and forth. Latency will show up in input lag and animation smoothness; the renderer is otherwise transport-agnostic.

## License

Dual-licensed under either of:

- [Apache License, Version 2.0](./LICENSE-APACHE)
- [MIT license](./LICENSE-MIT)

at your option.

## Acknowledgements

- [Hyprland](https://hypr.land) — motion-feel reference for the animation curves.
- [iTerm2](https://iterm2.com) — `tmux -CC` integration documentation.
- [vt100](https://crates.io/crates/vt100) — VT escape sequence parser.
- [portable-pty](https://crates.io/crates/portable-pty) — PTY abstraction.
