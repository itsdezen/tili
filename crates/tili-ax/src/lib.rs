pub mod display;
pub mod enumerate;
pub mod frame_setter;
pub mod hotkey;
pub mod mouse;
pub mod watch;
pub mod window;
pub mod workspace;

pub use display::{
    Monitor, combined_bounds, list_monitors, main_display_frame, spawn_display_watcher,
};
pub use enumerate::{list_windows, list_windows_for_pid};
pub use frame_setter::{InstantFrameSetter, WindowFrameSetter};
pub use hotkey::{KeyCombo, parse_key_combo, spawn_hotkey_tap};
pub use mouse::{spawn_mouse_watcher, warp_cursor_to};
pub use watch::{WmEvent, spawn_event_watcher};
pub use window::AxWindow;
pub use workspace::{AppEvent, bundle_id_for_pid, spawn_workspace_watcher};

/// Checks Accessibility permission, prompting the user via the system
/// dialog if it hasn't been granted yet. Every AX call in this crate will
/// silently return empty/error results until this permission is granted, so
/// the daemon calls this once at startup so the prompt actually appears.
pub fn ensure_accessibility_permission() -> bool {
    axuielement::is_process_trusted_with_prompt()
}
