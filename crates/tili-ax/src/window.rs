use axuielement::AXError;
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
use axuielement::{AXCopyMultipleAttributeOptions, AXPoint, AXSize, AXValue};
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

/// The attributes `from_element` asks for in one
/// `AXUIElementCopyMultipleAttributeValues` call, listed in the exact order
/// the returned array carries them: 0 `AXRole`, 1 `AXSubrole`, 2
/// `AXFullScreenButton`, 3 `AXCloseButton`, 4 `AXZoomButton`, 5
/// `AXMinimizeButton`, 6 `AXTitle`, 7 `AXPosition`, 8 `AXSize`, 9
/// `AXMinimized`, 10 `AXFullScreen`. The reply carries no names, only
/// values, so `WindowAttributes::from_batched_values` destructures it
/// positionally — the two have to stay in lockstep.
///
/// `AXCopyMultipleAttributeOptions::NONE` rather than `::STOP_ON_ERROR` is
/// deliberate: a window that simply doesn't have one of the optional
/// attributes comes back with a null/error placeholder in that slot instead
/// of failing the whole batch.
const BATCHED_ATTRIBUTE_NAMES: [&str; 11] = [
    AX_ROLE_ATTRIBUTE,
    AX_SUBROLE_ATTRIBUTE,
    AX_FULL_SCREEN_BUTTON_ATTRIBUTE,
    AX_CLOSE_BUTTON_ATTRIBUTE,
    AX_ZOOM_BUTTON_ATTRIBUTE,
    AX_MINIMIZE_BUTTON_ATTRIBUTE,
    kAXTitleAttribute,
    kAXPositionAttribute,
    kAXSizeAttribute,
    AX_MINIMIZED_ATTRIBUTE,
    AX_FULL_SCREEN_ATTRIBUTE,
];

/// What a window's unreadable frame falls back to — `read_frame` needs both
/// position and size, so either one missing means "no frame" rather than a
/// half-filled rect.
const ZERO_RECT: Rect = Rect {
    x: 0.0,
    y: 0.0,
    width: 0.0,
    height: 0.0,
};

/// Everything `from_element` needs from a window's attributes, read either
/// as one batch (`from_batched_values`) or one attribute at a time
/// (`read_individually`, the fallback). Both paths must agree attribute for
/// attribute — the classification downstream reads this struct, not AX.
#[derive(Debug, PartialEq)]
struct WindowAttributes {
    role: Option<String>,
    subrole: Option<String>,
    title: String,
    frame: Rect,
    minimized: bool,
    fullscreen: bool,
    has_fullscreen_button: bool,
    has_close_button: bool,
    has_zoom_button: bool,
    has_minimize_button: bool,
}

impl WindowAttributes {
    /// Maps one batched read's positional values (see
    /// `BATCHED_ATTRIBUTE_NAMES`) onto a `WindowAttributes`. `None` means the
    /// batch can't be trusted and the caller must fall back to
    /// `read_individually`.
    ///
    /// A length check comes before anything else: the reply is positional,
    /// so a short or long array would silently shift every field into the
    /// wrong slot. There's no way to detect that after the fact, which is
    /// why anything but an exact match is a hard `None` rather than a
    /// best-effort map.
    fn from_batched_values(values: &[AXValue]) -> Option<Self> {
        if values.len() != BATCHED_ATTRIBUTE_NAMES.len() {
            return None;
        }
        let [
            role,
            subrole,
            fullscreen_button,
            close_button,
            zoom_button,
            minimize_button,
            title,
            position,
            size,
            minimized,
            fullscreen,
        ] = values
        else {
            return None;
        };
        // Position and size both have to be readable, matching `read_frame`:
        // one without the other is the same "frame unavailable" case as a
        // window that answers neither.
        let frame = match (position.as_point(), size.as_size()) {
            (Some(position), Some(size)) => Rect {
                x: position.x,
                y: position.y,
                width: size.width,
                height: size.height,
            },
            _ => ZERO_RECT,
        };
        Some(Self {
            role: role.as_string(),
            subrole: subrole.as_string(),
            title: title.as_string().unwrap_or_default(),
            frame,
            minimized: minimized.as_bool().unwrap_or(false),
            fullscreen: fullscreen.as_bool().unwrap_or(false),
            // A chrome button counts as present only when its slot holds a
            // real element. An unsupported attribute comes back as a
            // null/error placeholder, whose `as_element` is `None` — the
            // same answer the old per-attribute read gave when it failed.
            has_fullscreen_button: fullscreen_button.as_element().is_some(),
            has_close_button: close_button.as_element().is_some(),
            has_zoom_button: zoom_button.as_element().is_some(),
            has_minimize_button: minimize_button.as_element().is_some(),
        })
    }

