# Changelog

All notable changes to this project are documented here. Format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/).

**Versioning (pre-1.0):** plain SemVer — minor bumps ship new features,
patch bumps are fixes. This resets to standard SemVer conventions at v1.0.

## [Unreleased]

### Fixed

- **Post-wake readiness and the workspace-jump bug were both symptoms of the
  same guessed timer** — `WAKE_GRACE_MAX`/`WAKE_GRACE_DEBOUNCE` replaced
  entirely with a real per-window AX probe fired the instant wake is
  observed, confirming each pid individually (`WmState::unconfirmed_pids`)
  instead of gating everything behind one global deadline. A fast machine's
  "wait for ready" now shrinks to the probe's actual round-trip instead of a
  worst-case constant; a workspace can no longer auto-switch on behalf of a
  pid that hasn't proven it's reconnected, however long that takes, instead
  of being freed the moment an arbitrary clock ran out.
- **Still-open windows without a matching `workspace-rules` entry got
  relaid-out into the wrong spot after wake, and the active workspace/focus
  could still drift away from what was showing right before sleep.** Two
  fixes: the background watcher's own resync backstop
  (`tili-ax`'s `resync_watchers`) could reliably win a race against the real
  wake notification and rescan every window before `note_system_wake` had a
  chance to protect any of them — it now self-detects a real sleep (its
  `recv_timeout` wait running far longer than requested) and forwards a
  synthetic wake signal first, closing the gap. And `place_new_window`/
  `reveal_frontmost`'s auto-switch is no longer gated per-app-pid at all —
  a hard `WmState::wake_lock_active` lock now blocks every auto-switch and
  auto-refocus unconditionally from the moment wake is observed until the
  next real hotkey- or socket-triggered command, since a per-pid probe
  responding quickly doesn't mean the rest of the system's AX/WindowServer
  state has actually settled.
- **Switching workspaces had visible deadtime between the hotkey and the
  screen actually updating** on workspaces with several windows —
  `switch_workspace`'s park/relayout/reposition AX writes ran one window at
  a time. They now fire concurrently (one thread per window, joined before
  the switch returns), so the wait drops from the sum of every window's
  round-trip to roughly the slowest one.
- **All three fixes above never actually ran for this project's own
  day-to-day "sleep," because that gesture isn't a real sleep.** Every one
  of them is gated on `NSWorkspaceDidWakeNotification`, which macOS only
  sends when the machine genuinely suspends — confirmed via real
  `daemon.err.log` output that locking the screen and waiting for the
  display to blank (this project's actual daily trigger, as opposed to
  `pmset sleepnow`/lid-close/Apple-menu Sleep) never produces that
  notification at all. Added `WmEvent::ScreenLocked`/`ScreenUnlocked`
  (`com.apple.screenIsLocked`/`screenIsUnlocked`, via a new
  `NSDistributedNotificationCenter` registration — a screen lock isn't an
  `NSWorkspace` event) reusing the exact same `unconfirmed_pids`/
  `wake_lock_active` machinery above rather than a parallel mechanism:
  locking arms the freeze and marks every tracked window unconfirmed
  immediately (no AX probe yet — the session is switched to `loginwindow`
  while locked, where AX reads can't be trusted), and unlocking runs the
  deferred probe once the session — and its AX connections — are actually
  back.
- **A window could stay parked off-screen indefinitely after unlock, only
  reappearing after switching workspaces back and forth several times** —
  `AxWindow::set_frame`/`set_position`/`set_size` discarded the AX write's
  `Result` (`let _ =`) and advanced the cached frame to the target
  unconditionally, even when the write itself had failed. A write briefly
  failing right after unlock (the app still reconnecting to AX/WindowServer)
  left the cache lying about the window's real position; every later call's
  no-op-if-unchanged guard then compared against that lying cache, saw a
  false match, and silently skipped writing the window back on-screen. All
  three now only advance the cache on a successful write, so a failed write
  leaves the cache honest and the very next call retries it for real.
- **Two genuinely still-open windows got finalized as closed shortly after
  unlocking the screen** — confirmed via real `daemon.err.log` output
  showing them finalized ~93s "pending," essentially the entire lock
  duration. The screen-lock freeze above blocks `finalize_expired_removals`
  and the auto-switch paths, but did nothing about the background watcher's
  own periodic resync (`tili-ax`'s `resync_watchers`), which kept
  re-enumerating every on-screen app's windows throughout the lock — AX
  reads for *other* apps can silently come back empty while the session is
  on `loginwindow`, and a scan that comes back empty was indistinguishable
  from a real close, landing the window in `pending_removal` mid-lock. The
  watcher thread now tracks lock state and skips this resync entirely while
  locked, forces one real full resync the instant unlock is observed
  (rather than waiting for the next tick), and unlocking also restarts the
  grace-period clock on any already-pending window — closing a race where
  the daemon's single event loop could otherwise finalize an old pending
  entry before that forced resync had actually re-verified it.
- **The same finalize-as-closed bug above can also happen on a genuine
  sleep, not just a screen lock** — a long real sleep advances `Instant`
  just as much as a lock sitting frozen does, so any window already in
  `pending_removal` before the machine suspended would have a clock read as
  far past `removal_grace` the moment it wakes, regardless of the lock/wake
  distinction. Both of the lock/unlock fix's layers now apply symmetrically
  to `WmEvent::SystemDidWake`: the watcher thread forces one real full
  resync immediately on wake (at both places it can observe one — the real
  `NSWorkspaceDidWakeNotification` and its own `recv_timeout`-based sleep
  detection — rather than only one of them), and `WmState::note_system_wake`
  now also restarts the grace-period clock on every already-pending window,
  same as `note_screen_unlocked`.

## [0.7.2] - 2026-08-24

### Fixed

- **Wake-grace used one fixed 90s timer for every wake, instead of tracking
  how long reconnection was actually still happening** — two symptoms, same
  root cause. On a fast machine, everything after wake waited out the full
  90s even once every app had long since reconnected. On a machine slower to
  reconnect, an app that took longer than 90s to reconnect still got its
  window wrongly `finalize_expired_removals`'d as closed, then rediscovered
  moments later and treated as brand-new — re-triggering a matching
  `workspace-rules` entry and silently jumping the active workspace (e.g.
  entertainment → work). `note_system_wake` now starts a capped debounce
  instead of a flat timer: the grace window extends by 3s (proposed,
  `WAKE_GRACE_DEBOUNCE`) each time a window visibly vanishes or reconnects
  during `apply_windows_changed`, capped at 180s from the wake instant
  (proposed, `WAKE_GRACE_MAX`) — mirroring the existing
  `FULL_RESYNC_DEBOUNCE`/`FULL_RESYNC_MAX_INTERVAL` shape in
  `tili-ax/src/watch.rs`. `place_new_window`'s and `reveal_frontmost`'s
  auto-switch guards are unchanged in behavior, just now keyed off the same
  debounced deadline. Both new values are proposals pending real-hardware
  validation, the same way 90s itself was only confirmed correct after
  several rounds of real sleep/wake testing.

## [0.7.1] - 2026-08-19

### Fixed

- **Dismissing a notification (or Spotlight with Esc) over an empty
  workspace could jump the display to whatever workspace the previously
  frontmost app lives on, and stick there.** Notification Center/Spotlight
  transiently holding AX-frontmost status on themselves while animating a
  dismiss was corrupting internal frontmost-tracking state, which the
  following handback then misread as a genuine app switch to follow.

## [0.7.0] - 2026-08-09

### Added

- **Menu bar badge now shows a distinct icon while `resize`/`manage`
  keybinding modes are active** — ↔ for `resize`, ⚙ for `manage` (sized to
  match the workspace name's own text height), instead of always showing
  the plain workspace dot. Updates in lockstep with the daemon over the
  existing event-driven `WaitForChange` channel — no polling, no visible
  delay entering or exiting a mode.

### Changed

- **`manage` mode's default entry bind changed from `alt-shift-s` to
  `alt-shift-m`**, for a clearer mnemonic ("**m**anage"). Update your own
  config if you copied this bind from `example/tili.kdl`.

## [0.6.1] - 2026-07-25

### Fixed

- **Notch-aware `gap top` (v0.6.0) inflated the effective top gap by a
  full extra menu-bar height on every notched Mac**, leaving a wide gap
  above tiled windows even with `outer`'s top value set to `0`.
  `Monitor.notch` was the raw `NSScreen.safeAreaInsets.top`, which on a
  notched display already covers the same top-of-screen zone the existing
  hardcoded menu-bar inset accounts for — adding both double-counted it.
  `Monitor.notch` is now only the amount by which the notch's safe area
  *exceeds* that baseline.

## [0.6.0] - 2026-07-25

### Added

- **Notch-aware `gap top`.** On a display with a notch, the tiled area's
  top edge now starts below the notch instead of overlapping it — `Monitor`
  picks up its display's `NSScreen.safeAreaInsets.top` and the daemon folds
  that height into the configured `outer` top gap. New `ignore-notch`
  flag in the `gaps` config block (`#false`/default: notch-aware;
  `#true`: use the configured top gap as-is, matching pre-existing
  behavior) — settable globally or per-workspace, same as `inner`/`outer`/
  `accordion`.

## [0.5.3] - 2026-07-23

### Fixed

- **Finder's Quick Look preview panel (Space) and "Get Info" panel
  (Cmd+I) were floating and centering instead of being left alone**, even
  when `com.apple.finder` wasn't configured as a floating app at all.
  Quick Look is a `WindowKind::Popup`, which already defaults to `Ignore`,
  but a `floating-rules` entry for `com.apple.finder` (written for its real
  browser windows) has no way to exclude this one auxiliary panel and an
  explicit rule always wins over the kind-based default. Get Info reaches
  `WindowKind::Dialog` via the zoom-but-no-fullscreen heuristic, whose own
  kind-based default is `Float` — wrong regardless of any rule, since the
  window should never be touched at all. Both are now force-`Ignore`d
  unconditionally, the same mechanism already used for Finder's "Copy" and
  "Connect to Server" dialogs and System Settings' suggestion popup.

## [0.5.2] - 2026-07-23

### Fixed

- **A Preferences/Settings-style window (e.g. System Settings' own main
  window) could be force-`Ignore`d instead of floating/centering**, even
  with an explicit `floating-rules` entry asking for it to float.
  `is_transient_empty_dialog` (added to catch the input-source-switch HUD
  glyph) matched any `WindowKind::Dialog` with an empty title, but a
  Preferences/Settings-style window also reaches `Dialog` via
  `classify_window_kind`'s zoom-but-no-fullscreen heuristic — and its
  `AXTitle` routinely reads empty at the exact moment tili scans it,
  before AX has populated it. That race got it misidentified as the
  transient HUD glyph and force-`Ignore`d, overriding the user's own
  config. `is_transient_empty_dialog` now also requires the window to have
  no zoom button, which the HUD glyph never has but a Settings-style
  window always does.
- **System Settings' search-suggestions dropdown (shown while typing in its
  search field) was floating and centering instead of being left alone.**
  Confirmed via diagnostic logging it's a borderless, chrome-less,
  empty-titled `WindowKind::Popup` — the same ambiguous shape as an
  ordinary tooltip/menu, which normally defaults to `Ignore`. The user's own
  `rule app-id="com.apple.systempreferences"` `floating-rules` entry (meant
  for the app's real Preferences windows) has no way to exclude just this
  one auxiliary window, and an explicit rule always wins over the
  kind-based default. Now force-`Ignore`d unconditionally, regardless of
  `floating-rules` config, the same way Finder's protected dialogs are.

## [0.5.1] - 2026-07-22

### Fixed

- **Moving "the focused window" to another workspace could act on a stale,
  previously-focused window instead of whatever the user actually had
  focused** — most reliably reproduced with a floating window (e.g. Note)
  sharing a workspace with a tiled one (e.g. Ghostty). Root-caused to two
  compounding gaps: (1) `tili-daemon`'s event loop only applied a pending
  new-window registration on the next 30ms maintenance tick, so a hotkey
  fired right after opening a window could dispatch before that window was
  even known about — now drained before dispatching a hotkey- or
  socket-triggered command instead. (2) The system-wide focus check
  (`sync_focus_from_frontmost`, run before every command) read
  `kAXFocusedWindowAttribute`, an attribute macOS's system-wide
  accessibility element never actually populates — confirmed on real
  hardware it silently failed on *every* call, every time, regardless of
  what was focused. Now reads `kAXFocusedUIElementAttribute` (the one
  attribute the system-wide element reliably supports) and resolves the
  owning window from whatever control that returns.
- **`tili uninstall` could still delete the real config file when a
  dotfiles tool symlinked the whole `~/.config/tili` directory**, rather
  than just `tili.kdl` itself — the file-level symlink case was already
  handled, but a symlinked ancestor directory wasn't. Now walks every
  ancestor of the config path and leaves it in place if any of them is a
  symlink.
- **`brew upgrade tili` didn't actually restart onto the new binary.**
  `post_install`'s restart works by signaling the running process and
  relying on the LaunchAgent's `KeepAlive` to relaunch it — but the plist's
  cached path was fully resolved to a specific, version-pinned Cellar path,
  which never changes on its own, so every upgrade kept relaunching the
  exact same old binary until a manual `tili stop && tili start`. `tili
  start` now resolves that path through Homebrew's `opt/tili` symlink
  instead, which Homebrew itself relinks to the current keg on every
  upgrade. **If you're upgrading from an affected version (v0.4.3 or
  later), run `tili stop && tili start` once after this upgrade** to
  regenerate the LaunchAgent plist with the corrected path — this fix only
  changes what a future `tili start` writes, not an already-installed
  plist.

## [0.5.0] - 2026-07-22

### Added

- **`tili doctor [--fix]`** — checks config syntax, daemon/menu-bar
  LaunchAgent state, the IPC socket, Accessibility/Input Monitoring
  permission grants, and the last config load's warnings. Offers
  confirmed auto-fix for whatever's safely automatable (a stale socket, an
  unloaded LaunchAgent, a missing half of the daemon/menu-bar pair); a bad
  config file or an ungranted permission is reported only, never guessed
  at. `--fix` skips the confirmation prompt for scripting.

### Changed

- **`tili-daemon` and `tili-menubar` now run as a synchronized pair,
  never one without the other.** `tili start` requires both LaunchAgents
  to install, rolling the daemon back if the menu bar badge's install
  fails; `tili-menubar` gives up and stops itself after a sustained run of
  daemon-unreachable retries; `tili-daemon`'s shutdown paths
  (`Command::Shutdown`, `stop_self`) tear the menu bar's LaunchAgent down
  too.

### Fixed

- **A hotkey-triggered command that failed (no window focused, an
  undeclared workspace, ...) left no trace anywhere** — unlike the
  socket path, which already reported the error back to `tili-cli`. Now
  logged.
- **The daemon's Unix socket had no cap on an incoming command's declared
  length**, so a stray or malformed local client could drive an
  unbounded allocation (up to ~4 GiB) just from its 4-byte length prefix.
  Capped at 1 MiB.
- **`tili start` could print a spurious `Load failed: 5: Input/output
  error` from `launchctl` before its own "LaunchAgent installed" message.**
  `install_launch_agent` called `launchctl load -w` unconditionally, even
  when the label was already loaded (e.g. the menu bar badge's LaunchAgent
  surviving from an earlier `tili start`) — a well-known legacy
  `launchctl` quirk on an already-loaded label, harmless but noisy, and it
  also meant a rewritten plist (e.g. a changed binary path after an
  upgrade) silently didn't take effect. Now unloads first if already
  loaded.
- **`tili uninstall` deleted `~/.config/tili/tili.kdl` even when it was a
  symlink**, breaking a dotfiles manager's (stow, chezmoi, ...) managed
  arrangement. Now left in place if it's a symlink.
- **Toggling input sources (Globe/Ctrl-Space) showed the "A" HUD glyph as
  a floating window tili re-centered**, instead of leaving it alone like
  other transient system UI. Unlike the volume/brightness HUD
  (`com.apple.OSDUIHelper`, also added to the system-UI bundle-id
  denylist while investigating this), this glyph isn't owned by a
  dedicated system process at all — it's attributed to whichever app is
  frontmost at the moment, confirmed via diagnostic logging. Now matched
  by shape instead: any brand-new `Dialog`-kind window with an empty title
  is ignored, regardless of which app it's attributed to.
- **Moving a focused floating window to another workspace could move a
  different, unrelated tiled window instead**, when the floating window
  belonged to a different app than whatever macOS still considered
  frontmost. `sync_focus_from_frontmost` resolved "what's focused" via
  `frontmost_app_pid()` (which app is frontmost) then that app's own
  focused window — a two-hop lookup that misses a floating panel/utility
  window that can hold real AX focus without its owning app ever becoming
  `NSWorkspace.frontmostApplication`. Now queries the system-wide focused
  window directly (`AxWindow::system_focused_id`), sidestepping the
  "which app" hop entirely.

## [0.4.4] - 2026-07-22

### Fixed

- **`tili uninstall` could print a spurious `Unload failed: 5:
  Input/output error` from `launchctl` before its own "uninstalled"
  message.** `stop_daemon` called `launchctl unload` on a LaunchAgent
  plist whenever the file existed on disk, even if launchd no longer had
  that job loaded (e.g. the daemon had already unloaded itself via
  `stop_self`). Now skips the unload when the job isn't actually loaded,
  still removing the stale plist file either way.
- **A fresh install's first `tili start` could show no `tili` item to
  toggle in System Settings' Accessibility list.** Right after
  triggering the Accessibility prompt, `tili-daemon` ran `tccutil reset`
  on its own TCC entry — since the prompt's row is created
  asynchronously by macOS, this reset raced it and deleted the row
  before it was ever visible to toggle, on every single attempt. That
  reset is gone.
- **`tili status` could report the daemon "running" while hotkeys
  silently didn't work.** Input Monitoring was treated as a soft
  condition — the daemon kept running in a degraded state if it wasn't
  granted. It's now a hard gate like Accessibility: if Input Monitoring
  isn't granted, the daemon stops itself and requires an explicit `tili
  start` once both permissions are granted, instead of half-running.
- **`brew install`/`brew upgrade` could skip restarting the menu bar
  badge (or the daemon) after an asymmetric LaunchAgent state.**
  `post_install`'s restart gate checked only `com.tili.daemon.plist`'s
  existence before restarting both processes; each LaunchAgent is now
  checked independently.

## [0.4.3] - 2026-07-22

### Fixed

- **The menu bar badge could show up in System Settings' Menu Bar list as
  `tili-menubar` with no icon instead of `tili` with its logo.**
  `sibling_binary_path` derived the badge's LaunchAgent path from
  `current_exe()`'s parent directory without canonicalizing first — since
  Homebrew's `bin/` prefix holds a `tili-menubar` symlink right next to
  `tili`'s own, an unresolved `current_exe()` found it there and baked
  that symlink path into the plist instead of the real path inside
  `tili.app`, which is what actually carries the bundle's name/icon.
  `sibling_binary_path` now canonicalizes first. Existing installs pick
  this up after `tili stop && tili start` (or a fresh `brew upgrade` per
  its caveats).

## [0.4.2] - 2026-07-22

### Fixed

- **`brew install`/`brew upgrade` could report a false "post-install step
  did not complete successfully" warning.** `post_install` used `system`
  to run `pkill -x tili-daemon`/`pkill -x tili-menubar`, which treats
  `pkill`'s exit 1 (no matching process — the normal case whenever the
  LaunchAgent plist exists but the process isn't currently running) as a
  build failure. Switched to `quiet_system`, which doesn't raise on a
  non-zero exit.

## [0.4.1] - 2026-07-22

### Fixed

- **Preferences/Settings-style windows no longer get tiled.** Some apps
  (e.g. Safari's own Settings window) present their preferences panel as
  an ordinary `AXStandardWindow`, indistinguishable by subrole from a real
  content window, so it fell through to `WindowKind::Standard` and got
  tiled like any other window. `classify_window_kind` now also treats a
  standard-subrole window as `Dialog` (tili's default float/center
  treatment) when it has a zoom button but no full-screen button — AppKit
  gives fullscreen-capable content windows a full-screen button and
  non-fullscreenable utility panels a classic zoom button instead, so this
  generalizes to any app with the same window shape rather than a
  per-bundle-id fix.

## [0.4.0] - 2026-07-22

### Added

- **Animated window movement (opt-in).** New `animate` setting (`#false`,
  `#true`, `"medium"`, or `"high"` — default `#false`; `#true` is
  shorthand for `"medium"`) eases tiled relayout and floating-window
  placement — including a newly-centered floating window's placement —
  into their new frame over a short duration (90ms) instead of jumping
  straight there, fixing the visible "flick" on relayout, post-drag
  mouse-resize snap, and floating centering. `"medium"`/`"high"` pick the
  animation's tick rate (~60fps/~120fps) — `tili-daemon` reconstructs its
  dedicated animation timer to match on every config change, and that
  timer only runs at all while something is actually animating.
  Performance tradeoff: each animation step is a real AX write, not a free
  interpolation, so `"medium"` costs roughly 6x the AX round-trips of a
  plain instant move per relayout, and `"high"` roughly 11x (double
  `"medium"`) — negligible on responsive native apps, more noticeable as
  extra per-relayout latency on a slow-to-respond one. Implemented as
  `TweenedFrameSetter`, a second `WindowFrameSetter` alongside the
  existing `InstantFrameSetter`. Doesn't affect a user's own native
  drag/resize of a floating window (tili never writes during that);
  parking a window off-screen and the size-discovery step of centering a
  floating window stay instant on purpose, and so does revealing a
  workspace's windows on switch (they're showing at wherever they were
  parked, which was never meant to be seen, so there's no start point to
  meaningfully ease from).

