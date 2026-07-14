use std::collections::HashSet;
use std::ffi::c_void;
use std::sync::mpsc::Sender;

use core_graphics::display::{
    CGDirectDisplayID, CGDisplay, CGDisplayRegisterReconfigurationCallback,
};
use tili_tree::Rect;

/// A conservative, hardcoded menu-bar height, applied only to the display
/// that's currently `CGDisplay::main()` — secondary displays don't carry a
/// menu bar unless the user has "Displays have separate Spaces" *and* "show
/// menu bar on all displays" both on, which isn't something `CGDisplay`
/// exposes either way. Real per-display `NSScreen.visibleFrame` (accounting
/// for notches, Dock placement, etc.) would be more precise but pulls in a
/// Cocoa coordinate-space flip (`NSScreen` is bottom-left-origin, AX/`CGDisplay`
/// are top-left-origin) that isn't worth the risk for what M9 actually needs.
const MENU_BAR_HEIGHT: f64 = 25.0;

/// One connected, active display.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Monitor {
    /// `CGDirectDisplayID` — stable for as long as the display stays
    /// connected, but not guaranteed to survive sleep/wake or a hot
    /// unplug+replug (macOS may hand out a new one). `list_monitors` is
    /// re-enumerated fresh on every call specifically so callers never hold
    /// onto a stale id longer than one event.
    pub id: u32,
    /// Usable tiling area: full bounds minus the menu-bar inset if `is_main`.
    pub frame: Rect,
    pub is_main: bool,
}

fn to_rect(bounds: core_graphics::geometry::CGRect) -> Rect {
    Rect {
        x: bounds.origin.x,
        y: bounds.origin.y,
        width: bounds.size.width,
        height: bounds.size.height,
    }
}

/// Every currently active display, main display first. Re-enumerated fresh
/// on every call (nothing cached) so hot-plug/unplug is picked up just by
/// calling this again.
pub fn list_monitors() -> Vec<Monitor> {
    let main_id = CGDisplay::main().id;
    let mut monitors: Vec<Monitor> = CGDisplay::active_displays()
        .unwrap_or_default()
        .into_iter()
        .map(|id| {
            let bounds = to_rect(CGDisplay::new(id).bounds());
            let is_main = id == main_id;
            let frame = if is_main {
                Rect {
                    x: bounds.x,
                    y: bounds.y + MENU_BAR_HEIGHT,
                    width: bounds.width,
                    height: (bounds.height - MENU_BAR_HEIGHT).max(0.0),
                }
            } else {
                bounds
            };
            Monitor { id, frame, is_main }
        })
        .collect();
    monitors.sort_by_key(|m| std::cmp::Reverse(m.is_main));
    monitors
}

/// The main display's usable frame. Kept as a convenience for callers that
/// genuinely only care about the main display; multi-monitor-aware callers
/// should use `list_monitors` instead.
pub fn main_display_frame() -> Rect {
    list_monitors()
        .into_iter()
        .find(|m| m.is_main)
        .map(|m| m.frame)
        .unwrap_or(Rect {
            x: 0.0,
            y: 0.0,
            width: 1440.0,
            height: 900.0,
        })
}

/// How far a parked window's origin sits *inside* a real monitor's own
/// corner — not a margin pushed outward. Confirmed on real hardware that
/// setting `kAXPositionAttribute` to somewhere far outside every monitor's
/// bounds (this function's previous approach) gets silently clamped back to
/// within roughly this same distance of the nearest real screen edge
/// regardless of how far outside it's requested — AppKit only constrains a
/// window's *origin* to stay reachable on some real display, it doesn't
/// care whether the window's full frame does. So instead of fighting that
/// clamp, `parking_position` exploits it exactly the way AeroSpace's
/// `MacWindow.hideInCorner` does: keep the origin legitimately on-screen,
/// just barely, and let the window's own size push everything past it off
/// the edge. A larger value here doesn't hide more — it would just be a
/// wider on-screen sliver, the opposite of what parking wants.
const PARK_EPSILON: f64 = 1.0;

/// Computes where to position a window so only a `PARK_EPSILON` sliver of
/// it remains on-screen, hugging the main monitor's (or the first
/// available one's) bottom-right corner. `size` isn't actually needed for
/// this corner specifically — the window's own top-left lands just inside
/// the corner, and its body (extending in the +x/+y direction from there)
/// does all the hiding regardless of how big it is — but is taken anyway
/// so this signature doesn't need to change if a different corner
/// (needing it, like AeroSpace's bottom-left case) is ever added.
///
/// Doesn't need to check other connected monitors: `PARK_EPSILON` is tiny
/// enough that this point is always deep inside the main monitor's own
/// bounds, never anywhere close to a neighboring display's — unlike the
/// previous far-outside-every-bound approach, which genuinely could land
/// on a different physical monitor depending on arrangement.
pub fn parking_position(monitors: &[Monitor], _size: (f64, f64)) -> (f64, f64) {
    let Some(main) = monitors
        .iter()
        .find(|m| m.is_main)
        .or_else(|| monitors.first())
    else {
        return (0.0, 0.0);
    };
    (
        main.frame.x + main.frame.width - PARK_EPSILON,
        main.frame.y + main.frame.height - PARK_EPSILON,
    )
}