    /// The one-attribute-at-a-time path, used whenever the batch call fails
    /// outright or comes back the wrong shape. Kept because a batch failure
    /// must not turn into a dropped window: this reads exactly what
    /// `from_element` read before the batched path existed.
    fn read_individually(element: &AXUIElement) -> Self {
        Self {
            role: element.string_attribute(AX_ROLE_ATTRIBUTE).ok().flatten(),
            subrole: element
                .string_attribute(AX_SUBROLE_ATTRIBUTE)
                .ok()
                .flatten(),
            title: element
                .string_attribute(kAXTitleAttribute)
                .ok()
                .flatten()
                .unwrap_or_default(),
            frame: read_frame(element).unwrap_or(ZERO_RECT),
            minimized: element
                .bool_attribute(AX_MINIMIZED_ATTRIBUTE)
                .ok()
                .flatten()
                .unwrap_or(false),
            fullscreen: element
                .bool_attribute(AX_FULL_SCREEN_ATTRIBUTE)
                .ok()
                .flatten()
                .unwrap_or(false),
            has_fullscreen_button: has_attribute(element, AX_FULL_SCREEN_BUTTON_ATTRIBUTE),
            has_close_button: has_attribute(element, AX_CLOSE_BUTTON_ATTRIBUTE),
            has_zoom_button: has_attribute(element, AX_ZOOM_BUTTON_ATTRIBUTE),
            has_minimize_button: has_attribute(element, AX_MINIMIZE_BUTTON_ATTRIBUTE),
        }
    }

    /// Whether any of the four window-chrome buttons exists — the ambiguity
    /// tie-break `classify_window_kind` uses for a missing subrole.
    fn has_any_chrome_button(&self) -> bool {
        self.has_fullscreen_button
            || self.has_close_button
            || self.has_zoom_button
            || self.has_minimize_button
    }