## [0.3.1] - 2026-07-21

### Fixed

- **Reopening a single centered floating window no longer drifts
  off-center.** The cascade nudge added in 0.3.0 (to keep several
  same-sized centered windows from stacking exactly on top of each other)
  used to advance a per-workspace counter on every placement and never
  reset it, so repeatedly opening and closing one floating window — with
  no other centered window ever present — still cycled through the
  cascade sequence on every reopen. The nudge now only applies when
  another centered floating window is actually on screen at the same
  time, so a lone window always reopens dead-center.
- **Siri AI's background panel no longer steals a floating window's
  dead-center placement.** Its bundle id (`com.apple.campo`) wasn't
  recognized as system UI, so its transient panel got floated and
  centered like a real app window — landing on the same dead-center spot
  a genuine floating window would otherwise get, and pushing that real
  window into the cascade-offset sequence instead. Added to the same
  ignore-list as the Dock/Spotlight/Notification Center's own transient
  chrome.

## [0.3.0] - 2026-07-21

### Added

- **Mouse-based tile resize.** Dragging a tiled window's real native
  edge/corner (the normal macOS way) now resizes it and its sibling(s) —
  previously only the `resize`/`mode resize` keyboard commands could
  resize a tile at all, and a native drag would just snap back on
  release. Siblings only relayout once, on mouse-up, never live during the
  drag. The released size always snaps to the same grid the keyboard
  `resize <amount>` command uses, via the new `mouse-resize-step` setting
  (default `0.1`) — never an arbitrary off-grid pixel value. Dragging past
  what's actually valid overflows straight to the tree's true max/min
  instead of refusing, same as spamming the keyboard shortcut past its own
  limit; dragging when there's no sibling to trade space with at all
  (alone, or tiled-fullscreen) is simply ignored.

