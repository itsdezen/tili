use axuielement::AXUIElement;
use axuielement::ax_attribute::roles::AX_WINDOW_ROLE;
use axuielement::ax_attribute::subroles::{
    AX_DIALOG_SUBROLE, AX_STANDARD_WINDOW_SUBROLE, AX_SYSTEM_DIALOG_SUBROLE,
};
use axuielement::ax_attribute::{
    AX_CLOSE_BUTTON_ATTRIBUTE, AX_FULL_SCREEN_BUTTON_ATTRIBUTE, AX_MINIMIZE_BUTTON_ATTRIBUTE,
    AX_MINIMIZED_ATTRIBUTE, AX_ROLE_ATTRIBUTE, AX_SUBROLE_ATTRIBUTE, AX_ZOOM_BUTTON_ATTRIBUTE,
};
use axuielement::ffi::{
    AXUIElementRef, kAXFocusedAttribute, kAXPositionAttribute, kAXPressAction, kAXRaiseAction,
    kAXSizeAttribute, kAXTitleAttribute,
};
use axuielement::{AXPoint, AXSize};
use tili_tree::{Rect, WindowId};

/// The window's own fullscreen state (distinct from `AX_FULL_SCREEN_BUTTON_ATTRIBUTE`,
/// which is just whether the button exists). Not exported as a constant by
/// the `axuielement` crate, but it's a plain bool attribute name so no patch
/// is needed to read/write it.
const AX_FULL_SCREEN_ATTRIBUTE: &str = "AXFullScreen";

/// A 3-way classification of what an `AXWindows`-attribute entry actually
/// is, replacing the old binary "is this a real window" check. `Dialog` and
/// `Popup` are both tracked (never dropped) — `Popup` just means "ambiguous,
/// couldn't confirm `Standard` or `Dialog`," not "not a window."
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WindowKind {
    Standard,
    Dialog,
    Popup,
}

/// Pure decision function behind `WindowKind` classification — plain
/// string/bool inputs so it's unit-testable without a real `AXUIElement`.
///
/// Checked first, ahead of subrole: a non-regular-activation-policy process
/// (`.accessory`/`.prohibited` — system-UI helpers like the Dock,
/// `SecurityAgent`, or the screenshot toolbar, none of which show a Dock
/// icon or appear in Cmd-Tab) presenting a window with no close button is
/// essentially always transient chrome, not a real user-facing window — a
/// single structural check (`activationPolicy == .accessory && closeButton
/// == nil`) instead of an open-ended per-bundle-id denylist. A regular
/// app's own dialogs/popups still fall through to the subrole logic below
/// unaffected, since `is_regular_app` is `true` for them.
///
/// Below that: a known dialog subrole is `Dialog`. A standard subrole with a
/// zoom button but no full-screen button is also `Dialog` — real content
/// windows are fullscreen-capable (`AXFullScreenButton`) while AppKit gives
/// non-fullscreenable utility panels a classic zoom button instead
/// (`AXZoomButton`), and Preferences/Settings-style windows are the
/// textbook case: hardware-confirmed on Safari's own Settings window, which
/// reports `AXStandardWindow` like a real browser window but swaps
/// full-screen for zoom. Otherwise a standard subrole is `Standard`; a
/// missing/ambiguous subrole falls back to whether the window has *any*
/// window-chrome button (close/fullscreen/zoom/minimize) — a window with
/// none of them and an ambiguous subrole is essentially always a
/// tooltip/context-menu/popover rather than a real top-level window.
fn classify_window_kind(
    subrole: Option<&str>,
    has_any_chrome_button: bool,
    has_close_button: bool,
    is_regular_app: bool,
    has_zoom_button: bool,
    has_fullscreen_button: bool,
) -> WindowKind {
    if !is_regular_app && !has_close_button {
        return WindowKind::Popup;
    }
    match subrole {
        Some(s) if s == AX_DIALOG_SUBROLE || s == AX_SYSTEM_DIALOG_SUBROLE => WindowKind::Dialog,
        Some(s) if s == AX_STANDARD_WINDOW_SUBROLE && has_zoom_button && !has_fullscreen_button => {
            WindowKind::Dialog
        }
        Some(s) if s == AX_STANDARD_WINDOW_SUBROLE => WindowKind::Standard,
        None if has_any_chrome_button => WindowKind::Standard,
        _ => WindowKind::Popup,
    }
}

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

pub(crate) fn frame_matches(a: Rect, b: Rect) -> bool {
    (a.x - b.x).abs() < FRAME_EPSILON
        && (a.y - b.y).abs() < FRAME_EPSILON
        && (a.width - b.width).abs() < FRAME_EPSILON
        && (a.height - b.height).abs() < FRAME_EPSILON
}

