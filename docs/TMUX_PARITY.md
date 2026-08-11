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
| `{left-of}` `{right-of}` `{up-of}` `{down-of}` | yes | Nearest neighbour of the current pane in that direction |
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
| `send-keys -H` | — | no | Hex key arguments |
| `send-keys -R/-M/-X` | — | no | Copy mode is out of scope |
| `respawn-pane [-k] [-c]` | — | yes | Keeps the pane's `%N` and its place in the layout |
| `select-pane -LRUD` | `focus-left`… | yes | Geometric, walks the layout tree |
| `select-pane -t` | — | yes | |
| `select-pane -l` | — | yes | |
| `select-pane -Z` | — | no | Use `resize-pane -Z` |
| `select-pane -T/-P` | — | no | Pane titles come from OSC, and styles from the theme |
| `next-layout` | — | no | |
| `select-window -t/-n/-p/-l` | `workspace-1`…`workspace-9` | yes | Fails on a missing window; the alias creates one |
| `next-window`, `previous-window`, `last-window` | — | yes | |
| `new-window [-d] [-n] [-c] [-t] [cmd]` | — | yes | Takes the lowest free window unless `-t` says otherwise |
| `kill-window [-t]` | — | yes | |
| `rename-window [-t] <name>` | — | yes | Pins the name against automatic renaming |
| `move-window`, `swap-window`, renumbering | — | no | Windows are fixed slots |
| `kill-pane -t` | `close` | yes | |
| `kill-pane -a` | — | yes | |
| `detach-client [-t id] [-a]` | `detach` | yes | No target detaches everyone |
| `refresh-client` | — | yes | Forces a full repaint |
| `switch-client` | — | no | A weave server hosts one session, so there is nowhere to switch |
| `kill-session` | `quit` | yes | |
| `resize-pane -L/-R/-U/-D [n]` | — | yes | Moves a border; which side the pane is on decides if it grows |
| `resize-pane -x/-y` | — | yes | |
| `resize-pane -Z` | — | yes | Zoom; the layout tree is untouched so unzoom animates back |
| `resize-pane -M` | — | no | Mouse support is out of scope |
| `swap-pane [-U/-D/-s/-t/-d]` | — | yes | Both panes must be in one window |
| `rotate-window [-U/-D]` | — | yes | |
| `select-layout` | — | yes | `even-horizontal`, `even-vertical`, `main-vertical`, `main-horizontal`, `tiled` |
| `select-layout -p/-o/-E`, layout strings | — | no | weave keeps no layout history |
| `break-pane [-s] [-t] [-n] [-d]` | — | yes | The pane moves; it is not killed and respawned |
| `join-pane`/`move-pane [-s] [-t] [-hv] [-d]` | — | yes | |
| `list-panes [-a] [-t] [-F]` | — | yes | |
| `list-windows [-t] [-F]` | — | yes | |
| `list-sessions [-F]` | `wv ls` | partial | The command describes the session it runs in; `wv ls` lists them all |
| `list-*  -f` filters | — | no | Filter with `-F` and your shell |
| `display-message -p [-F]` | — | yes | Expands formats |
| `display-message` without `-p` | — | yes | Shows on the status line for 3 seconds |
| `capture-pane [-p] [-t] [-S n] [-E n]` | — | yes | Visible screen only |
| `capture-pane -S -` / negative lines | — | no | Needs scrollback |
| `capture-pane -e/-C/-J` | — | no | Captures are plain text |
| `capture-pane -b/-a` | — | no | Buffers went with copy mode |
| `has-session [-t]` | `wv has-session [name]` | yes | Exit status is the answer, as in tmux |
| `kill-server` | `wv kill-server` | yes | |
| `rename-session [-t] <name>` | — | yes | Moves the socket; attached clients keep rendering through it |
| `bind-key [-n] [-r] [-T]` | — | yes | |
| `unbind-key [-n] [-T] [-a]` | — | yes | |
| `list-keys [-T]` | — | yes | |
| `set-option [-g/-w/-p] [-u]` | TOML config | yes | Scope flags accepted and ignored — weave has one session |
| `show-options [name]` | — | yes | |
| Prefix key, `prefix`/`prefix2` | — | yes | `C-b` by default, with tmux's default bindings behind it |
| `source-file` | — | yes | Relative to the file that names it; `~` expands |
| `set-environment` | — | PR 9 | |
| `bind-key -N`, `list-keys -N` | — | no | Binding descriptions |
| `command-prompt [-p] [-I]` | — | yes | One line of text, `%%` in the template |
| `command-prompt -1/-N/-i/-k/-W/-T/-F` | — | no | The prompt reads one line of text |
| `copy-mode`, buffers, scrollback, search | — | no | Dropped: a phase of its own, not a PR |
| `run-shell [-b]` | — | yes | Returns stdout; a non-zero exit is an error result |
| `if-shell [-b]` | — | yes | The condition runs inline; `-b` only skips waiting on the branch |
| `wait-for [-S]` | — | yes | Signals only; `wv exec wait-for` blocks with no timeout |
| `wait-for -L/-U` | — | no | Locks; signals cover the coordination cases |
| `set-hook`, `show-hooks` | — | no | Deferred out of PR 9 — see the plan |
| `pipe-pane` | — | no | Deferred out of PR 9 — use `capture-pane` |
| `set-environment` | — | no | Deferred out of PR 9 |
| `attach-session -d` | `wv attach -d [name]` | yes | Detaches everyone else first |
| Multiple clients per session | — | yes | Smallest terminal wins |
| `list-clients` | — | no | See the note below |
| Mouse support | — | no | Dropped: weave is keyboard-driven, and wheel-scroll would need scrollback |
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
  `-t :build` finds it, and the status bar shows `3 ❯ build`.