### Fixed

- **Centered floating windows no longer stack exactly on top of each
  other.** Opening several same-sized floating windows in a row used to
  center every one of them at the identical pixel, fully hiding all but
  the topmost. Each newly auto-centered window now gets a small `(dx,
  dy)` nudge, symmetric around dead center (alternating `±step,±step` at
  growing magnitude, wrapping back to dead center every few placements)
  rather than drifting toward one corner — the cluster still reads as
  centered, but each window is individually visible and grabbable.

## [0.2.0] - 2026-07-21

### Changed

- **Replaced the 250ms poll of `frontmost_app_pid()` for
  `WmEvent::FrontmostAppChanged` with a push notification.**
  `NSWorkspaceDidActivateApplicationNotification` had been confirmed dead
  for a `tili-daemon` with no `NSApplication`, same as
  `DidLaunchApplication`/`DidWakeNotification` — now that `main.rs` gives
  the process a real one, `workspace::register_on_main` also registers
  this notification, and `watch.rs`'s tick reacts to it directly
  (`AppEvent::Activated`) instead of polling. Removes the last thing in
  that tick needing a fast, separate cadence — it now shares
  `WATCHER_RESYNC_INTERVAL` (2s) with `resync_watchers` (see below) instead
  of running two different intervals. `frontmost_app_pid()` itself is
  unchanged and still used for its other, on-demand callers
  (`WmState::sync_focus_from_pid`, `reveal_current_frontmost`) — only the
  periodic tick's poll of it is gone. `reveal_frontmost`'s existing guards
  (`pending_launch_pids`/`wake_grace_until` distrust windows, system-UI
  suppression, self-inflicted-focus-change handling) are untouched — they
  guard against the same races regardless of whether the pid arrived via
  poll or push. Confirmed on real hardware that the notification is now
  reliably delivered, the same way `DidLaunchApplication`/
  `DidWakeNotification` were separately confirmed.
