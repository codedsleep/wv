# SCOPE — Deep tmux integration (script-driven layout)

> Companion to [SCOPE.md](./SCOPE.md). This adds a sub-feature on top of **Phase 4 — tmux backend** (already complete). It is sequenced as **Phase 4.5** in the master plan; numbering inside this doc is local (A.1, B.1, …) and cross-references master tasks where relevant (e.g. _depends on master 4.1_).

---

## Overview

**Goal.** Let external `tmux` commands (and shell scripts that issue them) drive a running `wv --backend tmux` instance's layout, panes, and workspaces — with weave reconciling its internal state and animating to the new layout, instead of weave being the only writer.

**Why.** Today, weave owns the tmux session lifecycle: `tmux -CC new-session -s weave-<uid> -d` (master 4.3, `SCOPE.md:351`) and only weave's own command grammar mutates layout. External `tmux split-window -t weave-<uid>` does emit a `%layout-change` notification (the parser already handles it — master 4.1) but the payload is captured as opaque `raw: String` (`src/backend/tmux/parser.rs:10,136-141`) and never reconciled into the BSP tree. The user wants scripts like `~/Documents/scripts/tmux.sh` to "just work" against weave.

**Conceptual mapping (locked).**

| tmux                    | weave                                                  |
|-------------------------|--------------------------------------------------------|
| session                 | one `wv` instance                                      |
| window (1..9)           | workspace (0..8 internally, `WORKSPACE_COUNT=9`)       |
| window (10..)           | overflow — addressable by name via `wv ls`, no digit   |
| pane (`%N`)             | `PaneId` (already bidirectional via `BiMap`, master 4.3)|
| layout string           | BSP tree (with normalization for non-BSP)              |

**Authority model (locked).** Tmux session is **source of truth**. Internal `Command::SplitH` etc. mutate by sending tmux commands and waiting for the resulting `%layout-change` — the same path external scripts take. This collapses two write paths into one.

**Out of scope for this feature.**
- Native backend gets no script support (would need a separate `wv` unix-socket CLI — tracked separately, not here).
- tmux copy-mode, hooks, scripts, `source-file` — not supported externally.
- Non-BSP layouts (`main-horizontal`, `tiled`, `even-vertical` produced by `select-layout`) — accepted but normalized; round-trip not guaranteed (see E.1).

**Non-goals.**
- Backwards-compat shims for old `weave-<uid>` sessions. Old sessions get auto-migrated to whatever name was last attached.
- Multi-session-per-instance. One `wv` still ↔ one tmux session.

---

## Architecture

### New / extended modules

```
src/backend/tmux/
  mod.rs             # (extend) re-exports for layout module
  parser.rs          # (extend) decode LayoutChange.raw, WindowAdd/Close window-id
  process.rs         # (extend) ingestion path for externally-spawned panes
  layout.rs          # (new)    tmux layout-string parser → LayoutAst
  reconcile.rs       # (new)    LayoutAst ↔ weave BSP tree diff + apply
  windows.rs         # (new)    tmux window-index ↔ weave workspace mapping

src/app.rs           # (extend) BackendEvent variants for ExternalLayoutChange,
                     #          ExternalWindowAdd, ExternalWindowClose
src/main.rs          # (extend) --session <name>, --bare, --no-attach flags
src/command.rs       # (extend) internal commands route through tmux send path
```

### Data flow

```
external script ──┐
                  ├──► tmux server ──► %layout-change ──► parser.rs
weave keybind ────┘                                            │
                                                               ▼
                                              layout.rs (decode layout string)
                                                               │
                                                               ▼
                                         reconcile.rs (LayoutAst → BSP diff)
                                                               │
                                                               ▼
                                              app.rs (apply + animate tweens)
```

The single inbound path means animations, focus rules, and pane spawn/death are uniform whether the change originated inside or outside weave.

---

## Phase A — Foundation: named sessions + layout string decode

**Goal.** A stable, scriptable session name and a working tmux-layout parser. No reconciliation yet — just decode and unit-test.

