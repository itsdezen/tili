# Roadmap

## In progress — v0.13.0

- **Global southeast parking and workspace routing.** All parked windows
  share the southeast-most display's bottom-right corner and are re-parked
  after display changes. Every display gets an active workspace;
  `monitor="..."` pins are honored; fallback workspaces use display names.
  Workspaces have a home display: `workspace X` goes there,
  `workspace-here X` pulls it to the focused display, and
  `move-workspace-to-monitor next|prev|main` relocates it. `workspace-back`
  restores the previous display+workspace pair.
- **`tili query` + `tili subscribe --json`.** JSON snapshots and
  line-delimited events for status bars and scripts.
- **Window metadata and rule ergonomics.** `bundle_id`, `workspace`,
  `focused`, `role`, `subrole`; copy-rule CLI/menu bar; `role`/`subrole`
  matchers for floating and workspace rules.
- **Native-tab guard.** Tab open/close/switch is opaque to tili: one
  managed window, no placement or relayout churn. Full native-tab support
  remains future work.
- **Floating toggle.** `toggle-floating` command and default hotkey.
- **Responsiveness.** Configurable AX timeout, 500ms default.

## Shipped

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
