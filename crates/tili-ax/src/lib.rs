pub mod enumerate;
pub mod frame_setter;
pub mod window;

pub use enumerate::list_windows;
pub use frame_setter::{InstantFrameSetter, WindowFrameSetter};
pub use window::AxWindow;

/// Checks Accessibility permission, prompting the user via the system
/// dialog if it hasn't been granted yet. Every AX call in this crate will
/// silently return empty/error results until this permission is granted, so
/// the daemon calls this once at startup so the prompt actually appears.
pub fn ensure_accessibility_permission() -> bool {
    axuielement::is_process_trusted_with_prompt()
}