- [x] **A.1 `--session <name>` flag** — *Codex* — `src/main.rs`, `src/backend/tmux/process.rs`
  - Add `--session <name>` to `wv` CLI. Default: keep `weave-<uid>` for backwards compat.
  - Validation: alphanumeric + `-` + `_` only, max 64 chars (port `validate_session_name` from `/home/zzz/Documents/react/tilerm/src-tauri/src/tmux/manager.rs:495-509` — prevents command injection through shell-quoting).
  - On collision with an existing session: error with `wv attach <name>` hint.
  - `wv ls` filter loosens to "any session weave created" via a marker `@weave-instance` tmux user option set at spawn (`set -t <name> -g @weave-instance 1`).
  - **Accept:** `wv --session foo` starts; `wv ls` lists it even though name lacks `weave-` prefix; `wv --session "foo;rm -rf /"` rejects with a typed error (no shell-out); `wv --session foo` while one runs errors clearly.

- [x] **A.2 `--bare` / `--no-attach` mode** — *Codex* — `src/main.rs`, `src/app.rs`
  - `wv --session foo --bare` creates the tmux session, sets `@weave-instance`, disables tmux chrome, **does not spawn a default pane**, and exits — leaving the session ready for a script to populate.
  - `wv attach foo` then attaches and reconciles whatever the script built.
  - **Accept:** `wv --session demo --bare && tmux split-window -t demo && tmux split-window -h -t demo && wv attach demo` shows the script-built layout.

- [x] **A.3 Tmux layout-string parser** — *Codex* — `src/backend/tmux/layout.rs` (new)
  - Parse the format documented in tmux source `layout-custom.c`: `<checksum>,<width>x<height>,<x>,<y>[{...}|[...]|,<pane-id>]`.
  - Output: `pub enum LayoutAst { Leaf { pane_id: u64, rect: Rect }, Horizontal(Vec<LayoutAst>), Vertical(Vec<LayoutAst>) }` (tmux uses `{}` for horizontal-split groups, `[]` for vertical).
  - Verify checksum on parse; reject malformed strings with a typed error (do not panic — feeds into the streaming parser).
  - **Accept:** unit tests for: single pane; one h-split; one v-split; nested 4-pane grid; pathological inputs (truncated, bad checksum, missing pane-id) return `Err`, not panic.

- [x] **A.4 LayoutChange / WindowAdd / WindowClose payload decode** — *Codex* — `src/backend/tmux/parser.rs`
  - `LayoutChange { window_id: u64, layout: LayoutAst, raw: String }` — keep `raw` for logging/debug, add decoded fields.
  - `WindowAdd { window_id: u64, name: Option<String> }`, `WindowClose { window_id: u64 }` — promote from `Option<u64>` to required, with a parse-error variant for malformed input.
  - Update all existing call sites + tests.
  - **Accept:** existing parser proptests (master 4.2) still pass; new tests cover decoded variants.

- [x] **A.5 Property tests for layout parser** — *Codex* — `src/backend/tmux/layout.rs`
  - proptest: round-trip — generate a random BSP `LayoutAst`, render to tmux's string format, parse back, assert equal.
  - proptest: any byte string into the parser must not panic.
  - **Accept:** 1024+ proptest cases green.

**Phase A acceptance:** `wv --session <name>` and `--bare` work. Layout strings decode into `LayoutAst`. Nothing yet reconciles them into the BSP tree (so external splits are still ignored by the renderer — but observable in `WEAVE_LOG=debug`).

---

## Phase B — Reconciliation: tmux state → weave BSP tree

**Goal.** External `tmux split-window`, `select-layout`, `swap-pane`, `kill-pane` are reflected in weave's BSP tree and animated like internal commands.

- [x] **B.1 LayoutAst → weave BSP tree diff** — *Codex* — `src/backend/tmux/reconcile.rs` (new)
  - Pure function: `fn diff(old: &layout::tree::Node, new: &LayoutAst) -> Vec<LayoutDelta>` where `LayoutDelta` ∈ { `AddPane`, `RemovePane`, `SplitInternal`, `MergeInternal`, `ResizeRatio`, `SwapLeaves` }.
  - Match leaves by `pane_id` (stable across layout changes); shape-match internals by structural index.
  - **Accept:** unit tests for: pure split (1→2), pure resize, pane death (2→1), swap (2 leaves trade positions), full rebuild (no leaf matches).

