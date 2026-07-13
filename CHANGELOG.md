# Changelog

All notable changes to this project are documented here. Format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/).

**Versioning convention (pre-1.0):** `v0.<milestone>.<patch>` — the minor
version tracks the milestone number from [ROADMAP.md](ROADMAP.md) (e.g.
`v0.1.x` ships once M1 is done), patch bumps are fixes within a milestone
that don't add new milestone scope. This resets to standard SemVer at v1.0.

## [Unreleased]

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
