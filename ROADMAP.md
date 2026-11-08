# Roadmap

## Shipped

- **v0.15.0** — native-tab guard fixes for Terminal's tab-bar resize and the
  2→1 close, new-window entry animation, configurable animation duration
  (120ms default), and immediate placement on window creation.
- **v0.14.0** — absolute floating sizes and the rounded menu bar default.
- **v0.13.0** — home-display workspace routing, global southeast parking,
  JSON query/subscribe, window metadata and rule ergonomics, native-tab
  guard, `toggle-floating`, and configurable AX timeout.
- **v0.12.0** — session state and recovery.
- Earlier releases — see [CHANGELOG.md](CHANGELOG.md).

## Planned

- **Tabbed/stacked containers.** Third container kind; design spike first.
- **Full native-tab support.** Only if the guard proves insufficient;
  depends on reliable AX tab semantics.
- **Sticky windows.** Stay visible across workspace switches.

## Design principles

- **Public API only.** One documented private call (`_AXUIElementGetWindow`)
  to resolve a window's `CGWindowID` — everything else is public
  Accessibility API. No SIP disable, ever.
- **Event-driven, not polling.** Every change to the daemon's event loop
  should be checked against idle CPU usage, not just correctness.
- **The animation seam stays a seam.** `WindowFrameSetter` is the only
  thing allowed to know how a window's frame actually gets set — now
  including `TweenedFrameSetter` itself (behind `Settings::animate`), not
  just its `InstantFrameSetter` predecessor. If a feature needs to reach
  around it, that's a design smell worth stopping for.

See [docs/ARCHITECTURE.md](docs/ARCHITECTURE.md) for the full technical
design, and [docs/BLUEPRINT.md](docs/BLUEPRINT.md) for the design
reference behind the planned items above.
