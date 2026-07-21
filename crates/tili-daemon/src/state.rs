use std::collections::{HashMap, HashSet};
use std::sync::LazyLock;
use std::time::{Duration, Instant};

use regex::Regex;
use tili_ax::{AxWindow, InstantFrameSetter, KeyCombo, Monitor, WindowFrameSetter};
use tili_ipc::{Command, MonitorInfo, RectInfo, WindowInfo, WorkspaceInfo};
use tili_tree::{Direction, Gaps, NodeId, Rect, Tree, WindowId};

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
// Every other workspace's windows are "parked" via `tili_ax::parking_position`
// — see `park`'s own doc comment for the technique and why every parked
// window now shares the same coordinate.

/// How close two frames need to be to count as "the same" when deciding
/// whether a `Floating` window's live frame has drifted from what tili
/// itself last set it to (see `maybe_capture_manual_geometry`) — matches
/// `tili-ax`'s own `AxWindow::set_frame` epsilon, since drift detection is
/// only meaningful at that same granularity.
const FLOAT_DRIFT_EPSILON: f64 = 0.5;

/// How long a window that's vanished from a fresh AX scan is kept in
/// `WmState::pending_removal` before being treated as genuinely closed —
/// long enough to absorb a transient AX hiccup (an app briefly failing to
/// report one of its windows mid-update) without losing track of a window
/// that's actually still open. `WmState::removal_grace` starts at this
/// value but is a field (not used directly) so tests can shrink it to zero
/// instead of sleeping for real.
const REMOVAL_GRACE_PERIOD: Duration = Duration::from_millis(100);

/// How long `removal_grace` is boosted to, and how long `reveal_frontmost`
/// distrusts a `frontmost_app_pid()` read, after `WmEvent::SystemDidWake`
/// fires — real hardware has shown an app's AX/WindowServer connection can
/// take far longer to come back after wake than `REMOVAL_GRACE_PERIOD`
/// tolerates. An earlier, shorter value (8s) was confirmed on real hardware
/// to still be too short: `NSWorkspaceDidWakeNotification` fires at the
/// hardware wake itself, which can precede the user actually finishing
/// unlock (password/Touch ID) by up to roughly a minute — apps effectively
/// don't resume reconnecting to the WindowServer/AX until after that, not
/// from the wake instant. `frontmost_app_pid()` itself can also transiently
/// misread which app is frontmost during that same window. Without the
/// removal-grace boost, a still-open window that simply hasn't reconnected
/// yet gets `finalize_expired_removals`'d as "closed," then reappears on the
/// next scan and gets treated as brand new — re-triggering any
/// `workspace-rules` match. Without the `reveal_frontmost` distrust, a
/// misread pid can look like a genuine app switch and force one instead.
/// Both yank the active workspace out from under whatever the user was
/// looking at before sleep. Bounded (not indefinite) so a window that's
/// genuinely closed during this window is still eventually cleaned up, and
/// so a real post-wake app switch isn't permanently ignored.
const WAKE_REMOVAL_GRACE: Duration = Duration::from_secs(90);

/// How long a pid stays in `WmState::pending_launch_pids` after
/// `AppLaunched` before being dropped even without ever getting a window —
/// a bound so a launched-but-windowless process (a background helper, or
/// one that fails to start) can't permanently suppress
/// `reveal_current_frontmost`. Generous relative to a normal GUI app's
/// cold-launch time on purpose: the cost of leaving it too long is a rare
/// missed reveal-on-click, not a wrong one.
const LAUNCH_GRACE_PERIOD: Duration = Duration::from_secs(2);

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

