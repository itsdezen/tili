use axuielement::AXUIElement;
use axuielement::ax_attribute::roles::AX_WINDOW_ROLE;
use axuielement::ax_attribute::subroles::AX_STANDARD_WINDOW_SUBROLE;
use axuielement::ax_attribute::{
    AX_FULL_SCREEN_BUTTON_ATTRIBUTE, AX_ROLE_ATTRIBUTE, AX_SUBROLE_ATTRIBUTE,
};
use axuielement::ffi::{
    AXUIElementRef, kAXFocusedAttribute, kAXPositionAttribute, kAXRaiseAction, kAXSizeAttribute,
    kAXTitleAttribute,
};
use axuielement::{AXPoint, AXSize};
use tili_tree::{Rect, WindowId};

// `_AXUIElementGetWindow` is undocumented but stable and widely relied upon
// by macOS window managers — it's the only way to resolve the real
// `CGWindowID` behind an `AXUIElement`, which the public Accessibility API
// has no attribute for. This is the single private API call anywhere in
// this codebase; everything else goes through public `AXUIElement*` calls,
// so users never need to disable System Integrity Protection.
unsafe extern "C" {
    fn _AXUIElementGetWindow(element: AXUIElementRef, out: *mut u32) -> i32;
}

/// How close two frame coordinates need to be to count as "the same" for
/// the purposes of skipping a redundant AX write — see `AxWindow::set_frame`.
const FRAME_EPSILON: f64 = 0.5;

fn frame_matches(a: Rect, b: Rect) -> bool {
    (a.x - b.x).abs() < FRAME_EPSILON
        && (a.y - b.y).abs() < FRAME_EPSILON
        && (a.width - b.width).abs() < FRAME_EPSILON
        && (a.height - b.height).abs() < FRAME_EPSILON
}

/// Wraps a cached `AXUIElement` handle for one window, plus its resolved
/// `CGWindowID`. All AX/CoreFoundation specifics are confined to this module.
pub struct AxWindow {
    id: WindowId,
    pid: i32,
    bundle_id: Option<String>,
    title: String,
    frame: Rect,
    element: AXUIElement,
}

impl AxWindow {
    /// Reinterprets a reference to `AXUIElement` as its inner raw pointer.
    ///
    /// SAFETY: `axuielement::AXUIElement` is `#[repr(transparent)]` around a
    /// single `*mut c_void` field (verified against the axuielement 0.9.1
    /// source — see the exact version pin in Cargo.toml). Rust guarantees
    /// `#[repr(transparent)]` types share layout, size, and alignment with
    /// their sole non-zero-sized field, so reinterpreting `&AXUIElement` as
    /// `&AXUIElementRef` is sound. This is necessary because `as_ptr()` on
    /// `AXUIElement` is crate-private in `axuielement`, and `_AXUIElementGetWindow`
    /// needs the raw handle.
    fn raw_ptr(element: &AXUIElement) -> AXUIElementRef {
        unsafe { *(element as *const AXUIElement).cast::<AXUIElementRef>() }
    }

    /// Resolves the real `CGWindowID` for an `AXUIElement` window handle.
    /// Returns `None` if the element isn't a real window (e.g. the call
    /// fails), which callers should treat as "skip this element."
    fn resolve_window_id(element: &AXUIElement) -> Option<WindowId> {
        let mut id: u32 = 0;
        // SAFETY: `raw_ptr` returns a valid AXUIElementRef for the lifetime
        // of `element`; `id` is a valid output pointer.
        let status = unsafe { _AXUIElementGetWindow(Self::raw_ptr(element), &mut id) };
        (status == 0).then_some(id)
    }

    /// Whether `element` looks like a real, tileable window rather than a
    /// transient popup/panel (autofill suggestions, IME candidate windows,
    /// spellcheck bubbles, system dialogs) that `kAXWindowsAttribute`
    /// sometimes includes alongside genuine windows. Mirrors (a stripped-down
    /// version of) AeroSpace's own window-vs-popup heuristic — `AXRole`/
    /// `AXSubrole` first, since that's the precise, standard signal AppKit
    /// populates for ordinary `NSWindow`s; a missing `AXSubrole` is treated
    /// as ambiguous rather than rejected outright (silently dropping a
    /// legitimate window is worse than occasionally tiling a stray popup),
    /// tie-broken by presence of a fullscreen button, the same secondary
    /// signal AeroSpace falls back on for exactly this ambiguous case.
    fn looks_like_a_real_window(element: &AXUIElement) -> bool {
        let role = element.string_attribute(AX_ROLE_ATTRIBUTE).ok().flatten();
        let subrole = element
            .string_attribute(AX_SUBROLE_ATTRIBUTE)
            .ok()
            .flatten();
        let result = if role.as_deref().is_some_and(|r| r != AX_WINDOW_ROLE) {
            false
        } else {
            match &subrole {
                Some(s) => s == AX_STANDARD_WINDOW_SUBROLE,
                None => element
                    .element_attribute(AX_FULL_SCREEN_BUTTON_ATTRIBUTE)
                    .ok()
                    .flatten()
                    .is_some(),
            }
        };
        eprintln!("DEBUG looks_like_a_real_window: role={role:?} subrole={subrole:?} -> {result}");
        result
    }

