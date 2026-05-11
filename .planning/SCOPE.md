# weave — SCOPE.md

> Source-of-truth checklist for building **weave**, an animated tiling terminal multiplexer in Rust. Mirror of [PROMPT.md](./PROMPT.md) with discovery deltas (Linux-only, 160 FPS budget, OSS-targeted, tmux backend critical, repo private until Phase 3).
>
> **How to use this file:** work tasks top-to-bottom. Each task lists owner, files touched, acceptance criteria. Tick `[x]` inline as work lands. End-of-phase batch ticks are allowed if SCOPE edits get entangled.

---

## Agent ownership

| Agent | tmux pane | Owns |
|---|---|---|
| **Claude** (planning) | `.1` | This SCOPE.md, design reviews, motion-feel decisions, open-question resolution, golden-frame design, README/CONTRIBUTING content, dispatch coordination |
| **Codex** (backend) | `.2` | All Rust implementation: backend, parsers, render, layout, anim, FFI, benches, snapshot tests |
| **Kimi** (git) | `.3` | All git operations: `init`, `add`, `commit`, `branch`, `tag`, `push`, repo creation when going public |

**Per-task contract for Codex:** after each task, Codex prints exactly:
```
### CODEX: TASK X.Y COMPLETE — files: <relative paths>
```
Claude watches via `tmux capture-pane -t "$WIN.2" -p` and dispatches Kimi to commit with a message derived from the task title.

**End-of-phase cleanup:** after Kimi tags the phase, Claude runs `/new` on Codex and Kimi panes to clear context.

---

## Overview

**Goal:** a terminal-native tiling window manager + multiplexer with 60–160 FPS interpolated motion, BSP layout, native and tmux process backends.

**Non-goals (locked):** no graphical WM, no plugin system, no floating windows, no GPU, not a 1:1 tmux replacement, no Windows/macOS in v1, no scrollback in v1.

**Success criteria for v1 (Phase 3 complete):** `wv` opens a working shell on Linux, supports recursive splits with smooth animated transitions at the user's monitor refresh, runs cleanly under `htop`-in-every-pane load, restores terminal on panic.

**Success criteria for v1.0 release (Phase 5 complete):** above + tmux backend with detach/reattach, themes, prebuilt binaries on GitHub Releases, public repo with CONTRIBUTING.md.

---

## Architecture (snapshot)

- Single-threaded tokio event loop. Tick @ ~6.25ms (160Hz target; configurable).
- `App` owns: `layout::tree::Node`, `term::Pane[]`, `Surface front`, `Surface back`, in-flight `Tween<_>` set.
- `PaneBackend` trait: `NativeBackend` (portable-pty) and `TmuxBackend` (`tmux -CC`). Backends spawn their own internal I/O threads and surface bytes via `mpsc::Receiver<(PaneId, Bytes)>`.
- Render pipeline per tick: `advance_animations → compose (blit panes at FRect) → subcell (fractional edges) → chrome → diff → flush`.
- **Hard rule:** stdout is written *only* in the tick handler.

See PROMPT.md for the full architecture write-up.

---

## Tech stack (locked)

| Crate | Version | Purpose |
|---|---|---|
| crossterm | 0.28 | raw mode, alt screen, input, queued output |
| tokio | 1, features = `["rt", "macros", "io-util", "time", "sync", "signal"]` | async runtime (trimmed from `full`) |
| portable-pty | 0.8 | PTY spawning |
| vt100 | 0.15 | VT parser → cell grid (swap to `alacritty_terminal` deferred to Phase 5 if needed) |
| bytes | 1 | zero-copy buffers |
| futures | 0.3 | combinators |
| serde | 1, derive | config |
| toml | 0.8 | config format |
| anyhow | 1 | boundary errors |
| thiserror | 2 | domain errors |
| tracing | 0.1 | structured logs |
| tracing-subscriber | 0.3 | log sink (file only — never stdout) |
| unicode-width | 0.2 | wide-char display width |
| unicode-segmentation | 1 | grapheme iteration |
| async-trait | 0.1 | trait-object backends |
| **dev:** criterion | 0.5 | hot-path benches (Phase 3+) |
| **dev:** insta | 1 | golden-frame snapshot tests (Phase 3+) |

Crate root: `#![forbid(unsafe_code)]`. Clippy: `pedantic` minus `module_name_repetitions`, `must_use_candidate`.

---

# Phase 0 — Bootstrap

