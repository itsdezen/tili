use std::collections::{HashMap, HashSet};
use std::sync::LazyLock;
use std::time::{Duration, Instant};

use regex::Regex;
use tili_ax::{
    AxWindow, InstantFrameSetter, KeyCombo, Monitor, TweenedFrameSetter, WindowFrameSetter,
};
use tili_ipc::{Command, MonitorInfo, RectInfo, WindowInfo, WorkspaceInfo};
use tili_tree::{
    Direction, Gaps, NodeId, NodeSnapshot, Orientation, Rect, Tree, TreeAnchor, TreeSnapshot,
    WindowId,
};

use crate::session::{
    CurrentWindowIdentity, RestoreMatcher, SavedActiveWorkspace, SavedFloatGeometry, SavedMonitor,
    SavedMonitorRef, SavedPlacement, SavedPreviousWorkspace, SavedRestore, SavedSession,
    SavedWindow, SavedWindowKind, SavedWorkspace,
};
use crate::trace;

/// The workspace new windows land in, and the one active at daemon startup.
/// Workspaces declared in config (M5) are created up front by `apply_config`;
/// this is just the one that's active before any config or explicit switch
/// says otherwise, and the fallback if config declares none.
const DEFAULT_WORKSPACE: &str = "main";

/// The keybinding mode active before any config declares modes, and the one
/// `ModeExit` always returns to. Doesn't need to exist in `mode_bindings` —
/// a config with no `keybindings mode="main" { ... }` block just means no
/// hotkeys are bound yet, not an error.
const DEFAULT_MODE: &str = "main";

// macOS has no public API to enumerate/control Spaces, so workspaces are
// virtual: only the active one's windows are actually laid out on screen.
// Every other workspace's windows are "parked" via
// `tili_ax::southeast_parking_position` — see `park`'s own doc comment for
// the technique and why every parked window, from every workspace, shares
// the same global coordinate at the southeast-most display's corner.

/// How close two frames need to be to count as "the same" when deciding
/// whether a `Floating` window's live frame has drifted from what tili
/// itself last set it to (see `maybe_capture_manual_geometry`) — matches
/// `tili-ax`'s own `AxWindow::set_frame` epsilon, since drift detection is
/// only meaningful at that same granularity.
const FLOAT_DRIFT_EPSILON: f64 = 0.5;

/// Built-in default for `Settings::animate_duration_ms` — used until the
/// first config load replaces it. Long enough to read as motion, short
/// enough not to hold up a window operation; `settings {
/// animate-duration-ms }` overrides it at runtime.
const ANIMATION_DURATION: Duration = Duration::from_millis(120);

/// `main.rs`'s `animation_tick` period for `AnimationSpeed::Medium`
/// (~60fps) and `::High` (~120fps) — see `WmState::animation_tick_period`.
/// `Off` has no period at all: `animation_tick`'s `select!` branch is
/// gated on `is_animating_anything`, which is always false under
/// `InstantFrameSetter`.
const ANIMATION_TICK_PERIOD_MEDIUM: Duration = Duration::from_millis(16);
const ANIMATION_TICK_PERIOD_HIGH: Duration = Duration::from_millis(8);

/// How long a window that's vanished from a fresh AX scan is kept in
/// `WmState::pending_removal` before being treated as genuinely closed —
/// long enough to absorb a transient AX hiccup (an app briefly failing to
/// report one of its windows mid-update) without losing track of a window
/// that's actually still open. `WmState::removal_grace` starts at this
/// value but is a field (not used directly) so tests can shrink it to zero
/// instead of sleeping for real.
const REMOVAL_GRACE_PERIOD: Duration = Duration::from_millis(100);

// Post-wake reconnection used to be gated by a flat/debounced timer
// (`WAKE_GRACE_MAX`/`WAKE_GRACE_DEBOUNCE`) — a guessed duration standing in
// for "has this app's AX connection actually come back yet." `note_system_wake`
// now answers that directly instead: it fires a real AX probe at every
// tracked window concurrently the instant wake is observed, and
// `WmState::unconfirmed_pids` tracks — per pid, not as one global episode —
// which ones haven't responded yet. See `note_system_wake`'s doc comment.

/// How long a pid stays in `WmState::pending_launch_pids` after
/// `AppLaunched` before being dropped even without ever getting a window —
/// a bound so a launched-but-windowless process (a background helper, or
/// one that fails to start) can't permanently suppress
/// `reveal_current_frontmost`. Generous relative to a normal GUI app's
/// cold-launch time on purpose: the cost of leaving it too long is a rare
/// missed reveal-on-click, not a wrong one.
const LAUNCH_GRACE_PERIOD: Duration = Duration::from_secs(2);

/// How often a non-empty deferred-session list is re-scanned. These windows
/// are alive but unenumerable at startup (native fullscreen on another
/// Space, minimized, ...); a slow bounded rescan catches them when their
/// Space becomes current again without turning the whole daemon into a
/// poller.
const DEFERRED_RESCAN_INTERVAL: Duration = Duration::from_secs(5);
/// Upper bound on that retry window; after this the saved records are
/// treated as genuinely gone instead of being retried forever.
const DEFERRED_RESCAN_WINDOW: Duration = Duration::from_secs(600);

/// How many times `apply_windows_changed` will defer a brand-new window's
/// disposition when its `bundle_id()` is still unresolved (racing
/// `NSRunningApplication`'s own registration right after a process
/// launches) before giving up and falling back to the kind-based default
/// anyway. Bounded so a process whose bundle id never resolves (e.g. an
/// unbundled helper binary) doesn't sit unplaced forever — each retry rides
/// a real `WindowsChanged` event (the next one for that pid, or the
/// `FULL_RESYNC_MAX_INTERVAL` safety net in `tili-ax/src/watch.rs`), not a
/// new poll.
const MAX_BUNDLE_ID_RETRIES: u8 = 3;

/// Small per-window nudge `place_floating_window` applies to a freshly
/// auto-centered floating window, so opening several same-sized floating
/// windows in a row doesn't stack them in an identical, fully-overlapping
/// spot. See `cascade_offset`.
const FLOATING_CASCADE_STEP: f64 = 28.0;

/// How many placements the cascade sequence spans before it wraps back to
/// dead center (`0, 0`) and repeats — keeps the offset bounded regardless
/// of how many floating windows get opened, rather than drifting further
/// and further from center forever.
const FLOATING_CASCADE_CYCLE: u32 = 8;

/// The `(dx, dy)` nudge for the `index`-th window in a cascade sequence —
/// symmetric around dead center rather than drifting monotonically toward
/// one corner: `0` is untouched, then alternating +/- at growing
/// magnitude (`+step,+step`, `-step,-step`, `+2*step,+2*step`, ...),
/// wrapping back to `(0, 0)` every `FLOATING_CASCADE_CYCLE` placements so
/// the offset never grows unbounded.
fn cascade_offset(index: u32) -> (f64, f64) {
    let cycle = index % FLOATING_CASCADE_CYCLE;
    if cycle == 0 {
        return (0.0, 0.0);
    }
    let magnitude = f64::from(cycle.div_ceil(2)) * FLOATING_CASCADE_STEP;
    let sign = if cycle % 2 == 1 { 1.0 } else { -1.0 };
    (magnitude * sign, magnitude * sign)
}

/// Resolves one dimension of a floating window's size, most specific
/// instruction first: the matching rule's absolute size, then its ratio,
/// then `floating_defaults`' absolute size, then its ratio, then the
/// built-in absolute default. Absolute beats a ratio at the same level
/// (a fixed size is the more specific request); a rule's ratio still beats
/// a default's absolute size, since the rule is the more specific source.
/// Pure so the precedence is unit-testable without a live `AXUIElement`.
fn resolve_floating_extent(
    rule_abs: Option<u32>,
    rule_ratio: Option<f32>,
    defaults_abs: Option<u32>,
    defaults_ratio: Option<f32>,
    builtin_default: f64,
    area_extent: f64,
) -> f64 {
    rule_abs
        .map(f64::from)
        .or_else(|| rule_ratio.map(|ratio| area_extent * f64::from(ratio)))
        .or_else(|| defaults_abs.map(f64::from))
        .or_else(|| defaults_ratio.map(|ratio| area_extent * f64::from(ratio)))
        .unwrap_or(builtin_default)
}

fn frames_match(a: Rect, b: Rect) -> bool {
    (a.x - b.x).abs() < FLOAT_DRIFT_EPSILON
        && (a.y - b.y).abs() < FLOAT_DRIFT_EPSILON
        && (a.width - b.width).abs() < FLOAT_DRIFT_EPSILON
        && (a.height - b.height).abs() < FLOAT_DRIFT_EPSILON
}

/// How far a window's *vertical* geometry may move when its native tab bar
/// appears or disappears and still count as the same app window. Adding the
/// tab bar grows the window and can shift its top edge (Terminal.app:
/// `644,33 620x751` -> `644,30 620x770` when tiled next to another window,
/// and `80,45 1120x699` -> `80,45 1118x741` when alone at the solo gaps).
const TAB_BAR_FRAME_TOLERANCE: f64 = 64.0;

/// Horizontal slack for the same tab-bar transition: the solo-gap case above
/// also loses 2pt of width when the tab bar appears. Still far tighter than
/// the distance between two distinct tiles, so it can't merge a real second
/// window; a genuine new window is additionally rejected by the missing
/// `AXTabGroup` signal.
const TAB_BAR_HORIZONTAL_TOLERANCE: f64 = 8.0;

/// Tab-bar-shaped counterpart to [`frames_match`]. The native-tab guard uses
/// it to identify the window a new tab replaced, and `relayout_monitor` uses
/// it to recognize a correction of the app's own tab-bar resize. Kept
/// separate from `frames_match` so every other caller — and every ambiguous
/// non-tab case — keeps the strict equality check.
///
/// Each tolerance is additionally capped at half the candidate's own extent.
/// Two distinct tiles are separated by at least a full tile size (the inner
/// gap only adds to that), so this makes it impossible for the match to reach
/// an adjacent tile — even with `inner 0` or many windows squeezed into one
/// axis — independently of the configured gaps.
fn frames_match_across_tab_bar(old: Rect, new: Rect) -> bool {
    let horizontal = TAB_BAR_HORIZONTAL_TOLERANCE.min(old.width / 2.0);
    let vertical = TAB_BAR_FRAME_TOLERANCE.min(old.height / 2.0);
    (old.x - new.x).abs() < horizontal
        && (old.width - new.width).abs() < horizontal
        && (old.y - new.y).abs() < vertical
        && (old.height - new.height).abs() < vertical
}

/// Whether a layout write is really just undoing the app's own tab-bar
/// resize: the window is already at a frame that differs from its tile by no
/// more than a tab bar. Such a write must snap instead of animating — no
/// layout changed, so a slide would only advertise the app's unwanted
/// resize. Covering the frame shape (rather than remembering which windows
/// were just folded) matters because closing a tab can move the surviving
/// window more than once: the first pass folds it, and a later pass corrects
/// the app's follow-up resize. Both passes see the same shape and snap.
fn is_tab_bar_correction(current: Rect, target: Rect) -> bool {
    !frames_match(current, target) && frames_match_across_tab_bar(current, target)
}

/// Start frame for a newly placed tiled window's entry animation: the target
/// slot's own size, shifted one slot plus the inner gap in the direction the
/// container grows. `insert_leaf` appends after the insertion point, so a
/// horizontal container grows rightward and a vertical one downward — the
/// start is therefore just outside the slot on that side, and the tween is a
/// short slide into place rather than a crawl from the app's creation frame.
fn entry_start_rect(target: Rect, orientation: Orientation, gap: f64) -> Rect {
    match orientation {
        Orientation::Horizontal => Rect {
            x: target.x + target.width + gap,
            ..target
        },
        Orientation::Vertical => Rect {
            y: target.y + target.height + gap,
            ..target
        },
    }
}

/// One already-managed window considered as the "old" half of a native-tab
/// replacement. Plain data so the decision is unit-testable without a live
/// `AXUIElement`.
#[derive(Clone, Copy)]
struct NativeTabCandidate {
    id: WindowId,
    pid: i32,
    frame: Rect,
    positionable: bool,
    pending_removal: bool,
    /// This candidate is already known to be a member of a multi-tab group,
    /// so it can be replaced by a survivor whose own tab bar has just
    /// disappeared (the 2→1 tab close).
    was_tab_group: bool,
}

/// Finds the unique managed window that a brand-new window is a native tab
/// of. The signal is deliberately conservative: same process, same frame,
/// and — unless the new window exposes a real `AXTabGroup` — the old window
/// must have vanished from the current `AXWindows` scan (macOS only lists
/// the active tab). Ambiguous matches are rejected rather than guessed.
fn native_tab_target(
    candidates: &[NativeTabCandidate],
    fresh_ids: &HashSet<WindowId>,
    new_id: WindowId,
    new_pid: i32,
    new_frame: Rect,
    new_tab_group_buttons: Option<usize>,
) -> Option<WindowId> {
    // A brand-new window has exactly one tab, so only a multi-tab group can
    // prove the new window is an already-populated tab group's active member
    // rather than a genuinely new window.
    let new_has_tab_group = new_tab_group_buttons.is_some();
    let new_is_multi_tab = new_tab_group_buttons.is_some_and(|count| count >= 2);
    let mut found = None;
    for candidate in candidates {
        if candidate.id == new_id
            || candidate.pid != new_pid
            || !candidate.positionable
            || candidate.pending_removal
        {
            continue;
        }
        // An exact frame match is the normal signal. A native tab group is
        // the one exception: showing/hiding the tab bar changes the window's
        // height (and can nudge its top edge, and a few points of width at
        // the solo gaps). Accept that shape only when the new window itself
        // proves it's a tab AND the old window is already gone from the
        // current scan, so an ordinary second window sharing a frame is
        // still refused.
        // A close from two tabs to one hides the tab bar, so the survivor
        // cannot prove a multi-tab group on its own — the replaced window
        // having been one is the equivalent proof there.
        let replaces_hidden_tab = (new_is_multi_tab || candidate.was_tab_group)
            && !fresh_ids.contains(&candidate.id)
            && frames_match_across_tab_bar(candidate.frame, new_frame);
        if !frames_match(candidate.frame, new_frame) && !replaces_hidden_tab {
            continue;
        }
        // A still-enumerable sibling is only trustworthy when the new
        // window itself proves it lives in a tab group; otherwise it could
        // be an ordinary second window that happens to share a frame.
        if fresh_ids.contains(&candidate.id) && !new_has_tab_group {
            continue;
        }
        if found.is_some() {
            return None;
        }
        found = Some(candidate.id);
    }
    found
}

/// One edge of a tiled window, for `magnet_resize_edge` to check independently — a corner drag
/// moves two of these (one horizontal, one vertical) at once.
#[derive(Debug, Clone, Copy)]
enum ResizeEdge {
    Left,
    Right,
    Top,
    Bottom,
}

/// If `edge` moved between `old_rect` and `new_rect`, magnet-snaps a tree weight change onto
/// `tree` for whichever border that edge sits on: converts the pixel delta to weight-space via
/// `Tree::resize_handle_at`'s `weight_per_pixel`, then either rounds it to the nearest whole
/// multiple of `step` (the normal case — the released size always matches some whole number of
/// `resize <step>` keypresses), or, if the drag asked for more than `Tree::resize_delta_bounds`
/// allows at all, overflows straight to that boundary instead — same as spamming the keyboard
/// shortcut past its limit, which `apply_resize`'s own clamp always keeps honoring rather than
/// refusing once no whole step fits. A no-op if the edge didn't move, sits on the workspace's
/// outer boundary (no `ResizeHandle` there — including the "alone"/tiled-fullscreen case, which
/// never reaches here at all since `capture_resize_snapshot` already refuses to snapshot those),
/// or the moved distance is both under one whole step *and* within bounds (dropped rather than
/// landing off-grid).
fn magnet_resize_edge(
    tree: &mut Tree,
    area: Rect,
    gaps: Gaps,
    step: f32,
    old_rect: Rect,
    new_rect: Rect,
    edge: ResizeEdge,
) {
    // `branch_is_before`: is *this* window the near (`before`) side of the border its moved
    // edge sits on, or the far (`after`) side? A window's left/top edge is the far side of the
    // border it touches (the sibling before it owns the near side); its right/bottom edge is
    // the near side.
    let (old_edge, new_edge, point, branch_is_before) = match edge {
        ResizeEdge::Left => (
            old_rect.x,
            new_rect.x,
            (old_rect.x, old_rect.y + old_rect.height / 2.0),
            false,
        ),
        ResizeEdge::Right => (
            old_rect.x + old_rect.width,
            new_rect.x + new_rect.width,
            (
                old_rect.x + old_rect.width,
                old_rect.y + old_rect.height / 2.0,
            ),
            true,
        ),
        ResizeEdge::Top => (
            old_rect.y,
            new_rect.y,
            (old_rect.x + old_rect.width / 2.0, old_rect.y),
            false,
        ),
        ResizeEdge::Bottom => (
            old_rect.y + old_rect.height,
            new_rect.y + new_rect.height,
            (
                old_rect.x + old_rect.width / 2.0,
                old_rect.y + old_rect.height,
            ),
            true,
        ),
    };

    // Left/top: the edge moving *outward* (toward smaller x/y) means this window grew, so the
    // sign flips relative to right/bottom, where growth is the edge moving toward larger x/y.
    let raw_delta_px = match edge {
        ResizeEdge::Left | ResizeEdge::Top => old_edge - new_edge,
        ResizeEdge::Right | ResizeEdge::Bottom => new_edge - old_edge,
    };
    if raw_delta_px.abs() < FLOAT_DRIFT_EPSILON {
        return;
    }

    let Some(handle) = tree.resize_handle_at(area, gaps, point) else {
        return;
    };
    let branch = if branch_is_before {
        handle.before
    } else {
        handle.after
    };

    let Some((max_shrink, max_grow)) = tree.resize_delta_bounds(branch) else {
        return;
    };
    let raw_weight_delta = raw_delta_px * handle.weight_per_pixel;
    let bound = f64::from(if raw_weight_delta >= 0.0 {
        max_grow
    } else {
        max_shrink
    });

    let step = f64::from(step.max(f32::EPSILON));

    if raw_weight_delta.abs() >= bound {
        // The drag wants more than what's actually valid — overflow straight to the
        // boundary, exactly like `apply_resize`'s own clamp does for the keyboard shortcut:
        // it never refuses just because no *whole* step fits, it keeps applying whatever's
        // left until there's truly nothing left (`bound <= 0.0`).
        if bound <= 0.0 {
            return;
        }
        tree.resize_weight(branch, (bound * raw_weight_delta.signum()) as f32);
        return;
    }

    // Otherwise, magnet-snap to the nearest whole step — dropped entirely if it rounds to
    // fewer than one whole step, so a sub-step drag never lands off-grid. `.min(bound)` is a
    // safety net for the rare case where rounding up to the next step would itself overflow
    // (a `bound` that isn't a whole multiple of `step`) — falls back to the same overflow
    // clamp above rather than exceeding what's valid.
    let steps = (raw_weight_delta.abs() / step).round();
    if steps < 1.0 {
        return;
    }
    let snapped_magnitude = (steps * step).min(bound);
    tree.resize_weight(
        branch,
        (snapped_magnitude * raw_weight_delta.signum()) as f32,
    );
}

/// The before-drag tiled layout of `monitor_id`'s active workspace, captured by
/// `on_mouse_button_down` so `on_mouse_button_up` can diff it against each window's live frame
/// and recover which one the user actually dragged (and by how much) — see
/// `WmState::capture_resize_snapshot`/`apply_mouse_resize`.
struct ResizeDragSnapshot {
    monitor_id: u32,
    frames: HashMap<WindowId, Rect>,
}

/// Which workspace a window belongs to, and its current placement state.
/// Indexed by `WindowId` in `WmState::placements` so "which workspace owns
/// this window" is an O(1) lookup instead of scanning every workspace's
/// tree (M4 through M7 did the latter).
struct Placement {
    workspace: String,
    kind: PlacementKind,
}

/// A window's placement state, beyond just "which workspace." Only `Tiled`
/// windows live in that workspace's `Tree`; `Floating` windows are
/// positioned once (creation, or workspace-reactivation) and otherwise left
/// alone; everything else is a "special" state a window can be demoted
/// into (see `demote_to_special`/`promote_from_special`) and back out of
/// without losing track of which of `Tiled`/`Floating` it should return to.
///
/// `Popup` is tracked (shows up in `list_windows`) but — like the transient
/// popups it replaces the old binary reject-outright behavior for — is
/// never tiled, floated, or parked; tili simply never touches its
/// geometry. Reached two ways: an ambiguous `WindowKind` (see
/// `tili_ax::WindowKind`) with no overriding floating-rule match, or an
/// explicit `mode="ignore"` rule (see `resolve_disposition`) forcing it
/// regardless of kind — both get identical runtime treatment.
#[derive(Clone, Debug)]
enum PlacementKind {
    Tiled,
    Floating {
        /// The user's own drag/resize, captured proportionally the first
        /// time a live frame diverges from what tili last set (see
        /// `maybe_capture_manual_geometry`) — `None` until then, meaning
        /// "recompute from the floating rule every time."
        manual: Option<FloatGeometry>,
    },
    NativeFullscreen(Restore),
    Minimized(Restore),
    HiddenApplication(Restore),
    Popup,
}

/// Which non-special state a demoted window (`PlacementKind::NativeFullscreen`/
/// `Minimized`/`HiddenApplication`) should return to once it stops being
/// special — decided once, from the window's `WindowKind`/floating-rule
/// match, at the moment it *first* enters a special state (or at creation,
/// if it starts in one), and carried forward across special-to-special
/// transitions (e.g. minimized while its app is also hidden).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Restore {
    Tiled,
    Floating,
}

/// The three "special" placement states, in priority order — see
/// `special_kind`.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum SpecialKind {
    HiddenApplication,
    Minimized,
    NativeFullscreen,
}

/// A window's floating position/size as a proportion of the monitor area it
/// was captured against, rather than absolute pixels — so restoring it
/// against a differently-sized monitor (or the same monitor at a different
/// resolution) scales sensibly instead of potentially landing off-screen.
/// See `capture_float_geometry`/`restore_floating_frame`.
#[derive(Clone, Copy, Debug, PartialEq)]
struct FloatGeometry {
    rel_x: f64,
    rel_y: f64,
    rel_w: f64,
    rel_h: f64,
}

/// Captures `frame`'s position/size as a proportion of `area` — the
/// inverse of `restore_floating_frame`. Used both when a user's manual
/// drag/resize is first observed (`maybe_capture_manual_geometry`) and
/// right before `park` moves a floating window off-screen
/// (`capture_manual_geometry_before_park`), since that's the last moment a
/// live, meaningful frame is available to capture from.
fn capture_float_geometry(frame: Rect, area: Rect) -> FloatGeometry {
    FloatGeometry {
        rel_x: if area.width > 0.0 {
            (frame.x - area.x) / area.width
        } else {
            0.0
        },
        rel_y: if area.height > 0.0 {
            (frame.y - area.y) / area.height
        } else {
            0.0
        },
        rel_w: if area.width > 0.0 {
            frame.width / area.width
        } else {
            0.0
        },
        rel_h: if area.height > 0.0 {
            frame.height / area.height
        } else {
            0.0
        },
    }
}

/// Maps a captured `FloatGeometry` back onto a (possibly different) `area`,
/// clamping so the result never hangs off-screen even if `area` shrank
/// since capture (e.g. swapping to a smaller monitor). Width/height are
/// clamped first so the subsequent position clamp always has a valid
/// (non-inverted) range to clamp into.
fn restore_floating_frame(geometry: FloatGeometry, area: Rect) -> Rect {
    let width = (geometry.rel_w * area.width).clamp(0.0, area.width);
    let height = (geometry.rel_h * area.height).clamp(0.0, area.height);
    let x = (area.x + geometry.rel_x * area.width).clamp(area.x, area.x + area.width - width);
    let y = (area.y + geometry.rel_y * area.height).clamp(area.y, area.y + area.height - height);
    Rect {
        x,
        y,
        width,
        height,
    }
}

/// Which special state (if any) a window's live AX/process flags put it in
/// right now, checked in priority order: an app being hidden outranks the
/// window itself being minimized, which outranks it being (natively)
/// fullscreen — so e.g. minimizing a window in an already-hidden app still
/// reports `HiddenApplication`, and un-hiding the app then correctly
/// re-checks down to `Minimized` rather than jumping straight back to
/// `Tiled`/`Floating`.
fn special_kind(app_hidden: bool, minimized: bool, fullscreen: bool) -> Option<SpecialKind> {
    if app_hidden {
        Some(SpecialKind::HiddenApplication)
    } else if minimized {
        Some(SpecialKind::Minimized)
    } else if fullscreen {
        Some(SpecialKind::NativeFullscreen)
    } else {
        None
    }
}

/// The `SpecialKind` a stored `PlacementKind` currently represents, if any —
/// the inverse of `special_kind`'s classification, used to detect a change
/// worth reconciling in `reconcile_existing_placement`.
fn current_special(kind: &PlacementKind) -> Option<SpecialKind> {
    match kind {
        PlacementKind::HiddenApplication(_) => Some(SpecialKind::HiddenApplication),
        PlacementKind::Minimized(_) => Some(SpecialKind::Minimized),
        PlacementKind::NativeFullscreen(_) => Some(SpecialKind::NativeFullscreen),
        PlacementKind::Tiled | PlacementKind::Floating { .. } | PlacementKind::Popup => None,
    }
}

/// What a demoted window should be restored to once it leaves whichever
/// special state it's currently in — `Tiled`/`Floating` carry it directly;
/// a window already in a special state keeps whatever `Restore` it was
/// demoted with; a `Popup` has no floating/tiled resting state of its own,
/// so it falls back to `Tiled` (a corner case: a `Popup`-kind window being
/// minimized/hidden/fullscreened at all is rare in practice).
fn restore_for(kind: &PlacementKind) -> Restore {
    match kind {
        PlacementKind::Tiled | PlacementKind::Popup => Restore::Tiled,
        PlacementKind::Floating { .. } => Restore::Floating,
        PlacementKind::HiddenApplication(r)
        | PlacementKind::Minimized(r)
        | PlacementKind::NativeFullscreen(r) => *r,
    }
}

fn saved_window_kind(kind: tili_ax::WindowKind) -> SavedWindowKind {
    match kind {
        tili_ax::WindowKind::Standard => SavedWindowKind::Standard,
        tili_ax::WindowKind::Dialog => SavedWindowKind::Dialog,
        tili_ax::WindowKind::Popup => SavedWindowKind::Popup,
    }
}

fn saved_restore(restore: Restore) -> SavedRestore {
    match restore {
        Restore::Tiled => SavedRestore::Tiled,
        Restore::Floating => SavedRestore::Floating,
    }
}

fn saved_placement(kind: &PlacementKind) -> SavedPlacement {
    match kind {
        PlacementKind::Tiled => SavedPlacement::Tiled,
        PlacementKind::Floating { manual } => SavedPlacement::Floating {
            manual: manual.map(|geometry| SavedFloatGeometry {
                rel_x: geometry.rel_x,
                rel_y: geometry.rel_y,
                rel_w: geometry.rel_w,
                rel_h: geometry.rel_h,
            }),
        },
        PlacementKind::NativeFullscreen(restore) => SavedPlacement::NativeFullscreen {
            restore: saved_restore(*restore),
        },
        PlacementKind::Minimized(restore) => SavedPlacement::Minimized {
            restore: saved_restore(*restore),
        },
        PlacementKind::HiddenApplication(restore) => SavedPlacement::HiddenApplication {
            restore: saved_restore(*restore),
        },
        PlacementKind::Popup => SavedPlacement::Popup,
    }
}

fn float_geometry_from_saved(geometry: SavedFloatGeometry) -> FloatGeometry {
    FloatGeometry {
        rel_x: geometry.rel_x,
        rel_y: geometry.rel_y,
        rel_w: geometry.rel_w,
        rel_h: geometry.rel_h,
    }
}

fn saved_restore_from_placement(placement: &SavedPlacement) -> Restore {
    match placement {
        SavedPlacement::Tiled | SavedPlacement::Popup => Restore::Tiled,
        SavedPlacement::Floating { .. } => Restore::Floating,
        SavedPlacement::NativeFullscreen { restore }
        | SavedPlacement::Minimized { restore }
        | SavedPlacement::HiddenApplication { restore } => match restore {
            SavedRestore::Tiled => Restore::Tiled,
            SavedRestore::Floating => Restore::Floating,
        },
    }
}

fn clamp_frame(mut frame: Rect, area: Rect) -> Rect {
    frame.width = frame.width.clamp(0.0, area.width);
    frame.height = frame.height.clamp(0.0, area.height);
    frame.x = frame.x.clamp(area.x, area.x + area.width - frame.width);
    frame.y = frame.y.clamp(area.y, area.y + area.height - frame.height);
    frame
}

/// Rewrites a saved tree's leaf IDs to the current window IDs that matched
/// them, dropping leaves that did not reopen.
fn remap_snapshot(
    node: &Option<NodeSnapshot>,
    map: &HashMap<WindowId, WindowId>,
) -> Option<NodeSnapshot> {
    let node = node.as_ref()?;
    match node {
        NodeSnapshot::Window(saved) => map.get(saved).copied().map(NodeSnapshot::Window),
        NodeSnapshot::Floating(saved) => map.get(saved).copied().map(NodeSnapshot::Floating),
        NodeSnapshot::Container {
            layout,
            orientation,
            children,
            weights,
            mru,
        } => {
            let mut kept_children = Vec::new();
            let mut kept_weights = Vec::new();
            let mut kept_mru = 0usize;
            for (idx, (child, weight)) in children.iter().zip(weights.iter()).enumerate() {
                if let Some(child) = remap_snapshot(&Some(child.clone()), map) {
                    if idx <= *mru {
                        kept_mru = kept_children.len();
                    }
                    kept_children.push(child);
                    kept_weights.push(*weight);
                }
            }
            if kept_children.is_empty() {
                return None;
            }
            kept_mru = kept_mru.min(kept_children.len() - 1);
            Some(NodeSnapshot::Container {
                layout: *layout,
                orientation: *orientation,
                children: kept_children,
                weights: kept_weights,
                mru: kept_mru,
            })
        }
    }
}

/// Bundle ids of macOS shell/system-UI processes whose windows are always
/// transient chrome (context menus, thumbnail previews, toolbars,
/// authorization prompts, Spotlight's search panel, notification banners)
/// rather than real user-facing windows. Two uses:
///
/// - Forces `FloatingRuleMode::Ignore` regardless of `tili_ax::WindowKind`'s
///   AX classification. A second, belt-and-suspenders layer on top of
///   `classify_window_kind`'s own general
///   `!is_regular_app && !has_close_button` gate (`tili-ax/src/window.rs`),
///   which should already catch all of these structurally; kept as a
///   guaranteed fix for the *specific* cases below in case that general
///   signal ever doesn't apply (e.g. a future macOS version reports one of
///   these processes as `.regular` activation policy) — confirmed in
///   practice for the Dock's right-click context menu and the floating
///   thumbnail preview shown after taking a screenshot (both misclassified
///   `Standard` -> tiled), `SecurityAgent`'s keychain-unlock prompt
///   (misclassified `Dialog` -> re-centered/resized as a floating window
///   instead of left exactly where macOS placed it), and Siri AI's
///   (`com.apple.campo`) background panel (misclassified as an ordinary
///   floating window matching `floating_defaults`, so it silently claimed
///   the dead-center `cascade_offset` slot every time it flashed open —
///   observed making an unrelated app's own centered floating window
///   never land at dead center, since `floating_cascade_index` correctly
///   saw it as "another centered window" and cascaded around it). Extend
///   only when a *specific* reported system surface is observed getting
///   moved/resized, not preemptively.
/// - `reveal_frontmost` uses it at the very top of the function to bail out
///   whenever the *current* pid being resolved is one of these — before
///   `last_frontmost_pid` is ever written — rather than trying to recognize
///   one after the fact via `previous_pid` (see that function's doc
///   comment). Spotlight and Notification Center are here for this reason
///   specifically — dismissing either (Esc, or a notification banner's
///   close button) was confirmed to otherwise corrupt `last_frontmost_pid`
///   and misread the *next* real pid transition as a genuine app switch,
///   jumping to whatever workspace the momentarily-reactivated app lives
///   on, the same failure mode `SYSTEM_UI_BUNDLE_IDS`'s `Ignore` forcing
///   prevents for the first use above.
const SYSTEM_UI_BUNDLE_IDS: &[&str] = &[
    "com.apple.dock",
    "com.apple.screencaptureui",
    "com.apple.SecurityAgent",
    "com.apple.Spotlight",
    "com.apple.notificationcenterui",
    // Siri AI.app's bundle id — not self-descriptive from the string alone.
    "com.apple.campo",
    // The on-screen-display HUD host — volume/brightness/caps-lock. Unlike
    // those, the input-source-switch glyph (Globe/Ctrl-Space) turned out
    // *not* to be owned by this process — confirmed via diagnostic logging
    // it's attributed to whichever app is frontmost at the moment instead
    // (see `is_transient_empty_dialog`, which catches that one by shape
    // rather than bundle id).
    "com.apple.OSDUIHelper",
];

fn is_system_ui_bundle(bundle_id: Option<&str>) -> bool {
    bundle_id.is_some_and(|id| SYSTEM_UI_BUNDLE_IDS.contains(&id))
}

const FINDER_BUNDLE_ID: &str = "com.apple.finder";

/// Compiled once — a fixed pattern, not a user-supplied regex, so
/// `.unwrap()` is safe here (unlike `apply_config`'s user rules, which must
/// handle an invalid pattern gracefully).
static PROTECTED_FINDER_DIALOG_TITLE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^(Copy|Connect to Server)$").unwrap());

/// Finder's "Copy" progress sheet and "Connect to Server" dialog: always
/// left untouched (no tile, no float, no center), regardless of whatever
/// `floating-rules` the user has configured for `com.apple.finder` — same
/// unconditional-override mechanism as `SYSTEM_UI_BUNDLE_IDS` above, but
/// scoped by title rather than bundle-id alone, since the rest of Finder
/// must still tile/float per the user's own config. Titles here are static
/// (system-assigned, not content-derived), so the title-not-yet-populated
/// risk that applies to general user `title=` rules doesn't apply the same
/// way to this pair.
fn is_protected_finder_dialog(bundle_id: Option<&str>, title: &str) -> bool {
    bundle_id.is_some_and(|id| id == FINDER_BUNDLE_ID)
        && PROTECTED_FINDER_DIALOG_TITLE.is_match(title)
}

/// Finder's Quick Look preview panel (opened with Space): confirmed via
/// diagnostic logging to be a `com.apple.finder`-owned `WindowKind::Popup`
/// window titled exactly `"Quick Look"`, plus one or two borderless,
/// empty-titled `Popup` windows it leaves behind for well under
/// `REMOVAL_GRACE_PERIOD` while closing. `Popup` already defaults to
/// `Ignore` (see `resolve_disposition`), but a bare
/// `rule app-id="com.apple.finder"` `floating-rules` entry — written for
/// Finder's real browser windows — has no way to exclude these auxiliary
/// panels, and an explicit rule always wins over the kind-based default;
/// same problem as `is_system_settings_suggestion_popup`.
fn is_finder_quick_look_window(
    bundle_id: Option<&str>,
    kind: tili_ax::WindowKind,
    title: &str,
) -> bool {
    bundle_id.is_some_and(|id| id == FINDER_BUNDLE_ID)
        && kind == tili_ax::WindowKind::Popup
        && (title == "Quick Look" || title.is_empty())
}

/// Finder's "Get Info" panel (Cmd+I): confirmed via diagnostic logging to
/// report `WindowKind::Dialog` (via `classify_window_kind`'s
/// zoom-but-no-fullscreen heuristic — it has a zoom button but isn't
/// fullscreenable) with a content-derived title of the form `"<name> Info"`
/// / `"<n> Items Info"`, so a static title match like
/// `is_protected_finder_dialog`'s can't catch it. `Dialog`'s kind-based
/// default is `Float` (see `resolve_disposition`), which centers it —
/// wrong regardless of whether the user has `com.apple.finder` configured
/// as a floating app, since the ask is "never touch this window," not
/// "float it."
fn is_finder_get_info_window(
    bundle_id: Option<&str>,
    kind: tili_ax::WindowKind,
    title: &str,
) -> bool {
    bundle_id.is_some_and(|id| id == FINDER_BUNDLE_ID)
        && kind == tili_ax::WindowKind::Dialog
        && title.ends_with(" Info")
}

/// Transient system overlays like the input-source-switch HUD glyph
/// (Globe/Ctrl-Space) don't have a fixed owning bundle id the way the
/// volume/brightness HUD does — confirmed via diagnostic logging that it
/// gets attributed to whichever app happens to be frontmost at the moment
/// (e.g. `com.mitchellh.ghostty`), not a dedicated system helper process,
/// so `SYSTEM_UI_BUNDLE_IDS` can't catch it without also force-`Ignore`ing
/// that app's real windows. Scoped by shape instead: it classifies
/// `WindowKind::Dialog` with no zoom button and an empty title, and was
/// confirmed to exist for well under `REMOVAL_GRACE_PERIOD` (closed
/// ~100-120ms after creation) — a real floating dialog is the rare case
/// that also happens to have no title, not the common one, so this is a
/// deliberately broad match rather than one scoped to a specific app.
///
/// The `!has_zoom_button` guard excludes the *other* way a window reaches
/// `WindowKind::Dialog`: `classify_window_kind`'s zoom-but-no-fullscreen
/// heuristic for Preferences/Settings-style windows (see
/// `tili-ax/src/window.rs`), which by construction always has a zoom
/// button. Without it, a real Preferences/Settings window (e.g. System
/// Settings' own main window) whose `AXTitle` just hasn't populated yet at
/// scan time — a routine AX timing race, not a transient overlay — got
/// force-`Ignore`d here, silently overriding the user's own `floating-rules`
/// entry for it.
fn is_transient_empty_dialog(
    kind: tili_ax::WindowKind,
    title: &str,
    has_zoom_button: bool,
) -> bool {
    kind == tili_ax::WindowKind::Dialog && title.is_empty() && !has_zoom_button
}

const SYSTEM_SETTINGS_BUNDLE_ID: &str = "com.apple.systempreferences";

/// System Settings' search-suggestions dropdown (shown while typing in its
/// search field): confirmed via diagnostic logging to be a borderless
/// `AXUnknown`-subrole window with no chrome buttons and no title —
/// `tili_ax::WindowKind::Popup`, the same ambiguous shape as an ordinary
/// tooltip/context-menu overlay, which normally defaults to `Ignore`
/// (see `resolve_disposition`). But the user's own `com.apple.systempreferences`
/// `floating-rules` entry — meant for the app's real Preferences/Settings
/// windows — matches this popup too, since a plain app-id rule has no way
/// to exclude one specific auxiliary window, and an explicit rule always
/// wins over the kind-based default. Force-`Ignore`d unconditionally here,
/// same override mechanism as `is_protected_finder_dialog`, since — unlike
/// the input-source-switch HUD glyph `is_transient_empty_dialog` already
/// handles — a `Popup`-shaped, empty-titled window is common enough
/// elsewhere (ordinary tooltips/menus in other apps) that force-`Ignore`ing
/// it globally would be too broad; scoping to this one bundle id keeps it
/// safe.
fn is_system_settings_suggestion_popup(
    bundle_id: Option<&str>,
    kind: tili_ax::WindowKind,
    title: &str,
) -> bool {
    bundle_id.is_some_and(|id| id == SYSTEM_SETTINGS_BUNDLE_ID)
        && kind == tili_ax::WindowKind::Popup
        && title.is_empty()
}

/// A window whose `kAXSizeAttribute` isn't settable (see
/// `tili_ax::AxWindow::is_resizable`) can't actually be tiled or usefully
/// floated/centered — forcing a frame write onto it just leaves the window
/// clamped to whatever size it refused, visibly wrong. Unlike the other
/// force-`Ignore` predicates above, this isn't scoped to one app or bundle
/// id: it's the general case reported for TradingView's startup splash
/// screen and Settings popup, but the same shape shows up across many
/// Electron apps that spin up an unresizable auxiliary window detached
/// from their main one. `AXUIElementIsAttributeSettable` asks the window
/// directly rather than inferring from subrole/chrome buttons (what `kind`
/// does), so — like the other predicates here — it always overrides a
/// matching floating rule rather than falling back to it: an explicit
/// `mode="tile"` rule for the owning app can't make a genuinely
/// non-resizable window tile any better than the kind-based default could.
fn is_non_resizable_window(is_resizable: bool) -> bool {
    !is_resizable
}

/// Combines every "this brand-new window is fake/transient system chrome,
/// not a real one" predicate above into one shared signal — used to force
/// `rule_mode` to `Ignore` (see `resolve_disposition`) and, via
/// `WmState::force_ignored_window_ids`, to also keep such a window from
/// ever counting as proof its app already has a real window (see
/// `WmState::pid_has_existing_window`). A window this flags is meaningless
/// to tili's actual job (tiling/floating real windows), so every decision
/// built on "what windows does this app have" should treat it as if it
/// doesn't exist, not just the one this predicate was originally written
/// for.
fn should_ignore_new_window(
    bundle_id: Option<&str>,
    kind: tili_ax::WindowKind,
    title: &str,
    has_zoom_button: bool,
    is_resizable: bool,
) -> bool {
    is_system_ui_bundle(bundle_id)
        || is_protected_finder_dialog(bundle_id, title)
        || is_transient_empty_dialog(kind, title, has_zoom_button)
        || is_system_settings_suggestion_popup(bundle_id, kind, title)
        || is_finder_quick_look_window(bundle_id, kind, title)
        || is_finder_get_info_window(bundle_id, kind, title)
        || is_non_resizable_window(is_resizable)
}

/// What a brand-new window's placement disposition should be: an explicit
/// floating-rule `mode` match always wins; otherwise falls back to the
/// kind-based default that predates per-rule modes — `Popup`
/// (AX-ambiguous) -> `Ignore`, `Dialog` -> `Float`, `Standard` -> `Tile`.
/// Resolved once, at creation, alongside `classify_new_window` — never
/// re-derived for an already-placed window (see `apply_config`; rule
/// changes only affect windows created after a reload).
fn resolve_disposition(
    kind: tili_ax::WindowKind,
    rule_mode: Option<tili_config::FloatingRuleMode>,
) -> tili_config::FloatingRuleMode {
    rule_mode.unwrap_or(match kind {
        tili_ax::WindowKind::Popup => tili_config::FloatingRuleMode::Ignore,
        tili_ax::WindowKind::Dialog => tili_config::FloatingRuleMode::Float,
        tili_ax::WindowKind::Standard => tili_config::FloatingRuleMode::Tile,
    })
}

/// Classifies a brand-new window's placement, in priority order:
/// `HiddenApplication`, then `Minimized`, then `NativeFullscreen` (each
/// carrying the `Restore` state to pop back to once the special state
/// ends), then `disposition` itself (see `resolve_disposition`):
/// `Ignore` -> `Popup`, `Float` -> `Floating` (frame computed separately
/// by the caller once disposition is known — see `place_floating_window`),
/// `Tile` -> `Tiled`.
fn classify_new_window(
    disposition: tili_config::FloatingRuleMode,
    app_hidden: bool,
    minimized: bool,
    fullscreen: bool,
) -> PlacementKind {
    let base_restore = if disposition == tili_config::FloatingRuleMode::Float {
        Restore::Floating
    } else {
        Restore::Tiled
    };
    if let Some(special) = special_kind(app_hidden, minimized, fullscreen) {
        return match special {
            SpecialKind::HiddenApplication => PlacementKind::HiddenApplication(base_restore),
            SpecialKind::Minimized => PlacementKind::Minimized(base_restore),
            SpecialKind::NativeFullscreen => PlacementKind::NativeFullscreen(base_restore),
        };
    }
    match disposition {
        tili_config::FloatingRuleMode::Ignore => PlacementKind::Popup,
        tili_config::FloatingRuleMode::Float => PlacementKind::Floating { manual: None },
        tili_config::FloatingRuleMode::Tile => PlacementKind::Tiled,
    }
}

/// Shared `WindowInfo` construction for `list_windows`/`focused_window`, so
/// both report identical identity/metadata fields.
fn window_info(
    window: &AxWindow,
    placement: Option<&Placement>,
    focused_id: Option<WindowId>,
) -> WindowInfo {
    let frame = window.frame();
    WindowInfo {
        id: window.id(),
        pid: window.pid(),
        title: window.title().to_string(),
        bundle_id: window.bundle_id().map(str::to_string),
        workspace: placement.map(|p| p.workspace.clone()),
        focused: focused_id == Some(window.id()),
        role: window.role().to_string(),
        subrole: window.subrole().map(str::to_string),
        placement: placement
            .map(|p| placement_info(&p.kind))
            .unwrap_or(tili_ipc::PlacementInfo::Tiled),
        frame: RectInfo {
            x: frame.x,
            y: frame.y,
            width: frame.width,
            height: frame.height,
        },
    }
}

fn placement_info(kind: &PlacementKind) -> tili_ipc::PlacementInfo {
    match kind {
        PlacementKind::Tiled => tili_ipc::PlacementInfo::Tiled,
        PlacementKind::Floating { .. } => tili_ipc::PlacementInfo::Floating,
        PlacementKind::NativeFullscreen(_) => tili_ipc::PlacementInfo::NativeFullscreen,
        PlacementKind::Minimized(_) => tili_ipc::PlacementInfo::Minimized,
        PlacementKind::HiddenApplication(_) => tili_ipc::PlacementInfo::HiddenApplication,
        PlacementKind::Popup => tili_ipc::PlacementInfo::Popup,
    }
}

/// A `tili_config::WorkspaceRule` with its `workspace` pre-validated
/// against the reloaded config's own declared workspaces — done once in
/// `apply_config`, not on every window creation. An undeclared target
/// drops the whole rule (see `apply_config`'s rebuild).
struct CompiledWorkspaceRule {
    app_id: String,
    workspace: String,
    role: Option<String>,
    subrole: Option<String>,
    /// See `tili_config::WorkspaceRule::always`'s doc comment.
    always: bool,
}

/// A `tili_config::FloatingRule` with its title pattern pre-compiled —
/// done once in `apply_config`, not on every window creation.
struct CompiledFloatingRule {
    app_id: String,
    title: Option<Regex>,
    role: Option<String>,
    subrole: Option<String>,
    width: Option<u32>,
    height: Option<u32>,
    width_ratio: Option<f32>,
    height_ratio: Option<f32>,
    center: Option<bool>,
    mode: tili_config::FloatingRuleMode,
}

/// One startup restore episode. Lives only from `begin_restore` until the
/// grace deadline (or early finalization), then is replaced by
/// `deferred_windows` for windows that could not be seen yet.
struct RestoreSession {
    saved: SavedSession,
    matcher: RestoreMatcher,
    deadline: Instant,
    /// Set once any new window has been offered to the matcher, so an empty
    /// `remaining_normal` before the initial scan cannot finalize early.
    saw_window: bool,
    /// current `WindowId` -> the saved record it matched.
    matched: HashMap<WindowId, SavedWindow>,
}

/// What the most recent restore did, exposed through `tili doctor`.
#[derive(Clone, Debug, Default)]
pub struct RestoreSummary {
    pub matched: usize,
    pub missing_normal: usize,
    pub deferred_windows: usize,
    pub zero_match: bool,
}

/// Holds the daemon's entire mutable state. Both the socket handler and the
/// global-hotkey handler call `dispatch` against the same `WmState`, so
/// CLI-invoked and hotkey-invoked commands can never behave differently.
///
/// `windows` holds the live `AxWindow` handles themselves (not just cached
/// metadata) across *every* workspace — M3 needs the real `AXUIElement` to
/// move/focus/park a window. Each workspace has its own `Tree` for its
/// tiled windows; floating windows (M8) sit outside every `Tree`, tracked
/// only via `placements`. `workspace_focus` remembers the last-focused
/// node per workspace, so switching back to one restores where you left
/// off rather than defaulting to the root every time.
///
/// M9: each connected monitor shows at most one workspace at a time
/// (`active_workspace: monitor id -> workspace name` — a workspace absent
/// from this map is parked, wherever it last was). `focused_monitor` is
/// which one `Focus`/`Move`/`WorkspaceSwitch`/etc. target; `FocusMonitor`
/// is the only thing that changes it. Config-driven workspace-to-monitor
/// pinning (`WorkspaceConfig.monitor`) is intentionally left unwired here —
/// M9's bar is hot-plug/unplug safety, not that finer-grained UX.
pub struct WmState {
    windows: HashMap<WindowId, AxWindow>,
    placements: HashMap<WindowId, Placement>,
    /// Windows missing from a fresh scan of their process's windows, and
    /// when they were first noticed missing — see `finalize_expired_removals`.
    /// Reappearing (found in a later scan) un-pends a window; `remove_app`
    /// (whole process quit) removes immediately/unconditionally and isn't
    /// affected by this grace period.
    pending_removal: HashMap<WindowId, Instant>,
    /// Brand-new windows whose `bundle_id()` was unresolved the last time
    /// `apply_windows_changed` saw them, and how many times that's happened
    /// so far — see `MAX_BUNDLE_ID_RETRIES`. Cleared once the window is
    /// actually placed (bundle id resolved, or retries exhausted).
    pending_bundle_retries: HashMap<WindowId, u8>,
    /// WindowIds whose disposition `should_ignore_new_window` forced to
    /// `Ignore` at creation — tracked directly rather than inferred from
    /// `PlacementKind::Popup`, since `classify_new_window`'s hidden/
    /// minimized/fullscreen precedence (checked before disposition) can
    /// land a force-ignored window in `HiddenApplication`/`Minimized`/
    /// `NativeFullscreen` instead of `Popup`. Read by
    /// `pid_has_existing_window` so a splash/welcome window (or any other
    /// force-ignored window, e.g. a genuinely non-resizable real app
    /// window) never counts as proof its app already has a real window —
    /// consistent either way, since tili never treats a force-ignored
    /// window as tiled/floated/trackable regardless of which `PlacementKind`
    /// it landed in. Cleaned up in `remove_placement`, alongside
    /// `floating_placed`/`floating_centered`.
    force_ignored_window_ids: HashSet<WindowId>,
    /// How long a pending removal waits before being finalized — starts at
    /// `REMOVAL_GRACE_PERIOD`; a field rather than using the constant
    /// directly so tests can shrink it to zero instead of sleeping for real.
    removal_grace: Duration,
    /// How long a `pending_launch_pids` entry survives without a window —
    /// starts at `LAUNCH_GRACE_PERIOD`; a field for the same reason as
    /// `removal_grace` above.
    launch_grace: Duration,
    /// Test-only instrumentation: how many times `relayout_monitor` actually
    /// proceeded to a real layout pass (not an early no-op return). Lets
    /// tests assert "a relayout happened" without needing a real `AxWindow`
    /// to observe a frame change through — compiled out of non-test builds.
    #[cfg(test)]
    relayout_calls: std::cell::Cell<u32>,
    workspaces: HashMap<String, Tree>,
    workspace_focus: HashMap<String, NodeId>,
    monitors: Vec<Monitor>,
    active_workspace: HashMap<u32, String>,
    /// Last monitor a workspace was shown on, including while parked — used
    /// by session restore and parking so an inactive workspace returns to
    /// the display it came from rather than the currently focused one.
    workspace_monitor: HashMap<String, u32>,
    /// Workspace names auto-created for a connected display rather than
    /// declared in config. These are cleaned up (after migrating any
    /// remaining windows) when their display disconnects, and are never
    /// persisted as empty workspaces.
    synthetic_workspaces: HashSet<String>,
    /// Declared workspace names in config order, used to assign the first
    /// free workspace deterministically instead of relying on HashMap order.
    declared_workspaces: Vec<String>,
    /// `workspace "name" monitor="main|12"` pins, kept as raw config strings
    /// and resolved against the current monitor list on assignment.
    workspace_pins: HashMap<String, String>,
    /// Per-monitor back-and-forth history, kept for session persistence and
    /// as a fallback when `previous_focus` has no recorded pair.
    previous_workspaces: HashMap<u32, String>,
    /// The `(monitor, workspace)` pair the user was looking at before the
    /// last navigation command. `workspace-back` restores this pair instead
    /// of following a workspace that may since have moved displays.
    previous_focus: Option<(u32, String)>,
    /// Last workspace that was active on each monitor, used to restore it
    /// after the same display reconnects.
    last_active_workspace: HashMap<u32, String>,
    focused_monitor: u32,
    frame_setter: Box<dyn WindowFrameSetter>,
    gaps: Gaps,
    workspace_gaps: HashMap<String, Gaps>,
    /// Whether `tiled_layout_inputs` should skip folding a monitor's notch
    /// height into its effective top gap — mirrors `gaps`/`workspace_gaps`'
    /// global-plus-per-workspace-override shape, kept separate from `Gaps`
    /// itself since `tili_tree::Gaps` has no (and shouldn't gain a) notch
    /// concept of its own.
    ignore_notch: bool,
    workspace_ignore_notch: HashMap<String, bool>,
    current_mode: String,
    /// mode name -> (key combo -> command), built fresh from config's
    /// `keybindings` on every `apply_config`.
    mode_bindings: HashMap<String, HashMap<KeyCombo, Command>>,
    /// Modes whose `auto_exit` config flag is set — dispatching any command
    /// bound while `current_mode` is one of these should be followed by an
    /// automatic `exit_mode()`, giving a one-shot mode that returns to
    /// `main` after firing a single bound command, without inventing
    /// multi-command-per-key syntax.
    auto_exit_modes: HashSet<String>,
    /// Matched in order — first match wins. Independent of `floating_rules`
    /// — applies regardless of whether the matched window ends up tiled or
    /// floating.
    workspace_rules: Vec<CompiledWorkspaceRule>,
    /// Matched in order — first match wins.
    floating_rules: Vec<CompiledFloatingRule>,
    floating_defaults: tili_config::FloatingDefaults,
    /// M10: warp the cursor to the newly-focused window on every
    /// focus-changing operation.
    mouse_follows_focus: bool,
    /// M10: moving the cursor onto a different monitor changes
    /// `focused_monitor`, same as an explicit `FocusMonitor` command.
    focus_follows_monitor: bool,
    /// Weight-space grid `apply_mouse_resize` snaps a mouse-drag tile
    /// resize to, so the released size always matches some whole number of
    /// `resize <mouse_resize_step>` keypresses — see `Settings::mouse_resize_step`.
    mouse_resize_step: f32,
    /// Whether a brand-new window that looks like an app-owned native tab
    /// is folded into the existing managed window instead of placed as a
    /// separate tile/float. See `native_tab_target`.
    native_tabs: bool,
    /// Newly placed tiled windows whose *first* layout write should seed an
    /// entry animation: snap them just outside their target slot first, then
    /// tween in. Without this, a new window animates from wherever the app
    /// first drew it (often full width or at the head of the list), which
    /// reads as a long crawl across the layout instead of an insertion.
    /// Consumed by `relayout_monitor` and cleared at the end of the pass.
    animate_entry: HashSet<WindowId>,
    /// Managed windows known to be the active member of a *multi-tab*
    /// app-owned group. Needed for the last close: going from two tabs to
    /// one hides the tab bar, so the surviving window no longer exposes the
    /// ≥2-tab-button proof a *new* window needs — the candidate it replaces
    /// having been a tab is what makes that transition recognizable. Updated
    /// from each new window's own tab-button count, so a group that collapses
    /// to a single tab drops out again.
    tab_group_members: HashSet<WindowId>,
    /// Mirrors `Settings::animate` — `apply_config` only swaps
    /// `frame_setter` between `InstantFrameSetter`/`TweenedFrameSetter`
    /// when this actually changes, so an unrelated config reload doesn't
    /// reset (and visually restart) any animation already in flight.
    /// `main.rs` also reads this (via `animation_tick_period`) to know
    /// what period to drive `animation_tick` at.
    animate: tili_config::AnimationSpeed,
    /// Mirrors `Settings::animate_duration_ms` — the duration the current
    /// `frame_setter` was built with, so a live reload can update it (and
    /// only it) without rebuilding the setter and dropping in-flight tweens.
    animate_duration: Duration,
    /// Whether `apply_config` has run before. Only the *first* call
    /// resolves and applies a real default workspace (see
    /// `apply_config`'s doc comment) — every call after that is a hot
    /// reload, which must never yank focus off whatever's on screen.
    config_loaded_once: bool,
    /// Whether `reveal_startup_frontmost` has already taken its one shot at
    /// correcting `apply_config`'s `default_workspace` seed. Set the first
    /// time that function runs, regardless of whether it actually found
    /// anything to reveal — never retried afterward, so a real workspace
    /// switch later in the process's life (the user actually changing
    /// workspaces) is never second-guessed by this startup-only logic. See
    /// `reveal_startup_frontmost`'s doc comment for why this can't just be
    /// `config_loaded_once` itself: that flips *before* the initial window
    /// scan even starts draining, while this must wait until after.
    startup_frontmost_revealed: bool,
    /// Whether the left mouse button is currently held down. While true,
    /// `apply_windows_changed` skips its relayout — a drag-resize fires a
    /// continuous stream of `AXWindowResized` notifications, and forcing
    /// the tree's computed frame on every single one fights the user's
    /// drag and flashes the screen. `on_mouse_button_up` relays out once
    /// to snap back to the tiled layout when the drag ends.
    mouse_button_down: bool,
    /// Captured by `on_mouse_button_down` (before-drag tiled layout for the
    /// focused monitor, `None` if there's nothing valid to resize against
    /// — see `capture_resize_snapshot`), consumed by `on_mouse_button_up`
    /// via `apply_mouse_resize` to derive a tree weight change from
    /// whichever tiled window's native edge/corner the user just dragged.
    resize_drag: Option<ResizeDragSnapshot>,
    /// Orientation a workspace root gets when created for its second
    /// window — `None` means "auto" (derive from the target monitor's
    /// aspect ratio in `root_orientation_hint`).
    default_root_orientation: Option<tili_tree::Orientation>,
    /// Non-native ("tiled") fullscreen (M-Phase 8b): which node, per
    /// workspace, is currently laid out at its monitor's full frame —
    /// `relayout_monitor` special-cases this instead of calling
    /// `Tree::layout` for that workspace, but the tree itself stays
    /// structurally intact underneath so toggling back restores the normal
    /// layout. A workspace absent from this map has no fullscreened window.
    fullscreen_focus: HashMap<String, NodeId>,
    /// The pid last passed to `reveal_frontmost`, so the next call can tell
    /// a genuine user-driven app switch (Cmd-Tab, Mission Control) apart
    /// from macOS reactivating the previous app because the current
    /// frontmost app just lost its last window — see that function's doc
    /// comment.
    last_frontmost_pid: Option<i32>,
    /// pid -> when `AppLaunched` was last recorded for it. Presence means
    /// the pid launched but doesn't have a window yet — during that
    /// window, `frontmost_app_pid()` can still report the *previous*
    /// frontmost pid (the new process hasn't taken over yet), so
    /// `reveal_current_frontmost` can't trust that read and skips instead
    /// of risking a spurious `switch_workspace` to wherever the stale pid
    /// lives. Cleared once the pid actually gets a window
    /// (`apply_windows_changed`), it terminates (`remove_app`), or
    /// `LAUNCH_GRACE_PERIOD` elapses (`finalize_expired_launches`).
    pending_launch_pids: HashMap<i32, Instant>,
    /// pid -> the workspace active when `AppLaunched` was recorded for it
    /// — a longer-lived cousin of `pending_launch_pids` (which clears on
    /// the *first* window, even a force-ignored splash one). Consumed
    /// exactly once, by `apply_windows_changed`, as the fallback
    /// `target_workspace` for that pid's first genuinely real (non-
    /// ignored) window when no `workspace-rules` entry matches — so
    /// switching workspaces while a splash/loading screen is up doesn't
    /// silently steal the real window that opens once it's done. Cleared
    /// on that consumption or `remove_app` (pid quits before ever getting
    /// a real window); unlike `pending_removal`/`pending_launch_pids`,
    /// deliberately has no time-based expiry — a stale-but-unconsumed
    /// entry is harmless (it only ever applies to a window that hasn't
    /// appeared yet), so there's nothing a grace period would protect
    /// against here.
    launch_active_workspace: HashMap<i32, String>,
    /// Bumped once per real (non-no-op, non-error) `switch_workspace`
    /// call. `main.rs` snapshots this when arming a deferred
    /// `reveal_current_frontmost` check (`pending_reveal_deadline`) and
    /// compares against a fresh read here before running it — if an
    /// explicit, more recent `switch_workspace` call already happened in
    /// the meantime, that call is authoritative and the stale deferred
    /// reveal is dropped instead of reverting the user's newer
    /// navigation. See `docs/architecture/tili-daemon.md` for the race
    /// this closes.
    switch_epoch: u64,
    /// Floating windows that have already gone through `place_floating_window`
    /// at least once. `reposition_floating_for_monitor` consults this so a
    /// window with no captured `manual` geometry gets its rule-based frame
    /// computed only the first time it's actually placed, not recomputed
    /// from scratch on every later workspace switch. Cleared in
    /// `remove_placement` alongside the placement itself.
    floating_placed: HashSet<WindowId>,
    /// Floating windows currently on screen via an auto-centered placement
    /// (as opposed to a non-centered rule, or a captured manual drag/resize
    /// — see `place_floating_window`). `floating_cascade_index` counts this
    /// set to decide how far off dead-center a newly-centered window's
    /// `cascade_offset` nudge should be, so the nudge reflects windows that
    /// actually exist right now rather than a persistent counter that
    /// doesn't know whether anything else is still open. Cleared in
    /// `remove_placement` alongside `floating_placed`.
    floating_centered: HashSet<WindowId>,
    /// Pids `note_system_wake`'s AX probe hasn't confirmed as reconnected
    /// yet — populated with every currently-tracked window's pid the
    /// instant a system wake is observed, then pulled out one at a time as
    /// each pid's probe responds (or, failing that, whenever a later real
    /// AX event for that pid succeeds — see `apply_windows_changed`).
    /// `finalize_expired_removals` won't finalize a pending-removal window
    /// whose pid is still in here — per-pid, not gated on one global
    /// episode. Empty the rest of the time (including before the first
    /// wake), so every check against it defaults to "confirmed" when
    /// nothing's actually pending confirmation. Unlike `wake_lock_active`
    /// below, this alone does *not* gate `place_new_window`/
    /// `reveal_frontmost`'s auto-switch — see that field's doc comment for
    /// why a per-pid signal isn't strong enough for that.
    unconfirmed_pids: HashSet<i32>,
    /// True from the instant `note_system_wake` observes a real system
    /// wake until the next genuine `dispatch()`-routed command (hotkey or a
    /// mutating socket command — see `clear_wake_lock`). While active,
    /// `place_new_window`'s workspace-rules auto-switch and every action
    /// `reveal_frontmost` would otherwise take (workspace switch,
    /// focused-monitor change, refocus/raise) are hard-disabled outright,
    /// rather than gated per-pid like `unconfirmed_pids` above. Confirmed
    /// on real hardware that per-pid gating alone isn't enough: an app that
    /// answers its own reconnect probe quickly is "confirmed" almost
    /// immediately, but `frontmost_app_pid()` can still transiently report
    /// the *wrong* already-confirmed app while the rest of the system's
    /// AX/WindowServer connections are still settling from the same wake —
    /// `reveal_frontmost` has no way to tell that misread apart from a
    /// genuine Cmd-Tab using per-pid state alone. Deliberately doesn't
    /// decay on any timer or per-pid signal — only an unambiguous real user
    /// action (never a reactive NSWorkspace/AX notification path, even a
    /// click-driven `reveal_current_frontmost` call) is trusted to end it,
    /// so the workspace/focused window active at the moment of sleep stays
    /// exactly as they were until the user actually does something.
    wake_lock_active: bool,
    /// Rules `apply_config` skipped on its *most recent* call (not a
    /// lifetime accumulation — cleared and rebuilt on every call), mirroring
    /// the `eprintln!` already logged at each skip site. Exists so
    /// `Command::Doctor` has something to report beyond the log file, which
    /// nobody reads day to day.
    config_warnings: Vec<String>,
    /// The most recently loaded `menubar { }` config, converted to its
    /// wire type at load time — served over `Command::MenubarStyle` so
    /// `tili-menubar` learns styling through the same IPC channel it
    /// already uses for everything else, without a `tili-config`
    /// dependency of its own.
    menubar_style: tili_ipc::MenubarStyle,

    /// Bumped whenever something a session snapshot would observe changes.
    /// `main.rs` compares this against its last-saved revision to avoid
    /// rewriting an unchanged session every debounce interval.
    session_revision: u64,
    /// `Some` while startup restore is still matching/deferring windows.
    restore: Option<RestoreSession>,
    /// Saved anchors for windows currently in a special placement, so they
    /// can rejoin their old sibling slot when promoted back to Tiled or
    /// Floating.
    special_anchors: HashMap<WindowId, tili_tree::TreeAnchor>,
    /// Floating geometry captured when a `Floating` window enters a special
    /// state, so promoting it back restores the user's own frame.
    special_float_manual: HashMap<WindowId, FloatGeometry>,
    /// Special-state saved windows not seen during the startup grace —
    /// they may still appear later (native fullscreen on another Space,
    /// minimized app, ...). Matched lazily as windows show up.
    deferred_windows: Vec<SavedWindow>,
    deferred_rescan_deadline: Option<Instant>,
    deferred_rescan_last: Option<Instant>,
    last_restore_summary: Option<RestoreSummary>,
    /// Set when a restore matched zero windows: keep the previous on-disk
    /// state rather than replacing it with an empty/partial session. Any
    /// real user mutation clears this (via `mark_session_dirty`).
    session_save_suppressed: bool,
}

impl Default for WmState {
    fn default() -> Self {
        let mut workspaces = HashMap::new();
        workspaces.insert(DEFAULT_WORKSPACE.to_string(), Tree::new());

        let monitors = tili_ax::list_monitors();
        let focused_monitor = monitors.first().map(|m| m.id).unwrap_or(0);
        let mut active_workspace = HashMap::new();
        active_workspace.insert(focused_monitor, DEFAULT_WORKSPACE.to_string());
        let mut workspace_monitor = HashMap::new();
        workspace_monitor.insert(DEFAULT_WORKSPACE.to_string(), focused_monitor);

        Self {
            windows: HashMap::new(),
            placements: HashMap::new(),
            pending_removal: HashMap::new(),
            pending_bundle_retries: HashMap::new(),
            force_ignored_window_ids: HashSet::new(),
            removal_grace: REMOVAL_GRACE_PERIOD,
            launch_grace: LAUNCH_GRACE_PERIOD,
            #[cfg(test)]
            relayout_calls: std::cell::Cell::new(0),
            workspaces,
            workspace_focus: HashMap::new(),
            monitors,
            active_workspace,
            workspace_monitor,
            synthetic_workspaces: HashSet::new(),
            declared_workspaces: Vec::new(),
            workspace_pins: HashMap::new(),
            previous_workspaces: HashMap::new(),
            previous_focus: None,
            last_active_workspace: HashMap::new(),
            focused_monitor,
            frame_setter: Box::new(InstantFrameSetter),
            gaps: Gaps::default(),
            workspace_gaps: HashMap::new(),
            ignore_notch: false,
            workspace_ignore_notch: HashMap::new(),
            current_mode: DEFAULT_MODE.to_string(),
            mode_bindings: HashMap::new(),
            auto_exit_modes: HashSet::new(),
            workspace_rules: Vec::new(),
            floating_rules: Vec::new(),
            floating_defaults: tili_config::FloatingDefaults::default(),
            mouse_follows_focus: false,
            focus_follows_monitor: false,
            mouse_resize_step: 0.1,
            native_tabs: true,
            animate_entry: HashSet::new(),
            tab_group_members: HashSet::new(),
            animate: tili_config::AnimationSpeed::Off,
            animate_duration: ANIMATION_DURATION,
            config_loaded_once: false,
            startup_frontmost_revealed: false,
            mouse_button_down: false,
            resize_drag: None,
            default_root_orientation: None,
            fullscreen_focus: HashMap::new(),
            last_frontmost_pid: None,
            pending_launch_pids: HashMap::new(),
            launch_active_workspace: HashMap::new(),
            switch_epoch: 0,
            floating_placed: HashSet::new(),
            floating_centered: HashSet::new(),
            unconfirmed_pids: HashSet::new(),
            wake_lock_active: false,
            config_warnings: Vec::new(),
            menubar_style: tili_ipc::MenubarStyle::default(),
            session_revision: 0,
            restore: None,
            special_anchors: HashMap::new(),
            special_float_manual: HashMap::new(),
            deferred_windows: Vec::new(),
            deferred_rescan_deadline: None,
            deferred_rescan_last: None,
            last_restore_summary: None,
            session_save_suppressed: false,
        }
    }
}

impl WmState {
    /// Finalizes any `pending_removal` entry that's been missing for at
    /// least `removal_grace`: actually drops it from `windows`/`placements`,
    /// same as the old unconditional-removal behavior. Called at the start
    /// of `apply_windows_changed` so a grace period set by one process's
    /// event still gets rechecked opportunistically by any other process's
    /// event in the meantime; `main.rs`'s periodic maintenance tick calls
    /// this too, so it's rechecked even with no window events at all.
    pub fn finalize_expired_removals(&mut self) {
        let now = Instant::now();
        let grace = self.removal_grace;
        // A window whose pid is still `unconfirmed_pids` (its app hasn't
        // proven it's reconnected to AX/WindowServer since the last wake —
        // see `note_system_wake`) never finalizes here, however long it's
        // been pending: it stays blocked until that pid actually responds,
        // rather than being freed after a guessed timeout regardless of
        // whether reconnection is actually done. `is_none_or` (rather than
        // requiring a cached `AxWindow`) is only reached in practice for a
        // window that's already been dropped from `self.windows` some
        // other way — every ordinary pending-removal window still has one,
        // since only this function's own removal below ever drops it.
        let expired: Vec<WindowId> = self
            .pending_removal
            .iter()
            .filter(|&(&id, &since)| {
                now.duration_since(since) >= grace
                    && self
                        .windows
                        .get(&id)
                        .is_none_or(|w| self.is_pid_confirmed(w.pid()))
            })
            .map(|(&id, _)| id)
            .collect();
        if expired.is_empty() {
            return;
        }
        for id in &expired {
            if let Some(&since) = self.pending_removal.get(id) {
                eprintln!(
                    "tili-daemon: finalizing window {id} as closed after {:?} pending (grace={grace:?})",
                    now.duration_since(since)
                );
            }
        }
        for id in expired {
            self.pending_removal.remove(&id);
            let pid = self.windows.remove(&id).map(|w| w.pid());
            self.remove_placement(id, pid);
            self.pending_bundle_retries.remove(&id);
        }
        self.mark_session_dirty();
        // A finalized removal changes which windows the tree lays out —
        // without this, survivors keep their pre-removal frames and the
        // closed window's slot stays an empty gap. `relayout_all_visible`,
        // not just the focused monitor: the removed window's workspace may
        // be visible on a different one.
        self.relayout_all_visible();
    }

    /// Records that `pid` just launched (`WmEvent::AppLaunched`) — see
    /// `pending_launch_pids`'s doc comment for why `reveal_current_frontmost`
    /// needs to know this, and `launch_active_workspace`'s doc comment for
    /// why its first real window's workspace fallback needs it too.
    pub fn note_app_launched(&mut self, pid: i32) {
        self.pending_launch_pids.insert(pid, Instant::now());
        self.launch_active_workspace
            .insert(pid, self.active_workspace_name());
    }

    /// Records that the system just woke from sleep
    /// (`WmEvent::SystemDidWake`) — immediately fires a cheap AX probe at
    /// every currently-tracked window's owning pid, concurrently
    /// (`tokio::task::spawn_blocking`, one task per window), and confirms
    /// whichever pids respond. Replaces the old flat/debounced
    /// `WAKE_GRACE_MAX`/`WAKE_GRACE_DEBOUNCE` timer entirely: there's no
    /// "grace episode" left to lapse or extend — a window proves itself
    /// ready by actually answering an AX read, not by a clock running out,
    /// so a fast machine's wait shrinks to whatever that read's real
    /// round-trip takes instead of a guessed constant.
    ///
    /// A pid that doesn't respond stays in `unconfirmed_pids` — only that
    /// pid, not the whole daemon — until either a later probe would
    /// confirm it (nothing re-probes automatically; see below) or a real
    /// AX event for it succeeds first: `apply_windows_changed` confirms a
    /// pid itself once a scan for it actually returns windows, which
    /// covers the pid arriving via its own `WindowsChanged`/resync path
    /// even if this function's own probe missed it. `finalize_expired_removals`
    /// won't finalize a pending-removal window whose pid is still
    /// unconfirmed, for as long as that holds — bounded only by the app
    /// actually reconnecting, not by a timeout that might fire before or
    /// long after it does.
    ///
    /// Also arms `wake_lock_active` — synchronously, before any probing —
    /// so `place_new_window`/`reveal_frontmost`'s auto-switch is hard-blocked
    /// from this instant, not just once the (concurrent, `.await`-ing)
    /// probing below happens to finish. See that field's doc comment for
    /// why per-pid confirmation alone isn't a strong enough guard for those
    /// two, unlike `finalize_expired_removals`.
    ///
    /// Also restarts the grace-period clock (`pending_removal`'s `Instant`)
    /// on every entry already pending — same reasoning as
    /// `note_screen_unlocked`'s identical reset, just for a real sleep
    /// instead of a lock: `Instant` keeps advancing across suspend on macOS
    /// (see `tili-ax/src/watch.rs`'s `SUSPECTED_SLEEP_GAP` doc comment), so
    /// an entry that predates a sleep lasting hours would already read as
    /// far past `removal_grace` the moment this function's probe below
    /// confirms its pid — before anything has actually re-verified the
    /// window itself. Resetting it here means `finalize_expired_removals`
    /// can't act on it until a real `removal_grace` window has elapsed
    /// *after* wake, giving `tili-ax`'s forced post-wake resync (see
    /// `spawn_event_watcher`) a real chance to un-pend it first.
    pub async fn note_system_wake(&mut self) {
        self.wake_lock_active = true;
        eprintln!("tili-daemon: system woke (real sleep), probing tracked windows");
        self.restart_pending_removal_clocks();
        self.probe_tracked_windows().await;
    }

    /// Records that the screen just locked (`WmEvent::ScreenLocked`,
    /// `com.apple.screenIsLocked`) — real-hardware testing on this project
    /// found that the user's actual "put this machine to sleep" gesture is
    /// almost always locking the screen and waiting for the display to
    /// blank (Control+Cmd+Q, the menu-bar lock icon, or an idle timeout),
    /// not a genuine `pmset`/lid-close/Apple-menu sleep — which means
    /// `note_system_wake` above, gated entirely on
    /// `NSWorkspaceDidWakeNotification`, never ran for that case at all:
    /// that notification is only sent when the machine actually suspends,
    /// and it doesn't for a plain screen lock. Locking still switches the
    /// session to loginwindow, and other apps' AX connections have been
    /// observed to stop answering while that's active — the same
    /// reconnect instability `note_system_wake` guards against, just
    /// triggered by a different macOS mechanism.
    ///
    /// Arms `wake_lock_active` synchronously, same as `note_system_wake` —
    /// freezing the active workspace/focus from the instant lock is
    /// observed, since an AX read made *while* the screen is locked can't
    /// be trusted to prove the app is actually reconnected either way (a
    /// read could succeed against a stale cached value, or hang for the
    /// lock's full duration). Also seeds `unconfirmed_pids` with every
    /// currently-tracked window's pid, but deliberately does *not* fire the
    /// AX probe itself here — that's `note_screen_unlocked`'s job, once the
    /// user's own session (and its AX connections) are actually back.
    pub fn note_screen_locked(&mut self) {
        self.wake_lock_active = true;
        let pids: Vec<i32> = self.windows.values().map(|w| w.pid()).collect();
        let count = pids.len();
        self.unconfirmed_pids.extend(pids);
        eprintln!(
            "tili-daemon: screen locked, freezing workspace/focus and marking {count} tracked window(s) unconfirmed"
        );
    }

    /// Records that the screen just unlocked (`WmEvent::ScreenUnlocked`,
    /// `com.apple.screenIsUnlocked`) — the AX server is expected to be
    /// genuinely back by now, so this is where the confirmation probe
    /// `note_screen_locked` deferred actually runs. Doesn't itself touch
    /// `wake_lock_active`: like a real sleep/wake, that only ever clears
    /// from a genuine `dispatch()`-routed command (see
    /// `clear_wake_lock`'s doc comment) — unlocking the screen isn't itself
    /// a hotkey or CLI action, so the workspace/focus stays frozen until
    /// the user's very next real one. Also runs the probe unconditionally
    /// (not gated on `note_screen_locked` having actually run first): a
    /// daemon that started while the screen was already locked would reach
    /// here with an empty `unconfirmed_pids`, in which case this is just a
    /// harmless extra confirmation pass.
    ///
    /// Also restarts the grace-period clock (`pending_removal`'s `Instant`)
    /// on every entry that's already pending: `spawn_event_watcher`'s
    /// `AppEvent::ScreenUnlocked` handling fires a real full resync
    /// alongside this same event, but that resync's `WindowsChanged` events
    /// still have to make it through the daemon's single select! loop and a
    /// `maintenance_tick` before `apply_windows_changed` actually re-scans
    /// and un-pends anything — and `tokio::select!` picks among multiple
    /// simultaneously-ready branches at random, so a `maintenance_tick`
    /// (which calls `finalize_expired_removals` unconditionally) can win
    /// that race and run *before* the resync's own `WindowsChanged` is
    /// drained. Without this reset, an entry that predates the lock — its
    /// clock already far past `removal_grace` from sitting frozen for the
    /// whole lock duration (see `note_screen_locked`) — would be confirmed
    /// finalizable the instant this function's probe below confirms its
    /// pid, even though nothing has actually re-verified the window itself
    /// yet. Resetting it here means `finalize_expired_removals` can't act on
    /// it until a real `removal_grace` window has elapsed *after* unlock,
    /// giving the resync's own re-scan a real chance to un-pend it first.
    pub async fn note_screen_unlocked(&mut self) {
        eprintln!("tili-daemon: screen unlocked, probing tracked windows");
        self.restart_pending_removal_clocks();
        self.probe_tracked_windows().await;
    }

    /// Restarts the grace-period clock (`pending_removal`'s `Instant`) on
    /// every entry already pending, so `finalize_expired_removals` can't act
    /// on a clock that's stale from before a lock/sleep — shared by
    /// `note_screen_unlocked` and `note_system_wake`; see each caller's doc
    /// comment for why its own triggering event needs this.
    fn restart_pending_removal_clocks(&mut self) {
        let now = Instant::now();
        for since in self.pending_removal.values_mut() {
            *since = now;
        }
    }

    /// Fires the concurrent AX responsiveness probe (see
    /// `tili_ax::WindowProbeHandle`) at every currently-tracked window's
    /// owning pid, confirming whichever pids respond — the mechanism shared
    /// by `note_system_wake` and `note_screen_unlocked`. Doesn't touch
    /// `wake_lock_active`; each caller arms that lock itself, at the point
    /// that's actually correct for its own triggering event (immediately,
    /// for both — see each caller's doc comment for why).
    async fn probe_tracked_windows(&mut self) {
        let probes: Vec<(i32, tili_ax::WindowProbeHandle)> = self
            .windows
            .values()
            .map(|w| (w.pid(), w.probe_handle()))
            .collect();
        self.unconfirmed_pids
            .extend(probes.iter().map(|&(pid, _)| pid));
        eprintln!(
            "tili-daemon: probing {} tracked window(s) for AX responsiveness",
            probes.len()
        );

        let handles: Vec<(i32, tokio::task::JoinHandle<bool>)> = probes
            .into_iter()
            .map(|(pid, handle)| {
                (
                    pid,
                    tokio::task::spawn_blocking(move || handle.is_responsive()),
                )
            })
            .collect();
        // A second, coarser bound on top of `tili_ax`'s own AX messaging
        // timeout — protects this loop (which runs synchronously inside
        // `main.rs`'s single `select!` loop, see `note_system_wake`/
        // `note_screen_unlocked`) even if `spawn_blocking`'s worker pool is
        // saturated enough that a probe doesn't start running promptly.
        const PROBE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(2);
        for (pid, handle) in handles {
            let responsive = tokio::time::timeout(PROBE_TIMEOUT, handle)
                .await
                .ok()
                .and_then(Result::ok)
                .unwrap_or(false);
            if responsive {
                self.unconfirmed_pids.remove(&pid);
            }
        }
        eprintln!(
            "tili-daemon: probe done, {} pid(s) still unconfirmed",
            self.unconfirmed_pids.len()
        );
    }

    /// Whether `pid` has proven it's responsive since the last
    /// `note_system_wake` probe — always `true` outside an active wake
    /// (`unconfirmed_pids` only ever gains entries from that probe). The
    /// single check `finalize_expired_removals` gates its wake-specific
    /// behavior on. `place_new_window`/`reveal_frontmost` use
    /// `wake_lock_active` instead — see that field's doc comment for why a
    /// per-pid signal isn't strong enough for those two.
    fn is_pid_confirmed(&self, pid: i32) -> bool {
        !self.unconfirmed_pids.contains(&pid)
    }

    /// Clears `wake_lock_active` — called once from `dispatch()`, for every
    /// command except the read-only queries `tili-menubar`'s own
    /// long-poll-driven refresh issues (`command_is_read_only`). Excluding
    /// those matters: the wake reconciliation itself (processing the
    /// `WindowsChanged` burst `note_system_wake`'s probe and
    /// `resync_watchers` both trigger) sets `changed`, which wakes any
    /// blocked `WaitForChange` connection — `tili-menubar` would then send
    /// a read-only refresh query within milliseconds of the wake, and
    /// treating that as "real user action" would defeat the whole point of
    /// this lock. Deliberately not called from any reactive
    /// NSWorkspace/AX-notification-driven path (`reveal_current_frontmost`,
    /// `apply_windows_changed`, ...) — only `dispatch()`'s hotkey/socket
    /// seam is unambiguous proof the user actually did something.
    pub(crate) fn clear_wake_lock(&mut self) {
        self.wake_lock_active = false;
    }

    /// Drops any `pending_launch_pids` entry older than `LAUNCH_GRACE_PERIOD`
    /// — the bounded fallback for a launched pid that never gets a window.
    /// Called from `main.rs`'s `maintenance_tick`, same "a little time has
    /// passed, go recheck something" shape as `finalize_expired_removals`.
    pub fn finalize_expired_launches(&mut self) {
        let now = Instant::now();
        let grace = self.launch_grace;
        self.pending_launch_pids
            .retain(|_, &mut since| now.duration_since(since) < grace);
    }

    /// Detects brand-new windows that are really the active member of an
    /// app-owned native tab group, returning `new_id -> old_id` for each.
    /// Only called for a fresh scan and only when `native_tabs` is enabled;
    /// the returned map is empty in the common (non-tab) case.
    fn native_tab_replacements(
        &self,
        pid: i32,
        fresh: &[AxWindow],
        tab_group_buttons: &HashMap<WindowId, Option<usize>>,
    ) -> HashMap<WindowId, WindowId> {
        if !self.native_tabs || self.restore_active() {
            return HashMap::new();
        }
        // Ordinary reconciliation scans contain no new windows; skip the
        // candidate pass entirely so this adds zero steady-state cost.
        if !fresh.iter().any(|window| {
            !self.placements.contains_key(&window.id())
                && matches!(window.kind(), tili_ax::WindowKind::Standard)
        }) {
            return HashMap::new();
        }
        let candidates: Vec<NativeTabCandidate> = self
            .windows
            .iter()
            .filter_map(|(&id, window)| {
                let placement = self.placements.get(&id)?;
                Some(NativeTabCandidate {
                    id,
                    pid: window.pid(),
                    frame: window.frame(),
                    positionable: matches!(
                        placement.kind,
                        PlacementKind::Tiled | PlacementKind::Floating { .. }
                    ),
                    pending_removal: self.pending_removal.contains_key(&id),
                    was_tab_group: self.tab_group_members.contains(&id),
                })
            })
            .collect();
        if candidates.is_empty() {
            return HashMap::new();
        }
        let fresh_ids: HashSet<WindowId> = fresh.iter().map(AxWindow::id).collect();
        let mut replacements = HashMap::new();
        for window in fresh {
            let id = window.id();
            if self.placements.contains_key(&id)
                || !matches!(window.kind(), tili_ax::WindowKind::Standard)
            {
                continue;
            }
            let new_frame = window.frame();
            let new_tab_group_buttons = tab_group_buttons.get(&id).copied().flatten();
            if trace::enabled() {
                trace::log(format_args!(
                    "tili-daemon: native-tab probe new={id} pid={pid} frame=({:.1},{:.1} {:.1}x{:.1}) tab_buttons={:?} in_fresh={}",
                    new_frame.x,
                    new_frame.y,
                    new_frame.width,
                    new_frame.height,
                    new_tab_group_buttons,
                    fresh_ids.contains(&id),
                ));
                for candidate in candidates.iter().filter(|c| c.pid == pid) {
                    trace::log(format_args!(
                        "tili-daemon: native-tab candidate id={} pid={} frame=({:.1},{:.1} {:.1}x{:.1}) positionable={} pending_removal={} in_fresh={} frame_match={} tab_bar_match={} was_tab_group={}",
                        candidate.id,
                        candidate.pid,
                        candidate.frame.x,
                        candidate.frame.y,
                        candidate.frame.width,
                        candidate.frame.height,
                        candidate.positionable,
                        candidate.pending_removal,
                        fresh_ids.contains(&candidate.id),
                        frames_match(candidate.frame, new_frame),
                        frames_match_across_tab_bar(candidate.frame, new_frame),
                        candidate.was_tab_group,
                    ));
                }
            }
            match native_tab_target(
                &candidates,
                &fresh_ids,
                id,
                pid,
                new_frame,
                new_tab_group_buttons,
            ) {
                Some(old_id) => {
                    if trace::enabled() {
                        trace::log(format_args!(
                            "tili-daemon: native-tab decision new={id} -> old={old_id}"
                        ));
                    }
                    replacements.insert(id, old_id);
                }
                None => {
                    if trace::enabled() {
                        trace::log(format_args!(
                            "tili-daemon: native-tab decision new={id} -> None"
                        ));
                    }
                }
            }
        }
        replacements
    }

    /// Folds a native tab's newly-visible window into the existing managed
    /// window: swaps the tree leaf and every per-window map from `old` to
    /// `new` in place. No placement, rule matching, or relayout runs, so a
    /// tab open/switch/close cannot move or resize the window. Returns the
    /// new window back (`Some`) if `old` is no longer a positionable
    /// managed window, so the caller can fall back to normal placement;
    /// `None` means the replacement succeeded.
    fn replace_window_id(&mut self, old: WindowId, new: AxWindow) -> Option<AxWindow> {
        let new_id = new.id();
        let Some(placement) = self.placements.remove(&old) else {
            return Some(new);
        };
        if !matches!(
            placement.kind,
            PlacementKind::Tiled | PlacementKind::Floating { .. }
        ) {
            self.placements.insert(old, placement);
            return Some(new);
        }
        let Some(old_window) = self.windows.remove(&old) else {
            self.placements.insert(old, placement);
            return Some(new);
        };

        let mut replaced_in_tree = false;
        for tree in self.workspaces.values_mut() {
            if tree.replace_window(old, new_id) {
                replaced_in_tree = true;
            }
        }
        if !replaced_in_tree {
            // The placement said this was positionable, so a missing leaf
            // is an inconsistent state — restore the old bookkeeping rather
            // than silently creating a placement with no tree node.
            self.windows.insert(old, old_window);
            self.placements.insert(old, placement);
            return Some(new);
        }

        self.windows.insert(new_id, new);
        self.placements.insert(new_id, placement);
        if let Some(anchor) = self.special_anchors.remove(&old) {
            self.special_anchors.insert(new_id, anchor);
        }
        if let Some(manual) = self.special_float_manual.remove(&old) {
            self.special_float_manual.insert(new_id, manual);
        }
        if self.floating_placed.remove(&old) {
            self.floating_placed.insert(new_id);
        }
        if self.floating_centered.remove(&old) {
            self.floating_centered.insert(new_id);
        }
        self.force_ignored_window_ids.remove(&old);
        self.pending_removal.remove(&old);
        self.pending_bundle_retries.remove(&old);
        self.animate_entry.remove(&old);
        self.tab_group_members.remove(&old);
        self.mark_session_dirty();
        None
    }

    /// Replaces one process's windows with a freshly-scanned set, in
    /// response to a `WmEvent::WindowsChanged { pid }`: whatever isn't in
    /// the fresh scan is gone (removed from whichever workspace/placement
    /// it had), whatever's new either joins the *active* workspace's tiled
    /// tree next to the current focus, or — if it matches a floating rule
    /// (M8) — gets centered/sized once and left out of the tree entirely.
    /// Whatever already existed just has its cached handle/frame
    /// refreshed. Re-lays-out the active workspace's tiled windows
    /// afterward.
    pub fn apply_windows_changed(&mut self, pid: i32, mut fresh: Vec<AxWindow>) {
        self.finalize_expired_removals();

        // Zero-size entries are usually menu-extra/phantom AX windows, not
        // real user-facing ones — skip them rather than giving them tiled
        // screen real estate.
        fresh.retain(|w| {
            let frame = w.frame();
            frame.width > 0.0 && frame.height > 0.0
        });

        // `pid` now genuinely has a window — whatever launched it is no
        // longer a "just launched, no window yet" case `reveal_current_frontmost`
        // needs to distrust a `frontmost_app_pid()` read over. A real,
        // non-empty scan is also proof `pid`'s AX connection is up, same as
        // `note_system_wake`'s own probe responding — confirms it here too,
        // in case that probe missed this pid's window (an empty `fresh`
        // isn't used to confirm: it's indistinguishable from a still-down
        // AX connection silently returning nothing).
        if !fresh.is_empty() {
            self.pending_launch_pids.remove(&pid);
            self.unconfirmed_pids.remove(&pid);
        }

        // Native tab groups: the newly-visible tab is a fresh CGWindowID,
        // but it is not a new managed window. Fold it into the existing
        // placement before any of the new-window/rule/placement logic runs,
        // and drop the old tab (still tracked, now hidden) out of this
        // scan so it isn't mistaken for a normal close/repark.
        //
        // One AX read per brand-new standard window, reused both by the
        // guard and by the tab-group-member bookkeeping below.
        let probe_start = trace::enabled().then(Instant::now);
        let tab_group_buttons: HashMap<WindowId, Option<usize>> = if self.native_tabs {
            fresh
                .iter()
                .filter(|window| {
                    !self.placements.contains_key(&window.id())
                        && matches!(window.kind(), tili_ax::WindowKind::Standard)
                })
                .map(|window| (window.id(), window.tab_group_button_count()))
                .collect()
        } else {
            HashMap::new()
        };
        if let Some(probe_start) = probe_start {
            trace::log(format_args!(
                "tili-daemon: native-tab probe map took {:?}",
                probe_start.elapsed()
            ));
        }
        let native_tab_replacements = self.native_tab_replacements(pid, &fresh, &tab_group_buttons);
        let mut tab_replaced = false;
        if !native_tab_replacements.is_empty() {
            let replaced_old_ids: HashSet<WindowId> =
                native_tab_replacements.values().copied().collect();
            fresh.retain(|window| !replaced_old_ids.contains(&window.id()));
        }

        let fresh_ids: std::collections::HashSet<WindowId> =
            fresh.iter().map(AxWindow::id).collect();

        // A window missing from this scan isn't dropped immediately — it's
        // only a candidate for removal once `AxWindow::still_exists` agrees
        // it's really gone, and then only until `finalize_expired_removals`
        // confirms (on a later call) that it's stayed missing for
        // `removal_grace`, absorbing a transient AX hiccup rather than
        // treating every momentary gap as a real close.
        //
        // The liveness check is what makes absence meaningful at all:
        // `kAXWindowsAttribute` (what `fresh` comes from) only reports
        // windows on the macOS Space that's currently active, so every
        // window tili tracks drops out of its own app's scan the moment
        // something else — a native fullscreen transition, most commonly —
        // makes a different Space active. Treating that as "closed" tore
        // real, still-open windows out of their tree, which then
        // re-collapsed the workspace to a single tile and (via
        // `remove_placement`'s reassigned-focus raise) activated a sibling
        // app, dragging macOS straight back out of the Space the user had
        // just entered. `removal_grace` alone can't cover this: the window
        // stays absent for as long as the other Space is showing, which is
        // however long the user watches the video.
        let stale_ids: Vec<WindowId> = self
            .windows
            .iter()
            .filter(|(id, w)| w.pid() == pid && !fresh_ids.contains(id))
            .map(|(&id, _)| id)
            .collect();
        for id in stale_ids {
            if self.windows.get(&id).is_some_and(AxWindow::still_exists) {
                // Alive, just not enumerable from here right now. Clears any
                // entry an earlier scan left pending too — the window has
                // since proven itself, so the old clock shouldn't keep
                // running toward a removal.
                self.pending_removal.remove(&id);
                continue;
            }
            if let std::collections::hash_map::Entry::Vacant(entry) = self.pending_removal.entry(id)
            {
                entry.insert(Instant::now());
            }
        }

        let active_workspace = self.active_workspace_name();
        let app_hidden = tili_ax::is_app_hidden(pid);
        // Snapshotted once per call, not re-checked per window inside the
        // loop below — the loop inserts each window into `self.windows` as
        // it processes it (see `self.windows.insert(id, window)` below), so
        // a live check would see an earlier window from this same batch
        // already present and wrongly conclude "not first" for a later one
        // in the same launch burst (e.g. an app restoring several windows
        // at once). Snapshotting keeps every window from one burst treated
        // as part of the same "first open."
        let pid_is_first_window = !self.pid_has_existing_window(pid);
        // Set when a brand-new window actually gets placed this pass — lets
        // the post-loop re-sync below skip the extra AX query on every
        // ordinary reconciliation call, not just window-creation ones.
        let mut placed_new_window = false;
        for window in fresh {
            let id = window.id();
            // Present in this scan after all — un-pend it, whether or not
            // it was ever actually pending.
            self.pending_removal.remove(&id);
            let window = if let Some(&old_id) = native_tab_replacements.get(&id) {
                match self.replace_window_id(old_id, window) {
                    None => {
                        tab_replaced = true;
                        continue;
                    }
                    Some(window) => window,
                }
            } else {
                window
            };
            let is_new = !self.placements.contains_key(&id);
            let kind = window.kind();
            let minimized = window.minimized();
            let fullscreen = window.fullscreen();
            let live_frame = window.frame();
            let old_frame = if is_new {
                None
            } else {
                self.windows.get(&id).map(AxWindow::frame)
            };

            // A brand-new window with no bundle id yet is usually racing
            // `NSRunningApplication`'s own registration right after its
            // process launched — `matching_floating_rule` below can't tell
            // "no rule configured for this app" apart from "couldn't check
            // because bundle id isn't resolved yet" without one, so
            // finalizing disposition now would silently misclassify it via
            // the kind-based fallback (e.g. `Standard -> Tile`) instead of
            // the floating rule that would've matched a moment later.
            // Defer, bounded by `MAX_BUNDLE_ID_RETRIES` so a process whose
            // bundle id never resolves still eventually gets placed.
            if is_new && window.bundle_id().is_none() {
                let retries = self.pending_bundle_retries.entry(id).or_insert(0);
                if *retries < MAX_BUNDLE_ID_RETRIES {
                    *retries += 1;
                    self.windows.insert(id, window);
                    continue;
                }
            }
            self.pending_bundle_retries.remove(&id);

            // Only resolved for brand-new windows — an existing placement's
            // disposition is never re-derived on a later scan or config
            // reload (see `resolve_disposition`'s doc comment). Past the
            // shared `should_ignore_new_window` gate, `rule_mode` and
            // `rule_workspace` are still independent: which workspace a
            // window lands on has nothing to do with whether it tiles or
            // floats.
            let (rule_mode, rule_workspace, ignore) = if is_new {
                let ignore = should_ignore_new_window(
                    window.bundle_id(),
                    kind,
                    window.title(),
                    window.has_zoom_button(),
                    window.is_resizable(),
                );
                if ignore {
                    self.force_ignored_window_ids.insert(id);
                }
                let mode = if ignore {
                    Some(tili_config::FloatingRuleMode::Ignore)
                } else {
                    self.matching_floating_rule(&window).map(|r| r.mode)
                };
                let workspace = if ignore {
                    None
                } else {
                    self.matching_workspace_rule(&window).and_then(|rule| {
                        (rule.always || pid_is_first_window).then(|| rule.workspace.clone())
                    })
                };
                (mode, workspace, ignore)
            } else {
                (None, None, false)
            };

            self.windows.insert(id, window);

            if is_new {
                let restored = if self.restore_active() {
                    self.try_restore_window(id, app_hidden, minimized, fullscreen)
                } else {
                    self.try_restore_deferred_window(id, app_hidden, minimized, fullscreen)
                };
                if restored {
                    placed_new_window = true;
                    continue;
                }
            }

            if !is_new {
                if let Some(old_frame) = old_frame {
                    self.maybe_capture_manual_geometry(id, old_frame, live_frame);
                }
                self.reconcile_existing_placement(id, app_hidden, minimized, fullscreen);
                continue;
            }

            // A real (non-ignored) window with no `workspace-rules` match
            // falls back to wherever was active when `pid` launched
            // (`AppLaunched`, before even a splash screen showed up) rather
            // than wherever's active right now — otherwise switching
            // workspaces while a splash/loading screen is up (itself
            // force-`Ignore`d, so it never consumes this) silently steals
            // the real window that opens once it's done. One-shot: `remove`
            // both consumes it (a later window from the same still-running
            // app must use the plain "wherever's active now" fallback
            // instead, same as any other subsequent window) and means an
            // ignored splash's own bookkeeping-only `target_workspace`
            // never touches it. Best-effort — `AppLaunched` doesn't
            // reliably fire for every launch (see `note_app_launched`), so
            // this silently degrades to today's behavior when it's absent.
            let target_workspace = rule_workspace.unwrap_or_else(|| {
                if ignore {
                    active_workspace.clone()
                } else {
                    self.launch_active_workspace
                        .remove(&pid)
                        .unwrap_or_else(|| active_workspace.clone())
                }
            });
            let disposition = resolve_disposition(kind, rule_mode);
            let placement_kind =
                classify_new_window(disposition, app_hidden, minimized, fullscreen);
            // Recorded *before* `place_new_window` runs — that function's
            // inactive-workspace path can call `reposition_floating_for_monitor`,
            // which looks windows up via `self.placements`, so it needs to
            // already know about this one.
            self.placements.insert(
                id,
                Placement {
                    workspace: target_workspace.clone(),
                    kind: placement_kind.clone(),
                },
            );
            self.place_new_window(id, &placement_kind, &target_workspace);
            placed_new_window = true;
        }

        // A window that is already the active member of a multi-tab group is
        // remembered, so closing the second-to-last tab (the tab bar then
        // disappears, leaving no ≥2-tab-button proof on the survivor) can
        // still be folded. Counts below 2 drop the window back out.
        for (&id, buttons) in &tab_group_buttons {
            if !self.windows.contains_key(&id) {
                continue;
            }
            if buttons.is_some_and(|count| count >= 2) {
                self.tab_group_members.insert(id);
            } else {
                self.tab_group_members.remove(&id);
            }
        }

        // A window that's real-OS-focused the instant it's created can win
        // that race against `apply_windows_changed` itself: `sync_focus_from_pid`
        // (called reactively from `dispatch()`/`reveal_frontmost`) resolves
        // the focused window via a live AX query first, then looks it up in
        // `self.placements` — which doesn't have an entry yet for a window
        // still being processed by this very function, so that sync silently
        // no-ops and nothing retries it. Re-running it here, now that this
        // pass's placements are guaranteed to exist, closes that gap instead
        // of leaving the workspace's remembered focus on whatever was
        // focused before.
        if placed_new_window || tab_replaced {
            self.sync_focus_from_pid(pid);
            self.mark_session_dirty();
        }

        if !self.mouse_button_down && !self.restore_active() {
            self.relayout_active();
        }
        // Only this pass's relayout is meant to consume entry seeds; if it
        // was skipped (mid-drag, restore), don't replay the slide on some
        // later, unrelated layout.
        self.animate_entry.clear();
    }

    /// Places a brand-new window into `target_workspace` — tiled into that
    /// workspace's own `Tree` next to its own last focus, or floated/
    /// centered if `target_workspace` is the one currently active on the
    /// focused monitor. If `target_workspace` isn't active on the focused
    /// monitor, the focused monitor switches to it immediately (via
    /// `switch_workspace`, same as `move_focused_to_workspace`) so a window
    /// auto-placed by a `workspace-rules` match is never left off-screen —
    /// unless `wake_lock_active` is still set, in which case the window
    /// still gets placed into `target_workspace` (parked, not shown) but the
    /// switch itself is skipped. Confirmed on real hardware: a window whose
    /// app hasn't reconnected to the WindowServer/AX yet after a real wake
    /// can get wrongly finalized as closed and then rediscovered moments
    /// later — `is_new` from this function's caller's perspective, but not
    /// from the user's — matching its `workspace-rules` entry and switching
    /// away from whatever workspace was showing before sleep with no real
    /// trigger. `reveal_frontmost` already gets this same guard for its own
    /// auto-switch; this mirrors it for the `workspace-rules` one — see
    /// `wake_lock_active`'s doc comment for why a hard, non-decaying lock
    /// replaced this function's original per-pid (`unconfirmed_pids`) gate.
    /// Deliberately keyed only by `WindowId`/`PlacementKind`, no `AxWindow`
    /// — this is the seam that makes per-app-workspace placement
    /// unit-testable without a live `AXUIElement`, unlike
    /// `apply_windows_changed` itself.
    /// Inserts `id` into `workspace`'s tree (tiled or floating) and, if that
    /// workspace has no recorded focus yet, records this new node as it —
    /// the bookkeeping shared by `place_new_window` and
    /// `promote_from_special`'s `Tiled`/`Floating` arms. Not used by
    /// `set_floating`, which unconditionally re-focuses the node instead of
    /// only filling in a missing one — a real behavioral difference, not
    /// just a naming one.
    fn insert_into_workspace_tree(
        &mut self,
        id: WindowId,
        workspace: &str,
        floating: bool,
    ) -> NodeId {
        let near = self.workspace_focus.get(workspace).copied();
        let root_orientation = self.root_orientation_hint();
        let tree = self.workspaces.entry(workspace.to_string()).or_default();
        let node = if floating {
            tree.insert_floating(id, near, root_orientation)
        } else {
            tree.insert_window(id, near, root_orientation)
        };
        self.workspace_focus
            .entry(workspace.to_string())
            .or_insert(node);
        node
    }

    fn insert_into_workspace_tree_at_anchor(
        &mut self,
        id: WindowId,
        workspace: &str,
        floating: bool,
        anchor: &tili_tree::TreeAnchor,
    ) -> NodeId {
        let root_orientation = self.root_orientation_hint();
        let tree = self.workspaces.entry(workspace.to_string()).or_default();
        let node = if floating {
            tree.insert_floating_at_anchor(id, anchor, root_orientation)
        } else {
            tree.insert_window_at_anchor(id, anchor, root_orientation)
        };
        self.workspace_focus
            .entry(workspace.to_string())
            .or_insert(node);
        node
    }

    fn place_new_window(
        &mut self,
        id: WindowId,
        placement_kind: &PlacementKind,
        target_workspace: &str,
    ) {
        let active_workspace = self.active_workspace_name();
        let inactive = target_workspace != active_workspace;

        match placement_kind {
            PlacementKind::Tiled => {
                self.insert_into_workspace_tree(id, target_workspace, false);
                if !inactive {
                    // This window's first layout write seeds its entry
                    // animation instead of tweening from the app's own
                    // creation frame — see `animate_entry`.
                    self.animate_entry.insert(id);
                }
            }
            PlacementKind::Floating { .. } => {
                // Joins the tree as a `Node::Floating` leaf exactly like the
                // `Tiled` arm above, so `workspace_focus`/`focused_node()`
                // can address it — its actual on-screen frame is separate
                // (`place_floating_window` below), only written when its
                // workspace is actually visible right now.
                self.insert_into_workspace_tree(id, target_workspace, true);
                if !inactive {
                    let area = self.focused_monitor_area();
                    self.place_floating_window(id, area);
                }
            }
            // Every "no positional action" kind: nothing to do here —
            // parking below (if inactive) or simply leaving the window
            // alone is correct either way.
            _ => {}
        }

        if inactive && let Some(monitor) = self.monitor_showing_workspace(target_workspace) {
            // The destination is already visible on another display; the
            // new window must be laid out there even though focus is about
            // to follow it via switch_workspace.
            self.relayout_monitor(monitor);
            self.reposition_floating_for_monitor(monitor, true);
        }

        if inactive {
            eprintln!(
                "tili-daemon: {} workspace '{target_workspace}' for new window {id} (wake_lock_active={})",
                if self.wake_lock_active {
                    "skipping auto-switch to"
                } else {
                    "auto-switching to"
                },
                self.wake_lock_active
            );
            if !self.wake_lock_active {
                // Can only fail on an undeclared workspace, which can't
                // happen here — `target_workspace` was just inserted into
                // `self.workspaces` above via `entry(...).or_default()`.
                let _ = self.switch_workspace(target_workspace);
            }
        }
    }

    /// Diffs an already-known window's live AX/process flags against its
    /// stored `PlacementKind`, demoting it into (or promoting it out of) a
    /// special state as needed, then — for whatever it ends up as — checks
    /// whether it needs re-parking (only `Tiled`/`Floating` ever do; a
    /// `Minimized`/`NativeFullscreen`/`HiddenApplication` window is never
    /// force-moved, matching the design invariant that tili shouldn't fight
    /// a state the user or another app put the window in deliberately).
    fn reconcile_existing_placement(
        &mut self,
        id: WindowId,
        app_hidden: bool,
        minimized: bool,
        fullscreen: bool,
    ) {
        let Some(kind) = self.placements.get(&id).map(|p| p.kind.clone()) else {
            return;
        };
        let stored_special = current_special(&kind);
        let live_special = special_kind(app_hidden, minimized, fullscreen);

        if stored_special != live_special {
            let restore = restore_for(&kind);
            if stored_special.is_some() {
                self.promote_from_special(id, restore);
            }
            if let Some(new_special) = live_special {
                self.demote_to_special(id, new_special, restore);
            }
        }

        // Parking is a one-shot nudge at the moment a workspace becomes
        // inactive (see `park`/`switch_workspace`), not a continuously-
        // enforced invariant — if the real window drifts back on screen
        // afterward (some apps resist an off-screen move, or just re-notify
        // without actually moving), nothing previously re-asserted it.
        // Every refresh of an already-known window re-checks this instead.
        // `park`'s target is idempotent (see `AxWindow::set_position`'s
        // no-op-if-unchanged guard) since every parked window shares the
        // same coordinate now (see `park`'s doc comment) — calling it
        // redundantly here whenever the window truly is already parked
        // costs nothing.
        if self.parked_positionable_ids().contains(&id) {
            self.park(id);
        }
    }

    /// Moves a window into a special (non-plain) placement state, removing
    /// it from its workspace's tree first if it was `Tiled` or `Floating`
    /// (both live there as leaves — see `tili_tree::Node` — and a
    /// minimized/hidden/fullscreen window has no business occupying either
    /// kind of slot).
    fn demote_to_special(&mut self, id: WindowId, special: SpecialKind, restore: Restore) {
        let Some(workspace) = self.placements.get(&id).map(|p| p.workspace.clone()) else {
            return;
        };
        let was_in_tree = self.placements.get(&id).is_some_and(|p| {
            matches!(
                p.kind,
                PlacementKind::Tiled | PlacementKind::Floating { .. }
            )
        });
        let anchor = if was_in_tree {
            self.workspaces
                .get(&workspace)
                .and_then(|tree| tree.anchor_for(id))
        } else {
            self.special_anchors.get(&id).cloned()
        };
        if let Some(manual) = self
            .placements
            .get(&id)
            .and_then(|placement| match placement.kind {
                PlacementKind::Floating { manual } => manual,
                _ => None,
            })
        {
            self.special_float_manual.insert(id, manual);
        } else if was_in_tree {
            // A Tiled window has no manual float geometry to carry over.
            self.special_float_manual.remove(&id);
        }
        if was_in_tree {
            self.remove_from_tree(id, &workspace);
        }
        if let Some(anchor) = anchor {
            self.special_anchors.insert(id, anchor);
        } else {
            self.special_anchors.remove(&id);
        }
        let kind = match special {
            SpecialKind::HiddenApplication => PlacementKind::HiddenApplication(restore),
            SpecialKind::Minimized => PlacementKind::Minimized(restore),
            SpecialKind::NativeFullscreen => PlacementKind::NativeFullscreen(restore),
        };
        self.placements.insert(id, Placement { workspace, kind });
        self.mark_session_dirty();
    }

    /// Moves a window back out of a special placement state into whatever
    /// `restore` says it should be — reinserting it into its workspace's
    /// tiled tree (`Restore::Tiled`) or recomputing its floating frame
    /// (`Restore::Floating`; Phase 3 layers a captured manual geometry on
    /// top of this).
    fn promote_from_special(&mut self, id: WindowId, restore: Restore) {
        let Some(workspace) = self.placements.get(&id).map(|p| p.workspace.clone()) else {
            return;
        };
        let anchor = self.special_anchors.remove(&id).unwrap_or_default();
        match restore {
            Restore::Tiled => {
                self.insert_into_workspace_tree_at_anchor(id, &workspace, false, &anchor);
                self.placements.insert(
                    id,
                    Placement {
                        workspace,
                        kind: PlacementKind::Tiled,
                    },
                );
            }
            Restore::Floating => {
                let manual = self.special_float_manual.remove(&id);
                self.insert_into_workspace_tree_at_anchor(id, &workspace, true, &anchor);
                // Only write a real on-screen frame if `workspace` is
                // actually the one showing on the focused monitor right now
                // — same gate `place_new_window`'s `Floating` arm applies.
                // Without it, un-hiding an app whose window belongs to a
                // different (possibly parked-elsewhere, possibly
                // wrong-monitor) workspace either sizes the frame against
                // the wrong monitor's area, or writes a visible frame that
                // the very next re-park check below immediately overwrites.
                let is_active = self.active_workspace_name() == workspace;
                self.placements.insert(
                    id,
                    Placement {
                        workspace,
                        kind: PlacementKind::Floating { manual },
                    },
                );
                if is_active {
                    let area = self.focused_monitor_area();
                    if manual.is_some() {
                        self.reposition_floating_for_monitor(self.focused_monitor, false);
                    } else {
                        self.place_floating_window(id, area);
                    }
                }
            }
        }
        self.mark_session_dirty();
    }

    /// If `id` is a `Floating` window with no manual geometry captured yet,
    /// and its live frame has drifted from what tili itself last set
    /// (beyond `FLOAT_DRIFT_EPSILON`) — the only way that can happen is a
    /// user's own drag/resize, since every write tili makes updates the
    /// cached frame to match — captures its current position/size
    /// proportionally against whichever monitor its workspace is showing
    /// on, so future recentering (`reposition_floating_for_monitor`)
    /// restores the user's placement instead of overwriting it with the
    /// floating rule's.
    ///
    /// Skipped entirely while `id` is animating (`Settings::animate`):
    /// every tween step is a real, tili-initiated frame change too, so the
    /// "only a user drag causes drift" invariant this function relies on
    /// doesn't hold mid-animation — treating it as one would misfile the
    /// animation's own intermediate frames as a manual placement.
    fn maybe_capture_manual_geometry(&mut self, id: WindowId, old_frame: Rect, live_frame: Rect) {
        if self.frame_setter.is_animating(id) {
            return;
        }
        if frames_match(old_frame, live_frame) {
            return;
        }
        let Some(placement) = self.placements.get(&id) else {
            return;
        };
        if !matches!(placement.kind, PlacementKind::Floating { manual: None }) {
            return;
        }
        let workspace = placement.workspace.clone();
        self.capture_manual_geometry(id, workspace, live_frame);
    }

    /// Captures a `Floating` window's current on-screen position/size
    /// proportionally right before `park` moves it off-screen — otherwise,
    /// once parked, there's no meaningful live frame left to observe a
    /// manual drag/resize from, and reactivating its workspace would fall
    /// back to recomputing a fresh rule-based frame instead of restoring
    /// where the user actually left it. A no-op for anything already
    /// captured, or not `Floating`.
    fn capture_manual_geometry_before_park(&mut self, id: WindowId) {
        let Some(placement) = self.placements.get(&id) else {
            return;
        };
        if !matches!(placement.kind, PlacementKind::Floating { manual: None }) {
            return;
        }
        let Some(frame) = self.windows.get(&id).map(AxWindow::frame) else {
            return;
        };
        let workspace = placement.workspace.clone();
        self.capture_manual_geometry(id, workspace, frame);
    }

    /// Captures `frame` (`id`'s current position/size) proportionally
    /// against whichever monitor `workspace` is showing on — the tail
    /// shared by `maybe_capture_manual_geometry` (a live drag/resize) and
    /// `capture_manual_geometry_before_park` (right before going off-screen),
    /// which differ only in *which* frame counts as "current" and under
    /// what condition it's worth capturing at all.
    fn capture_manual_geometry(&mut self, id: WindowId, workspace: String, frame: Rect) {
        let area = self
            .active_workspace
            .iter()
            .find(|(_, w)| **w == workspace)
            .and_then(|(&mid, _)| self.monitor_frame(mid))
            .or_else(|| self.monitor_frame(self.focused_monitor))
            .unwrap_or(frame);
        let geometry = capture_float_geometry(frame, area);
        self.placements.insert(
            id,
            Placement {
                workspace,
                kind: PlacementKind::Floating {
                    manual: Some(geometry),
                },
            },
        );
    }

    /// Drops every window belonging to a process that just terminated,
    /// wherever (whichever workspace, tiled or floating) it was — immediate
    /// and unconditional, unlike a single missing window in
    /// `apply_windows_changed`; the whole process is gone, so there's
    /// nothing to wait out a grace period for.
    /// Whether `pid` currently owns a real, trackable window — excludes a
    /// window still sitting out `pending_removal`'s grace period (matches
    /// `reveal_frontmost`'s `suppress` check, which uses the same idiom for
    /// "does this pid still count as alive") and anything in
    /// `force_ignored_window_ids` (see that field's doc comment). Used by
    /// `apply_windows_changed` to decide whether a new window is this app's
    /// first — a splash/welcome screen or any other force-ignored window
    /// never counts, so it can't itself block the real first window from
    /// getting routed.
    fn pid_has_existing_window(&self, pid: i32) -> bool {
        self.windows.iter().any(|(wid, w)| {
            w.pid() == pid
                && !self.pending_removal.contains_key(wid)
                && !self.force_ignored_window_ids.contains(wid)
        })
    }

    pub fn remove_app(&mut self, pid: i32) {
        let ids: Vec<WindowId> = self
            .windows
            .iter()
            .filter(|(_, w)| w.pid() == pid)
            .map(|(&id, _)| id)
            .collect();
        for id in ids {
            self.windows.remove(&id);
            self.remove_placement(id, Some(pid));
            self.pending_removal.remove(&id);
            self.pending_bundle_retries.remove(&id);
        }
        self.pending_launch_pids.remove(&pid);
        self.launch_active_workspace.remove(&pid);
        self.deferred_windows.retain(|saved| saved.pid != pid);
        if self.deferred_windows.is_empty() {
            self.deferred_rescan_deadline = None;
            self.deferred_rescan_last = None;
        }
        self.relayout_all_visible();
        self.mark_session_dirty();
    }

    /// Moves every currently-parked `Tiled`/`Floating` window back on
    /// screen, within the focused monitor's bounds — called once, right
    /// before the daemon process exits (`Shutdown`), so windows aren't left
    /// sitting off-screen indefinitely if the user doesn't restart tili
    /// right away. Precise layout doesn't matter here (a restart rescans
    /// and re-tiles everything from scratch); this only needs to land each
    /// window somewhere visible. Never touches `Minimized`/
    /// `NativeFullscreen`/`HiddenApplication`/`Popup` placements — those
    /// were never "parked" (moved off-screen by `park`) in the first
    /// place, so leaving them exactly as they are is correct, not an
    /// oversight.
    ///
    /// Writes directly via `AxWindow::set_frame`, bypassing `frame_setter`
    /// — this only ever runs once, immediately before the process exits,
    /// so an animated write would never get the later ticks it needs to
    /// actually reach its target, leaving windows stuck off-screen instead
    /// of restored.
    pub fn unpark_all(&mut self) {
        let Some(area) = self.monitor_frame(self.focused_monitor) else {
            return;
        };
        for id in self.parked_positionable_ids() {
            let manual = self.placements.get(&id).and_then(|p| match &p.kind {
                PlacementKind::Floating { manual } => *manual,
                _ => None,
            });
            let frame = manual.map_or(area, |geometry| restore_floating_frame(geometry, area));
            if let Some(window) = self.windows.get_mut(&id) {
                window.set_frame(frame);
            }
        }
    }

    /// Every `Tiled`/`Floating` window whose workspace isn't currently
    /// active on any connected monitor — i.e. exactly the set `park` would
    /// have moved off-screen at some point. Shared by `unpark_all` and
    /// `reconcile_existing_placement`'s repark check.
    fn parked_positionable_ids(&self) -> Vec<WindowId> {
        self.placements
            .iter()
            .filter(|(_, p)| {
                matches!(
                    p.kind,
                    PlacementKind::Tiled | PlacementKind::Floating { .. }
                ) && !self.active_workspace.values().any(|w| w == &p.workspace)
            })
            .map(|(&id, _)| id)
            .collect()
    }

    /// Marks the session snapshot stale. Cheap and intentionally coarse.
    pub fn mark_session_dirty(&mut self) {
        self.session_revision = self.session_revision.wrapping_add(1);
    }

    /// A real user action (command, hotkey, mouse release, config reload)
    /// also clears the zero-match save suppression. Automatic OS/window
    /// events must call `mark_session_dirty` instead, or a zero-match
    /// restore would overwrite the previous good state merely by placing
    /// the windows it already found.
    pub fn mark_user_session_dirty(&mut self) {
        self.session_save_suppressed = false;
        self.mark_session_dirty();
    }

    pub fn session_revision(&self) -> u64 {
        self.session_revision
    }

    pub fn session_save_suppressed(&self) -> bool {
        self.session_save_suppressed
    }

    pub fn restore_active(&self) -> bool {
        self.restore.is_some()
    }

    pub fn last_restore_summary(&self) -> Option<&RestoreSummary> {
        self.last_restore_summary.as_ref()
    }

    pub fn begin_restore(&mut self, saved: SavedSession, grace: Duration) {
        let matcher = RestoreMatcher::new(&saved.windows);
        self.restore = Some(RestoreSession {
            saved,
            matcher,
            deadline: Instant::now() + grace,
            saw_window: false,
            matched: HashMap::new(),
        });
        self.deferred_windows.clear();
        self.deferred_rescan_deadline = None;
        self.deferred_rescan_last = None;
        self.special_anchors.clear();
        self.special_float_manual.clear();
        self.session_save_suppressed = false;
    }

    /// Pids worth re-scanning while some saved windows are still deferred.
    /// Returns an empty list outside the retry window or between attempts;
    /// `main.rs` folds the result into its existing `pending_pids` set, so
    /// this stays a bounded retry rather than a new polling loop.
    pub fn deferred_rescan_pids(&mut self) -> Vec<i32> {
        if self.deferred_windows.is_empty() {
            self.deferred_rescan_deadline = None;
            self.deferred_rescan_last = None;
            return Vec::new();
        }
        let now = Instant::now();
        let deadline = *self
            .deferred_rescan_deadline
            .get_or_insert(now + DEFERRED_RESCAN_WINDOW);
        if now >= deadline {
            self.deferred_windows.clear();
            self.deferred_rescan_deadline = None;
            self.deferred_rescan_last = None;
            return Vec::new();
        }
        if self
            .deferred_rescan_last
            .is_some_and(|last| now.duration_since(last) < DEFERRED_RESCAN_INTERVAL)
        {
            return Vec::new();
        }
        self.deferred_rescan_last = Some(now);
        let mut pids: Vec<i32> = self
            .deferred_windows
            .iter()
            .map(|saved| saved.pid)
            .collect();
        pids.sort_unstable();
        pids.dedup();
        pids
    }

    /// Finalizes once the grace deadline expires, or once every normal
    /// saved window has matched and at least one window has actually been
    /// seen. Returns `true` when this call finalized.
    pub fn tick_restore(&mut self) -> bool {
        let ready = self.restore.as_ref().is_some_and(|restore| {
            Instant::now() >= restore.deadline
                || (restore.saw_window
                    && restore.matcher.remaining_normal() == 0
                    && restore.matcher.unmatched().is_empty())
        });
        if ready {
            self.finalize_restore();
        }
        ready
    }

    /// Called from `apply_windows_changed` before a brand-new window's
    /// normal placement. Returns `true` when the window was claimed by the
    /// saved session (so the caller must skip rules/classify/place).
    fn try_restore_window(
        &mut self,
        id: WindowId,
        app_hidden: bool,
        minimized: bool,
        fullscreen: bool,
    ) -> bool {
        let Some(window) = self.windows.get(&id) else {
            return false;
        };
        let identity = CurrentWindowIdentity::new(
            id,
            window.pid(),
            window.bundle_id().unwrap_or_default(),
            window.title(),
            saved_window_kind(window.kind()),
        );
        let Some((saved, workspace)) = ({
            let Some(restore) = self.restore.as_mut() else {
                return false;
            };
            restore.saw_window = true;
            restore.matcher.try_match(&identity).map(|(_, saved)| {
                let workspace = if self.workspaces.contains_key(&saved.workspace) {
                    saved.workspace.clone()
                } else {
                    self.default_workspace_name()
                };
                (saved, workspace)
            })
        }) else {
            return false;
        };

        self.apply_restored_window(
            id,
            &saved,
            &workspace,
            special_kind(app_hidden, minimized, fullscreen),
        );
        true
    }

    fn apply_restored_window(
        &mut self,
        id: WindowId,
        saved: &SavedWindow,
        workspace: &str,
        live_special: Option<SpecialKind>,
    ) {
        if let Some(restore) = self.restore.as_mut() {
            restore.matched.insert(id, saved.clone());
        }

        if live_special.is_some() {
            let restore = saved_restore_from_placement(&saved.placement);
            let kind = match live_special {
                Some(SpecialKind::HiddenApplication) => PlacementKind::HiddenApplication(restore),
                Some(SpecialKind::Minimized) => PlacementKind::Minimized(restore),
                Some(SpecialKind::NativeFullscreen) => PlacementKind::NativeFullscreen(restore),
                None => PlacementKind::Tiled,
            };
            self.placements.insert(
                id,
                Placement {
                    workspace: workspace.to_string(),
                    kind,
                },
            );
            let anchor = saved.anchor.clone().unwrap_or_default();
            self.special_anchors.insert(id, anchor);
            if let Some(manual) = saved.special_manual {
                self.special_float_manual
                    .insert(id, float_geometry_from_saved(manual));
            }
            return;
        }

        match &saved.placement {
            SavedPlacement::Popup => {
                self.placements.insert(
                    id,
                    Placement {
                        workspace: workspace.to_string(),
                        kind: PlacementKind::Popup,
                    },
                );
            }
            SavedPlacement::Tiled => {
                self.insert_restored_normal(id, workspace, saved, false);
            }
            SavedPlacement::Floating { manual } => {
                self.insert_restored_normal(id, workspace, saved, true);
                self.placements.insert(
                    id,
                    Placement {
                        workspace: workspace.to_string(),
                        kind: PlacementKind::Floating {
                            manual: manual.map(float_geometry_from_saved),
                        },
                    },
                );
            }
            SavedPlacement::NativeFullscreen { restore }
            | SavedPlacement::Minimized { restore }
            | SavedPlacement::HiddenApplication { restore } => {
                // Saved as special, but currently normal: restore the
                // placement it was expected to return to.
                let floating = matches!(restore, SavedRestore::Floating);
                self.special_anchors
                    .insert(id, saved.anchor.clone().unwrap_or_default());
                self.insert_restored_normal(id, workspace, saved, floating);
                self.placements.insert(
                    id,
                    Placement {
                        workspace: workspace.to_string(),
                        kind: match restore {
                            SavedRestore::Tiled => PlacementKind::Tiled,
                            SavedRestore::Floating => PlacementKind::Floating {
                                manual: saved.special_manual.map(float_geometry_from_saved),
                            },
                        },
                    },
                );
            }
        }

        if let Some(frame) = saved.frame
            && self.restored_workspace_is_visible(workspace)
            && let Some(monitor) = saved.monitor.as_ref().and_then(|m| self.find_monitor(m))
        {
            let area = self.monitor_frame(monitor).unwrap_or(frame.to_rect());
            if let Some(window) = self.windows.get_mut(&id) {
                window.set_frame(clamp_frame(frame.to_rect(), area));
            }
        }
    }

    fn insert_restored_normal(
        &mut self,
        id: WindowId,
        workspace: &str,
        saved: &SavedWindow,
        floating: bool,
    ) {
        let anchor = saved.anchor.clone().unwrap_or_default();
        let root_orientation = self.root_orientation_hint();
        let tree = self.workspaces.entry(workspace.to_string()).or_default();
        let node = if floating {
            tree.insert_floating_at_anchor(id, &anchor, root_orientation)
        } else {
            tree.insert_window_at_anchor(id, &anchor, root_orientation)
        };
        self.workspace_focus
            .entry(workspace.to_string())
            .or_insert(node);
        self.placements.insert(
            id,
            Placement {
                workspace: workspace.to_string(),
                kind: if floating {
                    PlacementKind::Floating { manual: None }
                } else {
                    PlacementKind::Tiled
                },
            },
        );
    }

    fn finalize_restore(&mut self) {
        let Some(restore) = self.restore.take() else {
            return;
        };
        let matched = restore.matched;
        let mut saved_to_current: HashMap<WindowId, WindowId> = HashMap::new();
        for (&current, saved) in &matched {
            saved_to_current.insert(saved.id, current);
        }

        // Only windows that are currently normal belong in the tree; a
        // saved-normal window that reopened in a special state stays out.
        let mut tree_map = saved_to_current.clone();
        for (&current, saved) in &matched {
            if matches!(
                saved.placement,
                SavedPlacement::Tiled | SavedPlacement::Floating { .. }
            ) && !matches!(
                self.placements.get(&current).map(|p| &p.kind),
                Some(PlacementKind::Tiled | PlacementKind::Floating { .. })
            ) {
                tree_map.remove(&saved.id);
            }
        }

        for saved_workspace in &restore.saved.workspaces {
            if !self.workspaces.contains_key(&saved_workspace.name) {
                continue;
            }
            let snapshot = remap_snapshot(&saved_workspace.tree.root, &tree_map);
            let default_layout = self
                .workspaces
                .get(&saved_workspace.name)
                .map(Tree::default_layout)
                .unwrap_or(tili_tree::Layout::Tiles);
            let Ok(mut tree) = Tree::from_snapshot(&TreeSnapshot {
                root: snapshot,
                collapsed_root_layout: saved_workspace.tree.collapsed_root_layout,
            }) else {
                continue;
            };
            let collapsed = tree.collapsed_root_layout();
            tree.set_default_layout(default_layout);
            tree.set_collapsed_root_layout(collapsed);

            // Preserve windows that were opened normally while restore was
            // still in progress (unmatched new windows); the saved tree
            // only contains matched leaves.
            let existing: Vec<(WindowId, bool, TreeAnchor)> = self
                .placements
                .iter()
                .filter(|(_, placement)| placement.workspace == saved_workspace.name)
                .filter_map(|(&id, placement)| match placement.kind {
                    PlacementKind::Tiled => Some((id, false)),
                    PlacementKind::Floating { .. } => Some((id, true)),
                    _ => None,
                })
                .filter(|(id, _)| tree.find_node(*id).is_none())
                .map(|(id, floating)| {
                    let anchor = self.special_anchors.get(&id).cloned().unwrap_or_default();
                    (id, floating, anchor)
                })
                .collect();
            for (id, floating, anchor) in existing {
                let root_orientation = self.root_orientation_hint();
                if floating {
                    tree.insert_floating_at_anchor(id, &anchor, root_orientation);
                } else {
                    tree.insert_window_at_anchor(id, &anchor, root_orientation);
                }
            }

            let focus = saved_workspace
                .focus
                .and_then(|saved| tree_map.get(&saved).copied())
                .and_then(|window| tree.find_node(window))
                .or_else(|| tree.default_focus());
            if let Some(node) = focus {
                self.workspace_focus
                    .insert(saved_workspace.name.clone(), node);
            } else {
                self.workspace_focus.remove(&saved_workspace.name);
            }

            if let Some(saved) = saved_workspace.fullscreen {
                if let Some(node) = tree_map
                    .get(&saved)
                    .copied()
                    .and_then(|window| tree.find_node(window))
                {
                    self.fullscreen_focus
                        .insert(saved_workspace.name.clone(), node);
                } else {
                    self.fullscreen_focus.remove(&saved_workspace.name);
                }
            } else {
                self.fullscreen_focus.remove(&saved_workspace.name);
            }

            self.workspaces.insert(saved_workspace.name.clone(), tree);
            if saved_workspace.synthetic
                || (saved_workspace.name.starts_with("monitor-")
                    && !self.declared_workspaces.contains(&saved_workspace.name))
            {
                // `monitor-<id>` was the pre-0.13 fallback name; migrate it
                // into the synthetic lifecycle on upgrade.
                self.synthetic_workspaces
                    .insert(saved_workspace.name.clone());
            }
            if let Some(monitor) = saved_workspace
                .monitor
                .as_ref()
                .and_then(|monitor| self.find_monitor(monitor))
            {
                self.workspace_monitor
                    .insert(saved_workspace.name.clone(), monitor);
            }
        }

        // Restore active workspaces per monitor, but only for workspaces
        // that still exist in config. Missing monitors fall back to main.
        if !restore.saved.active_workspaces.is_empty() {
            self.active_workspace.clear();
            let mut used = HashSet::new();
            for active in &restore.saved.active_workspaces {
                let Some(monitor) = self.find_monitor(&active.monitor) else {
                    continue;
                };
                if !self.workspaces.contains_key(&active.workspace)
                    || !used.insert(active.workspace.clone())
                {
                    continue;
                }
                self.last_active_workspace
                    .insert(monitor, active.workspace.clone());
                self.active_workspace
                    .insert(monitor, active.workspace.clone());
            }
            // Fill any monitor missing from the saved set without ever
            // assigning the same workspace to two displays.
            self.ensure_monitor_workspaces();
        }

        if let Some(saved) = restore.saved.focused_monitor.as_ref()
            && let Some(monitor) = self.find_monitor(saved)
        {
            self.focused_monitor = monitor;
        } else if !self
            .monitors
            .iter()
            .any(|monitor| monitor.id == self.focused_monitor)
        {
            self.focused_monitor = self.monitors.first().map_or(self.focused_monitor, |m| m.id);
        }

        self.previous_workspaces.clear();
        for previous in &restore.saved.previous_workspaces {
            if let Some(monitor) = self.find_monitor(&previous.monitor)
                && self.workspaces.contains_key(&previous.workspace)
            {
                self.previous_workspaces
                    .insert(monitor, previous.workspace.clone());
            }
        }
        // Old state files only had one global value; attribute it to the
        // restored focused monitor.
        if self.previous_workspaces.is_empty()
            && let Some(previous) = &restore.saved.previous_workspace
            && self.workspaces.contains_key(previous)
        {
            self.previous_workspaces
                .insert(self.focused_monitor, previous.clone());
        }
        // Seed the pair-based back target from the restored per-monitor
        // history. It's only a best-effort convenience; the first real
        // navigation overwrites it.
        self.previous_focus = self
            .previous_workspaces
            .get(&self.focused_monitor)
            .cloned()
            .map(|workspace| (self.focused_monitor, workspace));

        // Special saved windows that were never seen stay available for a
        // later appearance (native fullscreen on another Space, minimized
        // app, ...). Normal unmatched windows are simply missing.
        self.deferred_windows = restore
            .matcher
            .unmatched()
            .into_iter()
            .filter(|saved| !matched.values().any(|m| m.id == saved.id))
            .collect();
        self.deferred_rescan_deadline = if self.deferred_windows.is_empty() {
            None
        } else {
            Some(Instant::now() + DEFERRED_RESCAN_WINDOW)
        };
        self.deferred_rescan_last = None;

        let deferred = self.deferred_windows.len();
        let matched_count = matched.len();
        let missing_normal = restore.matcher.remaining_normal();
        let zero_match = matched_count == 0;
        self.last_restore_summary = Some(RestoreSummary {
            matched: matched_count,
            missing_normal,
            deferred_windows: deferred,
            zero_match,
        });

        // Re-apply the final layout without animation: restored frames are
        // already meaningful, and windows arriving from parking should snap,
        // not slide across the screen.
        self.frame_setter.set_suppressed(true);
        for id in self.active_workspace.keys().copied().collect::<Vec<_>>() {
            self.relayout_monitor(id);
            self.reposition_floating_for_monitor(id, false);
        }
        self.frame_setter.set_suppressed(false);

        // Everything not active on a monitor is parked exactly like a normal
        // workspace switch would leave it.
        let active: Vec<String> = self.active_workspace.values().cloned().collect();
        let inactive: Vec<String> = self
            .workspaces
            .keys()
            .filter(|name| !active.iter().any(|active| active == *name))
            .cloned()
            .collect();
        for name in inactive {
            self.park_workspace(&name);
        }

        let special_ids: HashSet<WindowId> = self
            .placements
            .iter()
            .filter(|(_, placement)| {
                matches!(
                    placement.kind,
                    PlacementKind::NativeFullscreen(_)
                        | PlacementKind::Minimized(_)
                        | PlacementKind::HiddenApplication(_)
                )
            })
            .map(|(&id, _)| id)
            .collect();
        self.special_anchors
            .retain(|id, _| special_ids.contains(id));

        // The restored monitor list/arrangement may differ from the one the
        // saved session came from; the per-workspace loop above already
        // parks at the current corner, but re-asserting it here also covers
        // a monitor connected between restore start and finalization.
        self.repark_parked_windows();

        self.mark_session_dirty();
        self.session_save_suppressed = zero_match;
    }

    /// Called for a brand-new window after restore has finalized. A saved
    /// window may be alive but temporarily unenumerable — native fullscreen
    /// on another Space, a minimized app, or a window left on a different
    /// macOS Space during startup. Claim it lazily and put it back into its
    /// saved workspace instead of treating it as a new window.
    fn try_restore_deferred_window(
        &mut self,
        id: WindowId,
        app_hidden: bool,
        minimized: bool,
        fullscreen: bool,
    ) -> bool {
        if self.deferred_windows.is_empty() {
            return false;
        }
        let Some(window) = self.windows.get(&id) else {
            return false;
        };
        let identity = CurrentWindowIdentity::new(
            id,
            window.pid(),
            window.bundle_id().unwrap_or_default(),
            window.title(),
            saved_window_kind(window.kind()),
        );
        let mut matcher = RestoreMatcher::new(&self.deferred_windows);
        let Some((_, saved)) = matcher.try_match(&identity) else {
            return false;
        };
        let workspace = if self.workspaces.contains_key(&saved.workspace) {
            saved.workspace.clone()
        } else {
            self.default_workspace_name()
        };
        // Remove exactly the entry that matched; `try_match` returns a clone
        // without its original index, so match by saved id/identity.
        if let Some(pos) = self
            .deferred_windows
            .iter()
            .position(|entry| entry.id == saved.id)
        {
            self.deferred_windows.remove(pos);
        }
        if self.deferred_windows.is_empty() {
            self.deferred_rescan_deadline = None;
            self.deferred_rescan_last = None;
        }
        self.apply_restored_window(
            id,
            &saved,
            &workspace,
            special_kind(app_hidden, minimized, fullscreen),
        );
        let positionable = matches!(
            self.placements.get(&id).map(|p| &p.kind),
            Some(PlacementKind::Tiled | PlacementKind::Floating { .. })
        );
        if positionable
            && !self
                .active_workspace
                .values()
                .any(|name| name == &workspace)
        {
            self.park(id);
        }
        self.mark_session_dirty();
        true
    }

    fn restored_workspace_is_visible(&self, workspace: &str) -> bool {
        let Some(restore) = &self.restore else {
            return false;
        };
        restore.saved.active_workspaces.iter().any(|active| {
            active.workspace == workspace && self.find_monitor(&active.monitor).is_some()
        })
    }

    fn default_workspace_name(&self) -> String {
        self.active_workspace
            .get(&self.focused_monitor)
            .cloned()
            .or_else(|| self.workspaces.keys().min().cloned())
            .unwrap_or_else(|| DEFAULT_WORKSPACE.to_string())
    }

    fn find_monitor(&self, saved: &SavedMonitorRef) -> Option<u32> {
        if let Some(monitor) = self
            .monitors
            .iter()
            .find(|monitor| monitor.id == saved.id_hint)
        {
            return Some(monitor.id);
        }
        let target = saved.frame.to_rect();
        if let Some(monitor) = self.monitors.iter().find(|monitor| {
            (monitor.frame.x - target.x).abs() <= 50.0 && (monitor.frame.y - target.y).abs() <= 50.0
        }) {
            return Some(monitor.id);
        }
        if saved.is_main {
            return self
                .monitors
                .iter()
                .find(|monitor| monitor.is_main)
                .map(|monitor| monitor.id);
        }
        self.monitors.first().map(|monitor| monitor.id)
    }

    fn monitor_ref(&self, id: u32) -> Option<SavedMonitorRef> {
        self.monitors
            .iter()
            .find(|monitor| monitor.id == id)
            .map(|monitor| SavedMonitorRef {
                id_hint: monitor.id,
                frame: monitor.frame.into(),
                is_main: monitor.is_main,
            })
    }

    fn monitor_ref_for_workspace(&self, workspace: &str) -> Option<SavedMonitorRef> {
        self.active_workspace
            .iter()
            .find(|(_, name)| name.as_str() == workspace)
            .and_then(|(id, _)| self.monitor_ref(*id))
            .or_else(|| self.monitor_ref(self.focused_monitor))
    }

    pub fn snapshot_session(&self) -> SavedSession {
        let monitors: Vec<SavedMonitor> = self
            .monitors
            .iter()
            .map(|monitor| SavedMonitor {
                id_hint: monitor.id,
                frame: monitor.frame.into(),
                is_main: monitor.is_main,
            })
            .collect();
        let focused_monitor = self.monitor_ref(self.focused_monitor);
        let active_workspaces = self
            .active_workspace
            .iter()
            .filter_map(|(&monitor, workspace)| {
                self.monitor_ref(monitor)
                    .map(|monitor| SavedActiveWorkspace {
                        monitor,
                        workspace: workspace.clone(),
                    })
            })
            .collect();
        let workspaces = self
            .workspaces
            .iter()
            .map(|(name, tree)| SavedWorkspace {
                name: name.clone(),
                tree: tree.snapshot(),
                focus: self
                    .workspace_focus
                    .get(name)
                    .and_then(|node| tree.window_at(*node)),
                fullscreen: self
                    .fullscreen_focus
                    .get(name)
                    .and_then(|node| tree.window_at(*node)),
                monitor: self
                    .workspace_monitor
                    .get(name)
                    .and_then(|id| self.monitor_ref(*id))
                    .or_else(|| self.monitor_ref_for_workspace(name)),
                synthetic: self.synthetic_workspaces.contains(name),
            })
            .collect();
        let mut windows: Vec<SavedWindow> = self
            .placements
            .iter()
            .filter_map(|(&id, placement)| {
                let window = self.windows.get(&id)?;
                Some(SavedWindow {
                    id,
                    pid: window.pid(),
                    bundle_id: window.bundle_id().unwrap_or_default().to_string(),
                    title: window.title().to_string(),
                    kind: saved_window_kind(window.kind()),
                    workspace: placement.workspace.clone(),
                    placement: saved_placement(&placement.kind),
                    anchor: self.special_anchors.get(&id).cloned(),
                    special_manual: self.special_float_manual.get(&id).map(|geometry| {
                        SavedFloatGeometry {
                            rel_x: geometry.rel_x,
                            rel_y: geometry.rel_y,
                            rel_w: geometry.rel_w,
                            rel_h: geometry.rel_h,
                        }
                    }),
                    frame: Some(window.frame().into()),
                    monitor: self.monitor_ref_for_workspace(&placement.workspace),
                })
            })
            .collect();
        // A saved window may be alive but temporarily unenumerable during
        // restore (native fullscreen on another Space, minimized, ...).
        // Keep its saved record in the next snapshot too, otherwise a
        // restart while it is deferred would permanently lose it.
        for saved in &self.deferred_windows {
            if !windows.iter().any(|window| window.id == saved.id) {
                windows.push(saved.clone());
            }
        }

        let previous_workspaces = self
            .previous_workspaces
            .iter()
            .filter_map(|(&monitor, workspace)| {
                self.monitor_ref(monitor)
                    .map(|monitor| SavedPreviousWorkspace {
                        monitor,
                        workspace: workspace.clone(),
                    })
            })
            .collect();
        SavedSession::new(
            monitors,
            focused_monitor,
            active_workspaces,
            previous_workspaces,
            workspaces,
            windows,
        )
    }

    pub fn list_windows(&self) -> Vec<WindowInfo> {
        let focused_id = self
            .focused_node()
            .and_then(|node| self.active_tree().window_at(node));
        self.windows
            .values()
            .map(|w| window_info(w, self.placements.get(&w.id()), focused_id))
            .collect()
    }

    /// The daemon's currently focused window, if any — shared by
    /// `QueryState` and the copy-rule CLI.
    pub fn focused_window(&self) -> Option<WindowInfo> {
        let node = self.focused_node()?;
        let id = self.active_tree().window_at(node)?;
        let window = self.windows.get(&id)?;
        Some(window_info(window, self.placements.get(&id), Some(id)))
    }

    /// One versioned full snapshot for `tili query state --json`.
    pub fn query_state(&self) -> tili_ipc::QueryState {
        tili_ipc::QueryState {
            version: 1,
            monitors: self.list_monitors(),
            workspaces: self.list_workspaces(),
            windows: self.list_windows(),
            mode: self.current_mode().to_string(),
            focused_window: self.focused_window(),
        }
    }

    /// The compact subset streamed by `tili subscribe --json` — deliberately
    /// excludes the full window list so a busy workspace doesn't reserialize
    /// every window on every event.
    pub fn subscribe_state(&self) -> tili_ipc::SubscribeState {
        tili_ipc::SubscribeState {
            monitors: self.list_monitors(),
            workspaces: self.list_workspaces(),
            mode: self.current_mode().to_string(),
            focused_window: self.focused_window(),
        }
    }

    /// Applies a freshly-loaded (or hot-reloaded) config: updates gaps
    /// (global + per-workspace overrides), rebuilds the keybinding table,
    /// recompiles floating rules (M8 — an invalid title regex logs a
    /// warning and drops just that rule, not the whole config), primes
    /// each workspace's `default_layout` (top-level `default-layout`,
    /// overridden per-workspace by `workspace "name" layout="..."`; an
    /// unrecognized value warns and falls back — see `resolve_layout` —
    /// this only affects a workspace's *next* first-container creation,
    /// never relayouts windows already tiled), and ensures every workspace
    /// it declares exists (creating empty ones as needed) — without
    /// switching to any of them, so a config edit never yanks focus away
    /// from whatever workspace the user is actually looking at. Re-lays-out
    /// the active workspace afterward so a gap change is visible
    /// immediately.
    ///
    /// Floating rules only apply to windows created *after* this call —
    /// an already-tiled window that a newly-added rule would now match
    /// stays tiled until it's recreated (matches the M8 acceptance bar:
    /// "auto-center/size on window creation").
    pub fn apply_config(&mut self, config: &tili_config::Config) {
        // Rebuilt from scratch on every call — see this field's own doc
        // comment for why it's the *last* load's warnings, not a lifetime
        // accumulation.
        self.config_warnings.clear();
        self.declared_workspaces = config
            .workspaces
            .iter()
            .map(|workspace| workspace.name.clone())
            .collect();
        self.workspace_pins.clear();
        for workspace in &config.workspaces {
            let Some(spec) = &workspace.monitor else {
                continue;
            };
            if spec != "main" && spec.parse::<u32>().is_err() {
                let warning = format!(
                    "ignoring workspace '{}' monitor pin '{spec}' — expected \"main\" or a \
                     numeric display id",
                    workspace.name
                );
                eprintln!("tili-daemon: {warning}");
                self.config_warnings.push(warning);
                continue;
            }
            self.workspace_pins
                .insert(workspace.name.clone(), spec.clone());
        }
        // A config-declared workspace always wins over a stale synthetic
        // marker from a previous monitor name.
        for workspace in &config.workspaces {
            self.synthetic_workspaces.remove(&workspace.name);
        }
        self.menubar_style = tili_ipc::MenubarStyle {
            fill: match config.menubar.fill {
                tili_config::MenubarFill::Filled => tili_ipc::MenubarFill::Filled,
                tili_config::MenubarFill::Outlined => tili_ipc::MenubarFill::Outlined,
            },
            shape: match config.menubar.shape {
                tili_config::MenubarShape::Pill => tili_ipc::MenubarShape::Pill,
                tili_config::MenubarShape::Rounded => tili_ipc::MenubarShape::Rounded,
            },
            uppercase: config.menubar.uppercase,
        };
        self.gaps = to_tree_gaps(config.gaps);
        self.workspace_gaps = config
            .workspace_gaps
            .iter()
            .map(|(name, gaps)| (name.clone(), to_tree_gaps(*gaps)))
            .collect();
        self.ignore_notch = config.gaps.ignore_notch;
        self.workspace_ignore_notch = config
            .workspace_gaps
            .iter()
            .map(|(name, gaps)| (name.clone(), gaps.ignore_notch))
            .collect();

        let global_layout = resolve_layout(
            config.default_layout.as_deref(),
            &mut self.config_warnings,
            "default-layout",
            tili_tree::Layout::Tiles,
        );
        for workspace in &config.workspaces {
            let layout = resolve_layout(
                workspace.layout.as_deref(),
                &mut self.config_warnings,
                &format!("workspace '{}' layout", workspace.name),
                global_layout,
            );
            let tree = self.workspaces.entry(workspace.name.clone()).or_default();
            tree.set_default_layout(layout);
        }

        // Only on the very first load (daemon startup, before any real
        // window has been scanned — `apply_config` always runs before the
        // event loop starts draining `WindowsChanged` events): swap the
        // internal `DEFAULT_WORKSPACE` seed for a real, user-declared one,
        // so windows present at startup don't land in a "main" workspace
        // that doesn't exist in the user's config. A later hot reload must
        // never do this — that would yank focus off whatever's on screen.
        if !self.config_loaded_once {
            self.config_loaded_once = true;
            let pinned_default = self
                .workspace_pins
                .iter()
                .filter(|(workspace, spec)| {
                    self.resolve_workspace_pin(spec) == Some(self.focused_monitor)
                        && self.workspaces.contains_key(*workspace)
                })
                .map(|(workspace, _)| workspace.clone())
                .min();
            if let Some(default_name) = pinned_default.or_else(|| resolve_default_workspace(config))
            {
                self.active_workspace
                    .insert(self.focused_monitor, default_name.clone());
                self.last_active_workspace
                    .insert(self.focused_monitor, default_name);
            }
            // The bootstrap seed is only a placeholder until real
            // workspaces are declared — once they are, drop it (unless the
            // user's own config happens to declare a workspace literally
            // named "main", in which case the declare-loop above already
            // treats it as a normal one) so `switch_workspace` can no
            // longer target an undeclared name through it.
            if !config.workspaces.is_empty()
                && !config
                    .workspaces
                    .iter()
                    .any(|w| w.name == DEFAULT_WORKSPACE)
            {
                self.workspaces.remove(DEFAULT_WORKSPACE);
                self.active_workspace
                    .retain(|_, name| name != DEFAULT_WORKSPACE);
            }
        }

        self.mode_bindings = config
            .keybindings
            .iter()
            .map(|mode| {
                let bindings = mode
                    .bindings
                    .iter()
                    .filter_map(|kb| {
                        let combo = tili_ax::parse_key_combo(&kb.key)?;
                        Some((combo, tili_ipc::parse(&kb.command)))
                    })
                    .collect();
                (mode.name.clone(), bindings)
            })
            .collect();
        // A reload that drops the mode we were in (e.g. the config no
        // longer declares it) falls back to the default rather than
        // leaving hotkeys pointing at a table that no longer exists.
        if self.current_mode != DEFAULT_MODE && !self.mode_bindings.contains_key(&self.current_mode)
        {
            self.current_mode = DEFAULT_MODE.to_string();
        }

        self.auto_exit_modes = config
            .keybindings
            .iter()
            .filter(|mode| mode.auto_exit)
            .map(|mode| mode.name.clone())
            .collect();

        self.workspace_rules = config
            .workspace_rules
            .iter()
            .filter_map(|rule| {
                if config.workspaces.iter().any(|w| w.name == rule.workspace) {
                    Some(CompiledWorkspaceRule {
                        app_id: rule.app_id.clone(),
                        workspace: rule.workspace.clone(),
                        role: rule.role.clone(),
                        subrole: rule.subrole.clone(),
                        always: rule.always,
                    })
                } else {
                    let warning = format!(
                        "skipping workspace rule for '{}' — workspace '{}' isn't declared in \
                         config",
                        rule.app_id, rule.workspace
                    );
                    eprintln!("tili-daemon: {warning}");
                    self.config_warnings.push(warning);
                    None
                }
            })
            .collect();

        self.floating_rules = config
            .floating_rules
            .iter()
            .filter_map(|rule| {
                let title = match &rule.title {
                    Some(pattern) => match Regex::new(pattern) {
                        Ok(re) => Some(re),
                        Err(e) => {
                            let warning = format!(
                                "skipping floating rule for '{}' — invalid title regex \
                                 '{pattern}': {e}",
                                rule.app_id
                            );
                            eprintln!("tili-daemon: {warning}");
                            self.config_warnings.push(warning);
                            return None;
                        }
                    },
                    None => None,
                };
                Some(CompiledFloatingRule {
                    app_id: rule.app_id.clone(),
                    title,
                    role: rule.role.clone(),
                    subrole: rule.subrole.clone(),
                    width: rule.width,
                    height: rule.height,
                    width_ratio: rule.width_ratio,
                    height_ratio: rule.height_ratio,
                    center: rule.center,
                    mode: rule.mode,
                })
            })
            .collect();
        self.floating_defaults = config.floating_defaults;

        self.mouse_follows_focus = config.settings.mouse_follows_focus;
        self.focus_follows_monitor = config.settings.focus_follows_monitor;
        self.mouse_resize_step = config.settings.mouse_resize_step;
        self.native_tabs = config.settings.native_tabs;
        if !self.native_tabs {
            // The remembered members only exist to feed the guard; keeping
            // stale ids across a disable/re-enable would outlive the app
            // state they describe.
            self.tab_group_members.clear();
        }
        tili_ax::set_call_timeout_ms(config.settings.ax_timeout_ms);
        let animate_duration = Duration::from_millis(config.settings.animate_duration_ms);
        if config.settings.animate != self.animate {
            self.animate = config.settings.animate;
            self.animate_duration = animate_duration;
            self.frame_setter = match self.animate {
                tili_config::AnimationSpeed::Off => Box::new(InstantFrameSetter),
                tili_config::AnimationSpeed::Medium | tili_config::AnimationSpeed::High => {
                    Box::new(TweenedFrameSetter::new(animate_duration))
                }
            };
        } else if animate_duration != self.animate_duration {
            // Only the duration moved: update the existing tweened setter in
            // place so an unrelated reload doesn't drop in-flight animations.
            self.animate_duration = animate_duration;
            self.frame_setter.set_duration(animate_duration);
        }
        self.default_root_orientation = match config.settings.default_root_orientation.as_str() {
            "horizontal" => Some(tili_tree::Orientation::Horizontal),
            "vertical" => Some(tili_tree::Orientation::Vertical),
            _ => None,
        };

        self.relayout_all_visible();
    }

    /// Switches which mode's keybindings are active. Returns an error if
    /// `name` isn't a mode the config declares (the default mode, `"main"`,
    /// is always valid even with no keybindings configured for it yet).
    pub fn enter_mode(&mut self, name: &str) -> Result<(), String> {
        if name == DEFAULT_MODE || self.mode_bindings.contains_key(name) {
            self.current_mode = name.to_string();
            Ok(())
        } else {
            Err(format!("unknown keybinding mode '{name}'"))
        }
    }

    pub fn exit_mode(&mut self) {
        self.current_mode = DEFAULT_MODE.to_string();
    }

    /// The active mode's name — `"main"` unless a mode was entered and
    /// hasn't auto-exited/been exited yet.
    pub fn current_mode(&self) -> &str {
        &self.current_mode
    }

    /// Whether `current_mode` is a one-shot mode — see `auto_exit_modes`.
    pub fn current_mode_auto_exits(&self) -> bool {
        self.auto_exit_modes.contains(&self.current_mode)
    }

    /// Looks up the `Command` bound to `combo` in the current mode, if any
    /// — called when a hotkey press arrives from `tili_ax::spawn_hotkey_tap`.
    pub fn resolve_hotkey(&self, combo: KeyCombo) -> Option<Command> {
        self.mode_bindings
            .get(&self.current_mode)?
            .get(&combo)
            .cloned()
    }

    /// Every key combo bound in the current mode — kept in sync with the
    /// `Arc<Mutex<_>>` the hotkey tap reads synchronously (see
    /// `tili_ax::spawn_hotkey_tap`'s docs for why that's a `Mutex` and not
    /// routed through this state's normal single-owner-loop model).
    pub fn active_key_combos(&self) -> HashSet<KeyCombo> {
        self.mode_bindings
            .get(&self.current_mode)
            .map(|bindings| bindings.keys().copied().collect())
            .unwrap_or_default()
    }

    pub fn list_workspaces(&self) -> Vec<WorkspaceInfo> {
        let mut workspaces: Vec<WorkspaceInfo> = self
            .workspaces
            .iter()
            .map(|(name, tree)| {
                let monitor = self
                    .active_workspace
                    .iter()
                    .find(|(_, n)| *n == name)
                    .map(|(&id, _)| id);
                let last_monitor = self.workspace_monitor.get(name).copied();
                let synthetic = self.synthetic_workspaces.contains(name);
                let display_name = synthetic
                    .then(|| {
                        last_monitor
                            .and_then(|id| self.monitors.iter().find(|m| m.id == id))
                            .map(|m| m.pretty_name.clone())
                    })
                    .flatten();
                WorkspaceInfo {
                    name: name.clone(),
                    display_name,
                    synthetic,
                    active: self.active_workspace.values().any(|n| n == name),
                    window_count: tree.window_ids().len(),
                    monitor,
                    last_monitor,
                }
            })
            .collect();
        workspaces.sort_by(|a, b| a.name.cmp(&b.name));
        workspaces
    }

    /// Every connected monitor and what it's currently showing (M9).
    pub fn list_monitors(&self) -> Vec<MonitorInfo> {
        self.monitors
            .iter()
            .map(|m| MonitorInfo {
                id: m.id,
                ordinal: m.ordinal,
                pretty_name: m.pretty_name.clone(),
                is_main: m.is_main,
                focused: m.id == self.focused_monitor,
                active_workspace: self.active_workspace.get(&m.id).cloned(),
                frame: RectInfo {
                    x: m.frame.x,
                    y: m.frame.y,
                    width: m.frame.width,
                    height: m.frame.height,
                },
            })
            .collect()
    }

    /// Rules skipped by the most recent `apply_config` call — see this
    /// field's own doc comment. Backs `Command::Doctor`.
    pub fn config_warnings(&self) -> Vec<String> {
        self.config_warnings.clone()
    }

    /// The most recently loaded `menubar { }` config. Backs
    /// `Command::MenubarStyle`.
    pub fn menubar_style(&self) -> tili_ipc::MenubarStyle {
        self.menubar_style
    }

    /// Cycles `focused_monitor` to the next connected monitor, wrapping —
    /// a no-op with fewer than two monitors connected.
    pub fn focus_monitor_next(&mut self) {
        if let Some(id) = self.next_monitor_id() {
            self.record_previous_focus();
            self.focus_monitor_by_id(id);
        }
    }

    /// Focuses a specific monitor (`main`, `next`, `prev`, or an id). Unlike
    /// `focus_monitor_next`, an invalid target is an error rather than a
    /// silent no-op.
    pub fn focus_monitor_target(&mut self, target: tili_ipc::MonitorTarget) -> Result<(), String> {
        let id = self.resolve_monitor_target(target)?;
        if id != self.focused_monitor {
            self.record_previous_focus();
        }
        self.focus_monitor_by_id(id);
        Ok(())
    }

    /// Focuses `id` as the command target and raises its remembered focus.
    /// Shared by `focus-monitor`, `workspace`'s "already visible elsewhere"
    /// branch, and `workspace-back`, none of which should change layouts.
    fn focus_monitor_by_id(&mut self, id: u32) {
        self.focused_monitor = id;
        if let Some(workspace) = self.active_workspace.get(&id).cloned() {
            self.last_active_workspace.insert(id, workspace.clone());
            let restore = self.workspace_focus.get(&workspace).copied().or_else(|| {
                self.workspaces
                    .get(&workspace)
                    .and_then(Tree::default_focus)
            });
            if let Some(node) = restore {
                self.workspace_focus.insert(workspace, node);
                self.raise_focused();
            }
        }
    }

    /// The connected monitor id after `focused_monitor` in wraparound
    /// order, or `None` with fewer than two monitors connected — shared by
    /// `focus_monitor_next` and `resolve_monitor_target`'s
    /// `MonitorTarget::Next` arm, which must agree on exactly the same
    /// "next" monitor.
    fn next_monitor_id(&self) -> Option<u32> {
        self.adjacent_monitor_id(self.focused_monitor, 1)
    }

    /// `base`'s neighbor in the deterministic monitor list, wrapping in
    /// either direction. `delta` is `+1` for next / `-1` for previous.
    fn adjacent_monitor_id(&self, base: u32, delta: isize) -> Option<u32> {
        if self.monitors.len() < 2 {
            return None;
        }
        let ids: Vec<u32> = self.monitors.iter().map(|m| m.id).collect();
        let current_idx = ids.iter().position(|&id| id == base).unwrap_or(0);
        let len = isize::try_from(ids.len()).unwrap_or(1);
        let index = (isize::try_from(current_idx).unwrap_or(0) + delta).rem_euclid(len);
        ids.get(usize::try_from(index).unwrap_or(0)).copied()
    }

    /// Resolves a config `workspace "x" monitor="..."` value against the
    /// current monitor list. Accepts `"main"` and a numeric display id.
    fn resolve_workspace_pin(&self, spec: &str) -> Option<u32> {
        match spec {
            "main" => self
                .monitors
                .iter()
                .find(|monitor| monitor.is_main)
                .map(|monitor| monitor.id),
            _ => spec
                .parse::<u32>()
                .ok()
                .filter(|id| self.monitors.iter().any(|monitor| monitor.id == *id)),
        }
    }

    fn workspace_pinned_elsewhere(&self, workspace: &str, monitor_id: u32) -> bool {
        // An unconnected numeric pin still reserves the workspace: returning
        // `true` on `None` keeps it from being stolen while its display is
        // temporarily absent.
        self.workspace_pins
            .get(workspace)
            .is_some_and(|spec| self.resolve_workspace_pin(spec) != Some(monitor_id))
    }

    fn monitor_showing_workspace(&self, workspace: &str) -> Option<u32> {
        self.active_workspace
            .iter()
            .find(|(_, name)| name.as_str() == workspace)
            .map(|(&id, _)| id)
    }

    /// Remembers the `(display, workspace)` the user is looking at right
    /// before a navigation command, so `workspace-back` can return there
    /// even if the workspace later moves displays.
    fn record_previous_focus(&mut self) {
        if let Some(workspace) = self.active_workspace.get(&self.focused_monitor).cloned() {
            self.previous_focus = Some((self.focused_monitor, workspace));
        }
    }

    fn active_display_for_workspace(&self, workspace: &str) -> Option<u32> {
        self.monitor_showing_workspace(workspace)
    }

    /// Where `workspace X` should take the user: the display already showing
    /// it, then a connected pin, then the last display it lived on. `None`
    /// when it has never been shown (or its home disconnected), in which
    /// case the caller assigns it to the focused display.
    fn workspace_home(&self, workspace: &str) -> Option<u32> {
        if let Some(display) = self.active_display_for_workspace(workspace) {
            return Some(display);
        }
        if let Some(spec) = self.workspace_pins.get(workspace) {
            return self.resolve_workspace_pin(spec);
        }
        self.workspace_monitor
            .get(workspace)
            .copied()
            .filter(|id| self.monitors.iter().any(|monitor| monitor.id == *id))
    }

    /// A pinned workspace whose target display is currently disconnected.
    /// It stays reserved (never re-homed by accident), so `workspace X`
    /// surfaces a clear error instead of silently showing it elsewhere.
    fn workspace_pinned_to_disconnected_display(&self, workspace: &str) -> bool {
        self.workspace_pins
            .get(workspace)
            .is_some_and(|spec| self.resolve_workspace_pin(spec).is_none())
    }

    fn workspace_is_synthetic(&self, workspace: &str) -> bool {
        self.synthetic_workspaces.contains(workspace)
    }

    fn active_workspace_on_another_monitor(&self, workspace: &str, monitor_id: u32) -> bool {
        self.active_workspace
            .iter()
            .any(|(id, name)| *id != monitor_id && name == workspace)
    }

    /// Gives `monitor_id` an active workspace if it doesn't have one.
    /// Preference: pinned -> last active on this display -> first free
    /// declared workspace -> synthetic fallback. Never assigns the same
    /// workspace to two monitors.
    fn assign_workspace_for_monitor(&mut self, monitor_id: u32) {
        if self.active_workspace.contains_key(&monitor_id) {
            return;
        }

        let pinned = self
            .workspace_pins
            .iter()
            .filter(|(workspace, spec)| {
                self.resolve_workspace_pin(spec) == Some(monitor_id)
                    && self.workspaces.contains_key(*workspace)
                    && !self.active_workspace_on_another_monitor(workspace, monitor_id)
            })
            .map(|(workspace, _)| workspace.clone())
            .min();
        if let Some(workspace) = pinned {
            self.set_active_workspace(monitor_id, workspace);
            return;
        }

        if let Some(workspace) = self.last_active_workspace.get(&monitor_id).cloned()
            && self.workspaces.contains_key(&workspace)
            && !self.active_workspace_on_another_monitor(&workspace, monitor_id)
            && !self.workspace_pinned_elsewhere(&workspace, monitor_id)
        {
            self.set_active_workspace(monitor_id, workspace);
            return;
        }

        let declared = self
            .declared_workspaces
            .iter()
            .find(|workspace| {
                self.workspaces.contains_key(*workspace)
                    && !self.synthetic_workspaces.contains(*workspace)
                    && !self.active_workspace_on_another_monitor(workspace, monitor_id)
                    && !self.workspace_pinned_elsewhere(workspace, monitor_id)
            })
            .cloned()
            .or_else(|| {
                self.workspaces
                    .keys()
                    .filter(|workspace| {
                        !self.synthetic_workspaces.contains(*workspace)
                            && !self.active_workspace_on_another_monitor(workspace, monitor_id)
                            && !self.workspace_pinned_elsewhere(workspace, monitor_id)
                    })
                    .min()
                    .cloned()
            });
        if let Some(workspace) = declared {
            self.set_active_workspace(monitor_id, workspace);
            return;
        }

        let ordinal = self
            .monitors
            .iter()
            .find(|monitor| monitor.id == monitor_id)
            .map_or(monitor_id, |monitor| monitor.ordinal);
        let base = format!("display-{ordinal}");
        let mut name = base.clone();
        let mut suffix = 2_u32;
        while self.workspaces.contains_key(&name)
            || self.active_workspace_on_another_monitor(&name, monitor_id)
        {
            name = format!("{base}-{suffix}");
            suffix += 1;
        }
        self.workspaces.entry(name.clone()).or_default();
        self.synthetic_workspaces.insert(name.clone());
        self.set_active_workspace(monitor_id, name);
    }

    fn set_active_workspace(&mut self, monitor_id: u32, workspace: String) {
        self.workspaces.entry(workspace.clone()).or_default();
        self.active_workspace.insert(monitor_id, workspace.clone());
        self.workspace_monitor.insert(workspace.clone(), monitor_id);
        self.last_active_workspace.insert(monitor_id, workspace);
    }

    /// Ensures every connected monitor has an active workspace. Called after
    /// config/session load and after display changes; on a fresh two-display
    /// startup this is what prevents the secondary display from being left
    /// without any workspace.
    pub fn ensure_monitor_workspaces(&mut self) {
        let existing: HashSet<String> = self.workspaces.keys().cloned().collect();
        self.active_workspace
            .retain(|_, workspace| existing.contains(workspace));
        let current: Vec<(u32, String)> = self
            .active_workspace
            .iter()
            .map(|(&monitor, workspace)| (monitor, workspace.clone()))
            .collect();
        for (monitor, workspace) in current {
            self.last_active_workspace
                .entry(monitor)
                .or_insert_with(|| workspace.clone());
            self.workspace_monitor.insert(workspace, monitor);
        }
        let ids: Vec<u32> = self.monitors.iter().map(|monitor| monitor.id).collect();
        for id in ids {
            self.assign_workspace_for_monitor(id);
        }
        self.cleanup_synthetic_workspaces();
        self.renumber_synthetic_workspaces();
    }

    /// Startup/reconnect cleanup for synthetic display workspaces restored
    /// from a previous session: drop empty fallbacks, and migrate a
    /// non-empty fallback whose display is gone instead of leaving it
    /// parked under a stale name.
    fn cleanup_synthetic_workspaces(&mut self) {
        let names: Vec<String> = self.synthetic_workspaces.iter().cloned().collect();
        for name in names {
            if !self.workspaces.contains_key(&name) {
                continue;
            }
            let monitor = self.workspace_monitor.get(&name).copied();
            let connected =
                monitor.is_some_and(|id| self.monitors.iter().any(|monitor| monitor.id == id));
            let active = self
                .active_workspace
                .values()
                .any(|workspace| workspace == &name);
            if !connected {
                if self.workspace_has_windows(&name) {
                    let fallback = self.fallback_workspace_for(monitor.unwrap_or(0));
                    self.migrate_workspace_windows(&name, &fallback);
                } else {
                    self.remove_synthetic_workspace(&name);
                }
            } else if !active && !self.workspace_has_windows(&name) {
                self.remove_synthetic_workspace(&name);
            }
        }
    }

    /// Synthetic workspace ids carry the monitor's current ordinal
    /// (`display-2`). When the display layout changes, rename them so the id
    /// keeps matching the visible "Display N" label. Declared workspaces
    /// never change.
    fn renumber_synthetic_workspaces(&mut self) {
        let mut renames: Vec<(String, String)> = self
            .synthetic_workspaces
            .iter()
            .filter_map(|workspace| {
                let monitor = self.workspace_monitor.get(workspace)?;
                let monitor = self.monitors.iter().find(|m| m.id == *monitor)?;
                let base = format!("display-{}", monitor.ordinal);
                let mut desired = base.clone();
                let mut suffix = 2_u32;
                while desired != *workspace
                    && self.workspaces.contains_key(&desired)
                    && !self.synthetic_workspaces.contains(&desired)
                {
                    desired = format!("{base}-{suffix}");
                    suffix += 1;
                }
                (desired != *workspace).then(|| (workspace.clone(), desired))
            })
            .collect();
        if renames.is_empty() {
            return;
        }
        renames.sort();
        // Two-phase rename avoids transient collisions when two synthetic
        // workspaces swap ordinals.
        let mut temporaries = Vec::with_capacity(renames.len());
        for (index, (old, _)) in renames.iter().enumerate() {
            let temp = format!("__tili-display-tmp-{index}");
            self.rename_workspace(old, &temp);
            temporaries.push(temp);
        }
        for (temp, (_, new)) in temporaries.into_iter().zip(renames) {
            self.rename_workspace(&temp, &new);
        }
    }

    fn rename_workspace(&mut self, old: &str, new: &str) {
        if old == new {
            return;
        }
        let Some(tree) = self.workspaces.remove(old) else {
            return;
        };
        self.workspaces.insert(new.to_string(), tree);
        if let Some(node) = self.workspace_focus.remove(old) {
            self.workspace_focus.insert(new.to_string(), node);
        }
        if let Some(node) = self.fullscreen_focus.remove(old) {
            self.fullscreen_focus.insert(new.to_string(), node);
        }
        if let Some(monitor) = self.workspace_monitor.remove(old) {
            self.workspace_monitor.insert(new.to_string(), monitor);
        }
        for workspace in self.active_workspace.values_mut() {
            if workspace == old {
                *workspace = new.to_string();
            }
        }
        for workspace in self.last_active_workspace.values_mut() {
            if workspace == old {
                *workspace = new.to_string();
            }
        }
        for workspace in self.previous_workspaces.values_mut() {
            if workspace == old {
                *workspace = new.to_string();
            }
        }
        if let Some((_, workspace)) = self.previous_focus.as_mut()
            && workspace == old
        {
            *workspace = new.to_string();
        }
        for placement in self.placements.values_mut() {
            if placement.workspace == old {
                placement.workspace = new.to_string();
            }
        }
        for saved in &mut self.deferred_windows {
            if saved.workspace == old {
                saved.workspace = new.to_string();
            }
        }
        if self.synthetic_workspaces.remove(old) {
            self.synthetic_workspaces.insert(new.to_string());
        }
    }

    /// Fallback workspace that receives windows from a disconnected
    /// display's synthetic workspace: the main display's active workspace,
    /// then the focused monitor's, then a declared/default workspace.
    fn fallback_workspace_for(&self, disconnected: u32) -> String {
        if let Some(main) = self.monitors.iter().find(|monitor| monitor.is_main)
            && main.id != disconnected
            && let Some(name) = self.active_workspace.get(&main.id)
            && !self.synthetic_workspaces.contains(name)
        {
            return name.clone();
        }
        if self.focused_monitor != disconnected
            && let Some(name) = self.active_workspace.get(&self.focused_monitor)
            && !self.synthetic_workspaces.contains(name)
        {
            return name.clone();
        }
        self.workspaces
            .keys()
            .find(|name| !self.synthetic_workspaces.contains(*name))
            .cloned()
            .unwrap_or_else(|| DEFAULT_WORKSPACE.to_string())
    }

    /// Moves every placement in `from` into `to`, preserving tiled/floating
    /// character and anchors, then drops the now-empty source workspace.
    /// Used when a synthetic display workspace is cleaned up on disconnect;
    /// never silently closes a window.
    fn migrate_workspace_windows(&mut self, from: &str, to: &str) {
        if from == to {
            return;
        }
        self.workspaces.entry(to.to_string()).or_default();
        let ids: Vec<WindowId> = self
            .placements
            .iter()
            .filter(|(_, placement)| placement.workspace == from)
            .map(|(&id, _)| id)
            .collect();
        for id in ids {
            let Some(kind) = self.placements.get(&id).map(|p| p.kind.clone()) else {
                continue;
            };
            let anchor = self
                .workspaces
                .get(from)
                .and_then(|tree| tree.anchor_for(id))
                .unwrap_or_default();
            match kind {
                PlacementKind::Tiled => {
                    self.remove_from_tree(id, from);
                    self.insert_into_workspace_tree_at_anchor(id, to, false, &anchor);
                }
                PlacementKind::Floating { .. } => {
                    self.remove_from_tree(id, from);
                    self.insert_into_workspace_tree_at_anchor(id, to, true, &anchor);
                }
                PlacementKind::NativeFullscreen(_)
                | PlacementKind::Minimized(_)
                | PlacementKind::HiddenApplication(_)
                | PlacementKind::Popup => {}
            }
            if let Some(placement) = self.placements.get_mut(&id) {
                placement.workspace = to.to_string();
            }
        }
        for saved in &mut self.deferred_windows {
            if saved.workspace == from {
                saved.workspace = to.to_string();
            }
        }
        self.workspaces.remove(from);
        self.workspace_focus.remove(from);
        self.fullscreen_focus.remove(from);
        self.workspace_monitor.remove(from);
        self.synthetic_workspaces.remove(from);
        for workspace in self.active_workspace.values_mut() {
            if workspace == from {
                *workspace = to.to_string();
            }
        }
        for workspace in self.last_active_workspace.values_mut() {
            if workspace == from {
                *workspace = to.to_string();
            }
        }
        for workspace in self.previous_workspaces.values_mut() {
            if workspace == from {
                *workspace = to.to_string();
            }
        }
        if let Some((_, workspace)) = self.previous_focus.as_mut()
            && workspace == from
        {
            *workspace = to.to_string();
        }
    }

    /// Removes an empty synthetic workspace and its bookkeeping.
    fn workspace_has_windows(&self, name: &str) -> bool {
        self.placements
            .values()
            .any(|placement| placement.workspace == name)
    }

    fn remove_synthetic_workspace(&mut self, name: &str) {
        if !self.synthetic_workspaces.contains(name) {
            return;
        }
        if self.workspace_has_windows(name) {
            return;
        }
        self.workspaces.remove(name);
        self.workspace_focus.remove(name);
        self.fullscreen_focus.remove(name);
        self.workspace_monitor.remove(name);
        self.synthetic_workspaces.remove(name);
        let fallback = self.default_workspace_name();
        for workspace in self.last_active_workspace.values_mut() {
            if workspace == name {
                *workspace = fallback.clone();
            }
        }
        for workspace in self.previous_workspaces.values_mut() {
            if workspace == name {
                *workspace = fallback.clone();
            }
        }
        if self
            .previous_focus
            .as_ref()
            .is_some_and(|(_, workspace)| workspace == name)
        {
            self.previous_focus = None;
        }
    }

    /// Re-enumerates connected monitors in response to a hot-plug/unplug
    /// signal from `tili_ax::spawn_display_watcher`, diffing via
    /// `tili_ax::match_monitors` rather than a plain id set-diff — an id
    /// that changed but whose frame origin didn't (common across sleep/
    /// wake) is a `renamed` pair, remapped in place with no park/unpark
    /// churn, since that display never actually went anywhere. A genuinely
    /// disconnected monitor's active workspace is parked (its windows
    /// aren't lost, just no longer shown anywhere, exactly like switching
    /// away from it); a genuinely newly connected monitor gets a fresh,
    /// empty workspace. Every still-visible workspace is re-laid-out
    /// afterward since frames may have changed even for monitors that
    /// stayed connected (resolution or arrangement change).
    pub fn on_displays_changed(&mut self) {
        let new_monitors = tili_ax::list_monitors();
        self.on_displays_changed_with(new_monitors);
    }

    /// Testable half of `on_displays_changed`: the caller supplies the
    /// freshly-enumerated monitor list, so synthetic multi-display
    /// disconnect/rename/reconnect sequences can be exercised without real
    /// hardware. The zero-monitor guard stays here (both the real caller and
    /// tests need the same "treat it as sleep, keep the old snapshot"
    /// behavior).
    fn on_displays_changed_with(&mut self, new_monitors: Vec<Monitor>) {
        if new_monitors.is_empty() {
            // A momentary zero-display enumeration is, as far as observed,
            // always the whole system going to sleep (lid close with no
            // external display), not a real user action to react to — there's
            // nothing to lay out with no displays anyway. Returning here
            // without touching `self.monitors` keeps it as the pre-sleep
            // snapshot indefinitely, however long the sleep lasts, so the
            // eventual wake-time call diffs true before/after state and
            // `match_monitors`'s rename-pairing (built for exactly this) has
            // something to pair against instead of nothing — a disconnect
            // signal processed eagerly right before suspend would otherwise
            // zero `self.monitors` and strand the reconnect signal with no
            // baseline to compare against, spuriously creating a fresh
            // `monitor-<id>` workspace instead of restoring the real one.
            return;
        }
        let diff = tili_ax::match_monitors(&self.monitors, &new_monitors);

        self.monitors = new_monitors;

        for (old_id, new_id) in diff.renamed {
            if let Some(name) = self.active_workspace.remove(&old_id) {
                self.active_workspace.insert(new_id, name);
            }
            if let Some(name) = self.last_active_workspace.remove(&old_id) {
                self.last_active_workspace.insert(new_id, name);
            }
            if let Some(name) = self.previous_workspaces.remove(&old_id) {
                self.previous_workspaces.insert(new_id, name);
            }
            if let Some((monitor, _)) = self.previous_focus.as_mut()
                && *monitor == old_id
            {
                *monitor = new_id;
            }
            for monitor in self.workspace_monitor.values_mut() {
                if *monitor == old_id {
                    *monitor = new_id;
                }
            }
            let old_spec = old_id.to_string();
            let new_spec = new_id.to_string();
            for spec in self.workspace_pins.values_mut() {
                if *spec == old_spec {
                    *spec = new_spec.clone();
                }
            }
            if self.focused_monitor == old_id {
                self.focused_monitor = new_id;
            }
        }

        for id in diff.disconnected {
            if self
                .previous_focus
                .as_ref()
                .is_some_and(|(monitor, _)| *monitor == id)
            {
                self.previous_focus = None;
            }
            if let Some(name) = self.active_workspace.remove(&id) {
                self.last_active_workspace.insert(id, name.clone());
                if self.synthetic_workspaces.contains(&name) {
                    let fallback = self.fallback_workspace_for(id);
                    self.migrate_workspace_windows(&name, &fallback);
                } else {
                    self.park_workspace(&name);
                }
            }
            // A synthetic workspace associated with this display may be
            // parked rather than active; clean it too.
            let associated: Vec<String> = self
                .synthetic_workspaces
                .iter()
                .filter(|name| self.workspace_monitor.get(*name) == Some(&id))
                .cloned()
                .collect();
            for name in associated {
                if !self.active_workspace.values().any(|active| active == &name)
                    && self.workspace_has_windows(&name)
                {
                    let fallback = self.fallback_workspace_for(id);
                    self.migrate_workspace_windows(&name, &fallback);
                } else {
                    self.remove_synthetic_workspace(&name);
                }
            }
        }

        if !self.monitors.iter().any(|m| m.id == self.focused_monitor) {
            self.focused_monitor = self.monitors.first().map(|m| m.id).unwrap_or(0);
        }

        for id in diff.connected {
            self.assign_workspace_for_monitor(id);
        }

        // Remaining monitors may still have no workspace after a rename or
        // first startup-shaped event.
        self.ensure_monitor_workspaces();

        let active_ids: Vec<u32> = self.active_workspace.keys().copied().collect();
        for id in active_ids {
            self.relayout_monitor(id);
            self.reposition_floating_for_monitor(id, true);
        }

        // Do this last: the southeast-most display can have changed (the old
        // one disconnected, a new one connected, or the arrangement moved),
        // and windows parked at the old corner may now overlap another
        // monitor. Workspaces migrated above are already active and thus
        // excluded by `parked_positionable_ids`.
        self.repark_parked_windows();
    }

    /// Moves the focus pointer to the window adjacent to the current focus
    /// in `dir`, and raises/focuses it for real. Returns an error message
    /// (suitable for `Response::Err`) if there's nothing focused yet or
    /// nothing in that direction. Goes through `focus_in_direction`, not
    /// plain `navigate`, so this also cycles an Accordion container's
    /// active child when the focused window is one of its members (M7).
    pub fn focus(&mut self, dir: Direction) -> Result<(), String> {
        let current = self.focused_node().ok_or("no window is focused")?;
        let target = self
            .active_tree_mut()
            .focus_in_direction(current, dir)
            .ok_or("no window in that direction")?;
        self.set_focused_node(target);
        self.relayout_active();
        self.raise_focused();
        Ok(())
    }

    /// Re-resolves `workspace_focus` from whichever window real macOS
    /// currently considers focused, *before* any command that acts on it
    /// runs — not a reactive sync triggered by a notification/poll event
    /// arriving at some later, unpredictable time. Confirmed on real
    /// hardware that the reactive version has an unavoidable race: a
    /// background poll/notification updating `workspace_focus`
    /// asynchronously can easily still be stale by the time a hotkey fires
    /// moments later, since there's no ordering guarantee between "the poll
    /// noticed the click" and "the user's next keypress got processed."
    /// Other AX-based tiling WMs resolve this the same way, synchronously at
    /// the top of every command — resolve reality immediately before using
    /// it, rather than trusting a cache that was last updated by some
    /// independent, unsynchronized background event.
    /// Called once at the top of `dispatch()`, covering both socket- and
    /// hotkey-triggered commands; read-only queries skip it there
    /// (`dispatch::needs_focus_sync`), since none of them reads
    /// `workspace_focus`.
    ///
    /// Uses `tili_ax::AxWindow::system_focused_id` (the system-wide
    /// `kAXFocusedUIElementAttribute`, not "which app is frontmost then that
    /// app's focused window") — a floating panel/utility window can hold
    /// real AX focus without its owning app ever becoming
    /// `NSWorkspace.frontmostApplication`, which the app-first two-hop
    /// lookup would silently miss (the sync would keep resolving to
    /// whichever *other* app's window was frontmost before, so a command
    /// like `move-to-workspace` would act on that stale window instead of
    /// the floating one actually focused). See `sync_focus_from_pid`, used
    /// by `apply_windows_changed` instead, which *does* want the app-first
    /// lookup since it already knows the exact pid it just placed windows
    /// for.
    pub fn sync_focus_from_frontmost(&mut self) {
        if let Some(window_id) = tili_ax::AxWindow::system_focused_id() {
            self.sync_focus_to_window(window_id);
        }
    }

    /// Syncs `workspace_focus` to reflect a real OS focus change for `pid`'s
    /// currently AX-focused/main window — used by `apply_windows_changed`'s
    /// own re-sync (see its call site's doc comment), which already knows
    /// the specific pid it just placed windows for.
    fn sync_focus_from_pid(&mut self, pid: i32) {
        if let Some(window_id) = tili_ax::AxWindow::focused_id_for_pid(pid) {
            self.sync_focus_to_window(window_id);
        }
    }

    /// Shared core: given a `WindowId` known to hold real OS focus right
    /// now (however the caller resolved it — see `sync_focus_from_frontmost`
    /// and `sync_focus_from_pid`), updates `workspace_focus` bookkeeping to
    /// match. A no-op if: the window isn't one of ours, it's neither
    /// `Tiled` nor `Floating` (every other special state has no tree node
    /// to focus), or it's already the recorded focus for its workspace —
    /// deliberately doesn't relayout or raise anything, since the OS
    /// already did the actual focusing here; this only updates internal
    /// bookkeeping to match reality.
    fn sync_focus_to_window(&mut self, window_id: WindowId) {
        let Some(placement) = self.placements.get(&window_id) else {
            return;
        };
        if !matches!(
            placement.kind,
            PlacementKind::Tiled | PlacementKind::Floating { .. }
        ) {
            return;
        }
        let workspace = placement.workspace.clone();
        let Some(tree) = self.workspaces.get_mut(&workspace) else {
            return;
        };
        let Some(node) = tree.node_for_window(window_id) else {
            return;
        };
        if self.workspace_focus.get(&workspace) == Some(&node) {
            return;
        }
        tree.record_focus(node);
        self.workspace_focus.insert(workspace, node);
    }

    /// The focused node, rejected if there isn't one or if it's a
    /// `Floating` leaf and `root` is `false` — the guard shared by
    /// `move_focused`/`join` (always `root: false`, since a tiled-topology
    /// command is never meaningful on a floating window) and
    /// `set_orientation`/`toggle_layout`/`set_layout`/`balance_sizes`
    /// (`root: true` skips the floating check, since targeting the
    /// workspace root doesn't depend on which node is focused). Not used
    /// by `resize`/`toggle_fullscreen`'s non-native branch — same shape,
    /// but a distinct error message each caller already relies on.
    fn require_focused_tiled_node(&self, root: bool) -> Result<NodeId, String> {
        let current = self.focused_node().ok_or("no window is focused")?;
        if !root && self.focused_window_is_floating(current) {
            return Err("no tiled window is focused".to_string());
        }
        Ok(current)
    }

    /// Moves the focused window one step in `dir`, re-parenting it through
    /// the tree (see `Tree::move_in_direction`) rather than just swapping
    /// which window sits where — the moved window keeps its own `NodeId`,
    /// so it stays "the focused one" without needing to look up a target.
    pub fn move_focused(&mut self, dir: Direction) -> Result<(), String> {
        let current = self.require_focused_tiled_node(false)?;
        if !self.active_tree_mut().move_in_direction(current, dir) {
            return Err("no window in that direction".to_string());
        }
        self.set_focused_node(current);
        self.relayout_active();
        self.raise_focused();
        Ok(())
    }

    /// Wraps the focused window and its neighbor in `dir` into a new,
    /// perpendicular container.
    pub fn join(&mut self, dir: Direction) -> Result<(), String> {
        let current = self.require_focused_tiled_node(false)?;
        if self.active_tree_mut().join_with(current, dir) {
            self.relayout_active();
            Ok(())
        } else {
            Err("nothing to join in that direction".to_string())
        }
    }

    /// Grows/shrinks the focused window's share of its nearest tiled
    /// container by `amount` (weight-space, not pixels — see
    /// `Tree::resize_weight`).
    pub fn resize(&mut self, amount: f32) -> Result<(), String> {
        let current = self.focused_node().ok_or("no window is focused")?;
        if self.focused_window_is_floating(current) {
            return Err("nothing to resize — no tiled container here".to_string());
        }
        if self.active_tree_mut().resize_weight(current, amount) {
            self.relayout_active();
            Ok(())
        } else {
            Err("nothing to resize — no tiled container here".to_string())
        }
    }

    /// Sets the focused window's parent container's orientation (or the
    /// workspace root's, if `root`).
    pub fn set_orientation(
        &mut self,
        orientation: tili_tree::Orientation,
        root: bool,
    ) -> Result<(), String> {
        let current = self.require_focused_tiled_node(root)?;
        let changed = if root {
            self.active_tree_mut().set_root_orientation(orientation)
        } else {
            self.active_tree_mut().set_orientation(current, orientation)
        };
        if changed {
            self.relayout_active();
            Ok(())
        } else {
            Err("nothing to set — only one window here".to_string())
        }
    }

    /// Flips the focused window's parent container's orientation between
    /// horizontal and vertical (or the workspace root's, if `root`) — same
    /// contract as `set_orientation`, but reads the current axis instead of
    /// requiring the caller to name one explicitly. A lone window/root has
    /// no orientation yet, so that case defaults to flipping away from
    /// `Horizontal` (i.e. to `Vertical`), matching `root_orientation_hint`'s
    /// existing default-to-horizontal convention for a fresh root.
    pub fn toggle_orientation(&mut self, root: bool) -> Result<(), String> {
        let current = self.focused_node().ok_or("no window is focused")?;
        let existing = if root {
            self.active_tree().root_orientation()
        } else {
            self.active_tree().orientation_of(current)
        };
        let next = match existing.unwrap_or(tili_tree::Orientation::Horizontal) {
            tili_tree::Orientation::Horizontal => tili_tree::Orientation::Vertical,
            tili_tree::Orientation::Vertical => tili_tree::Orientation::Horizontal,
        };
        self.set_orientation(next, root)
    }

    /// Toggles a container between `Split` (tiled) and `Accordion`
    /// (stacked, one visible at a time) — the focused window's immediate
    /// parent, or (`root: true`) the workspace's root container instead
    /// (see `Tree::toggle_root_layout` — still a single container, not a
    /// recursive apply-to-everything). Errors if nothing's focused, or if
    /// the target container is a lone window with no container to toggle.
    pub fn toggle_layout(&mut self, root: bool) -> Result<(), String> {
        let current = self.require_focused_tiled_node(root)?;
        let toggled = if root {
            self.active_tree_mut().toggle_root_layout()
        } else {
            self.active_tree_mut().toggle_layout(current)
        };
        if toggled {
            self.relayout_active();
            Ok(())
        } else {
            Err("nothing to toggle — only one window here".to_string())
        }
    }

    /// Sets a container (focused window's parent, or the workspace root if
    /// `root: true` — see `toggle_layout`) to a specific layout kind — a
    /// no-op if it's already that kind, otherwise the same toggle
    /// `toggle_layout` does (there are only two kinds, so "set" and
    /// "toggle away from the other one" are the same operation).
    pub fn set_layout(&mut self, kind: tili_ipc::LayoutKind, root: bool) -> Result<(), String> {
        let current = self.require_focused_tiled_node(root)?;
        let is_accordion = if root {
            self.active_tree().is_root_accordion()
        } else {
            self.active_tree().is_accordion_container(current)
        };
        let want_accordion = matches!(kind, tili_ipc::LayoutKind::Accordion);
        if is_accordion == want_accordion {
            return Ok(());
        }
        let toggled = if root {
            self.active_tree_mut().toggle_root_layout()
        } else {
            self.active_tree_mut().toggle_layout(current)
        };
        if toggled {
            self.relayout_active();
            Ok(())
        } else {
            Err("nothing to set — only one window here".to_string())
        }
    }

    /// Resets every child weight of the focused window's parent container
    /// (or the workspace root, if `root`) evenly, undoing any manual
    /// `resize_weight` calls. Same dual-target split as
    /// `set_orientation`/`toggle_layout`.
    pub fn balance_sizes(&mut self, root: bool) -> Result<(), String> {
        let current = self.require_focused_tiled_node(root)?;
        if self.active_tree_mut().balance_weights(current, root) {
            self.relayout_active();
            Ok(())
        } else {
            Err("nothing to balance — only one window here".to_string())
        }
    }

    /// Toggles the focused window fullscreen. `native` selects macOS's own
    /// `AXFullScreen` (a separate Space); tili itself doesn't need to change
    /// anything about the window's placement for that — the existing
    /// `apply_windows_changed`/`reconcile_existing_placement` machinery
    /// (Phase 1) already demotes it to `PlacementKind::NativeFullscreen`
    /// once macOS reports the `AXFullScreen` flag on the next scan, and
    /// promotes it back the same way when it exits. Non-native ("tiled")
    /// fullscreen is tili's own: the window stays exactly where it is in
    /// the tree, just laid out at the monitor's full frame until toggled
    /// off (see `fullscreen_focus` and `relayout_monitor`'s special case).
    pub fn toggle_fullscreen(&mut self, native: bool) -> Result<(), String> {
        if native {
            let id = self.native_fullscreen_target(AxWindow::system_focused_id())?;
            let window = self
                .windows
                .get_mut(&id)
                .ok_or("focused window not found")?;
            let next = !window.fullscreen();
            window.set_native_fullscreen(next);
            return Ok(());
        }
        let current = self.focused_node().ok_or("no window is focused")?;
        if self.focused_window_is_floating(current) {
            return Err("no tiled window is focused".to_string());
        }
        let workspace = self.active_workspace_name();
        if self.fullscreen_focus.get(&workspace) == Some(&current) {
            self.fullscreen_focus.remove(&workspace);
        } else {
            self.fullscreen_focus.insert(workspace, current);
        }
        self.relayout_active();
        Ok(())
    }

    /// Which window native `toggle_fullscreen` acts on, given whatever
    /// window macOS itself reports as focused right now (`real_focus`,
    /// resolved by the caller via `AxWindow::system_focused_id` — split out
    /// so the decision is unit-testable without a live `AXUIElement`).
    ///
    /// Deliberately not `focused_node()`. `demote_to_special` takes a window
    /// out of its workspace tree the moment macOS reports `AXFullScreen`, so
    /// a natively-fullscreened window can never be what `workspace_focus`
    /// points at — which made the command strictly one-way. Confirmed on
    /// real hardware, both shapes: with that window alone in its workspace
    /// the tree was empty and the command failed outright ("no window is
    /// focused"); with a sibling present, `workspace_focus` had already been
    /// reassigned to the sibling and toggling fullscreened *that* instead of
    /// exiting, leaving two fullscreen windows.
    ///
    /// `dispatch()`'s own focus sync can't close this from the other end:
    /// `sync_focus_to_window` only records `Tiled`/`Floating` placements, so
    /// the real focus it resolves is discarded for exactly the window this
    /// command needs. Hence reading real focus directly here.
    ///
    /// Scoped to the active workspace so a stale read can't retarget some
    /// unrelated window, and falls back to the tree node otherwise — for an
    /// ordinary, not-currently-fullscreen window the two agree anyway.
    fn native_fullscreen_target(&self, real_focus: Option<WindowId>) -> Result<WindowId, String> {
        if let Some(id) = real_focus
            && self
                .placements
                .get(&id)
                .is_some_and(|p| p.workspace == self.active_workspace_name())
        {
            return Ok(id);
        }
        let current = self.focused_node().ok_or("no window is focused")?;
        self.active_tree()
            .window_at(current)
            .ok_or_else(|| "focused window not found".to_string())
    }

    /// Sends the focused window an `AXCloseButton` press (best-effort) and
    /// returns immediately — never assumes the window is actually gone;
    /// the existing destroy-event/grace-period path in
    /// `apply_windows_changed` reconciles that once macOS actually reports
    /// it, same as a user-initiated close via any other method.
    pub fn close_focused(&mut self) -> Result<(), String> {
        let current = self.focused_node().ok_or("no window is focused")?;
        let id = self
            .active_tree()
            .window_at(current)
            .ok_or("focused window not found")?;
        let window = self.windows.get(&id).ok_or("focused window not found")?;
        window.close();
        Ok(())
    }

    /// Focuses (and raises) the first known window whose title or bundle id
    /// contains `query`, switching that window's workspace onto
    /// `focused_monitor` first if it isn't currently visible anywhere (or
    /// onto whatever monitor already shows it, if any). Errors (rather than
    /// launching anything) if no window matches.
    pub fn summon(&mut self, query: &str) -> Result<(), String> {
        let id = self
            .windows
            .values()
            .find(|w| w.title().contains(query) || w.bundle_id().is_some_and(|b| b.contains(query)))
            .map(AxWindow::id)
            .ok_or_else(|| format!("no window matching '{query}'"))?;

        let Some(placement) = self.placements.get(&id) else {
            return Err("window has no placement".to_string());
        };
        let workspace = placement.workspace.clone();
        // `Tiled` and `Floating` are both addressable via a tree `NodeId`
        // (see `tili_tree::Node::Floating`) — every other kind (Popup,
        // Minimized, ...) has no node for `find_node` below to return.
        let addressable = matches!(
            placement.kind,
            PlacementKind::Tiled | PlacementKind::Floating { .. }
        );

        let visible_on = self
            .active_workspace
            .iter()
            .find(|(_, w)| **w == workspace)
            .map(|(&mid, _)| mid);
        match visible_on {
            Some(monitor_id) => self.focused_monitor = monitor_id,
            None => self.switch_workspace(&workspace)?,
        }

        if addressable
            && let Some(tree) = self.workspaces.get(&workspace)
            && let Some(node) = tree.find_node(id)
        {
            self.set_focused_node(node);
        }

        self.raise_focused_window(id);
        Ok(())
    }

    /// Reveals whatever window `pid` currently has AX-focused, in response
    /// to `WmEvent::FrontmostAppChanged` (Cmd-Tab, or clicking another app
    /// via Mission Control/Control Center — pure OS-level frontmost changes
    /// that never otherwise route through `dispatch()`). Mirrors `summon`'s
    /// body exactly, just resolving the target window via
    /// `AxWindow::focused_id_for_pid` instead of a title/bundle-id text
    /// query — same "switch onto whichever monitor already shows it, or
    /// reveal it on `focused_monitor` if it's parked" behavior. A silent
    /// no-op if the pid has no focused window or that window has no
    /// placement (e.g. it's a `Popup`-classified element) — unlike `summon`,
    /// there's no user-facing error to report this to.
    ///
    /// One exception to "always follow": macOS itself changes the
    /// frontmost app when the current one closes its last window — it
    /// reactivates whichever app was frontmost before, producing the exact
    /// same kind of pid-change edge `FrontmostAppChanged` fires on for a
    /// real Cmd-Tab. Blindly following that yanks the display onto whatever
    /// workspace the reactivated app happens to live in (often wherever the
    /// user was before switching to the workspace they just emptied) even
    /// though nothing was asked for. `last_frontmost_pid` distinguishes the
    /// two: if the previously-seen pid no longer owns any live window at
    /// all, this change is almost certainly that OS reactivation rather
    /// than a user gesture, so the workspace jump is skipped.
    ///
    /// A second, similarly-shaped case is handled at the very top of this
    /// function instead, before even the windowless guard: Spotlight, the
    /// Dock, and Notification Center (`SYSTEM_UI_BUNDLE_IDS`) only ever own
    /// `Popup`-classified windows, not none at all — so when *one of them*
    /// is transiently the pid being resolved, it sails past the windowless
    /// guard above and would otherwise corrupt `last_frontmost_pid` with a
    /// pid unrelated to any real app transition, the exact failure the
    /// windowless guard exists to prevent, just via a process that happens
    /// to own a window instead of none. Confirmed on real hardware:
    /// dismissing a notification banner's close button (or Spotlight with
    /// Esc) routinely holds AX-frontmost status on the system-UI process
    /// itself for close to or over `REVEAL_DEBOUNCE`, so the transient
    /// activation settles as its own call rather than coalescing with the
    /// handback — letting the corruption stick and dragging the *next*
    /// call (for whatever real app macOS reactivates) into treating a
    /// passive dismissal as a genuine pid transition to follow. Bailing out
    /// unconditionally whenever the *current* pid is system UI — before
    /// `last_frontmost_pid` is ever written — means `previous_pid` can
    /// never be system-UI on a later call either, so there's no "system-UI
    /// previous pid" case left for this function's `suppress` logic to
    /// special-case at all; a deliberate pick (a Spotlight result, a
    /// notification's body) is unaffected, since the app that gets
    /// activated as a *result* of that pick is itself a distinct, later,
    /// non-system-UI pid that reaches the normal `suppress` check below on
    /// its own call. This avoids both prior failure modes: it isn't the
    /// windowless-only guard's blind spot (Popup windows do exist here),
    /// and it isn't a separate stale-prone "last real pid" history (no new
    /// field — `last_frontmost_pid` itself is simply never written by a
    /// transient system-UI read, so it can't go stale the way a
    /// last-observed-pid tracker did).
    ///
    /// None of the above fires at all for a Dock icon click, though:
    /// unlike Spotlight, `Dock.app` never becomes the AX/`NSWorkspace`
    /// frontmost application while handling one, so if the clicked app was
    /// already the OS's nominal frontmost app (nothing else was competing
    /// for it — the common case when the current workspace is empty),
    /// `frontmost_app_pid()` reads identically before and after the click.
    /// There's no pid edge for `FrontmostAppChanged` to ever fire on, so
    /// this function never runs at all — confirmed on real hardware (there's
    /// no real OS-level transition here at all, so neither a poll nor a
    /// push notification has anything to fire on). See
    /// `reveal_current_frontmost` for how that's handled instead.
    ///
    /// One more guard lives inside the `None`/not-visible branch below: if
    /// `pending_launch_pids` is non-empty, `pid` might be a stale or merely
    /// transient read rather than the user's real target — e.g. Spotlight
    /// closing right after launching a cold app briefly reports the
    /// *previous* frontmost app again (a real, if short-lived, AX
    /// transition `FrontmostAppChanged` genuinely fires on) before the new
    /// app takes over; blindly following it would switch away to wherever
    /// that previous pid happens to live. This is why both trigger sources
    /// — a Dock click and a real `FrontmostAppChanged` transition — funnel
    /// through `reveal_current_frontmost` and its `main.rs`-side debounce
    /// rather than calling this function directly: see that function's doc
    /// comment and `pending_launch_pids`'s doc comment on `WmState` for the
    /// full mechanism.
    ///
    /// This whole function is a no-op while `wake_lock_active` is set (see
    /// that field's doc comment) — `frontmost_app_pid()` is a synchronous
    /// `AXFocusedApplication` query, and right after a real wake it can
    /// transiently report a different app than the one genuinely frontmost
    /// while the system's AX/WindowServer connections are still settling,
    /// *even for an app whose own pid has already confirmed responsive* —
    /// so this can't be narrowed to a per-pid check the way
    /// `finalize_expired_removals` is. Without this, that misread pid still
    /// owns live windows (so `suppress` above doesn't catch it either) and
    /// can force-switch the active workspace, or just silently steal focus
    /// to the wrong monitor/window, before the wake has actually settled —
    /// the exact symptom `note_system_wake` was introduced to fix, just
    /// through this path instead of `finalize_expired_removals`.
    ///
    /// `allow_unchanged_pid` controls whether the `None`/not-visible branch
    /// below is allowed to switch workspaces when `pid` is the *same* pid
    /// `last_frontmost_pid` already recorded (`pid_unchanged`). A Dock icon
    /// click needs `true`: per this doc comment's "Dock icon click" section
    /// above, re-clicking an already-frontmost app produces no pid edge at
    /// all, so `pid_unchanged` is expected to be true for the one legitimate
    /// case this function exists to handle for a click. A
    /// notification-detected `WmEvent::FrontmostAppChanged` needs `false`: by
    /// the time this
    /// deferred call actually runs, a same-pid read means nothing has
    /// *really* changed since `last_frontmost_pid` was last set to a real,
    /// window-owning pid — chasing it anyway reverts a workspace switch the
    /// user made in the meantime for no reason (confirmed on real hardware:
    /// rapidly hopping to an empty workspace can transiently reassign
    /// AX-frontmost to a windowless system process — WindowServer, Dock —
    /// during `park()`'s off-screen window move, immediately reverting back;
    /// see `last_frontmost_pid`'s update point below for the other half of
    /// this fix).
    ///
    /// `defer_unchanged_pid` and the `bool` return are the retry half of the
    /// click path: a same-pid, not-visible reveal is ambiguous (the click may
    /// have been aimed at an already-running app with no windows, which
    /// spawns no new pid), so with `defer_unchanged_pid` it returns `true`
    /// without switching and lets `main.rs` re-check later. See
    /// `reveal_current_frontmost`'s doc comment for the whole mechanism.
    pub fn reveal_frontmost(
        &mut self,
        pid: i32,
        allow_unchanged_pid: bool,
        defer_unchanged_pid: bool,
    ) -> bool {
        // Bail out before touching any state at all when `pid` is itself
        // transient system-UI chrome (Spotlight, Notification Center, ...)
        // — unlike a windowless process, these *do* own a real (`Popup`)
        // window, so the guard below alone can't catch them; see this
        // function's doc comment and `SYSTEM_UI_BUNDLE_IDS`'s.
        if is_system_ui_bundle(tili_ax::bundle_id_for_pid(pid).as_deref()) {
            return false;
        }
        // See this function's doc comment on `wake_lock_active` — bails
        // out before touching *any* state (not just before the workspace
        // switch below), including `last_frontmost_pid`: this whole
        // function is inert while the lock holds, so there's nothing here
        // worth recording yet. `raise_focused_window` still updates
        // `last_frontmost_pid` independently for any real dispatch()-driven
        // focus change, so nothing goes stale while this is skipped.
        if self.wake_lock_active {
            return false;
        }
        // Deliberately updated only once `pid` is confirmed to own a real
        // focused window, not unconditionally at the top of the function —
        // a windowless system process (WindowServer, Dock) transiently and
        // spuriously becoming AX-frontmost during `park()`'s off-screen
        // window move would otherwise overwrite `last_frontmost_pid` with a
        // pid that has nothing to do with any real transition, making the
        // *next* call (for the real, unchanged app) wrongly compute
        // `pid_unchanged = false` and chase it as if it were a genuine
        // Cmd-Tab.
        let Some(id) = AxWindow::focused_id_for_pid(pid) else {
            return false;
        };
        let previous_pid = self.last_frontmost_pid.replace(pid);
        let pid_unchanged = previous_pid == Some(pid);

        let Some(placement) = self.placements.get(&id) else {
            return false;
        };
        let workspace = placement.workspace.clone();
        // `Tiled` and `Floating` are both addressable via a tree `NodeId`
        // (see `tili_tree::Node::Floating`) — every other kind (Popup,
        // Minimized, ...) has no node for `find_node` below to return.
        let addressable = matches!(
            placement.kind,
            PlacementKind::Tiled | PlacementKind::Floating { .. }
        );

        let visible_on = self
            .active_workspace
            .iter()
            .find(|(_, w)| **w == workspace)
            .map(|(&mid, _)| mid);
        // Whether this call actually revealed/moved anything — distinct
        // from `pid_unchanged` below, since `reveal_current_frontmost`
        // (the mouse-click fallback) calls this with a pid that often
        // hasn't changed at all, and a call that's a total no-op shouldn't
        // re-raise (see `pid_unchanged` below).
        let mut did_reveal = false;
        match visible_on {
            Some(monitor_id) => {
                if self.focused_monitor != monitor_id {
                    self.focused_monitor = monitor_id;
                    did_reveal = true;
                }
            }
            None => {
                // `None` previous_pid (no prior event this run) never
                // suppresses — only a *confirmed* "that pid has zero live
                // windows left" does. Excludes `pending_removal` too, not
                // just presence in `windows` — a just-closed window sits
                // there for `removal_grace` before actually dropping out,
                // so a `FrontmostAppChanged` arriving inside that window
                // would otherwise still see it as "live" and fail to
                // suppress. Also excludes `Popup` placements: those get
                // tracked like any other window, landing in whatever
                // workspace was active when they opened, but they're
                // transient system chrome rather than a real window the
                // user is looking at — a still-open one shouldn't count
                // as "the previous pid is still alive" and defeat the
                // suppression below. Unlike
                // `Minimized`/`NativeFullscreen`/`HiddenApplication`, which
                // stay in `self.windows` too but represent a genuinely
                // still-open window in a special display state, so those
                // three are deliberately *not* excluded here. `previous_pid`
                // itself can never be system UI by this point — the guard
                // at the top of this function stops `last_frontmost_pid`
                // from ever being set to one — so there's no separate
                // system-UI carve-out needed here anymore.
                let suppress = previous_pid.is_some_and(|prev| {
                    !self.windows.iter().any(|(wid, w)| {
                        w.pid() == prev
                            && !self.pending_removal.contains_key(wid)
                            && !matches!(
                                self.placements.get(wid).map(|p| &p.kind),
                                Some(PlacementKind::Popup)
                            )
                    })
                });
                // `pending_launch_pids` non-empty means some pid launched
                // moments ago but doesn't have a window yet — `pid` here
                // could be a stale/transient read (the real target hasn't
                // taken over AX-frontmost status yet), so don't trust it
                // enough to switch away from wherever it points. This is
                // the real enforcement point; `reveal_current_frontmost`'s
                // own top-of-function check is just a short-circuit on top
                // of it, not a separate guard.
                if suppress || !self.pending_launch_pids.is_empty() {
                    return false;
                }
                // See this function's doc comment on `allow_unchanged_pid`:
                // a same-pid read this function didn't already permit
                // chasing means nothing really changed since the last
                // resolved-to-a-real-window pid, so there's nothing to
                // reveal.
                if pid_unchanged {
                    if !allow_unchanged_pid {
                        return false;
                    }
                    // Ambiguous click: same frontmost pid as before, on a
                    // click path allowed to chase it. The click may have
                    // been aimed at a *different* app whose activation
                    // hasn't taken over AX-frontmost yet — re-opening an
                    // already-running app with no windows from its Dock
                    // icon spawns no new pid, so `click_started_a_launch`
                    // can't see it. Ask the caller to retry instead of
                    // switching now; any real activation in the meantime
                    // re-arms the deadline with `allow_unchanged_pid =
                    // false`, and a launch's own placement bumps
                    // `switch_epoch`, so both supersede this reveal.
                    if defer_unchanged_pid {
                        return true;
                    }
                }
                eprintln!(
                    "tili-daemon: auto-switching to workspace '{workspace}' to reveal frontmost pid {pid} (previous_pid={previous_pid:?})"
                );
                if self.switch_workspace(&workspace).is_err() {
                    return false;
                }
                did_reveal = true;
            }
        }

        if addressable
            && let Some(tree) = self.workspaces.get(&workspace)
            && let Some(node) = tree.find_node(id)
        {
            self.set_focused_node(node);
        }

        // A same-pid, nothing-moved call (only `reveal_current_frontmost`
        // produces one) is a true no-op — skip re-focusing/re-warping the
        // cursor for `mouse_follows_focus`. Any real pid transition (Cmd-Tab,
        // Mission Control, ...) always raises, matching prior behavior,
        // since `mouse_follows_focus` should track *those* too even when the
        // target was already visible on the current monitor.
        if !pid_unchanged || did_reveal {
            self.raise_focused_window(id);
        }
        false
    }

    /// Fallback for a Dock icon click reactivating an app that was already
    /// the OS's nominal frontmost application — see `reveal_frontmost`'s
    /// doc comment for why that leaves no pid edge for
    /// `WmEvent::FrontmostAppChanged` to fire on. Called from `main.rs`'s
    /// `maintenance_tick`, deferred by `REVEAL_DEBOUNCE` from the
    /// triggering `MouseSignal::ButtonUp` *or* `WmEvent::FrontmostAppChanged`
    /// — never called synchronously from either — with whatever
    /// `frontmost_app_pid()` reports at that later point, regardless of
    /// whether it differs from last time. Safe to call this often:
    /// `reveal_frontmost` treats a same-pid, already-visible call as a true
    /// no-op (see `pid_unchanged`/`did_reveal` there), so a deferred check
    /// that turns out to have nothing to do with switching apps costs one
    /// AX query and does nothing further.
    ///
    /// The `pending_launch_pids` check up front is a short-circuit, not the
    /// real guard — `reveal_frontmost` enforces it itself (see its doc
    /// comment). Checking it here as well just skips a real
    /// `frontmost_app_pid()` AX query when the answer is already known to
    /// be discarded. `main.rs`'s deferral (through `pending_reveal_deadline`)
    /// is what gives a same-trigger `AppLaunched` event (a different,
    /// independent async source, no ordering guarantee against either
    /// `MouseSignal::ButtonUp` or `WmEvent::FrontmostAppChanged`) time to
    /// land in `pending_launch_pids` before this runs; the real "how long
    /// to keep distrusting reads" question is answered by that pid actually
    /// getting a window (`apply_windows_changed`) or `LAUNCH_GRACE_PERIOD`
    /// expiring, not by the fixed debounce.
    ///
    /// `allow_unchanged_pid` is forwarded to `reveal_frontmost` as-is — see
    /// its doc comment. `main.rs` passes `true` when a `MouseSignal::ButtonUp`
    /// contributed to the pending deferred check, `false` otherwise (a pure
    /// `WmEvent::FrontmostAppChanged` notification edge).
    ///
    /// `app_pids_at_click` is `tili_ax::workspace::regular_app_pids()`
    /// snapshotted on `MouseSignal::ButtonDown`, and `None` for the
    /// notification-driven path. It closes a gap neither
    /// `pending_launch_pids` nor `suppress` can: a Dock click that *starts
    /// launching* an app leaves `frontmost_app_pid()` still reporting the
    /// previously-frontmost app until the new one takes over AX-frontmost
    /// status — and on an empty workspace that previous app is typically
    /// one whose window is parked on another workspace, so `suppress` is
    /// false (it owns a live window) and the reveal switches to *that*
    /// workspace for a frame, before the new window's own placement
    /// switches back. `pending_launch_pids` only knows about launches
    /// `WmEvent::AppLaunched` actually reported, and is cleared by the
    /// launch's first scanned window — a splash/ignored window can clear it
    /// before the real one exists. A Dock-visible pid that wasn't running
    /// at mouse-down is proof this click started a launch without relying
    /// on any notification, and `mouse-down` is deliberately earlier than
    /// the Dock's own mouse-up launch: a mouse-up snapshot could already
    /// contain the new pid. The launch's `place_new_window`
    /// (`workspace-rules`) switches to the right workspace as soon as the
    /// real window exists, so dropping the reveal is the whole fix.
    ///
    /// `defer_unchanged_pid` is forwarded to `reveal_frontmost` as-is: when
    /// true, a same-pid click reveal whose target workspace isn't visible is
    /// *deferred* (this function returns `true`) instead of switching
    /// immediately. `main.rs` re-arms the deadline a bounded number of times
    /// for exactly that outcome. This closes the other half of the same
    /// race: clicking the Dock icon of an **already-running app with no
    /// windows** re-activates it without spawning a new pid, so
    /// `app_pids_at_click` can't see it — the app takes over AX-frontmost a
    /// moment later, and the retry (or that app's own `FrontmostAppChanged`
    /// re-arm) then resolves the correct pid. A genuinely unchanged pid
    /// (re-clicking the already-frontmost app's own Dock icon) still
    /// switches, just after the bounded wait; the retries stop immediately
    /// on any real activation, launch, or workspace switch.
    ///
    /// One known gap remains: a launch whose process hasn't registered with
    /// `NSWorkspace` *and* hasn't taken AX-frontmost within the whole retry
    /// window. Closing it would mean resolving the clicked Dock tile via
    /// AX, which the localized tile titles make fragile.
    ///
    /// Returns whether the reveal was deferred for a retry (see
    /// `defer_unchanged_pid`); `false` means there is nothing left to do.
    pub fn reveal_current_frontmost(
        &mut self,
        allow_unchanged_pid: bool,
        app_pids_at_click: Option<&HashSet<i32>>,
        defer_unchanged_pid: bool,
    ) -> bool {
        if !self.pending_launch_pids.is_empty() {
            return false;
        }
        // See this function's doc comment on `app_pids_at_click`.
        if let Some(app_pids_at_click) = app_pids_at_click
            && Self::click_started_a_launch(
                app_pids_at_click,
                &tili_ax::workspace::regular_app_pids(),
            )
        {
            trace::log(format_args!(
                "tili-daemon: click reveal skipped: an app launch appeared since mouse-down"
            ));
            return false;
        }
        if let Some(pid) = tili_ax::workspace::frontmost_app_pid() {
            return self.reveal_frontmost(pid, allow_unchanged_pid, defer_unchanged_pid);
        }
        false
    }

    /// Whether a Dock-visible app that wasn't running when the mouse went
    /// down is running now — see `reveal_current_frontmost`'s doc comment.
    /// Takes already-resolved pid sets rather than querying `NSWorkspace`
    /// itself so the decision is unit-testable without a live AX/
    /// `NSWorkspace` environment.
    fn click_started_a_launch(
        app_pids_at_click: &HashSet<i32>,
        app_pids_now: &HashSet<i32>,
    ) -> bool {
        app_pids_now
            .iter()
            .any(|pid| !app_pids_at_click.contains(pid))
    }

    /// One-time startup correction: `apply_config` always seeds
    /// `focused_monitor`'s `active_workspace` entry with `default_workspace`
    /// before a single real window has been scanned (see that function's
    /// doc comment). Left alone, that seed stays showing even after the
    /// initial burst of already-open windows lands — worse, whichever of
    /// those windows a `workspace-rules` match happens to place *last*
    /// (`apply_windows_changed`'s per-pid iteration order over a `HashSet`
    /// is nondeterministic) can silently auto-switch away from it too, via
    /// `place_new_window`'s inactive-workspace branch — neither outcome has
    /// anything to do with what the user was actually looking at. Called
    /// from `main.rs`'s `drain_pending_windows`, on every call (cheap: see
    /// `startup_frontmost_revealed`), so the correction runs as soon as
    /// possible after the startup burst — seeded into the event channel
    /// synchronously by `tili_ax::spawn_event_watcher` before the daemon's
    /// `select!` loop even starts — has actually drained into
    /// `WmState::apply_windows_changed`/`self.placements`.
    ///
    /// Deliberately does not reuse `reveal_frontmost`/`reveal_current_frontmost`
    /// outright: those carry runtime-only semantics (`wake_lock_active`,
    /// `last_frontmost_pid`/`pid_unchanged`, mouse-follows-focus raising,
    /// monitor-swap-to-reveal) that don't apply to a cold boot and would
    /// make this harder to reason about and to unit-test. This only ever
    /// overrides `focused_monitor`'s entry via `switch_workspace` — the only
    /// monitor `apply_config`'s seed (and thus this correction) ever
    /// touches; a secondary monitor gets no `active_workspace` entry at all
    /// until a later `on_displays_changed` (a separate, pre-existing gap,
    /// not this function's concern).
    pub fn reveal_startup_frontmost(&mut self) {
        if self.startup_frontmost_revealed {
            return;
        }
        self.startup_frontmost_revealed = true;

        let frontmost = tili_ax::workspace::frontmost_app_pid().and_then(|pid| {
            if is_system_ui_bundle(tili_ax::bundle_id_for_pid(pid).as_deref()) {
                return None;
            }
            AxWindow::focused_id_for_pid(pid)
        });
        self.reveal_startup_frontmost_window(frontmost);
    }

    /// The testable half of `reveal_startup_frontmost` — kept keyed by
    /// `WindowId` alone, no `AxWindow`/pid, mirroring `place_new_window`'s
    /// seam. `frontmost` is `None` when there's nothing unambiguous to go
    /// on (no real frontmost window at all — e.g. Finder showing the
    /// desktop with nothing focused — or system-UI chrome), which is
    /// deliberately indistinguishable here from "that window isn't placed
    /// yet" (the initial scan hasn't reached it, or it has no `Placement`
    /// for some other reason): either way, the only safe move is to leave
    /// whatever `default_workspace` seed is already showing alone.
    /// `switch_workspace` is already a no-op when its target is already
    /// active, so this doesn't need its own "already showing" check.
    fn reveal_startup_frontmost_window(&mut self, frontmost: Option<WindowId>) {
        let Some(id) = frontmost else {
            return;
        };
        let Some(placement) = self.placements.get(&id) else {
            return;
        };
        let workspace = placement.workspace.clone();
        let _ = self.switch_workspace(&workspace);
    }

    /// Raises/focuses `id` directly (not through `focused_node()` — used by
    /// `summon`, which may target a `Floating` window that has no tracked
    /// tree focus at all).
    ///
    /// Updates `last_frontmost_pid` synchronously, right here at the moment
    /// tili itself changes real macOS frontmost state — not left for
    /// `reveal_frontmost` to eventually catch up to reactively. Without
    /// this, a self-inflicted focus change (e.g. `switch_workspace` raising
    /// a different app when entering a non-empty workspace) is a genuine
    /// AX-level activation, so it shows up to `watch.rs`'s
    /// `NSWorkspaceDidActivateApplicationNotification` handling as
    /// indistinguishable from a real, external Cmd-Tab; if the user has
    /// already navigated elsewhere by the time that notification's deferred
    /// `reveal_frontmost` call finally runs, `pid_unchanged` would wrongly
    /// read `false` (since `last_frontmost_pid` was still whatever it was
    /// *before* this raise) and chase back to the workspace this very call
    /// just left — confirmed on real hardware via diagnostic logging:
    /// rapidly hopping through empty workspaces after switching out of one
    /// with a real app produces exactly this late, self-inflicted "edge."
    fn raise_focused_window(&mut self, id: WindowId) {
        if let Some(window) = self.windows.get(&id) {
            window.focus();
            self.last_frontmost_pid = Some(window.pid());
            if self.mouse_follows_focus {
                let frame = window.frame();
                tili_ax::warp_cursor_to(frame.x + frame.width / 2.0, frame.y + frame.height / 2.0);
            }
        }
    }

    /// Moves a workspace to `target` without changing what the user is
    /// looking at.
    ///
    /// * Active workspace: swap it with whatever `target` currently shows,
    ///   so both displays keep exactly one active workspace and no window is
    ///   lost. Repeating the command with the original target reverses it.
    /// * Parked workspace: only move its **home** to `target`; the target's
    ///   current screen is untouched. `workspace X` then opens it there.
    ///
    /// Refuses pinned workspaces moving off their pin, synthetic display
    /// fallbacks, and swaps that would push a pinned/synthetic workspace off
    /// its own display. `Next`/`Prev` are relative to the workspace's current
    /// (or last) display, not necessarily the focused one.
    pub fn move_workspace_to_monitor(
        &mut self,
        workspace: Option<&str>,
        target: tili_ipc::MonitorTarget,
    ) -> Result<(), String> {
        let active_name = self.active_workspace_name();
        let workspace = workspace.unwrap_or(&active_name);
        if !self.workspaces.contains_key(workspace) {
            return Err(format!("workspace '{workspace}' isn't declared in config"));
        }
        if self.workspace_is_synthetic(workspace) {
            return Err(format!(
                "workspace '{workspace}' is a display fallback and can't be moved"
            ));
        }

        let source = self.active_display_for_workspace(workspace);
        let base = source
            .or_else(|| self.workspace_home(workspace))
            .unwrap_or(self.focused_monitor);
        let target_monitor = self.resolve_monitor_target_from(target, base)?;
        if source == Some(target_monitor) {
            return Ok(());
        }
        if self.workspace_pinned_elsewhere(workspace, target_monitor) {
            return Err(format!(
                "workspace '{workspace}' is pinned to another monitor"
            ));
        }

        if let Some(source_monitor) = source {
            match self.active_workspace.get(&target_monitor).cloned() {
                Some(displaced_name) if displaced_name != workspace => {
                    if self.workspace_is_synthetic(&displaced_name) {
                        if self.workspace_has_windows(&displaced_name) {
                            return Err(format!(
                                "the target display is using fallback workspace \
                                 '{displaced_name}' with windows; move those windows first"
                            ));
                        }
                        self.remove_synthetic_workspace(&displaced_name);
                    }
                    if self.workspace_pinned_elsewhere(&displaced_name, source_monitor) {
                        return Err(format!(
                            "workspace '{displaced_name}' is pinned to another monitor"
                        ));
                    }

                    self.previous_workspaces
                        .insert(source_monitor, displaced_name.clone());
                    self.previous_workspaces
                        .insert(target_monitor, workspace.to_string());
                    self.set_active_workspace(source_monitor, displaced_name);
                    self.set_active_workspace(target_monitor, workspace.to_string());
                    self.switch_epoch += 1;

                    self.relayout_monitor(source_monitor);
                    self.reposition_floating_for_monitor(source_monitor, true);
                    self.relayout_monitor(target_monitor);
                    self.reposition_floating_for_monitor(target_monitor, true);
                    let focused = self.focused_monitor;
                    self.focus_monitor_by_id(focused);
                }
                Some(_) => return Ok(()),
                None => {
                    self.active_workspace.remove(&source_monitor);
                    self.set_active_workspace(target_monitor, workspace.to_string());
                    self.ensure_monitor_workspaces();
                    self.switch_epoch += 1;
                    self.relayout_monitor(target_monitor);
                    self.reposition_floating_for_monitor(target_monitor, true);
                    let focused = self.focused_monitor;
                    self.focus_monitor_by_id(focused);
                }
            }
        } else {
            // Parked: relocate the home only. This is the "safe move" — no
            // visible workspace is disturbed, and `workspace X` opens it on
            // the new display later.
            self.workspace_monitor
                .insert(workspace.to_string(), target_monitor);
        }
        self.renumber_synthetic_workspaces();
        Ok(())
    }

    fn resolve_monitor_target(&self, target: tili_ipc::MonitorTarget) -> Result<u32, String> {
        self.resolve_monitor_target_from(target, self.focused_monitor)
    }

    /// Resolves a monitor target relative to `base` — used by workspace
    /// moves so `next`/`prev` step from the workspace's own display rather
    /// than from whichever display happens to be focused.
    fn resolve_monitor_target_from(
        &self,
        target: tili_ipc::MonitorTarget,
        base: u32,
    ) -> Result<u32, String> {
        match target {
            tili_ipc::MonitorTarget::Id(id) => {
                if self.monitors.iter().any(|m| m.id == id) {
                    Ok(id)
                } else {
                    Err(format!("no connected monitor with id {id}"))
                }
            }
            tili_ipc::MonitorTarget::Main => self
                .monitors
                .iter()
                .find(|m| m.is_main)
                .map(|m| m.id)
                .ok_or_else(|| "no main monitor connected".to_string()),
            tili_ipc::MonitorTarget::Next => self
                .adjacent_monitor_id(base, 1)
                .ok_or_else(|| "fewer than two monitors connected".to_string()),
            tili_ipc::MonitorTarget::Prev => self
                .adjacent_monitor_id(base, -1)
                .ok_or_else(|| "fewer than two monitors connected".to_string()),
        }
    }

    /// Toggles the focused window between `Tiled` and `Floating` at
    /// runtime, independent of any `floating-rules` match at creation time.
    /// Always targets whatever `focused_node()` points at — since floating
    /// windows are `Node::Floating` leaves in the same tree as tiled ones
    /// (see `tili_tree::Node`), that's now exactly the real focused window
    /// either way, no separate "arbitrary floating window" fallback needed.
    pub fn set_floating(&mut self, floating: bool) -> Result<(), String> {
        let workspace = self.active_workspace_name();
        let current = self.focused_node().ok_or("no window is focused")?;
        let id = self
            .active_tree()
            .window_at(current)
            .ok_or("focused window not found")?;
        let already_floating = matches!(
            self.placements.get(&id).map(|p| &p.kind),
            Some(PlacementKind::Floating { .. })
        );
        if floating == already_floating {
            return Err(if floating {
                "focused window is already floating".to_string()
            } else {
                "focused window is already tiled".to_string()
            });
        }

        self.remove_from_tree(id, &workspace);
        if floating {
            self.placements.insert(
                id,
                Placement {
                    workspace: workspace.clone(),
                    kind: PlacementKind::Floating { manual: None },
                },
            );
            let near = self.workspace_focus.get(&workspace).copied();
            let root_orientation = self.root_orientation_hint();
            let node = self
                .workspaces
                .entry(workspace.clone())
                .or_default()
                .insert_floating(id, near, root_orientation);
            self.set_focused_node(node);
            let area = self.focused_monitor_area();
            self.place_floating_window(id, area);
        } else {
            let near = self.workspace_focus.get(&workspace).copied();
            let root_orientation = self.root_orientation_hint();
            let node = self
                .workspaces
                .entry(workspace.clone())
                .or_default()
                .insert_window(id, near, root_orientation);
            self.placements.insert(
                id,
                Placement {
                    workspace,
                    kind: PlacementKind::Tiled,
                },
            );
            self.set_focused_node(node);
        }

        self.relayout_active();
        Ok(())
    }

    /// Hotkey-friendly counterpart to `set_floating`: flips whichever
    /// placement the focused window currently has. Returns an error only
    /// when nothing is focused (or the window has no placement).
    pub fn toggle_floating(&mut self) -> Result<(), String> {
        let current = self.focused_node().ok_or("no window is focused")?;
        let id = self
            .active_tree()
            .window_at(current)
            .ok_or("focused window not found")?;
        let floating = matches!(
            self.placements.get(&id).map(|p| &p.kind),
            Some(PlacementKind::Floating { .. })
        );
        self.set_floating(!floating)
    }

    /// Switches to `name`, navigating to its **home display**: the display
    /// already showing it, a connected `monitor="..."` pin, or the display
    /// it last lived on. A workspace that has never been shown is assigned
    /// to the focused display (so first use still feels local).
    ///
    /// A parked workspace is shown on its home display — never on an
    /// unrelated focused display. The workspace occupying that display is
    /// parked, so both workspaces keep their homes and no layout is swapped
    /// behind the user's back. Use `workspace-here` for the explicit
    /// "change this screen" action.
    pub fn switch_workspace(&mut self, name: &str) -> Result<(), String> {
        if !self.workspaces.contains_key(name) {
            return Err(format!("workspace '{name}' isn't declared in config"));
        }
        let focused_current = self.active_workspace.get(&self.focused_monitor).cloned();
        if focused_current.as_deref() == Some(name) {
            return Ok(());
        }

        // Already visible somewhere: navigating there is the whole action.
        if let Some(display) = self.active_display_for_workspace(name) {
            self.record_previous_focus();
            self.switch_epoch += 1;
            self.focus_monitor_by_id(display);
            return Ok(());
        }

        if self.workspace_pinned_to_disconnected_display(name) {
            return Err(format!(
                "workspace '{name}' is pinned to a monitor that isn't connected"
            ));
        }

        self.record_previous_focus();
        let target = self
            .workspace_home(name)
            .filter(|id| self.monitors.iter().any(|monitor| monitor.id == *id))
            .unwrap_or(self.focused_monitor);
        self.switch_on_monitor(name, target, true)
    }

    /// Shows `name` on the currently focused display, pulling it from another
    /// display (swap) or from parking (local switch). This is the explicit
    /// "change this screen" counterpart to `switch_workspace`'s "go to X";
    /// it is also what makes a pinned workspace's home meaningful.
    pub fn switch_workspace_here(&mut self, name: &str) -> Result<(), String> {
        if !self.workspaces.contains_key(name) {
            return Err(format!("workspace '{name}' isn't declared in config"));
        }
        let target = self.focused_monitor;
        if self.active_workspace.get(&target).map(String::as_str) == Some(name) {
            return Ok(());
        }
        if self.workspace_is_synthetic(name) {
            return Err(format!(
                "workspace '{name}' is a display fallback and can't be pulled to another monitor"
            ));
        }
        if self.workspace_pinned_elsewhere(name, target) {
            return Err(format!("workspace '{name}' is pinned to another monitor"));
        }

        self.record_previous_focus();

        if let Some(other) = self.active_display_for_workspace(name) {
            self.swap_workspace_displays(name, other, target)?;
            return Ok(());
        }
        self.switch_on_monitor(name, target, true)
    }

    /// Shows `name` on `monitor_id`, parking whatever was active there.
    /// Shared by `workspace`'s home branch, `workspace-here`'s parked branch,
    /// and `workspace-back`. Back-history is the caller's job, not this
    /// function's.
    fn switch_on_monitor(
        &mut self,
        name: &str,
        monitor_id: u32,
        focus: bool,
    ) -> Result<(), String> {
        let switch_start = Instant::now();
        if !self.workspaces.contains_key(name) {
            return Err(format!("workspace '{name}' isn't declared in config"));
        }
        let current = self.active_workspace.get(&monitor_id).cloned();
        if current.as_deref() == Some(name) {
            if focus {
                self.focus_monitor_by_id(monitor_id);
            }
            return Ok(());
        }

        self.switch_epoch += 1;
        if let Some(outgoing) = current.clone() {
            self.previous_workspaces.insert(monitor_id, outgoing);
        }

        // Captured before any parking/relayout so the incoming workspace's
        // windows can be brought on screen first. The steps stay ordered for
        // the same reason the original single-monitor switch did: parking
        // the outgoing tiled windows first would leave a visible blank gap.
        let outgoing_tiled: Vec<WindowId> = current.as_ref().map_or(Vec::new(), |outgoing_name| {
            self.workspaces
                .get(outgoing_name)
                .map(Tree::tiled_window_ids)
                .unwrap_or_default()
        });
        let outgoing_floating: Vec<WindowId> =
            current.as_ref().map_or(Vec::new(), |outgoing_name| {
                self.floating_windows_in(outgoing_name)
            });

        self.set_active_workspace(monitor_id, name.to_string());

        self.frame_setter.set_suppressed(true);
        let phase_start = Instant::now();
        self.relayout_monitor_concurrently(monitor_id);
        trace::log(format_args!(
            "tili-daemon: switch_workspace {name} phase=incoming_relayout took {:?}",
            phase_start.elapsed()
        ));
        let phase_start = Instant::now();
        self.reposition_floating_for_monitor_concurrently(monitor_id);
        trace::log(format_args!(
            "tili-daemon: switch_workspace {name} phase=incoming_floating_reposition took {:?}",
            phase_start.elapsed()
        ));

        let restore = self
            .workspace_focus
            .get(name)
            .copied()
            .or_else(|| self.workspaces.get(name).and_then(Tree::default_focus));
        if focus {
            self.focused_monitor = monitor_id;
        }
        if focus && let Some(node) = restore {
            self.set_focused_node(node);
            let phase_start = Instant::now();
            self.raise_focused();
            trace::log(format_args!(
                "tili-daemon: switch_workspace {name} phase=raise_focused took {:?}",
                phase_start.elapsed()
            ));
        }

        let phase_start = Instant::now();
        self.park_concurrently(&outgoing_floating);
        trace::log(format_args!(
            "tili-daemon: switch_workspace {name} phase=outgoing_floating_park took {:?}",
            phase_start.elapsed()
        ));

        let phase_start = Instant::now();
        self.park_concurrently(&outgoing_tiled);
        trace::log(format_args!(
            "tili-daemon: switch_workspace {name} phase=outgoing_tiled_park took {:?}",
            phase_start.elapsed()
        ));

        self.frame_setter.set_suppressed(false);

        // An empty synthetic fallback workspace should not linger once the
        // user has switched away from it; a non-empty one is kept until its
        // display disconnects (windows still need a home).
        if let Some(outgoing) = current {
            self.remove_synthetic_workspace(&outgoing);
        }

        trace::log(format_args!(
            "tili-daemon: switch_workspace {name} total took {:?}",
            switch_start.elapsed()
        ));
        Ok(())
    }

    /// Swaps `workspace` (currently on `from`) with whatever is active on
    /// `to`. Used only by `workspace-here`: both displays keep exactly one
    /// active workspace, and every window follows its workspace.
    fn swap_workspace_displays(
        &mut self,
        workspace: &str,
        from: u32,
        to: u32,
    ) -> Result<(), String> {
        if from == to {
            return Ok(());
        }
        let displaced = self.active_workspace.get(&to).cloned();
        let Some(displaced_name) = displaced else {
            // Target had no workspace at all (should be rare after
            // `ensure_monitor_workspaces`); move it and re-fill the source.
            self.active_workspace.remove(&from);
            self.set_active_workspace(to, workspace.to_string());
            self.ensure_monitor_workspaces();
            self.relayout_monitor(to);
            self.reposition_floating_for_monitor(to, true);
            self.focus_monitor_by_id(to);
            self.switch_epoch += 1;
            return Ok(());
        };
        if displaced_name == workspace {
            self.focus_monitor_by_id(to);
            return Ok(());
        }
        if self.workspace_is_synthetic(&displaced_name) {
            if self.workspace_has_windows(&displaced_name) {
                return Err(format!(
                    "the focused display is using fallback workspace '{displaced_name}' \
                     with windows; move those windows before pulling another workspace here"
                ));
            }
            self.remove_synthetic_workspace(&displaced_name);
        }
        if self.workspace_pinned_elsewhere(&displaced_name, from) {
            return Err(format!(
                "workspace '{displaced_name}' is pinned to another monitor"
            ));
        }

        self.previous_workspaces.insert(to, displaced_name.clone());
        self.previous_workspaces.insert(from, workspace.to_string());
        self.set_active_workspace(from, displaced_name);
        self.set_active_workspace(to, workspace.to_string());
        self.switch_epoch += 1;

        self.relayout_monitor(from);
        self.reposition_floating_for_monitor(from, true);
        self.relayout_monitor(to);
        self.reposition_floating_for_monitor(to, true);
        self.focus_monitor_by_id(to);
        Ok(())
    }

    /// Returns to the `(display, workspace)` pair the user was looking at
    /// before the last navigation command. Restores the pair, not just the
    /// workspace: if it is still on the remembered display it is focused; if
    /// parked it is shown there; if it was swapped to another display from
    /// the current one (the `workspace-here` case) the swap is undone.
    /// Returning from a cross-display jump only focuses the remembered
    /// display, never rearranging another screen. Repeated calls toggle,
    /// since each one records the pair it left.
    pub fn switch_to_previous_workspace(&mut self) -> Result<(), String> {
        let previous = self
            .previous_focus
            .clone()
            .or_else(|| {
                self.previous_workspaces
                    .get(&self.focused_monitor)
                    .cloned()
                    .map(|workspace| (self.focused_monitor, workspace))
            })
            .ok_or("no previous workspace to switch back to")?;
        let (monitor, workspace) = previous;
        if !self
            .monitors
            .iter()
            .any(|candidate| candidate.id == monitor)
        {
            self.previous_focus = None;
            return Err("the previous monitor is no longer connected".to_string());
        }

        let current_pair = (self.focused_monitor, self.active_workspace_name());
        self.previous_focus = Some(current_pair.clone());

        if self.active_workspace.get(&monitor).map(String::as_str) == Some(workspace.as_str()) {
            self.focus_monitor_by_id(monitor);
            return Ok(());
        }

        if self.active_display_for_workspace(&workspace).is_none()
            && self.workspaces.contains_key(&workspace)
            && !self.workspace_pinned_elsewhere(&workspace, monitor)
        {
            self.switch_on_monitor(&workspace, monitor, true)?;
            return Ok(());
        }

        // Backing out of `workspace-here` on the same display: the previous
        // workspace was swapped to another display. Undo that swap locally
        // instead of leaving the user on the pulled workspace with a no-op
        // back. Only allowed for the display that was actually remembered;
        // returning from a cross-display jump must not rearrange anything.
        if monitor == self.focused_monitor
            && let Some(other) = self.active_display_for_workspace(&workspace)
            && self.workspaces.contains_key(&workspace)
            && !self.workspace_is_synthetic(&workspace)
            && !self.workspace_pinned_elsewhere(&workspace, monitor)
        {
            self.swap_workspace_displays(&workspace, other, monitor)?;
            return Ok(());
        }

        // The workspace moved elsewhere (or its display's layout changed
        // under us); returning to the remembered screen is enough. Never
        // pull another display's layout back implicitly.
        self.focus_monitor_by_id(monitor);
        Ok(())
    }

    /// Current value of `switch_epoch` — see its doc comment on `WmState`.
    /// `main.rs` is a separate module and the field itself is private, so
    /// this is the only way it can read it.
    pub fn switch_epoch(&self) -> u64 {
        self.switch_epoch
    }

    /// Moves the focused window — tiled or floating, whichever it actually
    /// is — into a different workspace's tree, then switches the focused
    /// monitor to that workspace (via `switch_workspace`) so the window is
    /// never left off-screen after being moved.
    pub fn move_focused_to_workspace(&mut self, target_name: &str) -> Result<(), String> {
        if !self.workspaces.contains_key(target_name) {
            return Err(format!(
                "workspace '{target_name}' isn't declared in config"
            ));
        }

        let active_workspace = self.active_workspace_name();
        if target_name == active_workspace {
            return Ok(());
        }
        let current_node = self.focused_node().ok_or("no window is focused")?;
        let id = self
            .active_tree()
            .window_at(current_node)
            .ok_or("focused window not found")?;
        // Preserves whatever `id` already was — moving a floating window to
        // another workspace must keep it floating there, not force it back
        // to tiled just because `Tree::remove_window`/`insert_*` need to
        // know which leaf kind to recreate.
        let was_floating = matches!(
            self.placements.get(&id).map(|p| &p.kind),
            Some(PlacementKind::Floating { .. })
        );

        // Goes through `remove_from_tree` rather than `Tree::remove_window`
        // directly so a `fullscreen_focus` entry pointing at `id` gets
        // cleared too — otherwise the source workspace is left with a
        // dangling fullscreen node id that `relayout_monitor`/
        // `relayout_monitor_concurrently` never fall back away from,
        // permanently freezing that workspace's tiled layout. The return
        // value is ignored, same reasoning as `demote_to_special`: `id` is
        // about to be reinserted (into a different workspace) and focused
        // again immediately below, so a mid-flight raise here would just be
        // a spurious flash.
        self.remove_from_tree(id, &active_workspace);

        let target_focus_hint = self.workspace_focus.get(target_name).copied();
        let root_orientation = self.root_orientation_hint();
        let target_tree = self
            .workspaces
            .get_mut(target_name)
            .expect("validated above");
        let new_node = if was_floating {
            target_tree.insert_floating(id, target_focus_hint, root_orientation)
        } else {
            target_tree.insert_window(id, target_focus_hint, root_orientation)
        };
        self.workspace_focus
            .insert(target_name.to_string(), new_node);
        self.placements.insert(
            id,
            Placement {
                workspace: target_name.to_string(),
                kind: if was_floating {
                    PlacementKind::Floating { manual: None }
                } else {
                    PlacementKind::Tiled
                },
            },
        );

        if let Some(monitor) = self.monitor_showing_workspace(target_name) {
            self.relayout_monitor(monitor);
            self.reposition_floating_for_monitor(monitor, true);
        }
        self.switch_workspace(target_name)?;
        Ok(())
    }

    fn active_workspace_name(&self) -> String {
        if let Some(name) = self.active_workspace.get(&self.focused_monitor)
            && self.workspaces.contains_key(name)
        {
            return name.clone();
        }
        // Never fall back to a workspace name config may have removed
        // (`"main"`): prefer any real tree before the internal default.
        self.workspaces
            .keys()
            .next()
            .cloned()
            .unwrap_or_else(|| DEFAULT_WORKSPACE.to_string())
    }

    fn active_tree(&self) -> &Tree {
        // `active_workspace_name` always returns an existing tree when one
        // exists, and `Default`/`apply_config` keep at least one workspace.
        self.workspaces
            .get(&self.active_workspace_name())
            .expect("active workspace always has a tree entry")
    }

    fn active_tree_mut(&mut self) -> &mut Tree {
        let name = self.active_workspace_name();
        self.workspaces.entry(name).or_default()
    }

    fn floating_windows_in(&self, workspace: &str) -> Vec<WindowId> {
        self.placements
            .iter()
            .filter(|(_, p)| {
                matches!(p.kind, PlacementKind::Floating { .. }) && p.workspace == workspace
            })
            .map(|(&id, _)| id)
            .collect()
    }

    /// Parks every tiled and floating window of `name`'s workspace — shared
    /// by `on_displays_changed` (a monitor disconnected) and
    /// `move_workspace_to_monitor` (a workspace displaced off every
    /// monitor), both of which need the same "this workspace is no longer
    /// shown anywhere" cleanup.
    fn park_workspace(&mut self, name: &str) {
        let outgoing: Vec<WindowId> = self
            .workspaces
            .get(name)
            .map(Tree::tiled_window_ids)
            .unwrap_or_default()
            .into_iter()
            .chain(self.floating_windows_in(name))
            .collect();
        for id in outgoing {
            self.park(id);
        }
    }

    /// Drops a window's placement entirely — from its workspace's `Tree`
    /// if it was `Tiled` or `Floating` (see `remove_from_tree`), or just
    /// from `placements` otherwise (every other kind never sat in a
    /// `Tree`). Also clears every other per-`WindowId` auxiliary set this
    /// window may be in (`floating_placed`/`floating_centered`/
    /// `force_ignored_window_ids`) — the single spot both of this
    /// function's callers (`finalize_expired_removals`, `remove_app`) go
    /// through, so neither needs its own copy of this cleanup.
    ///
    /// `pid` is the removed window's owning process, which both callers
    /// already have to drop it from `self.windows` — passed in rather than
    /// looked up here, since by this point that lookup would fail. See
    /// `same_app_window_in` for what it decides.
    fn remove_placement(&mut self, id: WindowId, pid: Option<i32>) {
        self.floating_placed.remove(&id);
        self.floating_centered.remove(&id);
        self.force_ignored_window_ids.remove(&id);
        self.special_anchors.remove(&id);
        self.special_float_manual.remove(&id);
        self.animate_entry.remove(&id);
        self.tab_group_members.remove(&id);
        let Some(placement) = self.placements.remove(&id) else {
            return;
        };
        if matches!(
            placement.kind,
            PlacementKind::Tiled | PlacementKind::Floating { .. }
        ) {
            // `id` is genuinely gone (unlike `demote_to_special`/`set_floating`'s
            // calls below, where the same window is about to be reinserted) —
            // if it was its workspace's real focus and that workspace is
            // still on screen, real macOS focus needs to be reasserted onto
            // whatever `remove_from_tree` reassigned. Left alone, real focus
            // stays wherever macOS's own app-reactivation history happens to
            // land when the quit app's process disappears — an app on a
            // completely different, possibly-parked workspace, oblivious to
            // this one still having another window.
            //
            // Except while that workspace holds a natively-fullscreened
            // window: macOS is then showing that window's own Space, and
            // `raise_focused_window` activates its target's app, which
            // yanks the user straight back out of fullscreen. The tree
            // bookkeeping still happens; only the real-focus assertion is
            // skipped, since there's no sibling on screen to assert it onto
            // in the first place.
            //
            // And except when the removed window's own app still has a
            // window here — see `same_app_window_in`.
            let node = self.remove_from_tree(id, &placement.workspace);
            let same_app = pid.and_then(|pid| self.same_app_window_in(&placement.workspace, pid));
            if let Some(same_app) = same_app {
                self.sync_focus_to_window(same_app);
            } else if !self.has_native_fullscreen_window(&placement.workspace)
                && let Some(node) = node
                && let Some(raise_id) = self
                    .workspaces
                    .get(&placement.workspace)
                    .and_then(|tree| tree.window_at(node))
            {
                self.raise_focused_window(raise_id);
            }
        }
    }

    /// Another window of `pid`'s still living in `workspace`, if any — the
    /// signal that `remove_placement` must neither raise anything nor trust
    /// its own structural guess about where focus went.
    ///
    /// That guess (`Tree::remove_window`'s nearest-MRU-leaf) and the raise
    /// built on it exist for one case: the removed window's app is gone
    /// from this workspace entirely, so macOS is free to reactivate
    /// whatever its own history points at — commonly an app on a
    /// different, possibly-parked workspace. When the app still owns a
    /// window here, none of that applies: macOS keeps it frontmost and
    /// focuses that remaining window itself, so raising a *different*
    /// window fights an OS decision that was already correct.
    ///
    /// The confirmed case is the throwaway window a browser opens for a
    /// native-fullscreen video. On exit it's promoted back into the tree as
    /// an ordinary `Floating` leaf (so the `NativeFullscreen` guard below
    /// no longer applies) and destroyed moments later. `remove_child`'s MRU
    /// fixup is purely positional — it has no memory of which sibling was
    /// focused before the removed one — so it landed on whichever leaf
    /// happened to be adjacent, a floating Finder window in the report,
    /// which then got raised over the browser macOS had just refocused and
    /// left stacked on top of it. Asking macOS directly at that moment
    /// doesn't work either: the window is mid-destruction and system-wide
    /// focus hasn't resolved to its successor yet (confirmed on real
    /// hardware — an `AxWindow::system_focused_id` check placed here
    /// declined every time). This is decided from state tili already
    /// holds, so no timing is involved.
    ///
    /// Restricted to `Tiled`/`Floating` placements: a minimized or hidden
    /// window is not something macOS can hand focus to. Which one is
    /// returned when the app has several here is deliberately unimportant —
    /// macOS picks its own, and `dispatch()`'s `sync_focus_from_frontmost`
    /// corrects the bookkeeping before the next command that acts on focus
    /// (read-only queries skip that sync, and none of them reads it).
    fn same_app_window_in(&self, workspace: &str, pid: i32) -> Option<WindowId> {
        self.windows
            .iter()
            .filter(|(wid, w)| {
                w.pid() == pid
                    && !self.pending_removal.contains_key(wid)
                    && self.placements.get(wid).is_some_and(|p| {
                        p.workspace == workspace
                            && matches!(
                                p.kind,
                                PlacementKind::Tiled | PlacementKind::Floating { .. }
                            )
                    })
            })
            .map(|(&wid, _)| wid)
            .min()
    }

    /// Whether `workspace` currently holds a window macOS has put into its
    /// own native-fullscreen Space. While that's true the workspace isn't
    /// really "on screen" the way `active_workspace` implies — the Space
    /// showing belongs to that one window — so anything that would assert
    /// real macOS focus onto a *different* window there has to hold off:
    /// `AxWindow::focus()` activates the target's app, which drops the user
    /// out of fullscreen with no action of their own.
    fn has_native_fullscreen_window(&self, workspace: &str) -> bool {
        self.placements.values().any(|p| {
            p.workspace == workspace && matches!(p.kind, PlacementKind::NativeFullscreen(_))
        })
    }

    /// Removes `id` from `workspace`'s tiled tree, reassigning that
    /// workspace's remembered focus if `id` was it (and clearing its
    /// fullscreen focus, if any — a removed node can't stay "the
    /// fullscreened one") — without touching `self.placements`, so callers
    /// that are about to overwrite the placement with a new kind
    /// (`demote_to_special`) or drop it entirely (`remove_placement`) both
    /// funnel through here. Returns the reassigned focus node when the
    /// removed leaf actually was `workspace`'s recorded focus *and*
    /// `workspace` is currently visible on some monitor — `None` otherwise
    /// (including when the tree is now empty) — for `remove_placement` to
    /// re-raise for real; `demote_to_special`/`set_floating` deliberately
    /// ignore this, since both immediately reinsert the same window and
    /// re-focus it themselves right after, making a mid-flight raise here
    /// just a spurious flash.
    fn remove_from_tree(&mut self, id: WindowId, workspace: &str) -> Option<NodeId> {
        let tree = self.workspaces.get_mut(workspace)?;
        let removed_leaf = tree.find_node(id);
        let suggested = tree.remove_window(id);
        let mut reassigned_visible_focus = None;
        if removed_leaf.is_some() && self.workspace_focus.get(workspace) == removed_leaf.as_ref() {
            match suggested {
                Some(n) => {
                    self.workspace_focus.insert(workspace.to_string(), n);
                    if self.active_workspace.values().any(|w| w == workspace) {
                        reassigned_visible_focus = Some(n);
                    }
                }
                None => {
                    self.workspace_focus.remove(workspace);
                }
            }
        }
        if removed_leaf.is_some() && self.fullscreen_focus.get(workspace) == removed_leaf.as_ref() {
            self.fullscreen_focus.remove(workspace);
        }
        reassigned_visible_focus
    }

    /// Whether `node` (a `focused_node()` result) is a `Floating` leaf —
    /// every tiled-topology command (move/join/resize/orientation/layout/
    /// balance, and non-native fullscreen) is meaningless for one, since
    /// `layout` always excludes `Floating` nodes from sizing (see
    /// `tili_tree::Node::Floating`'s doc comment). `focus`,
    /// `move_focused_to_workspace`, `set_floating`, `close_focused`, and
    /// native fullscreen all work correctly for a floating focus and don't
    /// call this.
    fn focused_window_is_floating(&self, node: NodeId) -> bool {
        self.active_tree()
            .window_at(node)
            .and_then(|id| self.placements.get(&id))
            .is_some_and(|p| matches!(p.kind, PlacementKind::Floating { .. }))
    }

    fn focused_node(&self) -> Option<NodeId> {
        self.workspace_focus
            .get(&self.active_workspace_name())
            .copied()
    }

    fn set_focused_node(&mut self, node: NodeId) {
        let name = self.active_workspace_name();
        self.workspace_focus.insert(name, node);
        self.active_tree_mut().record_focus(node);
    }

    /// Orientation a workspace root gets when it's created for its second
    /// window — the configured `default_root_orientation` if explicit,
    /// else derived from the focused monitor's aspect ratio (wide -> rows
    /// side by side, tall -> stacked).
    fn root_orientation_hint(&self) -> tili_tree::Orientation {
        if let Some(o) = self.default_root_orientation {
            return o;
        }
        match self.monitor_frame(self.focused_monitor) {
            Some(frame) if frame.height > frame.width => tili_tree::Orientation::Vertical,
            _ => tili_tree::Orientation::Horizontal,
        }
    }

    /// See `raise_focused_window`'s doc comment for why `last_frontmost_pid`
    /// is updated synchronously here rather than left for `reveal_frontmost`
    /// to catch up to reactively.
    fn raise_focused(&mut self) {
        if let Some(node) = self.focused_node()
            && let Some(id) = self.active_tree().window_at(node)
            && let Some(window) = self.windows.get(&id)
        {
            window.focus();
            self.last_frontmost_pid = Some(window.pid());
            if self.mouse_follows_focus {
                let frame = window.frame();
                tili_ax::warp_cursor_to(frame.x + frame.width / 2.0, frame.y + frame.height / 2.0);
            }
        }
    }

    /// The cursor moved to `(x, y)` (M10, `focus-follows-monitor`) — if
    /// that setting is on and the point now falls on a *different*
    /// connected monitor than `focused_monitor`, that monitor becomes
    /// focused, same as an explicit `Command::FocusMonitor`. A no-op
    /// otherwise (including when the setting is off — the daemon still
    /// receives these throttled position updates either way, but only
    /// acts on them when configured to).
    pub fn on_mouse_moved(&mut self, x: f64, y: f64) {
        if !self.focus_follows_monitor {
            return;
        }
        let under_cursor = self.monitors.iter().find(|m| {
            x >= m.frame.x
                && x < m.frame.x + m.frame.width
                && y >= m.frame.y
                && y < m.frame.y + m.frame.height
        });
        if let Some(monitor) = under_cursor
            && monitor.id != self.focused_monitor
        {
            self.focused_monitor = monitor.id;
        }
    }

    /// Marks the left mouse button as held — see `mouse_button_down`'s doc
    /// comment for why this suppresses relayout. Also captures
    /// `resize_drag`: the focused monitor's before-drag tiled layout, so
    /// `on_mouse_button_up` can tell whether this turns out to be a
    /// tiled-window edge/corner drag once it's over.
    pub fn on_mouse_button_down(&mut self) {
        self.mouse_button_down = true;
        self.resize_drag = self.capture_resize_snapshot();
    }

    /// Marks the left mouse button as released. If `resize_drag` shows a
    /// tiled window's real frame actually changed while the button was
    /// down, `apply_mouse_resize` derives a step-snapped tree weight change
    /// from it *before* the relayout below — so that relayout snaps every
    /// window (the dragged one and its siblings) straight to the new,
    /// magnetized layout in one go, instead of first snapping back to the
    /// pre-drag layout the way it would with no `resize_drag` captured.
    pub fn on_mouse_button_up(&mut self) {
        self.mouse_button_down = false;
        if let Some(drag) = self.resize_drag.take() {
            self.apply_mouse_resize(drag);
        }
        self.relayout_active();
    }

    /// The focused monitor's active-workspace tiled layout right now, to
    /// diff against once the drag that's about to start is over — or
    /// `None` if there's nothing valid to resize against: no active
    /// workspace, a `fullscreen_focus` tiled-fullscreen window showing
    /// (only one window is actually on screen, mirroring
    /// `relayout_monitor`'s own special case), or fewer than 2 tiled
    /// windows (nothing to trade weight with, the same "alone" guarantee
    /// `resize_weight`/`resize_handle_at` already enforce structurally).
    fn capture_resize_snapshot(&self) -> Option<ResizeDragSnapshot> {
        let monitor_id = self.focused_monitor;
        let (name, area, gaps) = self.tiled_layout_inputs(monitor_id)?;
        if self.fullscreen_focus.contains_key(&name) {
            return None;
        }
        let tree = self.workspaces.get(&name)?;
        if tree.tiled_window_ids().len() < 2 {
            return None;
        }
        let frames = tree.layout(area, gaps).into_iter().collect();
        Some(ResizeDragSnapshot { monitor_id, frames })
    }

    /// Finds whichever window in `drag.frames` no longer matches its
    /// snapshotted rect — the one the user actually dragged — and, for
    /// each edge that moved, magnet-snaps a tree weight change to the
    /// nearest valid `mouse_resize_step` grid point via
    /// `magnet_resize_edge`. Only one window should ever differ; the loop
    /// stops as soon as it finds and processes that one.
    fn apply_mouse_resize(&mut self, drag: ResizeDragSnapshot) {
        let Some((name, area, gaps)) = self.tiled_layout_inputs(drag.monitor_id) else {
            return;
        };
        let step = self.mouse_resize_step;
        for (&id, &old_rect) in &drag.frames {
            let Some(new_rect) = self.windows.get(&id).map(AxWindow::frame) else {
                continue;
            };
            if frames_match(old_rect, new_rect) {
                continue;
            }
            let Some(tree) = self.workspaces.get_mut(&name) else {
                return;
            };
            for edge in [
                ResizeEdge::Left,
                ResizeEdge::Right,
                ResizeEdge::Top,
                ResizeEdge::Bottom,
            ] {
                magnet_resize_edge(tree, area, gaps, step, old_rect, new_rect, edge);
            }
            break;
        }
    }

    /// Moves a window to hug the southeast-most display's bottom-right
    /// corner (see `tili_ax::southeast_parking_position`) without resizing
    /// it. Every parked window — whatever workspace it belongs to — targets
    /// this exact global coordinate, so parking is consistent and
    /// re-parking is idempotent: `reconcile_existing_placement` and
    /// `repark_parked_windows` write the same point no matter which caller
    /// parked the window first, and `AxWindow::set_position`'s
    /// no-op-if-unchanged guard genuinely no-ops. Spreading parked windows
    /// apart would move the origin off the one `PARK_EPSILON` spot the
    /// hiding trick depends on, exposing a real on-screen strip, so they
    /// all stay at the same point.
    fn park(&mut self, id: WindowId) {
        // Settles any in-flight animation to its true target first — both
        // so the manual-geometry capture below reads the window's real
        // intended frame instead of an arbitrary mid-tween interruption
        // point, and so a stale tween can't resume on a later tick and
        // drag the window away from the parked position this function is
        // about to write.
        if let Some(window) = self.windows.get_mut(&id) {
            self.frame_setter.finish(window);
        }
        self.capture_manual_geometry_before_park(id);
        let Some(window) = self.windows.get_mut(&id) else {
            return;
        };
        let (origin_x, origin_y) = tili_ax::southeast_parking_position(&self.monitors);
        window.set_position(origin_x, origin_y);
    }

    fn monitor_frame(&self, monitor_id: u32) -> Option<Rect> {
        self.monitors
            .iter()
            .find(|m| m.id == monitor_id)
            .map(|m| m.frame)
    }

    /// Resolves the three inputs `Tree::layout` needs for `monitor_id`'s
    /// active workspace — its name, monitor area, and effective gaps —
    /// shared by `relayout_monitor` and `capture_resize_snapshot`/
    /// `apply_mouse_resize`, which all need the exact same resolution.
    /// `None` if the monitor isn't connected or has no active workspace.
    ///
    /// Folds the monitor's notch height (if any) into the effective top
    /// gap here, not in `Tree::layout` itself — `tili-tree` has no macOS
    /// concept of a notch, so this is the one place monitor geometry and
    /// gap config already converge for every tiled-layout caller.
    fn tiled_layout_inputs(&self, monitor_id: u32) -> Option<(String, Rect, Gaps)> {
        let name = self.active_workspace.get(&monitor_id)?.clone();
        let monitor = self.monitors.iter().find(|m| m.id == monitor_id)?;
        let mut gaps = self.workspace_gaps.get(&name).copied().unwrap_or(self.gaps);
        let ignore_notch = self
            .workspace_ignore_notch
            .get(&name)
            .copied()
            .unwrap_or(self.ignore_notch);
        if !ignore_notch {
            gaps.outer.0 += monitor.notch;
            if let Some(outer_solo) = gaps.outer_solo.as_mut() {
                outer_solo.0 += monitor.notch;
            }
        }
        Some((name, monitor.frame, gaps))
    }

    /// Advances any in-flight `TweenedFrameSetter` animations by one step —
    /// called from `main.rs`'s dedicated animation timer, gated by
    /// `is_animating_anything` so that timer never fires (or is even
    /// polled) while nothing is animating. A no-op entirely under
    /// `InstantFrameSetter` (`Settings::animate` off).
    pub fn step_animations(&mut self) {
        self.frame_setter.tick(&mut self.windows);
    }

    /// Whether any window currently has a `TweenedFrameSetter` animation in
    /// flight — see `step_animations`.
    pub fn is_animating_anything(&self) -> bool {
        self.frame_setter.has_active_animations()
    }

    /// The period `main.rs` should drive `animation_tick` at for the
    /// current `Settings::animate` — `None` under `Off` (the tick's own
    /// `select!` guard already keeps it from firing then regardless, but
    /// `main.rs` also uses this to know whether it's worth reconstructing
    /// the interval on a config reload at all).
    pub fn animation_tick_period(&self) -> Option<Duration> {
        match self.animate {
            tili_config::AnimationSpeed::Off => None,
            tili_config::AnimationSpeed::Medium => Some(ANIMATION_TICK_PERIOD_MEDIUM),
            tili_config::AnimationSpeed::High => Some(ANIMATION_TICK_PERIOD_HIGH),
        }
    }

    /// Recomputes every tiled window's frame on `focused_monitor` and
    /// applies it via the `WindowFrameSetter` seam — never a direct AX call
    /// from here. See `relayout_monitor` for the actual per-monitor logic;
    /// most callers only ever need to affect the focused monitor since
    /// that's the only one their own mutation touched.
    fn relayout_active(&mut self) {
        self.relayout_monitor(self.focused_monitor);
    }

    /// Re-lays-out every monitor that's currently showing something —
    /// used after a change (config reload, app termination) that could
    /// plausibly have touched a workspace visible on a *non*-focused
    /// monitor.
    fn relayout_all_visible(&mut self) {
        for id in self.active_workspace.keys().copied().collect::<Vec<_>>() {
            self.relayout_monitor(id);
        }
    }

    /// Recomputes every tiled window's frame in whatever workspace is
    /// active on `monitor_id` and applies it via the `WindowFrameSetter`
    /// seam. A no-op if `monitor_id` isn't connected or has no active
    /// workspace assigned. Windows in other (parked) workspaces are left
    /// exactly where they are. Only touches *tiled* windows — floating
    /// windows keep whatever position they were last centered/parked at
    /// (see `reposition_floating_for_monitor`).
    ///
    /// If the workspace has a `fullscreen_focus` entry (Phase 8b — tiled,
    /// non-native fullscreen), this lays out *only* that node at the full
    /// monitor frame instead of calling `Tree::layout` at all — every other
    /// tiled window is left exactly where it last was (invisible behind the
    /// fullscreened one on the same monitor, but still structurally in the
    /// tree) so toggling fullscreen back off restores the normal layout on
    /// the very next relayout.
    fn relayout_monitor(&mut self, monitor_id: u32) {
        let Some((name, area, gaps)) = self.tiled_layout_inputs(monitor_id) else {
            return;
        };
        let Some(tree) = self.workspaces.get(&name) else {
            return;
        };
        #[cfg(test)]
        self.relayout_calls.set(self.relayout_calls.get() + 1);

        if let Some(&fullscreen_node) = self.fullscreen_focus.get(&name) {
            if let Some(id) = tree.window_at(fullscreen_node)
                && let Some(window) = self.windows.get_mut(&id)
            {
                if is_tab_bar_correction(window.frame(), area) {
                    self.frame_setter.snap(window, area);
                } else {
                    self.frame_setter.set_frame(window, area);
                }
            }
            return;
        }

        let animate_entry = self.animate != tili_config::AnimationSpeed::Off;
        let placements = tree.layout(area, gaps);
        let layout_start = trace::enabled().then(Instant::now);
        for (id, rect) in placements {
            let entry = self.animate_entry.remove(&id);
            if let Some(window) = self.windows.get_mut(&id) {
                if entry && animate_entry {
                    // New window: start it at its final size just outside the
                    // slot on the side the container grows toward, then tween
                    // in. This removes any dependence on whichever frame the
                    // app happened to draw it at (full width for a second
                    // window, an inherited tile for later ones).
                    if let Some(orientation) = tree
                        .find_node(id)
                        .and_then(|node| tree.orientation_of(node))
                    {
                        let start = entry_start_rect(rect, orientation, gaps.inner);
                        if trace::enabled() {
                            trace::log(format_args!(
                                "tili-daemon: new-window entry id={id} start=({:.1},{:.1} {:.1}x{:.1}) target=({:.1},{:.1} {:.1}x{:.1})",
                                start.x,
                                start.y,
                                start.width,
                                start.height,
                                rect.x,
                                rect.y,
                                rect.width,
                                rect.height,
                            ));
                        }
                        self.frame_setter.snap(window, start);
                        self.frame_setter.set_frame(window, rect);
                    } else {
                        // Sole window in its workspace: no sibling to slide
                        // in beside, so land straight on the slot.
                        if trace::enabled() {
                            trace::log(format_args!(
                                "tili-daemon: new-window entry id={id} -> snap target=({:.1},{:.1} {:.1}x{:.1})",
                                rect.x, rect.y, rect.width, rect.height,
                            ));
                        }
                        self.frame_setter.snap(window, rect);
                    }
                } else if is_tab_bar_correction(window.frame(), rect) {
                    self.frame_setter.snap(window, rect);
                } else {
                    self.frame_setter.set_frame(window, rect);
                }
            }
        }
        if let Some(layout_start) = layout_start {
            trace::log(format_args!(
                "tili-daemon: relayout monitor={monitor_id} took {:?}",
                layout_start.elapsed()
            ));
        }
    }

    /// Re-centers/sizes every floating window belonging to whatever
    /// workspace is active on `monitor_id`. A window with captured manual
    /// geometry (the user dragged/resized it at some point) is restored
    /// proportionally via `restore_floating_frame` instead of being
    /// recomputed fresh from its floating rule, so a reactivated workspace
    /// or a monitor swap doesn't silently discard the user's own placement.
    /// A window with no manual geometry but already in `floating_placed` is
    /// left exactly as-is on an ordinary same-monitor switch: it already got
    /// its one rule-based placement when it first floated, and re-deriving
    /// that same frame would undo a resize the user made that hasn't been
    /// AX-observed as "manual" yet (e.g. right before parking). `force`
    /// exists for the one case that genuinely needs a new frame — the
    /// workspace moved to another display, where the old rule-derived
    /// coordinates are now wrong/off-screen.
    fn reposition_floating_for_monitor(&mut self, monitor_id: u32, force: bool) {
        let Some(name) = self.active_workspace.get(&monitor_id).cloned() else {
            return;
        };
        let Some(area) = self.monitor_frame(monitor_id) else {
            return;
        };
        let ids = self.floating_windows_in(&name);
        for id in ids {
            let manual = self.placements.get(&id).and_then(|p| match &p.kind {
                PlacementKind::Floating { manual } => *manual,
                _ => None,
            });
            match manual {
                // A captured drag/resize is restored exactly as the user
                // left it (proportionally) — no centering intent to
                // re-derive or correct here.
                Some(geometry) => {
                    let frame = restore_floating_frame(geometry, area);
                    if let Some(window) = self.windows.get_mut(&id) {
                        self.frame_setter.set_frame(window, frame);
                    }
                }
                None if force || !self.floating_placed.contains(&id) => {
                    self.place_floating_window(id, area);
                }
                None => {}
            }
        }
    }

    /// Fires `f`'s AX write for every `(WindowId, target)` pair
    /// concurrently — one `std::thread::spawn` per window, all joined
    /// before this returns — instead of writing them one at a time.
    /// `switch_workspace`'s deadtime came from N synchronous AX
    /// round-trips run in sequence; a write to one process's window is
    /// completely independent of a write to another's (confirmed safe to
    /// fire concurrently — see AeroSpace's own per-app-thread model), so
    /// joining `max(latency)` instead of summing `N × latency` is free
    /// correctness-wise. Plain OS threads rather than
    /// `tokio::task::spawn_blocking`: every caller here runs synchronously
    /// inside `dispatch()` (invoked directly from the `select!` loop, not
    /// `.await`-ed), so there's no async boundary to join a spawned task
    /// through — `f` runs on each window's own thread and hands the
    /// (possibly frame-updated) `AxWindow` back so it can go back into
    /// `self.windows` once every thread has finished; nothing outlives this
    /// function's own return, so there's no job lifecycle to manage.
    fn write_windows_concurrently<T: Send + 'static>(
        &mut self,
        targets: Vec<(WindowId, T)>,
        f: fn(&mut AxWindow, T),
    ) {
        let join_start = Instant::now();
        let window_count = targets.len();
        let handles: Vec<std::thread::JoinHandle<AxWindow>> = targets
            .into_iter()
            .filter_map(|(id, target)| self.windows.remove(&id).map(|window| (window, target)))
            .map(|(mut window, target)| {
                std::thread::spawn(move || {
                    let id = window.id();
                    let write_start = Instant::now();
                    f(&mut window, target);
                    trace::log(format_args!(
                        "tili-daemon: write_windows_concurrently window={id} took {:?}",
                        write_start.elapsed()
                    ));
                    window
                })
            })
            .collect();
        for handle in handles {
            if let Ok(window) = handle.join() {
                self.windows.insert(window.id(), window);
            }
        }
        trace::log(format_args!(
            "tili-daemon: write_windows_concurrently windows={window_count} join took {:?}",
            join_start.elapsed()
        ));
    }

    /// `switch_workspace`-only variant of `park`: the bookkeeping that
    /// touches shared `WmState` fields (settling a stale tween, capturing
    /// manual floating geometry, resolving the parking origin) runs
    /// sequentially first — none of that is safe to touch from multiple
    /// threads at once — and only the actual `set_position` AX write, one
    /// per window, fires concurrently via `write_windows_concurrently`.
    fn park_concurrently(&mut self, ids: &[WindowId]) {
        let mut targets = Vec::with_capacity(ids.len());
        // The same global southeast corner for every window; only the
        // per-window bookkeeping above is sequential.
        let origin = tili_ax::southeast_parking_position(&self.monitors);
        for &id in ids {
            if let Some(window) = self.windows.get_mut(&id) {
                self.frame_setter.finish(window);
            }
            self.capture_manual_geometry_before_park(id);
            if self.windows.contains_key(&id) {
                targets.push((id, origin));
            }
        }
        self.write_windows_concurrently(targets, |window: &mut AxWindow, (x, y): (f64, f64)| {
            window.set_position(x, y);
        });
    }

    /// Re-parks every currently parked tiled/floating window at the current
    /// global southeast corner. Called after a display change (connect,
    /// disconnect, arrangement/resolution/scale, id rename) and after
    /// session restore: the southeast-most display may now be a different
    /// one, and windows left at the old corner would otherwise sit exactly
    /// where another monitor now is — the original per-display parking
    /// bug, just one event later.
    ///
    /// Idempotent and safe when nothing changed (`set_position` no-ops on an
    /// unchanged frame). Skipped while the user is mid-drag so a display
    /// event can't fight a live resize; the next normal park re-establishes
    /// the invariant.
    fn repark_parked_windows(&mut self) {
        if self.monitors.is_empty() || self.mouse_button_down {
            return;
        }
        let ids = self.parked_positionable_ids();
        if !ids.is_empty() {
            self.park_concurrently(&ids);
        }
    }

    /// `switch_workspace`-only variant of `relayout_monitor`: the same
    /// layout computation, but writes every window's frame concurrently
    /// (see `write_windows_concurrently`) instead of looping through
    /// `frame_setter.set_frame` one window at a time. Only valid to call
    /// while `frame_setter` is suppressed (see `switch_workspace`'s
    /// `set_suppressed(true)`/`set_suppressed(false)` bracket): under
    /// suppression `frame_setter.set_frame` already reduces to exactly
    /// "settle any stale tween, then write the target straight to
    /// `AxWindow`," which is what this does directly — so bypassing
    /// `frame_setter` here doesn't change behavior, only the write loop's
    /// shape. Not reused by any other relayout call site: those need
    /// `Settings::animate` to keep working, which requires going through
    /// the ordinary, single-threaded `frame_setter`.
    fn relayout_monitor_concurrently(&mut self, monitor_id: u32) {
        let Some((name, area, gaps)) = self.tiled_layout_inputs(monitor_id) else {
            return;
        };
        let Some(tree) = self.workspaces.get(&name) else {
            return;
        };
        #[cfg(test)]
        self.relayout_calls.set(self.relayout_calls.get() + 1);

        let targets: Vec<(WindowId, Rect)> =
            if let Some(&fullscreen_node) = self.fullscreen_focus.get(&name) {
                tree.window_at(fullscreen_node)
                    .map(|id| vec![(id, area)])
                    .unwrap_or_default()
            } else {
                tree.layout(area, gaps).into_iter().collect()
            };

        for &(id, _) in &targets {
            if let Some(window) = self.windows.get_mut(&id) {
                self.frame_setter.finish(window);
            }
        }
        self.write_windows_concurrently(targets, AxWindow::set_frame);
    }

    /// `switch_workspace`-only variant of `reposition_floating_for_monitor`:
    /// only the common "restore a captured manual geometry" branch writes
    /// concurrently (see `write_windows_concurrently`) — the first-time
    /// placement branch (`place_floating_window`) stays sequential, since
    /// it reads/writes `floating_centered`/`floating_cascade_index` for its
    /// cascade-offset nudge, shared state that isn't safe to touch from
    /// multiple threads at once.
    fn reposition_floating_for_monitor_concurrently(&mut self, monitor_id: u32) {
        let Some(name) = self.active_workspace.get(&monitor_id).cloned() else {
            return;
        };
        let Some(area) = self.monitor_frame(monitor_id) else {
            return;
        };
        let ids = self.floating_windows_in(&name);
        let mut targets = Vec::new();
        for id in ids {
            let manual = self.placements.get(&id).and_then(|p| match &p.kind {
                PlacementKind::Floating { manual } => *manual,
                _ => None,
            });
            match manual {
                Some(geometry) => {
                    if let Some(window) = self.windows.get_mut(&id) {
                        self.frame_setter.finish(window);
                    }
                    targets.push((id, restore_floating_frame(geometry, area)));
                }
                None if !self.floating_placed.contains(&id) => {
                    self.place_floating_window(id, area);
                }
                None => {}
            }
        }
        self.write_windows_concurrently(targets, AxWindow::set_frame);
    }

    /// Finds the first configured floating rule matching `window`'s bundle
    /// id (and title regex, if the rule has one) — `None` if `window` has
    /// no resolvable bundle id or no rule matches. Shared by disposition
    /// resolution (`resolve_disposition`'s caller) and frame sizing
    /// (`initial_floating_frame_in`), so both agree on which rule "won."
    fn matching_floating_rule(&self, window: &AxWindow) -> Option<&CompiledFloatingRule> {
        let bundle_id = window.bundle_id()?;
        self.floating_rules.iter().find(|rule| {
            rule.app_id == bundle_id
                && rule.role.as_deref().is_none_or(|r| r == window.role())
                && rule
                    .subrole
                    .as_deref()
                    .is_none_or(|r| window.subrole() == Some(r))
                && rule
                    .title
                    .as_ref()
                    .is_none_or(|re| re.is_match(window.title()))
        })
    }

    /// Finds the first configured `workspace-rules` entry matching
    /// `window`'s bundle id (plus optional role/subrole) — `None` if
    /// unresolvable or no rule matches. Deliberately independent of
    /// `matching_floating_rule`: which workspace a window is created on has
    /// nothing to do with whether it ends up tiled or floating.
    fn matching_workspace_rule(&self, window: &AxWindow) -> Option<&CompiledWorkspaceRule> {
        let bundle_id = window.bundle_id()?;
        self.workspace_rules.iter().find(|rule| {
            rule.app_id == bundle_id
                && rule.role.as_deref().is_none_or(|r| r == window.role())
                && rule
                    .subrole
                    .as_deref()
                    .is_none_or(|r| window.subrole() == Some(r))
        })
    }

    /// `focused_monitor`'s frame, since new/reattached floating windows
    /// always land on the focused monitor's active workspace — falls back
    /// to a hardcoded size if it's unresolvable, so callers always have an
    /// area to size/center against.
    fn focused_monitor_area(&self) -> Rect {
        self.monitor_frame(self.focused_monitor).unwrap_or(Rect {
            x: 0.0,
            y: 0.0,
            width: 1440.0,
            height: 900.0,
        })
    }

    /// Sizes/centers a `Float`-disposition window within `area` — an
    /// explicit matching rule's `width`/`height`/`center` win, falling back
    /// to `floating_defaults` for anything the rule didn't specify (or if
    /// no rule matched at all, e.g. a `Dialog`-kind window with no app-id
    /// entry in `floating-rules`), and finally to the built-in absolute
    /// size. See `resolve_floating_extent` for the per-dimension
    /// precedence. Also returns whether centering was requested, so
    /// `place_floating_window` knows whether a post-write correction is
    /// meaningful.
    fn initial_floating_frame_in(&self, window: &AxWindow, area: Rect) -> (Rect, bool) {
        let rule = self.matching_floating_rule(window);
        let width = resolve_floating_extent(
            rule.and_then(|r| r.width),
            rule.and_then(|r| r.width_ratio),
            self.floating_defaults.width,
            self.floating_defaults.width_ratio,
            f64::from(tili_config::DEFAULT_FLOATING_WIDTH),
            area.width,
        );
        let height = resolve_floating_extent(
            rule.and_then(|r| r.height),
            rule.and_then(|r| r.height_ratio),
            self.floating_defaults.height,
            self.floating_defaults.height_ratio,
            f64::from(tili_config::DEFAULT_FLOATING_HEIGHT),
            area.height,
        );
        let center = rule
            .and_then(|r| r.center)
            .unwrap_or(self.floating_defaults.center);
        let (x, y) = if center {
            (
                area.x + (area.width - width) / 2.0,
                area.y + (area.height - height) / 2.0,
            )
        } else {
            (area.x, area.y)
        };

        (
            Rect {
                x,
                y,
                width,
                height,
            },
            center,
        )
    }

    /// The `cascade_offset` index for a window about to be centered in
    /// `workspace` — the count of *other* floating windows in that
    /// workspace currently in `floating_centered` (i.e. actually on screen
    /// via an auto-centered placement right now, not just "centered at
    /// some point in the past"). Zero whenever no other centered window is
    /// currently present, which is what keeps `cascade_offset(0) ==
    /// (0.0, 0.0)` — dead center — every time a window is reopened alone,
    /// rather than drifting via a counter that outlives the windows it
    /// counted.
    ///
    /// Also excludes anything in `pending_removal`, same reasoning as the
    /// `previous_pid` liveness check in `reveal_current_frontmost`: a
    /// window that just vanished from the latest AX scan sits there for
    /// `removal_grace` before `remove_placement` actually drops it from
    /// `floating_centered`, so without this exclusion a window closed and
    /// immediately reopened (a common flow) would see its own predecessor
    /// as "another centered window" for that whole grace window and land
    /// off-center — reproducing exactly the bug this function exists to
    /// fix, just gated on timing instead of gone for good.
    fn floating_cascade_index(&self, workspace: &str, exclude: WindowId) -> u32 {
        self.floating_windows_in(workspace)
            .into_iter()
            .filter(|&id| {
                id != exclude
                    && self.floating_centered.contains(&id)
                    && !self.pending_removal.contains_key(&id)
            })
            .count() as u32
    }

    /// Computes and writes `id`'s floating frame within `area` (see
    /// `initial_floating_frame_in`). If centering wasn't requested, this is
    /// a single `frame_setter.set_frame` write, like any other placement.
    ///
    /// If centering *was* requested, resizes first — before ever writing a
    /// position — and reads back the window's *actual* resulting size, so
    /// centering math always runs against the real size instead of the
    /// requested one. Some apps silently clamp the requested size along one
    /// axis (e.g. System Settings' fixed width); computing the centered
    /// position from the *requested* size first and correcting afterward
    /// (as this used to do) moves the window once to a wrong center and
    /// then again to the real one — two separate AX round-trips, and thus
    /// two separate paints, a visible double-flick on every placement of a
    /// fixed-one-axis app. Resizing first means the position is only ever
    /// written once. `AxWindow::set_size`/`set_position`'s own
    /// no-op-if-unchanged guards make both a no-op when nothing was
    /// actually clamped, matching the old behavior for a normally-resizable
    /// app. This bypasses `frame_setter` for the centered case (same
    /// precedent as `park`'s direct `set_position` call — a narrow,
    /// non-animateable write, not the tiled-layout seam `WindowFrameSetter`
    /// exists for).
    ///
    /// The centered position also gets a small `cascade_offset` nudge (via
    /// `floating_cascade_index`) so several same-sized floating windows
    /// centered at the same time don't all land on the exact same pixel
    /// and fully overlap — clamped back into `area` afterward in case the
    /// nudge would otherwise push the window off-screen (a small monitor,
    /// or little slack between the window and `area`'s edges). The index
    /// is derived from `floating_centered`, so reopening a window with no
    /// other centered floating window currently present always nudges by
    /// zero instead of drifting further each time.
    ///
    /// Note: this window also lives in its workspace's `tili_tree::Tree` as
    /// a `Node::Floating` leaf (see `place_new_window`'s `Floating` arm) so
    /// `workspace_focus`/`focused_node()` can address it, but that leaf is
    /// excluded from all `Tiles`/`Accordion` sizing — the only frame this
    /// function or its caller ever writes is the floating one computed
    /// here, never a tiled one from `relayout_active`/`relayout_monitor`. A
    /// brief visible flash of the app/OS's own default frame *before* this
    /// runs (i.e. before tili's `WindowsChanged` event for the new window
    /// is even dispatched) is a separate, accepted architectural
    /// limitation: reacting after window creation via AX notifications has
    /// no way to intercept the very first paint, and there's no reliable
    /// public-API way to hide a single window (only a whole app) to mask
    /// it.
    fn place_floating_window(&mut self, id: WindowId, area: Rect) {
        let Some(window) = self.windows.get(&id) else {
            return;
        };
        let (frame, center) = self.initial_floating_frame_in(window, area);

        let cascade = center
            .then(|| self.placements.get(&id).map(|p| p.workspace.clone()))
            .flatten()
            .map(|workspace| cascade_offset(self.floating_cascade_index(&workspace, id)));

        let Some(window) = self.windows.get_mut(&id) else {
            return;
        };
        if !center {
            self.frame_setter.set_frame(window, frame);
        } else {
            // Settles any in-flight animation first — the size-discovery
            // write below goes straight to `AxWindow`, bypassing
            // `frame_setter`, so a stale tween could otherwise resume on a
            // later tick and fight it.
            self.frame_setter.finish(window);
            // Direct, instant write on purpose: this is measurement, not a
            // user-facing placement — resizing first and reading back
            // `live_frame()` is how a fixed-one-axis app's real (possibly
            // clamped) size gets discovered, and animating a measurement
            // step would only delay finding it out.
            window.set_size(frame.width, frame.height);
            let actual = window.live_frame();
            // `set_size` only updates the cache to the *requested*
            // width/height, not the real (possibly clamped) one `actual`
            // just discovered — sync it before the animated move below
            // compares against it, so a `TweenedFrameSetter` interpolates
            // from the window's true current size instead of the
            // never-actually-on-screen requested one.
            window.sync_frame(actual);
            let (dx, dy) = cascade.unwrap_or((0.0, 0.0));
            let cascaded_x = (area.x + (area.width - actual.width) / 2.0 + dx)
                .clamp(area.x, area.x + area.width - actual.width);
            let cascaded_y = (area.y + (area.height - actual.height) / 2.0 + dy)
                .clamp(area.y, area.y + area.height - actual.height);
            // The actual placement — routed through the seam so it
            // animates like any other floating placement.
            self.frame_setter.set_frame(
                window,
                Rect {
                    x: cascaded_x,
                    y: cascaded_y,
                    width: actual.width,
                    height: actual.height,
                },
            );
        }
        self.floating_placed.insert(id);
        if center {
            self.floating_centered.insert(id);
        } else {
            self.floating_centered.remove(&id);
        }
    }
}

/// Which workspace should be active at daemon startup:
/// `settings.default-workspace` if it names a declared workspace, else the
/// alphabetically-first declared workspace, else `None` (config declares no
/// workspaces at all — caller keeps the internal `DEFAULT_WORKSPACE` seed).
fn resolve_default_workspace(config: &tili_config::Config) -> Option<String> {
    if let Some(name) = &config.settings.default_workspace
        && config.workspaces.iter().any(|w| &w.name == name)
    {
        return Some(name.clone());
    }
    config.workspaces.iter().map(|w| &w.name).min().cloned()
}

/// Resolves a raw `layout="..."` config string against `"tiles"`/
/// `"accordion"`, falling back to `fallback` when unset — and, unlike
/// `default_root_orientation`'s silent fallback, warning via
/// `config_warnings` when it's set to something else entirely, so a typo
/// doesn't silently do nothing.
fn resolve_layout(
    raw: Option<&str>,
    warnings: &mut Vec<String>,
    context: &str,
    fallback: tili_tree::Layout,
) -> tili_tree::Layout {
    match raw {
        None => fallback,
        Some("tiles") => tili_tree::Layout::Tiles,
        Some("accordion") => tili_tree::Layout::Accordion,
        Some(other) => {
            let warning = format!(
                "{context}: invalid layout '{other}' (expected \"tiles\" or \"accordion\") — \
                 falling back to {fallback:?}"
            );
            eprintln!("tili-daemon: {warning}");
            warnings.push(warning);
            fallback
        }
    }
}

fn to_tree_gaps(gaps: tili_config::Gaps) -> Gaps {
    let (top, right, bottom, left) = gaps.outer;
    Gaps {
        inner: f64::from(gaps.inner),
        outer: (
            f64::from(top),
            f64::from(right),
            f64::from(bottom),
            f64::from(left),
        ),
        outer_solo: gaps.outer_solo.map(|(top, right, bottom, left)| {
            (
                f64::from(top),
                f64::from(right),
                f64::from(bottom),
                f64::from(left),
            )
        }),
        accordion: f64::from(gaps.accordion),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn special_kind_priority_order() {
        assert_eq!(
            special_kind(true, true, true),
            Some(SpecialKind::HiddenApplication)
        );
        assert_eq!(
            special_kind(false, true, true),
            Some(SpecialKind::Minimized)
        );
        assert_eq!(
            special_kind(false, false, true),
            Some(SpecialKind::NativeFullscreen)
        );
        assert_eq!(special_kind(false, false, false), None);
    }

    #[test]
    fn current_special_mirrors_special_kind() {
        assert_eq!(
            current_special(&PlacementKind::HiddenApplication(Restore::Tiled)),
            Some(SpecialKind::HiddenApplication)
        );
        assert_eq!(
            current_special(&PlacementKind::Minimized(Restore::Floating)),
            Some(SpecialKind::Minimized)
        );
        assert_eq!(
            current_special(&PlacementKind::NativeFullscreen(Restore::Tiled)),
            Some(SpecialKind::NativeFullscreen)
        );
        assert_eq!(current_special(&PlacementKind::Tiled), None);
        assert_eq!(
            current_special(&PlacementKind::Floating { manual: None }),
            None
        );
        assert_eq!(current_special(&PlacementKind::Popup), None);
    }

    #[test]
    fn restore_for_carries_through_special_states() {
        assert_eq!(restore_for(&PlacementKind::Tiled), Restore::Tiled);
        assert_eq!(
            restore_for(&PlacementKind::Floating { manual: None }),
            Restore::Floating
        );
        assert_eq!(
            restore_for(&PlacementKind::Minimized(Restore::Floating)),
            Restore::Floating
        );
        assert_eq!(restore_for(&PlacementKind::Popup), Restore::Tiled);
    }

    #[test]
    fn classify_new_window_priority_order() {
        // HiddenApplication beats everything else, even a `Tile`-disposition
        // window that would otherwise just end up Tiled.
        assert!(matches!(
            classify_new_window(tili_config::FloatingRuleMode::Tile, true, true, true),
            PlacementKind::HiddenApplication(Restore::Tiled)
        ));
        // Minimized beats NativeFullscreen.
        assert!(matches!(
            classify_new_window(tili_config::FloatingRuleMode::Tile, false, true, true),
            PlacementKind::Minimized(Restore::Tiled)
        ));
        assert!(matches!(
            classify_new_window(tili_config::FloatingRuleMode::Tile, false, false, true),
            PlacementKind::NativeFullscreen(Restore::Tiled)
        ));
    }

    #[test]
    fn classify_new_window_ignore_disposition_is_popup() {
        assert!(matches!(
            classify_new_window(tili_config::FloatingRuleMode::Ignore, false, false, false),
            PlacementKind::Popup
        ));
    }

    #[test]
    fn is_protected_finder_dialog_matches_only_the_two_named_titles() {
        assert!(is_protected_finder_dialog(Some("com.apple.finder"), "Copy"));
        assert!(is_protected_finder_dialog(
            Some("com.apple.finder"),
            "Connect to Server"
        ));
        assert!(!is_protected_finder_dialog(
            Some("com.apple.finder"),
            "Copy 5 Items"
        ));
        assert!(!is_protected_finder_dialog(
            Some("com.apple.finder"),
            "Downloads"
        ));
        assert!(!is_protected_finder_dialog(
            Some("com.apple.TextEdit"),
            "Copy"
        ));
        assert!(!is_protected_finder_dialog(None, "Copy"));
    }

    #[test]
    fn is_finder_quick_look_window_matches_the_preview_panel_and_its_closing_overlays() {
        assert!(is_finder_quick_look_window(
            Some("com.apple.finder"),
            tili_ax::WindowKind::Popup,
            "Quick Look"
        ));
        assert!(is_finder_quick_look_window(
            Some("com.apple.finder"),
            tili_ax::WindowKind::Popup,
            ""
        ));
        assert!(!is_finder_quick_look_window(
            Some("com.apple.finder"),
            tili_ax::WindowKind::Standard,
            "Documents"
        ));
        assert!(!is_finder_quick_look_window(
            Some("com.apple.TextEdit"),
            tili_ax::WindowKind::Popup,
            "Quick Look"
        ));
        assert!(!is_finder_quick_look_window(
            None,
            tili_ax::WindowKind::Popup,
            "Quick Look"
        ));
    }

    #[test]
    fn is_finder_get_info_window_matches_only_finder_dialogs_ending_in_info() {
        assert!(is_finder_get_info_window(
            Some("com.apple.finder"),
            tili_ax::WindowKind::Dialog,
            "Surge XT Info"
        ));
        assert!(is_finder_get_info_window(
            Some("com.apple.finder"),
            tili_ax::WindowKind::Dialog,
            "3 Items Info"
        ));
        assert!(!is_finder_get_info_window(
            Some("com.apple.finder"),
            tili_ax::WindowKind::Standard,
            "Server Info"
        ));
        assert!(!is_finder_get_info_window(
            Some("com.apple.finder"),
            tili_ax::WindowKind::Dialog,
            "Documents"
        ));
        assert!(!is_finder_get_info_window(
            Some("com.apple.TextEdit"),
            tili_ax::WindowKind::Dialog,
            "Surge XT Info"
        ));
        assert!(!is_finder_get_info_window(
            None,
            tili_ax::WindowKind::Dialog,
            "Surge XT Info"
        ));
    }

    #[test]
    fn is_transient_empty_dialog_matches_the_chromeless_hud_glyph_shape() {
        assert!(is_transient_empty_dialog(
            tili_ax::WindowKind::Dialog,
            "",
            false
        ));
    }

    #[test]
    fn is_transient_empty_dialog_does_not_match_a_zoom_button_settings_window() {
        // A Preferences/Settings-style window (e.g. System Settings' own
        // main window) reaches `WindowKind::Dialog` via the zoom-but-no-
        // fullscreen heuristic, not via subrole match, and always has a
        // zoom button. Its `AXTitle` can legitimately still be empty at
        // scan time (a routine AX timing race, not a transient overlay) —
        // this must not force-`Ignore` it and override the user's own
        // `floating-rules` entry for it.
        assert!(!is_transient_empty_dialog(
            tili_ax::WindowKind::Dialog,
            "",
            true
        ));
    }

    #[test]
    fn is_transient_empty_dialog_requires_an_empty_title() {
        assert!(!is_transient_empty_dialog(
            tili_ax::WindowKind::Dialog,
            "Some Dialog",
            false
        ));
    }

    #[test]
    fn is_transient_empty_dialog_requires_dialog_kind() {
        assert!(!is_transient_empty_dialog(
            tili_ax::WindowKind::Standard,
            "",
            false
        ));
        assert!(!is_transient_empty_dialog(
            tili_ax::WindowKind::Popup,
            "",
            false
        ));
    }

    #[test]
    fn is_system_settings_suggestion_popup_matches_the_confirmed_shape() {
        // Confirmed via diagnostic logging: the search-suggestions dropdown
        // is a borderless, empty-titled `Popup`-kind window belonging to
        // System Settings.
        assert!(is_system_settings_suggestion_popup(
            Some("com.apple.systempreferences"),
            tili_ax::WindowKind::Popup,
            ""
        ));
    }

    #[test]
    fn is_system_settings_suggestion_popup_is_scoped_to_the_app() {
        assert!(!is_system_settings_suggestion_popup(
            Some("com.apple.finder"),
            tili_ax::WindowKind::Popup,
            ""
        ));
        assert!(!is_system_settings_suggestion_popup(
            None,
            tili_ax::WindowKind::Popup,
            ""
        ));
    }

    #[test]
    fn is_system_settings_suggestion_popup_requires_popup_kind_and_empty_title() {
        assert!(!is_system_settings_suggestion_popup(
            Some("com.apple.systempreferences"),
            tili_ax::WindowKind::Dialog,
            ""
        ));
        assert!(!is_system_settings_suggestion_popup(
            Some("com.apple.systempreferences"),
            tili_ax::WindowKind::Popup,
            "Wi‑Fi"
        ));
    }

    #[test]
    fn is_non_resizable_window_matches_only_non_resizable() {
        assert!(is_non_resizable_window(false));
        assert!(!is_non_resizable_window(true));
    }

    #[test]
    fn should_ignore_new_window_is_false_for_an_ordinary_window() {
        assert!(!should_ignore_new_window(
            Some("com.example.SomeApp"),
            tili_ax::WindowKind::Standard,
            "Untitled",
            true,
            true,
        ));
    }

    #[test]
    fn should_ignore_new_window_matches_a_system_ui_bundle_regardless_of_shape() {
        assert!(should_ignore_new_window(
            Some("com.apple.dock"),
            tili_ax::WindowKind::Standard,
            "Untitled",
            true,
            true,
        ));
    }

    #[test]
    fn should_ignore_new_window_matches_a_non_resizable_window_regardless_of_bundle() {
        assert!(should_ignore_new_window(
            Some("com.example.SomeApp"),
            tili_ax::WindowKind::Standard,
            "Untitled",
            true,
            false,
        ));
    }

    #[test]
    fn should_ignore_new_window_matches_a_transient_empty_dialog() {
        assert!(should_ignore_new_window(
            Some("com.example.SomeApp"),
            tili_ax::WindowKind::Dialog,
            "",
            false,
            true,
        ));
    }

    #[test]
    fn classify_new_window_float_disposition_is_floating() {
        assert!(matches!(
            classify_new_window(tili_config::FloatingRuleMode::Float, false, false, false),
            PlacementKind::Floating { manual: None }
        ));
    }

    #[test]
    fn classify_new_window_tile_disposition_is_tiled() {
        assert!(matches!(
            classify_new_window(tili_config::FloatingRuleMode::Tile, false, false, false),
            PlacementKind::Tiled
        ));
    }

    #[test]
    fn resolve_disposition_falls_back_to_kind_when_no_rule_matches() {
        assert_eq!(
            resolve_disposition(tili_ax::WindowKind::Popup, None),
            tili_config::FloatingRuleMode::Ignore
        );
        assert_eq!(
            resolve_disposition(tili_ax::WindowKind::Dialog, None),
            tili_config::FloatingRuleMode::Float
        );
        assert_eq!(
            resolve_disposition(tili_ax::WindowKind::Standard, None),
            tili_config::FloatingRuleMode::Tile
        );
    }

    #[test]
    fn resolve_disposition_explicit_rule_mode_overrides_popup_kind() {
        // A `Popup`-kind window (AX-ambiguous) that matches a `mode="float"`
        // rule is rescued into floating — previously impossible, since kind
        // unconditionally beat any rule match.
        assert_eq!(
            resolve_disposition(
                tili_ax::WindowKind::Popup,
                Some(tili_config::FloatingRuleMode::Float)
            ),
            tili_config::FloatingRuleMode::Float
        );
    }

    #[test]
    fn resolve_disposition_explicit_rule_mode_overrides_dialog_kind() {
        // A `Dialog`-kind window that matches a `mode="tile"` rule is forced
        // back into tiling — previously impossible, `Dialog` always floated.
        assert_eq!(
            resolve_disposition(
                tili_ax::WindowKind::Dialog,
                Some(tili_config::FloatingRuleMode::Tile)
            ),
            tili_config::FloatingRuleMode::Tile
        );
    }

    #[test]
    fn resolve_disposition_explicit_ignore_overrides_standard_kind() {
        assert_eq!(
            resolve_disposition(
                tili_ax::WindowKind::Standard,
                Some(tili_config::FloatingRuleMode::Ignore)
            ),
            tili_config::FloatingRuleMode::Ignore
        );
    }

    #[test]
    fn demote_to_special_removes_a_tiled_window_from_its_tree() {
        let mut state = WmState::default();
        let id: WindowId = 1;
        let root_orientation = state.root_orientation_hint();
        state
            .workspaces
            .get_mut(DEFAULT_WORKSPACE)
            .unwrap()
            .insert_window(id, None, root_orientation);
        state.placements.insert(
            id,
            Placement {
                workspace: DEFAULT_WORKSPACE.to_string(),
                kind: PlacementKind::Tiled,
            },
        );

        state.demote_to_special(id, SpecialKind::Minimized, Restore::Tiled);

        assert!(
            state
                .workspaces
                .get(DEFAULT_WORKSPACE)
                .unwrap()
                .find_node(id)
                .is_none()
        );
        assert!(matches!(
            state.placements.get(&id).map(|p| &p.kind),
            Some(PlacementKind::Minimized(Restore::Tiled))
        ));
    }

    #[test]
    fn remove_from_tree_returns_the_reassigned_node_when_its_workspace_is_visible() {
        let mut state = WmState::default();
        let focused_id: WindowId = 1;
        let sibling_id: WindowId = 2;
        let root_orientation = state.root_orientation_hint();
        let tree = state.workspaces.get_mut(DEFAULT_WORKSPACE).unwrap();
        let focused_node = tree.insert_window(focused_id, None, root_orientation);
        let sibling_node = tree.insert_window(sibling_id, None, root_orientation);
        state
            .workspace_focus
            .insert(DEFAULT_WORKSPACE.to_string(), focused_node);

        let reassigned = state.remove_from_tree(focused_id, DEFAULT_WORKSPACE);

        assert_eq!(
            reassigned,
            Some(sibling_node),
            "remove_placement needs this to real-focus the sibling instead of leaving \
             whatever macOS's own app-reactivation history picked"
        );
    }

    #[test]
    fn remove_from_tree_returns_none_when_the_workspace_is_parked() {
        let mut state = WmState::default();
        state.workspaces.insert("side".to_string(), Tree::new());
        let focused_id: WindowId = 1;
        let sibling_id: WindowId = 2;
        let root_orientation = state.root_orientation_hint();
        let tree = state.workspaces.get_mut("side").unwrap();
        let focused_node = tree.insert_window(focused_id, None, root_orientation);
        tree.insert_window(sibling_id, None, root_orientation);
        state
            .workspace_focus
            .insert("side".to_string(), focused_node);
        // "side" was never added to `active_workspace`, so it's parked —
        // nothing should get real-focused on a workspace nobody can see.

        let reassigned = state.remove_from_tree(focused_id, "side");

        assert_eq!(reassigned, None);
    }

    #[test]
    fn remove_from_tree_returns_none_when_the_removed_window_was_not_the_recorded_focus() {
        let mut state = WmState::default();
        let focused_id: WindowId = 1;
        let other_id: WindowId = 2;
        let root_orientation = state.root_orientation_hint();
        let tree = state.workspaces.get_mut(DEFAULT_WORKSPACE).unwrap();
        let focused_node = tree.insert_window(focused_id, None, root_orientation);
        tree.insert_window(other_id, None, root_orientation);
        state
            .workspace_focus
            .insert(DEFAULT_WORKSPACE.to_string(), focused_node);

        let reassigned = state.remove_from_tree(other_id, DEFAULT_WORKSPACE);

        assert_eq!(reassigned, None);
    }

    #[test]
    fn has_native_fullscreen_window_is_scoped_to_one_workspace() {
        let mut state = WmState::default();
        state.workspaces.insert("side".to_string(), Tree::new());
        state.placements.insert(
            1,
            Placement {
                workspace: DEFAULT_WORKSPACE.to_string(),
                kind: PlacementKind::Tiled,
            },
        );
        assert!(!state.has_native_fullscreen_window(DEFAULT_WORKSPACE));

        state.placements.insert(
            2,
            Placement {
                workspace: "side".to_string(),
                kind: PlacementKind::NativeFullscreen(Restore::Tiled),
            },
        );

        assert!(state.has_native_fullscreen_window("side"));
        assert!(
            !state.has_native_fullscreen_window(DEFAULT_WORKSPACE),
            "another workspace's fullscreen must not suppress this one's focus handoff"
        );
    }

    #[test]
    fn remove_placement_still_updates_the_tree_while_native_fullscreen_is_showing() {
        // Only the real-focus assertion is skipped there — the tree
        // bookkeeping has to happen either way, or the removed window would
        // keep occupying a slot forever.
        let mut state = WmState::default();
        let closed_id: WindowId = 1;
        let sibling_id: WindowId = 2;
        let fullscreen_id: WindowId = 3;
        let root_orientation = state.root_orientation_hint();
        let tree = state.workspaces.get_mut(DEFAULT_WORKSPACE).unwrap();
        let closed_node = tree.insert_window(closed_id, None, root_orientation);
        let sibling_node = tree.insert_window(sibling_id, None, root_orientation);
        state
            .workspace_focus
            .insert(DEFAULT_WORKSPACE.to_string(), closed_node);
        for (id, kind) in [
            (closed_id, PlacementKind::Tiled),
            (sibling_id, PlacementKind::Tiled),
            (
                fullscreen_id,
                PlacementKind::NativeFullscreen(Restore::Tiled),
            ),
        ] {
            state.placements.insert(
                id,
                Placement {
                    workspace: DEFAULT_WORKSPACE.to_string(),
                    kind,
                },
            );
        }

        state.remove_placement(closed_id, None);

        assert!(!state.placements.contains_key(&closed_id));
        assert!(
            state
                .workspaces
                .get(DEFAULT_WORKSPACE)
                .unwrap()
                .find_node(closed_id)
                .is_none()
        );
        assert_eq!(
            state.workspace_focus.get(DEFAULT_WORKSPACE),
            Some(&sibling_node)
        );
    }

    #[test]
    fn promote_from_special_reinserts_into_the_tree() {
        let mut state = WmState::default();
        let id: WindowId = 1;
        state.placements.insert(
            id,
            Placement {
                workspace: DEFAULT_WORKSPACE.to_string(),
                kind: PlacementKind::Minimized(Restore::Tiled),
            },
        );

        state.promote_from_special(id, Restore::Tiled);

        assert!(matches!(
            state.placements.get(&id).map(|p| &p.kind),
            Some(PlacementKind::Tiled)
        ));
        assert!(
            state
                .workspaces
                .get(DEFAULT_WORKSPACE)
                .unwrap()
                .find_node(id)
                .is_some()
        );
    }

    #[test]
    fn reconcile_existing_placement_round_trips_tiled_through_minimized() {
        let mut state = WmState::default();
        let id: WindowId = 1;
        let root_orientation = state.root_orientation_hint();
        state
            .workspaces
            .get_mut(DEFAULT_WORKSPACE)
            .unwrap()
            .insert_window(id, None, root_orientation);
        state.placements.insert(
            id,
            Placement {
                workspace: DEFAULT_WORKSPACE.to_string(),
                kind: PlacementKind::Tiled,
            },
        );

        // Goes minimized.
        state.reconcile_existing_placement(id, false, true, false);
        assert!(matches!(
            state.placements.get(&id).map(|p| &p.kind),
            Some(PlacementKind::Minimized(Restore::Tiled))
        ));
        assert!(
            state
                .workspaces
                .get(DEFAULT_WORKSPACE)
                .unwrap()
                .find_node(id)
                .is_none()
        );

        // Un-minimizes — back to Tiled, reinserted into the tree.
        state.reconcile_existing_placement(id, false, false, false);
        assert!(matches!(
            state.placements.get(&id).map(|p| &p.kind),
            Some(PlacementKind::Tiled)
        ));
        assert!(
            state
                .workspaces
                .get(DEFAULT_WORKSPACE)
                .unwrap()
                .find_node(id)
                .is_some()
        );
    }

    #[test]
    fn reconcile_existing_placement_no_change_is_a_no_op() {
        let mut state = WmState::default();
        let id: WindowId = 1;
        let root_orientation = state.root_orientation_hint();
        state
            .workspaces
            .get_mut(DEFAULT_WORKSPACE)
            .unwrap()
            .insert_window(id, None, root_orientation);
        state.placements.insert(
            id,
            Placement {
                workspace: DEFAULT_WORKSPACE.to_string(),
                kind: PlacementKind::Tiled,
            },
        );

        state.reconcile_existing_placement(id, false, false, false);

        assert!(matches!(
            state.placements.get(&id).map(|p| &p.kind),
            Some(PlacementKind::Tiled)
        ));
        assert!(
            state
                .workspaces
                .get(DEFAULT_WORKSPACE)
                .unwrap()
                .find_node(id)
                .is_some()
        );
    }

    // `WmState`'s `Default` derives its fields (monitors, workspaces, etc.)
    // from real state (`tili_ax::list_monitors()`) rather than being a
    // simple zero-value default, so overwriting a couple of fields
    // afterward for a deterministic test fixture is clearer here than
    // spelling out every other field via `..Default::default()`.
    /// The 1920x1080 main + 1920x1080 secondary two-monitor fixture several
    /// tests below share verbatim.
    fn two_monitor_fixture() -> Vec<Monitor> {
        vec![
            Monitor {
                id: 1,
                frame: Rect {
                    x: 0.0,
                    y: 0.0,
                    width: 1920.0,
                    height: 1080.0,
                },
                is_main: true,
                notch: 0.0,
                pretty_name: "Main Display".to_string(),
                ordinal: 1,
            },
            Monitor {
                id: 2,
                frame: Rect {
                    x: 1920.0,
                    y: 0.0,
                    width: 1920.0,
                    height: 1080.0,
                },
                is_main: false,
                notch: 0.0,
                pretty_name: "Display 2".to_string(),
                ordinal: 2,
            },
        ]
    }

    fn three_monitor_fixture() -> Vec<Monitor> {
        vec![
            Monitor {
                id: 1,
                frame: Rect {
                    x: 0.0,
                    y: 0.0,
                    width: 1920.0,
                    height: 1080.0,
                },
                is_main: true,
                notch: 0.0,
                pretty_name: "Main Display".to_string(),
                ordinal: 1,
            },
            Monitor {
                id: 2,
                frame: Rect {
                    x: 1920.0,
                    y: 0.0,
                    width: 1920.0,
                    height: 1080.0,
                },
                is_main: false,
                notch: 0.0,
                pretty_name: "Display 2".to_string(),
                ordinal: 2,
            },
            Monitor {
                id: 3,
                frame: Rect {
                    x: 3840.0,
                    y: 0.0,
                    width: 1920.0,
                    height: 1080.0,
                },
                is_main: false,
                notch: 0.0,
                pretty_name: "Display 3".to_string(),
                ordinal: 3,
            },
        ]
    }

    #[allow(clippy::field_reassign_with_default)]
    fn floating_test_state() -> WmState {
        let mut state = WmState::default();
        state.monitors = vec![Monitor {
            id: 1,
            frame: Rect {
                x: 0.0,
                y: 0.0,
                width: 1920.0,
                height: 1080.0,
            },
            is_main: true,
            notch: 0.0,
            pretty_name: "Main Display".to_string(),
            ordinal: 1,
        }];
        state.active_workspace.clear();
        state
            .active_workspace
            .insert(1, DEFAULT_WORKSPACE.to_string());
        state.focused_monitor = 1;
        state
    }

    /// m1=work, m2=entertain; random home m1, trade home m2.
    #[allow(clippy::field_reassign_with_default)]
    fn two_display_home_state() -> WmState {
        let mut state = WmState::default();
        state.monitors = two_monitor_fixture();
        state.workspaces.clear();
        for name in ["work", "entertain", "random", "trade"] {
            state.workspaces.insert(name.to_string(), Tree::new());
        }
        state.active_workspace.clear();
        state.active_workspace.insert(1, "work".to_string());
        state.active_workspace.insert(2, "entertain".to_string());
        state.workspace_monitor.clear();
        state.workspace_monitor.insert("work".to_string(), 1);
        state.workspace_monitor.insert("entertain".to_string(), 2);
        state.workspace_monitor.insert("random".to_string(), 1);
        state.workspace_monitor.insert("trade".to_string(), 2);
        state.last_active_workspace.insert(1, "work".to_string());
        state
            .last_active_workspace
            .insert(2, "entertain".to_string());
        state.focused_monitor = 1;
        state
    }

    #[test]
    fn capture_and_restore_round_trip_within_the_same_area() {
        let area = Rect {
            x: 0.0,
            y: 0.0,
            width: 1920.0,
            height: 1080.0,
        };
        let frame = Rect {
            x: 200.0,
            y: 150.0,
            width: 800.0,
            height: 600.0,
        };
        let geometry = capture_float_geometry(frame, area);
        let restored = restore_floating_frame(geometry, area);
        assert!((restored.x - frame.x).abs() < 0.01);
        assert!((restored.y - frame.y).abs() < 0.01);
        assert!((restored.width - frame.width).abs() < 0.01);
        assert!((restored.height - frame.height).abs() < 0.01);
    }

    #[test]
    fn restore_floating_frame_scales_proportionally_to_a_different_area() {
        let original_area = Rect {
            x: 0.0,
            y: 0.0,
            width: 1920.0,
            height: 1080.0,
        };
        // Bottom-right quadrant of the original area.
        let frame = Rect {
            x: 960.0,
            y: 540.0,
            width: 960.0,
            height: 540.0,
        };
        let geometry = capture_float_geometry(frame, original_area);

        let new_area = Rect {
            x: 0.0,
            y: 0.0,
            width: 3840.0,
            height: 2160.0,
        };
        let restored = restore_floating_frame(geometry, new_area);
        assert!((restored.x - 1920.0).abs() < 0.01);
        assert!((restored.y - 1080.0).abs() < 0.01);
        assert!((restored.width - 1920.0).abs() < 0.01);
        assert!((restored.height - 1080.0).abs() < 0.01);
    }

    #[test]
    fn restore_floating_frame_clamps_when_the_captured_size_no_longer_fits() {
        let original_area = Rect {
            x: 0.0,
            y: 0.0,
            width: 1920.0,
            height: 1080.0,
        };
        let frame = Rect {
            x: 100.0,
            y: 100.0,
            width: 1800.0,
            height: 1000.0,
        };
        let geometry = capture_float_geometry(frame, original_area);

        let smaller_area = Rect {
            x: 0.0,
            y: 0.0,
            width: 1280.0,
            height: 800.0,
        };
        let restored = restore_floating_frame(geometry, smaller_area);
        assert!(restored.width <= smaller_area.width);
        assert!(restored.height <= smaller_area.height);
        assert!(restored.x >= smaller_area.x);
        assert!(restored.y >= smaller_area.y);
        assert!(restored.x + restored.width <= smaller_area.x + smaller_area.width + 0.01);
        assert!(restored.y + restored.height <= smaller_area.y + smaller_area.height + 0.01);
    }

    #[test]
    fn frames_match_within_epsilon_but_not_beyond() {
        let a = Rect {
            x: 0.0,
            y: 0.0,
            width: 100.0,
            height: 100.0,
        };
        let close = Rect {
            x: 0.2,
            y: 0.0,
            width: 100.0,
            height: 100.0,
        };
        let far = Rect {
            x: 5.0,
            y: 0.0,
            width: 100.0,
            height: 100.0,
        };
        assert!(frames_match(a, close));
        assert!(!frames_match(a, far));
    }

    #[test]
    fn native_tab_target_requires_same_pid_frame_and_a_missing_old_window() {
        let frame = Rect {
            x: 0.0,
            y: 0.0,
            width: 800.0,
            height: 600.0,
        };
        let candidates = [NativeTabCandidate {
            id: 1,
            pid: 42,
            frame,
            positionable: true,
            pending_removal: false,
            was_tab_group: false,
        }];
        let mut fresh = HashSet::new();
        fresh.insert(2);
        assert_eq!(
            native_tab_target(&candidates, &fresh, 2, 42, frame, None),
            Some(1)
        );
        // Wrong pid or frame never matches.
        assert_eq!(
            native_tab_target(&candidates, &fresh, 2, 7, frame, None),
            None
        );
        assert_eq!(
            native_tab_target(&candidates, &fresh, 2, 42, Rect { x: 10.0, ..frame }, None),
            None
        );
    }

    #[test]
    fn native_tab_target_accepts_the_tab_bar_frame_shift_of_a_hidden_tab() {
        // Real-hardware Terminal.app geometry, two shapes: a tab bar next to
        // another window (`620x751` -> `620x770`), and a tab bar on a window
        // alone at the solo gaps (`1120x699` -> `1118x741`, losing 2pt of
        // width).
        let old = Rect {
            x: 644.0,
            y: 33.0,
            width: 620.0,
            height: 751.0,
        };
        let with_tab_bar = Rect {
            x: 644.0,
            y: 30.0,
            width: 620.0,
            height: 770.0,
        };
        let solo = Rect {
            x: 80.0,
            y: 45.0,
            width: 1120.0,
            height: 699.0,
        };
        let solo_with_tab_bar = Rect {
            x: 80.0,
            y: 45.0,
            width: 1118.0,
            height: 741.0,
        };
        let candidates = [NativeTabCandidate {
            id: 1,
            pid: 42,
            frame: old,
            positionable: true,
            pending_removal: false,
            was_tab_group: false,
        }];
        let solo_candidates = [NativeTabCandidate {
            frame: solo,
            ..candidates[0]
        }];
        // Old tab hidden from the current scan, new window is a multi-tab
        // group member (2+ tab buttons).
        let old_hidden: HashSet<WindowId> = [2].into_iter().collect();
        assert_eq!(
            native_tab_target(&candidates, &old_hidden, 2, 42, with_tab_bar, Some(2)),
            Some(1)
        );
        assert_eq!(
            native_tab_target(
                &solo_candidates,
                &old_hidden,
                2,
                42,
                solo_with_tab_bar,
                Some(2)
            ),
            Some(1)
        );
        // No tab group at all, or a group with just one tab button, is not
        // proof of an already-populated group — a brand-new window has one.
        assert_eq!(
            native_tab_target(&candidates, &old_hidden, 2, 42, with_tab_bar, None),
            None
        );
        assert_eq!(
            native_tab_target(&candidates, &old_hidden, 2, 42, with_tab_bar, Some(1)),
            None
        );
        // Horizontal movement beyond a few points is never a tab-bar change.
        assert_eq!(
            native_tab_target(
                &candidates,
                &old_hidden,
                2,
                42,
                Rect {
                    x: old.x + 10.0,
                    ..with_tab_bar
                },
                Some(2)
            ),
            None
        );
        // Same for a width change larger than rounding slack.
        assert_eq!(
            native_tab_target(
                &solo_candidates,
                &old_hidden,
                2,
                42,
                Rect {
                    width: solo.width - 20.0,
                    ..solo_with_tab_bar
                },
                Some(2)
            ),
            None
        );
        // A vertical change beyond a tab bar's height is not a tab replacement.
        assert_eq!(
            native_tab_target(
                &candidates,
                &old_hidden,
                2,
                42,
                Rect {
                    y: old.y - 100.0,
                    height: old.height + 100.0,
                    ..with_tab_bar
                },
                Some(2)
            ),
            None
        );
        // A still-enumerable sibling keeps the strict frame requirement.
        let both_visible: HashSet<WindowId> = [1, 2].into_iter().collect();
        assert_eq!(
            native_tab_target(&candidates, &both_visible, 2, 42, with_tab_bar, Some(2)),
            None
        );
    }

    #[test]
    fn native_tab_target_folds_the_last_close_when_the_old_window_was_a_tab() {
        let old = Rect {
            x: 80.0,
            y: 45.0,
            width: 1120.0,
            height: 699.0,
        };
        // 2→1 close: the surviving window's tab bar is gone, so it can't
        // prove a multi-tab group by itself.
        let survivor = Rect {
            width: 1118.0,
            height: 693.0,
            ..old
        };
        let fresh: HashSet<WindowId> = [2].into_iter().collect();
        let tab_member = [NativeTabCandidate {
            id: 1,
            pid: 42,
            frame: old,
            positionable: true,
            pending_removal: false,
            was_tab_group: true,
        }];
        assert_eq!(
            native_tab_target(&tab_member, &fresh, 2, 42, survivor, None),
            Some(1)
        );
        // A window that was never a tab-group member stays refused — this is
        // exactly what the ≥2-tab-button proof is there to protect.
        let plain = [NativeTabCandidate {
            was_tab_group: false,
            ..tab_member[0]
        }];
        assert_eq!(
            native_tab_target(&plain, &fresh, 2, 42, survivor, None),
            None
        );
    }

    #[test]
    fn native_tab_target_bounds_the_tab_bar_slack_by_the_candidates_own_size() {
        // With `inner 0` and many stacked windows, a neighboring tile can be
        // closer than the flat 64pt vertical tolerance. Capping it at half
        // the candidate's height keeps a hold of the right tile only.
        let old = Rect {
            x: 0.0,
            y: 0.0,
            width: 800.0,
            height: 100.0,
        };
        let neighbor = Rect {
            x: 0.0,
            y: 100.0,
            ..old
        };
        let candidates = [
            NativeTabCandidate {
                id: 1,
                pid: 42,
                frame: old,
                positionable: true,
                pending_removal: false,
                was_tab_group: false,
            },
            NativeTabCandidate {
                id: 3,
                pid: 42,
                frame: neighbor,
                positionable: true,
                pending_removal: false,
                was_tab_group: false,
            },
        ];
        let fresh: HashSet<WindowId> = [2].into_iter().collect();
        // A tab-bar-sized growth of the old tile matches it…
        assert_eq!(
            native_tab_target(
                &candidates,
                &fresh,
                2,
                42,
                Rect {
                    height: 141.0,
                    ..old
                },
                Some(2)
            ),
            Some(1)
        );
        // …but a change past half its height doesn't (and would also be
        // rejected for the neighbor, so the ambiguity guard never fires).
        assert_eq!(
            native_tab_target(
                &candidates,
                &fresh,
                2,
                42,
                Rect {
                    height: 161.0,
                    ..old
                },
                Some(2)
            ),
            None
        );
    }

    #[test]
    fn native_tab_target_ignores_a_still_enumerable_sibling_without_tab_group() {
        let frame = Rect {
            x: 0.0,
            y: 0.0,
            width: 800.0,
            height: 600.0,
        };
        let candidates = [NativeTabCandidate {
            id: 1,
            pid: 42,
            frame,
            positionable: true,
            pending_removal: false,
            was_tab_group: false,
        }];
        let fresh: HashSet<WindowId> = [1, 2].into_iter().collect();
        assert_eq!(
            native_tab_target(&candidates, &fresh, 2, 42, frame, None),
            None
        );
        assert_eq!(
            native_tab_target(&candidates, &fresh, 2, 42, frame, Some(2)),
            Some(1)
        );
    }

    #[test]
    fn native_tab_target_rejects_ambiguous_matches() {
        let frame = Rect {
            x: 0.0,
            y: 0.0,
            width: 800.0,
            height: 600.0,
        };
        let candidates = [
            NativeTabCandidate {
                id: 1,
                pid: 42,
                frame,
                positionable: true,
                pending_removal: false,
                was_tab_group: false,
            },
            NativeTabCandidate {
                id: 3,
                pid: 42,
                frame,
                positionable: true,
                pending_removal: false,
                was_tab_group: false,
            },
        ];
        let fresh: HashSet<WindowId> = [2].into_iter().collect();
        assert_eq!(
            native_tab_target(&candidates, &fresh, 2, 42, frame, None),
            None
        );
    }

    #[test]
    fn native_tab_target_skips_special_and_pending_windows() {
        let frame = Rect {
            x: 0.0,
            y: 0.0,
            width: 800.0,
            height: 600.0,
        };
        let fresh: HashSet<WindowId> = [2].into_iter().collect();
        for candidate in [
            NativeTabCandidate {
                id: 1,
                pid: 42,
                frame,
                positionable: false,
                pending_removal: false,
                was_tab_group: false,
            },
            NativeTabCandidate {
                id: 1,
                pid: 42,
                frame,
                positionable: true,
                pending_removal: true,
                was_tab_group: false,
            },
        ] {
            assert_eq!(
                native_tab_target(&[candidate], &fresh, 2, 42, frame, None),
                None
            );
        }
    }

    #[test]
    fn entry_start_rect_offsets_one_slot_in_the_containers_growth_direction() {
        let target = Rect {
            x: 100.0,
            y: 50.0,
            width: 400.0,
            height: 300.0,
        };
        assert_eq!(
            entry_start_rect(target, Orientation::Horizontal, 8.0),
            Rect {
                x: 508.0,
                y: 50.0,
                width: 400.0,
                height: 300.0,
            }
        );
        assert_eq!(
            entry_start_rect(target, Orientation::Vertical, 8.0),
            Rect {
                x: 100.0,
                y: 358.0,
                width: 400.0,
                height: 300.0,
            }
        );
    }

    #[test]
    fn tab_bar_corrections_snap_only_for_tab_bar_shaped_changes() {
        let tile = Rect {
            x: 80.0,
            y: 45.0,
            width: 1120.0,
            height: 699.0,
        };
        // Opening a tab: the app grew the window before tili saw it.
        assert!(is_tab_bar_correction(
            Rect {
                width: 1118.0,
                height: 741.0,
                ..tile
            },
            tile
        ));
        // Closing a tab can move the survivor again, in the other direction.
        assert!(is_tab_bar_correction(
            Rect {
                width: 1118.0,
                height: 693.0,
                ..tile
            },
            tile
        ));
        // A real layout change (the tile itself was resized) is not.
        assert!(!is_tab_bar_correction(
            Rect {
                width: 620.0,
                height: 751.0,
                ..tile
            },
            tile
        ));
        // Already exact — nothing to correct.
        assert!(!is_tab_bar_correction(tile, tile));
    }

    #[test]
    fn cascade_offset_is_symmetric_around_dead_center_and_wraps() {
        let step = FLOATING_CASCADE_STEP;
        assert_eq!(cascade_offset(0), (0.0, 0.0));
        assert_eq!(cascade_offset(1), (step, step));
        assert_eq!(cascade_offset(2), (-step, -step));
        assert_eq!(cascade_offset(3), (2.0 * step, 2.0 * step));
        assert_eq!(cascade_offset(4), (-2.0 * step, -2.0 * step));
        assert_eq!(cascade_offset(7), (4.0 * step, 4.0 * step));
        // Wraps back to dead center every FLOATING_CASCADE_CYCLE placements,
        // so the offset never grows without bound.
        assert_eq!(cascade_offset(FLOATING_CASCADE_CYCLE), (0.0, 0.0));
        assert_eq!(
            cascade_offset(FLOATING_CASCADE_CYCLE + 1),
            cascade_offset(1)
        );
    }

    #[test]
    fn floating_extent_prefers_absolute_over_ratio_within_one_level() {
        // Both a rule-level absolute and ratio: absolute wins.
        assert_eq!(
            resolve_floating_extent(Some(900), Some(0.5), None, None, 800.0, 1000.0),
            900.0
        );
        // Both defaults-level: absolute still wins.
        assert_eq!(
            resolve_floating_extent(None, None, Some(720), Some(0.5), 800.0, 1000.0),
            720.0
        );
    }

    #[test]
    fn floating_extent_falls_back_through_rule_then_defaults_then_builtin() {
        // Rule ratio beats a defaults absolute: the rule is more specific.
        assert_eq!(
            resolve_floating_extent(None, Some(0.5), Some(720), None, 800.0, 1000.0),
            500.0
        );
        // No rule sizing falls through to the defaults absolute.
        assert_eq!(
            resolve_floating_extent(None, None, Some(720), Some(0.5), 800.0, 1000.0),
            720.0
        );
        // A ratio-only defaults block beats the built-in absolute.
        assert_eq!(
            resolve_floating_extent(None, None, None, Some(0.5), 800.0, 1000.0),
            500.0
        );
        // Nothing configured anywhere: built-in absolute, not a ratio.
        assert_eq!(
            resolve_floating_extent(None, None, None, None, 800.0, 1000.0),
            800.0
        );
    }

    #[test]
    fn floating_extent_keeps_width_and_height_independent() {
        let width = resolve_floating_extent(Some(640), None, None, None, 800.0, 1000.0);
        let height = resolve_floating_extent(None, None, None, None, 600.0, 1000.0);
        assert_eq!((width, height), (640.0, 600.0));
    }

    #[test]
    fn floating_cascade_index_only_counts_other_currently_centered_windows() {
        let mut state = floating_test_state();
        state.placements.insert(
            1,
            Placement {
                workspace: DEFAULT_WORKSPACE.to_string(),
                kind: PlacementKind::Floating { manual: None },
            },
        );
        state.placements.insert(
            2,
            Placement {
                workspace: DEFAULT_WORKSPACE.to_string(),
                kind: PlacementKind::Floating { manual: None },
            },
        );

        // Nothing else centered yet: reopening window 1 alone always stays
        // dead-center, no matter how many times this is called — the fix
        // for repeatedly opening/closing a single floating window drifting
        // off-center.
        assert_eq!(state.floating_cascade_index(DEFAULT_WORKSPACE, 1), 0);
        assert_eq!(state.floating_cascade_index(DEFAULT_WORKSPACE, 1), 0);

        state.floating_centered.insert(2);
        assert_eq!(state.floating_cascade_index(DEFAULT_WORKSPACE, 1), 1);
        // Excludes itself even if already marked centered.
        state.floating_centered.insert(1);
        assert_eq!(state.floating_cascade_index(DEFAULT_WORKSPACE, 1), 1);
    }

    #[test]
    fn floating_cascade_index_ignores_a_window_still_pending_removal() {
        let mut state = floating_test_state();
        state.placements.insert(
            1,
            Placement {
                workspace: DEFAULT_WORKSPACE.to_string(),
                kind: PlacementKind::Floating { manual: None },
            },
        );
        state.placements.insert(
            2,
            Placement {
                workspace: DEFAULT_WORKSPACE.to_string(),
                kind: PlacementKind::Floating { manual: None },
            },
        );
        state.floating_centered.insert(1);

        // Window 2 opens while window 1 is still genuinely on screen: it's
        // the "another centered window" case, so it should cascade.
        assert_eq!(state.floating_cascade_index(DEFAULT_WORKSPACE, 2), 1);

        // Window 1 closes — it disappears from the AX scan immediately, but
        // `remove_placement` (and thus clearing `floating_centered`) is
        // deferred until `removal_grace` elapses, so it sits in
        // `pending_removal` in the meantime. A window closed and instantly
        // reopened (window 2, alone) must not see window 1's ghost as
        // "another centered window" and cascade away from dead center.
        state.pending_removal.insert(1, Instant::now());
        assert_eq!(state.floating_cascade_index(DEFAULT_WORKSPACE, 2), 0);
    }

    #[test]
    fn capture_resize_snapshot_is_none_with_fewer_than_two_tiled_windows() {
        let mut state = floating_test_state();
        assert!(state.capture_resize_snapshot().is_none());

        let root_orientation = state.root_orientation_hint();
        let tree = state.workspaces.get_mut(DEFAULT_WORKSPACE).unwrap();
        tree.insert_window(1, None, root_orientation);
        assert!(
            state.capture_resize_snapshot().is_none(),
            "a lone tiled window has no sibling to resize against"
        );
    }

    #[test]
    fn capture_resize_snapshot_returns_the_current_tiled_layout() {
        let mut state = floating_test_state();
        let root_orientation = state.root_orientation_hint();
        let tree = state.workspaces.get_mut(DEFAULT_WORKSPACE).unwrap();
        let a = tree.insert_window(1, None, root_orientation);
        tree.insert_window(2, Some(a), root_orientation);

        let drag = state
            .capture_resize_snapshot()
            .expect("two tiled windows share a resizable border");
        assert_eq!(drag.monitor_id, 1);
        assert_eq!(drag.frames.len(), 2);

        let expected = state
            .workspaces
            .get(DEFAULT_WORKSPACE)
            .unwrap()
            .layout(state.monitor_frame(1).unwrap(), state.gaps);
        for (id, rect) in expected {
            assert!(frames_match(drag.frames[&id], rect));
        }
    }

    #[test]
    fn tiled_layout_inputs_adds_monitor_notch_to_top_gap_unless_ignored() {
        let mut state = floating_test_state();
        state.monitors[0].notch = 30.0;
        state.gaps.outer.0 = 8.0;

        let (_, _, gaps) = state.tiled_layout_inputs(1).unwrap();
        assert_eq!(gaps.outer.0, 38.0);

        state.ignore_notch = true;
        let (_, _, gaps) = state.tiled_layout_inputs(1).unwrap();
        assert_eq!(gaps.outer.0, 8.0);
    }

    #[test]
    fn tiled_layout_inputs_uses_per_workspace_ignore_notch_override() {
        let mut state = floating_test_state();
        state.monitors[0].notch = 30.0;
        state.gaps.outer.0 = 8.0;
        state.ignore_notch = false;
        state
            .workspace_ignore_notch
            .insert(DEFAULT_WORKSPACE.to_string(), true);

        let (_, _, gaps) = state.tiled_layout_inputs(1).unwrap();
        assert_eq!(gaps.outer.0, 8.0);
    }

    #[test]
    fn capture_resize_snapshot_is_none_during_tiled_fullscreen() {
        let mut state = floating_test_state();
        let root_orientation = state.root_orientation_hint();
        let tree = state.workspaces.get_mut(DEFAULT_WORKSPACE).unwrap();
        let a = tree.insert_window(1, None, root_orientation);
        tree.insert_window(2, Some(a), root_orientation);
        state
            .fullscreen_focus
            .insert(DEFAULT_WORKSPACE.to_string(), a);

        assert!(
            state.capture_resize_snapshot().is_none(),
            "only one window is actually on screen during tiled fullscreen"
        );
    }

    fn two_window_tree() -> (Tree, NodeId, Rect, Gaps) {
        let mut tree = Tree::new();
        let a = tree.insert_window(1, None, tili_tree::Orientation::Horizontal);
        tree.insert_window(2, Some(a), tili_tree::Orientation::Horizontal);
        let area = Rect {
            x: 0.0,
            y: 0.0,
            width: 1000.0,
            height: 800.0,
        };
        (tree, a, area, Gaps::default())
    }

    #[test]
    fn magnet_resize_edge_snaps_growth_to_the_nearest_step_multiple() {
        let (mut tree, _a, area, gaps) = two_window_tree();

        let before = tree.layout(area, gaps);
        let old_rect = before.iter().find(|(w, _)| *w == 1).unwrap().1;
        let mut new_rect = old_rect;
        new_rect.width += 80.0; // dragged the right edge 80px further right

        magnet_resize_edge(
            &mut tree,
            area,
            gaps,
            0.1,
            old_rect,
            new_rect,
            ResizeEdge::Right,
        );

        // 80px maps to a 0.16 weight delta at this container's weight-per-pixel
        // (2 total weight / 1000 divisible px) — rounds to 2 whole 0.1 steps
        // (0.2), landing window 1 at 600px, not the raw 580px the pixel delta
        // alone would have produced.
        let after = tree.layout(area, gaps);
        let width1 = after.iter().find(|(w, _)| *w == 1).unwrap().1.width;
        assert!((width1 - 600.0).abs() < 0.01);
    }

    #[test]
    fn magnet_resize_edge_below_half_a_step_is_a_no_op() {
        let (mut tree, _a, area, gaps) = two_window_tree();

        let before = tree.layout(area, gaps);
        let old_rect = before.iter().find(|(w, _)| *w == 1).unwrap().1;
        let mut new_rect = old_rect;
        new_rect.width += 20.0; // 0.04 weight delta — rounds to 0 steps at a 0.1 grid

        magnet_resize_edge(
            &mut tree,
            area,
            gaps,
            0.1,
            old_rect,
            new_rect,
            ResizeEdge::Right,
        );

        let after = tree.layout(area, gaps);
        let width1 = after.iter().find(|(w, _)| *w == 1).unwrap().1.width;
        assert!(
            (width1 - old_rect.width).abs() < 0.01,
            "a sub-step drag snaps fully back, never lands off-grid"
        );
    }

    #[test]
    fn magnet_resize_edge_on_a_lone_window_is_a_no_op() {
        let mut tree = Tree::new();
        tree.insert_window(1, None, tili_tree::Orientation::Horizontal);
        let area = Rect {
            x: 0.0,
            y: 0.0,
            width: 1000.0,
            height: 800.0,
        };
        let gaps = Gaps::default();

        let before = tree.layout(area, gaps);
        let old_rect = before.iter().find(|(w, _)| *w == 1).unwrap().1;
        let mut new_rect = old_rect;
        new_rect.width -= 200.0; // dragged the (only) window's edge natively

        magnet_resize_edge(
            &mut tree,
            area,
            gaps,
            0.1,
            old_rect,
            new_rect,
            ResizeEdge::Right,
        );

        let after = tree.layout(area, gaps);
        assert_eq!(
            after, before,
            "no sibling to resize against — tree stays untouched"
        );
    }

    #[test]
    fn magnet_resize_edge_overflows_straight_to_the_bound_for_a_huge_drag() {
        let (mut tree, a, area, gaps) = two_window_tree();

        // Skew heavily up front so there's only a small amount of valid room
        // left to grow window 1 any further — the same clamp
        // `resize_weight` itself already enforces.
        assert!(tree.resize_weight(a, 0.7));
        let (_, max_grow) = tree.resize_delta_bounds(a).unwrap();
        assert!(max_grow > 0.0 && max_grow < 0.3);

        let before = tree.layout(area, gaps);
        let old_rect = *before
            .iter()
            .find(|(w, _)| *w == 1)
            .map(|(_, r)| r)
            .unwrap();
        let mut new_rect = old_rect;
        new_rect.width += 5000.0; // a drag far beyond anything valid

        magnet_resize_edge(
            &mut tree,
            area,
            gaps,
            0.1,
            old_rect,
            new_rect,
            ResizeEdge::Right,
        );

        let after = tree.layout(area, gaps);
        let new_width = after.iter().find(|(w, _)| *w == 1).unwrap().1.width;
        let weight_per_pixel = 2.0 / 1000.0; // total weight / divisible px — unchanged by this resize
        let applied_weight_delta = (new_width - old_rect.width) * weight_per_pixel;

        // Same as spamming the keyboard shortcut past its limit: it lands exactly on the
        // boundary `resize_delta_bounds` reported, not rounded down to some smaller whole
        // step (and not refused just because that boundary isn't a step multiple itself).
        assert!(
            (applied_weight_delta - f64::from(max_grow)).abs() < 0.01,
            "a drag past the limit overflows straight to the exact valid bound"
        );
    }

    #[test]
    fn magnet_resize_edge_overflow_still_applies_when_less_than_one_step_of_room_remains() {
        let (mut tree, a, area, gaps) = two_window_tree();

        // Skew almost all the way to the limit, so under one whole 0.1 step of room is left.
        assert!(tree.resize_weight(a, 0.93));
        let (_, max_grow) = tree.resize_delta_bounds(a).unwrap();
        assert!(
            max_grow > 0.0 && max_grow < 0.1,
            "less than one 0.1 step of room left"
        );

        let before = tree.layout(area, gaps);
        let old_rect = *before
            .iter()
            .find(|(w, _)| *w == 1)
            .map(|(_, r)| r)
            .unwrap();
        let mut new_rect = old_rect;
        new_rect.width += 5000.0; // a drag far beyond anything valid

        magnet_resize_edge(
            &mut tree,
            area,
            gaps,
            0.1,
            old_rect,
            new_rect,
            ResizeEdge::Right,
        );

        let after = tree.layout(area, gaps);
        let new_width = after.iter().find(|(w, _)| *w == 1).unwrap().1.width;
        assert!(
            new_width > old_rect.width,
            "still applies the small remaining amount rather than refusing outright \
             because no whole step fits — matches spamming the keyboard shortcut"
        );
    }

    #[test]
    fn maybe_capture_manual_geometry_ignores_an_unchanged_frame() {
        let mut state = floating_test_state();
        let id: WindowId = 1;
        state.placements.insert(
            id,
            Placement {
                workspace: DEFAULT_WORKSPACE.to_string(),
                kind: PlacementKind::Floating { manual: None },
            },
        );

        let frame = Rect {
            x: 100.0,
            y: 100.0,
            width: 400.0,
            height: 300.0,
        };
        state.maybe_capture_manual_geometry(id, frame, frame);

        assert!(matches!(
            state.placements.get(&id).map(|p| &p.kind),
            Some(PlacementKind::Floating { manual: None })
        ));
    }

    #[test]
    fn maybe_capture_manual_geometry_captures_on_drift() {
        let mut state = floating_test_state();
        let id: WindowId = 1;
        state.placements.insert(
            id,
            Placement {
                workspace: DEFAULT_WORKSPACE.to_string(),
                kind: PlacementKind::Floating { manual: None },
            },
        );

        let old_frame = Rect {
            x: 100.0,
            y: 100.0,
            width: 400.0,
            height: 300.0,
        };
        let dragged_frame = Rect {
            x: 500.0,
            y: 500.0,
            width: 400.0,
            height: 300.0,
        };
        state.maybe_capture_manual_geometry(id, old_frame, dragged_frame);

        assert!(matches!(
            state.placements.get(&id).map(|p| &p.kind),
            Some(PlacementKind::Floating { manual: Some(_) })
        ));
    }

    #[test]
    fn maybe_capture_manual_geometry_is_idempotent_once_captured() {
        let mut state = floating_test_state();
        let id: WindowId = 1;
        let captured = FloatGeometry {
            rel_x: 0.1,
            rel_y: 0.1,
            rel_w: 0.2,
            rel_h: 0.2,
        };
        state.placements.insert(
            id,
            Placement {
                workspace: DEFAULT_WORKSPACE.to_string(),
                kind: PlacementKind::Floating {
                    manual: Some(captured),
                },
            },
        );

        let old_frame = Rect {
            x: 100.0,
            y: 100.0,
            width: 400.0,
            height: 300.0,
        };
        let dragged_frame = Rect {
            x: 999.0,
            y: 999.0,
            width: 400.0,
            height: 300.0,
        };
        state.maybe_capture_manual_geometry(id, old_frame, dragged_frame);

        assert!(matches!(
            state.placements.get(&id).map(|p| &p.kind),
            Some(PlacementKind::Floating { manual: Some(g) }) if *g == captured
        ));
    }

    #[test]
    fn parking_uses_the_global_southeast_corner_not_the_workspaces_display() {
        let mut state = floating_test_state();
        state.monitors = two_monitor_fixture();
        state.workspace_monitor.insert("secondary".to_string(), 1);
        state.placements.insert(
            1,
            Placement {
                workspace: "secondary".to_string(),
                kind: PlacementKind::Tiled,
            },
        );

        // Even though the workspace last lived on display 1, every parked
        // window targets the southeast-most display (2).
        assert_eq!(
            tili_ax::southeast_parking_position(&state.monitors),
            (3839.0, 1079.0)
        );
    }

    #[test]
    fn active_workspace_fallback_never_points_at_a_removed_main_workspace() {
        let mut state = floating_test_state();
        state.workspaces.clear();
        state.workspaces.insert("work".to_string(), Tree::new());
        state.active_workspace.clear();

        assert_eq!(state.active_workspace_name(), "work");
        assert!(state.active_tree().is_empty());
    }

    #[test]
    fn ensure_monitor_workspaces_fills_a_fresh_secondary_with_a_declared_workspace() {
        let mut state = floating_test_state();
        state.monitors = two_monitor_fixture();
        state.workspaces.clear();
        state.workspaces.insert("work".to_string(), Tree::new());
        state.workspaces.insert("side".to_string(), Tree::new());
        state.active_workspace.clear();
        state.active_workspace.insert(1, "work".to_string());
        state.focused_monitor = 1;

        state.ensure_monitor_workspaces();

        assert_eq!(
            state.active_workspace.get(&1).map(String::as_str),
            Some("work")
        );
        assert_eq!(
            state.active_workspace.get(&2).map(String::as_str),
            Some("side")
        );
        assert!(!state.synthetic_workspaces.contains("side"));
    }

    #[test]
    fn ensure_monitor_workspaces_prefers_a_pinned_workspace() {
        let mut state = floating_test_state();
        state.monitors = two_monitor_fixture();
        state.workspaces.clear();
        for name in ["work", "pinned", "free"] {
            state.workspaces.insert(name.to_string(), Tree::new());
        }
        state.active_workspace.clear();
        state.active_workspace.insert(1, "work".to_string());
        state.focused_monitor = 1;
        state
            .workspace_pins
            .insert("pinned".to_string(), "2".to_string());

        state.ensure_monitor_workspaces();

        assert_eq!(
            state.active_workspace.get(&2).map(String::as_str),
            Some("pinned")
        );
    }

    #[test]
    fn a_workspace_pinned_to_a_disconnected_display_is_not_stolen() {
        let mut state = floating_test_state();
        state.monitors = two_monitor_fixture();
        state.workspaces.clear();
        state.workspaces.insert("work".to_string(), Tree::new());
        state.workspaces.insert("reserved".to_string(), Tree::new());
        state.active_workspace.clear();
        state.active_workspace.insert(1, "work".to_string());
        state.focused_monitor = 1;
        state
            .workspace_pins
            .insert("reserved".to_string(), "99".to_string());

        state.ensure_monitor_workspaces();

        assert_eq!(
            state.active_workspace.get(&2).map(String::as_str),
            Some("display-2")
        );
        assert!(
            !state
                .active_workspace
                .values()
                .any(|name| name == "reserved")
        );
    }

    #[test]
    fn ensure_monitor_workspaces_creates_a_synthetic_only_when_no_declared_is_free() {
        let mut state = floating_test_state();
        state.monitors = two_monitor_fixture();
        state.workspaces.clear();
        state.workspaces.insert("only".to_string(), Tree::new());
        state.active_workspace.clear();
        state.active_workspace.insert(1, "only".to_string());
        state.focused_monitor = 1;

        state.ensure_monitor_workspaces();

        assert_eq!(
            state.active_workspace.get(&2).map(String::as_str),
            Some("display-2")
        );
        assert!(state.synthetic_workspaces.contains("display-2"));
        let info = state
            .list_workspaces()
            .into_iter()
            .find(|workspace| workspace.name == "display-2")
            .expect("synthetic workspace should be listed");
        assert!(info.synthetic);
        assert_eq!(info.display_name.as_deref(), Some("Display 2"));
    }

    #[test]
    fn an_empty_synthetic_workspace_is_removed_when_its_display_disconnects() {
        let mut state = floating_test_state();
        state.monitors = two_monitor_fixture();
        state
            .workspaces
            .insert("display-2".to_string(), Tree::new());
        state.synthetic_workspaces.insert("display-2".to_string());
        state.active_workspace.insert(2, "display-2".to_string());

        state.on_displays_changed_with(vec![two_monitor_fixture()[0].clone()]);

        assert!(!state.workspaces.contains_key("display-2"));
        assert!(!state.synthetic_workspaces.contains("display-2"));
        assert!(!state.active_workspace.contains_key(&2));
    }

    #[test]
    #[allow(clippy::field_reassign_with_default)]
    fn focus_monitor_target_focuses_a_specific_display() {
        let mut state = WmState::default();
        state.monitors = two_monitor_fixture();
        state.focused_monitor = 1;

        state
            .focus_monitor_target(tili_ipc::MonitorTarget::Id(2))
            .unwrap();
        assert_eq!(state.focused_monitor, 2);

        state
            .focus_monitor_target(tili_ipc::MonitorTarget::Main)
            .unwrap();
        assert_eq!(state.focused_monitor, 1);
        assert!(
            state
                .focus_monitor_target(tili_ipc::MonitorTarget::Id(99))
                .is_err()
        );
    }

    #[test]
    fn display_rename_remaps_workspace_monitor_without_a_park_round_trip() {
        let mut state = floating_test_state();
        state.monitors = two_monitor_fixture();
        state.workspaces.insert("side".to_string(), Tree::new());
        state.workspace_monitor.insert("side".to_string(), 2);
        state.active_workspace.insert(2, "side".to_string());

        let mut renamed = two_monitor_fixture();
        renamed[1].id = 3;
        state.on_displays_changed_with(renamed);

        assert_eq!(state.workspace_monitor.get("side"), Some(&3));
        assert_eq!(state.active_workspace.get(&2), None);
        assert_eq!(
            state.active_workspace.get(&3).map(String::as_str),
            Some("side")
        );
    }

    #[test]
    fn reconnecting_the_same_display_restores_its_last_declared_workspace() {
        let mut state = floating_test_state();
        state.monitors = two_monitor_fixture();
        state.workspaces.insert("side".to_string(), Tree::new());
        state.active_workspace.insert(2, "side".to_string());
        state.workspace_monitor.insert("side".to_string(), 2);
        state.last_active_workspace.insert(2, "side".to_string());

        state.on_displays_changed_with(vec![two_monitor_fixture()[0].clone()]);
        assert!(!state.active_workspace.contains_key(&2));
        assert_eq!(
            state.last_active_workspace.get(&2).map(String::as_str),
            Some("side")
        );

        state.on_displays_changed_with(two_monitor_fixture());
        assert_eq!(
            state.active_workspace.get(&2).map(String::as_str),
            Some("side")
        );
    }

    #[test]
    fn disconnecting_a_display_keeps_its_workspace_parked_and_ready_to_fall_back() {
        let mut state = floating_test_state();
        state.monitors = two_monitor_fixture();
        state.workspaces.insert("side".to_string(), Tree::new());
        state.workspace_monitor.insert("side".to_string(), 2);
        state.active_workspace.insert(2, "side".to_string());

        state.on_displays_changed_with(vec![two_monitor_fixture()[0].clone()]);

        // The workspace still remembers display 2; parking is now global, so
        // pulling the southeast display moves every parked window's corner
        // back to the remaining (main) display.
        assert_eq!(state.workspace_monitor.get("side"), Some(&2));
        assert_eq!(
            tili_ax::southeast_parking_position(&state.monitors),
            (1919.0, 1079.0)
        );
    }

    #[test]
    fn disconnecting_the_southeast_display_reparks_at_the_new_corner() {
        let mut state = floating_test_state();
        state.monitors = two_monitor_fixture();
        // Two parked windows: repark must survive the southeast display
        // disappearing. No live `AxWindow` here, so `repark_parked_windows`
        // is a write-free no-op — this proves the path runs without
        // panicking and still reports both as parked.
        state.placements.insert(
            1,
            Placement {
                workspace: "parked".to_string(),
                kind: PlacementKind::Tiled,
            },
        );
        state.placements.insert(
            2,
            Placement {
                workspace: "parked".to_string(),
                kind: PlacementKind::Floating { manual: None },
            },
        );
        assert_eq!(
            tili_ax::southeast_parking_position(&state.monitors),
            (3839.0, 1079.0)
        );

        state.on_displays_changed_with(vec![two_monitor_fixture()[0].clone()]);

        assert_eq!(
            tili_ax::southeast_parking_position(&state.monitors),
            (1919.0, 1079.0)
        );
        let mut ids = state.parked_positionable_ids();
        ids.sort_unstable();
        assert_eq!(ids, vec![1, 2]);
    }

    #[test]
    fn an_empty_display_scan_is_treated_as_sleep_and_keeps_the_old_snapshot() {
        let mut state = floating_test_state();
        state.monitors = two_monitor_fixture();
        let before = state.monitors.clone();

        state.on_displays_changed_with(Vec::new());

        assert_eq!(state.monitors, before);
        assert_eq!(state.focused_monitor, 1);
    }

    #[test]
    fn parked_positionable_ids_includes_only_parked_tiled_and_floating() {
        let mut state = floating_test_state();

        // Tiled, in a workspace that's parked (not in active_workspace).
        state.placements.insert(
            1,
            Placement {
                workspace: "parked".to_string(),
                kind: PlacementKind::Tiled,
            },
        );
        // Floating, also parked.
        state.placements.insert(
            2,
            Placement {
                workspace: "parked".to_string(),
                kind: PlacementKind::Floating { manual: None },
            },
        );
        // Tiled, but in the currently-active workspace — not parked.
        state.placements.insert(
            3,
            Placement {
                workspace: DEFAULT_WORKSPACE.to_string(),
                kind: PlacementKind::Tiled,
            },
        );
        // Minimized in a parked workspace — never "parked" in the park()
        // sense, since it was never moved off-screen to begin with.
        state.placements.insert(
            4,
            Placement {
                workspace: "parked".to_string(),
                kind: PlacementKind::Minimized(Restore::Tiled),
            },
        );

        let mut ids = state.parked_positionable_ids();
        ids.sort_unstable();
        assert_eq!(ids, vec![1, 2]);
    }

    #[test]
    fn reconcile_existing_placement_reparks_an_already_parked_window() {
        // Regression test: every parked window targets the same coordinate
        // now (see `park`'s doc comment), so `reconcile_existing_placement`'s
        // re-assertion can call `park(id)` with no offset to remember or
        // reproduce — this just confirms the call succeeds without a real
        // `AxWindow` present (park's own AX write is best-effort/no-op'd).
        let mut state = floating_test_state();
        state.placements.insert(
            1,
            Placement {
                workspace: "parked".to_string(),
                kind: PlacementKind::Floating { manual: None },
            },
        );
        state.park(1);

        state.reconcile_existing_placement(1, false, false, false);

        assert!(state.parked_positionable_ids().contains(&1));
    }

    #[test]
    fn unpark_all_does_not_panic_with_no_real_windows() {
        let mut state = floating_test_state();
        state.placements.insert(
            1,
            Placement {
                workspace: "parked".to_string(),
                kind: PlacementKind::Tiled,
            },
        );
        state.unpark_all();
    }

    #[test]
    fn finalize_expired_removals_drops_entries_past_the_grace_period() {
        let mut state = WmState {
            removal_grace: Duration::ZERO,
            ..WmState::default()
        };
        let id: WindowId = 1;
        state.placements.insert(
            id,
            Placement {
                workspace: DEFAULT_WORKSPACE.to_string(),
                kind: PlacementKind::Tiled,
            },
        );
        state.pending_removal.insert(id, Instant::now());

        state.finalize_expired_removals();

        assert!(!state.placements.contains_key(&id));
        assert!(!state.pending_removal.contains_key(&id));
    }

    #[test]
    fn finalize_expired_removals_keeps_entries_still_within_the_grace_period() {
        let mut state = WmState {
            removal_grace: Duration::from_secs(3600),
            ..WmState::default()
        };
        let id: WindowId = 1;
        state.placements.insert(
            id,
            Placement {
                workspace: DEFAULT_WORKSPACE.to_string(),
                kind: PlacementKind::Tiled,
            },
        );
        state.pending_removal.insert(id, Instant::now());

        state.finalize_expired_removals();

        assert!(state.placements.contains_key(&id));
        assert!(state.pending_removal.contains_key(&id));
    }

    #[tokio::test]
    async fn note_system_wake_is_a_no_op_with_no_tracked_windows() {
        // `self.windows` can't hold a real `AxWindow` at this level (see
        // `place_new_window`'s doc comment on why it, not
        // `apply_windows_changed`, is this module's unit-testable seam) —
        // so the probe loop always finds zero windows here. Still worth
        // covering directly: this proves `note_system_wake` doesn't panic
        // or leave stray state (`unconfirmed_pids`) behind when there's
        // nothing to probe, the shape every test below relies on for its
        // own `WmState::default()` starting point. `wake_lock_active` is
        // armed regardless of whether there was anything to probe — see
        // `note_system_wake`'s doc comment on why that's synchronous and
        // unconditional.
        let mut state = WmState::default();

        state.note_system_wake().await;

        assert!(state.unconfirmed_pids.is_empty());
        assert!(state.wake_lock_active);
    }

    #[tokio::test]
    async fn note_system_wake_restarts_pending_removal_grace_clocks() {
        // Mirrors `note_screen_unlocked_restarts_pending_removal_grace_clocks`
        // below: a real sleep of several hours advances `Instant` just as
        // much as a lock sitting frozen does (macOS keeps it running across
        // suspend), so a window that predates the sleep can have a
        // `pending_removal` timestamp far past `removal_grace` the instant
        // this probe confirms its pid. Without this reset,
        // `finalize_expired_removals` would treat that stale clock as proof
        // enough to finalize before `tili-ax`'s forced post-wake resync has
        // actually re-verified the window.
        let mut state = WmState {
            removal_grace: Duration::from_secs(3600),
            ..WmState::default()
        };
        let id: WindowId = 1;
        state
            .pending_removal
            .insert(id, Instant::now() - Duration::from_secs(93));

        state.note_system_wake().await;

        let since = *state.pending_removal.get(&id).expect("still pending");
        assert!(
            since.elapsed() < Duration::from_secs(1),
            "waking must restart the entry's grace-period clock, not just leave the pre-sleep one in place"
        );
    }

    #[test]
    fn note_screen_locked_arms_the_wake_lock_and_is_a_no_op_with_no_tracked_windows() {
        // Same limitation as `note_system_wake_is_a_no_op_with_no_tracked_windows`
        // above: `self.windows` can't hold a real `AxWindow` at this level,
        // so there's nothing for the pid-seeding loop to find here. Still
        // worth covering directly: proves `note_screen_locked` arms
        // `wake_lock_active` synchronously and doesn't panic with nothing
        // to seed.
        let mut state = WmState::default();

        state.note_screen_locked();

        assert!(state.wake_lock_active);
        assert!(state.unconfirmed_pids.is_empty());
    }

    #[tokio::test]
    async fn note_screen_unlocked_probes_but_never_touches_the_wake_lock() {
        // Unlike `note_system_wake`/`note_screen_locked`, unlocking the
        // screen isn't itself proof of real user activity (see
        // `clear_wake_lock`'s doc comment) — it must neither arm nor clear
        // `wake_lock_active` on its own, only run the confirmation probe
        // `note_screen_locked` deferred.
        let mut state = WmState {
            wake_lock_active: true,
            ..WmState::default()
        };

        state.note_screen_unlocked().await;

        assert!(
            state.wake_lock_active,
            "unlocking doesn't clear a lock armed by a prior lock"
        );
        assert!(state.unconfirmed_pids.is_empty());

        state.wake_lock_active = false;
        state.note_screen_unlocked().await;

        assert!(
            !state.wake_lock_active,
            "unlocking never arms the lock either, unlike note_system_wake/note_screen_locked"
        );
    }

    #[tokio::test]
    async fn note_screen_unlocked_restarts_pending_removal_grace_clocks() {
        // A window that predates the lock can have a `pending_removal`
        // timestamp far older than `removal_grace` just from sitting frozen
        // for the whole lock duration — real hardware showed a window
        // finalized as closed after 93s "pending" despite being genuinely
        // still open the entire time. Without this reset,
        // `finalize_expired_removals` would treat that stale clock as
        // proof enough to finalize the moment any later tick runs, racing
        // ahead of the real resync `AppEvent::ScreenUnlocked` triggers to
        // actually re-verify the window.
        let mut state = WmState {
            removal_grace: Duration::from_secs(3600),
            ..WmState::default()
        };
        let id: WindowId = 1;
        state
            .pending_removal
            .insert(id, Instant::now() - Duration::from_secs(93));

        state.note_screen_unlocked().await;

        let since = *state.pending_removal.get(&id).expect("still pending");
        assert!(
            since.elapsed() < Duration::from_secs(1),
            "unlocking must restart the entry's grace-period clock, not just leave the pre-lock one in place"
        );
    }

    #[test]
    fn clear_wake_lock_clears_it() {
        let mut state = WmState {
            wake_lock_active: true,
            ..WmState::default()
        };

        state.clear_wake_lock();

        assert!(!state.wake_lock_active);
    }

    #[test]
    fn is_pid_confirmed_reflects_unconfirmed_pids() {
        let mut state = WmState::default();
        assert!(
            state.is_pid_confirmed(1234),
            "a pid never marked unconfirmed is confirmed by default"
        );

        state.unconfirmed_pids.insert(1234);
        assert!(!state.is_pid_confirmed(1234));
        assert!(
            state.is_pid_confirmed(5678),
            "marking one pid unconfirmed doesn't affect another"
        );

        state.unconfirmed_pids.remove(&1234);
        assert!(
            state.is_pid_confirmed(1234),
            "removing it (as note_system_wake's probe or apply_windows_changed does once it responds) restores confirmed"
        );
    }

    #[test]
    fn finalize_expired_removals_defaults_to_confirmed_when_the_window_cant_be_resolved() {
        // `finalize_expired_removals` only blocks on `unconfirmed_pids` for
        // a pending-removal window it can still resolve via `self.windows`
        // (`is_none_or`'s "confirmed" default otherwise) — in production
        // that lookup always succeeds (only this function's own removal
        // below ever drops a `self.windows` entry), but this module has no
        // way to populate `self.windows` with a real `AxWindow` (see
        // `place_new_window`'s doc comment), so the blocking branch itself
        // can't be exercised at this level, only this default. A populated
        // `unconfirmed_pids` (for some unrelated pid) proves finalization
        // isn't accidentally gated on "any pid is unconfirmed" instead.
        let mut state = WmState {
            removal_grace: Duration::ZERO,
            ..WmState::default()
        };
        state.unconfirmed_pids.insert(9999);
        let id: WindowId = 1;
        state.placements.insert(
            id,
            Placement {
                workspace: DEFAULT_WORKSPACE.to_string(),
                kind: PlacementKind::Tiled,
            },
        );
        state.pending_removal.insert(id, Instant::now());

        state.finalize_expired_removals();

        assert!(!state.placements.contains_key(&id));
        assert!(!state.pending_removal.contains_key(&id));
    }

    #[test]
    fn place_new_window_skips_auto_switch_while_the_wake_lock_is_active_and_fires_once_cleared() {
        let mut state = floating_test_state();
        state.workspaces.insert("side".to_string(), Tree::new());
        state.wake_lock_active = true;

        state.place_new_window(1, &PlacementKind::Tiled, "side");

        assert!(
            state
                .workspaces
                .get("side")
                .unwrap()
                .window_ids()
                .contains(&1),
            "the window is still placed into its target workspace, just parked"
        );
        assert_eq!(
            state.active_workspace_name(),
            DEFAULT_WORKSPACE,
            "auto-switch is skipped while the wake lock is active"
        );

        state.clear_wake_lock();
        state.place_new_window(2, &PlacementKind::Tiled, "side");

        assert_eq!(
            state.active_workspace_name(),
            "side",
            "auto-switch fires normally once the wake lock is cleared"
        );
    }

    #[test]
    fn finalize_expired_removals_relayouts_the_survivors() {
        let mut state = WmState {
            removal_grace: Duration::ZERO,
            ..WmState::default()
        };
        let root_orientation = state.root_orientation_hint();
        let tree = state.workspaces.get_mut(DEFAULT_WORKSPACE).unwrap();
        let a = tree.insert_window(1, None, root_orientation);
        tree.insert_window(2, Some(a), root_orientation);
        state.placements.insert(
            1,
            Placement {
                workspace: DEFAULT_WORKSPACE.to_string(),
                kind: PlacementKind::Tiled,
            },
        );
        state.placements.insert(
            2,
            Placement {
                workspace: DEFAULT_WORKSPACE.to_string(),
                kind: PlacementKind::Tiled,
            },
        );
        state.pending_removal.insert(1, Instant::now());

        state.finalize_expired_removals();

        assert!(!state.placements.contains_key(&1));
        assert!(state.placements.contains_key(&2));
        assert!(
            state.relayout_calls.get() > 0,
            "closing a tiled window must trigger a relayout so the survivor fills the freed space"
        );
    }

    #[test]
    fn finalize_expired_removals_skips_relayout_when_nothing_expired() {
        let mut state = WmState {
            removal_grace: Duration::from_secs(3600),
            ..WmState::default()
        };
        state.pending_removal.insert(1, Instant::now());

        state.finalize_expired_removals();

        assert_eq!(state.relayout_calls.get(), 0);
    }

    #[test]
    fn finalize_expired_launches_drops_entries_past_the_grace_period() {
        let mut state = WmState {
            launch_grace: Duration::ZERO,
            ..WmState::default()
        };
        state.pending_launch_pids.insert(1234, Instant::now());

        state.finalize_expired_launches();

        assert!(!state.pending_launch_pids.contains_key(&1234));
    }

    #[test]
    fn finalize_expired_launches_keeps_entries_still_within_the_grace_period() {
        let mut state = WmState {
            launch_grace: Duration::from_secs(3600),
            ..WmState::default()
        };
        state.pending_launch_pids.insert(1234, Instant::now());

        state.finalize_expired_launches();

        assert!(state.pending_launch_pids.contains_key(&1234));
    }

    #[test]
    fn reveal_current_frontmost_skips_while_a_launch_is_pending() {
        let mut state = floating_test_state();
        state.note_app_launched(1234);

        let deferred = state.reveal_current_frontmost(false, None, false);
        assert!(!deferred);

        // The guard must return before ever touching `last_frontmost_pid`
        // (only `reveal_frontmost` sets it) or triggering a relayout —
        // proof the stale-pid AX query never ran.
        assert_eq!(state.last_frontmost_pid, None);
        assert_eq!(state.relayout_calls.get(), 0);
    }

    #[test]
    fn click_started_a_launch_is_true_only_for_a_previously_absent_pid() {
        let at_click = HashSet::from([100, 200]);
        // A Dock-visible app that appeared after mouse-down: the click is
        // launching something, so the frontmost read can't be trusted.
        assert!(WmState::click_started_a_launch(
            &at_click,
            &HashSet::from([100, 200, 300])
        ));
        // Nothing new: an ordinary click — including re-clicking the
        // already-frontmost app's Dock icon — must still reveal.
        assert!(!WmState::click_started_a_launch(
            &at_click,
            &HashSet::from([100, 200])
        ));
        // An app quitting between mouse-down and the deadline is not a
        // launch and must not suppress the reveal.
        assert!(!WmState::click_started_a_launch(
            &at_click,
            &HashSet::from([100])
        ));
    }

    #[test]
    fn is_system_ui_bundle_recognizes_every_listed_entry() {
        for id in SYSTEM_UI_BUNDLE_IDS {
            assert!(is_system_ui_bundle(Some(id)), "expected {id} to match");
        }
        assert!(!is_system_ui_bundle(Some("com.apple.finder")));
        assert!(!is_system_ui_bundle(Some("com.example.SomeApp")));
        assert!(!is_system_ui_bundle(None));
    }

    #[test]
    fn remove_app_clears_pending_launch_pids() {
        let mut state = floating_test_state();
        state.note_app_launched(1234);

        state.remove_app(1234);

        assert!(!state.pending_launch_pids.contains_key(&1234));
    }

    #[test]
    fn note_app_launched_records_the_active_workspace() {
        let mut state = floating_test_state();
        state.note_app_launched(1234);
        assert_eq!(
            state.launch_active_workspace.get(&1234),
            Some(&state.active_workspace_name())
        );
    }

    #[test]
    fn remove_app_clears_launch_active_workspace() {
        let mut state = floating_test_state();
        state.note_app_launched(1234);

        state.remove_app(1234);

        assert!(!state.launch_active_workspace.contains_key(&1234));
    }

    #[test]
    fn toggle_orientation_flips_between_horizontal_and_vertical() {
        let mut state = floating_test_state();
        let root_orientation = state.root_orientation_hint();
        let tree = state.workspaces.get_mut(DEFAULT_WORKSPACE).unwrap();
        let a = tree.insert_window(1, None, root_orientation);
        tree.insert_window(2, Some(a), root_orientation);
        state
            .workspace_focus
            .insert(DEFAULT_WORKSPACE.to_string(), a);

        let before = state.active_tree().orientation_of(a);
        assert!(state.toggle_orientation(false).is_ok());
        let after = state.active_tree().orientation_of(a);
        assert_ne!(before, after);

        assert!(state.toggle_orientation(false).is_ok());
        assert_eq!(state.active_tree().orientation_of(a), before);
    }

    #[test]
    fn toggle_orientation_errors_when_nothing_is_focused() {
        let mut state = WmState::default();
        assert!(state.toggle_orientation(false).is_err());
    }

    #[test]
    fn current_mode_auto_exits_reflects_config() {
        let mut state = WmState::default();
        let config = tili_config::parse(
            r#"
            keybindings mode="main" {
                bind "alt-shift-m" "mode manage"
            }
            keybindings mode="manage" auto-exit=#true {
                bind "escape" "mode main"
            }
            "#,
        )
        .unwrap();
        state.apply_config(&config);

        assert!(!state.current_mode_auto_exits());

        state.enter_mode("manage").unwrap();
        assert!(state.current_mode_auto_exits());

        state.exit_mode();
        assert!(!state.current_mode_auto_exits());
    }

    #[test]
    fn current_mode_reflects_enter_and_exit() {
        let mut state = WmState::default();
        assert_eq!(state.current_mode(), "main");

        let config = tili_config::parse(
            r#"
            keybindings mode="manage" auto-exit=#true {
                bind "escape" "mode main"
            }
            "#,
        )
        .unwrap();
        state.apply_config(&config);

        state.enter_mode("manage").unwrap();
        assert_eq!(state.current_mode(), "manage");

        state.exit_mode();
        assert_eq!(state.current_mode(), "main");
    }

    #[test]
    fn current_mode_auto_exits_defaults_false_when_flag_omitted() {
        let mut state = WmState::default();
        let config = tili_config::parse(
            r#"
            keybindings mode="resize" {
                bind "escape" "mode main"
            }
            "#,
        )
        .unwrap();
        state.apply_config(&config);

        state.enter_mode("resize").unwrap();
        assert!(!state.current_mode_auto_exits());
    }

    #[test]
    fn apply_config_applies_global_default_layout_to_new_workspace() {
        let mut state = WmState::default();
        let config = tili_config::parse(
            r#"
            default-layout "accordion"
            workspaces {
                workspace "work"
            }
            "#,
        )
        .unwrap();
        state.apply_config(&config);

        let root_orientation = state.root_orientation_hint();
        let tree = state.workspaces.get_mut("work").unwrap();
        let first = tree.insert_window(1, None, root_orientation);
        tree.insert_window(2, Some(first), root_orientation);
        assert!(tree.is_root_accordion());
    }

    #[test]
    fn apply_config_workspace_layout_overrides_global_default() {
        let mut state = WmState::default();
        let config = tili_config::parse(
            r#"
            default-layout "accordion"
            workspaces {
                workspace "work" layout="tiles"
            }
            "#,
        )
        .unwrap();
        state.apply_config(&config);

        let root_orientation = state.root_orientation_hint();
        let tree = state.workspaces.get_mut("work").unwrap();
        let first = tree.insert_window(1, None, root_orientation);
        tree.insert_window(2, Some(first), root_orientation);
        assert!(!tree.is_root_accordion());
    }

    #[test]
    fn apply_config_defaults_to_tiles_when_layout_is_unset() {
        let mut state = WmState::default();
        let config = tili_config::parse(
            r#"
            workspaces {
                workspace "work"
            }
            "#,
        )
        .unwrap();
        state.apply_config(&config);

        let root_orientation = state.root_orientation_hint();
        let tree = state.workspaces.get_mut("work").unwrap();
        let first = tree.insert_window(1, None, root_orientation);
        tree.insert_window(2, Some(first), root_orientation);
        assert!(!tree.is_root_accordion());
    }

    #[test]
    fn apply_config_warns_and_falls_back_on_invalid_layout() {
        let mut state = WmState::default();
        let config = tili_config::parse(
            r#"
            default-layout "bogus"
            workspaces {
                workspace "work" layout="also-bogus"
            }
            "#,
        )
        .unwrap();
        state.apply_config(&config);

        let warnings = state.config_warnings();
        assert!(warnings.iter().any(|w| w.contains("default-layout")));
        assert!(
            warnings
                .iter()
                .any(|w| w.contains("workspace 'work' layout"))
        );

        let root_orientation = state.root_orientation_hint();
        let tree = state.workspaces.get_mut("work").unwrap();
        let first = tree.insert_window(1, None, root_orientation);
        tree.insert_window(2, Some(first), root_orientation);
        assert!(!tree.is_root_accordion());
    }

    #[test]
    fn balance_sizes_resets_skewed_weights() {
        let mut state = floating_test_state();
        let root_orientation = state.root_orientation_hint();
        let tree = state.workspaces.get_mut(DEFAULT_WORKSPACE).unwrap();
        let a = tree.insert_window(1, None, root_orientation);
        tree.insert_window(2, Some(a), root_orientation);
        assert!(tree.resize_weight(a, 0.3));
        state
            .workspace_focus
            .insert(DEFAULT_WORKSPACE.to_string(), a);

        assert!(state.balance_sizes(false).is_ok());

        let tree = state.workspaces.get(DEFAULT_WORKSPACE).unwrap();
        let area = Rect {
            x: 0.0,
            y: 0.0,
            width: 1000.0,
            height: 1000.0,
        };
        let layout = tree.layout(area, Gaps::default());
        let w1 = layout.iter().find(|(id, _)| *id == 1).unwrap().1.width;
        let w2 = layout.iter().find(|(id, _)| *id == 2).unwrap().1.width;
        assert!((w1 - w2).abs() < 0.01);
    }

    #[test]
    fn balance_sizes_errors_when_nothing_is_focused() {
        let mut state = WmState::default();
        assert!(state.balance_sizes(false).is_err());
    }

    #[test]
    fn toggle_fullscreen_tiled_marks_and_clears_fullscreen_focus() {
        let mut state = floating_test_state();
        let root_orientation = state.root_orientation_hint();
        let node = state
            .workspaces
            .get_mut(DEFAULT_WORKSPACE)
            .unwrap()
            .insert_window(1, None, root_orientation);
        state
            .workspace_focus
            .insert(DEFAULT_WORKSPACE.to_string(), node);

        assert!(state.toggle_fullscreen(false).is_ok());
        assert_eq!(state.fullscreen_focus.get(DEFAULT_WORKSPACE), Some(&node));

        assert!(state.toggle_fullscreen(false).is_ok());
        assert!(!state.fullscreen_focus.contains_key(DEFAULT_WORKSPACE));
    }

    #[test]
    fn native_fullscreen_target_uses_real_focus_even_when_it_left_the_tree() {
        // The whole point: a natively-fullscreened window has been demoted
        // out of the tree, so `focused_node()` can never name it.
        let mut state = WmState::default();
        let sibling: WindowId = 1;
        let fullscreened: WindowId = 2;
        let root_orientation = state.root_orientation_hint();
        let tree = state.workspaces.get_mut(DEFAULT_WORKSPACE).unwrap();
        let sibling_node = tree.insert_window(sibling, None, root_orientation);
        state
            .workspace_focus
            .insert(DEFAULT_WORKSPACE.to_string(), sibling_node);
        for (id, kind) in [
            (sibling, PlacementKind::Tiled),
            (
                fullscreened,
                PlacementKind::NativeFullscreen(Restore::Tiled),
            ),
        ] {
            state.placements.insert(
                id,
                Placement {
                    workspace: DEFAULT_WORKSPACE.to_string(),
                    kind,
                },
            );
        }

        assert_eq!(
            state.native_fullscreen_target(Some(fullscreened)),
            Ok(fullscreened),
            "toggling must exit the fullscreen window, not fullscreen its sibling"
        );
    }

    #[test]
    fn native_fullscreen_target_falls_back_to_the_tree_node() {
        let mut state = WmState::default();
        state.workspaces.insert("side".to_string(), Tree::new());
        let focused: WindowId = 1;
        let elsewhere: WindowId = 2;
        let root_orientation = state.root_orientation_hint();
        let tree = state.workspaces.get_mut(DEFAULT_WORKSPACE).unwrap();
        let node = tree.insert_window(focused, None, root_orientation);
        state
            .workspace_focus
            .insert(DEFAULT_WORKSPACE.to_string(), node);
        state.placements.insert(
            elsewhere,
            Placement {
                workspace: "side".to_string(),
                kind: PlacementKind::Tiled,
            },
        );

        assert_eq!(
            state.native_fullscreen_target(None),
            Ok(focused),
            "nothing focused system-wide"
        );
        assert_eq!(
            state.native_fullscreen_target(Some(99)),
            Ok(focused),
            "a window tili doesn't track"
        );
        assert_eq!(
            state.native_fullscreen_target(Some(elsewhere)),
            Ok(focused),
            "real focus is on another workspace's window"
        );
    }

    #[test]
    fn native_fullscreen_target_errors_with_nothing_to_act_on() {
        let state = WmState::default();
        assert!(state.native_fullscreen_target(None).is_err());
    }

    #[test]
    fn toggle_fullscreen_errors_when_nothing_is_focused() {
        let mut state = WmState::default();
        assert!(state.toggle_fullscreen(false).is_err());
        assert!(state.toggle_fullscreen(true).is_err());
    }

    #[test]
    fn removing_the_fullscreened_window_clears_fullscreen_focus() {
        let mut state = floating_test_state();
        let root_orientation = state.root_orientation_hint();
        let node = state
            .workspaces
            .get_mut(DEFAULT_WORKSPACE)
            .unwrap()
            .insert_window(1, None, root_orientation);
        state
            .workspace_focus
            .insert(DEFAULT_WORKSPACE.to_string(), node);
        state
            .fullscreen_focus
            .insert(DEFAULT_WORKSPACE.to_string(), node);
        state.placements.insert(
            1,
            Placement {
                workspace: DEFAULT_WORKSPACE.to_string(),
                kind: PlacementKind::Tiled,
            },
        );

        state.remove_placement(1, None);

        assert!(!state.fullscreen_focus.contains_key(DEFAULT_WORKSPACE));
    }

    #[test]
    fn close_focused_errors_when_nothing_is_focused() {
        let mut state = WmState::default();
        assert!(state.close_focused().is_err());
    }

    #[test]
    fn summon_errors_when_no_window_matches() {
        let mut state = WmState::default();
        assert!(state.summon("nonexistent").is_err());
    }

    #[test]
    #[allow(clippy::field_reassign_with_default)]
    fn resolve_monitor_target_id_main_and_next() {
        let mut state = WmState::default();
        state.monitors = two_monitor_fixture();
        state.focused_monitor = 1;

        assert_eq!(
            state.resolve_monitor_target(tili_ipc::MonitorTarget::Id(2)),
            Ok(2)
        );
        assert!(
            state
                .resolve_monitor_target(tili_ipc::MonitorTarget::Id(99))
                .is_err()
        );
        assert_eq!(
            state.resolve_monitor_target(tili_ipc::MonitorTarget::Main),
            Ok(1)
        );
        assert_eq!(
            state.resolve_monitor_target(tili_ipc::MonitorTarget::Next),
            Ok(2)
        );
        assert_eq!(
            state.resolve_monitor_target(tili_ipc::MonitorTarget::Prev),
            Ok(2),
            "with two monitors, previous wraps to the same neighbor as next"
        );
    }

    #[test]
    #[allow(clippy::field_reassign_with_default)]
    fn focus_monitor_prev_wraps_backwards() {
        let mut state = WmState::default();
        state.monitors = three_monitor_fixture();
        state.focused_monitor = 1;

        state
            .focus_monitor_target(tili_ipc::MonitorTarget::Prev)
            .unwrap();
        assert_eq!(state.focused_monitor, 3);

        state
            .focus_monitor_target(tili_ipc::MonitorTarget::Next)
            .unwrap();
        assert_eq!(state.focused_monitor, 1);
    }

    #[test]
    #[allow(clippy::field_reassign_with_default)]
    fn move_workspace_to_monitor_swaps_with_whatever_is_there() {
        let mut state = WmState::default();
        state.monitors = two_monitor_fixture();
        state.workspaces.insert("a".to_string(), Tree::new());
        state.workspaces.insert("b".to_string(), Tree::new());
        state.active_workspace.clear();
        state.active_workspace.insert(1, "a".to_string());
        state.active_workspace.insert(2, "b".to_string());
        state.focused_monitor = 1;

        assert!(
            state
                .move_workspace_to_monitor(Some("a"), tili_ipc::MonitorTarget::Id(2))
                .is_ok()
        );

        assert_eq!(state.active_workspace.get(&2), Some(&"a".to_string()));
        assert_eq!(state.active_workspace.get(&1), Some(&"b".to_string()));
    }

    #[test]
    #[allow(clippy::field_reassign_with_default)]
    fn move_workspace_to_monitor_from_parked_only_changes_its_home() {
        let mut state = WmState::default();
        state.monitors = two_monitor_fixture();
        state.workspaces.insert("parked".to_string(), Tree::new());
        state.workspaces.insert("shown".to_string(), Tree::new());
        state.active_workspace.clear();
        state.active_workspace.insert(2, "shown".to_string());
        state.focused_monitor = 1;

        assert!(
            state
                .move_workspace_to_monitor(Some("parked"), tili_ipc::MonitorTarget::Id(2))
                .is_ok()
        );

        assert_eq!(
            state.active_workspace.get(&2),
            Some(&"shown".to_string()),
            "a parked workspace's move must not disturb the target display"
        );
        assert!(!state.active_workspace.values().any(|w| w == "parked"));
        assert_eq!(state.workspace_monitor.get("parked"), Some(&2));
        assert_eq!(state.workspace_home("parked"), Some(2));
    }

    #[test]
    #[allow(clippy::field_reassign_with_default)]
    fn move_workspace_to_monitor_next_and_prev_step_from_the_workspaces_display() {
        let mut state = WmState::default();
        state.monitors = three_monitor_fixture();
        for name in ["a", "b", "c"] {
            state.workspaces.insert(name.to_string(), Tree::new());
        }
        state.active_workspace.clear();
        state.active_workspace.insert(1, "a".to_string());
        state.active_workspace.insert(2, "b".to_string());
        state.active_workspace.insert(3, "c".to_string());
        state.focused_monitor = 1;

        // "a" is on monitor 1; next is monitor 2.
        assert!(
            state
                .move_workspace_to_monitor(Some("a"), tili_ipc::MonitorTarget::Next)
                .is_ok()
        );
        assert_eq!(state.active_workspace.get(&2), Some(&"a".to_string()));
        assert_eq!(state.active_workspace.get(&1), Some(&"b".to_string()));

        // "a" is now on monitor 2; prev is monitor 1.
        assert!(
            state
                .move_workspace_to_monitor(Some("a"), tili_ipc::MonitorTarget::Prev)
                .is_ok()
        );
        assert_eq!(state.active_workspace.get(&1), Some(&"a".to_string()));
        assert_eq!(state.active_workspace.get(&2), Some(&"b".to_string()));
    }

    #[test]
    #[allow(clippy::field_reassign_with_default)]
    fn move_workspace_to_monitor_refuses_a_pinned_workspace_off_its_pin() {
        let mut state = WmState::default();
        state.monitors = two_monitor_fixture();
        state.workspaces.insert("pinned".to_string(), Tree::new());
        state.workspaces.insert("other".to_string(), Tree::new());
        state.active_workspace.clear();
        state.active_workspace.insert(1, "pinned".to_string());
        state.active_workspace.insert(2, "other".to_string());
        state.focused_monitor = 1;
        state
            .workspace_pins
            .insert("pinned".to_string(), "1".to_string());

        assert!(
            state
                .move_workspace_to_monitor(Some("pinned"), tili_ipc::MonitorTarget::Id(2))
                .is_err()
        );
        assert_eq!(state.active_workspace.get(&1), Some(&"pinned".to_string()));
    }

    #[test]
    #[allow(clippy::field_reassign_with_default)]
    fn move_workspace_to_monitor_refuses_a_synthetic_fallback() {
        let mut state = WmState::default();
        state.monitors = two_monitor_fixture();
        state
            .workspaces
            .insert("display-2".to_string(), Tree::new());
        state.synthetic_workspaces.insert("display-2".to_string());
        state.active_workspace.clear();
        state.active_workspace.insert(2, "display-2".to_string());
        state.focused_monitor = 1;

        assert!(
            state
                .move_workspace_to_monitor(Some("display-2"), tili_ipc::MonitorTarget::Main)
                .is_err()
        );
    }

    #[test]
    fn move_workspace_to_monitor_errors_for_an_undeclared_workspace() {
        let mut state = WmState::default();
        assert!(
            state
                .move_workspace_to_monitor(Some("nope"), tili_ipc::MonitorTarget::Main)
                .is_err()
        );
    }

    #[test]
    #[allow(clippy::field_reassign_with_default)]
    fn move_workspace_to_monitor_with_no_name_targets_the_current_workspace() {
        let mut state = WmState::default();
        state.monitors = two_monitor_fixture();
        state.workspaces.insert("a".to_string(), Tree::new());
        state.workspaces.insert("b".to_string(), Tree::new());
        state.active_workspace.clear();
        state.active_workspace.insert(1, "a".to_string());
        state.active_workspace.insert(2, "b".to_string());
        state.focused_monitor = 1;

        assert!(
            state
                .move_workspace_to_monitor(None, tili_ipc::MonitorTarget::Id(2))
                .is_ok()
        );

        assert_eq!(state.active_workspace.get(&2), Some(&"a".to_string()));
        assert_eq!(state.active_workspace.get(&1), Some(&"b".to_string()));
    }

    #[test]
    fn set_floating_true_moves_the_focused_tiled_window_out_of_the_tree() {
        let mut state = floating_test_state();
        let root_orientation = state.root_orientation_hint();
        let node = state
            .workspaces
            .get_mut(DEFAULT_WORKSPACE)
            .unwrap()
            .insert_window(1, None, root_orientation);
        state
            .workspace_focus
            .insert(DEFAULT_WORKSPACE.to_string(), node);
        state.placements.insert(
            1,
            Placement {
                workspace: DEFAULT_WORKSPACE.to_string(),
                kind: PlacementKind::Tiled,
            },
        );

        assert!(state.set_floating(true).is_ok());

        assert!(matches!(
            state.placements.get(&1).map(|p| &p.kind),
            Some(PlacementKind::Floating { .. })
        ));
        let tree = state.workspaces.get(DEFAULT_WORKSPACE).unwrap();
        // Still addressable — just no longer counted as tiled or laid out.
        assert!(tree.find_node(1).is_some());
        assert!(!tree.tiled_window_ids().contains(&1));
    }

    #[test]
    fn set_floating_false_moves_a_floating_window_into_the_tree() {
        let mut state = floating_test_state();
        let root_orientation = state.root_orientation_hint();
        let node = state
            .workspaces
            .get_mut(DEFAULT_WORKSPACE)
            .unwrap()
            .insert_floating(1, None, root_orientation);
        state
            .workspace_focus
            .insert(DEFAULT_WORKSPACE.to_string(), node);
        state.placements.insert(
            1,
            Placement {
                workspace: DEFAULT_WORKSPACE.to_string(),
                kind: PlacementKind::Floating { manual: None },
            },
        );

        assert!(state.set_floating(false).is_ok());

        assert!(matches!(
            state.placements.get(&1).map(|p| &p.kind),
            Some(PlacementKind::Tiled)
        ));
        assert!(
            state
                .workspaces
                .get(DEFAULT_WORKSPACE)
                .unwrap()
                .tiled_window_ids()
                .contains(&1)
        );
    }

    #[test]
    fn set_floating_true_targets_the_tiled_focus_even_with_another_window_already_floating() {
        let mut state = floating_test_state();
        state.placements.insert(
            1,
            Placement {
                workspace: DEFAULT_WORKSPACE.to_string(),
                kind: PlacementKind::Floating { manual: None },
            },
        );
        let root_orientation = state.root_orientation_hint();
        let node = state
            .workspaces
            .get_mut(DEFAULT_WORKSPACE)
            .unwrap()
            .insert_window(2, None, root_orientation);
        state
            .workspace_focus
            .insert(DEFAULT_WORKSPACE.to_string(), node);
        state.placements.insert(
            2,
            Placement {
                workspace: DEFAULT_WORKSPACE.to_string(),
                kind: PlacementKind::Tiled,
            },
        );

        assert!(state.set_floating(true).is_ok());

        assert!(matches!(
            state.placements.get(&2).map(|p| &p.kind),
            Some(PlacementKind::Floating { .. })
        ));
        assert!(matches!(
            state.placements.get(&1).map(|p| &p.kind),
            Some(PlacementKind::Floating { .. })
        ));
    }

    #[test]
    fn set_floating_true_errors_when_nothing_is_focused() {
        let mut state = floating_test_state();
        assert!(state.set_floating(true).is_err());
    }

    #[test]
    fn set_floating_false_errors_when_nothing_is_floating() {
        let mut state = floating_test_state();
        assert!(state.set_floating(false).is_err());
    }

    #[test]
    fn switch_to_previous_workspace_toggles_back_and_forth() {
        let mut state = WmState::default();
        let config = tili_config::parse(
            r#"
            workspaces {
                workspace "a"
                workspace "b"
            }
            "#,
        )
        .unwrap();
        state.apply_config(&config);

        assert!(state.switch_to_previous_workspace().is_err());

        assert!(state.switch_workspace("b").is_ok());
        assert_eq!(state.active_workspace_name(), "b");

        assert!(state.switch_to_previous_workspace().is_ok());
        assert_eq!(state.active_workspace_name(), "a");

        assert!(state.switch_to_previous_workspace().is_ok());
        assert_eq!(state.active_workspace_name(), "b");
    }

    #[test]
    fn workspace_goes_to_its_home_display_instead_of_the_focused_one() {
        let mut state = two_display_home_state();
        // Park work on m1 by switching m1 to random, then stand on m2.
        state.active_workspace.insert(1, "random".to_string());
        state.focused_monitor = 2;

        assert!(state.switch_workspace("work").is_ok());

        assert_eq!(
            state.active_workspace.get(&1),
            Some(&"work".to_string()),
            "work belongs to m1 and must return there"
        );
        assert_eq!(
            state.active_workspace.get(&2),
            Some(&"entertain".to_string()),
            "m2 keeps its own workspace"
        );
        assert_eq!(state.focused_monitor, 1);
    }

    #[test]
    fn workspace_already_visible_elsewhere_only_focuses_that_display() {
        let mut state = two_display_home_state();
        // work is on m1, entertain on m2, focus m1.
        assert!(state.switch_workspace("entertain").is_ok());

        assert_eq!(state.focused_monitor, 2);
        assert_eq!(state.active_workspace.get(&1), Some(&"work".to_string()));
        assert_eq!(
            state.active_workspace.get(&2),
            Some(&"entertain".to_string())
        );
    }

    #[test]
    fn first_use_assigns_a_never_shown_workspace_to_the_focused_display() {
        let mut state = two_display_home_state();
        state.workspaces.insert("fresh".to_string(), Tree::new());
        state.workspace_monitor.remove("fresh");
        state.focused_monitor = 2;

        assert!(state.switch_workspace("fresh").is_ok());

        assert_eq!(state.active_workspace.get(&2), Some(&"fresh".to_string()));
        assert_eq!(state.workspace_monitor.get("fresh"), Some(&2));
        assert_eq!(state.active_workspace.get(&1), Some(&"work".to_string()));
    }

    #[test]
    fn workspace_here_pulls_a_parked_workspace_to_the_focused_display() {
        let mut state = two_display_home_state();
        state.active_workspace.insert(1, "random".to_string());
        state.focused_monitor = 2;

        assert!(state.switch_workspace_here("work").is_ok());

        assert_eq!(state.active_workspace.get(&2), Some(&"work".to_string()));
        assert_eq!(state.workspace_monitor.get("work"), Some(&2));
        assert_eq!(
            state.active_workspace.get(&1),
            Some(&"random".to_string()),
            "the source display is unchanged for a parked pull"
        );
        assert_eq!(state.focused_monitor, 2);
    }

    #[test]
    fn workspace_here_swaps_a_workspace_visible_on_another_display() {
        let mut state = two_display_home_state();

        assert!(state.switch_workspace_here("entertain").is_ok());

        assert_eq!(
            state.active_workspace.get(&1),
            Some(&"entertain".to_string())
        );
        assert_eq!(state.active_workspace.get(&2), Some(&"work".to_string()));
        assert_eq!(state.workspace_monitor.get("entertain"), Some(&1));
        assert_eq!(state.workspace_monitor.get("work"), Some(&2));
        assert_eq!(state.focused_monitor, 1);
    }

    #[test]
    fn workspace_here_refuses_a_workspace_pinned_to_another_display() {
        let mut state = two_display_home_state();
        state
            .workspace_pins
            .insert("entertain".to_string(), "2".to_string());

        assert!(state.switch_workspace_here("entertain").is_err());
        assert_eq!(
            state.active_workspace.get(&2),
            Some(&"entertain".to_string())
        );
    }

    #[test]
    fn workspace_pinned_to_a_disconnected_display_errors_instead_of_moving() {
        let mut state = two_display_home_state();
        state.workspaces.insert("reserved".to_string(), Tree::new());
        state
            .workspace_pins
            .insert("reserved".to_string(), "99".to_string());

        assert!(state.switch_workspace("reserved").is_err());
        assert!(!state.active_workspace.values().any(|w| w == "reserved"));
    }

    #[test]
    fn workspace_back_returns_to_the_previous_display_workspace_pair() {
        let mut state = two_display_home_state();

        assert!(state.switch_workspace("entertain").is_ok());
        assert_eq!(state.focused_monitor, 2);

        assert!(state.switch_to_previous_workspace().is_ok());
        assert_eq!(state.focused_monitor, 1);
        assert_eq!(state.active_workspace.get(&1), Some(&"work".to_string()));
    }

    #[test]
    fn workspace_back_does_not_steal_a_workspace_that_moved_displays() {
        let mut state = two_display_home_state();

        // focus m2 via the visible workspace.
        assert!(state.switch_workspace("entertain").is_ok());
        // work moves to m2, swapping with entertain.
        assert!(
            state
                .move_workspace_to_monitor(Some("work"), tili_ipc::MonitorTarget::Id(2))
                .is_ok()
        );
        assert_eq!(
            state.active_workspace.get(&1),
            Some(&"entertain".to_string())
        );
        assert_eq!(state.active_workspace.get(&2), Some(&"work".to_string()));

        // Back remembers (m1, work), but work now lives on m2. It must only
        // return to m1, not drag work back across displays.
        assert!(state.switch_to_previous_workspace().is_ok());
        assert_eq!(state.focused_monitor, 1);
        assert_eq!(
            state.active_workspace.get(&1),
            Some(&"entertain".to_string())
        );
        assert_eq!(state.active_workspace.get(&2), Some(&"work".to_string()));
    }

    #[test]
    fn workspace_back_undoes_a_workspace_here_swap() {
        let mut state = two_display_home_state();

        // Pull entertain (m2) onto m1: m1=entertain, m2=work.
        assert!(state.switch_workspace_here("entertain").is_ok());
        assert_eq!(
            state.active_workspace.get(&1),
            Some(&"entertain".to_string())
        );
        assert_eq!(state.active_workspace.get(&2), Some(&"work".to_string()));

        // Back restores the pair on the same display by swapping back.
        assert!(state.switch_to_previous_workspace().is_ok());
        assert_eq!(state.active_workspace.get(&1), Some(&"work".to_string()));
        assert_eq!(
            state.active_workspace.get(&2),
            Some(&"entertain".to_string())
        );
        assert_eq!(state.focused_monitor, 1);
    }

    #[test]
    fn switch_epoch_bumps_only_on_a_real_switch() {
        let mut state = WmState::default();
        let config = tili_config::parse(
            r#"
            workspaces {
                workspace "a"
                workspace "b"
            }
            "#,
        )
        .unwrap();
        state.apply_config(&config);

        let epoch0 = state.switch_epoch();

        // No-op: already on "a".
        assert!(state.switch_workspace("a").is_ok());
        assert_eq!(state.switch_epoch(), epoch0);

        // Error: unknown workspace.
        assert!(state.switch_workspace("nope").is_err());
        assert_eq!(state.switch_epoch(), epoch0);

        // Real switch.
        assert!(state.switch_workspace("b").is_ok());
        assert_eq!(state.switch_epoch(), epoch0 + 1);

        // Real switch back.
        assert!(state.switch_to_previous_workspace().is_ok());
        assert_eq!(state.switch_epoch(), epoch0 + 2);
    }

    #[test]
    fn apply_config_keeps_a_workspace_rule_when_its_workspace_is_declared() {
        let mut state = WmState::default();
        let config = tili_config::parse(
            r#"
            workspaces {
                workspace "work"
            }
            workspace-rules {
                rule app-id="com.mitchellh.ghostty" workspace="work"
            }
            "#,
        )
        .unwrap();
        state.apply_config(&config);
        assert_eq!(state.workspace_rules.len(), 1);
        assert_eq!(state.workspace_rules[0].app_id, "com.mitchellh.ghostty");
        assert_eq!(state.workspace_rules[0].workspace, "work");
        assert!(!state.workspace_rules[0].always);
    }

    #[test]
    fn apply_config_threads_the_always_flag_through_to_the_compiled_rule() {
        let mut state = WmState::default();
        let config = tili_config::parse(
            r#"
            workspaces {
                workspace "work"
            }
            workspace-rules {
                rule app-id="com.mitchellh.ghostty" workspace="work" always=#true
            }
            "#,
        )
        .unwrap();
        state.apply_config(&config);
        assert!(state.workspace_rules[0].always);
    }

    #[test]
    fn apply_config_drops_the_whole_workspace_rule_when_its_workspace_isnt_declared() {
        let mut state = WmState::default();
        let config = tili_config::parse(
            r#"
            workspace-rules {
                rule app-id="com.mitchellh.ghostty" workspace="ghost"
            }
            "#,
        )
        .unwrap();
        state.apply_config(&config);
        assert!(
            state.workspace_rules.is_empty(),
            "an undeclared workspace target has nothing else in the rule worth keeping"
        );
    }

    #[test]
    fn place_new_window_tiled_into_the_active_workspace_matches_previous_behavior() {
        let mut state = floating_test_state();
        state.place_new_window(1, &PlacementKind::Tiled, DEFAULT_WORKSPACE);
        assert!(state.active_tree().window_ids().contains(&1));
        assert_eq!(
            state.relayout_calls.get(),
            0,
            "place_new_window itself never relayouts the active case — that's still apply_windows_changed's job"
        );
    }

    #[test]
    fn place_new_window_tiled_into_an_inactive_workspace_switches_to_it_instead_of_parking() {
        let mut state = floating_test_state();
        state.workspaces.insert("side".to_string(), Tree::new());

        state.place_new_window(1, &PlacementKind::Tiled, "side");

        assert!(
            state
                .workspaces
                .get("side")
                .unwrap()
                .window_ids()
                .contains(&1)
        );
        assert_eq!(state.active_workspace_name(), "side");
        assert!(state.active_tree().window_ids().contains(&1));
        assert!(state.relayout_calls.get() > 0);
    }

    #[test]
    #[allow(clippy::field_reassign_with_default)]
    fn place_new_window_tiled_into_a_workspace_active_on_another_monitor_focuses_that_monitor() {
        let mut state = WmState::default();
        state.monitors = two_monitor_fixture();
        state.workspaces.insert("a".to_string(), Tree::new());
        state.workspaces.insert("b".to_string(), Tree::new());
        state.active_workspace.clear();
        state.active_workspace.insert(1, "a".to_string());
        state.active_workspace.insert(2, "b".to_string());
        state.focused_monitor = 1;

        state.place_new_window(1, &PlacementKind::Tiled, "b");

        assert!(state.workspaces.get("b").unwrap().window_ids().contains(&1));
        assert!(
            state.relayout_calls.get() > 0,
            "workspace 'b' is visible on monitor 2, so it should be relaid-out immediately \
             instead of left parked"
        );
        assert_eq!(
            state.active_workspace.get(&1),
            Some(&"a".to_string()),
            "monitor 1 keeps its own workspace; switching to 'b' must not swap layouts"
        );
        assert_eq!(
            state.active_workspace.get(&2),
            Some(&"b".to_string()),
            "monitor 2 keeps showing the workspace the new window landed on"
        );
        assert_eq!(
            state.focused_monitor, 2,
            "focus should move to the display already showing 'b'"
        );
    }

    #[test]
    fn reveal_startup_frontmost_window_with_no_frontmost_leaves_default_untouched() {
        let mut state = floating_test_state();

        state.reveal_startup_frontmost_window(None);

        assert_eq!(state.active_workspace_name(), DEFAULT_WORKSPACE);
        assert_eq!(state.relayout_calls.get(), 0);
    }

    #[test]
    fn reveal_startup_frontmost_window_with_an_unplaced_window_leaves_default_untouched() {
        let mut state = floating_test_state();

        // The initial scan hasn't reached this window yet (or it was
        // filtered out entirely) — no `Placement` exists for it, same
        // observable state as "no clear frontmost at all".
        state.reveal_startup_frontmost_window(Some(1));

        assert_eq!(state.active_workspace_name(), DEFAULT_WORKSPACE);
        assert_eq!(state.relayout_calls.get(), 0);
    }

    #[test]
    fn reveal_startup_frontmost_window_switches_to_the_frontmost_windows_workspace() {
        let mut state = floating_test_state();
        state.workspaces.insert("work".to_string(), Tree::new());
        state.placements.insert(
            1,
            Placement {
                workspace: "work".to_string(),
                kind: PlacementKind::Tiled,
            },
        );

        state.reveal_startup_frontmost_window(Some(1));

        assert_eq!(
            state.active_workspace_name(),
            "work",
            "the daemon restarted with the frontmost window already resolved into 'work' — \
             that should win over apply_config's default_workspace seed"
        );
        assert!(state.relayout_calls.get() > 0);
    }

    #[test]
    fn reveal_startup_frontmost_window_already_showing_the_default_is_a_no_op() {
        let mut state = floating_test_state();
        state.placements.insert(
            1,
            Placement {
                workspace: DEFAULT_WORKSPACE.to_string(),
                kind: PlacementKind::Tiled,
            },
        );

        state.reveal_startup_frontmost_window(Some(1));

        assert_eq!(state.active_workspace_name(), DEFAULT_WORKSPACE);
        assert_eq!(
            state.relayout_calls.get(),
            0,
            "switch_workspace no-ops when the target is already active — nothing to relayout"
        );
    }

    #[test]
    fn place_new_window_floating_joins_the_tree_as_an_inert_leaf() {
        let mut state = floating_test_state();
        state.place_new_window(
            1,
            &PlacementKind::Floating { manual: None },
            DEFAULT_WORKSPACE,
        );
        // Addressable (so `workspace_focus`/`focused_node()` can point at
        // it — see `tili_tree::Node::Floating`), but never counted as a
        // *tiled* window and never given a layout rect.
        assert!(state.active_tree().window_ids().contains(&1));
        assert!(!state.active_tree().tiled_window_ids().contains(&1));
        assert!(
            state
                .active_tree()
                .layout(
                    Rect {
                        x: 0.0,
                        y: 0.0,
                        width: 1000.0,
                        height: 800.0,
                    },
                    Gaps::default()
                )
                .is_empty()
        );
    }

    #[test]
    #[allow(clippy::field_reassign_with_default)]
    fn move_focused_to_workspace_focuses_the_monitor_already_showing_it() {
        let mut state = WmState::default();
        state.monitors = two_monitor_fixture();
        state.workspaces.insert("a".to_string(), Tree::new());
        state.workspaces.insert("b".to_string(), Tree::new());
        state.active_workspace.clear();
        state.active_workspace.insert(1, "a".to_string());
        state.active_workspace.insert(2, "b".to_string());
        state.focused_monitor = 1;

        let root_orientation = state.root_orientation_hint();
        let node = state
            .workspaces
            .get_mut("a")
            .unwrap()
            .insert_window(1, None, root_orientation);
        state.set_focused_node(node);
        state.placements.insert(
            1,
            Placement {
                workspace: "a".to_string(),
                kind: PlacementKind::Tiled,
            },
        );

        assert!(state.move_focused_to_workspace("b").is_ok());
        assert!(state.relayout_calls.get() > 0);
        assert_eq!(
            state.active_workspace.get(&1),
            Some(&"a".to_string()),
            "monitor 1 keeps its own workspace; moving a window must not swap layouts"
        );
        assert_eq!(
            state.active_workspace.get(&2),
            Some(&"b".to_string()),
            "monitor 2 continues showing the destination workspace"
        );
        assert_eq!(
            state.focused_monitor, 2,
            "focus should follow the window to the display already showing 'b'"
        );
    }

    #[test]
    fn move_focused_to_workspace_switches_active_workspace() {
        let mut state = floating_test_state();
        let root_orientation = state.root_orientation_hint();

        let node = state
            .workspaces
            .get_mut(DEFAULT_WORKSPACE)
            .unwrap()
            .insert_window(1, None, root_orientation);
        state.set_focused_node(node);
        state.placements.insert(
            1,
            Placement {
                workspace: DEFAULT_WORKSPACE.to_string(),
                kind: PlacementKind::Tiled,
            },
        );
        state.workspaces.insert("side".to_string(), Tree::new());

        assert!(state.move_focused_to_workspace("side").is_ok());

        assert_eq!(state.active_workspace_name(), "side");
        assert!(state.active_tree().window_ids().contains(&1));
    }

    #[test]
    fn move_focused_to_workspace_clears_the_source_workspaces_fullscreen_focus() {
        // Regression test: `move_focused_to_workspace` used to bypass
        // `remove_from_tree` (calling `Tree::remove_window` directly), which
        // left a stale `fullscreen_focus` entry pointing at the moved
        // window's now-dangling node id — permanently freezing the source
        // workspace's tiled layout, since `relayout_monitor` never falls
        // back away from a `fullscreen_focus` entry it can't resolve.
        let mut state = floating_test_state();
        let root_orientation = state.root_orientation_hint();

        let node = state
            .workspaces
            .get_mut(DEFAULT_WORKSPACE)
            .unwrap()
            .insert_window(1, None, root_orientation);
        state.set_focused_node(node);
        state.placements.insert(
            1,
            Placement {
                workspace: DEFAULT_WORKSPACE.to_string(),
                kind: PlacementKind::Tiled,
            },
        );
        state
            .fullscreen_focus
            .insert(DEFAULT_WORKSPACE.to_string(), node);
        state.workspaces.insert("side".to_string(), Tree::new());

        assert!(state.move_focused_to_workspace("side").is_ok());

        assert!(!state.fullscreen_focus.contains_key(DEFAULT_WORKSPACE));
    }

    #[test]
    fn move_focused_to_workspace_moves_the_floating_focus_not_a_stale_tiled_one() {
        // Regression test: Ghostty (tiled) and Note (floating) both on the
        // default workspace, with Note actually focused — before
        // `Node::Floating` existed, `workspace_focus` could only ever point
        // at a tiled node, so this command moved Ghostty instead of Note.
        let mut state = floating_test_state();
        let root_orientation = state.root_orientation_hint();

        let tiled_node = state
            .workspaces
            .get_mut(DEFAULT_WORKSPACE)
            .unwrap()
            .insert_window(1, None, root_orientation);
        state.placements.insert(
            1,
            Placement {
                workspace: DEFAULT_WORKSPACE.to_string(),
                kind: PlacementKind::Tiled,
            },
        );

        let floating_node = state
            .workspaces
            .get_mut(DEFAULT_WORKSPACE)
            .unwrap()
            .insert_floating(2, Some(tiled_node), root_orientation);
        state.placements.insert(
            2,
            Placement {
                workspace: DEFAULT_WORKSPACE.to_string(),
                kind: PlacementKind::Floating { manual: None },
            },
        );
        state
            .workspace_focus
            .insert(DEFAULT_WORKSPACE.to_string(), floating_node);
        state.workspaces.insert("random".to_string(), Tree::new());

        assert!(state.move_focused_to_workspace("random").is_ok());

        assert_eq!(
            state.placements.get(&2).map(|p| &p.workspace),
            Some(&"random".to_string())
        );
        assert!(matches!(
            state.placements.get(&2).map(|p| &p.kind),
            Some(PlacementKind::Floating { .. })
        ));
        assert_eq!(
            state.placements.get(&1).map(|p| &p.workspace),
            Some(&DEFAULT_WORKSPACE.to_string()),
            "the tiled window must stay put"
        );
        assert!(
            state
                .workspaces
                .get(DEFAULT_WORKSPACE)
                .unwrap()
                .tiled_window_ids()
                .contains(&1)
        );
    }

    #[test]
    fn place_new_window_tiled_into_an_already_focused_workspace_defers_to_the_reactive_resync() {
        // A brand-new window created into a workspace that already has a
        // recorded focus must not silently steal it (`place_new_window`'s
        // `or_insert` only seeds focus for an empty workspace) — but once
        // `sync_focus_to_window` confirms it actually holds real OS focus,
        // `workspace_focus` (and therefore `move_focused_to_workspace`) must
        // follow it, not the stale node.
        let mut state = floating_test_state();
        let root_orientation = state.root_orientation_hint();

        let existing_node = state
            .workspaces
            .get_mut(DEFAULT_WORKSPACE)
            .unwrap()
            .insert_window(1, None, root_orientation);
        state.placements.insert(
            1,
            Placement {
                workspace: DEFAULT_WORKSPACE.to_string(),
                kind: PlacementKind::Tiled,
            },
        );
        state
            .workspace_focus
            .insert(DEFAULT_WORKSPACE.to_string(), existing_node);

        // Mirrors apply_windows_changed: the placement is recorded before
        // place_new_window runs.
        state.placements.insert(
            2,
            Placement {
                workspace: DEFAULT_WORKSPACE.to_string(),
                kind: PlacementKind::Tiled,
            },
        );
        state.place_new_window(2, &PlacementKind::Tiled, DEFAULT_WORKSPACE);

        assert_eq!(
            state.workspace_focus.get(DEFAULT_WORKSPACE),
            Some(&existing_node),
            "a background-opened window must not steal focus by itself"
        );

        // Simulates the live AX resync (sync_focus_from_pid/
        // sync_focus_from_frontmost) discovering window 2 actually holds
        // real OS focus.
        state.sync_focus_to_window(2);

        let new_node = state
            .workspaces
            .get(DEFAULT_WORKSPACE)
            .unwrap()
            .node_for_window(2)
            .unwrap();
        assert_eq!(
            state.workspace_focus.get(DEFAULT_WORKSPACE),
            Some(&new_node)
        );

        state
            .workspaces
            .insert("elsewhere".to_string(), Tree::new());
        assert!(state.move_focused_to_workspace("elsewhere").is_ok());
        assert_eq!(
            state.placements.get(&2).map(|p| &p.workspace),
            Some(&"elsewhere".to_string())
        );
        assert_eq!(
            state.placements.get(&1).map(|p| &p.workspace),
            Some(&DEFAULT_WORKSPACE.to_string()),
            "the pre-existing tiled window must stay put"
        );
    }

    #[test]
    fn place_new_window_floating_into_an_already_focused_workspace_defers_to_the_reactive_resync() {
        // Same race as the tiled variant above, but for a floating window
        // (e.g. Note) created into a workspace that already has a focused
        // tiled window (e.g. Ghostty) — the exact reported repro.
        let mut state = floating_test_state();
        let root_orientation = state.root_orientation_hint();

        let existing_node = state
            .workspaces
            .get_mut(DEFAULT_WORKSPACE)
            .unwrap()
            .insert_window(1, None, root_orientation);
        state.placements.insert(
            1,
            Placement {
                workspace: DEFAULT_WORKSPACE.to_string(),
                kind: PlacementKind::Tiled,
            },
        );
        state
            .workspace_focus
            .insert(DEFAULT_WORKSPACE.to_string(), existing_node);

        state.placements.insert(
            2,
            Placement {
                workspace: DEFAULT_WORKSPACE.to_string(),
                kind: PlacementKind::Floating { manual: None },
            },
        );
        state.place_new_window(
            2,
            &PlacementKind::Floating { manual: None },
            DEFAULT_WORKSPACE,
        );

        assert_eq!(
            state.workspace_focus.get(DEFAULT_WORKSPACE),
            Some(&existing_node),
            "a background-opened floating window must not steal focus by itself"
        );

        state.sync_focus_to_window(2);

        let new_node = state
            .workspaces
            .get(DEFAULT_WORKSPACE)
            .unwrap()
            .node_for_window(2)
            .unwrap();
        assert_eq!(
            state.workspace_focus.get(DEFAULT_WORKSPACE),
            Some(&new_node)
        );

        state
            .workspaces
            .insert("elsewhere".to_string(), Tree::new());
        assert!(state.move_focused_to_workspace("elsewhere").is_ok());
        assert_eq!(
            state.placements.get(&2).map(|p| &p.workspace),
            Some(&"elsewhere".to_string())
        );
        assert!(matches!(
            state.placements.get(&2).map(|p| &p.kind),
            Some(PlacementKind::Floating { .. })
        ));
        assert_eq!(
            state.placements.get(&1).map(|p| &p.workspace),
            Some(&DEFAULT_WORKSPACE.to_string()),
            "the pre-existing tiled window must stay put"
        );
    }

    #[test]
    fn move_focused_errors_when_the_focused_window_is_floating() {
        let mut state = floating_test_state();
        let root_orientation = state.root_orientation_hint();
        let floating_node = state
            .workspaces
            .get_mut(DEFAULT_WORKSPACE)
            .unwrap()
            .insert_floating(1, None, root_orientation);
        state
            .workspace_focus
            .insert(DEFAULT_WORKSPACE.to_string(), floating_node);
        state.placements.insert(
            1,
            Placement {
                workspace: DEFAULT_WORKSPACE.to_string(),
                kind: PlacementKind::Floating { manual: None },
            },
        );

        assert!(state.move_focused(Direction::Right).is_err());
        assert!(state.join(Direction::Right).is_err());
        assert!(state.resize(0.1).is_err());
        assert!(state.balance_sizes(false).is_err());
    }

    fn saved_test_window(id: WindowId, placement: SavedPlacement) -> SavedWindow {
        SavedWindow {
            id,
            pid: id as i32,
            bundle_id: "com.example.app".to_string(),
            title: format!("window {id}"),
            kind: SavedWindowKind::Standard,
            workspace: DEFAULT_WORKSPACE.to_string(),
            placement,
            anchor: None,
            special_manual: None,
            frame: None,
            monitor: None,
        }
    }

    #[test]
    fn finalize_restore_rebuilds_tiles_accordion_and_floating() {
        let mut state = WmState::default();
        state.workspaces.clear();
        state
            .workspaces
            .insert(DEFAULT_WORKSPACE.to_string(), Tree::new());
        state
            .active_workspace
            .insert(state.focused_monitor, DEFAULT_WORKSPACE.to_string());

        let saved = SavedSession {
            version: 1,
            saved_at_unix_ms: 0,
            monitors: Vec::new(),
            focused_monitor: None,
            active_workspaces: Vec::new(),
            previous_workspace: None,
            previous_workspaces: Vec::new(),
            workspaces: vec![SavedWorkspace {
                name: DEFAULT_WORKSPACE.to_string(),
                tree: TreeSnapshot {
                    root: Some(NodeSnapshot::Container {
                        layout: tili_tree::Layout::Accordion,
                        orientation: tili_tree::Orientation::Horizontal,
                        children: vec![
                            NodeSnapshot::Window(1),
                            NodeSnapshot::Window(2),
                            NodeSnapshot::Floating(3),
                        ],
                        weights: vec![1.0, 2.0, 1.0],
                        mru: 2,
                    }),
                    collapsed_root_layout: None,
                },
                focus: Some(2),
                fullscreen: None,
                monitor: None,
                synthetic: false,
            }],
            windows: Vec::new(),
        };
        state.begin_restore(saved, Duration::from_secs(60));
        {
            let restore = state.restore.as_mut().unwrap();
            restore
                .matched
                .insert(101, saved_test_window(1, SavedPlacement::Tiled));
            restore
                .matched
                .insert(102, saved_test_window(2, SavedPlacement::Tiled));
            restore.matched.insert(
                103,
                saved_test_window(3, SavedPlacement::Floating { manual: None }),
            );
        }
        state.placements.insert(
            101,
            Placement {
                workspace: DEFAULT_WORKSPACE.to_string(),
                kind: PlacementKind::Tiled,
            },
        );
        state.placements.insert(
            102,
            Placement {
                workspace: DEFAULT_WORKSPACE.to_string(),
                kind: PlacementKind::Tiled,
            },
        );
        state.placements.insert(
            103,
            Placement {
                workspace: DEFAULT_WORKSPACE.to_string(),
                kind: PlacementKind::Floating { manual: None },
            },
        );

        state.finalize_restore();

        let tree = state
            .workspaces
            .get(DEFAULT_WORKSPACE)
            .expect("restored workspace");
        let mut ids = tree.window_ids();
        ids.sort_unstable();
        assert_eq!(ids, vec![101, 102, 103]);
        assert!(tree.is_root_accordion());
        assert!(tree.find_node(103).is_some(), "floating leaf restored");
        let focus = state.workspace_focus.get(DEFAULT_WORKSPACE).copied();
        assert_eq!(tree.window_at(focus.unwrap()), Some(102));
    }

    #[test]
    fn automatic_session_marks_do_not_clear_zero_match_suppression() {
        let mut state = WmState {
            session_save_suppressed: true,
            ..WmState::default()
        };
        state.mark_session_dirty();
        assert!(state.session_save_suppressed());
        state.mark_user_session_dirty();
        assert!(!state.session_save_suppressed());
    }

    #[test]
    fn remap_snapshot_drops_missing_and_remaps_present_leaves() {
        let snapshot = TreeSnapshot {
            root: Some(NodeSnapshot::Container {
                layout: tili_tree::Layout::Tiles,
                orientation: tili_tree::Orientation::Horizontal,
                children: vec![NodeSnapshot::Window(1), NodeSnapshot::Window(2)],
                weights: vec![1.0, 1.0],
                mru: 1,
            }),
            collapsed_root_layout: None,
        };
        let mut map = HashMap::new();
        map.insert(2, 102);
        let remapped = remap_snapshot(&snapshot.root, &map).expect("one leaf remains");
        let placeholders = TreeSnapshot {
            root: Some(remapped),
            collapsed_root_layout: None,
        };
        let tree = Tree::from_snapshot(&placeholders).expect("valid snapshot");
        assert_eq!(tree.window_ids(), vec![102]);
    }
}
