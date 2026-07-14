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

/// The bounding rect of every monitor's full (unadjusted) bounds combined —
/// used to compute a parking-zone origin that can never land on a real,
/// currently-connected display no matter how many are attached, or how
/// they're arranged. `Rect::default()` (all zeros) if `monitors` is empty.
pub fn combined_bounds(monitors: &[Monitor]) -> Rect {
    let Some(first) = monitors.first() else {
        return Rect {
            x: 0.0,
            y: 0.0,
            width: 0.0,
            height: 0.0,
        };
    };
    let mut min_x = first.frame.x;
    let mut min_y = first.frame.y;
    let mut max_x = first.frame.x + first.frame.width;
    let mut max_y = first.frame.y + first.frame.height;
    for m in &monitors[1..] {
        min_x = min_x.min(m.frame.x);
        min_y = min_y.min(m.frame.y);
        max_x = max_x.max(m.frame.x + m.frame.width);
        max_y = max_y.max(m.frame.y + m.frame.height);
    }
    Rect {
        x: min_x,
        y: min_y,
        width: max_x - min_x,
        height: max_y - min_y,
    }
}

fn point_in_rect(x: f64, y: f64, rect: Rect) -> bool {
    x >= rect.x && x <= rect.x + rect.width && y >= rect.y && y <= rect.y + rect.height
}

/// Which corner of `combined_bounds` a parking origin was chosen at — only
/// used to know which direction to push `margin` outward in.
#[derive(Clone, Copy)]
enum Corner {
    BottomRight,
    BottomLeft,
    TopRight,
    TopLeft,
}

/// Picks a corner of the connected monitors' combined bounds to park
/// invisible/inactive-workspace windows beyond, then pushes `margin` units
/// outward from it (in both axes) so the returned point is guaranteed clear
/// of every connected monitor.
///
/// Checked in a fixed preference order — bottom-right, bottom-left,
/// top-right, top-left — picking whichever corner point (the bounding box's
/// actual corner, before pushing outward) is contained in the *fewest*
/// connected monitors' frames; ties (including the common single/dual-
/// monitor case, where every corner sits on some monitor equally) keep
/// today's default of parking off the bottom-right. This matters for
/// irregular (e.g. L-shaped) arrangements: a bounding-box corner that
/// happens to coincide with a real monitor's own corner is a worse parking
/// spot than one that falls in genuinely empty space no monitor reaches —
/// e.g. three monitors arranged in an L leave one quadrant of their
/// combined bounding box empty; parking there instead of hugging whichever
/// monitor happens to own the bottom-right corner keeps parked windows
/// further from anything the user might drag/Mission-Control near.
pub fn choose_parking_corner(monitors: &[Monitor], margin: f64) -> (f64, f64) {
    let bounds = combined_bounds(monitors);
    let candidates = [
        (
            Corner::BottomRight,
            bounds.x + bounds.width,
            bounds.y + bounds.height,
        ),
        (Corner::BottomLeft, bounds.x, bounds.y + bounds.height),
        (Corner::TopRight, bounds.x + bounds.width, bounds.y),
        (Corner::TopLeft, bounds.x, bounds.y),
    ];

    let mut best = candidates[0];
    let mut best_count = usize::MAX;
    for &(corner, x, y) in &candidates {
        let count = monitors
            .iter()
            .filter(|m| point_in_rect(x, y, m.frame))
            .count();
        if count < best_count {
            best_count = count;
            best = (corner, x, y);
        }
    }

    let (corner, x, y) = best;
    match corner {
        Corner::BottomRight => (x + margin, y + margin),
        Corner::BottomLeft => (x - margin, y + margin),
        Corner::TopRight => (x + margin, y - margin),
        Corner::TopLeft => (x - margin, y - margin),
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

    #[test]
    fn combined_bounds_of_no_monitors_is_zero() {
        let bounds = combined_bounds(&[]);
        assert_eq!(bounds.width, 0.0);
        assert_eq!(bounds.height, 0.0);
    }

    #[test]
    fn combined_bounds_of_one_monitor_matches_its_frame() {
        let monitors = [Monitor {
            id: 1,
            frame: Rect {
                x: 0.0,
                y: 0.0,
                width: 1920.0,
                height: 1080.0,
            },
            is_main: true,
        }];
        let bounds = combined_bounds(&monitors);
        assert_eq!(bounds.width, 1920.0);
        assert_eq!(bounds.height, 1080.0);
    }

    #[test]
    fn combined_bounds_spans_side_by_side_monitors() {
        let monitors = [
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
                    width: 2560.0,
                    height: 1440.0,
                },
                is_main: false,
            },
        ];
        let bounds = combined_bounds(&monitors);
        assert_eq!(bounds.x, 0.0);
        assert_eq!(bounds.width, 4480.0);
        assert_eq!(bounds.height, 1440.0);
    }

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
    fn single_monitor_parks_bottom_right_beyond_it() {
        let monitors = [monitor(1, 0.0, 0.0, 1920.0, 1080.0)];
        let (x, y) = choose_parking_corner(&monitors, 100.0);
        assert_eq!(x, 2020.0);
        assert_eq!(y, 1180.0);
    }

    #[test]
    fn side_by_side_equal_height_pair_ties_to_bottom_right() {
        let monitors = [
            monitor(1, 0.0, 0.0, 1920.0, 1080.0),
            monitor(2, 1920.0, 0.0, 1920.0, 1080.0),
        ];
        let (x, y) = choose_parking_corner(&monitors, 100.0);
        assert_eq!(x, 3940.0);
        assert_eq!(y, 1180.0);
    }

    #[test]
    fn l_shaped_triple_prefers_the_uncovered_quadrant() {
        // Top row spans both monitors; only the bottom-left is populated,
        // leaving the combined bounds' bottom-right quadrant empty.
        let monitors = [
            monitor(1, 0.0, 0.0, 1920.0, 1080.0),
            monitor(2, 1920.0, 0.0, 1920.0, 1080.0),
            monitor(3, 0.0, 1080.0, 1920.0, 1080.0),
        ];
        let (x, y) = choose_parking_corner(&monitors, 100.0);
        assert_eq!(x, 3940.0);
        assert_eq!(y, 2260.0);
    }

    #[test]
    fn chooses_the_empty_quadrant_even_when_its_not_bottom_right() {
        // Mirror image of the L above: the *top-left* quadrant is the one
        // left empty, and no monitor's own corner sits there — proving
        // this doesn't just always default to bottom-right.
        let monitors = [
            monitor(1, 1920.0, 0.0, 1920.0, 1080.0),
            monitor(2, 0.0, 1080.0, 1920.0, 1080.0),
            monitor(3, 1920.0, 1080.0, 1920.0, 1080.0),
        ];
        let (x, y) = choose_parking_corner(&monitors, 100.0);
        assert_eq!(x, -100.0);
        assert_eq!(y, -100.0);
    }
}
