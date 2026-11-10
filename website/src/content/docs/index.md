---
title: Introduction
description: Why tili exists, how it stays event-driven on public macOS APIs, and what state the project is in today.
---

tili is a tiling window manager for macOS, built for speed. It follows an
i3-style workflow — directional focus/move, named workspaces, keybinding
modes — while staying on public macOS APIs.

## Where to start

- [Getting Started](/docs/getting-started) — install, permissions, first
  windows, and the default keybindings.
- [Configuration](/docs/configuration) — every `tili.kdl` section, key,
  type, and default.
- [Commands](/docs/commands) — the CLI reference and the command strings
  keybindings bind to.
- [Architecture](/docs/architecture) — how the Rust workspace is split and
  why the boundaries are where they are.
- [Menu Bar Badge](/docs/menu-bar) — the workspace badge and its menu.

The rest of this page explains why tili exists and what state the project
is in today.

## Why tili

tili is built around the details that matter once it is part of a daily
workflow: state that survives a restart, workspaces that know where they
belong, rules that can see the whole window, and placement that happens the
moment a window appears.

- **Event-driven.** The daemon subscribes to Accessibility and workspace
  notifications and reacts to them, so it does no work while your windows
  aren't changing.
- **Public API only.** Exactly one narrowly-scoped private call is used
  anywhere in the codebase (to resolve a window's real `CGWindowID`) —
  everything else is the public Accessibility API. You never disable System
  Integrity Protection to run tili.
- **Session restore, down to the special states.** Layout, focus, the active
  workspace on each monitor, floating geometry, and special states — native
  fullscreen, minimized, hidden app — survive a restart or crash.
- **Native-tab guard.** App-owned tabs (Finder, Terminal, Ghostty) stay
  inside the app: opening, closing, or switching a tab never creates a
  placement or relayouts the workspace.
- **Display-aware workspaces.** Every workspace has a home display:
  `workspace X` goes there, `workspace-here X` pulls it to the focused
  display, and `move-workspace-to-monitor next|prev|main` relocates it.
  Inactive workspaces park together at the southeast-most display's corner,
  and every connected display gets an active workspace, with synthetic
  fallbacks cleaned up after their windows migrate.
- **Window rules that see the whole window.** Match `app-id`, title regex,
  AX `role`, and `subrole`; route a window to a workspace; choose `float`,
  `tile`, or `ignore`; and size floating windows absolutely or by ratio with
  optional centering. Workspace rules route an app's first window by
  default, with `always` for every new window, and `tili rule` writes a
  ready-to-paste rule for the focused window.
- **Opt-in animation.** Moves are instant by default. Turn on `animate` for
  an eased transition with a configurable duration (`animate-duration-ms`,
  120ms by default), including a new window's entry into its slot.
- **Placement at window creation.** A new window joins the layout the moment
  it appears, so placement doesn't wait for a later refresh.
- **Mouse drag-resize.** Drag a tile's edge or corner to resize it; the result
  snaps to the same grid as keyboard resizes (`mouse-resize-step`).
- **KDL configuration, hot reloaded.** Workspaces, keybinding modes, and
  floating rules nest the way they actually relate, and saving the file
  applies changes live.
- **Menu bar workspace badge.** `tili-menubar` ships with the daemon and
  shows the active workspace on the focused monitor, without a second status
  bar app.
- **Signed releases through Homebrew.** The tap installs a signed `tili.app`;
  tili is MIT licensed and released through Homebrew.

## Status

tili is daily-drivable today: tiling, workspaces, hot-reloaded config,
built-in hotkeys, window rules (role/subrole, float/tile/ignore, floating
geometry), multi-monitor with automatic display fallback workspaces,
mouse/focus-follows-monitor, session restore, opt-in animation, a native-tab
guard, a menu bar workspace badge, JSON query/subscribe, and a real signed
release pipeline. Pre-1.0
still means config schema and CLI surface can change between releases —
check the [Roadmap](/docs/roadmap) for what's planned next, and the
[CHANGELOG](https://github.com/itsdezen/tili/blob/main/CHANGELOG.md) on
GitHub when upgrading.

Ready to try it? Head to [Getting Started](/docs/getting-started).
