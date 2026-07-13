use std::collections::HashMap;

use tili_ax::{AxWindow, InstantFrameSetter, WindowFrameSetter};
use tili_ipc::{RectInfo, WindowInfo};
use tili_tree::{Direction, NodeId, Tree, WindowId};

/// Holds the daemon's entire mutable state. Both the socket handler and the
/// (future, M6) global-hotkey handler call `dispatch` against the same
/// `WmState`, so CLI-invoked and hotkey-invoked commands can never behave
/// differently.
///
/// `windows` holds the live `AxWindow` handles themselves (not just cached
/// metadata) — M3 needs the real `AXUIElement` to move/focus a window, and
/// keeping them here means a fresh AX round-trip isn't needed to act on a
/// command. They're kept live the same way M2 already does: replaced
/// wholesale for a process whenever a `WmEvent::WindowsChanged` fires.
pub struct WmState {
    windows: HashMap<WindowId, AxWindow>,
    tree: Tree,
    focused: Option<NodeId>,
    frame_setter: Box<dyn WindowFrameSetter>,
}

impl Default for WmState {
    fn default() -> Self {
        Self {
            windows: HashMap::new(),
            tree: Tree::new(),
            focused: None,
            frame_setter: Box::new(InstantFrameSetter),
        }
    }
}

impl WmState {
    /// Replaces one process's windows with a freshly-scanned set, in
    /// response to a `WmEvent::WindowsChanged { pid }`: whatever isn't in
    /// the fresh scan is gone (removed from the tree too), whatever's new
    /// is tiled in next to the current focus, whatever already existed just
    /// has its cached handle/frame refreshed. Re-lays-out and applies the
    /// result to every real window afterward.
    pub fn apply_windows_changed(&mut self, pid: i32, mut fresh: Vec<AxWindow>) {
        // Zero-size entries are usually menu-extra/phantom AX windows, not
        // real user-facing ones — skip them rather than giving them tiled
        // screen real estate.
        fresh.retain(|w| {
            let frame = w.frame();
            frame.width > 0.0 && frame.height > 0.0
        });
        let fresh_ids: std::collections::HashSet<WindowId> =
            fresh.iter().map(AxWindow::id).collect();

        let stale_ids: Vec<WindowId> = self
            .windows
            .iter()
            .filter(|(id, w)| w.pid() == pid && !fresh_ids.contains(id))
            .map(|(&id, _)| id)
            .collect();
        for id in stale_ids {
            self.windows.remove(&id);
            let suggested_focus = self.tree.remove_window(id);
            if self.focused_window() == Some(id) {
                self.focused = suggested_focus;
            }
        }

        for window in fresh {
            let id = window.id();
            if self.tree.find_node(id).is_none() {
                let node = self.tree.insert_window(id, self.focused);
                if self.focused.is_none() {
                    self.focused = Some(node);
                }
            }
            self.windows.insert(id, window);
        }

        self.relayout();
    }

    /// Drops every window belonging to a process that just terminated.
    pub fn remove_app(&mut self, pid: i32) {
        let ids: Vec<WindowId> = self
            .windows
            .iter()
            .filter(|(_, w)| w.pid() == pid)
            .map(|(&id, _)| id)
            .collect();
        for id in ids {
            self.windows.remove(&id);
            let suggested_focus = self.tree.remove_window(id);
            if self.focused_window() == Some(id) {
                self.focused = suggested_focus;
            }
        }
        self.relayout();
    }

    pub fn list_windows(&self) -> Vec<WindowInfo> {
        self.windows
            .values()
            .map(|w| {
                let frame = w.frame();
                WindowInfo {
                    id: w.id(),
                    pid: w.pid(),
                    title: w.title().to_string(),
                    frame: RectInfo {
                        x: frame.x,
                        y: frame.y,
                        width: frame.width,
                        height: frame.height,
                    },
                }
            })
            .collect()
    }

    /// Moves the focus pointer to the window adjacent to the current focus
    /// in `dir`, and raises/focuses it for real. Returns an error message
    /// (suitable for `Response::Err`) if there's nothing focused yet or
    /// nothing in that direction.
    pub fn focus(&mut self, dir: Direction) -> Result<(), String> {
        let current = self.focused.ok_or("no window is focused")?;
        let target = self
            .tree
            .navigate(current, dir)
            .ok_or("no window in that direction")?;
        self.focused = Some(target);
        self.raise_focused();
        Ok(())
    }

    /// Swaps the focused window with its neighbor in `dir` — the focused
    /// window ends up physically where the neighbor was (and vice versa),
    /// and focus follows it there.
    pub fn move_focused(&mut self, dir: Direction) -> Result<(), String> {
        let current = self.focused.ok_or("no window is focused")?;
        let target = self
            .tree
            .navigate(current, dir)
            .ok_or("no window in that direction")?;
        self.tree.swap_windows(current, target);
        self.focused = Some(target);
        self.relayout();
        self.raise_focused();
        Ok(())
    }

    fn focused_window(&self) -> Option<WindowId> {
        self.focused.and_then(|node| self.tree.window_at(node))
    }

    fn raise_focused(&self) {
        if let Some(id) = self.focused_window()
            && let Some(window) = self.windows.get(&id)
        {
            window.focus();
        }
    }

    /// Recomputes every tiled window's frame and applies it via the
    /// `WindowFrameSetter` seam — never a direct AX call from here.
    fn relayout(&mut self) {
        let area = tili_ax::main_display_frame();
        for (id, rect) in self.tree.layout(area) {
            if let Some(window) = self.windows.get(&id) {
                self.frame_setter.set_frame(window, rect);
            }
        }
    }
}
