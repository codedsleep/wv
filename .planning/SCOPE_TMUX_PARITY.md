# weave — SCOPE_TMUX_PARITY.md

> Plan for reaching tmux feature parity, staged as independently shippable PRs.
> Goal: a `.tmux.conf` / tmux driver script has a faithful `wv` equivalent, and the
> agent-harness workflow in [SCOPE.md](./SCOPE.md) (`send-keys` + `capture-pane` into
> named panes) runs on weave instead of tmux.

---

## Where we are today

| Area | tmux | weave (v0.1.0) |
|---|---|---|
| Command model | ~90 commands, argv-parsed, with flags and targets | 9 variants, no payloads (`src/command.rs:8`) |
| Targeting | `-t session:window.pane`, `%id`, `@id`, `$id` | focused pane only; directional focus |
| Scripting I/O | commands print to stdout, exit non-zero on error | `exec` is fire-and-forget, no reply (`src/session/launch.rs:101`) |
| Pane content | any command, `-c` cwd, `send-keys`, `respawn-pane` | always `$SHELL` at the *client's* cwd (`src/app.rs:1433`) |
| Windows | dynamic, named, indexed | 9 fixed unnamed workspaces (`src/app.rs:41`) |
| Sizing | `-p/-l`, `resize-pane`, `-Z` zoom | even BSP only; `ratio`/`ratio_target` exist but are always 0.5 |
| Introspection | `list-*` with `-F` formats, `capture-pane` | `wv ls` only |
| Keys | prefix-modal, `bind-key`, runtime `set-option` | direct Alt-chords, static TOML (`src/config.rs:127`) |
| Scrollback | full, plus copy-mode and buffers | none — and staying that way (PR 8 dropped) |

**Good news:** the hard parts are already in place. `PaneCommand` carries `program/args/env/cwd`
(`src/backend/mod.rs:21`), `PaneBackend::pane_cwd` is a defined hook, the protocol is a typed
bincode duplex that already round-trips `Exec` (`src/session/protocol.rs:26`), and `Node::Internal`
already animates an arbitrary `ratio` — so resize and zoom are tween wiring, not new machinery.

---

## Decisions to lock before PR 1

1. **Config ingestion.** Do we parse real `.tmux.conf` syntax (`set -g`, `bind`, `#{...}`),
   or ship an equivalent `weave.conf` and a `tmux2weave` converter?
   *Assumed for this plan:* real tmux-syntax config in PR 7, TOML stays supported.
2. **Multi-client attach.** tmux allows N clients per session; weave evicts
   (`ExitReason::TakenOver`, `src/session/protocol.rs:60`). Parity means multi-client with
   per-client size negotiation (smallest-wins). *Assumed:* out of scope, tracked as PR 10.
3. **Workspaces → windows.** ~~Fixed 1–9 workspaces become dynamic windows in PR 4.~~
   **Resolved 2026-08-11:** they do not. The nine slots stay and gain names.
   See PR 4 below for why, and what that trades away.
4. **Explicit non-goals.** tmux control mode (`-CC`), TPM/plugins, `#{}` arithmetic and
   conditionals beyond simple substitution, tmux's own socket protocol wire-compat.

---

## Cut line for "we can test scripts"

**PRs 1, 2, 3, 6** are the minimum for real scripting. After those, `send-keys`,
`capture-pane`, targets, and command output all work — the agent-harness loop is portable.
PRs 4, 5, 7 make it comfortable. PRs 9, 10, 11 are the long tail. PR 8 is dropped.

---

# PR 1 — Command model + target addressing

**Foundation. No user-visible behavior change beyond parsing.**

- Replace `Command` (`Copy`, payload-free) with an owned enum carrying arguments.
  `Copy` is currently relied on in `Keymap::command_for` (`src/input/keymap.rs:22`) and
  in `App::execute` — both become `Clone`/by-ref.
- Real argv parser: `wv exec split-window -h -p 30 -c /srv -t dev:build.1 -- npm run dev`.
  Keep every current kebab-case name (`split-h`, `focus-left`, `workspace-3`) as an alias so
  existing scripts and `docs/examples/weave-bootstrap.sh` keep working.
- `Target` type + grammar: `session:window.pane`, `%N` pane ids, `@N` window ids,
  relative forms (`{last}`, `{next}`, `+`, `-`), and a resolver against `App` state.
- Stable, monotonic pane ids surfaced to users (currently `PaneId` is backend-local and
  explicitly documented as non-portable — needs a session-level id map).
- Parity matrix doc: every tmux command → supported / aliased / planned / won't-do.

