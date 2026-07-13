# Changelog

All notable changes to this project are documented here. Format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/).

**Versioning convention (pre-1.0):** `v0.<milestone>.<patch>` — the minor
version tracks the milestone number from [ROADMAP.md](ROADMAP.md) (e.g.
`v0.1.x` ships once M1 is done), patch bumps are fixes within a milestone
that don't add new milestone scope. This resets to standard SemVer at v1.0.

## [Unreleased]

### Changed

- `tili start`/`stop` now manage tili-daemon's LaunchAgent directly,
  folding in what used to be the separate `tili daemon install`/
  `uninstall` subcommand: `tili start` writes and `launchctl load`s the
  `KeepAlive` LaunchAgent (no more foreground/Ctrl-C mode), and `tili stop`
  `launchctl unload`s and removes it. Fixes `tili stop` not actually
  stopping anything when the daemon was LaunchAgent-managed — it only sent
  a graceful shutdown over the socket, and launchd's `KeepAlive` would
  immediately respawn the process (and its global hotkey tap) the instant
  it exited.

## [0.11.1] - 2026-07-13

### Added

- `tili start`/`stop`/`status`: `tili start` `exec`s straight into
  `tili-daemon` (no separate binary to remember, no wrapper process left
  behind) as the common-case way to try tili out, replacing `tili-daemon &`
  in the docs. `tili stop` sends a new `Command::Shutdown`, handled
  directly in the daemon's main loop rather than through `dispatch()`
  (it's process lifecycle, not a `WmState` mutation — the one deliberate
  exception to that rule) and responds `Ok` before exiting. `tili status`
  reports reachability in plain language. README's Getting started/
  Commands sections rewritten around this simpler flow.

### Changed

- `example/tili.kdl`'s zero-padding gap override moved from the
  `entertain` workspace to `random`.

## [0.11.0] - 2026-07-13

### Added