    /// Whether `AXRole` was readable and isn't `AXWindow` — a genuinely
    /// non-window element, which `from_element` drops rather than
    /// classifies. An unreadable role stays, same as before.
    fn is_foreign_role(&self) -> bool {
        self.role.as_deref().is_some_and(|r| r != AX_WINDOW_ROLE)
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

/// Messaging timeout applied to every `AXUIElement` app connection this
/// crate opens (`AXUIElementSetMessagingTimeout`, `AXUIElement::set_timeout`)
/// — without it, a call into an unresponsive/hung app's AX connection blocks
/// its calling thread indefinitely, which for the synchronous calls made
/// directly from `tili-daemon`'s single event loop (`enumerate::list_windows_for_pid`
/// in particular) means one hung app freezes the entire daemon. Short enough
/// that a genuinely hung app doesn't stall the daemon noticeably, long
/// enough not to false-positive on an app that's merely doing legitimate
/// slow work when queried.
pub(crate) const AX_CALL_TIMEOUT_SECS: f32 = 1.0;

/// Best-effort: a failure to set the timeout just means this connection
/// keeps the (effectively unbounded) default — not worth failing the whole
/// lookup over.
pub(crate) fn apply_call_timeout(app: &AXUIElement) {
    let _ = app.set_timeout(AX_CALL_TIMEOUT_SECS);
}

/// Whether `element` has a given child attribute at all (e.g. a chrome
/// button) — the read succeeding with a value is all that's checked here,
/// the value itself is discarded.
fn has_attribute(element: &AXUIElement, attr: &str) -> bool {
    element.element_attribute(attr).ok().flatten().is_some()
}

pub(crate) fn frame_matches(a: Rect, b: Rect) -> bool {
    (a.x - b.x).abs() < FRAME_EPSILON
        && (a.y - b.y).abs() < FRAME_EPSILON
        && (a.width - b.width).abs() < FRAME_EPSILON
        && (a.height - b.height).abs() < FRAME_EPSILON
}

/// Logs an AX write failure, except `InvalidUIElement` — that's just the
/// window having closed out from under the write (the normal
/// destroy-notification path reconciles it separately), not something
/// worth noise for.
fn log_write_failure(what: &str, title: &str, err: AXError) {
    if err != AXError::InvalidUIElement {
        eprintln!("tili-ax: {what} write failed for {title:?}: {err}");
    }
}

/// Reads a window's current position/size straight from AX — `None` if
/// either attribute is unreadable. Shared by `WindowAttributes::read_individually`
/// (the batch-read fallback) and `AxWindow::live_frame` (a post-write check),
/// so both agree on exactly what "the real frame" means.
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
    has_zoom_button: bool,
    is_resizable: bool,
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
        apply_call_timeout(&app);
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
    /// `bundle_id` and `is_regular_app` are both resolved once per process by
    /// the caller (see `enumerate::list_windows_for_pid`) rather than once
    /// per window, to avoid redundant `NSRunningApplication` lookups for apps
    /// with multiple windows. Returns `None` if the private call can't resolve
    /// a window id, or if `AXRole` is present and isn't `AXWindow` at all
    /// (genuinely not a window) — anything else is kept and classified via
    /// `classify_window_kind` (`Dialog`/`Popup` are tracked too, never
    /// dropped, unlike the old binary `looks_like_a_real_window` check).
    ///
    /// Every attribute read is one batched
    /// `AXUIElementCopyMultipleAttributeValues` call (see
    /// `BATCHED_ATTRIBUTE_NAMES`), with the window-id resolution and the
    /// `kAXSizeAttribute` settability query kept separate — those aren't
    /// attribute values. A batch read that fails or returns the wrong shape
    /// falls back to `WindowAttributes::read_individually`, so a window is
    /// never lost to a failed batch.
    pub fn from_element(
        element: AXUIElement,
        pid: i32,
        bundle_id: Option<String>,
        is_regular_app: bool,
    ) -> Option<Self> {
        let attributes = element
            .copy_multiple_attribute_values(
                &BATCHED_ATTRIBUTE_NAMES,
                AXCopyMultipleAttributeOptions::NONE,
            )
            .ok()
            .and_then(|values| WindowAttributes::from_batched_values(&values))
            .unwrap_or_else(|| WindowAttributes::read_individually(&element));
        if attributes.is_foreign_role() {
            return None;
        }
        let kind = classify_window_kind(
            attributes.subrole.as_deref(),
            attributes.has_any_chrome_button(),
            attributes.has_close_button,
            is_regular_app,
            attributes.has_zoom_button,
            attributes.has_fullscreen_button,
        );
        // Deliberately independent of `kind`: subrole/chrome-button shape
        // says what *category* of window this claims to be, not whether it
        // can actually be resized — an Electron splash screen or a popup
        // torn off a main window commonly reports `AXStandardWindow` with a
        // close button (so classifies as `Standard`) while still refusing
        // any size write. `AXUIElementIsAttributeSettable` is the one API
        // that asks the app directly rather than inferring from shape, so
        // it applies uniformly across apps instead of needing a per-app
        // rule. `Err` (attribute unqueryable) defaults to `true`: wrongly
        // giving up on tiling a genuinely resizable window is worse than
        // occasionally missing a truly non-resizable one.
        let is_resizable = element
            .is_attribute_settable(kAXSizeAttribute)
            .unwrap_or(true);

        let id = Self::resolve_window_id(&element)?;

        Some(Self {
            id,
            pid,
            bundle_id,
            title: attributes.title,
            frame: attributes.frame,
            element,
            kind,
            has_zoom_button: attributes.has_zoom_button,
            is_resizable,
            minimized: attributes.minimized,
            fullscreen: attributes.fullscreen,
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

    /// Whether this window still exists at all, asked of the WindowServer
    /// directly (`_AXUIElementGetWindow` on the cached element) rather than
    /// inferred from an `AXWindows` scan.
    ///
    /// `kAXWindowsAttribute` only reports the windows on the macOS Space
    /// that's currently active, so a window absent from
    /// `enumerate::list_windows_for_pid`'s result is not proof it closed —
    /// it may simply have been left behind when something (a native
    /// fullscreen transition, most commonly) made a different Space active.
    /// The same goes for that function returning an empty `Vec` because the
    /// `AXWindows` read itself failed or timed out. `_AXUIElementGetWindow`
    /// resolves a `CGWindowID` for any live window regardless of which Space
    /// it's on, and fails once the element is genuinely destroyed, so it's
    /// the one signal that actually distinguishes the two.
    pub fn still_exists(&self) -> bool {
        Self::resolve_window_id(&self.element).is_some()
    }

    /// A `Send`, cheaply-cloneable (a CFType retain) handle for probing
    /// this window's AX responsiveness from another thread — see
    /// `WindowProbeHandle`. Exists because `AxWindow` itself borrows into
    /// `WmState`'s `self.windows` and isn't meant to cross threads, but a
    /// post-wake probe needs to fire concurrently from
    /// `tokio::task::spawn_blocking`.
    pub fn probe_handle(&self) -> WindowProbeHandle {
        WindowProbeHandle {
            element: self.element.clone(),
        }
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

    /// Whether the window reported an `AXZoomButton` at classification
    /// time — the exact signal `classify_window_kind` uses to promote a
    /// standard-subrole window to `Dialog` for Preferences/Settings-style
    /// windows. Exposed so callers can tell that case apart from a
    /// `Dialog` reached via subrole match alone (see
    /// `is_transient_empty_dialog` in `tili-daemon`).
    pub fn has_zoom_button(&self) -> bool {
        self.has_zoom_button
    }

    /// Whether `kAXSizeAttribute` reported as settable at classification
    /// time — see the doc comment on its computation in `from_element`.
    /// Used by `tili-daemon` to force windows that can't actually be
    /// resized to `Ignore` regardless of `kind` or any matching floating
    /// rule (see `is_non_resizable_window`).
    pub fn is_resizable(&self) -> bool {
        self.is_resizable
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
    /// they ended up. The cache is only advanced for a write that actually
    /// succeeded: an app that's temporarily unresponsive (e.g. right after
    /// screen unlock) can fail an AX write outright, and if the cache
    /// optimistically moved to `target` anyway, the no-op guard below would
    /// wrongly believe a *later* call already matches reality and skip the
    /// write forever — the window stays stuck wherever it last really was
    /// until something else (a full resync) happens to fix the cache.
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
        match self.element.set_point_attribute(
            kAXPositionAttribute,
            AXPoint {
                x: target.x,
                y: target.y,
            },
        ) {
            Ok(()) => {
                self.frame.x = target.x;
                self.frame.y = target.y;
            }
            Err(e) => log_write_failure("set_frame position", &self.title, e),
        }
        match self.element.set_size_attribute(
            kAXSizeAttribute,
            AXSize {
                width: target.width,
                height: target.height,
            },
        ) {
            Ok(()) => {
                self.frame.width = target.width;
                self.frame.height = target.height;
            }
            Err(e) => log_write_failure("set_frame size", &self.title, e),
        }
    }

    /// Moves the window without touching its size — used to park a window
    /// off-screen (M4), where resizing would be pointless and needlessly
    /// invasive to apps that dislike being resized. Same no-op-if-unchanged
    /// guard as `set_frame`, and for the same feedback-loop reason. Same
    /// cache-only-advances-on-success rule as `set_frame`, and for the same
    /// stuck-forever reason.
    pub fn set_position(&mut self, x: f64, y: f64) {
        if (self.frame.x - x).abs() < FRAME_EPSILON && (self.frame.y - y).abs() < FRAME_EPSILON {
            return;
        }
        match self
            .element
            .set_point_attribute(kAXPositionAttribute, AXPoint { x, y })
        {
            Ok(()) => {
                self.frame.x = x;
                self.frame.y = y;
            }
            Err(e) => log_write_failure("set_position", &self.title, e),
        }
    }

    /// Resizes the window without touching its position — used by
    /// `tili-daemon`'s floating-window centering to discover an app's real
    /// (possibly clamped) size before computing where to center it, so that
    /// step only ever needs one subsequent `set_position` call instead of
    /// moving the window once to a wrong center (computed against the
    /// requested size) and again to the right one (computed against the
    /// real, clamped size) — a visible double-move on every placement of a
    /// fixed-one-axis app like System Settings. Same no-op-if-unchanged
    /// guard as `set_frame`, and for the same feedback-loop reason. Same
    /// cache-only-advances-on-success rule as `set_frame`, and for the same
    /// stuck-forever reason.
    pub fn set_size(&mut self, width: f64, height: f64) {
        if (self.frame.width - width).abs() < FRAME_EPSILON
            && (self.frame.height - height).abs() < FRAME_EPSILON
        {
            return;
        }
        match self
            .element
            .set_size_attribute(kAXSizeAttribute, AXSize { width, height })
        {
            Ok(()) => {
                self.frame.width = width;
                self.frame.height = height;
            }
            Err(e) => log_write_failure("set_size", &self.title, e),
        }
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

/// An owned handle for probing one window's AX responsiveness from a
/// background thread — see `AxWindow::probe_handle`. Deliberately doesn't
/// expose the underlying `AXUIElement` type at all, so `tili-daemon` never
/// needs to name it directly (this crate stays the only one that touches
/// the Accessibility API).
#[derive(Clone)]
pub struct WindowProbeHandle {
    element: AXUIElement,
}

impl WindowProbeHandle {
    /// Cheap AX round-trip confirming the window's owning process has
    /// reconnected to AX/WindowServer after a system wake — reads
    /// `kAXPositionAttribute` and discards the value, since only whether
    /// the read succeeds at all matters here. A synchronous IPC round-trip
    /// to the owning process, so callers should run this off the async
    /// runtime thread (`tokio::task::spawn_blocking`), one per window,
    /// fired concurrently right after wake rather than waiting for the
    /// next ordinary AX event for that window to arrive.
    pub fn is_responsive(&self) -> bool {
        self.element
            .point_attribute(kAXPositionAttribute)
            .is_ok_and(|v| v.is_some())
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

    /// A batched-value slice with the shape a
    /// `AXCopyMultipleAttributeOptions::NONE` read produces for a window that
    /// has none of the optional attributes: an error placeholder in every
    /// slot.
    fn placeholder_values() -> Vec<AXValue> {
        BATCHED_ATTRIBUTE_NAMES
            .iter()
            .map(|_| AXValue::from_error(AXError::NoValue).expect("error placeholder"))
            .collect()
    }

    /// Overwrites the slot `name` occupies, found through
    /// `BATCHED_ATTRIBUTE_NAMES` so a test never hardcodes an index that
    /// could drift from the real attribute order.
    fn set(values: &mut [AXValue], name: &str, value: AXValue) {
        let index = BATCHED_ATTRIBUTE_NAMES
            .iter()
            .position(|candidate| *candidate == name)
            .expect("attribute name is part of the batch");
        values[index] = value;
    }

    #[test]
    fn batched_values_map_a_full_window() {
        let mut values = placeholder_values();
        set(
            &mut values,
            AX_ROLE_ATTRIBUTE,
            AXValue::from_string(AX_WINDOW_ROLE).unwrap(),
        );
        set(
            &mut values,
            AX_SUBROLE_ATTRIBUTE,
            AXValue::from_string(AX_STANDARD_WINDOW_SUBROLE).unwrap(),
        );
        set(
            &mut values,
            kAXTitleAttribute,
            AXValue::from_string("Inbox").unwrap(),
        );
        set(
            &mut values,
            kAXPositionAttribute,
            AXValue::from_point(AXPoint { x: 120.0, y: 48.0 }).unwrap(),
        );
        set(
            &mut values,
            kAXSizeAttribute,
            AXValue::from_size(AXSize {
                width: 800.0,
                height: 600.0,
            })
            .unwrap(),
        );
        set(
            &mut values,
            AX_MINIMIZED_ATTRIBUTE,
            AXValue::from_bool(true),
        );
        set(
            &mut values,
            AX_FULL_SCREEN_ATTRIBUTE,
            AXValue::from_bool(true),
        );

        let attributes = WindowAttributes::from_batched_values(&values).expect("well-formed batch");

        assert_eq!(attributes.role.as_deref(), Some(AX_WINDOW_ROLE));
        assert_eq!(
            attributes.subrole.as_deref(),
            Some(AX_STANDARD_WINDOW_SUBROLE)
        );
        assert_eq!(attributes.title, "Inbox");
        assert_eq!(
            attributes.frame,
            Rect {
                x: 120.0,
                y: 48.0,
                width: 800.0,
                height: 600.0,
            }
        );
        assert!(attributes.minimized);
        assert!(attributes.fullscreen);
        assert!(!attributes.is_foreign_role());
        // A present chrome button is an `AXValue` of kind `Element`, which
        // only `AXValue::from_element` can create — and that needs a live
        // `AXUIElement`, so it isn't reachable from a unit test. The
        // present case is covered by the real AX path; absent is covered
        // here and by `batched_error_placeholders_map_to_defaults`.
        assert!(!attributes.has_any_chrome_button());
    }

    #[test]
    fn batched_error_placeholders_map_to_defaults() {
        // Every optional attribute missing: the batch still maps, with the
        // same defaults the per-attribute path produced (`""` title, zero
        // frame, `false` booleans, no chrome buttons) rather than a dropped
        // window.
        let attributes = WindowAttributes::from_batched_values(&placeholder_values())
            .expect("well-formed batch");

        assert_eq!(attributes.role, None);
        assert_eq!(attributes.subrole, None);
        assert_eq!(attributes.title, "");
        assert_eq!(attributes.frame, ZERO_RECT);
        assert!(!attributes.minimized);
        assert!(!attributes.fullscreen);
        assert!(!attributes.has_fullscreen_button);
        assert!(!attributes.has_close_button);
        assert!(!attributes.has_zoom_button);
        assert!(!attributes.has_minimize_button);
        assert!(!attributes.has_any_chrome_button());
        assert!(!attributes.is_foreign_role());
    }

    #[test]
    fn batched_position_without_size_yields_zero_frame() {
        // `read_frame` requires both halves, so a batch carrying only one is
        // "no frame", not a rect with a zero width/height — either way round.
        let mut position_only = placeholder_values();
        set(
            &mut position_only,
            kAXPositionAttribute,
            AXValue::from_point(AXPoint { x: 120.0, y: 48.0 }).unwrap(),
        );
        let attributes =
            WindowAttributes::from_batched_values(&position_only).expect("well-formed batch");
        assert_eq!(attributes.frame, ZERO_RECT);

        let mut size_only = placeholder_values();
        set(
            &mut size_only,
            kAXSizeAttribute,
            AXValue::from_size(AXSize {
                width: 800.0,
                height: 600.0,
            })
            .unwrap(),
        );
        let attributes =
            WindowAttributes::from_batched_values(&size_only).expect("well-formed batch");
        assert_eq!(attributes.frame, ZERO_RECT);
    }

    #[test]
    fn batched_values_of_the_wrong_length_are_rejected() {
        // Not a best-effort map: the reply is positional, so a shifted array
        // would put every value in the wrong field. `None` is what sends
        // `from_element` down `read_individually`, which is why a rejected
        // batch can never change classification.
        let mut short = placeholder_values();
        short.pop();
        assert_eq!(WindowAttributes::from_batched_values(&short), None);

        let mut long = placeholder_values();
        long.push(AXValue::from_bool(false));
        assert_eq!(WindowAttributes::from_batched_values(&long), None);

        assert_eq!(WindowAttributes::from_batched_values(&[]), None);
    }

    #[test]
    fn batched_non_window_role_is_foreign() {
        let mut values = placeholder_values();
        set(
            &mut values,
            AX_ROLE_ATTRIBUTE,
            AXValue::from_string("AXButton").unwrap(),
        );

        let attributes = WindowAttributes::from_batched_values(&values).expect("well-formed batch");

        // The one role outcome `from_element` drops instead of classifying.
        assert!(attributes.is_foreign_role());
    }
}
