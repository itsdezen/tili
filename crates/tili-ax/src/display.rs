use core_graphics::display::CGDisplay;
use tili_tree::Rect;

/// A conservative, hardcoded menu-bar height. Getting the *exact* usable
/// area (menu bar + Dock, per-display, notch-aware) is an AppKit
/// `NSScreen.visibleFrame` concept, not something `CGDisplay` exposes —
/// proper multi-monitor `NSScreen` integration lands in M9. Until then this
/// keeps windows from tiling literally underneath the menu bar.
const MENU_BAR_HEIGHT: f64 = 25.0;

/// The main display's usable frame for tiling: full bounds minus a
/// hardcoded menu-bar inset. Single-monitor only — M9 replaces this with
/// real per-monitor `NSScreen.visibleFrame` lookups.
pub fn main_display_frame() -> Rect {
    let bounds = CGDisplay::main().bounds();
    Rect {
        x: bounds.origin.x,
        y: bounds.origin.y + MENU_BAR_HEIGHT,
        width: bounds.size.width,
        height: (bounds.size.height - MENU_BAR_HEIGHT).max(0.0),
    }
}
