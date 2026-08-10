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
| `:name` window name | yes | Automatic from the pane title, or set by `rename-window` |
| `@N` window id | no | Windows are fixed slots, so an index already is a stable id — use `:N` |
| `session:` | partial | Accepted and checked; a foreign session is refused, not forwarded (PR 6) |

## Commands

| tmux | weave alias | Status | Notes |
|---|---|---|---|
| `split-window [-hv] [-t]` | `split-h`, `split-v` | yes | |
| `split-window -p/-l` | — | yes | `-l 30%` is accepted as `-p 30` |
| `split-window -b/-f` | — | no | Splits always place the new pane second |
| `split-window -c` | — | yes | |
| `split-window -d` | — | yes | |
| `split-window <command>` | — | yes | Trailing words are the command; `--` forces it |
| `send-keys [-l] [-t]` | — | yes | Key names, or literal text when not one |
| `send-keys -H` | — | PR 9 | |
| `send-keys -R/-M/-X` | — | no | Copy mode is out of scope |
| `respawn-pane [-k] [-c]` | — | yes | Keeps the pane's `%N` and its place in the layout |
| `select-pane -LRUD` | `focus-left`… | yes | Geometric, walks the layout tree |
| `select-pane -t` | — | yes | |
| `select-pane -l` | — | yes | |
| `select-pane -Z` | — | PR 7 | Use `resize-pane -Z` |
| `select-pane -T/-P` | — | PR 7 | Titles and styles |
| `next-layout` | — | no | |
| `select-window -t/-n/-p/-l` | `workspace-1`…`workspace-9` | yes | Fails on a missing window; the alias creates one |
| `next-window`, `previous-window`, `last-window` | — | yes | |
| `new-window [-d] [-n] [-c] [-t] [cmd]` | — | yes | Takes the lowest free window unless `-t` says otherwise |
| `kill-window [-t]` | — | yes | |
| `rename-window [-t] <name>` | — | yes | Pins the name against automatic renaming |
| `move-window`, `swap-window`, renumbering | — | no | Windows are fixed slots |
| `kill-pane -t` | `close` | yes | |
| `kill-pane -a` | — | PR 9 | |
| `detach-client` | `detach` | yes | `-t`/`-a` need multi-client (PR 10) |
| `kill-session` | `quit` | yes | |
| `resize-pane -L/-R/-U/-D [n]` | — | yes | Moves a border; which side the pane is on decides if it grows |
| `resize-pane -x/-y` | — | yes | |
| `resize-pane -Z` | — | yes | Zoom; the layout tree is untouched so unzoom animates back |
| `resize-pane -M` | — | PR 11 | Mouse |
| `swap-pane [-U/-D/-s/-t/-d]` | — | yes | Both panes must be in one window |
| `rotate-window [-U/-D]` | — | yes | |
| `select-layout` | — | yes | `even-horizontal`, `even-vertical`, `main-vertical`, `main-horizontal`, `tiled` |
| `select-layout -p/-o/-E`, layout strings | — | no | weave keeps no layout history |
| `break-pane`, `join-pane` | — | PR 9 | Moving panes between windows |
| `list-panes [-a] [-t] [-F]` | — | yes | |
| `list-windows [-t] [-F]` | — | yes | |
| `list-sessions [-F]` | `wv ls` | partial | The command describes the session it runs in; `wv ls` lists them all |
| `list-*  -f` filters | — | PR 9 | |
| `display-message -p [-F]` | — | yes | Expands formats |
| `display-message` without `-p` | — | PR 7 | Needs a status line message area |
| `capture-pane [-p] [-t] [-S n] [-E n]` | — | yes | Visible screen only |
| `capture-pane -S -` / negative lines | — | no | Needs scrollback |
| `capture-pane -e/-C/-J/-b/-a` | — | PR 9 / no | Escapes and joining are PR 9; buffers went with copy mode |
| `has-session [-t]` | `wv has-session [name]` | yes | Exit status is the answer, as in tmux |
| `kill-server` | `wv kill-server` | yes | |
| `rename-session` | — | no | The socket is named after the session; renaming would break every attached client |
| `bind-key`, `unbind-key`, prefix key, `set-option`, `source-file` | TOML config | PR 7 | |
| `copy-mode`, buffers, scrollback, search | — | no | Dropped: a phase of its own, not a PR |
| `run-shell`, `if-shell`, `set-hook`, `pipe-pane` | — | PR 9 | |
| `wait-for` | — | PR 9 | Moved from PR 2: it needs `run-shell -b` to be useful |
| `switch-client`, `attach -d`, multiple clients per session | — | PR 10 | One client at a time today |
| Mouse support | — | PR 11 | |
| Control mode (`-CC`), TPM/plugins, tmux wire compatibility | — | no | Explicit non-goals |

