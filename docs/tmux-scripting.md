# Scripting weave with tmux

`wv --backend tmux` is driven by a real tmux session — externally-issued `tmux`
commands are reconciled into weave's BSP tree and animated like internal
commands. This means an existing tmux bootstrap script can drive a `wv`
instance with only minor changes.

The typical pattern:

```sh
# 1. create an empty, weave-marked tmux session and exit
wv --session main --bare --backend tmux

# 2. populate it with plain tmux commands
tmux new-window   -t main:1 -n code
tmux split-window -t main:1 -h
tmux new-window   -t main:2 -n term
# …

# 3. take over the session — animations resume for any further changes
exec wv attach main
```

See [`examples/weave-bootstrap.sh`](./examples/weave-bootstrap.sh) for a working
script.

## Conceptual mapping

| tmux                  | weave                                                |
|-----------------------|------------------------------------------------------|
| session               | one `wv` instance                                    |
| window `1..9`         | workspace `0..8` (`Alt+1` … `Alt+9`)                 |
| window `10..`         | overflow — visible via `wv ls --windows`, no digit   |
| pane (`%N`)           | weave `PaneId` (mapped via internal `BiMap`)         |
| layout string         | BSP tree (with right-leaning normalization)          |

The tmux session is the **source of truth**. Internal weave commands round-trip
through tmux, so external scripts and weave's own keybindings share one write
path.

## Command contract

### Safe — reconciled into weave's state

These commands produce a `%layout-change` / `%window-add` / `%window-close`
notification that weave decodes and animates. Use them freely:

| Command                                            | Effect                                                  |
|----------------------------------------------------|---------------------------------------------------------|
| `tmux new-window -t <session>:<n> -n <name>`       | Adds a new workspace (windows 1..9) or overflow (10+).  |
| `tmux kill-window -t <session>:<n>`                | Closes a workspace; weave falls back to the lowest non-empty one if the current is removed. |
| `tmux select-window -t <session>:<n>`              | Switches workspace; `Alt+<n>` does the same.            |
| `tmux rename-window -t <session>:<n> <new-name>`   | Updates the visible workspace label.                    |
| `tmux split-window  -t <session>:<n>.<pane> [-h]`  | Splits the targeted pane; animates the open.            |
| `tmux kill-pane     -t <session>:<n>.<pane>`       | Removes a pane; animates the close.                     |
| `tmux select-pane   -t <session>:<n>.<pane>`       | Moves focus inside a workspace.                         |
| `tmux swap-pane    -s <src> -t <dst>`              | Swaps two leaves; layout tree is diffed and re-applied. |
| `tmux resize-pane   -t <session>:<n>.<pane> -[LRUD]<n>` | Adjusts split ratio; weave tweens the resize.       |
| `tmux select-layout -t <session>:<n> <layout>`     | Accepted; N-ary tmux layouts (`tiled`, `main-*`, `even-*`) are normalized to right-leaning BSP. Round-trip is not guaranteed. |
| `tmux send-keys     -t <session>:<n>.<pane> '…'`   | Writes to the pane PTY exactly as for any tmux session. |
| `tmux respawn-pane  -t <session>:<n>.<pane>`       | Replaces the pane's process; weave keeps the leaf.      |

### Unsafe / undefined

These either fight weave for control or set state weave depends on. Behaviour
is **not defined** and may change:

- `tmux set-hook …` — weave attaches its own hooks; user hooks can race with
  reconciliation.
- `tmux bind-key …` — irrelevant because weave disables tmux's prefix
  (`prefix None`, `prefix2 None`). Anything you bind here is dead code.
- `tmux source-file …` — same as above; bindings won't fire.
- `tmux copy-mode …` — tmux's copy-mode UI is not compatible with weave's
  renderer. Use a terminal-side selection tool instead.
- Custom user options outside the `@weave-*` namespace — fine to read, avoid
  writing options weave may rely on (`status`, `pane-border-status`,
  `allow-passthrough`, `aggressive-resize`).
- `tmux set -g status on` — weave forces it off; toggling it back on causes
  the inner renderer and tmux to disagree about row count.

### Forbidden

- **Renaming the attached session.** `tmux rename-session -t <weave-session>
  <new>` will detach `wv` with no clean shutdown path.
- **Killing the attached session.** `tmux kill-session -t <weave-session>`
  drops `wv` immediately; auto-named sessions are reaped on next launch
  (see `--session` semantics below) but in-flight state is lost.
- Any command that targets weave's outer terminal (e.g.
  `set-environment WEAVE_*`) — these are reserved.

## Session lifecycle

- **Auto-named sessions** (`wv` without `--session`) use `weave-<8-hex-uid>`
  and are killed on clean shutdown. Crashed instances leave orphans; the next
  auto-named launch reaps them.
- **Named sessions** (`wv --session <name>`) survive crashes and clean exits.
  They are detectable via the `@weave-instance` tmux user option, and listed
  by `wv ls` regardless of whether the name starts with `weave-`.
- **Bare mode** (`wv --session <name> --bare --backend tmux`) creates the
  session, applies the weave option set, **does not spawn a default pane**,
  and exits. Use this to hand a blank weave-flavoured session to a script.
- **Reattach** (`wv attach <name>`) hydrates the BSP forest from
  `tmux list-windows`/`list-panes` on first frame (no animations), then
  animates anything that happens after.

## Convenience: `wv exec`

`wv exec <tmux-args…>` resolves the current weave session (most recently
created weave-marked session, or `--session <name>` to disambiguate) and runs
the supplied tmux subcommand against it. This avoids hard-coding the session
name in scripts:

```sh
wv exec split-window -h
wv exec select-window -t :2
wv --session main exec send-keys 'echo hi' Enter
```

If multiple weave sessions exist and `--session` is not supplied, `wv exec`
errors with the list of candidate names.

## tmux version

Weave requires tmux **3.3 or newer**. Older versions had a different
layout-string checksum algorithm and incomplete `-CC` notifications.

## Troubleshooting

- **`WEAVE_LOG=debug wv attach <name>`** dumps every parsed control-mode
  notification (`%layout-change`, `%window-add`, `%pane-died`, …) into the
  rotating log at `~/.local/state/weave/weave.log`.
- **Layout drift after an external `select-layout tiled`** — expected. tmux's
  N-ary layouts collapse into right-leaning BSPs; if you then internally
  split, the result may differ from a fresh `tiled` render.
- **Pane appears but stays blank.** External `split-window` panes are
  registered lazily on the first `%layout-change`; if the layout payload was
  malformed, weave logs a parse error and skips the change. Look for
  `parse error` in the weave log.
- **`wv exec` says "ambiguous session".** Pass `--session <name>` explicitly,
  or use `wv ls` to pick a target.
