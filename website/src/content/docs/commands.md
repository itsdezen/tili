---
title: Commands
description: Every tili CLI command, plus the keybinding command-string syntax that tili.kdl bind lines resolve to.
---

`tili start`/`stop`/`uninstall`/`status`/`doctor` manage the daemon's
LaunchAgent and local state directly; everything else below is a client
command sent over a Unix socket to a daemon that's already running.

> **Note**
>
> The CLI (`crates/tili-cli`) and the keybinding command strings your config
> binds (parsed by `crates/tili-ipc`) are two separate parsers that mostly
> agree on spelling — but not always. Where they differ, it's called out
> below; see [Configuration → keybindings](/docs/configuration#keybindings)
> for how `bind` lines resolve.

## Daemon & local state

| Command | What it does |
|---|---|
| <span id="cmd-tili"></span> `tili` (no subcommand) | Same as `tili start`, after an Enter-key confirmation (Ctrl-C or Ctrl-D cancels) |
| <span id="cmd-start"></span> `tili start` | Install and start tili-daemon as a background LaunchAgent (auto-restart, auto-start at login) |
| <span id="cmd-stop"></span> `tili stop` | Stop tili-daemon and remove its LaunchAgent |
| <span id="cmd-status"></span> `tili status` | Report whether the daemon is running and reachable |
| <span id="cmd-uninstall"></span> `tili uninstall` | Full teardown: stops both LaunchAgents, then removes the config file, logs, IPC socket, and the Accessibility permission grant. Doesn't touch a Homebrew install itself — run `brew uninstall tili` separately if that's how tili was installed. |
| <span id="cmd-doctor"></span> `tili doctor [--fix]` | Checks daemon/menu-bar LaunchAgent state, the IPC socket, and the config file, then reports any problems found. A stale socket file, an installed-but-unloaded LaunchAgent, or one half of the daemon/menu-bar pair missing while the other is installed can all be auto-fixed (with a confirmation prompt, unless `--fix` skips it). A bad config file or a missing permission grant is reported only — never auto-fixed, since only you can decide what you meant. |
| <span id="cmd-version"></span> `tili --version`/`-v` | Print the installed version |
| <span id="cmd-help"></span> `tili help`/`-h` | Print the full command list |

## Window & workspace commands

Sent over the socket to an already-running daemon; the same verbs (mostly)
resolve from keybindings too.

| Command | What it does |
|---|---|
| <span id="cmd-ping"></span> `tili ping` | Check the daemon is reachable (scripting-friendly; see `status` for the human-readable version) |
| <span id="cmd-focus"></span> `tili focus <left\|right\|up\|down>` | Move focus to the window in that direction |
| <span id="cmd-move"></span> `tili move <left\|right\|up\|down>` | Move the focused window one step in that direction, re-parenting it through the tree |
| <span id="cmd-join"></span> `tili join <left\|right\|up\|down>` | Wrap the focused window and its neighbor into a new, perpendicular container |
| <span id="cmd-resize"></span> `tili resize <amount>` | Grow (positive) or shrink (negative) the focused window's share of its nearest tiled container |
| <span id="cmd-balance"></span> `tili balance [--root]` | Reset child weights of the focused container (or workspace root, with `--root`) evenly |
| <span id="cmd-layout"></span> `tili layout <toggle\|tiles\|accordion\|horizontal\|vertical\|toggle-orientation> [--root]` | Toggle or set the focused container's layout/orientation |
| <span id="cmd-flatten"></span> `tili flatten` | Re-normalize the tree (no-op today — normalization already runs after every mutation) |
| <span id="cmd-fullscreen"></span> `tili fullscreen [--native]` | Toggle the focused window fullscreen (tiled, or `--native` for a real separate Space) |
| <span id="cmd-close"></span> `tili close` | Close the focused window (best-effort `AXCloseButton` press) |
| <span id="cmd-summon"></span> `tili summon <query>` | Focus (and raise) the first known window whose title or bundle id contains `query` |
| <span id="cmd-set-floating"></span> `tili set-floating <on\|off>` | Set the focused window's tiled/floating placement |
| <span id="cmd-toggle-floating"></span> `tili toggle-floating` | Flip the focused window between tiled and floating |
| <span id="cmd-query"></span> `tili query [state\|windows\|workspaces\|monitors] [--json]` | Print a machine-readable JSON snapshot |
| <span id="cmd-subscribe"></span> `tili subscribe [--json]` | Stream JSON state events until interrupted |
| <span id="cmd-rule"></span> `tili rule [--workspace <name>] [--copy]` | Print/copy a rule snippet for the focused window |
| <span id="cmd-workspace"></span> `tili workspace <name>` | Go to the workspace's home display (or focus the display already showing it) instead of moving a parked workspace to an unrelated display. |
| <span id="cmd-workspace-here"></span> `tili workspace-here <name>` | Switch the focused display to the workspace, pulling/swapping it here. |
| <span id="cmd-workspace-back"></span> `tili workspace-back` | Return to the previous display+workspace pair. |
| <span id="cmd-move-to-workspace"></span> `tili move-to-workspace <name>` | Move the focused window to another workspace without switching focus to it. **Note:** the equivalent keybinding command string is spelled `move-node-to-workspace <name>`, not `move-to-workspace` — the CLI and the keybinding parser chose different verbs for the same underlying command. |
| <span id="cmd-move-workspace-to-monitor"></span> `tili move-workspace-to-monitor <target> [--workspace <name>]` | Push a workspace to another monitor without switching focus; `target` is a monitor id, `"next"`, `"prev"`, or `"main"`. An active workspace swaps with the target's; a parked one only changes its home. Defaults to the active workspace on the focused monitor. |
| <span id="cmd-list-workspaces"></span> `tili list-workspaces` | List workspaces, with active/monitor markers |
| <span id="cmd-focus-monitor"></span> `tili focus-monitor [target]` | Cycle monitors, or focus `main`, `next`, `prev`, or a display id |
| <span id="cmd-list-monitors"></span> `tili list-monitors` | List connected monitors, marking which one is focused |
| <span id="cmd-list-windows"></span> `tili list-windows` | List known windows (tiled/floating, frame, pid) |

## JSON query and subscription

```sh
tili query state --json
tili query windows --json
tili subscribe --json
```

`query` prints one JSON document and exits. `query state` is versioned and
includes every window's `bundle_id`, `workspace`, `focused`, `role`,
`subrole`, placement, and frame; the other `query` targets print just that
array. `subscribe` keeps the connection open and prints one JSON event per
line: an initial `"snapshot"`, then a coalesced `"state_changed"` event with
a monotonic `seq` whenever state changes. Events carry monitors,
workspaces, mode, and the focused window — use `query state` when you need
the full window list.

`tili rule` prints a ready-to-paste `floating-rules` snippet for the focused
window; `--workspace <name>` emits a `workspace-rules` snippet, and
`--copy` puts it on the clipboard. The menu bar's "Copy rule for focused
window" item does the same.

## Keybinding-only command strings

A few command strings are only reachable from a `bind` line in your
config — they have no dedicated `tili <subcommand>`:

| Command string | What it does |
|---|---|
| <span id="cmd-mode"></span> `mode <name>` | Switch to a named keybindings mode |
| <span id="cmd-mode-exit"></span> `mode exit` | Return to the default (`"main"`) mode |
| <span id="cmd-reload-config"></span> `reload-config` | Force a config reload |
| <span id="cmd-shutdown"></span> `shutdown` | Shut the daemon down (used internally; not meant for interactive use) |

For the default `resize`/`manage` modes tili ships and how `join`'s
top-level quick-bind relates to the join binds inside `manage` mode, see
[Built-in default modes](/docs/configuration#built-in-default-modes).

## Targeting a specific window or workspace

Any keybinding or CLI command that operates on "the focused window" can
instead be pointed at a specific one by appending `--window-id <id>` and/or
`--workspace <name>` — both flags may appear in either order, and a later
one of the same kind wins if repeated. Anything not recognized as one of
the commands above (a typo, or a command from a newer config schema this
daemon build doesn't know yet) is accepted without error and simply
reported by `dispatch()` as "not implemented" rather than failing the
whole config load.