- `new-window` takes the **lowest-numbered free window**, or the one `-t`
  names if it is free. With all nine in use it fails and says so.
- `select-window` **fails** on a window that does not exist, as in tmux. The
  `workspace-N` aliases behind `Alt+N` create one instead — that is how weave
  has always behaved and the keybindings keep it.

## Configuration

weave reads two files, both optional:

| File | Syntax | Applied |
|---|---|---|
| `$XDG_CONFIG_HOME/weave/config.toml` | TOML | first |
| `$XDG_CONFIG_HOME/weave/weave.conf` | tmux | second, so it wins |

The `.conf` file is a list of commands in the same language `wv exec` speaks.
`set`, `setw`, `bind`, `unbind` and `source` expand to their full names, `#`
starts a comment at a word boundary, and quotes and backslashes work as in a
shell — so `bind '#' ...` and `-F "#{pane_id}"` survive.

Only commands that configure weave are honoured there. A `split-window` in a
config file is refused with an explanation, because a config is read before any
session exists.

**A line that cannot be honoured does not abort the file.** Each one is logged
with its file and line number, so one unsupported option in a long `.tmux.conf`
costs you that line and nothing else.

### Options are live, inert, or unknown

- **live** — weave reads it: `prefix`, `prefix2`, `status`,
  `pane-border-status`, `repeat-time`, `default-shell`, `automatic-rename`,
  `target-fps`, `status-powerline`.
- **inert** — a real tmux option weave stores so `show-options` round-trips,
  but nothing reads. Setting one logs *why*: `history-limit` does nothing
  because there is no scrollback, `base-index` because windows are fixed slots.
- **unknown** — not a tmux option at all, so it is a typo and it fails.

### Keys

`C-b` is the prefix, with tmux's defaults behind it: `c` new-window, `%` and
`"` split, `x` kill-pane, `z` zoom, `&` kill-window, `d` detach, `n`/`p`/`l`
window movement, `o` next pane, `{`/`}` swap, `,` rename window, `$` rename
session, digits select windows, arrows move focus and `C-`arrows resize
(repeating, via `-r`).

`Alt+R` renames the current window and `Alt+Shift+R` the session, without a
prefix.

`Alt+Shift+H`/`J`/`K`/`L` move the focused pane itself, swapping it with the
neighbour in that direction the way a tiling window manager does. tmux has no
equivalent key; the underlying command is `swap-pane -t {left-of}`, aliased as
`move-left` and friends.

### Prompts

A rename needs a name, and a keybinding cannot supply one, so both rename keys
open a `command-prompt`: a one-line editor on the status bar, prefilled with
the current name so you edit rather than retype.

```
bind-key -T root M-r  command-prompt -p "rename-window:" -I "#W" "rename-window %%"
```

`%%` is where the typed text goes. Standing alone it becomes exactly one
argument however many spaces are typed into it, so a two-word name stays one
name.

While a prompt is open it is **modal** — every key goes to it, so nothing you
type can leak into the pane behind it. `Enter` runs it, `Escape`, `C-c` and
`C-g` cancel, `C-u` clears, and `Left`/`Right`/`Home`/`End`/`Backspace`/`Delete`
edit. Submitting an empty line cancels rather than running the command with an
empty argument.

weave's own `Alt` chords keep working with no prefix, in the `root` table —
which is also where `bind -n` binds. A key in no table reaches the pane. A key
pressed after the prefix is swallowed whether or not it is bound, so a mistyped
chord cannot leak into what you are editing.

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

## Pane titles

A pane's OSC 0/2 title names the **window** it is in, which is what the status
bar shows as `1:vim` and what `-t :vim` finds.

