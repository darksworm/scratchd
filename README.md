# scratchd

Scratchpads for Hyprland: toggle an app (Slack, Spotify, a terminal) in and
out of view with a key. A Rust rewrite of [pyprland](https://github.com/hyprland-community/pyprland)'s
scratchpads plugin that reads the same config.

The point of the rewrite is isolation. pyprland runs every scratchpad command
through one queue and waits up to ~12 seconds for a launched app's window, so
one app that is slow to start, crashes, or opens with an unexpected class
freezes every other scratchpad key until the wait runs out.

## How it works

- **One actor per scratchpad.** Each scratchpad has its own task and queue. A
  stuck app only delays its own key.
- **Launches don't block.** The command is started and the actor goes back to
  its queue. When the window opens it is shown or hidden, depending on the
  last key press, so pressing the key again during a slow start just changes
  your mind.
- **Failures surface at once.** A command that exits with an error raises a
  Hyprland notification right away; one that never opens a window does so
  after `spawn_timeout`. App output goes to
  `~/.local/state/scratchd/<name>.log`.
- **No tracked visibility.** Window placement and pin state are read from
  Hyprland on each command, so the daemon can't drift from what's on screen.
- **Fast client.** `scratchd toggle x` is a synchronous connect, write and
  read; the daemon acknowledges once the command is queued.
- **Self-healing.** Hyprland requests time out after 2s, the event stream
  reconnects, and a panicking actor is restarted and re-adopts its window.
  Apps run in their own session, so restarting scratchd leaves them running.

Requires Hyprland with the Lua config (0.55+).

## Usage

```sh
cargo install --locked --path .          # or --root ~/.local
scratchd daemon --log ~/.cache/scratchd.log   # start from Hyprland's autostart
scratchd toggle slack                    # bind this to a key
scratchd show|hide NAME...               # NAME may be * for all
scratchd status                          # JSON state of every scratchpad
scratchd reload                          # re-read the config (also on SIGHUP)
scratchd exit
```

In `hyprland.lua`:

```lua
hl.on("hyprland.start", function()
    hl.exec_cmd("scratchd daemon --log ~/.cache/scratchd.log")
end)
hl.bind("SUPER + Y", hl.dsp.exec_cmd("scratchd toggle slack"))
```

## Config

`~/.config/scratchd/config.toml` (or `--config FILE`), using pyprland's
`[scratchpads.<name>]` tables:

```toml
[scratchpads.slack]
command = "slack"
class = "slack"
size = "75% 87%"
position = "13% 7%"
excludes = ["*"]      # hide the others when this one shows
unfocus = "hide"      # hide when focus moves to another app
pinned = false
```

| key | default | |
|---|---|---|
| `command` | | shell command that starts the app |
| `class` | | window class; `re:` prefix for a regex |
| `match_by` | `class` if set, else `pid` | `pid`, `class`, `initialClass`, `title`, `initialTitle` |
| `size` | `80% 80%` | `%` of the monitor or `px` |
| `position` | centered on the animation edge | top-left corner, same units |
| `max_size` | | caps `size` |
| `animation` | `fromTop` | `fromBottom`, `fromLeft`, `fromRight`, corners, or `""` |
| `margin` | `60` | px from the screen edge |
| `lazy` | `true` | `false` starts the app with the daemon, hidden |
| `pinned` | `true` | show on every workspace |
| `unfocus` | | `"hide"` |
| `hysteresis` | `0.4` | seconds before an unfocus hide |
| `excludes` | | names, or `"*"` |
| `hide_delay` | `0.2` | seconds for the slide-out animation |
| `spawn_timeout` | `20` | seconds to wait for a window |

Differences from pyprland:

- A configured `class` is matched instead of the process ID, since PID
  matching misses apps that re-exec through wrappers (snap, flatpak).
- Focusing another window of the same class (an app's own popups) doesn't
  trigger `unfocus` hiding.
- `multi`, `smart_focus`, `use`, per-monitor overrides and the other pyprland
  plugins aren't implemented; their keys are ignored with a warning, and an
  invalid scratchpad is skipped without affecting the rest.

## Tests

```sh
cargo test
```

`tests/isolation.rs` runs the daemon against a fake Hyprland socket and
checks that a scratchpad stuck launching doesn't delay another, that a failing
command is reported immediately, and that toggling during a launch is honoured.