- **`watch.rs`'s `resync_watchers` (attach/detach watchers for the current
  app/pid set) now runs on its own 2s `WATCHER_RESYNC_INTERVAL`, not every
  250ms tick.** It used to share the same 250ms cadence as
  `frontmost_app_pid()`'s poll (kept fast for Cmd-Tab responsiveness) back
  when `NSWorkspace` launch/terminate notifications were also unreliable;
  now that those are confirmed reliably delivered (`tili-daemon` has a real
  `NSApplication`), `resync_watchers` is a rare-miss backstop rather than
  the primary detection path, so it doesn't need 250ms responsiveness —
  matches `FULL_RESYNC_DEBOUNCE`'s existing cadence. (The frontmost-pid poll
  itself was still 250ms at the time this landed; see below for its own
  follow-up removal, after which this became the tick's only cadence.)
- **Removed `display.rs`'s resolution-only-change polling fallback.**
  `CGDisplayRegisterReconfigurationCallback` had been confirmed on real
  hardware to never fire for a display-resolution-only change (no monitor
  added/removed), attributed to `tili-daemon` having no `NSApplication` —
  the same underlying cause as the `NSWorkspace` notification gaps fixed
  below. Re-tested on real hardware after that fix: the callback now fires
  reliably for resolution-only changes too, so the polling fallback
  (`RESOLUTION_POLL_INTERVAL`, re-diffing `list_monitors()` every second)
  is gone. `spawn_display_watcher`'s dedicated thread still bounds its
  `CFRunLoopRun` into 1s chunks — that was always to avoid a separate
  run-loop spin-forever bug, not to poll anything, so it stays. This drops
  `display.rs` from the project's sanctioned "no polling" exceptions
  entirely (three remain, down from four — see `invariants.md`).
- **Removed 3 permanently-alive relay threads that existed purely to
  forward events into a `tokio::sync::mpsc` channel.** `tili-daemon`'s
  `spawn_hotkey_bridge`/`spawn_display_watcher_bridge`/
  `spawn_mouse_watcher_bridge` each did nothing but `recv()` a
  `std::sync::mpsc` message and `send()` it into a `tokio::sync::mpsc`
  channel — pure boilerplate. `tili-ax`'s `spawn_hotkey_tap`/
  `spawn_display_watcher`/`spawn_mouse_watcher` now build and send on the
  `tokio::sync::mpsc` channel directly from their own already-existing
  dedicated thread (`tili-ax` already depends on Tokio, and
  `UnboundedSender::send` is a plain synchronous call, legal from any
  thread — the same pattern `tili-ax::watch::spawn_event_watcher` already
  used internally). `tili_config`'s config-reload bridge is unchanged: its
  watcher deliberately stays runtime-agnostic (`std::sync::mpsc`, not
  `tokio`), so it still needs a separate relay thread.
- **`tili-daemon` now creates a real `NSApplication` instance and gives it
  the actual process main thread** (`main.rs`'s `fn main()`, no longer
  `#[tokio::main]` — the whole prior daemon body moved to `async_daemon_main`
  on a background thread with its own Tokio runtime), matching
  `tili-menubar`'s existing pattern. Root-cause fix for
  `NSWorkspaceDidLaunchApplicationNotification`/`DidWakeNotification` being
  silently undelivered to this process (see the `Fixed` entries below) —
  confirmed on real hardware, across multiple independent code comments
  already in this codebase, that several notification-delivery gaps
  (`DidActivateApplication`, and now confirmed `DidLaunchApplication`/
  `DidWakeNotification` too) all traced to `tili-daemon` never having a real
  `NSApplication`. `NSWorkspace` registration moved from its own dedicated
  background `CFRunLoopRun()` thread to the real main thread with
  `queue: .main` delivery (`tili_ax::register_on_main`). **Confirmed fixed
  on real hardware**, across several repeated real sleep/wake cycles and
  app launch/quit cycles: both notifications are now reliably delivered.

### Fixed

- **`NSWorkspaceDidWakeNotification` could silently never reach the daemon
  for an entire real sleep/wake cycle**, confirmed on real hardware with a
  process that stayed alive (same pid) throughout and still never received
  it — meaning `note_system_wake` and everything gated on it (below, and in
  v0.1.18) never ran at all, not just too late. Fixed by the `NSApplication`
  restructuring above. A temporary `SystemTime`-based wall-clock-gap
  backstop (`SLEEP_GAP_THRESHOLD`) was added and tested while diagnosing
  this, then removed once the restructuring was confirmed on real hardware,
  across several repeated sleep/wake cycles, to make the notification
  reliably delivered on its own.
- **`place_new_window`'s `workspace-rules` auto-switch had no wake-grace
  guard at all** — only a log line noting whether wake grace was active,
  never actually skipping the switch. Confirmed on real hardware after the
  `NSApplication` restructuring above: a real sleep/wake cycle (including a
  multi-stage one — a dark-wake followed by a real wake, both triggering
  `note_system_wake`) could still finalize a still-open window as closed
  *before* wake grace activated, then rediscover it moments *after* wake
  grace was active — which reached this switch, not `reveal_frontmost`'s
  (already guarded), producing a visible flicker to the wrong workspace and
  back rather than getting stuck there. `place_new_window` now skips the
  switch (still placing the window into its target workspace, just parked)
  whenever `wake_grace_until` is active, mirroring the guard
  `reveal_frontmost` already had.