fn frames_match(a: Rect, b: Rect) -> bool {
    (a.x - b.x).abs() < FLOAT_DRIFT_EPSILON
        && (a.y - b.y).abs() < FLOAT_DRIFT_EPSILON
        && (a.width - b.width).abs() < FLOAT_DRIFT_EPSILON
        && (a.height - b.height).abs() < FLOAT_DRIFT_EPSILON
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
///   `Standard` -> tiled) and `SecurityAgent`'s keychain-unlock prompt
///   (misclassified `Dialog` -> re-centered/resized as a floating window
///   instead of left exactly where macOS placed it). Extend only when a
///   *specific* reported system surface is observed getting moved/resized,
///   not preemptively.
/// - `reveal_frontmost` uses it to recognize a pid as a transient
///   activation source rather than an app the user is actually switching
///   to/from (see that function's doc comment). Spotlight and Notification
///   Center are here for this reason specifically — dismissing either
///   (Esc, or a notification banner's close button) was confirmed to
///   otherwise misread as a real app switch and jump/settle-back through
///   whatever workspace the momentarily-reactivated app lives on, the same
///   failure mode `SYSTEM_UI_BUNDLE_IDS`'s `Ignore` forcing prevents for
///   the first use above.
const SYSTEM_UI_BUNDLE_IDS: &[&str] = &[
    "com.apple.dock",
    "com.apple.screencaptureui",
    "com.apple.SecurityAgent",
    "com.apple.Spotlight",
    "com.apple.notificationcenterui",
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
/// drops the whole rule (see `apply_config`'s rebuild), since `workspace`
/// is this rule's only meaningful field.
struct CompiledWorkspaceRule {
    app_id: String,
    workspace: String,
}

/// A `tili_config::FloatingRule` with its title pattern pre-compiled —
/// done once in `apply_config`, not on every window creation.
struct CompiledFloatingRule {
    app_id: String,
    title: Option<Regex>,
    width: Option<u32>,
    height: Option<u32>,
    center: Option<bool>,
    mode: tili_config::FloatingRuleMode,
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
    focused_monitor: u32,
    frame_setter: Box<dyn WindowFrameSetter>,
    gaps: Gaps,
    workspace_gaps: HashMap<String, Gaps>,
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
    /// Whether `apply_config` has run before. Only the *first* call
    /// resolves and applies a real default workspace (see
    /// `apply_config`'s doc comment) — every call after that is a hot
    /// reload, which must never yank focus off whatever's on screen.
    config_loaded_once: bool,
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
    /// Whichever workspace was active on `focused_monitor` immediately
    /// before its last `switch_workspace` call — `Command::WorkspaceBack`'s
    /// target. `switch_workspace` updates this on every switch (including a
    /// `WorkspaceBack`-triggered one), so back-and-forth toggles correctly.
    previous_workspace: Option<String>,
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
    /// The next `cascade_offset` index for each workspace's floating
    /// windows — advanced only by `next_floating_cascade_index`, which
    /// `place_floating_window` calls once per window it actually
    /// auto-centers. Deliberately never reset when a workspace runs out of
    /// floating windows — `cascade_offset` already wraps back to dead
    /// center on its own every `FLOATING_CASCADE_CYCLE` placements, so
    /// resetting here would just be extra bookkeeping for no behavioral
    /// difference.
    floating_cascade: HashMap<String, u32>,
    /// Set by `note_system_wake` to `now + WAKE_REMOVAL_GRACE` — while
    /// `Instant::now()` is still before this, `finalize_expired_removals`
    /// uses `WAKE_REMOVAL_GRACE` instead of `removal_grace`. `None` the rest
    /// of the time.
    wake_grace_until: Option<Instant>,
}

impl Default for WmState {
    fn default() -> Self {
        let mut workspaces = HashMap::new();
        workspaces.insert(DEFAULT_WORKSPACE.to_string(), Tree::new());

        let monitors = tili_ax::list_monitors();
        let focused_monitor = monitors.first().map(|m| m.id).unwrap_or(0);
        let mut active_workspace = HashMap::new();
        active_workspace.insert(focused_monitor, DEFAULT_WORKSPACE.to_string());

        Self {
            windows: HashMap::new(),
            placements: HashMap::new(),
            pending_removal: HashMap::new(),
            pending_bundle_retries: HashMap::new(),
            removal_grace: REMOVAL_GRACE_PERIOD,
            launch_grace: LAUNCH_GRACE_PERIOD,
            #[cfg(test)]
            relayout_calls: std::cell::Cell::new(0),
            workspaces,
            workspace_focus: HashMap::new(),
            monitors,
            active_workspace,
            focused_monitor,
            frame_setter: Box::new(InstantFrameSetter),
            gaps: Gaps::default(),
            workspace_gaps: HashMap::new(),
            current_mode: DEFAULT_MODE.to_string(),
            mode_bindings: HashMap::new(),
            auto_exit_modes: HashSet::new(),
            workspace_rules: Vec::new(),
            floating_rules: Vec::new(),
            floating_defaults: tili_config::FloatingDefaults::default(),
            mouse_follows_focus: false,
            focus_follows_monitor: false,
            mouse_resize_step: 0.1,
            config_loaded_once: false,
            mouse_button_down: false,
            resize_drag: None,
            default_root_orientation: None,
            fullscreen_focus: HashMap::new(),
            previous_workspace: None,
            last_frontmost_pid: None,
            pending_launch_pids: HashMap::new(),
            switch_epoch: 0,
            floating_placed: HashSet::new(),
            floating_cascade: HashMap::new(),
            wake_grace_until: None,
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
        let grace = self.effective_removal_grace(now);
        let expired: Vec<WindowId> = self
            .pending_removal
            .iter()
            .filter(|&(_, &since)| now.duration_since(since) >= grace)
            .map(|(&id, _)| id)
            .collect();
        if expired.is_empty() {
            return;
        }
        for id in &expired {
            if let Some(&since) = self.pending_removal.get(id) {
                eprintln!(
                    "tili-daemon: finalizing window {id} as closed after {:?} pending (grace={grace:?}, wake_grace_active={})",
                    now.duration_since(since),
                    self.wake_grace_until.is_some_and(|until| now < until)
                );
            }
        }
        for id in expired {
            self.pending_removal.remove(&id);
            self.windows.remove(&id);
            self.remove_placement(id);
            self.pending_bundle_retries.remove(&id);
        }
        // A finalized removal changes which windows the tree lays out —
        // without this, survivors keep their pre-removal frames and the
        // closed window's slot stays an empty gap. `relayout_all_visible`,
        // not just the focused monitor: the removed window's workspace may
        // be visible on a different one.
        self.relayout_all_visible();
    }

    /// Records that `pid` just launched (`WmEvent::AppLaunched`) — see
    /// `pending_launch_pids`'s doc comment for why `reveal_current_frontmost`
    /// needs to know this.
    pub fn note_app_launched(&mut self, pid: i32) {
        self.pending_launch_pids.insert(pid, Instant::now());
    }

    /// Records that the system just woke from sleep
    /// (`WmEvent::SystemDidWake`) — boosts `finalize_expired_removals`'s
    /// effective grace period to `WAKE_REMOVAL_GRACE` for the next
    /// `WAKE_REMOVAL_GRACE` seconds, so a window whose owning app hasn't
    /// reconnected to the WindowServer/AX yet doesn't get finalized as
    /// closed and then re-placed as brand new, and makes `reveal_frontmost`
    /// distrust `frontmost_app_pid()` reads for that same window (see
    /// `WAKE_REMOVAL_GRACE`'s doc comment).
    pub fn note_system_wake(&mut self) {
        eprintln!("tili-daemon: system woke, granting {WAKE_REMOVAL_GRACE:?} wake grace");
        self.wake_grace_until = Some(Instant::now() + WAKE_REMOVAL_GRACE);
    }

    /// `removal_grace`, unless a recent `note_system_wake` call's boosted
    /// window (`wake_grace_until`) is still in effect, in which case the
    /// larger `WAKE_REMOVAL_GRACE` applies instead.
    fn effective_removal_grace(&self, now: Instant) -> Duration {
        match self.wake_grace_until {
            Some(until) if now < until => self.removal_grace.max(WAKE_REMOVAL_GRACE),
            _ => self.removal_grace,
        }
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
        // needs to distrust a `frontmost_app_pid()` read over.
        if !fresh.is_empty() {
            self.pending_launch_pids.remove(&pid);
        }

        let fresh_ids: std::collections::HashSet<WindowId> =
            fresh.iter().map(AxWindow::id).collect();

        // A window missing from this scan isn't dropped immediately — it's
        // only a candidate for removal until `finalize_expired_removals`
        // confirms (on a later call) that it's stayed missing for
        // `removal_grace`, absorbing a transient AX hiccup rather than
        // treating every momentary gap as a real close.
        let stale_ids: Vec<WindowId> = self
            .windows
            .iter()
            .filter(|(id, w)| w.pid() == pid && !fresh_ids.contains(id))
            .map(|(&id, _)| id)
            .collect();
        for id in stale_ids {
            self.pending_removal.entry(id).or_insert_with(Instant::now);
        }

        let active_workspace = self.active_workspace_name();
        let app_hidden = tili_ax::is_app_hidden(pid);
        // Set when a brand-new window actually gets placed this pass — lets
        // the post-loop re-sync below skip the extra AX query on every
        // ordinary reconciliation call, not just window-creation ones.
        let mut placed_new_window = false;
        for window in fresh {
            let id = window.id();
            // Present in this scan after all — un-pend it, whether or not
            // it was ever actually pending.
            self.pending_removal.remove(&id);
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
            // reload (see `resolve_disposition`'s doc comment). The two
            // matchers are deliberately independent: which workspace a
            // window lands on has nothing to do with whether it tiles or
            // floats.
            let rule_mode = if is_new {
                if is_system_ui_bundle(window.bundle_id())
                    || is_protected_finder_dialog(window.bundle_id(), window.title())
                {
                    Some(tili_config::FloatingRuleMode::Ignore)
                } else {
                    self.matching_floating_rule(&window).map(|r| r.mode)
                }
            } else {
                None
            };
            let rule_workspace = if is_new {
                self.matching_workspace_rule(&window)
                    .map(|r| r.workspace.clone())
            } else {
                None
            };

            self.windows.insert(id, window);

            if !is_new {
                if let Some(old_frame) = old_frame {
                    self.maybe_capture_manual_geometry(id, old_frame, live_frame);
                }
                self.reconcile_existing_placement(id, app_hidden, minimized, fullscreen);
                continue;
            }

            let target_workspace = rule_workspace.unwrap_or_else(|| active_workspace.clone());
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
        if placed_new_window {
            self.sync_focus_from_pid(pid);
        }

        if !self.mouse_button_down {
            self.relayout_active();
        }
    }

    /// Places a brand-new window into `target_workspace` — tiled into that
    /// workspace's own `Tree` next to its own last focus, or floated/
    /// centered if `target_workspace` is the one currently active on the
    /// focused monitor. If `target_workspace` isn't active on the focused
    /// monitor, the focused monitor switches to it immediately (via
    /// `switch_workspace`, same as `move_focused_to_workspace`) so a window
    /// auto-placed by a `workspace-rules` match is never left off-screen —
    /// unless `wake_grace_until` is still active, in which case the window
    /// still gets placed into `target_workspace` (parked, not shown) but the
    /// switch itself is skipped. Confirmed on real hardware: a window whose
    /// app hasn't reconnected to the WindowServer/AX yet after a real wake
    /// can get wrongly finalized as closed (see `WAKE_REMOVAL_GRACE`) and
    /// then rediscovered moments later — `is_new` from this function's
    /// caller's perspective, but not from the user's — matching its
    /// `workspace-rules` entry and switching away from whatever workspace
    /// was showing before sleep with no real trigger. `reveal_frontmost`
    /// already gets this same wake-grace guard for its own auto-switch
    /// branch; this mirrors it for the `workspace-rules` one.
    /// Deliberately keyed only by `WindowId`/`PlacementKind`, no `AxWindow`
    /// — this is the seam that makes per-app-workspace placement
    /// unit-testable without a live `AXUIElement`, unlike
    /// `apply_windows_changed` itself.
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
                let near = self.workspace_focus.get(target_workspace).copied();
                let root_orientation = self.root_orientation_hint();
                let node = self
                    .workspaces
                    .entry(target_workspace.to_string())
                    .or_default()
                    .insert_window(id, near, root_orientation);
                self.workspace_focus
                    .entry(target_workspace.to_string())
                    .or_insert(node);
            }
            PlacementKind::Floating { .. } => {
                // Joins the tree as a `Node::Floating` leaf exactly like the
                // `Tiled` arm above, so `workspace_focus`/`focused_node()`
                // can address it — its actual on-screen frame is separate
                // (`place_floating_window` below), only written when its
                // workspace is actually visible right now.
                let near = self.workspace_focus.get(target_workspace).copied();
                let root_orientation = self.root_orientation_hint();
                let node = self
                    .workspaces
                    .entry(target_workspace.to_string())
                    .or_default()
                    .insert_floating(id, near, root_orientation);
                self.workspace_focus
                    .entry(target_workspace.to_string())
                    .or_insert(node);
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

        if inactive {
            let in_wake_grace = self
                .wake_grace_until
                .is_some_and(|until| Instant::now() < until);
            eprintln!(
                "tili-daemon: {} workspace '{target_workspace}' for new window {id} (wake_grace_active={in_wake_grace})",
                if in_wake_grace {
                    "skipping auto-switch to"
                } else {
                    "auto-switching to"
                }
            );
            if !in_wake_grace {
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
        if was_in_tree {
            self.remove_from_tree(id, &workspace);
        }
        let kind = match special {
            SpecialKind::HiddenApplication => PlacementKind::HiddenApplication(restore),
            SpecialKind::Minimized => PlacementKind::Minimized(restore),
            SpecialKind::NativeFullscreen => PlacementKind::NativeFullscreen(restore),
        };
        self.placements.insert(id, Placement { workspace, kind });
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
        match restore {
            Restore::Tiled => {
                let near = self.workspace_focus.get(&workspace).copied();
                let root_orientation = self.root_orientation_hint();
                let node = self
                    .workspaces
                    .entry(workspace.clone())
                    .or_default()
                    .insert_window(id, near, root_orientation);
                self.workspace_focus
                    .entry(workspace.clone())
                    .or_insert(node);
                self.placements.insert(
                    id,
                    Placement {
                        workspace,
                        kind: PlacementKind::Tiled,
                    },
                );
            }
            Restore::Floating => {
                let near = self.workspace_focus.get(&workspace).copied();
                let root_orientation = self.root_orientation_hint();
                let node = self
                    .workspaces
                    .entry(workspace.clone())
                    .or_default()
                    .insert_floating(id, near, root_orientation);
                self.workspace_focus
                    .entry(workspace.clone())
                    .or_insert(node);
                self.placements.insert(
                    id,
                    Placement {
                        workspace,
                        kind: PlacementKind::Floating { manual: None },
                    },
                );
                let area = self.focused_monitor_area();
                self.place_floating_window(id, area);
            }
        }
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
    fn maybe_capture_manual_geometry(&mut self, id: WindowId, old_frame: Rect, live_frame: Rect) {
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
        let area = self
            .active_workspace
            .iter()
            .find(|(_, w)| **w == workspace)
            .and_then(|(&mid, _)| self.monitor_frame(mid))
            .or_else(|| self.monitor_frame(self.focused_monitor))
            .unwrap_or(live_frame);
        let geometry = capture_float_geometry(live_frame, area);
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
    pub fn remove_app(&mut self, pid: i32) {
        let ids: Vec<WindowId> = self
            .windows
            .iter()
            .filter(|(_, w)| w.pid() == pid)
            .map(|(&id, _)| id)
            .collect();
        for id in ids {
            self.windows.remove(&id);
            self.remove_placement(id);
            self.pending_removal.remove(&id);
            self.pending_bundle_retries.remove(&id);
        }
        self.pending_launch_pids.remove(&pid);
        self.relayout_all_visible();
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
                self.frame_setter.set_frame(window, frame);
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

    pub fn list_windows(&self) -> Vec<WindowInfo> {
        self.windows
            .values()
            .map(|w| {
                let frame = w.frame();
                WindowInfo {
                    id: w.id(),
                    pid: w.pid(),
                    title: w.title().to_string(),
                    placement: self
                        .placements
                        .get(&w.id())
                        .map(|p| placement_info(&p.kind))
                        .unwrap_or(tili_ipc::PlacementInfo::Tiled),
                    frame: RectInfo {
                        x: frame.x,
                        y: frame.y,
                        width: frame.width,
                        height: frame.height,
                    },
                }
            })
            .collect()
    }

    /// Applies a freshly-loaded (or hot-reloaded) config: updates gaps
    /// (global + per-workspace overrides), rebuilds the keybinding table,
    /// recompiles floating rules (M8 — an invalid title regex logs a
    /// warning and drops just that rule, not the whole config), and
    /// ensures every workspace it declares exists (creating empty ones as
    /// needed) — without switching to any of them, so a config edit never
    /// yanks focus away from whatever workspace the user is actually
    /// looking at. Re-lays-out the active workspace afterward so a gap
    /// change is visible immediately.
    ///
    /// Floating rules only apply to windows created *after* this call —
    /// an already-tiled window that a newly-added rule would now match
    /// stays tiled until it's recreated (matches the M8 acceptance bar:
    /// "auto-center/size on window creation").
    pub fn apply_config(&mut self, config: &tili_config::Config) {
        self.gaps = to_tree_gaps(config.gaps);
        self.workspace_gaps = config
            .workspace_gaps
            .iter()
            .map(|(name, gaps)| (name.clone(), to_tree_gaps(*gaps)))
            .collect();

        for workspace in &config.workspaces {
            self.workspaces.entry(workspace.name.clone()).or_default();
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
            if let Some(default_name) = resolve_default_workspace(config) {
                self.active_workspace
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
                    })
                } else {
                    eprintln!(
                        "tili-daemon: skipping workspace rule for '{}' — workspace '{}' isn't \
                         declared in config",
                        rule.app_id, rule.workspace
                    );
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
                            eprintln!(
                                "tili-daemon: skipping floating rule for '{}' — invalid title regex '{pattern}': {e}",
                                rule.app_id
                            );
                            return None;
                        }
                    },
                    None => None,
                };
                Some(CompiledFloatingRule {
                    app_id: rule.app_id.clone(),
                    title,
                    width: rule.width,
                    height: rule.height,
                    center: rule.center,
                    mode: rule.mode,
                })
            })
            .collect();
        self.floating_defaults = config.floating_defaults;

        self.mouse_follows_focus = config.settings.mouse_follows_focus;
        self.focus_follows_monitor = config.settings.focus_follows_monitor;
        self.mouse_resize_step = config.settings.mouse_resize_step;
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
            .map(|(name, tree)| WorkspaceInfo {
                name: name.clone(),
                active: self.active_workspace.values().any(|n| n == name),
                window_count: tree.window_ids().len(),
                monitor: self
                    .active_workspace
                    .iter()
                    .find(|(_, n)| *n == name)
                    .map(|(&id, _)| id),
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

    /// Cycles `focused_monitor` to the next connected monitor, wrapping —
    /// a no-op with fewer than two monitors connected.
    pub fn focus_monitor_next(&mut self) {
        if self.monitors.len() < 2 {
            return;
        }
        let ids: Vec<u32> = self.monitors.iter().map(|m| m.id).collect();
        let current_idx = ids
            .iter()
            .position(|&id| id == self.focused_monitor)
            .unwrap_or(0);
        self.focused_monitor = ids[(current_idx + 1) % ids.len()];
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
            if self.focused_monitor == old_id {
                self.focused_monitor = new_id;
            }
        }

        for id in diff.disconnected {
            if let Some(name) = self.active_workspace.remove(&id) {
                let outgoing: Vec<WindowId> = self
                    .workspaces
                    .get(&name)
                    .map(Tree::tiled_window_ids)
                    .unwrap_or_default()
                    .into_iter()
                    .chain(self.floating_windows_in(&name))
                    .collect();
                for wid in outgoing {
                    self.park(wid);
                }
            }
        }

        if !self.monitors.iter().any(|m| m.id == self.focused_monitor) {
            self.focused_monitor = self.monitors.first().map(|m| m.id).unwrap_or(0);
        }

        for id in diff.connected {
            let name = format!("monitor-{id}");
            self.workspaces.entry(name.clone()).or_default();
            self.active_workspace.insert(id, name);
        }

        for id in self.active_workspace.keys().copied().collect::<Vec<_>>() {
            self.relayout_monitor(id);
            self.reposition_floating_for_monitor(id);
        }
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
    /// currently considers focused, *before* any command runs — not a
    /// reactive sync triggered by a notification/poll event arriving at
    /// some later, unpredictable time. Confirmed on real hardware that the
    /// reactive version has an unavoidable race: a background poll/notification
    /// updating `workspace_focus` asynchronously can easily still be stale
    /// by the time a hotkey fires moments later, since there's no ordering
    /// guarantee between "the poll noticed the click" and "the user's next
    /// keypress got processed." Other AX-based tiling WMs resolve this the
    /// same way, synchronously at the top of every command — resolve
    /// reality immediately before using it, rather than trusting a cache
    /// that was last updated by some independent, unsynchronized
    /// background event.
    /// Called once at the top of `dispatch()`, covering both socket- and
    /// hotkey-triggered commands.
    pub fn sync_focus_from_frontmost(&mut self) {
        if let Some(pid) = tili_ax::workspace::frontmost_app_pid() {
            self.sync_focus_from_pid(pid);
        }
    }

    /// Syncs `workspace_focus` to reflect a real OS focus change for `pid`'s
    /// currently AX-focused/main window — see `sync_focus_from_frontmost`,
    /// the only real caller. A no-op if: the pid's focused window can't be
    /// resolved, it isn't one of ours, it's neither `Tiled` nor `Floating`
    /// (every other special state has no tree node to focus), or it's
    /// already the recorded focus for its workspace — deliberately doesn't
    /// relayout or raise anything, since the OS already did the actual
    /// focusing here; this only updates internal bookkeeping to match
    /// reality.
    fn sync_focus_from_pid(&mut self, pid: i32) {
        let Some(window_id) = tili_ax::AxWindow::focused_id_for_pid(pid) else {
            return;
        };
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

    /// Moves the focused window one step in `dir`, re-parenting it through
    /// the tree (see `Tree::move_in_direction`) rather than just swapping
    /// which window sits where — the moved window keeps its own `NodeId`,
    /// so it stays "the focused one" without needing to look up a target.
    pub fn move_focused(&mut self, dir: Direction) -> Result<(), String> {
        let current = self.focused_node().ok_or("no window is focused")?;
        if self.focused_window_is_floating(current) {
            return Err("no tiled window is focused".to_string());
        }
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
        let current = self.focused_node().ok_or("no window is focused")?;
        if self.focused_window_is_floating(current) {
            return Err("no tiled window is focused".to_string());
        }
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
        let current = self.focused_node().ok_or("no window is focused")?;
        if !root && self.focused_window_is_floating(current) {
            return Err("no tiled window is focused".to_string());
        }
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
        let current = self.focused_node().ok_or("no window is focused")?;
        if !root && self.focused_window_is_floating(current) {
            return Err("no tiled window is focused".to_string());
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
            Err("nothing to toggle — only one window here".to_string())
        }
    }

    /// Sets a container (focused window's parent, or the workspace root if
    /// `root: true` — see `toggle_layout`) to a specific layout kind — a
    /// no-op if it's already that kind, otherwise the same toggle
    /// `toggle_layout` does (there are only two kinds, so "set" and
    /// "toggle away from the other one" are the same operation).
    pub fn set_layout(&mut self, kind: tili_ipc::LayoutKind, root: bool) -> Result<(), String> {
        let current = self.focused_node().ok_or("no window is focused")?;
        if !root && self.focused_window_is_floating(current) {
            return Err("no tiled window is focused".to_string());
        }
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
        let current = self.focused_node().ok_or("no window is focused")?;
        if !root && self.focused_window_is_floating(current) {
            return Err("no tiled window is focused".to_string());
        }
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
        let current = self.focused_node().ok_or("no window is focused")?;
        if native {
            let id = self
                .active_tree()
                .window_at(current)
                .ok_or("focused window not found")?;
            let window = self
                .windows
                .get_mut(&id)
                .ok_or("focused window not found")?;
            let next = !window.fullscreen();
            window.set_native_fullscreen(next);
            Ok(())
        } else {
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
    /// A second, similarly-shaped case is deliberately *not* suppressed the
    /// same way: Spotlight, the Dock, and Notification Center
    /// (`SYSTEM_UI_BUNDLE_IDS`) only ever own `Popup`-classified windows, so
    /// the "owns zero live windows" check above is *always* true for them —
    /// suppressing on every transition away from one regardless of whether
    /// the user picked a result/icon (should follow) or dismissed it with
    /// Esc/a banner's close button (arguably shouldn't). This process has
    /// no signal to tell those apart — both look identical at the AX/pid
    /// level (a system-UI pid frontmost, then some real pid frontmost
    /// again, target workspace not visible) whenever whatever workspace was
    /// active in between never actually changed the OS-level frontmost app
    /// (e.g. it had nothing to focus). Picking "suppress on a match" over
    /// "always follow" would make that ambiguity resolve toward a pid that
    /// was merely the *last one this process happened to observe*, which
    /// goes stale across exactly that kind of no-op workspace switch and
    /// then permanently suppresses every later reactivation of the same
    /// app — a worse, stuck failure than the one-frame flicker suppressing
    /// would avoid. So a system-UI previous pid always means "follow,"
    /// full stop; only a *normal app's* last window disappearing (below)
    /// still suppresses.
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
    /// Same branch also distrusts `pid` while `wake_grace_until` (see
    /// `note_system_wake`) is still in effect — `frontmost_app_pid()` is a
    /// synchronous `AXFocusedApplication` query, and right after a real wake
    /// it can transiently report a different app than the one genuinely
    /// frontmost while everyone's AX/WindowServer connection is still
    /// reconnecting (the same underlying instability `WAKE_REMOVAL_GRACE`
    /// exists for). Without this, that misread pid still owns live windows
    /// (so `suppress` above doesn't catch it either) and can force-switch
    /// the active workspace a few seconds after wake — the exact symptom
    /// `note_system_wake` was introduced to fix, just through this path
    /// instead of `finalize_expired_removals`.
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
    pub fn reveal_frontmost(&mut self, pid: i32, allow_unchanged_pid: bool) {
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
            return;
        };
        let previous_pid = self.last_frontmost_pid.replace(pid);
        let pid_unchanged = previous_pid == Some(pid);

        let Some(placement) = self.placements.get(&id) else {
            return;
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
                let previous_is_system_ui = previous_pid.is_some_and(|prev| {
                    is_system_ui_bundle(tili_ax::bundle_id_for_pid(prev).as_deref())
                });
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
                // three are deliberately *not* excluded here. Skipped
                // entirely when the previous pid was system UI — see this
                // function's doc comment.
                let suppress = !previous_is_system_ui
                    && previous_pid.is_some_and(|prev| {
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
                // See this function's doc comment on `allow_unchanged_pid`:
                // a same-pid read this function didn't already permit
                // chasing means nothing really changed since the last
                // resolved-to-a-real-window pid, so there's nothing to
                // reveal.
                let in_wake_grace = self
                    .wake_grace_until
                    .is_some_and(|until| Instant::now() < until);
                if (pid_unchanged && !allow_unchanged_pid)
                    || suppress
                    || !self.pending_launch_pids.is_empty()
                    || in_wake_grace
                {
                    return;
                }
                eprintln!(
                    "tili-daemon: auto-switching to workspace '{workspace}' to reveal frontmost pid {pid} (previous_pid={previous_pid:?})"
                );
                if self.switch_workspace(&workspace).is_err() {
                    return;
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
    pub fn reveal_current_frontmost(&mut self, allow_unchanged_pid: bool) {
        if !self.pending_launch_pids.is_empty() {
            return;
        }
        if let Some(pid) = tili_ax::workspace::frontmost_app_pid() {
            self.reveal_frontmost(pid, allow_unchanged_pid);
        }
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

    /// Moves an entire workspace to a different connected monitor without
    /// switching `focused_monitor` to it — whatever workspace `target` was
    /// already showing gets displaced (swapped onto `workspace`'s old
    /// monitor if it was visible anywhere, parked otherwise), mirroring
    /// `switch_workspace`'s own "two monitors never show the same
    /// workspace" invariant. `workspace: None` means whatever's currently
    /// active on `focused_monitor`, rather than requiring an explicit name.
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
        let target_monitor = self.resolve_monitor_target(target)?;

        let current_monitor = self
            .active_workspace
            .iter()
            .find(|(_, w)| w.as_str() == workspace)
            .map(|(&mid, _)| mid);
        if current_monitor == Some(target_monitor) {
            return Ok(());
        }

        let displaced = self.active_workspace.get(&target_monitor).cloned();
        match current_monitor {
            Some(old_monitor) => match &displaced {
                Some(displaced_name) => {
                    self.active_workspace
                        .insert(old_monitor, displaced_name.clone());
                }
                None => {
                    self.active_workspace.remove(&old_monitor);
                }
            },
            None => {
                if let Some(displaced_name) = &displaced {
                    let outgoing: Vec<WindowId> = self
                        .workspaces
                        .get(displaced_name)
                        .map(Tree::tiled_window_ids)
                        .unwrap_or_default()
                        .into_iter()
                        .chain(self.floating_windows_in(displaced_name))
                        .collect();
                    for id in outgoing {
                        self.park(id);
                    }
                }
            }
        }

        self.active_workspace
            .insert(target_monitor, workspace.to_string());
        self.relayout_monitor(target_monitor);
        self.reposition_floating_for_monitor(target_monitor);
        if let Some(old_monitor) = current_monitor {
            self.relayout_monitor(old_monitor);
            self.reposition_floating_for_monitor(old_monitor);
        }
        Ok(())
    }

    fn resolve_monitor_target(&self, target: tili_ipc::MonitorTarget) -> Result<u32, String> {
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
            tili_ipc::MonitorTarget::Next => {
                if self.monitors.len() < 2 {
                    return Err("fewer than two monitors connected".to_string());
                }
                let ids: Vec<u32> = self.monitors.iter().map(|m| m.id).collect();
                let current_idx = ids
                    .iter()
                    .position(|&id| id == self.focused_monitor)
                    .unwrap_or(0);
                Ok(ids[(current_idx + 1) % ids.len()])
            }
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

    /// Switches which workspace is active on `focused_monitor`: parks every
    /// window in the outgoing workspace off-screen (tiled and floating
    /// alike), lays out the incoming one's tiled tree for real and
    /// re-centers its floating windows, and restores its remembered focus.
    /// Creates the target workspace (empty) if it doesn't exist yet. A
    /// no-op if `name` is already active on `focused_monitor`.
    ///
    /// If `name` is currently shown on a *different* monitor, that monitor
    /// swaps to whatever was on `focused_monitor` — two monitors never show
    /// the same workspace at once, since each has its own `Tree` layout
    /// computed against its own frame.
    ///
    /// Errors if `name` isn't a workspace declared in config — workspaces
    /// are only ever created by `apply_config`'s declare-loop, never
    /// on-the-fly by name.
    pub fn switch_workspace(&mut self, name: &str) -> Result<(), String> {
        if !self.workspaces.contains_key(name) {
            return Err(format!("workspace '{name}' isn't declared in config"));
        }

        let monitor_id = self.focused_monitor;
        let current = self.active_workspace.get(&monitor_id).cloned();
        if current.as_deref() == Some(name) {
            return Ok(());
        }

        self.switch_epoch += 1;

        let swap_monitor = self
            .active_workspace
            .iter()
            .find(|(id, n)| **id != monitor_id && n.as_str() == name)
            .map(|(&id, _)| id);

        self.previous_workspace = current.clone();

        // Captured before any parking/relayout so the incoming workspace's
        // windows can be brought on screen *first* (see below) — each AX
        // position write is a synchronous per-window round-trip, not an
        // atomic swap, so parking the outgoing *tiled* windows before the
        // incoming ones arrive leaves a real (if brief) gap with nothing on
        // screen but the desktop. Floating and tiled are tracked separately
        // (see below) since that "avoid a blank gap" reasoning only applies
        // to tiled windows, which fill the screen — floating ones don't.
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

        if let Some(swap_id) = swap_monitor {
            match &current {
                Some(outgoing_name) => {
                    self.active_workspace.insert(swap_id, outgoing_name.clone());
                }
                None => {
                    self.active_workspace.remove(&swap_id);
                }
            }
        }

        self.active_workspace.insert(monitor_id, name.to_string());

        self.relayout_active();
        self.reposition_floating_in_active_workspace();

        // Raised *before* either outgoing park loop below, not after —
        // `raise_focused` changes z-order/keyboard focus only, it doesn't
        // draw anything, so moving it earlier can't uncover the "blank
        // screen" gap `outgoing_tiled`'s comment above is about. Some apps
        // animate a `kAXPositionAttribute` change instead of snapping
        // instantly; if `name`'s app isn't topmost yet by the time a
        // floating outgoing window's park write lands, that window (always
        // above tiled content in z-order) can still render mid-animation on
        // top of the just-shown incoming workspace even though its park
        // write was issued first. Settling z-order here means any such
        // animation plays out already-hidden behind `name`.
        let restore = self
            .workspace_focus
            .get(name)
            .copied()
            .or_else(|| self.active_tree().default_focus());
        if let Some(node) = restore {
            self.set_focused_node(node);
            self.raise_focused();
        }

        if swap_monitor.is_none() {
            for id in outgoing_floating {
                self.park(id);
            }
        }

        if let Some(swap_id) = swap_monitor {
            // The outgoing workspace is now `swap_id`'s active one — laid
            // out directly onto that monitor's own frame below, never
            // parked at all (parking then immediately relaying out
            // elsewhere would just be a second, needless flash).
            self.relayout_monitor(swap_id);
            self.reposition_floating_for_monitor(swap_id);
        } else {
            for id in outgoing_tiled {
                self.park(id);
            }
        }

        Ok(())
    }

    /// Switches back to whichever workspace was active on `focused_monitor`
    /// immediately before its last switch (back-and-forth toggle). Goes
    /// through `switch_workspace` itself, so it updates
    /// `previous_workspace` again in turn (to whatever's active right now),
    /// making repeated calls toggle between the two rather than only ever
    /// working once.
    pub fn switch_to_previous_workspace(&mut self) -> Result<(), String> {
        let target = self
            .previous_workspace
            .clone()
            .ok_or("no previous workspace to switch back to")?;
        self.switch_workspace(&target)
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

        let suggested = self.active_tree_mut().remove_window(id);
        match suggested {
            Some(n) => {
                self.workspace_focus.insert(active_workspace.clone(), n);
            }
            None => {
                self.workspace_focus.remove(&active_workspace);
            }
        }

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

        self.switch_workspace(target_name)?;
        Ok(())
    }

    fn active_workspace_name(&self) -> String {
        self.active_workspace
            .get(&self.focused_monitor)
            .cloned()
            .unwrap_or_else(|| DEFAULT_WORKSPACE.to_string())
    }

    fn active_tree(&self) -> &Tree {
        // Always present: every place that changes `active_workspace` also
        // ensures the corresponding `workspaces` entry exists first, and the
        // `DEFAULT_WORKSPACE` fallback is inserted in `Default::default()`.
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

    /// Drops a window's placement entirely — from its workspace's `Tree`
    /// if it was `Tiled` or `Floating` (see `remove_from_tree`), or just
    /// from `placements` otherwise (every other kind never sat in a
    /// `Tree`).
    fn remove_placement(&mut self, id: WindowId) {
        self.floating_placed.remove(&id);
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
            if let Some(node) = self.remove_from_tree(id, &placement.workspace)
                && let Some(raise_id) = self
                    .workspaces
                    .get(&placement.workspace)
                    .and_then(|tree| tree.window_at(node))
            {
                self.raise_focused_window(raise_id);
            }
        }
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

    /// Moves a window to hug a real monitor's own corner (see
    /// `tili_ax::parking_position`) without resizing it. Every parked
    /// window targets the exact same coordinate — `parking_position`'s
    /// "hidden regardless of size" guarantee only holds with the origin
    /// sitting exactly `PARK_EPSILON` inside the corner; an earlier version
    /// of this function shifted each additional simultaneously-parked
    /// window inward by a step so they wouldn't all land on the identical
    /// point, but that shift itself moves the origin off the one spot the
    /// hiding trick depends on, exposing a real on-screen strip as wide as
    /// the shift — there's no way to "spread apart" and stay hidden at the
    /// same time, and nothing actually needs them spread apart (they're all
    /// invisible at the same point regardless of how many share it). This
    /// also keeps re-parking idempotent for free: `reconcile_existing_placement`'s
    /// re-assertion targets the same coordinate no matter which caller
    /// parked the window first, so `AxWindow::set_position`'s
    /// no-op-if-unchanged guard genuinely no-ops instead of needing to
    /// remember which offset a specific call used.
    fn park(&mut self, id: WindowId) {
        self.capture_manual_geometry_before_park(id);
        let Some(window) = self.windows.get_mut(&id) else {
            return;
        };
        let frame = window.frame();
        let (origin_x, origin_y) =
            tili_ax::parking_position(&self.monitors, (frame.width, frame.height));
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
    fn tiled_layout_inputs(&self, monitor_id: u32) -> Option<(String, Rect, Gaps)> {
        let name = self.active_workspace.get(&monitor_id)?.clone();
        let area = self.monitor_frame(monitor_id)?;
        let gaps = self.workspace_gaps.get(&name).copied().unwrap_or(self.gaps);
        Some((name, area, gaps))
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
                self.frame_setter.set_frame(window, area);
            }
            return;
        }

        let placements = tree.layout(area, gaps);
        for (id, rect) in placements {
            if let Some(window) = self.windows.get_mut(&id) {
                self.frame_setter.set_frame(window, rect);
            }
        }
    }

    /// Re-centers/sizes every floating window belonging to whatever
    /// workspace is active on `focused_monitor` — called when a workspace
    /// becomes active again after being parked, so floating windows land
    /// back where they should rather than staying at their off-screen
    /// parked coordinates.
    fn reposition_floating_in_active_workspace(&mut self) {
        self.reposition_floating_for_monitor(self.focused_monitor);
    }

    /// Re-centers/sizes every floating window belonging to whatever
    /// workspace is active on `monitor_id`. A window with captured manual
    /// geometry (the user dragged/resized it at some point) is restored
    /// proportionally via `restore_floating_frame` instead of being
    /// recomputed fresh from its floating rule, so a reactivated workspace
    /// or a monitor swap doesn't silently discard the user's own placement.
    /// A window with no manual geometry but already in `floating_placed` is
    /// left exactly as-is: it already got its one rule-based placement when
    /// it first floated, and re-deriving that same frame on every later
    /// switch would undo a resize the user made that hasn't been
    /// AX-observed as "manual" yet (e.g. right before parking).
    fn reposition_floating_for_monitor(&mut self, monitor_id: u32) {
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
                None if !self.floating_placed.contains(&id) => {
                    self.place_floating_window(id, area);
                }
                None => {}
            }
        }
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
                && rule
                    .title
                    .as_ref()
                    .is_none_or(|re| re.is_match(window.title()))
        })
    }

    /// Finds the first configured `workspace-rules` entry matching
    /// `window`'s bundle id — `None` if unresolvable or no rule matches.
    /// Deliberately independent of `matching_floating_rule`: which
    /// workspace a window is created on has nothing to do with whether it
    /// ends up tiled or floating.
    fn matching_workspace_rule(&self, window: &AxWindow) -> Option<&CompiledWorkspaceRule> {
        let bundle_id = window.bundle_id()?;
        self.workspace_rules
            .iter()
            .find(|rule| rule.app_id == bundle_id)
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
    /// entry in `floating-rules`). Also returns whether centering was
    /// requested, so `place_floating_window` knows whether a post-write
    /// correction is meaningful.
    fn initial_floating_frame_in(&self, window: &AxWindow, area: Rect) -> (Rect, bool) {
        let rule = self.matching_floating_rule(window);
        let width = rule
            .and_then(|r| r.width)
            .map(f64::from)
            .unwrap_or(area.width * f64::from(self.floating_defaults.width_ratio));
        let height = rule
            .and_then(|r| r.height)
            .map(f64::from)
            .unwrap_or(area.height * f64::from(self.floating_defaults.height_ratio));
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

    /// Returns and advances the next `cascade_offset` index for
    /// `workspace` — each workspace cascades independently, since floating
    /// windows in different workspaces are never shown at the same time.
    fn next_floating_cascade_index(&mut self, workspace: &str) -> u32 {
        let counter = self
            .floating_cascade
            .entry(workspace.to_string())
            .or_insert(0);
        let index = *counter;
        *counter = counter.wrapping_add(1);
        index
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
    /// `next_floating_cascade_index`) so several same-sized floating
    /// windows centered one after another don't all land on the exact same
    /// pixel and fully overlap — clamped back into `area` afterward in
    /// case the nudge would otherwise push the window off-screen (a small
    /// monitor, or little slack between the window and `area`'s edges).
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
            .map(|workspace| cascade_offset(self.next_floating_cascade_index(&workspace)));

        let Some(window) = self.windows.get_mut(&id) else {
            return;
        };
        if !center {
            self.frame_setter.set_frame(window, frame);
        } else {
            window.set_size(frame.width, frame.height);
            let actual = window.live_frame();
            let (dx, dy) = cascade.unwrap_or((0.0, 0.0));
            let cascaded_x = (area.x + (area.width - actual.width) / 2.0 + dx)
                .clamp(area.x, area.x + area.width - actual.width);
            let cascaded_y = (area.y + (area.height - actual.height) / 2.0 + dy)
                .clamp(area.y, area.y + area.height - actual.height);
            window.set_position(cascaded_x, cascaded_y);
        }
        self.floating_placed.insert(id);
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
        }];
        state.active_workspace.clear();
        state
            .active_workspace
            .insert(1, DEFAULT_WORKSPACE.to_string());
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
    fn next_floating_cascade_index_counts_up_independently_per_workspace() {
        let mut state = floating_test_state();
        assert_eq!(state.next_floating_cascade_index(DEFAULT_WORKSPACE), 0);
        assert_eq!(state.next_floating_cascade_index(DEFAULT_WORKSPACE), 1);
        assert_eq!(state.next_floating_cascade_index(DEFAULT_WORKSPACE), 2);
        // A different workspace starts its own sequence from 0.
        assert_eq!(state.next_floating_cascade_index("side"), 0);
        assert_eq!(state.next_floating_cascade_index(DEFAULT_WORKSPACE), 3);
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

    #[test]
    fn note_system_wake_keeps_a_pending_removal_alive_past_zero_removal_grace() {
        let mut state = WmState {
            removal_grace: Duration::ZERO,
            ..WmState::default()
        };
        state.note_system_wake();
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

    #[test]
    fn an_expired_wake_grace_falls_back_to_removal_grace() {
        let mut state = WmState {
            removal_grace: Duration::ZERO,
            ..WmState::default()
        };
        state.wake_grace_until = Some(Instant::now() - Duration::from_secs(1));
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

        state.reveal_current_frontmost(false);

        // The guard must return before ever touching `last_frontmost_pid`
        // (only `reveal_frontmost` sets it) or triggering a relayout —
        // proof the stale-pid AX query never ran.
        assert_eq!(state.last_frontmost_pid, None);
        assert_eq!(state.relayout_calls.get(), 0);
    }

    #[test]
    fn remove_app_clears_pending_launch_pids() {
        let mut state = floating_test_state();
        state.note_app_launched(1234);

        state.remove_app(1234);

        assert!(!state.pending_launch_pids.contains_key(&1234));
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
                bind "alt-shift-s" "mode manage"
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

        state.remove_placement(1);

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
        state.monitors = vec![
            Monitor {
                id: 1,
                frame: Rect {
                    x: 0.0,
                    y: 0.0,
                    width: 1920.0,
                    height: 1080.0,
                },
                is_main: true,
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
            },
        ];
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
    }

    #[test]
    #[allow(clippy::field_reassign_with_default)]
    fn move_workspace_to_monitor_swaps_with_whatever_is_there() {
        let mut state = WmState::default();
        state.monitors = vec![
            Monitor {
                id: 1,
                frame: Rect {
                    x: 0.0,
                    y: 0.0,
                    width: 1920.0,
                    height: 1080.0,
                },
                is_main: true,
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
            },
        ];
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
    fn move_workspace_to_monitor_from_parked_displaces_whatever_was_shown() {
        let mut state = WmState::default();
        state.monitors = vec![
            Monitor {
                id: 1,
                frame: Rect {
                    x: 0.0,
                    y: 0.0,
                    width: 1920.0,
                    height: 1080.0,
                },
                is_main: true,
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
            },
        ];
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

        assert_eq!(state.active_workspace.get(&2), Some(&"parked".to_string()));
        assert!(!state.active_workspace.values().any(|w| w == "shown"));
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
        state.monitors = vec![
            Monitor {
                id: 1,
                frame: Rect {
                    x: 0.0,
                    y: 0.0,
                    width: 1920.0,
                    height: 1080.0,
                },
                is_main: true,
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
            },
        ];
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
    fn place_new_window_tiled_into_a_workspace_active_on_another_monitor_swaps_monitors_to_show_it()
    {
        let mut state = WmState::default();
        state.monitors = vec![
            Monitor {
                id: 1,
                frame: Rect {
                    x: 0.0,
                    y: 0.0,
                    width: 1920.0,
                    height: 1080.0,
                },
                is_main: true,
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
            },
        ];
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
            Some(&"b".to_string()),
            "the focused monitor should now show 'b', the workspace the new window landed on"
        );
        assert_eq!(
            state.active_workspace.get(&2),
            Some(&"a".to_string()),
            "monitor 2 should pick up whatever monitor 1 was showing before, swap-style"
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
    fn move_focused_to_workspace_swaps_monitors_when_target_visible_on_another_monitor() {
        let mut state = WmState::default();
        state.monitors = vec![
            Monitor {
                id: 1,
                frame: Rect {
                    x: 0.0,
                    y: 0.0,
                    width: 1920.0,
                    height: 1080.0,
                },
                is_main: true,
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
            },
        ];
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
            Some(&"b".to_string()),
            "the focused monitor should now show 'b', the workspace the window was moved to"
        );
        assert_eq!(
            state.active_workspace.get(&2),
            Some(&"a".to_string()),
            "monitor 2 should pick up whatever monitor 1 was showing before, swap-style"
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
}
