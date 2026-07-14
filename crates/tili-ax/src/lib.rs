pub mod display;
pub mod enumerate;
pub mod frame_setter;
pub mod hotkey;
pub mod mouse;
pub mod watch;
pub mod window;
pub mod workspace;

pub use display::{
    Monitor, MonitorDiff, choose_parking_corner, combined_bounds, list_monitors,
    main_display_frame, match_monitors, spawn_display_watcher,
};
pub use enumerate::{list_windows, list_windows_for_pid};
pub use frame_setter::{InstantFrameSetter, WindowFrameSetter};
pub use hotkey::{
    KeyCombo, has_input_monitoring_permission, parse_key_combo,
    request_input_monitoring_permission, spawn_hotkey_tap,
};
pub use mouse::{MouseSignal, spawn_mouse_watcher, warp_cursor_to};
pub use watch::{WmEvent, spawn_event_watcher};
pub use window::{AxWindow, WindowKind};
pub use workspace::{AppEvent, bundle_id_for_pid, is_app_hidden, spawn_workspace_watcher};

/// Checks Accessibility permission, prompting the user via the system
/// dialog if it hasn't been granted yet. Every AX call in this crate will
/// silently return empty/error results until this permission is granted, so
/// the daemon calls this once at startup so the prompt actually appears.
///
/// Must be called *after* `request_input_monitoring_permission` — see that
/// function's doc comment for why the order matters (rdar://7381305).
pub fn ensure_accessibility_permission() -> bool {
    axuielement::is_process_trusted_with_prompt()
}

/// Non-prompting Accessibility check, for polling whether a previously
/// missing grant has since been made — `ensure_accessibility_permission`
/// only needs calling once to trigger the dialog; this is what a caller
/// polls afterward without re-triggering it.
///
/// Deliberately goes through `AXIsProcessTrustedWithOptions` (with the
/// prompt option off) rather than the older, simpler `AXIsProcessTrusted`
/// (`axuielement::is_process_trusted`) — the latter is known to return a
/// stale/cached result within an already-running process instead of
/// re-querying TCC live, which would make a polling loop never notice a
/// permission granted after the process started.
pub fn has_accessibility_permission() -> bool {
    axuielement::is_process_trusted_with_options(axuielement::ProcessTrustOptions::new())
}