- [x] **B.2 Non-BSP normalization** — *Codex* — `src/backend/tmux/reconcile.rs`
  - tmux's `select-layout main-horizontal | tiled | even-vertical` can produce layouts that are not pure binary trees (one h-split with 4 children).
  - Normalize: collapse N-ary splits into right-leaning BSP — `H(a,b,c,d) → H(a, H(b, H(c, d)))`. Ratios preserved by carrying cumulative widths.
  - **Accept:** unit test: 4-pane `tiled` layout normalizes deterministically; pane IDs preserved; ratios within ±1 cell of tmux's render.

- [ ] **B.3 Apply LayoutDelta + animate** — *Codex* — `src/app.rs`, `src/backend/tmux/reconcile.rs`
  - New `App` method `apply_external_layout_change(window_id, LayoutAst) -> Result<()>` that:
    1. Looks up workspace by window_id (see C.1).
    2. Diffs against current BSP for that workspace.
    3. Spawns tweens for `SplitInternal`/`MergeInternal`/`ResizeRatio` using existing tween machinery (Phase 3, master 3.x).
    4. For `AddPane`: backend-side, the pane already exists in tmux; weave just allocates a `PaneId`, registers it in the BiMap, and starts an open animation from a zero-size rect at the new leaf's target.
    5. For `RemovePane`: close animation; remove from BiMap.
  - Block: if a tween is in flight from a *prior* external change for the same window, snap it before applying (consistent with master 4.5 detach behavior).
  - **Accept:** integration test — start `wv --session t1 --bare`; from another shell run `tmux split-window -h -t t1`; attach with `wv attach t1`; observe two panes with the open animation completing.

- [ ] **B.4 Ingest externally-spawned pane IDs** — *Codex* — `src/backend/tmux/process.rs`
  - Today `spawn` is the only path that registers `(tmux %N ↔ weave PaneId)` in the BiMap. External `split-window` produces a new `%N` that arrives via `%layout-change` with no prior `spawn` call.
  - On unknown `%N` in a decoded layout: query `tmux display-message -p -t %N '#{pane_pid}'` to confirm the pane is real, allocate a fresh `PaneId`, register the mapping. Then subscribe to its `%output` stream (already automatic in `-CC` mode — verify with a test).
  - **Accept:** integration test — externally spawned pane echoes text, weave renders it without `wv` ever calling `PaneBackend::spawn` for that pane.

- [ ] **B.5 Full re-attach reconciliation** — *Codex* — `src/app.rs`
  - On `wv attach <name>`: before entering the event loop, run `tmux list-windows -t <name> -F '#{window_id}\t#{window_index}\t#{window_layout}'` and `tmux list-panes -s -t <name> -F '#{pane_id}\t#{pane_pid}'`.
  - Build initial BSP forest from the parsed layouts; populate BiMap; spawn no open-animations on initial attach (jump-cut to current state).
  - **Accept:** detach mid-session with 5 panes across 3 workspaces; modify externally; `wv attach` shows the new state correctly without animations on first frame.

- [ ] **B.6 Conflict resolution policy** — *Codex* — `src/app.rs`
  - If user issues internal `Command::SplitH` while a `%layout-change` from a prior external command is mid-animation, the internal command is queued (not applied) until the animation completes. Reason: tmux is the source of truth; sending another `split-window` mid-flight risks ordering bugs.
  - User-visible: brief status-bar indicator (`⟳` glyph) when external changes are in flight.
  - **Accept:** integration test — fire internal split during external resize; final state is consistent with tmux's `list-panes`.

**Phase B acceptance:** External tmux commands fully drive weave. Internal commands and external commands produce identical final BSP state for equivalent operations. Animations work in both directions.

---

## Phase C — Windows ↔ Workspaces

**Goal.** tmux window switching (`tmux select-window -t :2`) is synchronized with weave workspace switching (`Alt+2`), and vice versa.

