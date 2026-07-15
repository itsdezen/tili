# Changelog

All notable changes to this project are documented here. Format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/).

**Versioning (pre-1.0):** plain SemVer — minor bumps ship new features,
patch bumps are fixes. This resets to standard SemVer conventions at v1.0.

## [Unreleased]

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
