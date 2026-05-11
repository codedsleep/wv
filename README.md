# weave

An animated tiling terminal multiplexer in Rust.

`wv` is a terminal-native tiling window manager + multiplexer that aims for 60–160 FPS interpolated motion, BSP layout, and both native (PTY) and tmux process backends. Linux only.

**Status:** Phase 3 — splits, closes, and focus changes animate at 160 Hz with sub-cell precision. BSP layout, native PTY backend, Alt-chord keymap, TOML config, criterion benches, insta snapshot tests, `--debug` HUD.

> A demo cast lands alongside the public release at the end of Phase 3. Until then, run `wv --debug` and split/close panes — the top-right HUD shows fps, frame time, in-flight tweens, and dirty-cell count per frame.

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
- Run with `wv --debug` to overlay live frame stats in the top-right corner.

### Animation

`wv` interpolates every topology change at the monitor's refresh rate (160 Hz target, configurable via `[ui] target_fps`):

- **Open** — a new pane grows from the split line with an `EaseOutBack` overshoot (220 ms); the sibling shrinks in step (`EaseOutCubic`, 180 ms).
- **Close** — the closing pane collapses to a line (`EaseOutCubic`, 180 ms) before being removed; its sibling expands to fill.
- **Focus** — the active border color cross-fades (`EaseOutCubic`, 120 ms).
- **Sub-cell edges** — fractional pane offsets are painted with the half-block glyphs (`▌▐▀▄`) and an sRGB-blended color so motion looks smooth between cell boundaries.
- **PTYs only resize at tween completion** — children never see per-frame `SIGWINCH` storms.

### Default keybindings

`wv` follows tiling-window-manager conventions: every command uses an `Alt` chord, and any unbound key passes straight through to the focused pane.

| Key                       | Action                       |
|---------------------------|------------------------------|
| `Alt+Enter`               | Split horizontal             |
| `Alt+V`                   | Split vertical               |
| `Alt+H` / `J` / `K` / `L` | Focus left / down / up / right |
| `Alt+Q`                   | Close focused pane           |
| `Alt+Shift+Q`             | Quit                         |

## Config

`wv` reads `$XDG_CONFIG_HOME/weave/config.toml` (falling back to `~/.config/weave/config.toml`). A missing or malformed file falls back to defaults — `wv` will not crash on bad config.

Example:

```toml
[keymap.bindings]
"Alt+h" = "focus-left"
"Alt+j" = "focus-down"
"Alt+k" = "focus-up"
"Alt+l" = "focus-right"
"Alt+v" = "split-v"
"Alt+q" = "close"
"Alt+Q" = "quit"

[ui]
border_color = "cyan"
status_bar = true
target_fps = 160
```

The config parser currently accepts a single modifier (`Ctrl+` or `Alt+`). Multi-modifier chords aren't expressible in the config yet.

## Logs

Tracing output is written to `~/.local/state/weave/weave.log`. Set `WEAVE_LOG=debug` (or `trace`) for verbose output.

## Development

```sh
cargo test                     # unit + integration + insta snapshots
cargo bench                    # criterion benches for the diff and compose hot paths
cargo clippy -- -D warnings    # pedantic-clean
```

## Roadmap

See [`.planning/SCOPE.md`](./.planning/SCOPE.md) and [`.planning/PROMPT.md`](./.planning/PROMPT.md) for the full plan: tmux control-mode backend (Phase 4), themes + prebuilt-binary release (Phase 5).

## License

MIT OR Apache-2.0
