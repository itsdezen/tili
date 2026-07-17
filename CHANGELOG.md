# Changelog

All notable changes to this project are documented here. Format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/).

**Versioning (pre-1.0):** plain SemVer — minor bumps ship new features,
patch bumps are fixes. This resets to standard SemVer conventions at v1.0.

## [Unreleased]

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
