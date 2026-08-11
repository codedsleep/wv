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

# PR 6 — Introspection, formats, `capture-pane` — **SHIPPED**

The milestone: the agent-harness loop in [SCOPE.md](./SCOPE.md) is portable.
`tmux capture-pane -t "$WIN.2" -p` now has a direct equivalent, verified
against a live session driving a pane by window *name*.

**Shipped:** `src/format.rs` (variables, `#X` shorthands, `#{?..}`
conditionals, `##`), `list-panes [-a]`, `list-windows`, `list-sessions`,
`capture-pane`, `display-message` expanding formats, `wv has-session`,
`wv kill-server`, and `PaneBackend::pane_process_name`.

Format arithmetic and comparisons are **refused** rather than expanded to
nothing: a silently empty field in a listing is worse than a failed command.

**Divergences, documented in docs/TMUX_PARITY.md:**

- `capture-pane` reads the visible screen only — scrollback went with PR 8.
  `-S -` and negative line numbers are refused rather than clamped.
- `pane_current_command` is the pane's own process, not its foreground job.
- `list-sessions` as a *command* describes the session it runs in; `wv ls`
  lists them all. A session server does not know about its neighbours.
- ~~`rename-session` is won't-do: the socket is named after the session.~~
  **Reversed 2026-08-11.** The reasoning was wrong: an established Unix socket
  connection survives its path changing, so renaming moves the file and leaves
  every attached client untouched. Implemented.

# PR 7 — Prefix key, bindings, options, tmux config — **SHIPPED**

What closes the loop on the original question: hand weave a `.tmux.conf` and it
reads it.

**Shipped:** key tables with a `root`/`prefix` split and the prefix state
machine; tmux's default prefix bindings; `bind-key`/`unbind-key`/`list-keys`;
the option registry with `set-option`/`show-options`; and a tmux-syntax config
parser with `source-file`.

**The option registry's three states** are the load-bearing idea. Erroring on
every option weave lacks would make a real config fail on its first line;
ignoring them all would let a config quietly not work. So each option is
**live** (read), **inert** (stored, with a logged reason nothing reads it), or
**unknown** (a typo, refused).

Config files apply TOML first, then `.conf`, so the imperative file wins. A
line that cannot be honoured is logged with its location rather than aborting
the file.

**Won't do, moved out of this PR:** `select-pane -T/-P` (pane titles come from
OSC, styles from the theme). **PR 9:** `set-environment`, binding descriptions
(`-N`), and `display-message` without `-p`.

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
and wheel-scroll, had mouse support been in scope. `history-limit` is accepted as an
option in PR 7 but does nothing.

If it comes back, it wants its own scope document.

# PR 9 — Shell glue and pane movement — **SHIPPED (trimmed)**

**Shipped:** `break-pane` and `join-pane` (deferred here from PR 5),
`kill-pane -a`, `display-message` without `-p` on a status-line message area,
`run-shell [-b]`, `if-shell [-b]`, and `wait-for [-S]` (deferred here from
PR 2).

`wait-for` needed the one thing PR 2 could not do: a request that does not
reply. A waiting caller's reply channel is parked in `App::wait_channels` until
someone signals it, and `wv exec` exempts that one command from its ten-second
reply timeout.

**Trimmed out, and now marked won't-do rather than planned:**

- `set-hook`/`show-hooks`. Hooks want an event vocabulary and a re-entrancy
  story — a `pane-died` hook that kills a pane — that is its own design.
- `pipe-pane`. Streaming a pane's PTY output to a process duplicates what
  `capture-pane` already gives a polling script.
- `set-environment`, `bind-key -N` descriptions, `list-* -f` filters,
  `capture-pane -e/-J`, `send-keys -H`.

None of these blocked anything: the scripting and config story is complete
without them.

# PR 10 — Multi-client attach — **SHIPPED**

Attaching no longer evicts. `App` holds a `Vec<AttachedClient>`, each with its
own `front` surface and `DiffRenderer`, because a frame is a delta against what
*that* terminal has already seen. The composed frame is shared; the deltas
cannot be. The single-client path could swap front and back buffers; with
several, the price of correct per-client deltas is a copy each.

The session renders at the smallest attached terminal, renegotiated whenever a
client joins or leaves.

**Shipped:** N clients, smallest-wins sizing, `detach-client [-t] [-a]`,
`refresh-client`, `wv attach -d`. `ExitReason::TakenOver` is no longer sent;
the variant stays so a client can still explain an older server's message.

**Won't do:** `switch-client` (a weave server hosts exactly one session, so
there is nowhere to switch to).

**Known gap:** client ids are connection ids, handed out in arrival order, and
`wv exec` connections consume them too — so they are not predictable from a
script, and there is no `list-clients` to discover them. The no-target form,
which detaches everyone, is the reliable one. A `list-clients` with a stable
per-client name would fix this and is the obvious follow-up.

# PR 11 — Mouse — **DROPPED**

Removed on 2026-08-11: not needed. weave is keyboard-driven, and the parts of
mouse support that would matter most — wheel-scroll — need the scrollback that
went with PR 8, so what is left is click-to-focus and drag-to-resize.

`resize-pane -M` and the `mouse` option stay accepted-but-inert, with the
option registry saying why.

## Ordering summary

```
PR 1 (command model) ──┬── PR 2 (req/resp) ──┬── PR 3 (spawn + send-keys)   ← scripts work
                       │                     └── PR 6 (list/capture/formats) ← harness works
                       ├── PR 4 (windows)  ────── PR 5 (sizing/zoom)
                       └── PR 7 (prefix/bind/options)  [needs 1; wants 6 for formats]
                                   PR 9 (shell glue), PR 10 (multi-client)
                                   — independent of each other
                                   PR 8 (copy-mode), PR 11 (mouse) — dropped
```

## Cross-cutting requirements for every PR

- `#![forbid(unsafe_code)]` holds; clippy pedantic clean.
- Protocol changes bump the version field introduced in PR 1.
- Each PR updates the parity matrix doc and adds to a table-driven conformance suite:
  a script's tmux transcript and its `wv` transcript must produce the same final
  `list-panes -F` output.
- README "tmux users" section updated incrementally, not at the end.