- [ ] **C.1 Window-index ↔ workspace-index mapping** — *Codex* — `src/backend/tmux/windows.rs` (new)
  - Owns a `HashMap<u64 /* tmux window-id */, usize /* weave workspace 0..8 */>` plus the inverse.
  - On `%window-add`: if tmux's `window_index` is 1..=9, claim that weave workspace slot; else mark "overflow" (out-of-band, listed in `wv ls --windows` but no digit binding).
  - On `%window-close`: free the slot; if it was the current workspace, fall back to lowest non-empty workspace.
  - **Accept:** unit tests for the mapping table; integration test — `tmux new-window -t <session>:2` populates workspace 1 (zero-indexed).

- [ ] **C.2 `SwitchWorkspace` → `select-window`** — *Codex* — `src/app.rs:394` (extend existing `switch_workspace`)
  - When tmux backend active: emit `tmux select-window -t <session>:<index+1>` instead of in-memory toggle alone; the workspace switch completes when the resulting `%session-changed` / `%window-changed` arrives.
  - Native backend keeps its existing path unchanged.
  - **Accept:** `Alt+3` and `tmux select-window -t :3` produce identical user-observable state; external `prev-window` cycles through weave workspaces.

- [ ] **C.3 `%session-changed` / window-active tracking** — *Codex* — `src/backend/tmux/parser.rs`, `src/app.rs`
  - Parse `%session-changed $N <window-id>` to extract the active window-id (today only `raw` is captured).
  - On change: update `App.current_workspace` from the mapping. If the new window is unmapped (overflow), display a status-bar warning and stay on the previous workspace visually.
  - **Accept:** external `select-window` flips weave's visible workspace.

- [ ] **C.4 Overflow handling (windows 10+)** — *Codex* — `src/backend/tmux/windows.rs`
  - Overflow windows are visible via `wv ls --windows` (new flag) and addressable via a new `:goto-window <name>` command (Prefix `g` keybind).
  - **Accept:** create 11 windows externally; first 9 map to workspaces 0..8; windows 10–11 reachable via `:goto-window`.

**Phase C acceptance:** Workspaces and tmux windows are first-class synonyms. The user can use either control surface; both stay in sync.

---

## Phase D — Script ergonomics + docs

**Goal.** A user can port a `tmux.sh`-style script to weave with minimal changes and have it work reliably.

- [ ] **D.1 Document the safe-command contract** — *Claude* — `README.md`, new `docs/tmux-scripting.md`
  - **Safe** (reconciled): `new-window`, `kill-window`, `select-window`, `rename-window`, `split-window`, `kill-pane`, `select-pane`, `swap-pane`, `resize-pane`, `select-layout`, `send-keys`, `respawn-pane`.
  - **Unsafe / undefined**: `set-hook`, `bind-key`, `source-file`, `copy-mode`, custom user-options outside the `@weave-*` namespace, `set -g status on` (weave forces it off).
  - **Forbidden**: renaming or killing the session weave is attached to.
  - **Accept:** doc exists; each safe command has a one-liner example.

- [ ] **D.2 Example bootstrap script** — *Claude* — `docs/examples/weave-bootstrap.sh`
  - A port of the user's `~/Documents/scripts/tmux.sh` style: creates 4 named windows (Code / Terminal / Agents1 / Agents2), each with appropriate splits.
  - Uses `wv --session main --bare &` then drives the layout via `tmux` commands, then `exec wv attach main` to take over.
  - **Accept:** running the script produces a weave instance with the documented layout.

- [ ] **D.3 `wv exec` passthrough** — *Codex* — `src/main.rs`
  - `wv exec <tmux-args...>` resolves the most-recent (or explicitly-named via `--session`) weave session and runs the tmux subcommand against it — saves the user from looking up the session name in scripts.
  - **Accept:** `wv exec split-window -h` splits the current workspace's focused pane.

**Phase D acceptance:** A new user can read the doc, write a script, and have it drive weave. The existing dotfile-style `tmux.sh` ports in <10 lines of diff.

---

## Phase E — Tests + release gating

- [ ] **E.1 Backend parity test extension** — *Codex* — `tests/backend_parity.rs` (extend master 4.7)
  - For tmux backend: run the parity scenario both via internal `Command::*` and via external `tmux` subcommands; assert composed `Surface` and BSP tree match.
  - **Accept:** parity holds.