/// How close two frame origins need to be (in points) to treat an old and a
/// new monitor id as "the same physical display, just handed a new
/// `CGDirectDisplayID`" — comfortably smaller than any realistic gap between
/// two genuinely different, separately-arranged monitors.
const RENAME_ORIGIN_THRESHOLD: f64 = 50.0;

/// The result of diffing two `list_monitors()` snapshots — see
/// `match_monitors`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MonitorDiff {
    /// (old id, new id) pairs whose frame origins matched within
    /// `RENAME_ORIGIN_THRESHOLD` — treated as the same physical display
    /// reassigned a new id (common across sleep/wake or some hot-plug
    /// sequences), not a real disconnect+connect.
    pub renamed: Vec<(u32, u32)>,
    /// Old ids with no matching new one — genuinely disconnected.
    pub disconnected: Vec<u32>,
    /// New ids with no matching old one — genuinely connected.
    pub connected: Vec<u32>,
}

fn origin_distance(a: Rect, b: Rect) -> f64 {
    ((a.x - b.x).powi(2) + (a.y - b.y).powi(2)).sqrt()
}

/// Diffs two `list_monitors()` snapshots: ids present in both are left out
/// entirely (unchanged), ids only in `old` or only in `new` are first
/// tentatively "disconnected"/"connected," then greedily re-paired by
/// nearest frame-origin distance (closest pair first, each id used at most
/// once) into `renamed` wherever a pairing falls within
/// `RENAME_ORIGIN_THRESHOLD` — true nearest-neighbor matching, not a
/// same-index/array-position zip, so two monitors that happen to swap
/// which physical slot they occupy (each ending up at the *other's* old
/// origin) still match correctly to whichever one they actually are, not
/// to whichever one `list_monitors` happened to enumerate first.
pub fn match_monitors(old: &[Monitor], new: &[Monitor]) -> MonitorDiff {
    let old_ids: HashSet<u32> = old.iter().map(|m| m.id).collect();
    let new_ids: HashSet<u32> = new.iter().map(|m| m.id).collect();

    let mut gone: Vec<&Monitor> = old.iter().filter(|m| !new_ids.contains(&m.id)).collect();
    let mut arrived: Vec<&Monitor> = new.iter().filter(|m| !old_ids.contains(&m.id)).collect();

    let mut renamed = Vec::new();
    loop {
        let mut best: Option<(usize, usize, f64)> = None;
        for (gi, g) in gone.iter().enumerate() {
            for (ai, a) in arrived.iter().enumerate() {
                let dist = origin_distance(g.frame, a.frame);
                if dist <= RENAME_ORIGIN_THRESHOLD
                    && best.is_none_or(|(_, _, best_dist)| dist < best_dist)
                {
                    best = Some((gi, ai, dist));
                }
            }
        }
        let Some((gi, ai, _)) = best else {
            break;
        };
        renamed.push((gone[gi].id, arrived[ai].id));
        gone.remove(gi);
        arrived.remove(ai);
    }

    MonitorDiff {
        renamed,
        disconnected: gone.into_iter().map(|m| m.id).collect(),
        connected: arrived.into_iter().map(|m| m.id).collect(),
    }
}

unsafe extern "C" fn reconfiguration_callback(
    _display: CGDirectDisplayID,
    _flags: u32,
    user_info: *const c_void,
) {
    // SAFETY: `user_info` was created from `Box::into_raw` in
    // `spawn_display_watcher` and lives for the process's lifetime (the
    // callback is never unregistered), so the raw pointer is always valid
    // for this cast. The flags/summary aren't inspected — every callback
    // just tells the daemon "something changed, re-enumerate" via
    // `list_monitors`, which is simpler and less error-prone than trying to
    // interpret `CGDisplayChangeSummaryFlags` bit-by-bit.
    let tx = unsafe { &*(user_info as *const Sender<()>) };
    let _ = tx.send(());
}

unsafe extern "C" {
    fn CFRunLoopRun();
}

/// Spawns a dedicated OS thread that registers a `CGDisplayRegisterReconfigurationCallback`
/// and pumps a `CFRunLoop` on that thread for the process's lifetime — same
/// pattern as `workspace::spawn_workspace_watcher`, since reconfiguration
/// callbacks are delivered on whichever thread's run loop is running when
/// they're registered. Each signal on the returned channel just means "call
/// `list_monitors` again," not any specific change.
pub fn spawn_display_watcher() -> std::sync::mpsc::Receiver<()> {
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let user_info = Box::into_raw(Box::new(tx)) as *const c_void;
        unsafe {
            CGDisplayRegisterReconfigurationCallback(reconfiguration_callback, user_info);
            CFRunLoopRun();
        }
    });
    rx
}

