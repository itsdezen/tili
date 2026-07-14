use std::collections::{HashMap, HashSet};
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

/// macOS has no public API to enumerate/control Spaces, so workspaces are
/// virtual: only the active one's windows are actually laid out on screen.
/// Every other workspace's windows are "parked" — moved to a coordinate far
/// outside the display's bounds, offset per-window so they don't all stack
/// exactly on top of each other (irrelevant to the user, but keeps
/// `tili list-windows` output sane to eyeball while debugging).
const PARK_MARGIN: f64 = 10_000.0;
const PARK_OFFSET_STEP: f64 = 50.0;

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
const REMOVAL_GRACE_PERIOD: Duration = Duration::from_millis(300);

fn frames_match(a: Rect, b: Rect) -> bool {
    (a.x - b.x).abs() < FLOAT_DRIFT_EPSILON
        && (a.y - b.y).abs() < FLOAT_DRIFT_EPSILON
        && (a.width - b.width).abs() < FLOAT_DRIFT_EPSILON
        && (a.height - b.height).abs() < FLOAT_DRIFT_EPSILON
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
/// `Popup` (ambiguous `WindowKind`, see `tili_ax::WindowKind`) is tracked
/// (shows up in `list_windows`) but — like the transient popups it
/// replaces the old binary reject-outright behavior for — is never tiled,
/// floated, or parked; tili simply never touches its geometry.
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

/// Classifies a brand-new window's placement, in priority order:
/// `HiddenApplication`, then `Minimized`, then `NativeFullscreen`, then
/// `Popup` (kind), then `Floating` (a `Dialog`-kind window, or a
/// `Standard`-kind one matching a floating rule — `floating_frame` is
/// `Some` exactly when the latter applies, computed by the caller via
/// `compute_floating_frame`), and finally `Tiled`.
fn classify_new_window(
    kind: tili_ax::WindowKind,
    app_hidden: bool,
    minimized: bool,
    fullscreen: bool,
    floating_frame: Option<Rect>,
) -> PlacementKind {
    let base_restore = if kind == tili_ax::WindowKind::Dialog || floating_frame.is_some() {
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
    match kind {
        tili_ax::WindowKind::Popup => PlacementKind::Popup,
        tili_ax::WindowKind::Dialog => PlacementKind::Floating { manual: None },
        tili_ax::WindowKind::Standard if floating_frame.is_some() => {
            PlacementKind::Floating { manual: None }
        }
        tili_ax::WindowKind::Standard => PlacementKind::Tiled,
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

/// A `tili_config::FloatingRule` with its title pattern pre-compiled —
/// done once in `apply_config`, not on every window creation.
struct CompiledFloatingRule {
    app_id: String,
    title: Option<Regex>,
    width: Option<u32>,
    height: Option<u32>,
    center: Option<bool>,
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
    /// How long a pending removal waits before being finalized — starts at
    /// `REMOVAL_GRACE_PERIOD`; a field rather than using the constant
    /// directly so tests can shrink it to zero instead of sleeping for real.
    removal_grace: Duration,
    workspaces: HashMap<String, Tree>,
    workspace_focus: HashMap<String, NodeId>,
    monitors: Vec<Monitor>,
    /// Where `park` targets, beyond every connected monitor — recomputed
    /// via `tili_ax::choose_parking_corner` whenever `monitors` changes
    /// (`Default::default`, `on_displays_changed`) rather than on every
    /// `park` call, since it only depends on the current monitor
    /// arrangement, not on which window is being parked.
    parking_origin: (f64, f64),
    active_workspace: HashMap<u32, String>,
    focused_monitor: u32,
    frame_setter: Box<dyn WindowFrameSetter>,
    gaps: Gaps,
    workspace_gaps: HashMap<String, Gaps>,
    current_mode: String,
    /// mode name -> (key combo -> command), built fresh from config's
    /// `keybindings` on every `apply_config`.
    mode_bindings: HashMap<String, HashMap<KeyCombo, Command>>,
    /// Matched in order — first match wins.
    floating_rules: Vec<CompiledFloatingRule>,
    floating_defaults: tili_config::FloatingDefaults,
    /// M10: warp the cursor to the newly-focused window on every
    /// focus-changing operation.
    mouse_follows_focus: bool,
    /// M10: moving the cursor onto a different monitor changes
    /// `focused_monitor`, same as an explicit `FocusMonitor` command.
    focus_follows_monitor: bool,
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
    /// How many points of a non-visible `Accordion` sibling peek out from
    /// behind the active one — see `tili_config::Settings::accordion_padding`.
    accordion_padding: f64,
    /// Orientation a workspace root gets when created for its second
    /// window — `None` means "auto" (derive from the target monitor's
    /// aspect ratio in `root_orientation_hint`).
    default_root_orientation: Option<tili_tree::Orientation>,
}

impl Default for WmState {
    fn default() -> Self {
        let mut workspaces = HashMap::new();
        workspaces.insert(DEFAULT_WORKSPACE.to_string(), Tree::new());

        let monitors = tili_ax::list_monitors();
        let parking_origin = tili_ax::choose_parking_corner(&monitors, PARK_MARGIN);
        let focused_monitor = monitors.first().map(|m| m.id).unwrap_or(0);
        let mut active_workspace = HashMap::new();
        active_workspace.insert(focused_monitor, DEFAULT_WORKSPACE.to_string());

        Self {
            windows: HashMap::new(),
            placements: HashMap::new(),
            pending_removal: HashMap::new(),
            removal_grace: REMOVAL_GRACE_PERIOD,
            workspaces,
            workspace_focus: HashMap::new(),
            monitors,
            parking_origin,
            active_workspace,
            focused_monitor,
            frame_setter: Box::new(InstantFrameSetter),
            gaps: Gaps::default(),
            workspace_gaps: HashMap::new(),
            current_mode: DEFAULT_MODE.to_string(),
            mode_bindings: HashMap::new(),
            floating_rules: Vec::new(),
            floating_defaults: tili_config::FloatingDefaults::default(),
            mouse_follows_focus: false,
            focus_follows_monitor: false,
            config_loaded_once: false,
            mouse_button_down: false,
            accordion_padding: 30.0,
            default_root_orientation: None,
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
        let expired: Vec<WindowId> = self
            .pending_removal
            .iter()
            .filter(|&(_, &since)| now.duration_since(since) >= grace)
            .map(|(&id, _)| id)
            .collect();
        for id in expired {
            self.pending_removal.remove(&id);
            self.windows.remove(&id);
            self.remove_placement(id);
        }
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
            let floating_frame = if is_new {
                self.compute_floating_frame(&window)
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

            let placement_kind =
                classify_new_window(kind, app_hidden, minimized, fullscreen, floating_frame);
            match &placement_kind {
                PlacementKind::Tiled => {
                    let near = self.workspace_focus.get(&active_workspace).copied();
                    let root_orientation = self.root_orientation_hint();
                    let node = self
                        .active_tree_mut()
                        .insert_window(id, near, root_orientation);
                    self.workspace_focus
                        .entry(active_workspace.clone())
                        .or_insert(node);
                }
                PlacementKind::Floating { .. } => {
                    if let Some(frame) = floating_frame
                        && let Some(w) = self.windows.get_mut(&id)
                    {
                        self.frame_setter.set_frame(w, frame);
                    }
                }
                PlacementKind::NativeFullscreen(_)
                | PlacementKind::Minimized(_)
                | PlacementKind::HiddenApplication(_)
                | PlacementKind::Popup => {
                    // Tracked but left exactly where/however it already is
                    // — no tree insertion, no frame write.
                }
            }
            self.placements.insert(
                id,
                Placement {
                    workspace: active_workspace.clone(),
                    kind: placement_kind,
                },
            );
        }

        if !self.mouse_button_down {
            self.relayout_active();
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
        // no-op-if-unchanged guard), so calling it redundantly here
        // whenever the window truly is already parked costs nothing.
        if self.parked_positionable_ids().contains(&id) {
            self.park(id, 0);
        }
    }

    /// Moves a window into a special (non-plain) placement state, removing
    /// it from its workspace's tiled tree first if it was `Tiled` (a
    /// minimized/hidden/fullscreen window has no business occupying a
    /// tile) — `Floating` windows just get their kind swapped in place,
    /// since they were never in a tree to begin with.
    fn demote_to_special(&mut self, id: WindowId, special: SpecialKind, restore: Restore) {
        let Some(workspace) = self.placements.get(&id).map(|p| p.workspace.clone()) else {
            return;
        };
        let was_tiled = self
            .placements
            .get(&id)
            .is_some_and(|p| matches!(p.kind, PlacementKind::Tiled));
        if was_tiled {
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
                self.placements.insert(
                    id,
                    Placement {
                        workspace,
                        kind: PlacementKind::Floating { manual: None },
                    },
                );
                let frame = self
                    .windows
                    .get(&id)
                    .and_then(|w| self.compute_floating_frame(w));
                if let Some(frame) = frame
                    && let Some(w) = self.windows.get_mut(&id)
                {
                    self.frame_setter.set_frame(w, frame);
                }
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
        }
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
                })
            })
            .collect();
        self.floating_defaults = config.floating_defaults;

        self.mouse_follows_focus = config.settings.mouse_follows_focus;
        self.focus_follows_monitor = config.settings.focus_follows_monitor;
        self.accordion_padding = f64::from(config.settings.accordion_padding);
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
    /// signal from `tili_ax::spawn_display_watcher`. A disconnected
    /// monitor's active workspace is parked (its windows aren't lost, just
    /// no longer shown anywhere, exactly like switching away from it); a
    /// newly connected monitor gets a fresh, empty workspace. Every
    /// still-visible workspace is re-laid-out afterward since frames may
    /// have changed even for monitors that stayed connected (resolution or
    /// arrangement change).
    pub fn on_displays_changed(&mut self) {
        let new_monitors = tili_ax::list_monitors();
        let new_ids: HashSet<u32> = new_monitors.iter().map(|m| m.id).collect();
        let old_ids: HashSet<u32> = self.monitors.iter().map(|m| m.id).collect();
        let disconnected: Vec<u32> = old_ids.difference(&new_ids).copied().collect();
        let connected: Vec<u32> = new_ids.difference(&old_ids).copied().collect();

        self.monitors = new_monitors;
        self.parking_origin = tili_ax::choose_parking_corner(&self.monitors, PARK_MARGIN);

        for id in disconnected {
            if let Some(name) = self.active_workspace.remove(&id) {
                let outgoing: Vec<WindowId> = self
                    .workspaces
                    .get(&name)
                    .map(Tree::window_ids)
                    .unwrap_or_default()
                    .into_iter()
                    .chain(self.floating_windows_in(&name))
                    .collect();
                for (i, wid) in outgoing.into_iter().enumerate() {
                    self.park(wid, i);
                }
            }
        }

        if !self.monitors.iter().any(|m| m.id == self.focused_monitor) {
            self.focused_monitor = self.monitors.first().map(|m| m.id).unwrap_or(0);
        }

        for id in connected {
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

    /// Moves the focused window one step in `dir`, re-parenting it through
    /// the tree (see `Tree::move_in_direction`) rather than just swapping
    /// which window sits where — the moved window keeps its own `NodeId`,
    /// so it stays "the focused one" without needing to look up a target.
    pub fn move_focused(&mut self, dir: Direction) -> Result<(), String> {
        let current = self.focused_node().ok_or("no window is focused")?;
        if !self.active_tree_mut().move_in_direction(current, dir) {
            return Err("no window in that direction".to_string());
        }
        self.set_focused_node(current);
        self.relayout_active();
        self.raise_focused();
        Ok(())
    }

    /// Wraps the focused window and its neighbor in `dir` into a new,
    /// perpendicular container — AeroSpace's `join-with`.
    pub fn join(&mut self, dir: Direction) -> Result<(), String> {
        let current = self.focused_node().ok_or("no window is focused")?;
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
        if self.active_tree_mut().resize_weight(current, amount) {
            self.relayout_active();
            Ok(())
        } else {
            Err("nothing to resize — no tiled container here".to_string())
        }
    }

    /// Sets the focused window's parent container's orientation (or the
    /// workspace root's, if `root`) — matches AeroSpace's `layout
    /// horizontal`/`layout vertical` (optionally `--root`).
    pub fn set_orientation(
        &mut self,
        orientation: tili_tree::Orientation,
        root: bool,
    ) -> Result<(), String> {
        let current = self.focused_node().ok_or("no window is focused")?;
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

    /// Toggles a container between `Split` (tiled) and `Accordion`
    /// (stacked, one visible at a time) — the focused window's immediate
    /// parent, or (`root: true`) the workspace's root container instead
    /// (matches AeroSpace's `layout --root`; see `Tree::toggle_root_layout`
    /// — still a single container, not a recursive apply-to-everything).
    /// Errors if nothing's focused, or if the target container is a lone
    /// window with no container to toggle.
    pub fn toggle_layout(&mut self, root: bool) -> Result<(), String> {
        let current = self.focused_node().ok_or("no window is focused")?;
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

        let swap_monitor = self
            .active_workspace
            .iter()
            .find(|(id, n)| **id != monitor_id && n.as_str() == name)
            .map(|(&id, _)| id);

        if let Some(outgoing_name) = &current {
            let outgoing: Vec<WindowId> = self
                .workspaces
                .get(outgoing_name)
                .map(Tree::window_ids)
                .unwrap_or_default()
                .into_iter()
                .chain(self.floating_windows_in(outgoing_name))
                .collect();
            for (i, id) in outgoing.into_iter().enumerate() {
                self.park(id, i);
            }
        }

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
        if let Some(swap_id) = swap_monitor {
            self.relayout_monitor(swap_id);
            self.reposition_floating_for_monitor(swap_id);
        }

        let restore = self
            .workspace_focus
            .get(name)
            .copied()
            .or_else(|| self.active_tree().default_focus());
        if let Some(node) = restore {
            self.set_focused_node(node);
            self.raise_focused();
        }

        Ok(())
    }

    /// Moves the focused window into a different workspace's tree and
    /// parks it immediately, unless the target workspace happens to
    /// already be visible on some other monitor — in that case it's
    /// relaid-out there right away instead of sitting parked. Focus moves
    /// to whatever the active workspace suggests next.
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
        let new_node = target_tree.insert_window(id, target_focus_hint, root_orientation);
        self.workspace_focus
            .insert(target_name.to_string(), new_node);
        self.placements.insert(
            id,
            Placement {
                workspace: target_name.to_string(),
                kind: PlacementKind::Tiled,
            },
        );

        self.park(id, 0);
        let visible_elsewhere = self
            .active_workspace
            .iter()
            .find(|(mid, n)| **mid != self.focused_monitor && n.as_str() == target_name)
            .map(|(&mid, _)| mid);
        if let Some(mid) = visible_elsewhere {
            self.relayout_monitor(mid);
        }
        self.relayout_active();
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
    /// if it was tiled (see `remove_from_tree`), or just from `placements`
    /// otherwise (nothing else ever sat in a `Tree`).
    fn remove_placement(&mut self, id: WindowId) {
        let Some(placement) = self.placements.remove(&id) else {
            return;
        };
        if matches!(placement.kind, PlacementKind::Tiled) {
            self.remove_from_tree(id, &placement.workspace);
        }
    }

    /// Removes `id` from `workspace`'s tiled tree, reassigning that
    /// workspace's remembered focus if `id` was it — without touching
    /// `self.placements`, so callers that are about to overwrite the
    /// placement with a new kind (`demote_to_special`) or drop it entirely
    /// (`remove_placement`) both funnel through here.
    fn remove_from_tree(&mut self, id: WindowId, workspace: &str) {
        let Some(tree) = self.workspaces.get_mut(workspace) else {
            return;
        };
        let removed_leaf = tree.find_node(id);
        let suggested = tree.remove_window(id);
        if removed_leaf.is_some() && self.workspace_focus.get(workspace) == removed_leaf.as_ref() {
            match suggested {
                Some(n) => {
                    self.workspace_focus.insert(workspace.to_string(), n);
                }
                None => {
                    self.workspace_focus.remove(workspace);
                }
            }
        }
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

    fn raise_focused(&self) {
        if let Some(node) = self.focused_node()
            && let Some(id) = self.active_tree().window_at(node)
            && let Some(window) = self.windows.get(&id)
        {
            window.focus();
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
    /// comment for why this suppresses relayout.
    pub fn on_mouse_button_down(&mut self) {
        self.mouse_button_down = true;
    }

    /// Marks the left mouse button as released and relays out once, so a
    /// window that was just drag-resized snaps back to the tree's actual
    /// tiled frame instead of staying wherever the drag left it.
    pub fn on_mouse_button_up(&mut self) {
        self.mouse_button_down = false;
        self.relayout_active();
    }

    /// Moves a window off-screen without resizing it, outside the combined
    /// bounds of every *currently connected* monitor so a parked window can
    /// never land on a real display no matter how many are attached or how
    /// they're arranged. `offset_index` spreads multiple simultaneously-
    /// parked windows apart so they don't all land on the exact same
    /// off-screen coordinate.
    fn park(&mut self, id: WindowId, offset_index: usize) {
        self.capture_manual_geometry_before_park(id);
        let (origin_x, origin_y) = self.parking_origin;
        let x = origin_x + (offset_index as f64 * PARK_OFFSET_STEP);
        let y = origin_y;
        if let Some(window) = self.windows.get_mut(&id) {
            window.set_position(x, y);
        }
    }

    fn monitor_frame(&self, monitor_id: u32) -> Option<Rect> {
        self.monitors
            .iter()
            .find(|m| m.id == monitor_id)
            .map(|m| m.frame)
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
    fn relayout_monitor(&mut self, monitor_id: u32) {
        let Some(name) = self.active_workspace.get(&monitor_id).cloned() else {
            return;
        };
        let Some(area) = self.monitor_frame(monitor_id) else {
            return;
        };
        let Some(tree) = self.workspaces.get(&name) else {
            return;
        };
        let gaps = self.workspace_gaps.get(&name).copied().unwrap_or(self.gaps);
        let placements = tree.layout(area, gaps, self.accordion_padding);
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
    /// geometry (Phase 3 — the user dragged/resized it at some point) is
    /// restored proportionally via `restore_floating_frame` instead of
    /// being recomputed fresh from its floating rule, so a reactivated
    /// workspace or a monitor swap doesn't silently discard the user's own
    /// placement.
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
            let frame = match manual {
                Some(geometry) => Some(restore_floating_frame(geometry, area)),
                None => self
                    .windows
                    .get(&id)
                    .and_then(|w| self.initial_floating_frame_in(w, area)),
            };
            if let Some(frame) = frame
                && let Some(window) = self.windows.get_mut(&id)
            {
                self.frame_setter.set_frame(window, frame);
            }
        }
    }

    /// Returns the frame a floating window should be placed at if `window`
    /// matches a floating rule (first match wins), or `None` if it doesn't
    /// match any — meaning it should be tiled instead. Sized/centered
    /// against `focused_monitor`'s frame, since new windows always land on
    /// the focused monitor's active workspace. Rule-based, so only used at
    /// *creation* (and reattachment from a special state) — once a window
    /// has captured manual geometry, `restore_floating_frame` takes over.
    fn compute_floating_frame(&self, window: &AxWindow) -> Option<Rect> {
        let area = self.monitor_frame(self.focused_monitor).unwrap_or(Rect {
            x: 0.0,
            y: 0.0,
            width: 1440.0,
            height: 900.0,
        });
        self.initial_floating_frame_in(window, area)
    }

    fn initial_floating_frame_in(&self, window: &AxWindow, area: Rect) -> Option<Rect> {
        let bundle_id = window.bundle_id()?;
        let rule = self.floating_rules.iter().find(|rule| {
            rule.app_id == bundle_id
                && rule
                    .title
                    .as_ref()
                    .is_none_or(|re| re.is_match(window.title()))
        })?;

        let width = rule
            .width
            .map(f64::from)
            .unwrap_or(area.width * f64::from(self.floating_defaults.width_ratio));
        let height = rule
            .height
            .map(f64::from)
            .unwrap_or(area.height * f64::from(self.floating_defaults.height_ratio));
        let center = rule.center.unwrap_or(self.floating_defaults.center);
        let (x, y) = if center {
            (
                area.x + (area.width - width) / 2.0,
                area.y + (area.height - height) / 2.0,
            )
        } else {
            (area.x, area.y)
        };

        Some(Rect {
            x,
            y,
            width,
            height,
        })
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
        // HiddenApplication beats everything else, even a Standard window
        // that would otherwise just be Tiled.
        assert!(matches!(
            classify_new_window(tili_ax::WindowKind::Standard, true, true, true, None),
            PlacementKind::HiddenApplication(Restore::Tiled)
        ));
        // Minimized beats NativeFullscreen.
        assert!(matches!(
            classify_new_window(tili_ax::WindowKind::Standard, false, true, true, None),
            PlacementKind::Minimized(Restore::Tiled)
        ));
        assert!(matches!(
            classify_new_window(tili_ax::WindowKind::Standard, false, false, true, None),
            PlacementKind::NativeFullscreen(Restore::Tiled)
        ));
    }

    #[test]
    fn classify_new_window_popup_kind_beats_floating_rule_match() {
        let some_frame = Some(Rect {
            x: 0.0,
            y: 0.0,
            width: 100.0,
            height: 100.0,
        });
        assert!(matches!(
            classify_new_window(tili_ax::WindowKind::Popup, false, false, false, some_frame),
            PlacementKind::Popup
        ));
    }

    #[test]
    fn classify_new_window_dialog_is_floating_even_without_a_rule_match() {
        assert!(matches!(
            classify_new_window(tili_ax::WindowKind::Dialog, false, false, false, None),
            PlacementKind::Floating { manual: None }
        ));
    }

    #[test]
    fn classify_new_window_standard_follows_rule_match_else_tiled() {
        let some_frame = Some(Rect {
            x: 0.0,
            y: 0.0,
            width: 100.0,
            height: 100.0,
        });
        assert!(matches!(
            classify_new_window(
                tili_ax::WindowKind::Standard,
                false,
                false,
                false,
                some_frame
            ),
            PlacementKind::Floating { manual: None }
        ));
        assert!(matches!(
            classify_new_window(tili_ax::WindowKind::Standard, false, false, false, None),
            PlacementKind::Tiled
        ));
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
}