- **A still-open app's windows could get force-purged and rediscovered as
  brand new, re-triggering `workspace-rules` and yanking the active
  workspace — with none of the wake-grace protection below, since this path
  bypassed it entirely.** `resync_watchers`' `watched.retain` sent a
  synthetic `WmEvent::AppTerminated` (which routes to `WmState::remove_app`,
  an immediate, ungraced purge of every window tracked for that pid) for any
  pid that dropped out of `current` for even one 250ms tick — but `current`
  is partly sourced from `NSWorkspace.runningApplications()`, which real
  hardware confirmed can transiently omit a still-genuinely-running app's
  pid, most readily right after waking from sleep while the WindowServer/AX
  subsystem is still settling. `watched.retain` now only sends
  `AppTerminated` when the pid is confirmed dead via a kernel-level
  liveness check; a pid that's merely off `current` this tick just loses
  its subscription (silently re-attached once it's back), matching the
  same fix already applied to the `unwatchable` cache below.
- **Waking from sleep could still, up to roughly a minute later, silently
  switch away from whichever workspace was active before sleep** — the part
  of this bug v0.1.18's wake-grace fix didn't cover, plus its grace window
  was confirmed too short on real hardware. `frontmost_app_pid()` (a
  synchronous `AXFocusedApplication` query, polled every 250ms) can itself
  transiently misread which app is frontmost while the WindowServer/AX
  subsystem is still reconnecting after wake; that misread pid still owned
  live windows, so it wasn't caught by `reveal_frontmost`'s existing
  "previous app has zero windows left" suppression, and once its assigned
  workspace wasn't the one visible, `reveal_frontmost` force-switched to it.
  `reveal_frontmost` now also distrusts a `frontmost_app_pid()` read for the
  same wake-grace window `note_system_wake` already established for the
  removal-grace boost. Separately, `WAKE_REMOVAL_GRACE` was raised from 8s
  to 90s: `NSWorkspaceDidWakeNotification` fires at the hardware wake
  itself, which can precede the user actually finishing unlock by close to a
  minute, and apps don't meaningfully resume reconnecting until after that.
- **A system compositor process (WindowServer, Dock) could get retried and
  logged as a failed AX subscription on every single resync tick, forever**,
  flooding the daemon's log. `resync_watchers`' `unwatchable` cache was
  evicting a pid whenever it momentarily stopped owning any on-screen
  window that tick — which a compositor process's menu bar/cursor-layer
  windows do routinely — instead of only when the pid actually exits,
  defeating the cache's whole purpose.

## [0.1.18] - 2026-07-20

### Fixed

- **Floating windows that were never manually dragged/resized snapped back
  to their floating-rule default frame on every workspace switch, not just
  when first opened.** `reposition_floating_for_monitor` unconditionally
  called `place_floating_window` — which recomputes size/position from
  `floating-rules`/`floating-defaults` — for any window with no captured
  `manual` geometry, on every reactivation of its workspace. `WmState` now
  tracks which floating windows have already been placed once
  (`floating_placed`); a window with no manual geometry is left exactly
  where it is on later switches instead of being re-derived from the rule
  each time.
- **Waking from sleep could, a few seconds later, silently switch away from
  whichever workspace was active before sleep.** An app can take several
  seconds to reconnect to the WindowServer/AX after wake — far longer than
  `REMOVAL_GRACE_PERIOD`'s 100ms — so a still-open window could miss one
  scan, get finalized as "closed," then reappear on the next scan and get
  treated as brand new, re-triggering a matching `workspace-rules` entry
  and force-switching the active workspace. tili now listens for
  `NSWorkspaceDidWakeNotification` and temporarily widens the removal grace
  period to 8s right after a real wake, so a merely-slow-to-reconnect
  window isn't misread as closed.

## [0.1.17] - 2026-07-19

### Fixed

- **Finder's "Copy" progress sheet and "Connect to Server" dialog could get
  tiled instead of left alone.** Both windows don't reliably self-report as
  AX dialogs, so `tili_ax::WindowKind`'s structural classification alone
  couldn't be trusted to catch them. tili now protects these two windows
  from tiling unconditionally and by default — no config needed, and no
  `floating-rules` entry (not even one for `com.apple.finder`) can override
  it.

## [0.1.16] - 2026-07-18

### Fixed

- **`tili-daemon`/`tili-menubar` showed a generic icon instead of tili's
  logo in System Settings' Privacy & Security > Accessibility list and
  General > Login Items & Extensions list.** The bundled `tili.app` these
  two run from (see CONTRIBUTING.md's release engineering) never set
  `CFBundleIconFile`/`CFBundleIconName` in its `Info.plist`, and the repo
  had no icon asset at all. `xtask bundle` now converts a committed
  1024x1024 `assets/icon.png` into `Contents/Resources/AppIcon.icns` and
  embeds it — both processes pick it up since they run from the same
  bundle.

## [0.1.15] - 2026-07-18

### Fixed

- **Rapidly switching workspaces in succession, when the sequence ended on
  an empty workspace, could flick back to a previous, non-empty workspace
  a fraction of a second later.** `switch_workspace` raises/focuses a
  window when entering a workspace that already has one, which changes
  real macOS frontmost state — but `last_frontmost_pid` (the bookkeeping
  `reveal_frontmost` uses to tell a genuine Cmd-Tab apart from noise) only
  used to get updated *reactively*, whenever `reveal_frontmost` itself
  next happened to run. If the user hotkeyed onward before the 250ms poll
  thread noticed that self-inflicted frontmost change, `reveal_frontmost`
  read stale bookkeeping, mistook its own recent transition for a fresh
  external switch, and chased the display back to it. `raise_focused`/
  `raise_focused_window` now update `last_frontmost_pid` synchronously at
  the moment they focus a window, so a later, self-caused poll detection
  is recognized as already-accounted-for. Two smaller hardening changes
  ride along: `switch_workspace` now tracks a `switch_epoch` so a deferred
  reveal armed before a newer, explicit workspace switch drops instead of
  firing stale; and `reveal_frontmost` only chases a same-pid read when
  the trigger was a real click (not a poll edge), closing a related case
  where a windowless system process (WindowServer, Dock) transiently and
  spuriously becoming AX-frontmost during a switch could poison the same
  bookkeeping.

## [0.1.14] - 2026-07-18

### Fixed

- **Opening and focusing a new window, then switching workspaces away and
  back, could restore focus to whichever window was focused there before
  instead of the new one** — until the next unrelated real focus change
  happened to fix it. A window that's already real-OS-focused the instant
  it's created can win the race against `apply_windows_changed` itself:
  `sync_focus_from_pid` resolves the focused window via a live AX query
  first, then looks it up in `self.placements`, which doesn't have an entry
  yet for a window that function hasn't finished registering — so the sync
  silently no-op'd with nothing to retry it. `apply_windows_changed` now
  re-runs `sync_focus_from_pid` once after placing any brand-new window,
  once its own placement is guaranteed to exist.

- **Quitting the focused app in a still-visible workspace that had another
  window could hand real keyboard focus to an unrelated app on a different
  (possibly parked) workspace, instead of the remaining window right there.**
  `remove_from_tree` already reassigned `workspace_focus` internally when the
  removed window was its workspace's recorded focus, but never made real
  macOS focus follow — leaving the OS's own, tili-oblivious app-reactivation
  history (typically whatever was frontmost before the quit app) as the de
  facto focus. `remove_placement` now re-raises the reassigned window for
  real whenever its workspace is still on screen.

## [0.1.13] - 2026-07-18

### Added

- **Moving a window to another workspace — via `move-node-to-workspace`, or
  automatically because it matches a `workspace-rules` entry when it's
  first created — now switches the display to that workspace**, instead of
  leaving the window parked off-screen until a later, unrelated switch.
  `move_focused_to_workspace` and `place_new_window` call `switch_workspace`
  directly rather than only parking/resyncing.

### Fixed

- **Opening an app via a Dock click or Spotlight while on an empty
  workspace could briefly switch the display to the wrong workspace before
  landing on the right one.** A cold app launch leaves a short window where
  `frontmost_app_pid()` still reports the previously-frontmost app;
  `reveal_frontmost` could act on that stale read and switch away to
  wherever that unrelated app lived. Its workspace-switch is now deferred a
  short, bounded interval (`REVEAL_DEBOUNCE`) and guarded by
  `pending_launch_pids`, a `WmEvent::AppLaunched`-keyed tracker — covering
  both paths into `reveal_frontmost` (`reveal_current_frontmost`'s
  click-driven fallback and `WmEvent::FrontmostAppChanged`'s direct call).

## [0.1.12] - 2026-07-18

A fix release addressing six issues found while daily-driving floating
windows and the menu bar badge after `v0.1.11`.

### Fixed

- **A newly-opened window (System Settings, most reliably) occasionally
  got tiled instead of floated, permanently, until closed and reopened.**
  `apply_windows_changed` resolves a brand-new window's tiled/floating
  disposition exactly once, and the floating-rule matcher needs
  `AxWindow::bundle_id()` — resolved via `NSRunningApplication`, with no
  retry — to be populated. If the window's process had only just launched
  and `NSRunningApplication` hadn't registered it yet at that exact
  moment, the matcher silently read as "no rule configured" instead of
  "couldn't check yet," falling through to the kind-based `Tile` default.
  Disposition is now deferred (bounded by `MAX_BUNDLE_ID_RETRIES`) instead
  of resolved against an unresolved bundle id, riding the next
  `WindowsChanged` event or the existing 20s full-resync safety net.

- **A hotkey bound to move the focused window to another workspace moved
  the wrong window whenever the actually-focused window was floating.**
  `workspace_focus` (and the 11 other commands keyed off it — resize,
  join, orientation, layout, balance, close, ...) could only ever point at
  a tiled `tili_tree::Tree` node; floating windows lived entirely outside
  any `Tree`, so clicking one didn't update what the daemon considered
  "focused," leaving stale tiled-window state behind for these commands to
  act on. Floating windows are now `Node::Floating` leaves in the same
  tree as tiled ones — addressable via `workspace_focus`/`focused_node()`
  like any tiled window, but excluded from `Tiles`/`Accordion` sizing and
  skipped by directional navigation. Commands that are inherently
  tiled-only now error clearly instead of silently hitting the wrong
  window when the real focus is floating.

- **A floating window switching away from its workspace flickered
  visibly instead of parking cleanly out of sight.** `park()` used to
  offset each additional simultaneously-parked window inward by a step so
  multiple windows parked at once wouldn't share the exact same
  coordinate — but `tili_ax::parking_position`'s "hidden regardless of
  size" guarantee only holds with the window's origin sitting exactly
  `PARK_EPSILON` inside the monitor's corner; any inward offset exposes a
  real on-screen strip as wide as the shift. A second, independent bug
  (`reconcile_existing_placement` re-parking at a hardcoded offset of `0`
  regardless of what a window was actually parked at) had been
  accidentally self-correcting this for months by snapping every parked
  window back to the true corner roughly 30ms later; fixing that bug on
  its own is what surfaced this one. Removed the offsetting entirely —
  every parked window now targets the identical hidden coordinate, since
  nothing actually needs them spread apart.

- **The menu bar badge could get stuck showing a stale workspace when a
  workspace-switch hotkey was pressed rapidly.** `tokio::spawn` deferred
  `Notify::notified()` until the spawned task actually started running,
  so a `notify_waiters()` firing in the gap between spawning and that
  first poll was silently missed. Fixed via `Notify::notified_owned()`,
  which snapshots the wakeup baseline synchronously before the task is
  ever spawned.

- **The menu bar badge stayed blank on a freshly-started daemon until the
  user made some change (e.g. a workspace switch).** The background
  poller went straight into blocking on `Command::WaitForChange`, which
  doesn't unblock until something actually changes or its own 30s idle
  timeout fires — so a quiet daemon left the badge in its initial hidden
  state that whole time. It now polls once, unconditionally, before ever
  waiting.

- **The menu bar's long-poll design degraded into a continuously-spinning
  loop shortly after the first real change of a session.** Read-only
  queries (`Ping`/`ListWindows`/`ListWorkspaces`/`ListMonitors`) counted
  as "changed" on the daemon's socket handler the same as any mutating
  command — harmless for occasional CLI use, but the menu bar's own
  poller calls two of those on every single wakeup, so each wakeup
  re-notified the very `WaitForChange` connection it had just
  re-subscribed. Only commands that can actually mutate state count as a
  change now.

A ~1s delay between clicking a workspace item in the menu bar's dropdown
and the switch happening is still open — ruled out IPC/socket overhead,
App Nap, and the two menu-bar fixes above; narrowed to AppKit's own
`NSMenuItem` target-action dispatch, undiagnosable further without a
profiler attached to a running process. See
`docs/architecture/tili-menubar.md`'s "Known issue" section.

## [0.1.11] - 2026-07-17

A fix release addressing two issues found after `v0.1.10`.

### Fixed

- **Activating an app that already had a window on a different workspace
  (via Spotlight, or via Dock when the current workspace had no monitor)
  changed focus to that app without switching to its workspace; dismissing
  a notification banner's close button could conversely cause a spurious
  jump/settle-back to a previous workspace.** `v0.1.9` taught
  `WmState::reveal_frontmost` to exclude `Popup` windows from its "does
  the previously-frontmost pid still own a live window" check, to stop
  Spotlight's still-open search panel from defeating the OS-reactivation
  suppression added in `v0.1.8`. But Spotlight and the Dock only ever own
  `Popup` windows, so that check now read as "owns nothing" for *every*
  transition away from them — suppressing the workspace switch whether
  the user dismissed them passively (correct) or deliberately picked a
  result/icon to switch to (wrong; both produced the same signal).
  Notification Center's banner had the opposite problem: it wasn't forced
  into `Popup` at all, leaving it exposed to the exact race the `v0.1.9`
  exclusion was meant to close (now covered by `SYSTEM_UI_BUNDLE_IDS`
  alongside Spotlight). `reveal_frontmost` no longer tries to distinguish a
  deliberate switch from a passive dismissal when the previous pid was one
  of these helpers — it always follows. A pid-history-based attempt at
  telling the two apart correctly handled the passive case but went stale
  (and suppressed every later reactivation of the same app) whenever an
  in-between workspace switch never actually changed the OS-level
  frontmost app, which is indistinguishable from this bug's own repro at
  the AX level. Accepted trade-off: dismissing one of these helpers over a
  literally empty workspace can once again produce the narrower `v0.1.9`
  symptom (a one-frame jump before settling back).

- **A Dock icon click for an app that already had a window on a different
  workspace never revealed it, unlike the same activation via Spotlight.**
  This turned out to be a different bug from the one above, confirmed via a
  diagnostic build's logs: `Dock.app` never becomes the AX/`NSWorkspace`
  frontmost application while handling an icon click the way `Spotlight.app`
  does while its panel is open, so if the clicked app was already the OS's
  nominal frontmost app (the common case when the current workspace is
  empty), `frontmost_app_pid()` read identically before and after the
  click — no pid edge for `WmEvent::FrontmostAppChanged` to ever fire on,
  so `WmState::reveal_frontmost` never ran at all, at any polling interval.
  `WmState::reveal_current_frontmost` now runs the same reveal logic on
  every `MouseSignal::ButtonUp` (a real `CGEventTap` signal a Dock click
  always produces) against whatever's frontmost *right now*, regardless of
  whether it changed. `reveal_frontmost` treats an unchanged pid that was
  already fully visible as a true no-op, so an ordinary click that isn't
  reactivating anything costs one extra AX query and nothing more.

## [0.1.10] - 2026-07-17

A fix release addressing one issue found after `v0.1.9`.

### Fixed

- **Moving a window past a sibling that had previously been resized
  unevenly could silently reassign sizes instead of moving with the
  window.** `Tree::move_within`'s swap only reordered a container's
  `children`, not its parallel `weights` array, so after an uneven
  `resize_weight` a subsequent `move_in_direction` bound each window to
  whatever weight already sat at its new array position rather than
  carrying its own weight along. The swap now moves both arrays together.

## [0.1.9] - 2026-07-16

A fix release addressing one issue found after `v0.1.8`.

### Fixed

- **Opening Spotlight over an empty workspace and dismissing it with Esc
  could briefly jump the display to a different workspace before
  settling back.** `v0.1.8` taught `WmState::reveal_frontmost` to skip a
  workspace switch when the previously-frontmost pid no longer owns any
  live window, to tell a real Cmd-Tab apart from macOS reactivating the
  prior app after the current one closes its last window. Spotlight's
  search panel is tracked like any other window and lands in whichever
  workspace is active when it opens, but as AX-ambiguous chrome with no
  close button it's classified `Popup` — a kind the "still owns a live
  window" check didn't distinguish from a real window. Closing Spotlight
  could therefore still read as "the previous pid is still alive," defeat
  the suppression, and briefly reveal wherever that pid's real workspace
  was. The check now excludes `Popup` placements specifically, leaving
  `Minimized`/`NativeFullscreen`/`HiddenApplication` (real, still-open
  windows in a special display state) counted as before.

## [0.1.8] - 2026-07-16

A fix release addressing one issue found after `v0.1.7`.

### Fixed

- **Closing the last window on a non-default workspace silently jumped
  the display back to the default workspace.** macOS reactivates
  whichever app was previously frontmost when the current frontmost app
  closes its last window — the same kind of frontmost-app-changed signal
  `tili-daemon` uses to follow a real Cmd-Tab/Mission-Control switch onto
  a parked workspace (`WmState::reveal_frontmost`). With no way to tell
  the two apart from the OS-level pid change alone, tili blindly followed
  the reactivation too, most often landing back on whichever workspace
  was active before the user ever switched away — commonly the default
  one. `reveal_frontmost` now tracks the previously-seen frontmost pid and
  skips the workspace switch when that pid no longer owns any live
  window, since that's a reliable sign of this OS-driven reactivation
  rather than a genuine user-initiated app switch.

## [0.1.7] - 2026-07-16

A fix release addressing one issue found after `v0.1.6`.

### Fixed

- **`tili-daemon` kept ~40% of a CPU core busy at idle, even with zero
  window/workspace activity, from the moment it started.** `spawn_display_watcher`'s
  fallback poll loop calls `CFRunLoop::run_in_mode(..., 1s, false)` expecting
  it to block for close to a second each iteration, then re-checks
  `list_monitors()`. But `CFRunLoopRunInMode` returns immediately — a
  documented CoreFoundation behavior — whenever the calling thread's run
  loop has no input source or timer registered in that mode, which is the
  case here since `CGDisplayRegisterReconfigurationCallback`'s delivery
  mechanism doesn't register one. Confirmed via real CPU sampling: only
  33 of 5955 samples (0.5%) over an 8-second window were inside
  `run_in_mode` itself, while `list_monitors()` — several synchronous
  `mach_msg` round-trips to WindowServer (`SLGetActiveDisplayList`,
  `SLDisplayBounds`, `SLSMainDisplayID`) per call — accounted for the rest,
  running hundreds of times a second instead of once. The loop now
  explicitly sleeps out whatever's left of `RESOLUTION_POLL_INTERVAL`
  after each `run_in_mode` call, capping the fallback poll at its intended
  1Hz regardless of how fast `run_in_mode` returns. The real-time hot-plug/
  sleep-wake path (`reconfiguration_callback` sending directly on `tx`) is
  untouched and still fires instantly. Re-sampled after the fix: 99.97% of
  the same thread's samples now fall inside `nanosleep`, and measured
  daemon-wide average CPU dropped from ~40% to ~0.5% on real hardware.

## [0.1.6] - 2026-07-16

A fix release addressing one issue reported after `v0.1.5`.

### Fixed

- **Closing and reopening a MacBook's lid (system sleep/wake) could strand
  the active workspace and spawn a fresh, empty `monitor-<id>` workspace
  instead of restoring the one that was showing before sleep.**
  `on_displays_changed` committed `list_monitors()`'s result to
  `self.monitors` unconditionally on every raw
  `CGDisplayRegisterReconfigurationCallback` signal — including a momentary
  zero-display enumeration the callback fires as the system sleeps (the
  built-in display "disconnecting" gets processed and committed before the
  process itself is suspended for the sleep's actual duration, however long
  that turns out to be). By the time the display reappeared on wake,
  `self.monitors` had already been wiped, leaving nothing for
  `match_monitors`'s origin-distance rename-pairing (built specifically to
  recognize "same physical display, new `CGDirectDisplayID`" across
  sleep/wake) to compare against — the reconnect was processed as a
  brand-new monitor instead. `on_displays_changed` now returns immediately
  on a fully empty enumeration without touching `self.monitors`, since
  there's nothing to lay out with zero displays anyway; the pre-sleep
  snapshot survives intact for however long the sleep lasts, so the
  eventual wake-time call diffs genuine before/after state and restores the
  correct workspace.

## [0.1.5] - 2026-07-16

A fix release addressing two issues found after `v0.1.4`, plus a release-time
safeguard against the first one recurring.

### Fixed

- **`tili --version` always reported `0.1.0`, no matter which version was
  actually installed.** `[workspace.package] version` in the root
  `Cargo.toml` — the source of `CARGO_PKG_VERSION`, which clap embeds into
  the CLI's `--version` output at compile time — had never been bumped past
  its initial scaffold value, even though `v0.1.1` through `v0.1.4` all
  shipped. The version used for release tarball naming and packaging
  (`xtask package --version <tag>`) came from the pushed git tag, entirely
  decoupled from this field, so nothing ever caught the drift. Bumped to
  match this release; `xtask bundle`/`package` now also refuse to build if
  the release tag doesn't match `Cargo.toml`'s version, so a forgotten bump
  fails the release instead of silently shipping a stale version string.
- **`tili -v` didn't work** — only `--version` (and the clap-default `-V`)
  did. Added `-v` as an explicit alias.
- **Upgrading via `brew upgrade` never actually restarted a running
  `tili-daemon`/`tili-menubar`**, despite `post_install` existing
  specifically to do that. `post_install` runs inside Homebrew's install
  sandbox, which fakes `$HOME`/`Dir.home` to a throwaway temp directory —
  so the "is tili already running" check always saw a nonexistent path and
  silently returned early — and separately denies filesystem writes outside
  the Cellar/temp/log dirs, so even a corrected check would then fail to
  rewrite the LaunchAgent plist `tili stop`/`tili start` needs. `post_install`
  now resolves the real home via `Dir.home(ENV.fetch("USER"))` (bypassing
  the faked `$HOME` env var, the same trick Homebrew's own sandbox code
  uses) for the read-only existence check, and restarts by signaling the
  running processes directly (`pkill -x`) rather than rewriting any file —
  launchd's `KeepAlive` relaunches them immediately through the
  `bin/tili-daemon`/`bin/tili-menubar` symlinks, which Homebrew has already
  relinked to the new version by the time `post_install` runs. Confirmed on
  real hardware (a live `tili-daemon`/`tili-menubar` restarting with new
  PIDs under the actual Homebrew sandbox, not just a local `cargo run`).

## [0.1.4] - 2026-07-16

A fix release addressing one issue found after `v0.1.3`.

### Fixed

- **Config hot-reload never fired when `~/.config/tili/tili.kdl` was a
  symlink** (e.g. a dotfiles repo managed via stow/chezmoi/a plain `ln -s`).
  `spawn_config_watcher` watched the literal config path's parent
  directory, but a write to the real file (elsewhere, through the symlink)
  never touches that directory, so no filesystem event ever arrived —
  edits only took effect after a manual `tili stop`/`tili start`. It now
  resolves the config path with `std::fs::canonicalize` first and watches
  the real target's directory instead.

## [0.1.3] - 2026-07-16

A fix release addressing two issues found after `v0.1.2`, plus a small menu
bar polish.

### Fixed

- **A resolution-only display change (no monitor plugged/unplugged) never
  triggered a relayout.** `CGDisplayRegisterReconfigurationCallback`
  reliably fires for hot-plug/unplug and sleep/wake, but confirmed on real
  hardware to never fire for a pure resolution change in this process
  (`tili-daemon` has no `NSApplication`/UI-session-activation context).
  `spawn_display_watcher` now also bounds its run loop into 1-second
  chunks and re-diffs `list_monitors()` after every wake, catching the
  resolution change within about a second — the third documented,
  narrowly-scoped exception to the "no polling" invariant (see
  `CLAUDE.md`).
- **Switching to a workspace with no windows could silently revert back to
  the previous one a moment later.** Parking a still-real-macOS-frontmost
  window into its barely-on-screen corner sliver made
  `frontmost_app_pid()` transiently read `None` for one 250ms tick, even
  though no other app actually took focus. That `None` was overwriting the
  tracker `FrontmostAppChanged` compares against, so the very next tick
  reading the same (never-actually-changed) app looked like a fresh
  Cmd-Tab and fired the event again — which reveals a parked workspace,
  yanking the display right back. The tracker is now only ever updated on
  an actual resolved frontmost app, so a transient `None` can no longer
  reset the baseline a real app-to-app switch is compared against.

### Changed

- The menu bar's active-workspace indicator is now a leading "•" instead
  of a native checkmark, and "Quit" now reads "Quit tili" for clarity.

## [0.1.2] - 2026-07-15

A fix release addressing one issue found after `v0.1.1`.

### Fixed

- **`brew upgrade tili` left the old daemon/menu bar running until a manual
  restart.** Homebrew swaps the installed binaries in place, but a
  LaunchAgent already running keeps its old process image loaded — an
  upgrade silently had no effect until the user remembered to run
  `tili stop && tili start` themselves. The formula's `post_install` now
  detects an already-running daemon (its LaunchAgent plist present) and
  restarts it automatically, so `brew upgrade` takes effect immediately. A
  fresh `brew install` is unaffected — it still leaves the first
  `tili start` to the user.

## [0.1.1] - 2026-07-15

A fix release addressing four issues found in daily use of `v0.1.0`.

### Fixed

- **Cmd-Tab / Mission Control / Control Center couldn't switch to an app in
  a parked workspace.** Switching real macOS focus to an app whose window
  lived in a currently inactive (parked, off-screen) workspace made that
  app frontmost, but tili never revealed its window — it stayed off-screen
  until an unrelated `workspace`/`focus` command happened to bring it back.
  tili now detects the frontmost-app change (checked on the existing 250ms
  reconciliation tick, not a new poll) and immediately switches to/reveals
  the owning workspace, matching whichever monitor already shows it if it's
  visible elsewhere.
- **A pre-existing app quitting could leave a permanent "ghost" tile.** An
  app that was already running before `tili-daemon` started, and later quit
  while backgrounded with no open windows, could in rare cases leave a
  stale tile behind forever — both the primary termination notification and
  its existing reconciliation-tick backstop are sourced from `NSWorkspace`,
  so the two could go stale together for that specific case. Detection now
  also cross-checks a process's liveness at the kernel level
  (`kill(pid, 0)`), independent of `NSWorkspace`, closing that gap.
- **Transient system UI was misdetected as a real window and moved/resized.**
  Right-clicking the Trash in the Dock, the thumbnail preview shown after
  taking a screenshot, and macOS's keychain "always allow" authorization
  prompt could each get tiled or re-centered as if they were a real
  application window. Window classification now also checks whether the
  owning process is a regular, Dock-visible application and whether the
  window has a close button — system-UI chrome (no Dock icon, no close
  button) is now always left untouched, regardless of what AX role/subrole
  it happens to report.
- Also confirmed, and deliberately left as-is: floating windows can briefly
  flash at their app/OS-assigned default frame before tili repositions them
  to their configured floating frame. This is an inherent limitation of any
  window manager that reacts to window creation via Accessibility
  notifications (there's no way to intercept a window before its first
  paint), not a defect in tili's own placement logic — floating windows are
  never tiled first; see `place_floating_window`'s doc comment.

## [0.1.0] - 2026-07-15

First public release.

### Added

- **Tiling.** BSP-style tiling (`tili-tree`'s n-ary container tree) with
  i3-style directional `focus`/`move`, `join` (wrap two windows into a new
  perpendicular container), `resize`, `layout horizontal`/`vertical`, and
  `balance`. Accordion containers (`layout toggle`/`layout accordion`)
  stack windows with a configurable peek padding instead of splitting
  screen space.
- **Workspaces.** Named workspaces declared in config, switched via
  `workspace <name>`, `workspace-back` to return to the previous one, and
  `move-to-workspace`/`move-workspace-to-monitor` to relocate windows and
  whole workspaces without following them.
- **Multi-monitor.** Each connected display shows its own workspace
  independently; `focus-monitor` cycles which one commands target;
  hot-plug/unplug parks or restores workspaces automatically.
- **Config.** KDL config at `~/.config/tili/tili.kdl` with file-watch
  hot-reload — edits apply live, a parse error is logged and the previous
  config keeps running. Covers workspaces, gaps (global + per-workspace),
  default layout/orientation, keybindings, and floating rules.
- **Built-in global hotkeys.** No external tool required — a `CGEventTap`
  captures and dispatches bound keys directly, with keybinding modes
  (e.g. a `resize` mode) switchable at runtime.
- **Floating rules.** Match windows by app id / title regex and
  auto-center/size them on creation, plus a runtime `set-floating`
  toggle and a per-rule `mode` override (tile/float/ignore).
- **Workspace rules.** A standalone `workspace-rules` section that always
  creates a matching app's windows on a specific declared workspace
  instead of wherever's active — independent of `floating-rules`, so it
  applies the same whether the window ends up tiled or floating.
- **Mouse integration.** `mouse-follows-focus` (cursor warps to the
  newly-focused window) and `focus-follows-monitor` (moving the cursor to
  another display retargets which monitor commands act on).
- **Fullscreen and window control.** `fullscreen` (tiled or native
  macOS), `close`, `summon <query>` to find-and-raise a window by
  title/bundle id.
- **Menu bar badge (`tili-menubar`).** A `NSStatusItem` showing the
  active workspace as a knockout-text pill, with a dropdown to switch
  workspaces, open the config file, or quit — stays in sync with the
  daemon via a long-poll (`Command::WaitForChange`), not polling.
- **Lifecycle management.** `tili start`/`stop`/`uninstall` install,
  remove, and fully tear down both the daemon's and the menu bar badge's
  LaunchAgents — `uninstall` also removes the config, logs, IPC socket,
  and resets the Accessibility permission grant, so nothing is left for
  the user to clean up by hand.
- **Signed releases.** `xtask` builds and codesigns a real `tili.app`
  bundle with a stable self-signed identity; `release.yml` builds
  aarch64/x86_64 binaries and opens a draft GitHub release per tag.

### Notes

- Building `tili-daemon` requires full Xcode (not just Command Line
  Tools) — `axuielement`'s safe API links a Swift runtime bridge. See
  CONTRIBUTING.md.
