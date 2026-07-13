use axuielement::AXUIElement;
use tili_tree::{Rect, WindowId};

/// Wraps a cached `AXUIElement` handle for one window, plus its resolved
/// `CGWindowID`. All AX/CoreFoundation specifics are confined to this module.
pub struct AxWindow {
    id: WindowId,
    element: AXUIElement,
}

impl AxWindow {
    /// Resolves the real `CGWindowID` for an `AXUIElement` via the one
    /// documented-but-private call this project uses (`_AXUIElementGetWindow`).
    /// This is the only private API call anywhere in the codebase; everything
    /// else goes through the public Accessibility API, so users never need to
    /// disable System Integrity Protection.
    ///
    /// TODO(M1): wire up the actual `_AXUIElementGetWindow` FFI call and error
    /// handling. Left unimplemented during scaffolding.
    fn resolve_window_id(_element: &AXUIElement) -> WindowId {
        unimplemented!("resolve_window_id: wired up in M1 (read-only window listing)")
    }

    pub fn from_element(element: AXUIElement) -> Self {
        let id = Self::resolve_window_id(&element);
        Self { id, element }
    }

    pub fn id(&self) -> WindowId {
        self.id
    }

    pub fn element(&self) -> &AXUIElement {
        &self.element
    }

    /// Sets the window's position + size via `AXUIElementSetAttributeValue`.
    /// TODO(M3): implement once tiling layout needs to move real windows.
    pub fn set_frame(&self, _target: Rect) {
        unimplemented!("set_frame: wired up in M3 (single-workspace tiling)")
    }
}
