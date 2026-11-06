---
title: Getting Started
description: Install tili on macOS with Homebrew, start the daemon, and grant the Accessibility and Input Monitoring permissions it needs.
---

tili installs as a signed macOS app through Homebrew. This page covers the
install, the permissions tili asks for on first run, the default
keybindings, troubleshooting, and building from source if you would rather
not use the tap.

## Before you start

tili targets macOS. `brew --version` should print a version before you
begin. Building from source instead of using the tap needs the full Xcode
toolchain, not just the Command Line Tools.

## Install

```sh
brew install itsdezen/tap/tili
```

```sh
tili start
```

That's it — `tili start` installs a background LaunchAgent (auto-restart,
auto-start at login) and **waits** for tili-daemon to actually finish
starting before returning. Ctrl-C stops the wait without affecting the
daemon, which keeps running independently under launchd.

## First run

- Triggers the **Accessibility permission** prompt, then the **Input
  Monitoring** one — grant both whenever you get to it; tili waits (up to
  a minute) and reports "running" the moment you do, no restart needed. If
  you don't grant Accessibility within that minute (or interrupt the wait
  yourself), the daemon stops itself cleanly and `tili start` reports
  that — just run it again once you're ready.
- Writes a starter config to `~/.config/tili/tili.kdl` if none exists yet
  — edit it and save, no restart needed, changes hot-reload. See
  [`example/tili.kdl`](https://github.com/itsdezen/tili/blob/main/example/tili.kdl)
  on GitHub for the full commented version.

Use the keybindings from your config (or the `tili` CLI directly — see
[Commands](/docs/commands)) to focus/move/tile windows. Run `tili stop` to
stop it and remove the LaunchAgent (so it stays stopped until you run
`tili start` again), `tili status` to check whether it's running, and
`tili --version`/`tili help` for the installed version and full command
list.

> **Tip**
>
> If a three-finger swipe up (Mission Control) shows tiled windows at tiny or
> wildly mismatched sizes, enable **Group windows by application** under
> **System Settings → Desktop & Dock → Mission Control** — this is a macOS
> Mission Control layout setting, not something tili controls.

## Verify it is running

```sh
tili status
```

[`tili status`](/docs/commands#cmd-status) reports whether the daemon is running and reachable. Two
other quick checks: the workspace badge appears in the menu bar (see
[Menu Bar Badge](/docs/menu-bar)), and opening a second window tiles it
into the same workspace instead of stacking it on top of the first.

## Default keybindings

The starter config written on first run is
[`example/tili.kdl`](https://github.com/itsdezen/tili/blob/main/example/tili.kdl).
These are its bindings for `main` mode. The three workspace names below
come from that file — if you rename the workspaces, the workspace binds
change with them.

| Key | Action |
|---|---|
| `alt-h` / `alt-l` / `alt-j` / `alt-k` | Focus left / right / down / up |
| `alt-shift-h` / `alt-shift-l` / `alt-shift-j` / `alt-shift-k` | Move the focused window left / right / down / up |
| `alt-shift-g` | Join the focused window with its left neighbor into a new container |
| `alt-w` / `alt-e` / `alt-r` | Switch to workspace `work` / `entertain` / `random` |
| `alt-shift-w` / `alt-shift-e` / `alt-shift-r` | Move the focused window to `work` / `entertain` / `random` |
| `alt-ctrl-w` / `alt-ctrl-e` / `alt-ctrl-r` | Pull `work` / `entertain` / `random` onto the focused display |
| `alt-ctrl-comma` / `alt-ctrl-period` | Move the active workspace to the previous / next monitor |
| `alt-ctrl-m` | Move the active workspace to the main monitor |
| `alt-tab` | Return to the previous workspace |
| `alt-slash` | Toggle the focused container's layout |
| `alt-shift-slash` | Toggle the container's orientation |
| `alt-shift-minus` / `alt-shift-equal` | Resize the focused window down / up one step |
| `alt-shift-0` | Balance the focused container evenly |
| `alt-shift-f` | Toggle the focused window between tiled and floating |
| `alt-m` | Focus the next monitor |
| `alt-shift-semicolon` | Enter `resize` mode |
| `alt-shift-m` | Enter `manage` mode |

Two of those open a mode rather than running one command:

- **`resize` mode** keeps `h`/`l` resizing the focused window one step per
  press until you press `escape` or `enter`.
- **`manage` mode** is one-shot: `alt-shift-h`/`j`/`k`/`l` join in that
  direction, `r` flattens the tree, and `escape` reloads the config.

Every bind lives in the commented example file; the schema behind them is
in [Configuration → keybindings](/docs/configuration#keybindings). The CLI
equivalents are in [Commands](/docs/commands).

## Troubleshooting

`tili doctor` checks the daemon's and menu bar badge's LaunchAgent state,
the IPC socket, and the config file, then reports anything wrong. A stale
socket file, an unloaded LaunchAgent, or one half of the daemon/menu-bar
pair missing while the other is installed can all be fixed automatically
(it asks first, unless you pass `--fix`). A broken config file or a
missing permission grant is reported only, never auto-fixed — only you can
decide what you meant.

## Other ways to install

`brew install itsdezen/tap/tili` above is the recommended path — it
installs a real, signed `tili.app` via
[itsdezen/homebrew-tap](https://github.com/itsdezen/homebrew-tap).

Building from source instead:

```sh
git clone https://github.com/itsdezen/tili
cd tili
cargo build --release --workspace
```

Or grab a prebuilt `tili.app` directly from a
[GitHub release](https://github.com/itsdezen/tili/releases) without
Homebrew. Releases are codesigned but not notarized yet, so Gatekeeper will
still prompt on first launch — right-click → Open, or
`xattr -d com.apple.quarantine tili.app`.

Next: set up your [configuration](/docs/configuration), or check the
[Commands](/docs/commands) reference for what each `tili` subcommand does.