    /// Builds an `AxWindow` from an `AXUIElement` known to be a window
    /// (typically an entry from an application's `AXWindows` attribute).
    /// `bundle_id` is resolved once per process by the caller (see
    /// `enumerate::list_windows_for_pid`) rather than once per window, to
    /// avoid redundant `NSRunningApplication` lookups for apps with
    /// multiple windows. Returns `None` if the private call can't resolve
    /// a window id, or if `element` doesn't look like a real window (see
    /// `looks_like_a_real_window`) — both cases mean "skip this element."
    pub fn from_element(element: AXUIElement, pid: i32, bundle_id: Option<String>) -> Option<Self> {
        if !Self::looks_like_a_real_window(&element) {
            eprintln!("DEBUG from_element pid={pid}: rejected by looks_like_a_real_window");
            return None;
        }
        let Some(id) = Self::resolve_window_id(&element) else {
            eprintln!("DEBUG from_element pid={pid}: resolve_window_id failed");
            return None;
        };
        let title = element
            .string_attribute(kAXTitleAttribute)
            .ok()
            .flatten()
            .unwrap_or_default();
        let position = element.point_attribute(kAXPositionAttribute).ok().flatten();
        let size = element.size_attribute(kAXSizeAttribute).ok().flatten();
        let frame = match (position, size) {
            (Some(p), Some(s)) => Rect {
                x: p.x,
                y: p.y,
                width: s.width,
                height: s.height,
            },
            _ => Rect {
                x: 0.0,
                y: 0.0,
                width: 0.0,
                height: 0.0,
            },
        };

        Some(Self {
            id,
            pid,
            bundle_id,
            title,
            frame,
            element,
        })
    }

    pub fn id(&self) -> WindowId {
        self.id
    }

    pub fn pid(&self) -> i32 {
        self.pid
    }

    pub fn bundle_id(&self) -> Option<&str> {
        self.bundle_id.as_deref()
    }

    pub fn title(&self) -> &str {
        &self.title
    }

    pub fn frame(&self) -> Rect {
        self.frame
    }

    pub fn element(&self) -> &AXUIElement {
        &self.element
    }

    /// Sets the window's position + size via `AXUIElementSetAttributeValue`,
    /// then updates the cached frame to match so callers reading `frame()`
    /// (e.g. `WmState::list_windows`) see the new position immediately
    /// rather than a stale pre-write value — this matters for M4's parked
    /// windows, which are never re-scanned via AX just to confirm where
    /// they ended up.
    ///
    /// Position is set before size: some apps clamp/reflow size based on
    /// their current position (e.g. keeping a fixed distance from a screen
    /// edge), so moving first makes the subsequent resize land correctly.
    /// Best-effort: a window that refuses a resize (fixed-size dialogs,
    /// apps that don't honor AX writes) is left wherever it ends up — tili
    /// has no way to force it, matching every other AX-based WM.
    ///
    /// No-ops (skips both AX writes entirely) if `target` already matches
    /// the cached frame within `FRAME_EPSILON`. This isn't just an
    /// optimization: writing a position/size via AX fires
    /// `AXWindowMoved`/`AXWindowResized` *even when the value is unchanged*,
    /// and `tili-ax`'s own watcher reacts to those by re-triggering a
    /// relayout — without this check, repeatedly setting a window to the
    /// same frame it's already at becomes a self-sustaining feedback loop
    /// (write -> notification -> relayout -> same write -> ...), which
    /// showed up as the window visibly jittering.
    pub fn set_frame(&mut self, target: Rect) {
        if frame_matches(self.frame, target) {
            return;
        }
        let position_result = self.element.set_point_attribute(
            kAXPositionAttribute,
            AXPoint {
                x: target.x,
                y: target.y,
            },
        );
        let size_result = self.element.set_size_attribute(
            kAXSizeAttribute,
            AXSize {
                width: target.width,
                height: target.height,
            },
        );
        eprintln!(
            "DEBUG set_frame id={} target={target:?} position_result={position_result:?} size_result={size_result:?}",
            self.id
        );
        self.frame = target;
    }

    /// Moves the window without touching its size — used to park a window
    /// off-screen (M4), where resizing would be pointless and needlessly
    /// invasive to apps that dislike being resized. Same no-op-if-unchanged
    /// guard as `set_frame`, and for the same feedback-loop reason.
    pub fn set_position(&mut self, x: f64, y: f64) {
        if (self.frame.x - x).abs() < FRAME_EPSILON && (self.frame.y - y).abs() < FRAME_EPSILON {
            return;
        }
        let position_result = self
            .element
            .set_point_attribute(kAXPositionAttribute, AXPoint { x, y });
        eprintln!(
            "DEBUG set_position id={} target=({x}, {y}) position_result={position_result:?}",
            self.id
        );
        self.frame.x = x;
        self.frame.y = y;
    }

    /// Raises and focuses this window — used when tiling changes which
    /// window should be frontmost (e.g. after `focus`/`move`).
    pub fn focus(&self) {
        let _ = self.element.set_bool_attribute(kAXFocusedAttribute, true);
        let _ = self.element.perform_action(kAXRaiseAction);
    }
}