**Goal:** project skeleton compiles, runs, and is safe to crash.

- [x] **0.1 Scaffold cargo project** — *Codex* — `Cargo.toml`, `src/main.rs`
  - `cargo init --bin --name weave .` in repo root
  - Set binary name to `wv` via `[[bin]] name = "wv"`
  - Populate `[dependencies]` per stack table above
  - Set `edition = "2021"`, `rust-version = "1.75"`, `license = "MIT OR Apache-2.0"`
  - **Accept:** `cargo build` succeeds; `cargo run` prints "weave starting" and exits 0
- [x] **0.2 Crate-level lints** — *Codex* — `src/lib.rs` (or `src/main.rs`)
  - `#![forbid(unsafe_code)]`
  - `#![warn(clippy::pedantic)]` with `module_name_repetitions` and `must_use_candidate` allowed
  - **Accept:** `cargo clippy -- -D warnings` is clean
- [x] **0.3 Module skeleton** — *Codex* — full `src/` tree per PROMPT.md §Module layout
  - Empty `mod` files only; each file has a doc comment stating its responsibility
  - `app.rs`, `config.rs`, `command.rs`, `backend/{mod,native,tmux}.rs`, `term/{mod,pane,surface,cell}.rs`, `layout/{mod,tree,geometry}.rs`, `anim/{mod,tween,timeline}.rs`, `render/{mod,compositor,diff,chrome,subcell}.rs`, `input/{mod,keymap}.rs`
  - **Accept:** `cargo build` succeeds with zero code in modules
- [x] **0.4 Tracing to file** — *Codex* — `src/main.rs`
  - Initialize `tracing-subscriber` writing to `~/.local/state/weave/weave.log` (create dir if missing)
  - Default level `info`; honor `WEAVE_LOG=debug|trace` env var
  - **Forbid stdout sink** — alt screen will eat it
  - **Accept:** running `wv` creates the log file with a startup line
- [x] **0.5 Raw-mode RAII guard** — *Codex* — `src/term/mod.rs`
  - `struct TerminalGuard` that on `new()` calls `enable_raw_mode` + `EnterAlternateScreen` + `Hide` cursor
  - `Drop` impl restores: `LeaveAlternateScreen` + `disable_raw_mode` + `Show` cursor
  - **Accept:** create guard in main, sleep 1s, exit — terminal is restored
- [x] **0.6 Panic hook** — *Codex* — `src/main.rs`
  - `std::panic::set_hook` installed *before* creating `TerminalGuard`
  - Hook restores terminal state and writes panic info to the log file, then calls the previous hook
  - **Accept:** intentionally panic from main after entering raw mode — terminal restores cleanly, log contains panic info
- [x] **0.7 SIGWINCH + Ctrl-C handling** — *Codex* — `src/app.rs`
  - Install `tokio::signal` handlers for SIGWINCH (resize) and SIGINT/SIGTERM (clean exit)
  - **Accept:** Ctrl-C exits gracefully with terminal restored
- [x] **0.8 git init + initial commit** — *Kimi*
  - `git init`, add `.gitignore` (target/, *.log, .DS_Store, .vscode/)
  - Commit `chore: scaffold weave (phase 0)` *(split into atomic per-task commits)*
  - Tag `phase-0`
  - **Repo stays local — no remote yet**

**Phase 0 acceptance:** `cargo run` enters alt screen, sleeps, exits cleanly. Panic during raw mode restores the terminal. Log file is populated. Tag `phase-0` exists.

---

# Phase 1 — Static compositor + native backend

**Goal:** a single shell pane fills the screen, renders correctly, accepts input. No layout, no animation.

- [x] **1.1 `Cell` + `Surface` types** — *Codex* — `src/term/cell.rs`, `src/term/surface.rs`
  - `Cell { ch: char, fg: Color, bg: Color, attrs: CellAttrs }` (`Color` from crossterm, `CellAttrs` bitflags for bold/italic/underline/reverse)
  - `Surface { width: u16, height: u16, cells: Vec<Cell> }` row-major, indexed `y*width + x`
  - Methods: `new(w,h)`, `clear()`, `set(x,y,cell)`, `get(x,y)`, `blit(src: &Surface, dst_x, dst_y)` with clipping
  - Unit tests: blit clips at edges, get/set roundtrip, clear resets every cell
  - **Accept:** `cargo test term::` passes
