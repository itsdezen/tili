# Changelog

All notable changes to this project are documented here. Format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/).

**Versioning convention (pre-1.0):** `v0.<milestone>.<patch>` — the minor
version tracks the milestone number from [ROADMAP.md](ROADMAP.md) (e.g.
`v0.1.x` ships once M1 is done), patch bumps are fixes within a milestone
that don't add new milestone scope. This resets to standard SemVer at v1.0.

## [Unreleased]

## [0.8.0] - 2026-07-13

### Added

- M8: floating rules + auto-center/size on creation. `tili-config` parses
  `floating-rules { rule app-id="..." title="regex"? { width; height;
  center } ... defaults { center; width-ratio; height-ratio } }`.
  `tili-ax` gains `bundle_id_for_pid` (via `NSRunningApplication`,
  resolved once per process and shared across its windows, not once per
  window) and `AxWindow` now carries a `bundle_id`. `tili-daemon`'s
  `WmState` gains a `placements: HashMap<WindowId, Placement>` index
  (workspace + tiled-vs-floating) replacing the linear tree scan M4–M7
  used to find a window's workspace; floating windows are matched at
  creation time (first rule wins, title is an optional regex compiled
  once in `apply_config` — an invalid pattern logs a warning and drops
  just that rule, not the whole config) and centered/sized via the same
  `WindowFrameSetter` seam as tiling, entirely outside any workspace's
  `Tree`. Floating windows park and get re-centered alongside tiled ones
  on workspace switch. `tili list-windows` now shows `tile`/`float` per
  window. `example/tili.kdl` gains a real floating-rules block. Floating
  rules only apply at window-creation time, not retroactively to configs
  reloaded after a window already exists (matches the milestone's literal
  "on window creation" scope). Verified end-to-end on real hardware:
  windows matching a floating rule auto-center at the configured size
  instead of tiling, and stay excluded from the tiled layout.

## [0.7.0] - 2026-07-13

### Added

- M7: Accordion layout + toggle. `tili-tree`'s `Tree` gains `toggle_layout`
  (converts a window's parent container between `Split` and `Accordion` in
  place — converting to `Accordion` keeps the previously-visible window
  active rather than snapping to the first child), `is_accordion_container`,
  and `focus_in_direction` — a new, Accordion-aware navigation entry point
  that cycles (and wraps) an accordion's active child when the focused
  window is one of its members, falling back to plain spatial `navigate`
  otherwise. `WmState::focus`/`move_focused` now go through
  `focus_in_direction`, and gained `toggle_layout`/`set_layout` wired to
  `Command::LayoutToggle`/`LayoutSet`. `tili-cli` gains
  `tili layout <toggle|tiles|accordion>` (already bindable via
  `example/tili.kdl`'s existing `alt-slash` key from M6). Verified
  end-to-end on real hardware: toggling a live tiled container to Accordion
  stacks the windows (only one visible at a time), and focusing cycles
  through them.

## [0.6.0] - 2026-07-13

### Added

- M6: built-in global hotkey handling — no external tool (skhd etc.)
  needed. `tili-ax` gains `hotkey.rs`: a `CGEventTap` on its own dedicated
  `CFRunLoop` thread (same pattern as the NSWorkspace/AX watchers) that
  consumes matched keypresses so they don't leak into the focused app,
  plus `parse_key_combo` translating KDL key strings like `"alt-shift-h"`
  into a `KeyCombo` (modifiers can appear in any order). `tili-ipc` gains
  `parse()`, translating a keybinding's command string (`"focus left"`,
  `"mode resize"`, etc.) into a `Command` — unrecognized strings become
  `Command::Raw` rather than an error, so a config referencing a command
  ahead of its milestone (or with a typo) still loads. `tili-config` now
  actually parses `keybindings mode="..." { bind "key" "command" }` blocks
  (previously ignored as unrecognized). `tili-daemon`'s `WmState` gains
  `current_mode` and a `mode_bindings` table rebuilt on every
  `apply_config`; `Command::ModeEnter`/`ModeExit` switch it. Hotkey
  presses flow through the *same* `dispatch()` the socket handler uses —
  no parallel command path. The one exception to tili's "no locks, single
  owning loop" rule: the event-tap callback can't `.await` a round-trip to
  ask "is this bound," so a small `Arc<Mutex<HashSet<KeyCombo>>>` is
  shared with the daemon loop, re-synced after anything that could change
  the active mode or its bindings. Updated `example/tili.kdl` with a full
  `main`/`resize` keybinding set. Verified end-to-end on real hardware:
  bound keys are captured/consumed globally (not leaked into the focused
  app) and dispatch the right command, mode switching works, and editing a
  binding in `tili.kdl` changes its behavior live with no daemon restart.

## [0.5.0] - 2026-07-13

### Added

