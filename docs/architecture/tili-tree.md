# tili-tree — container tree and layout

Part of the [architecture notes](../ARCHITECTURE.md).

The container tree and layout algorithms (Tiles/BSP, Accordion). No
`AXUIElement`, no CoreFoundation, no `unsafe`. Everything operates on plain
`Rect`/`WindowId` (a `u32` newtype around the real `CGWindowID`) so it's
fully unit-testable without macOS — see `src/tree.rs`'s test module for the
actual coverage (insert/remove with parent-split collapsing, i3-style
direction `navigate`, `move`'s window-identity `swap_windows`, proportional
`layout`, Accordion toggle/cycle/wrap).

- `insert_window` always wraps the target leaf in a fresh 2-child `Split`
  rather than flattening into an existing same-orientation split — a
  deliberate M3 simplification (still a valid, correctly-tiling tree; just
  not the shallowest possible one — also means a "flat" Accordion built via
  sequential inserts only ever has 2 children, see the M7 accordion tests).
- `layout(area, gaps)` takes a `Gaps` (outer padding around the whole area,
  inner spacing between siblings, both `f64` — `tili-config`'s parsed `u32`
  gaps get converted at the `tili-daemon` boundary since this crate can't
  depend on `tili-config`).
- `toggle_layout(from)` (M7) converts `from`'s parent container between
  `Split` and `Accordion` in place — converting *to* Accordion sets `active`
  to `from`'s own position so the currently-visible window doesn't change.
- `focus_in_direction(from, dir)` is the Accordion-aware navigation entry
  point `WmState` actually calls (not plain `navigate`): if `from`'s parent
  is an `Accordion`, `dir` cycles (and wraps at the ends) which child is
  active instead of doing spatial `Split` navigation, since a stack of
  fully-overlapping children has no inherent left/right/up/down axis.
- `apply_resize`'s `MIN_WEIGHT` floor is purely proportional (a share of a
  container's weight total), with no relationship to real pixels or any
  app's actual minimum window size — this crate has no `AXUIElement`
  dependency and so has no way to know one. A tiled window resized past
  its app's real minimum will overflow its assigned rect; this is a known
  OS/app-level limitation, not something the layout engine tracks or
  corrects.