- [x] **1.2 `PaneBackend` trait + types** — *Codex* — `src/backend/mod.rs`
  - `PaneId(u64)` newtype, `Copy + Eq + Hash`
  - `PaneCommand { program: String, args: Vec<String>, env: Vec<(String,String)>, cwd: Option<PathBuf> }`
  - `BackendEvent::PaneDied(PaneId)`, `::SpawnFailed(PaneId, String)`
  - `#[async_trait] trait PaneBackend: Send` with methods per PROMPT.md
  - **Accept:** trait compiles; doc comments explain ID-space ownership
- [x] **1.3 `NativeBackend` skeleton** — *Codex* — `src/backend/native.rs`
  - Holds `HashMap<PaneId, MasterPty>`, `mpsc::Sender<(PaneId, Bytes)>`, `mpsc::Sender<BackendEvent>`
  - `spawn()` creates PTY via `portable_pty::native_pty_system()`, spawns child, kicks off a blocking-thread reader that pumps reads into the output channel
  - `write()`, `resize()`, `kill()` operate on the held `MasterPty`
  - On EOF or child exit: emit `PaneDied`, drop PTY
  - **Accept:** unit-style integration test in `tests/pty_smoke.rs`: spawn `echo hello\n`, read bytes containing "hello", get `PaneDied`
- [x] **1.4 `term::Pane` (vt100-backed)** — *Codex* — `src/term/pane.rs`
  - `Pane { id: PaneId, parser: vt100::Parser, dirty: bool }`
  - `process(&mut self, bytes: &[u8])` feeds parser, sets `dirty = true`
  - `screen() -> &vt100::Screen` accessor
  - `cells_into(surface, x, y)` blits the parser's screen into a `Surface` rect, mapping `vt100::Cell` → our `Cell`
  - **Wrap behind this interface** — vt100 must not leak elsewhere (so we can swap to `alacritty_terminal` later)
  - **Accept:** unit test feeds `"hello\x1b[31mworld"` and asserts the Surface contains red `world`
- [x] **1.5 Compositor: single pane fullscreen** — *Codex* — `src/render/compositor.rs`
  - `compose(panes: &[Pane], back: &mut Surface)` blits the (one) pane at (0,0) at full size
  - **Accept:** unit test composes a pane into a 80x24 surface, asserts cells match
- [x] **1.6 Diff renderer** — *Codex* — `src/render/diff.rs`
  - `flush(front: &Surface, back: &Surface, out: &mut impl Write)`: walk row-major; for each run of differing cells, emit `MoveTo + SetForegroundColor + SetBackgroundColor + SetAttributes + Print`
  - Coalesce runs of same-style cells into one `Print` call
  - **Reuse a `Vec<u8>` queue buffer across calls** — accept `&mut Vec<u8>` or own one in a renderer struct
  - Unit test: front=blank, back=`"hi"`, assert output contains the right escape sequence prefix
  - **Accept:** test passes; manual run shows correct output
- [x] **1.7 Event loop wiring** — *Codex* — `src/app.rs`
  - `App` owns: `Surface front/back`, one `Pane`, `NativeBackend`, `stdout: io::Stdout`, queue buffer
  - `tokio::select!` over: tick interval (16ms for now — 6.25ms targeted in Phase 3), input events, backend output, backend events
  - Tick handler: compose → diff → swap buffers
  - **Accept:** `wv` opens a shell, prompt renders, `ls` works
- [x] **1.8 Input passthrough** — *Codex* — `src/input/mod.rs`
  - Read `crossterm::event::EventStream`, map `KeyEvent` → bytes (handle Ctrl/Alt modifiers, function keys, arrows)
  - Forward bytes to focused pane via `backend.write()`
  - Handle `Ctrl+Q` as global quit (temporary; will move to keymap in Phase 2)
  - **Accept:** can run `vim`, type, `:q!` exits; `htop` renders and responds to `q`
- [ ] **1.9 SIGWINCH → resize** — *Codex* — `src/app.rs`
  - On SIGWINCH: read new size via `crossterm::terminal::size()`, resize `front`/`back`, call `backend.resize(pane_id, cols, rows)`, mark dirty
  - **Accept:** resize the host terminal — pane reflows correctly without garbage
- [ ] **1.10 README stub** — *Claude* — `README.md`
  - One paragraph summary, "status: phase 1 — single pane works", build instructions
  - **Accept:** clear enough that a contributor could `cargo run` it

