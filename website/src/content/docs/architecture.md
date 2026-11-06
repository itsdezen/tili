---
title: Architecture
description: How tili's Cargo workspace is split, why the Accessibility boundary is strict, and how layout stays testable without a Mac.
---

tili is a Cargo workspace split along strict boundaries so the hardest
parts (the container tree, the layout algorithms) can be tested without a
Mac at all, and the parts that touch the OS (`AXUIElement`, `NSWorkspace`,
display reconfiguration) stay isolated in one place.

```
crates/
├── tili-tree     pure container-tree + layout algorithms (Tiles/BSP, Accordion)
├── tili-ax       Accessibility API integration, the WindowFrameSetter seam
├── tili-config   KDL parsing + validation, hot-reload
├── tili-ipc      shared daemon/CLI protocol types
├── tili-daemon   the window manager: event loop, state, hotkeys
├── tili-cli      the `tili` command-line client
└── tili-menubar  the menu bar workspace badge
```

- **`tili-tree`** has no `AXUIElement`, no CoreFoundation, no `unsafe` — it
  operates on plain rectangles and window IDs, which is what makes it fully
  unit-testable on any platform.
- **`tili-ax`** is the only crate allowed to touch the Accessibility API.
  All real window-frame mutations go through one seam,
  `WindowFrameSetter` — an instant-move implementation today, with an
  animated one plugging into the same seam without a rewrite.
- **`tili-config`** parses and validates KDL, including hot-reload on save.
- **`tili-ipc`** defines the `Command`/`Response` protocol shared by the
  daemon and CLI, so there's exactly one definition of what a command
  means.
- **`tili-daemon`** is the window manager process. Every command — whether
  from a global hotkey or the CLI over a Unix socket — flows through the
  same `dispatch(&mut WmState, Command) -> Response` function. No locks, no
  drift between what a hotkey does and what the CLI does.
- **`tili-cli`** is a thin socket client; the binary is named `tili`. New
  behavior belongs in the daemon behind a `Command`, not here.
- **`tili-menubar`** is the `NSStatusItem` workspace badge described in
  [Menu Bar Badge](/docs/menu-bar).

## Workspaces are virtual

macOS exposes no public API to control Spaces, so inactive-workspace
windows are parked off-screen rather than relying on real Spaces — the same
technique other public-API-only tools in this space use. Every parked
window, whatever workspace it belongs to, sits at the bottom-right corner of
the southeast-most connected display; tili re-parks them after displays
connect, disconnect, or move. Picking the southeast-most display means the
window body extends past every other display's edges, so a parked window
can't reappear on a different monitor even with a diagonal arrangement.

Every connected display gets an active workspace at startup. A workspace
can be pinned to a display (`monitor="main"` or a numeric id); otherwise
tili reuses the display's last workspace, then the first free declared
workspace, and only then creates a temporary `display-N` fallback labeled
with the monitor's pretty name. Fallbacks with no windows disappear on
disconnect; non-empty ones migrate their windows to the main/focused
workspace first.

Each workspace also has a **home display**: where it is currently shown, a
connected pin, or the display it last lived on. `workspace X` goes there
(focusing the display already showing it, or showing it there and parking
that display's previous workspace); it never materializes a parked
workspace on an unrelated display. `workspace-here X` explicitly pulls X to
the focused display, swapping with its occupant, and
`move-workspace-to-monitor next|prev|main` pushes the active workspace to
another display (or only changes a parked workspace's home).
`workspace-back` restores the previous display+workspace pair.

## Design invariants

These are non-negotiable, enforced by the architecture rather than style
preference:

- **No private Accessibility/window APIs** beyond one documented call (to
  resolve a window's real `CGWindowID`) — this is what lets tili run
  without disabling System Integrity Protection.
- **No polling.** The daemon reacts to AXObserver/NSWorkspace/display
  notifications. A handful of narrowly-scoped exceptions exist (hotkey
  permission retry, a watcher-resync backstop, UI debounce, and
  animation-tick interpolation) — each is documented in the codebase as a
  deliberate, bounded exception, not a precedent for more.
- **Accessibility permission gets no in-process wait/poll/restart of any
  kind** once the daemon is running — it checks once at startup and stops
  itself if not granted.
- **All real window-frame mutations go through `WindowFrameSetter`** — the
  one seam that knows how a window's frame actually gets set.
- **One active workspace per display, never duplicated, and each workspace
  has a home display.** `workspace X` goes to X's home (focusing the display
  already showing X); `workspace-here X` pulls it to the focused display;
  `move-workspace-to-monitor` pushes/relocates it. Display fallbacks are
  synthetic, named from the monitor, and cleaned up on disconnect after
  migrating their windows.
- **Native app tabs are opaque.** A new window that is really the active
  member of an app-owned tab group replaces the managed window id in place;
  it never creates a placement or triggers a relayout. Full tab-group
  membership/restore is future work.
- **Hotkey- and socket-triggered commands both go through `dispatch()`** —
  no parallel command path.

For the full per-crate rationale and hardware-confirmed findings behind
each of these, see
[`docs/architecture/`](https://github.com/itsdezen/tili/tree/main/docs/architecture)
in the repository. The runtime side is documented in
[Commands](/docs/commands), and the config side in
[Configuration](/docs/configuration).
