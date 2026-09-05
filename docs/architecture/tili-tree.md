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
  depend on `tili-config`). `Gaps.outer_solo` (`Option<(f64,f64,f64,f64)>`)
  overrides `outer` when `tiled_window_ids().len() == 1` — resolved by
  `effective_outer`, shared by `layout` and `resize_handle_at` so the two
  stay geometrically consistent. `None` (the default) is a no-op, always
  falling back to `outer`.
- `toggle_layout(from)` (M7) converts `from`'s parent container between
  `Split` and `Accordion` in place — converting *to* Accordion sets `active`
  to `from`'s own position so the currently-visible window doesn't change.
- A perpendicular `move` at a two-child workspace root replaces that root
  along the requested axis while preserving its layout kind. Larger trees
  still gain a `Tiles` outer container so the move does not implicitly nest
  the existing layout inside a new Accordion.
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
- `resize_delta_bounds(from)` exposes the same `MIN_WEIGHT` clamp
  `apply_resize` enforces internally, but read-only — `apply_resize` is
  now a thin wrapper around it, so the two can't drift apart. Lets a caller
  pick a `delta` that's already guaranteed valid instead of getting a
  silently-truncated one back; `tili-daemon`'s mouse-drag resize uses this
  to pick a step-quantized delta that's still within bounds, rather than
  quantizing first and clamping into an off-grid value after the fact.
- `resize_handle_at(area, gaps, point)` is the mouse-resize counterpart to
  `layout`: given the same `area`/`gaps` a real `layout` call would use, it
  finds which `Tiles` container's inter-child border (if any) a screen
  point sits on, returning the two adjacent children plus
  `weight_per_pixel` for that border. Implemented as a parallel recursive
  traversal mirroring `layout_node`'s own `Tiles`/`Accordion` geometry
  (not a refactor of `layout_node` itself, to keep that well-tested path
  untouched) — a lone window, or any point that isn't on an internal
  border, structurally has no handle, giving mouse-based resize the same
  "can't resize when alone" guarantee `resize_weight` already has, without
  a separate check. `Accordion` containers are recursed into (via their
  `mru` child, with the same peek-padding math `layout_node` uses) but
  never yield a handle themselves — no borders in/around them, same as
  `resize_weight` skipping `Accordion` ancestors.
- `Node::Floating { window }` (a third leaf kind alongside `Container`/
  `Window`) is a floating window's focus/topology placeholder: a normal
  child for `insert_floating`/`remove_window`/`window_at`/`find_node`/
  `node_for_window` — so `tili-daemon`'s `workspace_focus: NodeId` can
  address a floating window exactly like a tiled one — but `layout`
  excludes it entirely from `Tiles`/`Accordion` sizing (zero footprint, no
  rect emitted, doesn't count toward sibling gaps), and `navigate`/
  `move_within` skip over one as an immediate sibling rather than landing
  spatial focus/movement on it. `window_ids()` returns both kinds;
  `tiled_window_ids()` is `Window`-only, for callers that need to lay out
  or park tiled windows specifically (e.g. `tili-daemon`'s workspace-switch
  parking). A floating window's actual on-screen frame is owned by
  `tili-daemon`'s `placements`/`compute_floating_frame`, never by this
  crate — `Node::Floating` carries no position/size of its own.
