pub mod display;
pub mod enumerate;
pub mod frame_setter;
pub mod hotkey;
pub mod mouse;
pub mod watch;
pub mod window;
pub mod workspace;

pub use display::{
    Monitor, MonitorDiff, list_monitors, main_display_frame, match_monitors, parking_position,
    spawn_display_watcher,
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
pub use workspace::{AppEvent, bundle_id_for_pid, is_app_hidden, register_on_main};

/// Checks Accessibility permission, prompting the user via the system
/// dialog if it hasn't been granted yet. Every AX call in this crate will
/// silently return empty/error results until this permission is granted, so
/// the daemon calls this once at startup so the prompt actually appears.
///
/// Must be called *after* `request_input_monitoring_permission` — see that
/// function's doc comment for why the order matters (rdar://7381305).
///
/// There is deliberately no non-prompting/polling variant of this check —
/// confirmed on real hardware, across several different mechanisms (plain
/// sleep-based polling, a run-loop-serviced polling thread, and re-testing
/// with a stable non-ad-hoc signing identity), that an already-running
/// process never reliably observes a grant made after it started. Only a
/// freshly launched process's own check reflects reality, so the daemon
/// doesn't try to wait/retry in place at all — see the call site in
/// `tili-daemon/src/main.rs`.
pub fn ensure_accessibility_permission() -> bool {
    axuielement::is_process_trusted_with_prompt()
}
