# Roadmap

## v0.1.0 — first release

tili shipped its first public release with a complete, daily-drivable
feature set:

- BSP tiling (`Tiles`) and Accordion layouts, with i3-style directional
  focus/move, join, resize, and balance.
- Named workspaces, off-screen virtualization, and full multi-monitor
  support (hot-plug/unplug safe).
- KDL config with file-watch hot-reload — no daemon restart needed.
- Built-in global hotkeys, with switchable keybinding modes.
- Floating rules (auto-center/size on creation, runtime tile/float
  toggle, per-rule mode override).
- Workspace rules — auto-assign an app to a specific workspace on
  creation, independent of whether it tiles or floats.
- Mouse-follows-focus and focus-follows-monitor.
- Native and tiled fullscreen, window close, and `summon` (find-and-raise
  by title/bundle id).
- A menu bar workspace badge (`tili-menubar`) that stays in sync with the
  daemon over an event-driven long-poll, not polling.
- LaunchAgent-managed `start`/`stop`/`uninstall`, the latter leaving
  nothing behind — config, logs, socket, and the Accessibility grant are
  all cleaned up automatically.
- A real, signed release pipeline (`xtask` + a stable self-signed
  identity + Homebrew tap).

See [CHANGELOG.md](CHANGELOG.md) for the full itemized list.

## Planned

Nothing here is scheduled — this is the backlog of ideas judged worth
doing eventually, roughly in the order they'd likely land:

- **Animated window movement.** `WindowFrameSetter` (`tili-ax`) is
  already designed as the single seam every real frame mutation goes
  through, specifically so an animated implementation
  (`TweenedFrameSetter`) can be dropped in later without touching layout
  code.
- **Third-party status bar integration (e.g. SketchyBar).** Right now
  only `tili-menubar` can show live workspace state, via an in-process
  long-poll. Two directions are already scoped from building that:
  a `tili subscribe` push protocol external tools can connect to over
  the existing socket, or a simpler synchronous exec-hook
  (`on-workspace-change` calling out to a user script) for tools that
  would rather not speak tili's protocol at all.
- **Tabbed/stacked containers.** A third container kind between `Tiles`
  and `Accordion` — tabs instead of full-window stacking.
- **Sticky windows.** Windows that stay visible across workspace
  switches instead of parking.
- **Native-tab support.** Deferred until macOS's native-tab semantics
  can be relied on consistently across apps.

## Design principles

- **Public API only.** One documented private call (`_AXUIElementGetWindow`)
  to resolve a window's `CGWindowID` — everything else is public
  Accessibility API. No SIP disable, ever.
- **Event-driven, not polling.** Every change to the daemon's event loop
  should be checked against idle CPU usage, not just correctness.
- **The animation seam stays a seam.** `WindowFrameSetter` is the only
  thing allowed to know how a window's frame actually gets set. If a
  feature needs to reach around it, that's a design smell worth stopping
  for.

See [docs/ARCHITECTURE.md](docs/ARCHITECTURE.md) for the full technical
design, and [docs/BLUEPRINT.md](docs/BLUEPRINT.md) for the design
reference behind the planned items above.
