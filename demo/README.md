# Recording the demo

The README's demo is rendered from [`demo.tape`](demo.tape) with
[VHS](https://github.com/charmbracelet/vhs), so it can be re-rendered whenever
the UI changes instead of being re-recorded by hand.

## Install VHS

```sh
# Fedora / any distro, from the release binary
curl -fsSL https://github.com/charmbracelet/vhs/releases/latest/download/vhs_Linux_x86_64.tar.gz \
  | tar -xz -C /tmp && sudo install -m755 /tmp/vhs*/vhs /usr/local/bin/vhs

# or, with Go
go install github.com/charmbracelet/vhs@latest
```

VHS also needs `ffmpeg` and `ttyd` on PATH.

## Render

```sh
wv --version >/dev/null || echo "put wv on PATH first"
vhs demo/demo.tape          # writes demo/demo.gif and demo/demo.mp4
```

Then shrink the GIF before committing it — VHS's palette pass is conservative
and the file is usually 2-3x larger than it needs to be:

```sh
gifsicle -O3 --lossy=80 --colors 128 demo/demo.gif -o demo/demo.gif
```

Reference it from the top-level README with:

```markdown
![wv](demo/demo.gif)
```

## Notes

- **Powerline glyphs need a Nerd Font.** `Set FontFamily` at the top of the tape
  points at JetBrainsMono Nerd Font. Without one installed, either change that
  line or add `status_powerline = false` to the config the tape writes.
- **GIF flattens the animation.** wv blends sub-cell edges in sRGB across a
  truecolor range; a GIF gets 256 colours and 50 fps, so the half-block motion
  bands visibly. The tape also writes an `.mp4` — use it anywhere that takes
  video, and keep the GIF for the README.
- **The tape writes its own config** into a temporary `XDG_CONFIG_HOME`, so the
  recording never picks up whatever is in your real `~/.config/weave`.
- **Timing is in `Sleep` calls.** wv's tweens are 120-220 ms, so a `Sleep`
  shorter than ~500 ms after a split will cut the animation off mid-flight.

## Driving it from a script instead

Because `wv exec` speaks the same command enum as the keybindings, a layout can
be choreographed from outside the session rather than typed into it — useful for
a longer demo where keystroke timing gets fiddly:

```sh
wv --bare --session demo
( sleep 2
  wv exec --session demo split-window -h
  sleep 2
  wv exec --session demo split-window -v -t %1
  sleep 2
  wv exec --session demo select-pane -t %0
) &
wv attach demo
```

Scripted changes animate exactly like typed ones, so the recording looks the
same either way.