**Files:** `src/command.rs` (grows into a module), new `src/command/target.rs`,
`src/input/keymap.rs`, `src/app.rs`, `src/session/protocol.rs`.
**Tests:** table-driven parser tests; target-resolution tests against a synthetic tree.
**Risk:** `Command` is `Serialize` in the wire protocol — bump a protocol version field now
so a stale client fails loudly instead of mis-decoding.

---

# PR 2 — Request/response protocol; `wv exec` returns data

**Every scripting idiom depends on commands being able to answer.**

- `ClientToServer::Request { id, command }` → `ServerToClient::Reply { id, result }` where
  result is `Ok(String)` / `Err(String)`.
- `wv exec` connects, sends, awaits the reply, prints stdout, exits non-zero on error.
  Today `send_command` writes the frame and drops the socket (`src/session/server.rs:199`).
- `display-message [-p]`, `wait-for -S/-L` (script barriers).
- Keep a fire-and-forget path for keybinding-driven execution so the render tick never blocks.

**Files:** `src/session/protocol.rs`, `src/session/server.rs`, `src/session/launch.rs`, `src/app.rs`.
**Tests:** extend `tests/session_smoke.rs` — exec returns output, exec of an invalid target exits 1.

---

# PR 3 — Pane spawn control and `send-keys`

**The single highest-value PR. Unblocks ~80% of real tmux scripts.**

- `split-window [-h|-v] [-c cwd] [-d] [shell-command]` and `new-window` wired through to
  `PaneCommand` — the struct already has the fields, `default_pane_command()` just ignores them.
- cwd inheritance: implement `PaneBackend::pane_cwd` for `NativeBackend` via
  `/proc/<pid>/cwd`, so new panes open where the focused pane is, as tmux does.
- `send-keys [-t target] [-l] keys...` with tmux key-name parsing (`C-c`, `M-x`, `Enter`,
  `Escape`, hex `0x..`), writing into the target pane's PTY.
- `respawn-pane [-k]`, `kill-pane -t`, `kill-window -t`.
- `-d` (don't switch focus) across creation commands.

**Files:** `src/app.rs`, `src/backend/native.rs`, new `src/input/keys.rs` (name→bytes).
**Tests:** `tests/pty_smoke.rs` — spawn a pane running `echo hi`, assert grid contents;
send-keys `C-c` interrupts a `sleep`.

---

# PR 4 — Named windows — **REVISED AND SHIPPED**

Originally "windows replace workspaces": a dynamic `Vec<Window>` with stable
`@id`s, indices distinct from positions, `move-window` and renumbering. Cut
down on 2026-08-11 after the question "do we really need to replace
workspaces?" — and the answer was no.

The two things were conflated. **Names** are what tmux configs and scripts
actually use (`-t dev:build`, `rename-window`, a status bar that says `build`
rather than `3`). **A dynamic window list** is what makes the refactor big, and
weave's `Alt+1`–`Alt+9` model gains little from an unbounded count.

**What shipped:** a `name: Option<String>` on the existing nine workspaces,
plus `new-window`, `kill-window`, `rename-window`, `next/previous/last-window`,
window-name targeting, automatic naming from the focused pane's OSC title, and
names in the status bar. `select-window` fails on a missing window as tmux
does; the `workspace-N` aliases behind `Alt+N` still create one.

**What was given up, and is now documented as won't-do:** `@id` targeting (an
index is already stable, so `:N` serves), `move-window` and `swap-window`,
renumbering, and more than nine windows.

**Cost:** about a third of the original estimate, and none of the
index-versus-position risk that made the original the highest-risk PR here.

# PR 5 — Sizing, zoom, pane manipulation — **SHIPPED**

Where weave's animation identity actually pays off: every one of these tweens.

**Shipped:** `split-window -p/-l` seeding `ratio_target`, `resize-pane`
(`-L/-R/-U/-D [n]`, `-x`, `-y`, `-Z`), `swap-pane`, `rotate-window`, and
`select-layout` with the five named presets. All of them route through one
`animate_to_new_layout` tail, so anything that reshapes a window animates.

Zoom keeps the layout tree untouched and stretches the zoomed leaf over the
window in `recompute_layout`, so unzooming animates back to the exact previous
geometry with no saved state.

**Deferred to PR 9:** `break-pane` and `join-pane`, which move panes *between*
windows and want the same machinery as the pane-movement commands there.

**Won't do:** `-b`/`-f` split placement, `select-layout -p/-o/-E` and layout
strings (weave keeps no layout history), `next-layout`.

# PR 6 — Introspection, formats, `capture-pane`

**Required for the agent-harness loop; pairs with PR 2.**

