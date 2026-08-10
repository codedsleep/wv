# Native detach: replace the tmux backend with a Zellij-style client/server session

## Context

Today `Alt+D` only works under `--backend tmux`. On the native backend `App::detach` (`src/app.rs:857`) logs a warning and quits, because `wv` *is* the process holding the PTYs — when it exits, the children die. Detach/reattach was bought by delegating to tmux, which cost ~3.2k lines of control-mode protocol code (`src/backend/tmux/`), ~950 lines of tmux-facing tests, and a large external-layout reconciliation subsystem inside `app.rs` whose only job is to keep weave in sync with tmux as the source of truth.

The goal is to own detach in pure Rust, the way Zellij does: a daemon that holds the PTYs, the vt100 state, the layout tree, the animation timeline and the compositor, plus a thin client that owns the terminal. Once that lands, the tmux backend is deleted outright — `wv` becomes a single-backend program with no external process dependency, and `app.rs` sheds the entire external-reconciliation path.

**Decisions taken:** server renders (thin client); `attach`/`ls` are unified over native sessions; one client per session at a time (a second attach takes over); build the session layer first and delete tmux in a second commit; `wv exec` is ported to the session socket so scripted layouts survive tmux's removal.

## Architecture

```
wv (client process)                      wv --server --session NAME (daemon)
  TerminalGuard: raw mode, alt screen      UnixListener on $XDG_RUNTIME_DIR/weave/NAME.sock
  crossterm EventStream ── Event ───────►  Keymap → Command → App::execute
  SIGWINCH ─────────────── Resize ──────►  App::resize_to(cols, rows)
  stdout  ◄────────────── Frame(ANSI) ───  App::tick → compose → DiffRenderer::flush
                          Detached ──────  NativeBackend owns every PTY
```

The split lands exactly at weave's existing terminal-I/O seam: `App` already funnels all output through `self.diff.flush(&front, &back, &mut self.stdout)` (`src/app.rs:1527`) and all input through `handle_input(Option<io::Result<Event>>)` (`src/app.rs:1435`). Everything else — layout, tweens, vt100 parsers — stays where it is and simply runs inside the daemon.

## Phase 1 — session layer (tmux still compiles)

### New module `src/session/`

- **`protocol.rs`** — the wire format.
  - `enum ClientToServer { Attach { cols, rows, truecolor }, Input(crossterm::event::Event), Resize { cols, rows }, Exec(Command), Detach, Quit }`
  - `enum ServerToClient { Frame(Vec<u8>), Detached, Exit(ExitReason), Error(String) }`; `enum ExitReason { Quit, TakenOver, ServerShutdown }` with user-facing text.
  - Framing: 4-byte little-endian length prefix + `bincode` payload, mirroring Zellij's `IpcSenderWithContext`. Async `read_frame`/`write_frame` helpers over `tokio::net::UnixStream` halves.
  - `Command` (`src/command.rs:6`) already derives the traits it needs bar `Serialize`/`Deserialize` — add those; it is `Copy` and tiny.
- **`paths.rs`** — `socket_dir()` = `$XDG_RUNTIME_DIR/weave/`, falling back to `/tmp/weave-<uid>/` (0700); `socket_path(name)`; `generate_session_name()` reusing the existing `weave-<8 hex>` convention; `list_sessions()` enumerating `*.sock`, probing each with a connect and unlinking stale entries.
- **`server.rs`** — binds the socket (refusing to start if a live one already holds the name, unlinking it if dead), accepts connections, decodes frames, and forwards them to `App` over a `tokio::sync::mpsc` channel. Owns socket teardown on shutdown.
- **`client.rs`** — `TerminalGuard` (`src/term/mod.rs:10`), `EventStream` → `Input` frames, SIGWINCH → `Resize`, and a loop writing `Frame` payloads straight to stdout. On `Detached` it drops the guard and prints `[detached from weave-xxxx]`; on `Exit` it prints the reason.

### Changes inside `App`