**Phase 1 acceptance:** `wv` opens a working shell. `htop`, `vim`, `less` render correctly. Resize works. Ctrl+Q exits cleanly. Tag `phase-1` exists.

- [ ] **1.99 commit & tag** — *Kimi* — commits per task as they land, then `git tag phase-1` after 1.10

---

# Phase 2 — Splits + BSP layout (still no animation)

**Goal:** recursive splits, focus navigation, chrome, config-driven keymap.

- [ ] **2.1 Geometry primitives** — *Codex* — `src/layout/geometry.rs`
  - `struct Rect { x: u16, y: u16, w: u16, h: u16 }`
  - `enum Direction { Left, Right, Up, Down }`
  - `enum Split { Horizontal, Vertical }`
  - `Rect::split(self, split, ratio: f32) -> (Rect, Rect)` with integer cell math, no zero-area children
  - Unit tests for split math at edges (1-cell, 2-cell, ratios near 0/1)
  - **Accept:** `cargo test layout::geometry` passes
- [ ] **2.2 BSP `Node` tree** — *Codex* — `src/layout/tree.rs`
  - `enum Node { Leaf { pane: PaneId, rect: Rect }, Internal { split: Split, ratio: f32, a: Box<Node>, b: Box<Node>, rect: Rect } }`
  - `compute_layout(&mut self, root_rect: Rect)` recursively assigns rects
  - `find_leaf(pane: PaneId) -> Option<&mut Node>`
  - `split_focused(focused: PaneId, split: Split, new_pane: PaneId)` mutates the tree
  - `close(pane: PaneId)` collapses a leaf, replacing its parent Internal with the sibling
  - `focus_neighbor(focused: PaneId, dir: Direction) -> Option<PaneId>` (geometric neighbor — pick the leaf whose rect shares the boundary)
  - Unit tests for split, close, focus_neighbor on hand-built trees
  - **Accept:** tests cover split/close/focus permutations
- [ ] **2.3 `Command` enum + dispatch** — *Codex* — `src/command.rs`, `src/app.rs`
  - `enum Command { SplitH, SplitV, FocusLeft/Right/Up/Down, Close, Quit }`
  - `App::execute(cmd)` mutates tree, spawns/kills via backend, recomputes layout, marks dirty
  - **Accept:** programmatic test: build app, execute SplitH, assert two leaves
- [ ] **2.4 Compositor: multi-pane** — *Codex* — `src/render/compositor.rs`
  - Walk leaves; blit each pane's screen at its `Rect`
  - **Accept:** integration test composes a 2-leaf tree, both panes appear
- [ ] **2.5 Chrome: borders + focus** — *Codex* — `src/render/chrome.rs`
  - 1-cell borders around every pane (use box-drawing chars: `─│┌┐└┘├┤┬┴┼`)
  - Focused pane border in accent color (default cyan); unfocused dim
  - **Pane interior shrinks by 1 cell per side** — recompute layout to account for this OR draw borders inside the rect (pick: draw inside, simpler)
  - **Accept:** visual check; borders join correctly at T/+ junctions
- [ ] **2.6 Status bar** — *Codex* — `src/render/chrome.rs`
  - 1-row strip at bottom: `[mode] panes:N HH:MM:SS`
  - Reduce layout root rect by 1 row to make room
  - **Accept:** clock ticks; mode label updates when prefix is pressed
- [ ] **2.7 Keymap + modal input** — *Codex* — `src/input/keymap.rs`
  - `enum Mode { Normal, Prefix }` — Normal forwards to focused pane; Prefix consumes one key, dispatches Command
  - Default prefix: `Ctrl+Space`
  - Default bindings (in Prefix mode): `s`→SplitH, `v`→SplitV, `h/j/k/l`→FocusLeft/Down/Up/Right, `x`→Close, `q`→Quit, `Esc`→back to Normal
  - **Accept:** all default bindings work
