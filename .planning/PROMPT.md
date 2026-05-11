# weave — animated tiling terminal multiplexer

> Working name. Feel free to rename. The crate is `weave`, binary is `wv`.

A terminal-native tiling window manager / multiplexer with smooth animations, written in Rust. Think tmux's process model + Hyprland's motion feel + Zellij's UX surface area, but with a clean compositor architecture that lets you swap process backends (native PTY vs. tmux control mode).

---

## Goals

- **60 FPS interpolated motion** for pane splits, swaps, resizes, opens, and closes. Animations are the headline feature — they must feel buttery, not stuttered.
- **Pluggable process backend** via a `PaneBackend` trait. Two implementations: `NativeBackend` (own the PTYs) and `TmuxBackend` (drive `tmux -CC` via control mode). The compositor is backend-agnostic.
- **Damage-tracked compositor.** Diff-and-flush rendering. Never rewrite the full screen.
- **BSP layout tree.** Binary splits with adjustable ratios; the dominant mental model from i3/Hyprland/dwindle.
- **Keyboard-first**, modal command system. Mouse support is a stretch goal.
- **Crash-resilient sessions** via the tmux backend (free attach/detach).

## Non-goals

- Not a graphical WM. We render with cells, optionally with sub-cell precision via half/quadrant block characters.
- No plugin system in v1. Hardcode features; extract later.
- No floating windows in v1. Tiling only.
- No GPU. Stdlib + libc terminal output only.
- Not a 1:1 tmux replacement. We don't reimplement copy mode, hooks, or the full command grammar.

---

## Tech stack

```toml
[dependencies]
crossterm        = "0.28"          # raw mode, alt screen, input, cursor, output
tokio            = { version = "1", features = ["full"] }
portable-pty     = "0.8"           # PTY spawning, cross-platform
vt100            = "0.15"          # VT parser → cell grid per pane
bytes            = "1"
futures          = "0.3"
serde            = { version = "1", features = ["derive"] }
toml             = "0.8"
anyhow           = "1"
thiserror        = "2"
tracing          = "0.1"
tracing-subscriber = "0.3"
unicode-width    = "0.2"
unicode-segmentation = "1"
```

Rationale:

- **crossterm** over termwiz: simpler API, wider adoption, sufficient for our needs. We're driving raw output anyway.
- **vt100** over alacritty_terminal: smaller dep, easier to integrate. Swap later if we need its scrollback or perf wins.
- **portable-pty**: works on Linux/macOS/Windows. Don't roll our own PTY syscalls.
- **tokio**: PTY I/O fan-in is naturally async.

---

## Architecture

### Module layout

```
src/
  main.rs                  # entry, arg parsing, init tracing, run app
  app.rs                   # top-level App struct, event loop wiring
  config.rs                # TOML config loading, keybind parsing
  
  backend/
    mod.rs                 # PaneBackend trait, PaneId, BackendEvent
    native.rs              # NativeBackend: portable-pty driven
    tmux.rs                # TmuxBackend: control mode driven (phase 3)
  
  term/
    mod.rs
    pane.rs                # Pane: vt100 parser + cell grid + metadata
    surface.rs             # Surface: 2D Cell buffer, blit/diff ops
    cell.rs                # Cell { ch, fg, bg, attrs }
    
  layout/
    mod.rs
    tree.rs                # BSP tree: Node enum (Internal | Leaf)
    geometry.rs            # Rect, Direction, split math
    
  anim/
    mod.rs
    tween.rs               # Tween<T>, easing functions
    timeline.rs            # animation state per node
    
  render/
    mod.rs                 # frame composition orchestrator
    compositor.rs          # blits panes into back buffer at animated rects
    diff.rs                # back vs front diff → minimal stdout writes
    chrome.rs              # borders, status bar, focus indicators
    subcell.rs             # half/quadrant block helpers for sub-cell pos
    
  input/
    mod.rs                 # crossterm event → Command
    keymap.rs              # mode-aware keybind resolution
    
  command.rs               # Command enum: SplitH, SplitV, Focus, Resize, Close, etc.
```

### Event loop (in `app.rs`)

Single-threaded, tokio-driven, fixed tick at 16ms (60 FPS):

```rust
loop {
    tokio::select! {
        _ = tick_interval.tick() => {
            self.advance_animations(dt);
            self.compose_frame();
            self.flush_diff();
        }
        Some(ev) = input_events.recv() => {
            self.handle_input(ev);
        }
        Some((pane_id, bytes)) = backend_output.recv() => {
            self.panes.get_mut(&pane_id).process(&bytes);
            // mark pane dirty; recomposed on next tick
        }
        Some(BackendEvent::PaneDied(id)) = backend_events.recv() => {
            self.handle_pane_death(id);
        }
    }
}
```

Key principle: **input and backend output mutate state; tick composes and flushes**. Never write to stdout outside the tick handler.