- [ ] **E.2 Script-driven integration test** — *Codex* — `tests/script_driven.rs` (new)
  - End-to-end: spawn `wv --session test --bare`, run a deterministic sequence of `tmux` commands as a subprocess, attach with `wv attach test`, sample frames at fixed intervals, assert final frame matches a golden snapshot.
  - **Accept:** snapshot stable across local + CI runs.

- [ ] **E.3 Animation budget under script load** — *Codex* — `benches/script_storm.rs` (new)
  - Bench: fire 50 `tmux split-window` commands in 2 seconds; assert weave maintains the 6.25ms frame budget (master `Locked decisions`).
  - **Accept:** p99 frame time ≤ 6.25ms on the reference machine.

- [ ] **E.4 README + SCOPE.md update** — *Claude* — `README.md`, `.planning/SCOPE.md`
  - README: new section "Scripting weave with tmux", linking to `docs/tmux-scripting.md`.
  - `SCOPE.md`: add a `Phase 4.5 — Script integration` block that references this file as its detail.
  - **Accept:** docs build clean; cross-links resolve.

- [ ] **E.99 commit & tag** — *Kimi* — per-task commits, then `git tag phase-4.5`

**Phase E acceptance:** Tests green. Frame budget held. README and SCOPE.md cross-reference this doc. Tag `phase-4.5` exists.

---

## Dependencies & sequencing

```
A.1 ─┐
A.2 ─┼─► A.3 ─► A.4 ─► A.5 ─┐
     │                       ├─► B.1 ─► B.2 ─► B.3 ─┬─► B.4 ─► B.5 ─► B.6 ─┐
     │                       │                      │                       │
     │                       │                      └─► C.1 ─► C.2 ─► C.3 ─► C.4 ─┤
     │                       │                                                    │
     │                       └────────────────────────────────────────────────────┤
     │                                                                            ▼
     └──► D.1 (can start once A.1/A.2 land) ─► D.2 ─► D.3 ──────────────► E.1..E.4
```

- **A.1–A.5** can land independently of B/C/D.
- **B** requires all of A.
- **C** requires B.1 (delta diff) and A.4 (window-id decode); does not require B.4/B.5.
- **D** can start with A.1 + A.2 in hand; D.3 needs nothing else.
- **E** is the gating phase — runs after everything else.

## Risks & open questions

1. **tmux layout checksums** — older tmux versions (<2.2) had a different checksum algorithm. Lock to tmux ≥ 3.3 (already the realistic minimum for `-CC` reliability); add a startup version check.
2. **Non-BSP layouts** — `select-layout tiled` on 5 panes produces a layout weave can render but cannot exactly round-trip if the user then internally splits. Accepted; documented in D.1.
3. **`send-keys` races** — external `send-keys -l` and weave's own write path could interleave on the same pane. Tmux serializes per-pane writes, so this should be safe, but verify with E.2.
4. **Detach during script execution** — if the user runs `wv attach foo` while a script is still issuing tmux commands, partial states may flicker. Mitigation: B.5 jump-cuts on attach, then animations resume for *new* changes only.
5. **`wv exec` ambiguity** when multiple weave sessions exist — error out and require explicit `--session`.

## Lessons from Tilerm

The Tilerm project (`/home/zzz/Documents/react/tilerm/src-tauri/src/tmux/`) ships a working tmux-driven tiling terminal using a *different* architecture than weave's current `-CC` model. Concrete patterns worth lifting verbatim, plus one architectural alternative to consider.

### Pattern transplants (add to Phase A)