/// Reads a window's current position/size straight from AX — `None` if
/// either attribute is unreadable. Shared by `from_element` (initial
/// construction) and `AxWindow::live_frame` (a post-write check), so both
/// agree on exactly what "the real frame" means.
fn read_frame(element: &AXUIElement) -> Option<Rect> {
    let position = element
        .point_attribute(kAXPositionAttribute)
        .ok()
        .flatten()?;
    let size = element.size_attribute(kAXSizeAttribute).ok().flatten()?;
    Some(Rect {
        x: position.x,
        y: position.y,
        width: size.width,
        height: size.height,
    })
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
    kind: WindowKind,
    minimized: bool,
    fullscreen: bool,
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

    /// Resolves which of `pid`'s windows currently holds AX focus — reads
    /// the app's `AXFocusedWindow` attribute, falling back to `AXMainWindow`
    /// for apps that only expose that one (some single-window apps never
    /// populate `AXFocusedWindow` distinctly). Used to sync `WmState`'s own
    /// focus tracking when the user changes real OS focus by some means
    /// other than tili's own `focus`/`move` commands (a mouse click,
    /// Cmd-Tab, ...) — those never update which window tili itself
    /// considers "current" otherwise, so the next directional command
    /// would act relative to stale state instead of the window the user is
    /// actually looking at.
    pub fn focused_id_for_pid(pid: i32) -> Option<WindowId> {
        let app = AXUIElement::from_pid(pid)?;
        let focused = app
            .element_attribute(axuielement::ffi::kAXFocusedWindowAttribute)
            .ok()
            .flatten()
            .or_else(|| {
                app.element_attribute(axuielement::ffi::kAXMainWindowAttribute)
                    .ok()
                    .flatten()
            })?;
        Self::resolve_window_id(&focused)
    }

    /// The `WindowId` of whatever window genuinely holds OS focus right
    /// now, system-wide, read directly rather than going through "which app
    /// is frontmost" first. That two-hop path (`workspace::frontmost_app_pid()`,
    /// then *that* app's own focused window via `focused_id_for_pid`) misses
    /// a floating panel/utility window belonging to a *different* app than
    /// whatever `NSWorkspace.frontmostApplication` still reports: some
    /// non-activating panels can become the real AX-focused window without
    /// their owning app ever becoming frontmost, so `frontmost_app_pid()`
    /// keeps returning the previously-frontmost app's pid and the focus
    /// sync silently no-ops. Querying the system-wide focus directly
    /// sidesteps the "which app" hop entirely — used by
    /// `WmState::sync_focus_from_frontmost`, the one call site that needs
    /// "whatever's really focused right now" rather than "this specific
    /// pid's focused window" (`apply_windows_changed`'s own re-sync still
    /// wants the latter, since it already knows the exact pid it just
    /// placed windows for).
    ///
    /// Reads `kAXFocusedUIElementAttribute`, not `kAXFocusedWindowAttribute`
    /// — confirmed on real hardware that the system-wide element never
    /// populates the latter at all (`SystemWideElement::focused_window()`
    /// returns `None` on every single call, regardless of what's actually
    /// focused; `kAXFocusedWindowAttribute` is only meaningful queried on a
    /// specific application element, which is exactly what `focused_id_for_pid`
    /// already does). `kAXFocusedUIElementAttribute` is the one attribute
    /// Apple's system-wide object reliably supports, but it can return any
    /// focused control (a text field, a button, ...), not necessarily the
    /// window itself — `resolve_window_id` still resolves it correctly
    /// either way, since `_AXUIElementGetWindow` works on any element and
    /// returns the `CGWindowID` of whatever window contains it.
    pub fn system_focused_id() -> Option<WindowId> {
        let focused = axuielement::system_wide()?
            .focused_ui_element()
            .ok()
            .flatten()?;
        Self::resolve_window_id(&focused)
    }

    /// Builds an `AxWindow` from an `AXUIElement` known to be a window
    /// (typically an entry from an application's `AXWindows` attribute).
    /// `bundle_id` is resolved once per process by the caller (see
    /// `enumerate::list_windows_for_pid`) rather than once per window, to
    /// avoid redundant `NSRunningApplication` lookups for apps with
    /// multiple windows. Returns `None` if the private call can't resolve a
    /// window id, or if `AXRole` is present and isn't `AXWindow` at all
    /// (genuinely not a window) — anything else is kept and classified via
    /// `classify_window_kind` (`Dialog`/`Popup` are tracked too, never
    /// dropped, unlike the old binary `looks_like_a_real_window` check).
    pub fn from_element(element: AXUIElement, pid: i32, bundle_id: Option<String>) -> Option<Self> {
        let role = element.string_attribute(AX_ROLE_ATTRIBUTE).ok().flatten();
        if role.as_deref().is_some_and(|r| r != AX_WINDOW_ROLE) {
            return None;
        }
        let subrole = element
            .string_attribute(AX_SUBROLE_ATTRIBUTE)
            .ok()
            .flatten();
        let has_fullscreen_button = element
            .element_attribute(AX_FULL_SCREEN_BUTTON_ATTRIBUTE)
            .ok()
            .flatten()
            .is_some();
        let has_close_button = element
            .element_attribute(AX_CLOSE_BUTTON_ATTRIBUTE)
            .ok()
            .flatten()
            .is_some();
        let has_zoom_button = element
            .element_attribute(AX_ZOOM_BUTTON_ATTRIBUTE)
            .ok()
            .flatten()
            .is_some();
        let has_any_chrome_button = has_fullscreen_button
            || has_close_button
            || has_zoom_button
            || element
                .element_attribute(AX_MINIMIZE_BUTTON_ATTRIBUTE)
                .ok()
                .flatten()
                .is_some();
        let is_regular_app = crate::workspace::is_regular_app(pid);
        let kind = classify_window_kind(
            subrole.as_deref(),
            has_any_chrome_button,
            has_close_button,
            is_regular_app,
            has_zoom_button,
            has_fullscreen_button,
        );

        let id = Self::resolve_window_id(&element)?;
        let title = element
            .string_attribute(kAXTitleAttribute)
            .ok()
            .flatten()
            .unwrap_or_default();
        let frame = read_frame(&element).unwrap_or(Rect {
            x: 0.0,
            y: 0.0,
            width: 0.0,
            height: 0.0,
        });
        let minimized = element
            .bool_attribute(AX_MINIMIZED_ATTRIBUTE)
            .ok()
            .flatten()
            .unwrap_or(false);
        let fullscreen = element
            .bool_attribute(AX_FULL_SCREEN_ATTRIBUTE)
            .ok()
            .flatten()
            .unwrap_or(false);

        Some(Self {
            id,
            pid,
            bundle_id,
            title,
            frame,
            element,
            kind,
            minimized,
            fullscreen,
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

    /// Reads the window's position/size fresh from AX, bypassing the
    /// cached `frame()` — used right after a `set_frame` write to check
    /// whether the app actually honored the requested size, since some
    /// apps clamp a resize along one axis (e.g. a fixed-width Settings
    /// window) and `set_frame` itself never verifies. Falls back to the
    /// cached frame if the read fails.
    pub fn live_frame(&self) -> Rect {
        read_frame(&self.element).unwrap_or(self.frame)
    }

    /// Corrects the cached `frame()` to match a `live_frame()` read,
    /// without writing anything to AX — for a caller that already
    /// discovered the window's real frame differs from what it last
    /// requested (e.g. `set_size` clamped along one axis) and needs the
    /// cache to reflect reality before a subsequent `WindowFrameSetter`
    /// write compares against it.
    pub fn sync_frame(&mut self, frame: Rect) {
        self.frame = frame;
    }

    pub fn element(&self) -> &AXUIElement {
        &self.element
    }

    pub fn kind(&self) -> WindowKind {
        self.kind
    }

    pub fn minimized(&self) -> bool {
        self.minimized
    }

    pub fn fullscreen(&self) -> bool {
        self.fullscreen
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
    /// invasive to apps that dislike being resized. Same no-op-if-unchanged
    /// guard as `set_frame`, and for the same feedback-loop reason.
    pub fn set_position(&mut self, x: f64, y: f64) {
        if (self.frame.x - x).abs() < FRAME_EPSILON && (self.frame.y - y).abs() < FRAME_EPSILON {
            return;
        }
        let _ = self
            .element
            .set_point_attribute(kAXPositionAttribute, AXPoint { x, y });
        self.frame.x = x;
        self.frame.y = y;
    }

    /// Resizes the window without touching its position — used by
    /// `tili-daemon`'s floating-window centering to discover an app's real
    /// (possibly clamped) size before computing where to center it, so that
    /// step only ever needs one subsequent `set_position` call instead of
    /// moving the window once to a wrong center (computed against the
    /// requested size) and again to the right one (computed against the
    /// real, clamped size) — a visible double-move on every placement of a
    /// fixed-one-axis app like System Settings. Same no-op-if-unchanged
    /// guard as `set_frame`, and for the same feedback-loop reason.
    pub fn set_size(&mut self, width: f64, height: f64) {
        if (self.frame.width - width).abs() < FRAME_EPSILON
            && (self.frame.height - height).abs() < FRAME_EPSILON
        {
            return;
        }
        let _ = self
            .element
            .set_size_attribute(kAXSizeAttribute, AXSize { width, height });
        self.frame.width = width;
        self.frame.height = height;
    }

    /// Raises and focuses this window — used when tiling changes which
    /// window should be frontmost (e.g. after `focus`/`move`).
    /// `kAXRaiseAction`/`kAXFocusedAttribute` alone only raise the window
    /// within its own app's window stack; `activate_app` is what actually
    /// brings a *different* app frontmost, which a plain AX raise can't do.
    pub fn focus(&self) {
        crate::workspace::activate_app(self.pid);
        let _ = self.element.set_bool_attribute(kAXFocusedAttribute, true);
        let _ = self.element.perform_action(kAXRaiseAction);
    }

    /// Closes the window by pressing its `AXCloseButton`, best-effort — a
    /// window with no close button (or that refuses the press) is left
    /// alone, same as every other AX write in this module. Never assumes
    /// the window is actually gone afterward: the caller relies on the
    /// normal destroy-notification path (`WmState::apply_windows_changed`,
    /// with its grace period) to reconcile that once macOS actually reports
    /// it, rather than removing it from any tracking immediately.
    pub fn close(&self) {
        if let Ok(Some(button)) = self.element.element_attribute(AX_CLOSE_BUTTON_ATTRIBUTE) {
            let _ = button.perform_action(kAXPressAction);
        }
    }

    /// Sets macOS's own native fullscreen (a separate Space) via the
    /// `AXFullScreen` boolean attribute — best-effort, and updates the
    /// cached `fullscreen` flag to match immediately, same reasoning as
    /// `set_frame`'s cached-frame update.
    pub fn set_native_fullscreen(&mut self, fullscreen: bool) {
        let _ = self
            .element
            .set_bool_attribute(AX_FULL_SCREEN_ATTRIBUTE, fullscreen);
        self.fullscreen = fullscreen;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dialog_subrole_classifies_as_dialog() {
        assert_eq!(
            classify_window_kind(Some(AX_DIALOG_SUBROLE), false, false, true, false, false),
            WindowKind::Dialog
        );
        assert_eq!(
            classify_window_kind(
                Some(AX_SYSTEM_DIALOG_SUBROLE),
                true,
                false,
                true,
                false,
                false
            ),
            WindowKind::Dialog
        );
    }

    #[test]
    fn standard_subrole_classifies_as_standard() {
        assert_eq!(
            classify_window_kind(
                Some(AX_STANDARD_WINDOW_SUBROLE),
                false,
                false,
                true,
                false,
                false
            ),
            WindowKind::Standard
        );
    }

    #[test]
    fn standard_subrole_with_zoom_but_no_fullscreen_button_is_dialog() {
        // Real content windows are fullscreen-capable; AppKit gives
        // non-fullscreenable utility panels (Preferences/Settings-style
        // windows) a classic zoom button instead — hardware-confirmed on
        // Safari's own Settings window vs. its browser window.
        assert_eq!(
            classify_window_kind(
                Some(AX_STANDARD_WINDOW_SUBROLE),
                true,
                true,
                true,
                true,
                false
            ),
            WindowKind::Dialog
        );
    }

    #[test]
    fn standard_subrole_with_zoom_and_fullscreen_button_is_standard() {
        assert_eq!(
            classify_window_kind(
                Some(AX_STANDARD_WINDOW_SUBROLE),
                true,
                true,
                true,
                true,
                true
            ),
            WindowKind::Standard
        );
    }

    #[test]
    fn missing_subrole_with_chrome_button_ties_break_to_standard() {
        assert_eq!(
            classify_window_kind(None, true, false, true, false, false),
            WindowKind::Standard
        );
    }

    #[test]
    fn missing_subrole_without_chrome_button_is_popup() {
        assert_eq!(
            classify_window_kind(None, false, false, true, false, false),
            WindowKind::Popup
        );
    }

    #[test]
    fn unrecognized_subrole_is_popup() {
        assert_eq!(
            classify_window_kind(Some("AXFloatingWindow"), false, false, true, false, false),
            WindowKind::Popup
        );
    }

    #[test]
    fn non_regular_app_without_close_button_is_popup_even_with_standard_subrole() {
        // The unconditional `activationPolicy == .accessory && closeButton
        // == nil` gate catches Dock context menus, SecurityAgent's keychain
        // prompt, the screenshot toolbar's thumbnail preview, etc.
        // regardless of what subrole they happen to report.
        assert_eq!(
            classify_window_kind(
                Some(AX_STANDARD_WINDOW_SUBROLE),
                true,
                false,
                false,
                false,
                false
            ),
            WindowKind::Popup
        );
    }

    #[test]
    fn non_regular_app_with_close_button_still_classifies_normally() {
        // A menu-bar-only utility app (accessory policy) with a real
        // preference window (has a close button) isn't caught by the gate
        // above — falls through to ordinary subrole-based classification.
        assert_eq!(
            classify_window_kind(
                Some(AX_STANDARD_WINDOW_SUBROLE),
                true,
                true,
                false,
                false,
                false
            ),
            WindowKind::Standard
        );
    }
}