- `list-sessions`, `list-windows`, `list-panes`, each with `-F` format strings.
- Format-variable engine: `#{session_name}`, `#{window_index}`, `#{window_name}`,
  `#{pane_id}`, `#{pane_index}`, `#{pane_title}`, `#{pane_current_path}`,
  `#{pane_current_command}`, `#{pane_width/height}`, `#{?cond,a,b}`. Shared by
  `list-*`, `display-message -p`, and the status bar (PR 7's `status-left/right`).
- `capture-pane [-p] [-t] [-S start] [-E end] [-e]` reading the pane's vt100 grid
  (`src/term/pane.rs:52`). Pre-scrollback this covers the visible screen only —
  document the limit. Capturing history needs scrollback, which is out of
  scope (see "Dropped" below).
- `has-session`, `kill-session`, `rename-session`, `kill-server`.

**Files:** new `src/format.rs`, `src/app.rs`, `src/session/paths.rs`, `src/term/pane.rs`.

---

# PR 7 — Prefix key, `bind-key`, runtime options, config parity

**What lets you hand me a `.tmux.conf` and get a faithful translation.**

- Modal prefix state machine (`C-b` default, `prefix`/`prefix2` options), `-n` (root table)
  and `-r` (repeat) bindings, named key tables (`copy-mode`, `root`, custom `switch-client -T`).
  Today's Alt-chords stay as the default *root* table so nothing breaks.
- `bind-key` / `unbind-key` / `list-keys`, at runtime and from config.
- Typed option registry: `set-option -g/-w/-p`, `show-options`, `set-environment`.
  Covers `mouse`, `base-index`, `escape-time`, `history-limit`, `status-*`, `pane-border-*`,
  `default-shell`, `default-command`, plus weave-native (`target_fps`, theme).
- tmux-syntax config parser + `source-file`; TOML remains supported and takes precedence.
- Optional: `tmux2weave` converter emitting a diff of unsupported directives.

**Files:** new `src/config/tmux_conf.rs`, `src/config.rs`, `src/input/keymap.rs`, `src/app.rs`.

---

# PR 8 — Scrollback and copy-mode — **DROPPED**

Removed from this plan on 2026-08-11. It is a phase, not a PR: it needs either
`vt100`'s scrollback driven through the compositor and diff renderer, or the
`alacritty_terminal` swap [SCOPE.md](./SCOPE.md) already defers, *plus*
copy-mode's key table, selection model, search, and paste buffers. It also
touches the render hot path everything else is built on.

The numbering below is deliberately left alone. PR 5, 6, 7 and 9 are already
named in shipped error messages, docs, and commit messages; renumbering would
make those references lie.

**What stays missing without it:** `copy-mode`, `paste-buffer` and the buffer
commands, search, `capture-pane -S/-E` beyond the visible screen, and
wheel-scroll in PR 11's mouse support. `history-limit` is accepted as an
option in PR 7 but does nothing.

If it comes back, it wants its own scope document.

# PR 9 — Shell glue and hooks

- `run-shell [-b]`, `if-shell`, `pipe-pane`.
- `set-hook` / `show-hooks` for the common hook set
  (`after-split-window`, `pane-died`, `client-attached`, `session-created`).
- `command-prompt`, `confirm-before`, `display-panes`.
- Command sequences and aliases (`;` separation, `\;` escaping) in `exec`.

---

# PR 10 — Multi-client attach (decision 2)

- N clients per session, per-client terminal size with smallest-wins clamping.
- `switch-client`, `attach-session -d`, `detach-client -t`, `refresh-client`.
- Removes the `TakenOver` eviction path.

---

# PR 11 — Mouse

- SGR mouse reporting in the client, forwarded through the protocol.
- `mouse` option: click-to-focus, drag borders to resize (feeding PR 5's tweens),
  status-bar clicks. Wheel-scroll needs scrollback, which is out of scope, so
  the wheel is forwarded to the pane as an escape sequence instead.

---

## Ordering summary

```
PR 1 (command model) ──┬── PR 2 (req/resp) ──┬── PR 3 (spawn + send-keys)   ← scripts work
                       │                     └── PR 6 (list/capture/formats) ← harness works
                       ├── PR 4 (windows)  ────── PR 5 (sizing/zoom)
                       └── PR 7 (prefix/bind/options)  [needs 1; wants 6 for formats]
                                   PR 9 (hooks), PR 10 (multi-client), PR 11 (mouse)
                                   — independent of each other
                                   PR 8 (copy-mode) — dropped, see above
```

## Cross-cutting requirements for every PR

- `#![forbid(unsafe_code)]` holds; clippy pedantic clean.
- Protocol changes bump the version field introduced in PR 1.
- Each PR updates the parity matrix doc and adds to a table-driven conformance suite:
  a script's tmux transcript and its `wv` transcript must produce the same final
  `list-panes -F` output.
- README "tmux users" section updated incrementally, not at the end.