- [x] **A.6 Apply Tilerm-proven session options** — *Codex* — `src/backend/tmux/process.rs`
  - On session create, set: `prefix None`, `prefix2 None`, `allow-passthrough on`, `aggressive-resize on` (mirrors `apply_session_config` at `manager.rs:207-220`).
  - `prefix None` / `prefix2 None` — disables tmux's own prefix key entirely so all keystrokes pass through to weave's input layer. **Critical** for the script-driven model: external scripts that `send-keys` won't accidentally trigger tmux bindings inside weave's view.
  - `allow-passthrough on` — lets DCS / OSC sequences (OSC 52 clipboard, OSC 0/2 titles already used by master 5.2) reach the outer terminal. Without this, pane-title updates from inside `wv` workspaces break.
  - `aggressive-resize on` — when multiple clients (e.g. `wv attach` + an external `tmux attach`) hold different dimensions, tmux resizes the window to the smaller current client rather than the smallest historical client. Avoids "frozen at smallest size" bug.
  - **Accept:** integration test — connect with `tmux attach-session -t <name>` from a 200×60 terminal while `wv` is attached at 80×24; resizing the tmux client doesn't shrink weave's view permanently.

- [x] **A.7 Stale-session cleanup on startup** — *Codex* — `src/backend/tmux/process.rs`, `src/main.rs`
  - Port `cleanup_orphaned_sessions` (`manager.rs:370-390`): on `wv` startup with auto-named `weave-<uid>`, kill any pre-existing `weave-*` session that lacks the `@weave-instance` marker option, since those are orphans from crashes.
  - Named sessions (`--session foo`) are exempt — user-named state is preserved across crashes.
  - Best-effort: if a kill fails (server gone), proceed without error.
  - **Accept:** kill `-9` a running `wv`; subsequent `wv` startup cleans up the orphaned auto-named session and reports the count.

- [x] **A.8 CWD discovery via `pane_current_path`** — *Codex* — `src/backend/tmux/process.rs`
  - When weave needs a pane's CWD (e.g. for "split with same CWD" semantics), query `tmux display-message -p -t %N '#{pane_current_path}'` instead of reading `/proc/<pid>/cwd`.
  - Reason from `manager.rs:423-436`: under tmux, `/proc/<pid>/cwd` returns the `tmux` process's cwd, not the shell's. `pane_current_path` is what tmux itself tracks and is authoritative.
  - **Accept:** `cd /tmp` in a pane, internal `Command::SplitH`-with-same-cwd opens at `/tmp`, not at weave's launch directory.

- [x] **A.9 Pipe-delimited `-F` format parsing** — *Codex* — `src/backend/tmux/process.rs`
  - When weave issues queries like `list-windows` / `list-panes` (used in B.5 re-attach reconciliation), use Tilerm's `#{a}|#{b}|#{c}` format-string pattern (`manager.rs:222-256`).
  - Cheap to parse, robust against window names containing spaces, no shell quoting issues.
  - Centralize the parser in a small `format::parse_rows(output: &str, expected_fields: usize)` helper so all `-F` callers go through it.
  - **Accept:** parse a window name containing `|` correctly using `splitn(expected_fields, '|')` (matches `manager.rs:239`).

- [x] **A.10 Best-effort shutdown cleanup** — *Codex* — `src/app.rs`, `src/backend/tmux/process.rs`
  - On `wv` quit (clean or signal): attempt to kill the session (auto-named only) and ignore "can't find session" / "no server running" errors (port `kill_tab_session` logic at `manager.rs:160-172`).
  - For `--session <name>`: leave the session alive (user explicitly named it = wants persistence).
  - **Accept:** Ctrl+C an auto-named `wv`; `tmux ls` shows no orphan; SIGKILL is caught by A.7 cleanup on next launch.

### Architectural alternative: grouped-session model (consider for v2)

Tilerm avoids `-CC` control mode entirely. Its model:

1. **Master session per workspace** — one `weave-<workspace-id>` tmux session owns the windows.
2. **Per-pane grouped sessions** — each pane spawns a *grouped* session via `tmux new-session -t <master> -s <pane-uuid>`. Grouped sessions share windows but have independent active-window pointers.
3. **Each pane's PTY runs `tmux attach-session -t <pane-uuid>`** — the PTY *is* a tmux client. Weave doesn't parse `%output` streams; it just owns the PTY bytes.
4. **No layout-string parsing required** — weave owns the geometry/animation layer; tmux owns the persistence layer; they don't fight over BSP shape because tmux's own layout is irrelevant (one window per grouped session view).