#[cfg(test)]
mod tests {
    use super::*;

    fn monitor(id: u32, x: f64, y: f64, width: f64, height: f64) -> Monitor {
        Monitor {
            id,
            frame: Rect {
                x,
                y,
                width,
                height,
            },
            is_main: id == 1,
        }
    }

    #[test]
    fn single_monitor_parks_one_epsilon_inside_its_bottom_right_corner() {
        let monitors = [monitor(1, 0.0, 0.0, 1920.0, 1080.0)];
        let (x, y) = parking_position(&monitors, (400.0, 300.0));
        assert_eq!(x, 1919.0);
        assert_eq!(y, 1079.0);
    }

    #[test]
    fn side_by_side_pair_still_parks_at_the_main_monitors_own_corner() {
        let monitors = [
            monitor(1, 0.0, 0.0, 1920.0, 1080.0),
            monitor(2, 1920.0, 0.0, 1920.0, 1080.0),
        ];
        let (x, y) = parking_position(&monitors, (400.0, 300.0));
        assert_eq!(x, 1919.0);
        assert_eq!(y, 1079.0);
    }

    #[test]
    fn falls_back_to_the_first_monitor_when_none_is_main() {
        let mut monitors = [monitor(1, 0.0, 0.0, 1920.0, 1080.0)];
        monitors[0].is_main = false;
        let (x, y) = parking_position(&monitors, (400.0, 300.0));
        assert_eq!(x, 1919.0);
        assert_eq!(y, 1079.0);
    }

    #[test]
    fn no_monitors_parks_at_the_origin() {
        let (x, y) = parking_position(&[], (400.0, 300.0));
        assert_eq!(x, 0.0);
        assert_eq!(y, 0.0);
    }

    #[test]
    fn same_origin_id_change_is_a_rename() {
        let old = [monitor(1, 0.0, 0.0, 1920.0, 1080.0)];
        let new = [monitor(2, 0.0, 0.0, 1920.0, 1080.0)];
        let diff = match_monitors(&old, &new);
        assert_eq!(diff.renamed, vec![(1, 2)]);
        assert!(diff.disconnected.is_empty());
        assert!(diff.connected.is_empty());
    }

    #[test]
    fn two_monitors_swapping_ids_are_matched_by_origin_not_array_position() {
        let old = [
            monitor(1, 0.0, 0.0, 1920.0, 1080.0),
            monitor(2, 1920.0, 0.0, 1920.0, 1080.0),
        ];
        // Deliberately listed in swapped order relative to `old`, so a
        // naive same-index zip would wrongly pair id 1 with id 3 (whose
        // origins are 1920 apart) instead of the true nearest match.
        let new = [
            monitor(3, 1920.0, 0.0, 1920.0, 1080.0),
            monitor(4, 0.0, 0.0, 1920.0, 1080.0),
        ];
        let diff = match_monitors(&old, &new);
        let mut renamed = diff.renamed;
        renamed.sort_unstable();
        assert_eq!(renamed, vec![(1, 4), (2, 3)]);
        assert!(diff.disconnected.is_empty());
        assert!(diff.connected.is_empty());
    }

    #[test]
    fn unchanged_ids_are_not_reported_at_all() {
        let old = [monitor(1, 0.0, 0.0, 1920.0, 1080.0)];
        let new = [monitor(1, 0.0, 0.0, 1920.0, 1080.0)];
        let diff = match_monitors(&old, &new);
        assert!(diff.renamed.is_empty());
        assert!(diff.disconnected.is_empty());
        assert!(diff.connected.is_empty());
    }

    #[test]
    fn a_genuinely_new_monitor_far_from_any_old_one_is_connected_not_renamed() {
        let old = [monitor(1, 0.0, 0.0, 1920.0, 1080.0)];
        let new = [
            monitor(1, 0.0, 0.0, 1920.0, 1080.0),
            monitor(2, 5000.0, 5000.0, 1920.0, 1080.0),
        ];
        let diff = match_monitors(&old, &new);
        assert!(diff.renamed.is_empty());
        assert!(diff.disconnected.is_empty());
        assert_eq!(diff.connected, vec![2]);
    }

    #[test]
    fn a_genuinely_disconnected_monitor_with_no_replacement_stays_disconnected() {
        let old = [
            monitor(1, 0.0, 0.0, 1920.0, 1080.0),
            monitor(2, 1920.0, 0.0, 1920.0, 1080.0),
        ];
        let new = [monitor(1, 0.0, 0.0, 1920.0, 1080.0)];
        let diff = match_monitors(&old, &new);
        assert!(diff.renamed.is_empty());
        assert_eq!(diff.disconnected, vec![2]);
        assert!(diff.connected.is_empty());
    }
}