### `PaneBackend` trait

```rust
#[async_trait]
pub trait PaneBackend: Send {
    async fn spawn(&mut self, cmd: PaneCommand) -> Result<PaneId>;
    async fn write(&mut self, id: PaneId, bytes: &[u8]) -> Result<()>;
    async fn resize(&mut self, id: PaneId, cols: u16, rows: u16) -> Result<()>;
    async fn kill(&mut self, id: PaneId) -> Result<()>;
    
    /// Channel of (PaneId, output_bytes). Drained by the event loop.
    fn output_rx(&mut self) -> mpsc::Receiver<(PaneId, Bytes)>;
    
    /// Channel of lifecycle events (death, spawn-failure, etc.).
    fn event_rx(&mut self) -> mpsc::Receiver<BackendEvent>;
}
```

`PaneId` is an opaque newtype (`u64` internally). Each backend maintains its own ID space; the trait hides it.

### Compositor + diff

- `Surface { width, height, cells: Vec<Cell> }` — flat row-major. Two surfaces kept in `App`: `front` (last flushed) and `back` (being composed).
- Compose pass: for each pane, blit its `vt100::Screen` grid into `back` at the pane's **animated rect** (interpolated, not target).
- Chrome pass: draw borders/status bar on top of `back`.
- Diff pass: walk `front` vs `back`, emit minimal `MoveTo(x, y) + SetColors + Print(ch)` runs. Use `crossterm::queue!` + a single `stdout.flush()` per tick.
- Swap: `mem::swap(&mut front, &mut back)`, clear new back.

### BSP layout

```rust
pub enum Node {
    Leaf { pane: PaneId, rect_current: FRect, rect_target: Rect },
    Internal { direction: Split, ratio: f32, ratio_target: f32, 
               a: Box<Node>, b: Box<Node> },
}

pub enum Split { Horizontal, Vertical }
```

- `Rect` is integer cells. `FRect` is `f32` for animated interpolation.
- Mutations (`split`, `swap`, `close`, `resize`) only set `*_target`. The animation tick interpolates `_current` toward `_target` each frame.
- Recompute target rects via a recursive layout pass whenever the tree topology or root size changes.

### Animation system

```rust
pub struct Tween<T> {
    pub from: T,
    pub to: T,
    pub elapsed: Duration,
    pub duration: Duration,
    pub easing: Easing,
}

pub enum Easing {
    Linear,
    EaseOutCubic,
    EaseInOutCubic,
    EaseOutBack,   // for "bouncy" feel on splits
    EaseOutExpo,
}
```

- Default durations: open/close 220ms, split 180ms, resize 120ms, swap 200ms.
- Defaults are config-overridable.
- When mid-tween and a new target is set: don't restart — re-tween from `current` to new `to`, reset `elapsed`, keep the easing.
- Color tweens are valid too — animate border color on focus change.

### Sub-cell positioning

When `rect_current.x = 12.4`, the pane visually starts at column 12 with a left edge softened via `▌` colored as a blend of pane bg and underlying bg at α=0.4. Same trick on right/top/bottom edges. The interior of the pane stays cell-aligned.

This is what gives the motion its perceived smoothness. Without it, animations look like teleport-by-cell. **Implement this in `render/subcell.rs` and unit test the alpha blend math first.**

---

## Implementation phases

### Phase 1 — Static compositor + native backend (no animations yet)

Goal: open a shell in a single pane, get clean input/output, prove the render pipeline.

- [ ] `Cell`, `Surface`, basic blit.
- [ ] `NativeBackend` spawning one PTY via `portable-pty`, fan-out into mpsc.
- [ ] `vt100`-backed `Pane` parsing output into a screen grid.
- [ ] Compositor blitting one pane fullscreen.
- [ ] Diff renderer (front vs back) emitting via `crossterm::queue!`.
- [ ] Input passthrough: keystrokes → PTY stdin.
- [ ] Resize handling (SIGWINCH → resize PTY → recompose).
- [ ] Clean teardown: restore terminal on panic + on Ctrl+Q.

**Acceptance:** `wv` opens a working shell, `htop`/`vim`/`less` render correctly, exit returns terminal to normal.

### Phase 2 — Splits + BSP layout (still static, no animation)

- [ ] `Node`/`Rect`/`layout()` recursive pass.
- [ ] Commands: split-h, split-v, focus-{left,right,up,down}, close.
- [ ] Chrome: 1-cell borders, focus highlight via border color.
- [ ] Status bar (1 row at bottom): mode, pane count, time.
- [ ] Default keybinds (modal: prefix `Ctrl+Space`, then `s`/`v`/`hjkl`/`x`).
- [ ] Config loading from `~/.config/weave/config.toml`.

**Acceptance:** can split, focus, and close panes by keyboard. Layout recomputes correctly on terminal resize.

### Phase 3 — Animation layer

