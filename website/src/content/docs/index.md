---
title: Introduction
description: Why tili exists, how it avoids polling and private APIs, and what state the project is in today.
---

tili is a tiling window manager for macOS, built for speed. It follows an
i3-style workflow — directional focus/move, named workspaces, keybinding
modes — while staying entirely on public macOS APIs.

## Why tili

Most tiling window managers on macOS force a trade-off: either they poll the
screen and burn CPU doing it, or they reach for private, undocumented APIs
that break every time Apple ships a system update. tili takes neither
shortcut.

- **Event-driven, not polling.** The daemon subscribes to Accessibility and
  workspace notifications and reacts to them — it does no work while your
  windows aren't changing. That's the whole idea behind lower idle CPU/RAM
  than the alternatives.
- **Public API only.** Exactly one narrowly-scoped private call is used
  anywhere in the codebase (to resolve a window's real `CGWindowID`) —
  everything else is the public Accessibility API. You never disable System
  Integrity Protection to run tili.
- **A config format that matches how you think.** [KDL](https://kdl.dev)
  instead of flat TOML tables — workspaces, keybinding modes, and floating
  rules nest the way they actually relate to each other.
- **Session restore.** Layout, focus, active workspace per monitor,
  floating geometry, and special states survive a restart or crash.
- **Display-aware workspaces.** Every workspace has a home display:
  `workspace X` goes there, `workspace-here X` pulls it to the focused
  display, and `move-workspace-to-monitor next|prev|main` relocates it.
  Inactive workspaces park together at the southeast-most display's corner,
  workspaces can be pinned to a monitor, and every display gets an active
  workspace at startup.
- **JSON query/subscribe.** `tili query state --json` and
  `tili subscribe --json` make status-bar and script integration a
  first-class path, not a long-poll workaround.
- **Native-tab guard.** App-owned tabs (Finder, Terminal, Ghostty) stay
  inside the app: opening/closing/switching tabs never creates placements
  or relayouts.
- **Animation-ready architecture, not animation-as-an-afterthought.** Every
  window-frame mutation goes through a single seam (`WindowFrameSetter`);
  instant moves and the opt-in tweened animation both use it.
- **No hotkey daemon required.** tili owns its own global hotkeys. One
  binary, one daemon, one config file.
- **Open source, installed via Homebrew.** No App Store, no sandbox
  restrictions on what a window manager needs to do.

## Status

tili is daily-drivable today: tiling, workspaces, hot-reloaded config,
built-in hotkeys, floating rules, multi-monitor, mouse/focus-follows-monitor,
session restore, a menu bar workspace badge, and a real signed release
pipeline. Pre-1.0
still means config schema and CLI surface can change between releases —
check the [Roadmap](/docs/roadmap) for what's planned next, and the
[CHANGELOG](https://github.com/itsdezen/tili/blob/main/CHANGELOG.md) on
GitHub when upgrading.

Ready to try it? Head to [Getting Started](/docs/getting-started).
