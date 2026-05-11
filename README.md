# weave

An animated tiling terminal multiplexer in Rust.

`wv` is a terminal-native tiling window manager + multiplexer that aims for 60–160 FPS interpolated motion, BSP layout, and both native (PTY) and tmux process backends. Linux only.

**Status:** Phase 2 — recursive BSP splits, focus navigation, bordered panes, status bar, modal keymap, TOML config. No animation yet (Phase 3).

## Build

```sh
cargo build --release
./target/release/wv
```

Or run directly:

```sh
cargo run --release
```

## Try it

`wv` enters the alternate screen with a single shell (your `$SHELL`, falling back to `/bin/sh`).

- Type to interact with the focused pane.
- Resize the host terminal — panes reflow.
- The bottom row shows mode, pane count, and the clock.

### Default keybindings

`wv` is modal: in **Normal** mode, keys pass through to the focused pane. Press the **prefix** to enter **Prefix** mode for one key, then bindings below take effect.

| Mode    | Key            | Action                  |
|---------|----------------|-------------------------|
| Normal  | `Ctrl+Space`   | Enter Prefix mode       |
| Prefix  | `s`            | Split horizontal        |
| Prefix  | `v`            | Split vertical          |
| Prefix  | `h` / `j` / `k` / `l` | Focus left/down/up/right |
| Prefix  | `x`            | Close focused pane      |
| Prefix  | `q`            | Quit                    |
| Prefix  | `Esc`          | Back to Normal          |

## Config

`wv` reads `$XDG_CONFIG_HOME/weave/config.toml` (falling back to `~/.config/weave/config.toml`). A missing or malformed file falls back to defaults — `wv` will not crash on bad config.

Example:

```toml
[keymap]
prefix = "Ctrl+Space"

[keymap.bindings]
s = "split-h"
v = "split-v"
h = "focus-left"
j = "focus-down"
k = "focus-up"
l = "focus-right"
x = "close"
q = "quit"

[ui]
border_color = "cyan"
status_bar = true
```

## Logs

Tracing output is written to `~/.local/state/weave/weave.log`. Set `WEAVE_LOG=debug` (or `trace`) for verbose output.

## Roadmap

See [`.planning/SCOPE.md`](./.planning/SCOPE.md) and [`.planning/PROMPT.md`](./.planning/PROMPT.md) for the full plan: animation layer with sub-cell precision (Phase 3), tmux control-mode backend (Phase 4), themes + release polish (Phase 5).

## License

MIT OR Apache-2.0