- [ ] `Tween<T>` + `Easing` + interpolation traits for `f32`, `FRect`, `Color`.
- [ ] Each `Node` carries `rect_current: FRect` alongside `rect_target: Rect`.
- [ ] Animation tick advances all in-flight tweens, marks affected regions dirty.
- [ ] Sub-cell rendering in `render/subcell.rs` for fractional edges.
- [ ] Border color tween on focus change.
- [ ] Open animation (new pane scales in from split line).
- [ ] Close animation (pane collapses, sibling expands).

**Acceptance:** all topology changes animate smoothly at 60 FPS on a kitty/wezterm/foot terminal. Motion stays at ≤16ms/frame budget under `htop`-in-every-pane load.

### Phase 4 — tmux backend

- [ ] Spawn `tmux -CC new-session -s weave` as child process.
- [ ] Control mode parser (`%output`, `%window-add`, `%pane-died`, `%exit`, etc.) — reference iTerm2's TmuxGateway as protocol guide.
- [ ] Map tmux pane IDs (`%N`) ↔ our `PaneId`.
- [ ] Send commands: `split-window`, `resize-pane`, `send-keys -l`, `kill-pane`.
- [ ] Disable tmux chrome: `set status off`, `set pane-border-status off`, no titles.
- [ ] Critical: **do not resize tmux panes mid-animation.** Animate visually; emit one `resize-pane -t %N -x C -y R` at tween end.
- [ ] Backend flag: `wv --backend tmux` or `--backend native` (default native).

**Acceptance:** can `wv --backend tmux`, detach with `Ctrl+Space d`, reattach with `wv attach`, panes still alive with state intact.

### Phase 5 — Polish

- [ ] Configurable keymaps + themes.
- [ ] Pane titles (from OSC 0/2 sequences).
- [ ] True color detection (`COLORTERM=truecolor`) with 256-color fallback.
- [ ] Logging to `~/.local/state/weave/weave.log` via `tracing`.
- [ ] `--debug` overlay showing FPS, frame time, active tweens, dirty cell count.

---

## Hard constraints

- **Never write to stdout outside the render tick.** All output goes through the diff path. Direct `print!` is a bug.
- **Never panic with raw mode active.** Install a panic hook that restores the terminal first.
- **Animations must not block.** Tweens advance in O(active_tweens); they cannot await I/O.
- **Resize is heavy.** Don't call `backend.resize()` per frame during a tween — only once at completion.
- **Diff must be the bottleneck, not allocation.** Reuse buffers across frames. No `Vec::new()` per tick.
- **Unicode width matters.** Wide characters (CJK, emoji) occupy 2 cells. Use `unicode-width` consistently; never assume `char.len()` is the display width.

---

## Project conventions

- Pre-scaffolded. Assume `cargo new --bin weave` has been run and `Cargo.toml` populated. Do not generate setup commands.
- `cargo run` should always work after each phase. Do not check in code that doesn't compile.
- Errors propagate with `anyhow::Result` at boundaries, `thiserror` for domain types.
- Tests live next to modules (`#[cfg(test)] mod tests`). Integration tests in `tests/`.
- `tracing` for logs at `info!`/`debug!`/`trace!`. No `println!` debugging — it'll corrupt the alt screen anyway.
- Formatting: `rustfmt` default. Clippy clean on `pedantic` minus `module_name_repetitions` and `must_use_candidate`.

---

## References worth consulting during implementation

- [Zellij source](https://github.com/zellij-org/zellij) — the closest existing reference. Their `zellij-server` PTY handling and `zellij-tile` renderer are instructive.
- [iTerm2 tmux gateway](https://github.com/gnachman/iTerm2) — canonical tmux control mode client. Search for `TmuxGateway` and `TmuxController`.
- [tmux control mode protocol](https://github.com/tmux/tmux/wiki/Control-Mode).
- [Hyprland's bezier curves](https://wiki.hypr.land/Configuring/Animations/) — port these easing curves directly for matching motion feel.
- [vt100 crate docs](https://docs.rs/vt100) for the parser API.
- [portable-pty docs](https://docs.rs/portable-pty) — note the `PtySystem` / `MasterPty` split.

---

## Open questions to resolve before coding

1. Status bar contents — minimal (mode + clock) or rich (pane titles + cpu/mem)? Default to minimal; rich behind a config flag.
2. Default prefix key — `Ctrl+Space` (i3-ish) or `Ctrl+B` (tmux-ish) or `Alt`-as-modifier (no prefix)? Suggest `Ctrl+Space` with no-prefix mode as an option.
3. Should `wv` work as a drop-in `tmux` CLI alias for common commands (`wv ls`, `wv attach`)? Probably yes for the tmux backend; defer.
4. Scrollback: own it in `Pane` (extend vt100) or punt entirely to the underlying shell + `less`? Punt for v1.