- [ ] **2.8 Config loading** — *Codex* — `src/config.rs`
  - Path: `~/.config/weave/config.toml` (or `$XDG_CONFIG_HOME/weave/config.toml`)
  - Schema: `[keymap.prefix] key = "Ctrl+Space"`, `[keymap.bindings] s = "split-h"` etc., `[ui] border_color = "cyan"`, `[ui] status_bar = true`
  - Missing file → defaults; parse errors → log warn + fall back to defaults (don't crash)
  - **Accept:** unit tests cover default, override, and malformed cases
- [ ] **2.9 Open-question defaults locked in** — *Claude* — record decisions in PROMPT.md or here
  - Status bar = minimal (mode + clock); rich behind future config
  - Prefix = `Ctrl+Space`
  - `wv ls`/`wv attach` aliases — defer to Phase 4
  - Scrollback — punt to v1
  - **Accept:** decisions captured in this file under §Locked decisions
- [ ] **2.10 Update README** — *Claude* — `README.md`
  - "status: phase 2 — splits work"
  - List default keybinds
  - Show example config

**Phase 2 acceptance:** can split/focus/close panes via keyboard. Layout reflows on resize. Config file works. Tag `phase-2` exists.

- [ ] **2.99 commit & tag** — *Kimi* — per-task commits, then `git tag phase-2`

---

# Phase 3 — Animation layer

**Goal:** all topology changes animate at 60–160 FPS with sub-cell precision. **Repo flips public** at the end of this phase.

- [ ] **3.1 Tween + Easing** — *Codex* — `src/anim/tween.rs`
  - `struct Tween<T: Lerp> { from: T, to: T, elapsed: Duration, duration: Duration, easing: Easing }`
  - `enum Easing { Linear, EaseOutCubic, EaseInOutCubic, EaseOutBack, EaseOutExpo }` with `apply(t: f32) -> f32`
  - **Port Hyprland's bezier curves directly** — match the motion feel
  - `trait Lerp { fn lerp(&self, other: &Self, t: f32) -> Self; }` impls for `f32`, `FRect`, `Color`
  - `Tween::value(&self) -> T` returns interpolated value
  - `Tween::advance(&mut self, dt: Duration) -> bool` returns true if still running
  - Re-target rule: if a new `to` is set mid-tween, `from = current_value`, `elapsed = 0`, keep easing
  - Unit tests: linear at midpoint, ease-out-cubic monotone, FRect lerp components, color lerp in RGB space
  - **Accept:** tests pass
- [ ] **3.2 `FRect` + node refactor** — *Codex* — `src/layout/geometry.rs`, `src/layout/tree.rs`
  - `struct FRect { x: f32, y: f32, w: f32, h: f32 }` with `to_rect()` (rounding) and `from(Rect)`
  - Refactor `Node`: leaves carry `rect_current: FRect, rect_target: Rect`; internals carry `ratio: f32, ratio_target: f32`
  - `compute_layout` updates `_target` only; never `_current`
  - **Accept:** existing layout tests still pass against `_target`
- [ ] **3.3 Animation tick** — *Codex* — `src/anim/timeline.rs`, `src/app.rs`
  - `Timeline` owns the set of in-flight tweens (per node + chrome focus color)
  - `App::advance_animations(dt)` advances all, marks all rendered regions dirty (any pane whose `FRect` changed this frame)
  - Tick interval drops to **6.25ms (160Hz)**; configurable via `[ui] target_fps = 160`
  - **Accept:** debug overlay (3.10) shows frames advancing at target rate
- [ ] **3.4 Compositor uses `FRect`** — *Codex* — `src/render/compositor.rs`
  - Round `FRect` to cells for the interior blit
  - Pass fractional offsets to `subcell::draw_edges` for soft edges
  - **Accept:** static layout still renders identically; animations look smooth
- [ ] **3.5 Sub-cell renderer** — *Codex* — `src/render/subcell.rs`
  - `draw_edges(surface, frect, fg_bg_below)` — when fractional offset on a side, paint that edge with `▌▐▀▄▘▖▝▗` and a blended color
  - **Alpha blend math first, with unit tests** — `blend(fg: Color, bg: Color, alpha: f32) -> Color` in linear-light or sRGB (pick sRGB, simpler)
  - Document blend choice in module doc comment
  - **Accept:** blend tests pass; visual smoke check at 0.0/0.25/0.5/0.75 fractional offsets
- [ ] **3.6 Border color tween on focus** — *Codex* — `src/render/chrome.rs`, `src/anim/timeline.rs`
  - On focus change, start a 120ms ease-out-cubic Color tween from old → new border color
  - **Accept:** focus change animates color
- [ ] **3.7 Open animation** — *Codex* — `src/app.rs`
  - On `SplitH/V`: new pane's `rect_current` starts at the split line (zero width/height), tweens to `rect_target` over 220ms ease-out-back
  - Sibling shrinks from old rect to new rect over 180ms ease-out-cubic
  - **Accept:** visual smoke check
- [ ] **3.8 Close animation** — *Codex* — `src/app.rs`
  - On `Close`: leaf collapses to a line over 180ms; sibling expands to fill
  - Pane is removed from the tree only after the tween completes
  - **Accept:** visual smoke check
- [ ] **3.9 Resize-at-tween-end** — *Codex* — `src/app.rs`
  - `backend.resize()` is called **only when a leaf's tween completes**, not per frame
  - Add a `debug_assert!` that catches per-frame resize calls
  - **Accept:** unit test on a mock backend confirms exactly one resize per topology change
- [ ] **3.10 `--debug` overlay** — *Codex* — `src/render/chrome.rs`
  - Top-right corner: `fps:160 frame:5.8ms tweens:3 dirty:1240`
  - Behind `wv --debug` flag
  - **Accept:** overlay renders without affecting normal output
- [ ] **3.11 Criterion benches** — *Codex* — `benches/diff.rs`, `benches/compose.rs`
  - `diff`: bench front-vs-back diff on 200x60 surfaces with 0% / 1% / 50% / 100% changed cells
  - `compose`: bench compose pass with 1, 4, 16 panes
  - **Accept:** `cargo bench` runs; numbers logged for baseline
- [ ] **3.12 Insta snapshot tests** — *Codex* — `tests/render_snapshots.rs`
  - Compose canonical scenarios (1 pane, 2-leaf horizontal, 4-leaf nested) into a Surface; render to ANSI string; snapshot via `insta::assert_snapshot!`
  - **Accept:** snapshots accepted; reruns are deterministic
- [ ] **3.13 Motion-feel review** — *Claude*
  - Drive the binary, evaluate easing curves and durations against Hyprland's reference
  - Tune Easing defaults if needed; record changes in this file
  - **Accept:** at least one written review pass with verdict
- [ ] **3.14 Update README + screenshots/cast** — *Claude* — `README.md`, `docs/`
  - Status: phase 3 — animations
  - asciinema cast or animated GIF embedded
  - **Accept:** README looks shippable
- [ ] **3.15 GitHub repo creation (private)** — *Kimi*
  - `gh repo create weave --private --source=. --remote=origin`
  - Push branches and tags
  - **Accept:** `gh repo view` shows the repo
- [ ] **3.16 Flip repo public** — *Kimi*
  - `gh repo edit weave --visibility public`
  - **Only after 3.13 sign-off from Claude**
  - **Accept:** repo is publicly visible

**Phase 3 acceptance:** all topology changes animate smoothly at the target frame rate on kitty/wezterm/foot/alacritty. Frame budget held under `htop`-in-every-pane load (verify with `--debug`). Snapshot tests + benches in CI-ready state. Repo is **public**. Tag `phase-3` exists.

- [ ] **3.99 commit & tag** — *Kimi* — per-task commits, then `git tag phase-3`

---

# Phase 4 — tmux backend

**Goal:** `wv --backend tmux` produces a session that survives detach/reattach.

- [ ] **4.1 Tmux control mode parser** — *Codex* — `src/backend/tmux/parser.rs` (new submodule)
  - Stream parser for `%output %N <data>`, `%window-add`, `%window-close`, `%pane-died %N`, `%session-changed`, `%layout-change`, `%exit`, `%begin/%end/%error` blocks
  - Returns `Vec<TmuxNotification>` per chunk
  - **Reference iTerm2's TmuxGateway** for edge cases (escaping, partial lines, nested begin/end)
  - Unit tests for each notification type, including malformed inputs (must not panic)
  - **Accept:** all tests pass; parser is `#[forbid(unsafe_code)]`-clean
- [ ] **4.2 Fuzz the parser** — *Codex* — `fuzz/fuzz_targets/tmux_parser.rs`
  - `cargo fuzz init` (one-time), target = parser entry point
  - Run `cargo fuzz run tmux_parser -- -max_total_time=60` locally; commit corpus
  - **Accept:** zero crashes after 60s fuzz
- [ ] **4.3 `TmuxBackend` impl** — *Codex* — `src/backend/tmux/mod.rs`, `tmux/process.rs`
  - Spawn `tmux -CC new-session -s weave -d` (detached create) then attach via `-CC attach`
  - One internal task pumps tmux stdout → parser → BackendEvent / output channel
  - Maps tmux pane IDs (`%42`) ↔ our `PaneId(u64)` via `BiMap` (or two HashMaps)
  - Implements `PaneBackend` trait
  - On `spawn`: send `split-window -P -F '#{pane_id}'`, capture the new `%N`
  - On `write`: `send-keys -t %N -l '<bytes>'` (literal mode)
  - On `resize`: `resize-pane -t %N -x C -y R` (called only at tween end per 3.9)
  - On `kill`: `kill-pane -t %N`
  - **Disable tmux chrome:** at startup, `set -g status off`, `set -g pane-border-status off`
  - **Accept:** integration test starts a session, spawns echo, captures output, kills pane
- [ ] **4.4 `--backend` flag + dispatch** — *Codex* — `src/main.rs`, `src/app.rs`
  - `wv --backend native|tmux` (default `native`)
  - `App` becomes generic over `Box<dyn PaneBackend>`
  - **Accept:** both backends launch and pass the same Phase 1–3 smoke tests
- [ ] **4.5 Detach/attach** — *Codex* — `src/app.rs`, `src/input/keymap.rs`
  - Default Prefix binding `d` → detach (clean exit, leave tmux session running)
  - `wv attach` subcommand → reattach to the most recent weave session
  - **Accept:** detach, run shell tasks, `wv attach`, panes still alive with state intact
- [ ] **4.6 Session naming** — *Codex* — `src/backend/tmux/mod.rs`
  - Auto-generate: `weave-<short-uid>`; `wv ls` lists weave-prefixed sessions; `wv attach <name>` attaches a specific one
  - **Accept:** can have two parallel sessions and attach the right one
- [ ] **4.7 Backend parity test** — *Codex* — `tests/backend_parity.rs`
  - Same scripted scenario (spawn 4 panes, write text, close one) under both backends; assert composed Surface matches
  - **Accept:** parity test passes (or documented diffs are intentional)
- [ ] **4.8 Update README** — *Claude*
  - Document `--backend tmux`, detach/attach workflow, the iTerm2/tmux protocol reference link

**Phase 4 acceptance:** `wv --backend tmux` runs everything Phase 3 did, plus survives detach/reattach with state intact. Parser is fuzz-clean. Tag `phase-4` exists.

- [ ] **4.99 commit & tag** — *Kimi* — per-task commits, then `git tag phase-4`

---

# Phase 5 — Polish + release

**Goal:** v0.1.0 ship: themes, titles, debug, prebuilt binaries, contributor docs.

- [ ] **5.1 Configurable themes** — *Codex* — `src/config.rs`, `src/render/chrome.rs`
  - `[theme] border_focused = "#7dcfff"`, `border_unfocused = "#3b4252"`, `status_fg = "#eceff4"`, `status_bg = "#2e3440"`, `accent = "#bf616a"`
  - Ship 2 built-ins: `nord`, `tokyonight` selectable via `[theme] preset = "nord"`
  - **Accept:** swap themes via config without restart? (defer hot-reload; restart is fine)
- [ ] **5.2 Pane titles via OSC 0/2** — *Codex* — `src/term/pane.rs`
  - vt100 surfaces `title()` from OSC 0/2 sequences; render in border-top center when present and `[ui] pane_titles = true`
  - **Accept:** `printf '\e]2;hello\a'` updates the border title
- [ ] **5.3 Truecolor detection + 256-color fallback** — *Codex* — `src/render/diff.rs`
  - Detect: `COLORTERM=truecolor` or `=24bit` → truecolor; else nearest-256
  - All theme colors quantize gracefully
  - **Accept:** runs cleanly in a 256-color-only terminal (test under `TERM=xterm-256color COLORTERM=`)
- [ ] **5.4 Logging polish** — *Codex* — `src/main.rs`
  - Rotate weave.log at 10MB; keep 3 archives
  - `WEAVE_LOG=debug` for verbose
  - **Accept:** log rotation observable
- [ ] **5.5 cargo-dist setup** — *Codex* — `Cargo.toml`, `.github/workflows/release.yml`
  - `cargo dist init` (one-time), targets `x86_64-unknown-linux-gnu` and `aarch64-unknown-linux-gnu`
  - Static-ish musl build optional; gnu acceptable for v0.1.0
  - On `git tag v*` push: builds release artifacts and creates a GitHub Release draft
  - **Accept:** dry-run a release tag on a side branch; artifacts produced
- [ ] **5.6 GitHub Actions CI** — *Codex* — `.github/workflows/ci.yml`
  - Jobs: `cargo fmt --check`, `cargo clippy -- -D warnings`, `cargo test`, `cargo bench --no-run`
  - Ubuntu runners only (Linux-only project)
  - **Accept:** CI green on a sample PR
- [ ] **5.7 CONTRIBUTING.md** — *Claude*
  - How to build, run, test, file issues
  - Architecture overview (link to PROMPT.md)
  - Note: tmux protocol parser is fuzzed; add new edge cases via the corpus
  - **Accept:** doc reads cleanly
- [ ] **5.8 README final pass** — *Claude*
  - Animated demo (asciinema preferred for terminal-native rendering)
  - Install instructions: prebuilt binary (preferred), `cargo install --git ...`, build from source
  - Feature list, non-goals, FAQ (why not a tmux fork?)
  - **Accept:** README is the kind you'd star
- [ ] **5.9 LICENSE** — *Claude* — `LICENSE-MIT`, `LICENSE-APACHE`
  - Dual MIT/Apache-2.0 (Rust standard)
  - **Accept:** files exist, Cargo.toml `license = "MIT OR Apache-2.0"` matches
- [ ] **5.10 Issue templates** — *Claude* — `.github/ISSUE_TEMPLATE/`
  - bug.yml, feature.yml, terminal-compat.yml
  - **Accept:** templates render in the GitHub UI
- [ ] **5.11 v0.1.0 release** — *Kimi*
  - `git tag v0.1.0`, push tag, watch cargo-dist build, edit GitHub Release notes
  - **Accept:** release page is live with binaries for download

**Phase 5 acceptance:** v0.1.0 is downloadable from GitHub Releases. CI is green. Themes work. CONTRIBUTING.md welcomes external contributors. Repo looks like one a stranger would clone.

- [ ] **5.99 commit & tag** — *Kimi* — `git tag v0.1.0`

---

## Locked decisions (from discovery)

- **Platform:** Linux only (no macOS, no Windows in v1)
- **Terminals tested:** kitty, wezterm, foot, ghostty, alacritty
- **Frame budget:** 6.25ms target (160Hz); configurable down to 16ms (60Hz)
- **Stack:** crossterm + tokio + portable-pty + vt100 (swap to alacritty_terminal deferred to Phase 5+ if needed)
- **Status bar:** minimal (mode + pane count + clock); rich behind future config
- **Prefix key:** `Ctrl+Space`
- **Scrollback:** punted to v1+
- **`wv` as tmux CLI alias:** `wv ls`, `wv attach` ship in Phase 4 only
- **Repo visibility:** private through Phase 2; public at end of Phase 3
- **Distribution:** prebuilt binaries via GitHub Releases (cargo-dist), Phase 5
- **Testing:** unit tests + insta golden frames + criterion benches + a few real-PTY smoke tests
- **License:** MIT OR Apache-2.0 (dual)

---

## Hard rules (from PROMPT.md, do not violate)

1. **Stdout is touched only in the render tick.** Direct `print!` is a bug. Tracing must go to file.
2. **No panic in raw mode without restoration.** Panic hook installed before raw-mode entry, always.
3. **Animations never await I/O.** Tweens are pure compute.
4. **`backend.resize()` runs only at tween completion**, never per frame. Debug-asserted.
5. **Diff is the bottleneck, not allocation.** Reuse buffers across frames.
6. **Unicode width is mandatory** — wide chars take 2 cells. Use `unicode-width`.
7. **`#![forbid(unsafe_code)]`** at the crate root. No exceptions in v1.

---

## How dispatch works in practice

Claude (you) drives this file top-to-bottom. For each task:

1. Claude dispatches Codex (`.2`) with the task scope:
   ```
   tmux send-keys -t "$WIN.2" -l "<task brief>"; tmux send-keys -t "$WIN.2" Enter
   ```
2. Codex implements and prints `### CODEX: TASK X.Y COMPLETE — files: …`.
3. Claude reads the pane (`tmux capture-pane -t "$WIN.2" -p`), reviews the diff if architectural, then dispatches Kimi (`.3`) to commit:
   ```
   tmux send-keys -t "$WIN.3" -l "commit task X.Y: <message>"; tmux send-keys -t "$WIN.3" Enter
   ```
4. Claude ticks the box in this file inline (or batches at end of phase if entangled).
5. After the phase tag lands, Claude runs `/new` on Codex and Kimi panes to clear context.
