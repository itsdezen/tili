use std::collections::HashMap;

use tili_ipc::{RectInfo, WindowInfo};

struct CachedWindow {
    pid: i32,
    title: String,
    frame: RectInfo,
}

/// Holds the daemon's entire mutable state. Both the socket handler and the
/// (future, M6) global-hotkey handler call `dispatch` against the same
/// `WmState`, so CLI-invoked and hotkey-invoked commands can never behave
/// differently.
///
/// `windows` is a cache kept live by reacting to `tili_ax::WmEvent`s (see
/// `main.rs`), not by re-scanning the system on every read — that's the
/// whole point of M2 event-driven updates over M1's on-demand scan.
#[derive(Default)]
pub struct WmState {
    windows: HashMap<u32, CachedWindow>,
    // TODO(M3+): tree per workspace, monitors, focus state.
}

impl WmState {
    /// Replaces one process's windows in the cache with a freshly-scanned
    /// set, in response to a `WmEvent::WindowsChanged { pid }`. Handles
    /// creation, destruction, move/resize, and title changes uniformly:
    /// whatever isn't in the fresh scan is gone, whatever is stays/updates.
    pub fn apply_windows_changed(&mut self, pid: i32, windows: &[tili_ax::AxWindow]) {
        self.windows.retain(|_, w| w.pid != pid);
        for window in windows {
            let frame = window.frame();
            self.windows.insert(
                window.id(),
                CachedWindow {
                    pid,
                    title: window.title().to_string(),
                    frame: RectInfo {
                        x: frame.x,
                        y: frame.y,
                        width: frame.width,
                        height: frame.height,
                    },
                },
            );
        }
    }

    /// Drops every window belonging to a process that just terminated.
    pub fn remove_app(&mut self, pid: i32) {
        self.windows.retain(|_, w| w.pid != pid);
    }

    pub fn list_windows(&self) -> Vec<WindowInfo> {
        self.windows
            .iter()
            .map(|(&id, w)| WindowInfo {
                id,
                pid: w.pid,
                title: w.title.clone(),
                frame: w.frame,
            })
            .collect()
    }
}