- M5: real KDL config parsing + hot-reload. `tili-config`'s `schema.rs`
  parses `workspaces`, `gaps` (global + per-workspace overrides),
  `default-layout`, and `settings` from `~/.config/tili/tili.kdl` via the
  `kdl` crate; unrecognized sections (`keybindings`, `floating-rules` —
  M6/M8 territory) are ignored rather than rejected, so a config can be
  written ahead of the parser catching up. New `watch.rs` uses `notify` to
  watch the config file's containing directory (not the file handle itself
  — covers editors that save via temp-file-then-rename) and re-parses on
  change; a parse error is logged and the previous config keeps applying,
  so a typo can't take down or silently misconfigure a running daemon.
  `tili-tree`'s `Tree::layout` now takes a `Gaps` (outer padding + inner
  spacing between siblings) instead of assuming zero. `tili-daemon` loads
  config at startup and bridges the (deliberately synchronous,
  runtime-agnostic) file watcher into its `tokio::select!` loop; new
  `WmState::apply_config` updates gaps and creates any workspaces the
  config declares (without switching to them, so a reload never yanks
  focus away from what's on screen). Added `example/tili.kdl` as a
  copy-pasteable starting config reflecting what's actually parsed today.
  Verified end-to-end on real hardware: editing gaps in `tili.kdl` and
  saving visibly changes spacing between real tiled windows with no
  daemon restart.

## [0.4.0] - 2026-07-13

### Added

- M4: named workspaces + off-screen virtualization, on the single monitor
  supported until M9. `WmState` now holds one `Tree` per workspace plus a
  `workspace_focus` map remembering each workspace's last focus, rather
  than one global tree. `switch_workspace` parks every window in the
  outgoing workspace (moves it off-screen via the new
  `AxWindow::set_position`, which — unlike `set_frame` — only touches
  position so parked windows aren't needlessly resized) and lays out the
  incoming one for real; `move_focused_to_workspace` moves the focused
  window into another workspace's tree and parks it immediately. New
  `Command::ListWorkspaces`/`WorkspaceSwitch`/`MoveNodeToWorkspace` handlers
  and matching `tili list-workspaces`/`workspace <name>`/
  `move-to-workspace <name>` CLI subcommands. Workspaces are created lazily
  on first switch (named workspaces from KDL config land in M5). Verified
  end-to-end on real hardware: switching away from a workspace with real
  windows visibly parks them off-screen, switching back restores them at
  their tiled positions.

## [0.3.0] - 2026-07-13

### Added

- M3: real BSP tiling on one monitor. `tili-tree`'s `Tree` gains actual
  mutation/layout/navigation algorithms (insert as a sibling of the focused
  leaf, remove with parent-split collapsing, i3-style direction navigation
  by walking up to the nearest matching-orientation ancestor split,
  window-identity swap for `move`), fully unit-tested without macOS
  (8 tests). `tili-ax`'s `AxWindow::set_frame`/`focus` are now real
  (`AXUIElementSetAttributeValue` for position/size, `AXFocused`/`AXRaise`
  for focus) instead of `unimplemented!()`; a new `display.rs` gets the
  main display's usable frame (full bounds minus a hardcoded menu-bar inset
  — proper per-monitor `NSScreen.visibleFrame` lookups land in M9).
  `tili-daemon`'s `WmState` now keeps live `AxWindow` handles (not just
  cached metadata) plus a `Tree` and focus pointer, wired through
  `Command::Focus`/`Command::Move`; `tili-cli` gains `focus <dir>` and
  `move <dir>`. Verified end-to-end on real hardware: opening several real
  windows tiles them via BSP, and `tili focus`/`move <dir>` correctly
  moves/raises the right window.

## [0.2.0] - 2026-07-13

### Added

- M2: `tili-daemon` now keeps a live, event-driven window cache instead of
  scanning on every `list-windows` request. `tili-ax` gains `workspace.rs`
  (bridges `NSWorkspaceDidLaunchApplicationNotification`
  /`DidTerminateApplication` via a dedicated `CFRunLoop` thread, since a
  non-`NSApplication` process needs one to receive Cocoa notifications at
  all) and `watch.rs` (subscribes each running app's `AXUIElement` to window
  created/destroyed/moved/resized/title-changed notifications via
  `axuielement`'s `AXNotificationStream`, coalesced into a single
  `WmEvent::WindowsChanged { pid }` signal per app so the daemon re-reads
  just that process's windows rather than diffing individual notification
  payloads). The daemon's main loop is now a single `tokio::select!` between
  socket connections and this event channel — no polling anywhere. Verified
  end-to-end on real hardware: launching/quitting apps and
  opening/closing/moving windows are reflected in `tili list-windows`
  without restarting the daemon, and idle CPU stays near zero.

## [0.1.0] - 2026-07-13

### Added

- M1: real window enumeration in `tili-ax` — finds on-screen windows' owner
  PIDs via public `CGWindowListCopyWindowInfo`, then reads each process's
  `AXWindows` through the public Accessibility API, resolving each window's
  real `CGWindowID` via the one private `_AXUIElementGetWindow` call.
  `tili-daemon` binds a real Unix socket and serves `Command::ListWindows`;
  `tili list-windows` is a real socket client. Verified end-to-end on real
  hardware: Accessibility permission granted, `tili list-windows` correctly
  lists real open windows.

### Notes

- Building `tili-daemon` requires full Xcode (not just Command Line Tools)
  — `axuielement`'s safe API links a Swift runtime bridge. See
  CONTRIBUTING.md.

## [0.0.0] - 2026-07-13

### Added

- Workspace scaffolding (M0): `tili-tree`, `tili-ax`, `tili-config`,
  `tili-ipc`, `tili-daemon`, `tili-cli`, `xtask` crates wired per the planned
  architecture. `cargo build --workspace` and `cargo test --workspace` pass.
- MIT license, community README, milestone roadmap, project dev context.
- CI: `fmt` + `clippy -D warnings` + `test` + `release build` gate on every
  push/PR to `main`.