This trades the `-CC` parser (≈ 580 lines in `parser.rs`) for a much simpler subprocess-shelling layer (Tilerm's `manager.rs` is 550 lines and covers *more* surface area). Trade-offs:

| dimension                       | current weave (`-CC` control mode)                          | tilerm-style (grouped sessions + PTYs)                          |
|---------------------------------|-------------------------------------------------------------|------------------------------------------------------------------|
| protocol parsing                | own `%output`/`%layout-change`/etc. parser                  | none — tmux speaks PTY to each pane natively                     |
| structured events               | `%window-add`, `%pane-died`, `%layout-change` for free      | must poll `list-*` or use `set-hook` for change detection        |
| script-driven layout            | reconcile `%layout-change` (Phase B work)                   | re-poll `list-windows` on tick or hook; simpler diff             |
| animations                      | weave owns geometry; backend just owns PTY contents         | same — weave owns geometry; cleaner separation                   |
| latency                         | direct binary stream from tmux                              | one extra `tmux attach` process per pane (overhead per pane)     |
| OSC / clipboard / titles        | requires careful passthrough plumbing                       | `allow-passthrough on` makes it transparent (A.6)                |
| multi-client (external attach)  | hard — `-CC` doesn't compose well with regular clients      | trivial — just another tmux client                               |

**Recommendation:** keep `-CC` as the v1 path (everything in Phase A–E is built on top of it). But if Phase B reconciliation proves painful — especially non-BSP normalization (B.2) and the diff/animate machinery (B.3) — fall back to the grouped-session architecture as a v2 redesign. Treat the `-CC` parser as a debt that we'd cut if reconciliation cost balloons.

> **Open question:** Tilerm caps at tmux ≥ 3.0 (`manager.rs:108-111`); weave currently aims at ≥ 3.3 for `-CC` reliability. If we ever do the grouped-session pivot, the floor drops to 3.0 and unlocks more user installations.

---

## Locked decisions for this feature

- **Source of truth:** tmux session state. Internal commands round-trip through tmux.
- **Mapping:** tmux windows 1..9 ↔ weave workspaces 0..8 (1:1, fixed). Windows 10+ are overflow.
- **Session naming:** user-chosen via `--session`; auto `weave-<uid>` retained as default.
- **Native backend:** intentionally unsupported for script-driven layout in this feature.
- **tmux version floor:** ≥ 3.3.
- **Layout normalization:** N-ary tmux splits → right-leaning BSP (deterministic, not user-configurable in v1).

---

## Quick checklist (flat view)

- [x] A.1 `--session <name>` flag
- [x] A.2 `--bare` / `--no-attach` mode
- [x] A.3 Tmux layout-string parser
- [x] A.4 LayoutChange / WindowAdd / WindowClose payload decode
- [x] A.5 Property tests for layout parser
- [x] A.6 Apply Tilerm-proven session options (`prefix None`, `allow-passthrough on`, `aggressive-resize on`)
- [x] A.7 Stale-session cleanup on startup
- [x] A.8 CWD discovery via `pane_current_path`
- [x] A.9 Pipe-delimited `-F` format parsing
- [x] A.10 Best-effort shutdown cleanup
- [x] B.1 LayoutAst → BSP tree diff
- [x] B.2 Non-BSP normalization
- [ ] B.3 Apply LayoutDelta + animate
- [ ] B.4 Ingest externally-spawned pane IDs
- [ ] B.5 Full re-attach reconciliation
- [ ] B.6 Conflict resolution policy
- [ ] C.1 Window-index ↔ workspace-index mapping
- [ ] C.2 `SwitchWorkspace` → `select-window`
- [ ] C.3 `%session-changed` / window-active tracking
- [ ] C.4 Overflow handling (windows 10+)
- [ ] D.1 Document safe-command contract
- [ ] D.2 Example bootstrap script
- [ ] D.3 `wv exec` passthrough
- [ ] E.1 Backend parity test extension
- [ ] E.2 Script-driven integration test
- [ ] E.3 Animation budget under script load
- [ ] E.4 README + SCOPE.md update
- [ ] E.99 commit & tag `phase-4.5`