1. **Output sink.** Replace the `stdout: io::Stdout` field (`src/app.rs:129`) with an `OutputSink` enum — `Stdout(io::Stdout)`, `Client(FrameSink)`, `Null` — implementing `io::Write`. `FrameSink` buffers into a `Vec<u8>` and, on `flush`, ships one `ServerToClient::Frame` down an `mpsc::UnboundedSender` (unbounded so frame diffs are never dropped; log when the queue backs up). `tick` is otherwise untouched.
2. **Skip work with no client.** Gate the compose/diff block in `tick` (`src/app.rs:1493`) on the sink not being `Null`, so a detached session costs only PTY reads and vt100 updates — no compositing at 160 Hz into a void. Tweens still advance so timing stays honest, and `dirty` stays set.
3. **Explicit resize.** Split `handle_resize` (`src/app.rs:1462`) into a `resize_to(cols, rows)` that does the work, plus the existing SIGWINCH caller that reads `crossterm::terminal::size()`. The server calls `resize_to` from `ClientToServer::Resize`.
4. **Attach repaint.** `attach_client(sink, cols, rows)` — swap the sink, `resize_to`, reset `front` to a blank `Surface` so the next diff emits every cell, and queue a clear-screen; then `snap_leaves_to_target` (`src/app.rs:2485`) so the first frame after reattach is static rather than mid-tween.
5. **Detach.** `App::detach` (`src/app.rs:857`) becomes backend-agnostic: swap the sink to `Null`, notify the client, and stay in `ExitState::Running`. `ExitState::Detached` collapses into that; the "don't kill panes" branch in `run` (`src/app.rs:744`) is then only about server shutdown.
6. **Server-mode inputs.** Extend the `tokio::select!` in `run` (`src/app.rs:700`) with two optional branches: new client connections and decoded `ClientToServer` messages. One loop still drives everything; the socket is just another event source.
7. **Truecolor.** `DiffRenderer` reads `COLORTERM` from its own environment (`src/render/diff.rs:40`) — wrong in a daemon. Take the client's reported capability from `Attach` and set it explicitly, keeping the env probe as the local default.

### CLI

- `wv` (no args) — generate a name, spawn `std::env::current_exe()` with `--server --session NAME` (stdio null), poll-connect the socket for ~2s, then run the client. The daemon ignores SIGINT/SIGHUP so a Ctrl-C or a closed terminal cannot take the session down; SIGTERM is a clean shutdown. This keeps `#![forbid(unsafe_code)]` intact — no `fork`/`setsid`.
- `wv --session NAME`, `wv --bare` (create, don't attach), `wv attach [name]`, `wv ls` — all resolve through `session::paths`.
- `wv exec <command>` — parses via the existing `Command::from_str` (`src/command.rs:22`, already accepts `split-h`, `focus-left`, `workspace-3`, …) and sends `ClientToServer::Exec`; the session animates it exactly as a keybinding would.

## Phase 2 — remove tmux

Delete `src/backend/tmux/` (7 files), `tests/tmux_smoke.rs`, `tests/backend_parity.rs`, `benches/script_storm.rs` plus its `[[bench]]` entry, `docs/tmux-scripting.md`, and `docs/examples/weave-bootstrap.sh` (rewrite the latter two around `wv exec`). In `app.rs` remove `BackendKind` and its branching, `WindowMap`/`tmux_windows`, `external_in_flight`, `external_animation_panes`, `pending_internal_commands` with `should_queue_internal_command`, `hydrate_attached_tmux_state` through `remove_external_panes` (`src/app.rs:887`–`1188`), `goto_window`/`sync_active_tmux_window`, and every tmux session helper (`src/app.rs:1864`–`2200`). Drop `Command::GotoWindow`, the tmux-only `PaneBackend` methods (`select_window`, `select_window_by_id`, `ingest_external_pane`, and `detach`, now handled by the session layer), and rewrite `tests/script_driven.rs` against the native session. `PaneBackend` stays as a trait — it is the right seam for tests and for any future backend.

## Verification

- `cargo test` — protocol round-trip including split reads across frame boundaries, socket path/stale-socket resolution, session-name parsing, `Command` serde round-trip; existing render snapshots must stay byte-identical.
- New `tests/session_smoke.rs`: start a server bound into a temp `XDG_RUNTIME_DIR`, connect a fake client, and assert (a) attach yields a full-screen frame, (b) input reaches the PTY and its echo comes back in a later frame, (c) `Detach` closes the connection while the server stays alive with panes running, (d) reattach produces a full repaint with the pre-detach content intact, (e) a second attach evicts the first with `Exit(TakenOver)`.
- `cargo clippy -- -D warnings` (pedantic-clean is the current bar) and confirm `#![forbid(unsafe_code)]` still holds.
- Manual: `cargo run --release -- --debug`, split a few panes, start `top` in one, `Alt+D`, confirm `wv ls` lists the session and the shell processes survive, `wv attach`, confirm the layout and `top` output come back intact and animations resume; then `wv exec split-v` from another terminal and watch the running session animate.
