# weave

An animated tiling terminal multiplexer in Rust.

`wv` is a terminal-native tiling window manager + multiplexer that aims for 60–160 FPS interpolated motion, BSP layout, and both native (PTY) and tmux process backends. Linux only.

**Status:** Phase 1 — single shell pane renders, accepts input, reflows on resize. No splits, no animation, no tmux backend yet.

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

`wv` enters the alternate screen with a single shell (your `$SHELL`, falling back to `/bin/sh`) filling the terminal.

- Type to interact with the shell — `ls`, `vim`, `htop`, etc.
- Resize the host terminal — the pane reflows.
- `Ctrl+Q` exits cleanly.

## Logs

Tracing output is written to `~/.local/state/weave/weave.log`. Set `WEAVE_LOG=debug` (or `trace`) for verbose output.

## Roadmap

See [`.planning/SCOPE.md`](./.planning/SCOPE.md) and [`.planning/PROMPT.md`](./.planning/PROMPT.md) for the full plan: BSP splits (Phase 2), animation layer with sub-cell precision (Phase 3), tmux control-mode backend (Phase 4), themes + release polish (Phase 5).

## License

MIT OR Apache-2.0
