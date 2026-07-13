# Roadmap

tili is built in small, independently verifiable milestones. Each one ships
something you can actually run and check — no milestone is "trust me, it
works."

Status legend: ✅ done · 🚧 in progress · ⬜ not started

| # | Milestone | Status | Verification |
|---|-----------|:---:|---|
| M0 | Workspace scaffolding | ✅ | `cargo build --workspace` succeeds; daemon triggers the Accessibility permission dialog |
| M1 | Read-only window listing | ✅ | `tili list-windows` over a real socket shows real running windows |
| M2 | Event-driven updates | ✅ | Window list stays live via AXObserver/NSWorkspace, no polling; near-zero idle CPU |
| M3 | Single-workspace Tiles layout | ⬜ | `tili focus/move <dir>` works with real BSP tiling on one monitor |
| M4 | Named workspaces + virtualization | ⬜ | Off-screen parking confirmed via `tili list-windows --json` |
| M5 | KDL config + hot-reload | ⬜ | Editing `tili.kdl` and saving updates gaps live, no restart |
| M6 | Built-in hotkey handling | ⬜ | Rebinding a key in config works with no external hotkey daemon |
| M7 | Accordion layout + toggle | ⬜ | `layout toggle` switches Tiles ↔ Accordion on a live workspace |
| M8 | Floating rules + auto-center | ⬜ | ~30 floating-app rules match and auto-center/size on window creation |
| M9 | Multi-monitor support | ⬜ | Hot-plug/unplug reassigns workspaces without losing windows |
| M10 | Daily-drivable MVP | ⬜ | Used as the only WM for one full workday, no manual intervention |
| M11 | Release engineering | ⬜ | `brew install` → `brew upgrade` does **not** reset Accessibility permission |

Post-v1 (deferred on purpose): animated window movement (`TweenedFrameSetter`),
tabbed/stacked containers, `tili subscribe` event streaming for status-bar
integrations, per-app default-workspace rules.

## Design principles guiding every milestone

- **Public API only.** One documented private call (`_AXUIElementGetWindow`)
  to resolve a window's `CGWindowID` — everything else is public Accessibility
  API. No SIP disable, ever.
- **Event-driven, not polling.** Every milestone that touches the daemon's
  event loop should be checked against idle CPU usage, not just correctness.
- **The animation seam stays a seam.** `WindowFrameSetter` is the only thing
  that's allowed to know how a window's frame actually gets set. If a
  milestone needs to reach around it, that's a design smell worth stopping for.

See the architecture notes for the full technical design behind each
milestone.