Drawing that title on every pane's top border as well is off by default: it
repeats what the pane's own contents already say, and at a few panes it is more
noise than help. Turn it back on with either spelling:

```toml
[ui]
pane_titles = true
```

```sh
set -g pane-border-status on
```

## The status bar

```
 dev  1 ❯ editor   2 ❯ build *      2026-05-11 ❮ 14:23  localhost 
```

Laid out the way tmux's `nordtheme/tmux` lays it out: blocks of colour with a
powerline wedge over each boundary, rather than a line of text.

The leftmost block is the **session name**, so a renamed session shows its new
name on the next frame. A `display-message` without `-p`, and an open
`command-prompt`, take that block over while they are up — which is where tmux
puts them too. Running outside a session it reads `weave`.

Then one block per window, `index ❯ name flags`, the current one in the accent
colour. The flags are tmux's `#F`: `*` for the current window, `-` for the last
one you were in, `Z` when it is zoomed.

The right end holds the date, the clock and the host — tmux's `#H`. A bar too
narrow for all three keeps the clock and drops the rest: the time is the part
you glance at, the date and the host the parts you already know.

A message and an open `command-prompt` take the leftmost block, and take
tmux's `message-style` with it — the block changes colour rather than just its
text, so a three-second notice does not read as a renamed session.

`status-left`, `status-right` and `status-style` are still accepted as options
but inert: the bar is not format-driven.

## Colours

The `nord` preset is the palette `nordtheme/tmux` resolves to, so weave beside
tmux is the same bar twice:

| What | tmux | nord |
|---|---|---|
| The bar | `status-style bg=black` | `#3b4252` |
| A window you are not in | `bg=brightblack` | `#4c566a` |
| The session block | `bg=blue` | `#81a1c1` |
| The current window, the host | `bg=cyan` | `#88c0d0` |
| Active pane border | `pane-active-border-style fg=blue` | `#81a1c1` |
| Other pane borders | `pane-border-style fg=brightblack` | `#4c566a` |
| A message or prompt | `message-style bg=brightblack,fg=cyan` | on `#4c566a` |

The border takes blue rather than cyan on purpose: cyan is what marks the
current window, and one accent doing two jobs is one fewer thing the eye can
rely on. `pane-border-style` and `pane-active-border-style` are not read from
a `.tmux.conf` — set `border_focused` and `border_unfocused` under `[theme]`.

### Without a patched font

The wedges are private-use codepoints, so a terminal running a font that has
not been patched draws a row of tofu. Turning them off falls back to plain
separators — `1 > editor`, `date | time` — and leaves the colours doing the
work:

```sh
set -g status-powerline off
```

## Renaming a session

`wv exec rename-session dev` renames a live session. It moves the listening
socket to match, which established connections do not care about — a Unix
socket connection survives its path changing, so every attached client keeps
rendering through the rename. Only new connections use the new name.

It refuses a name another live session already holds, and renaming to the name
it already has is a no-op rather than an error, so a script can set a name
unconditionally.

## Several terminals, one session

Attaching no longer evicts. Any number of terminals can watch a session, each
getting its own diff stream — a frame is a delta against what *that* terminal
has already seen, so the deltas cannot be shared even though the composed
frame is.

The session renders at the size of the **smallest** attached terminal, because
anything larger would be cut off for somebody. Terminals with more room see
the session in their top-left corner, as in tmux. Detaching the smallest client
grows the session back for everyone still watching.

- `wv attach [name]` joins.
- `wv attach -d [name]` joins and detaches everyone else.
- `wv exec detach-client` detaches everyone; `-t <id>` detaches one, `-a -t <id>`
  detaches all but one.

**Client ids are connection ids**, handed out in the order connections arrive —
and `wv exec` connections consume them too, so they are not predictable from a
script. There is no `list-clients` to discover them. In practice use the
no-target form, which detaches everyone.

Because a command arriving over a socket has no client of its own to mean, a
`detach-client` with no target detaches every terminal. That includes the
`Alt+D` keybinding: in a shared session it detaches everyone rather than
guessing which terminal pressed it.

## Moving panes between windows

`break-pane` and `join-pane` **move** a pane. It keeps running, keeps its `%N`,
and is never killed and respawned, so a script holding `%2` still means the
same pane after the move.

## Coordinating with the shell

`run-shell` returns the command's stdout, and a non-zero exit becomes an error
result — so `wv exec run-shell 'test -d /srv'` can be branched on.

`wait-for` blocks until something signals the channel:

```sh
wv exec wait-for -S build-done &   # from wherever the build finishes
wv exec wait-for build-done        # blocks here until then
```

A waiting `wv exec` has **no timeout** — waiting is the point — while every
other command still gives up after ten seconds rather than hanging a script.

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
