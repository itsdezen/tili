use axuielement::AXUIElement;
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

/// Wraps a cached `AXUIElement` handle for one window, plus its resolved
/// `CGWindowID`. All AX/CoreFoundation specifics are confined to this module.
pub struct AxWindow {
    id: WindowId,
    pid: i32,
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

    /// Builds an `AxWindow` from an `AXUIElement` known to be a window
    /// (typically an entry from an application's `AXWindows` attribute).
    /// Returns `None` if the private call can't resolve a window id.
    pub fn from_element(element: AXUIElement, pid: i32) -> Option<Self> {
        let id = Self::resolve_window_id(&element)?;
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
    pub fn set_frame(&mut self, target: Rect) {
        let _ = self.element.set_point_attribute(
            kAXPositionAttribute,
            AXPoint {
                x: target.x,
                y: target.y,
            },
        );
        let _ = self.element.set_size_attribute(
            kAXSizeAttribute,
            AXSize {
                width: target.width,
                height: target.height,
            },
        );
        self.frame = target;
    }

    /// Moves the window without touching its size — used to park a window
    /// off-screen (M4), where resizing would be pointless and needlessly
    /// invasive to apps that dislike being resized.
    pub fn set_position(&mut self, x: f64, y: f64) {
        let _ = self
            .element
            .set_point_attribute(kAXPositionAttribute, AXPoint { x, y });
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