- M11: release engineering — the real self-signed-cert pipeline, not just
  the tooling for one. `xtask` (`bundle`/`codesign`/`package`) wraps
  `tili-daemon`/`tili` in a minimal `tili.app` (bundle id
  `com.tili.daemon` — a stable, nameable target for Accessibility and
  codesigning, instead of a bare Unix executable), signs it with hardened
  runtime + a minimal entitlements file when `TILI_SIGN_IDENTITY` is set,
  and packages a tarball + sha256 per target. `release.yml`'s `build` job
  imports a certificate from `TILI_SIGNING_CERTIFICATE_P12`/
  `_PASSWORD` repo secrets and calls `xtask package`, conditionally — no
  secret, no signing, same as a local test build. The one-time certificate
  (fixed Common Name "tili Self-Signed", long validity, generated once via
  Keychain Access, never regenerated except on forced expiry — regenerating
  it would reset every user's Accessibility grant) and the
  `itsdezen/homebrew-tap` repository (hosting the real `Formula/tili.rb`,
  mirrored from this repo's copy) are both real, published infrastructure
  now, documented in CONTRIBUTING.md's new "Release Engineering" section.

  Getting the pipeline actually green took three real bugs, each found by
  reading the *actual* CI log (not guessed) after a same-shape failure hit
  more than once: (1) `secrets.X` isn't a recognized named-value inside a
  step's `if:` condition — has to go through a job-level `env:` var first;
  (2) `security find-identity`'s output has the identity's quoted name on
  a numbered line, not the first line — `head -1 | grep` grabbed
  `"Policy: Code Signing"` (no match) instead; a self-signed cert also
  correctly shows `CSSMERR_TP_NOT_TRUSTED` under "Matching identities" and
  0 under "Valid identities only" (expected/harmless — `codesign` doesn't
  require system trust, only `find-identity -v`'s own listing does); (3)
  `codesign`'s entitlements parser (`AMFIUnserializeXML`) is far stricter
  than a normal XML parser and rejects well-formed XML comments — the
  explanatory comment that used to live in `xtask/entitlements.plist`
  moved to a Rust doc comment on `xtask`'s `codesign()` instead, and the
  plist is now a bare `<dict/>`.

  Verified end-to-end on real hardware: `brew install itsdezen/tap/tili`
  installs a real signed `tili.app` (`codesign -dv` confirms
  `Authority=tili Self-Signed`, hardened runtime on), `tili --help`/`tili
  daemon install`/`uninstall` all work against the installed binaries, and
  the downloaded tarball's sha256 was independently recomputed and
  matched the published `*.tar.gz.sha256` before being written into the
  formula. `brew upgrade` preserving the Accessibility grant across two
  releases specifically wasn't exercised yet (only one signed release
  exists so far) — but it follows directly from the one thing this whole
  milestone was designed to guarantee: the signing identity is fixed and
  won't change between releases unless forced.

## [0.10.0] - 2026-07-13

### Added

- M10: daily-drivable MVP polish — wires up the three `settings` that had
  sat parsed-but-inert since M5. `mouse-follows-focus` warps the cursor
  (`CGDisplay::warp_mouse_cursor_position`) to the center of the
  newly-focused window inside `raise_focused`, so it applies uniformly to
  `focus`/`move`/`workspace switch`'s focus restore without duplicating the
  check at each call site. `focus-follows-monitor` adds `tili-ax`'s
  `spawn_mouse_watcher` — a `CGEventTap` on `kCGEventMouseMoved`,
  `ListenOnly` (never consumes/alters movement) and throttled to one
  position report per 80ms via a thread-local `Cell<Instant>` (not a
  shared `Mutex` — the callback only ever runs on its own dedicated
  thread) so mouse activity can't flood the daemon's event loop; the
  watcher runs unconditionally, same as the hotkey tap running regardless
  of whether any keybindings are configured, and `WmState::on_mouse_moved`
  is what actually gates on the setting, doing a cheap point-in-rect check
  against the already-cached `self.monitors` (no AX/CG call per event).
  `tili-cli` gains `tili daemon install`/`uninstall`, writing/removing a
  `~/Library/LaunchAgents/com.tili.daemon.plist` and driving `launchctl
  load|unload -w` — opt-in, never run automatically by `brew install`, so
  a fresh install can't hit a permission-denied respawn loop before
  Accessibility is granted; resolves the daemon binary relative to the
  running `tili` binary's own directory rather than trusting `PATH`, since
  a LaunchAgent's environment doesn't guarantee one. `tili-daemon` now
  writes `example/tili.kdl` (embedded via `include_str!`) to
  `~/.config/tili/tili.kdl` on first run if nothing's there yet, instead
  of silently applying empty built-in defaults with nothing to edit —
  best-effort; a write failure just falls back to defaults; the "config
  migration" half of this milestone is intentionally scoped narrowly here,
  since tili is pre-1.0 with no prior format to migrate *from*. Verified
  end-to-end on real hardware: focusing a window with mouse-follows-focus
  on visibly warps the cursor to it; moving the cursor onto a second
  monitor with focus-follows-monitor on retargets subsequent
  focus/move/workspace commands there; `tili daemon install` survives a
  logout/login with tili-daemon already running and the Accessibility
  grant intact.

## [0.9.0] - 2026-07-13

### Added

- M9: multi-monitor support. `tili-ax` gains real display enumeration
  (`list_monitors`, via `CGDisplay::active_displays`, re-enumerated fresh on
  every call rather than cached) and `spawn_display_watcher`, a
  `CGDisplayRegisterReconfigurationCallback` on its own dedicated
  `CFRunLoop` thread (same pattern as the NSWorkspace/AX watchers) that
  signals "something about the display setup changed, re-enumerate" — the
  flags aren't interpreted bit-by-bit, since re-running `list_monitors` is
  simpler and covers hot-plug, unplug, resolution, and rearrangement
  changes uniformly. `WmState.active_workspace` becomes a
  `HashMap<monitor id, workspace name>` — each connected monitor shows at
  most one workspace, laid out against its own frame; `focused_monitor`
  (new `Command::FocusMonitor` / `tili focus-monitor`, cycling, no-op with
  fewer than two monitors) is which one `Focus`/`Move`/`WorkspaceSwitch`
  target. `switch_workspace` now swaps a workspace already visible on
  another monitor rather than ever showing the same workspace on two
  monitors at once. On a monitor disconnecting, whatever workspace was
  showing there is parked (exactly like switching away from it — no window
  is lost, just no longer visible anywhere) and its monitor slot is
  dropped; a newly connected monitor gets a fresh empty workspace. Parking
  now targets a coordinate outside the *combined* bounds of every connected
  monitor (`tili_ax::combined_bounds`, unit-tested) instead of just past
  main's bounds — fixes a latent bug where a parked window could
  theoretically land on a second real monitor positioned to the right of
  main. `floating-rules` centering/sizing and `tili list-workspaces`/new
  `tili list-monitors` are all monitor-aware. Config-driven workspace-to-
  monitor pinning (`WorkspaceConfig.monitor`, parsed since M5) stays
  intentionally unwired — M9's bar is hot-plug/unplug safety, not that
  finer-grained UX. Verified end-to-end on real hardware: connecting a
  second display lets `focus-monitor` + `workspace <name>` tile a different
  workspace there independently of the main display, and unplugging it
  parks that workspace's windows (they reappear, still tiled correctly,
  the moment they're switched back to on a remaining display) rather than
  losing or stranding them.

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