Scrollback and copy-mode are **not planned**. `capture-pane` will read the
visible screen only, the mouse wheel is forwarded to the pane rather than
scrolling weave's own history, and `history-limit` is accepted but inert.

## Windows are nine named slots

weave has nine windows, numbered 1–9, addressed by `Alt+1`–`Alt+9` and `-t :N`.
They are fixed slots rather than tmux's dynamic list, which is a deliberate
divergence: `Alt+N` reaching window N is worth more here than an unbounded
window count, and it keeps indices stable for the life of a session.

What follows from that:

- A window's **index is its identity**. There is no separate `@id` space, and
  no `move-window` or renumbering, because nothing ever shifts.
- **Names work as they do in tmux.** A window with no name takes it from the
  focused pane's OSC title, so a window running `vim` labels itself `vim`.
  `rename-window` pins the name and stops it following the title. Either way
  `-t :build` finds it, and the status bar shows `3:build`.
- `new-window` takes the **lowest-numbered free window**, or the one `-t`
  names if it is free. With all nine in use it fails and says so.
- `select-window` **fails** on a window that does not exist, as in tmux. The
  `workspace-N` aliases behind `Alt+N` create one instead — that is how weave
  has always behaved and the keybindings keep it.

## Format strings

`#{name}`, the `#S`/`#W`/`#I`/`#P`/`#D`/`#T`/`#F` shorthands, `#{?flag,then,else}`
conditionals and `##` for a literal hash. Variables:

| Scope | Variables |
|---|---|
| Session | `session_name`, `session_windows`, `session_attached` |
| Window | `window_index`, `window_name`, `window_panes`, `window_active`, `window_zoomed_flag`, `window_flags` |
| Pane | `pane_id`, `pane_index`, `pane_title`, `pane_width`, `pane_height`, `pane_active`, `pane_dead`, `pane_current_path`, `pane_current_command` |

Format **arithmetic, comparisons and substitution** (`#{==:..}`, `#{e|+|:..}`,
`#{s/../../:..}`) are rejected with an error rather than expanded to nothing —
a silently empty field is worse than a failed command.

`pane_current_command` reports the **pane's own process**, not the foreground
job inside it. A pane whose shell is running vim reports the shell; a pane
spawned as `split-window npm run dev` reports `npm`.

## Reading a pane back out

```console
$ wv exec capture-pane -t build.1 -p
```

Returns the pane's visible screen as plain text, trailing blank lines trimmed.
`-S`/`-E` take zero-based line numbers within that screen. There is no
scrollback, so `-S -` and negative line numbers are refused rather than quietly
returning less than was asked for.

## Resizing moves a border

`resize-pane -L` does not always make a pane wider. It moves the pane's nearest
vertical border left, so a pane on the left of a split shrinks and one on the
right grows — the same as tmux. `-x`/`-y` set an absolute size instead.

Every reshaping command animates: resize, zoom, swap, rotate and
`select-layout` all tween from the old geometry to the new one rather than
snapping. Zoom leaves the layout tree untouched and simply stretches the zoomed
pane over the window, so unzooming animates back to the exact previous layout
with nothing stored.

A pane can only be squeezed to 5% of its split before the border stops moving,
and resizing while a pane is zoomed is refused rather than silently ignored.

## Panes inherit where you are

A new pane starts in the focused pane's current directory, read from
`/proc/<pid>/cwd`, so `split-window` opens where you were working rather than
where the session was started. `-c` overrides it. If the directory cannot be
read the pane falls back to the session server's own directory.

## Command results

`wv exec` runs one command and reports what it produced:

- the command's output on **stdout**
- its failure message on **stderr**, with exit status **1**
- exit status **0** when it ran

```console
$ wv exec display-message -p hello
hello
$ echo $?
0
$ wv exec kill-pane -t %99
no pane `%99`
$ echo $?
1
```

That covers malformed commands, targets that resolve to nothing, and a session
that cannot be reached. A command that fails never takes the session down: the
request completes, carrying the error back to the caller.

If the session does not answer within 10 seconds, `wv exec` gives up and fails
rather than hanging the script.

## Unsupported flags are loud

A tmux flag that is planned but not implemented is rejected by name, with the
PR that brings it:

```console
$ wv exec split-window -h -p 30
`split-window -p` is not supported yet (PR 5: pane sizing)
```

Nothing is silently ignored, so a ported script either does what it says or
tells you exactly what is missing.
