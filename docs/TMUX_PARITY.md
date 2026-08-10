# tmux parity matrix

What `wv` accepts today, what it will accept, and what it never will. Updated
with every parity PR — see [`.planning/SCOPE_TMUX_PARITY.md`](../.planning/SCOPE_TMUX_PARITY.md)
for the plan behind the PR numbers.

Legend: **yes** shipped · **partial** shipped with limits · **PR N** planned · **no** out of scope.

## The one thing to know when porting

`split-window -h` and weave's `split-h` are **opposites**.

tmux names how the panes end up sitting; weave names the axis being divided.

| You write | You get | weave calls it |
|---|---|---|
| `split-window -h` | panes side by side | `Split::Vertical` (divides the width) |
| `split-window -v` | panes stacked | `Split::Horizontal` (divides the height) |
| `split-h` (weave alias) | panes stacked | `Split::Horizontal` |
| `split-v` (weave alias) | panes side by side | `Split::Vertical` |

Both spellings are supported and neither has changed meaning. If you are
porting a `.tmux.conf`, use the tmux spellings and ignore the aliases.

## Targets

`-t session:window.pane`, as in tmux. Every part is optional; an omitted part
means the current one.

| Form | Status | Notes |
|---|---|---|
| `%N` pane id | yes | Stable for the pane's lifetime, never reused |
| `.N` pane index | yes | Zero-based within its window, as tmux's default `pane-base-index` |
| `+` `-` `{next}` `{previous}` | yes | Walks layout order, wrapping |
| `!` `{last}` | yes | Previously focused pane, or previous window |
| `{top}` `{bottom}` `{left}` `{right}` | yes | Extreme pane by geometry |
| `:N` window index | yes | One-based; maps to workspaces 1–9 |
| `{start}` `{end}` | yes | Lowest/highest occupied workspace |
| `@N` window id | PR 4 | Needs real windows |
| `:name` window name | PR 4 | Needs real windows |
| `session:` | partial | Accepted and checked; a foreign session is refused, not forwarded (PR 6) |

## Commands

| tmux | weave alias | Status | Notes |
|---|---|---|---|
| `split-window [-hv] [-t]` | `split-h`, `split-v` | yes | |
| `split-window -p/-l` | — | PR 5 | Sizing; splits are even for now |
| `split-window -c` | — | PR 3 | cwd; new panes inherit the focused pane's cwd already |
| `split-window -d` | — | PR 3 | |
| `split-window <command>` | — | PR 3 | Panes always run `$SHELL` for now |
| `select-pane -LRUD` | `focus-left`… | yes | Geometric, walks the layout tree |
| `select-pane -t` | — | yes | |
| `select-pane -l` | — | yes | |
| `select-pane -Z` | — | PR 5 | Zoom |
| `select-pane -T/-P` | — | PR 7 | Titles and styles |
| `select-window -t/-n/-p/-l` | `workspace-1`…`workspace-9` | yes | Windows are the nine workspaces until PR 4 |
| `kill-pane -t` | `close` | yes | |
| `kill-pane -a` | — | PR 5 | |
| `detach-client` | `detach` | yes | `-t`/`-a` need multi-client (PR 10) |
| `kill-session` | `quit` | yes | |
| `new-window`, `rename-window`, `next-window`, `previous-window`, `kill-window`, `move-window` | — | PR 4 | |
| `resize-pane`, `swap-pane`, `rotate-window`, `break-pane`, `join-pane`, `select-layout` | — | PR 5 | |
| `send-keys`, `respawn-pane` | — | PR 3 | |
| `list-sessions`, `list-windows`, `list-panes`, `-F` formats | `wv ls` (sessions only) | PR 6 | |
| `capture-pane`, `display-message` | — | PR 6 | |
| `has-session`, `rename-session`, `kill-server` | — | PR 6 | |
| `bind-key`, `unbind-key`, prefix key, `set-option`, `source-file` | TOML config | PR 7 | |
| `copy-mode`, buffers, scrollback | — | PR 8 | |
| `run-shell`, `if-shell`, `set-hook`, `pipe-pane`, `wait-for` | — | PR 9 | |
| `switch-client`, `attach -d`, multiple clients per session | — | PR 10 | One client at a time today |
| Mouse support | — | PR 11 | |
| Control mode (`-CC`), TPM/plugins, tmux wire compatibility | — | no | Explicit non-goals |

## Command results

`wv exec` currently reports parse errors and connection failures, and exits
non-zero on them. It does **not** yet report errors raised while the command
runs — a target naming a missing pane is logged by the session and reported to
an attached client, but `wv exec` still exits 0. PR 2 adds the request/response
protocol that fixes this; until then, do not branch a script on `wv exec`'s
exit status for anything but a malformed command.

## Unsupported flags are loud

A tmux flag that is planned but not implemented is rejected by name, with the
PR that brings it:

```console
$ wv exec split-window -h -p 30
`split-window -p` is not supported yet (PR 5: pane sizing)
```

Nothing is silently ignored, so a ported script either does what it says or
tells you exactly what is missing.
