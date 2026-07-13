use std::collections::HashMap;

use tili_ax::{AxWindow, InstantFrameSetter, WindowFrameSetter};
use tili_ipc::{RectInfo, WindowInfo, WorkspaceInfo};
use tili_tree::{Direction, Gaps, NodeId, Tree, WindowId};

/// The workspace new windows land in, and the one active at daemon startup.
/// Workspaces declared in config (M5) are created up front by `apply_config`;
/// this is just the one that's active before any config or explicit switch
/// says otherwise, and the fallback if config declares none.
const DEFAULT_WORKSPACE: &str = "main";

/// macOS has no public API to enumerate/control Spaces, so workspaces are
/// virtual: only the active one's windows are actually laid out on screen.
/// Every other workspace's windows are "parked" — moved to a coordinate far
/// outside the display's bounds, offset per-window so they don't all stack
/// exactly on top of each other (irrelevant to the user, but keeps
/// `tili list-windows` output sane to eyeball while debugging).
const PARK_MARGIN: f64 = 10_000.0;
const PARK_OFFSET_STEP: f64 = 50.0;

/// Holds the daemon's entire mutable state. Both the socket handler and the
/// (future, M6) global-hotkey handler call `dispatch` against the same
/// `WmState`, so CLI-invoked and hotkey-invoked commands can never behave
/// differently.
///
/// `windows` holds the live `AxWindow` handles themselves (not just cached
/// metadata) across *every* workspace — M3 needs the real `AXUIElement` to
/// move/focus/park a window. Each workspace has its own `Tree`; a window
/// belongs to exactly one workspace's tree at a time. `workspace_focus`
/// remembers the last-focused node per workspace, so switching back to one
/// restores where you left off rather than defaulting to the root every time.
pub struct WmState {
    windows: HashMap<WindowId, AxWindow>,
    workspaces: HashMap<String, Tree>,
    workspace_focus: HashMap<String, NodeId>,
    active_workspace: String,
    frame_setter: Box<dyn WindowFrameSetter>,
    gaps: Gaps,
    workspace_gaps: HashMap<String, Gaps>,
}

impl Default for WmState {
    fn default() -> Self {
        let mut workspaces = HashMap::new();
        workspaces.insert(DEFAULT_WORKSPACE.to_string(), Tree::new());
        Self {
            windows: HashMap::new(),
            workspaces,
            workspace_focus: HashMap::new(),
            active_workspace: DEFAULT_WORKSPACE.to_string(),
            frame_setter: Box::new(InstantFrameSetter),
            gaps: Gaps::default(),
            workspace_gaps: HashMap::new(),
        }
    }
}

impl WmState {
    /// Replaces one process's windows with a freshly-scanned set, in
    /// response to a `WmEvent::WindowsChanged { pid }`: whatever isn't in
    /// the fresh scan is gone (removed from whichever workspace it was in),
    /// whatever's new is tiled into the *active* workspace next to the
    /// current focus, whatever already existed just has its cached
    /// handle/frame refreshed. Re-lays-out the active workspace afterward.
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
            self.remove_from_all_workspaces(id);
        }

        for window in fresh {
            let id = window.id();
            if self.find_workspace_of(id).is_none() {
                let near = self.workspace_focus.get(&self.active_workspace).copied();
                let tree = self.active_tree_mut();
                let node = tree.insert_window(id, near);
                self.workspace_focus
                    .entry(self.active_workspace.clone())
                    .or_insert(node);
            }
            self.windows.insert(id, window);
        }

        self.relayout_active();
    }

    /// Drops every window belonging to a process that just terminated,
    /// wherever (whichever workspace) it was.
    pub fn remove_app(&mut self, pid: i32) {
        let ids: Vec<WindowId> = self
            .windows
            .iter()
            .filter(|(_, w)| w.pid() == pid)
            .map(|(&id, _)| id)
            .collect();
        for id in ids {
            self.windows.remove(&id);
            self.remove_from_all_workspaces(id);
        }
        self.relayout_active();
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

    /// Applies a freshly-loaded (or hot-reloaded) config: updates gaps
    /// (global + per-workspace overrides) and ensures every workspace it
    /// declares exists (creating empty ones as needed) — without switching
    /// to any of them, so a config edit never yanks focus away from
    /// whatever workspace the user is actually looking at. Re-lays-out the
    /// active workspace afterward so a gap change is visible immediately.
    pub fn apply_config(&mut self, config: &tili_config::Config) {
        self.gaps = to_tree_gaps(config.gaps);
        self.workspace_gaps = config
            .workspace_gaps
            .iter()
            .map(|(name, gaps)| (name.clone(), to_tree_gaps(*gaps)))
            .collect();

        for workspace in &config.workspaces {
            self.workspaces.entry(workspace.name.clone()).or_default();
        }

        self.relayout_active();
    }

    pub fn list_workspaces(&self) -> Vec<WorkspaceInfo> {
        let mut workspaces: Vec<WorkspaceInfo> = self
            .workspaces
            .iter()
            .map(|(name, tree)| WorkspaceInfo {
                name: name.clone(),
                active: *name == self.active_workspace,
                window_count: tree.window_ids().len(),
            })
            .collect();
        workspaces.sort_by(|a, b| a.name.cmp(&b.name));
        workspaces
    }

    /// Moves the focus pointer to the window adjacent to the current focus
    /// in `dir`, and raises/focuses it for real. Returns an error message
    /// (suitable for `Response::Err`) if there's nothing focused yet or
    /// nothing in that direction.
    pub fn focus(&mut self, dir: Direction) -> Result<(), String> {
        let current = self.focused_node().ok_or("no window is focused")?;
        let target = self
            .active_tree()
            .navigate(current, dir)
            .ok_or("no window in that direction")?;
        self.set_focused_node(target);
        self.raise_focused();
        Ok(())
    }

    /// Swaps the focused window with its neighbor in `dir` — the focused
    /// window ends up physically where the neighbor was (and vice versa),
    /// and focus follows it there.
    pub fn move_focused(&mut self, dir: Direction) -> Result<(), String> {
        let current = self.focused_node().ok_or("no window is focused")?;
        let target = self
            .active_tree()
            .navigate(current, dir)
            .ok_or("no window in that direction")?;
        self.active_tree_mut().swap_windows(current, target);
        self.set_focused_node(target);
        self.relayout_active();
        self.raise_focused();
        Ok(())
    }

    /// Switches which workspace is active on the (single, until M9) monitor:
    /// parks every window in the outgoing workspace off-screen, lays out
    /// the incoming one for real, and restores its remembered focus.
    /// Creates the target workspace (empty) if it doesn't exist yet.
    /// A no-op if `name` is already active.
    pub fn switch_workspace(&mut self, name: &str) {
        if name == self.active_workspace {
            return;
        }

        let outgoing_ids = self.active_tree().window_ids();
        for (i, id) in outgoing_ids.into_iter().enumerate() {
            self.park(id, i);
        }

        self.active_workspace = name.to_string();
        self.workspaces.entry(name.to_string()).or_default();

        self.relayout_active();

        let restore = self
            .workspace_focus
            .get(&self.active_workspace)
            .copied()
            .or_else(|| self.active_tree().default_focus());
        if let Some(node) = restore {
            self.set_focused_node(node);
            self.raise_focused();
        }
    }

    /// Moves the focused window into a different workspace's tree and
    /// parks it immediately (since, unless `target_name` happens to also be
    /// the active workspace, it's no longer visible). Focus moves to
    /// whatever the active workspace suggests next.
    pub fn move_focused_to_workspace(&mut self, target_name: &str) -> Result<(), String> {
        if target_name == self.active_workspace {
            return Ok(());
        }
        let current_node = self.focused_node().ok_or("no window is focused")?;
        let id = self
            .active_tree()
            .window_at(current_node)
            .ok_or("focused window not found")?;

        let suggested = self.active_tree_mut().remove_window(id);
        match suggested {
            Some(n) => {
                self.workspace_focus
                    .insert(self.active_workspace.clone(), n);
            }
            None => {
                self.workspace_focus.remove(&self.active_workspace);
            }
        }

        let target_focus_hint = self.workspace_focus.get(target_name).copied();
        let target_tree = self.workspaces.entry(target_name.to_string()).or_default();
        let new_node = target_tree.insert_window(id, target_focus_hint);
        self.workspace_focus
            .insert(target_name.to_string(), new_node);

        self.park(id, 0);
        self.relayout_active();
        Ok(())
    }

    fn active_tree(&self) -> &Tree {
        // Always present: every place that changes `active_workspace` also
        // ensures the corresponding entry exists first.
        self.workspaces
            .get(&self.active_workspace)
            .expect("active workspace always has a tree entry")
    }

    fn active_tree_mut(&mut self) -> &mut Tree {
        self.workspaces
            .entry(self.active_workspace.clone())
            .or_default()
    }

    fn find_workspace_of(&self, id: WindowId) -> Option<&str> {
        self.workspaces
            .iter()
            .find(|(_, tree)| tree.find_node(id).is_some())
            .map(|(name, _)| name.as_str())
    }

    fn remove_from_all_workspaces(&mut self, id: WindowId) {
        let Some(name) = self.find_workspace_of(id).map(str::to_string) else {
            return;
        };
        let Some(tree) = self.workspaces.get_mut(&name) else {
            return;
        };
        let removed_leaf = tree.find_node(id);
        let suggested = tree.remove_window(id);
        if removed_leaf.is_some() && self.workspace_focus.get(&name) == removed_leaf.as_ref() {
            match suggested {
                Some(n) => {
                    self.workspace_focus.insert(name, n);
                }
                None => {
                    self.workspace_focus.remove(&name);
                }
            }
        }
    }

    fn focused_node(&self) -> Option<NodeId> {
        self.workspace_focus.get(&self.active_workspace).copied()
    }

    fn set_focused_node(&mut self, node: NodeId) {
        self.workspace_focus
            .insert(self.active_workspace.clone(), node);
    }

    fn raise_focused(&self) {
        if let Some(node) = self.focused_node()
            && let Some(id) = self.active_tree().window_at(node)
            && let Some(window) = self.windows.get(&id)
        {
            window.focus();
        }
    }

    /// Moves a window off-screen without resizing it. `offset_index`
    /// spreads multiple simultaneously-parked windows apart so they don't
    /// all land on the exact same off-screen coordinate.
    fn park(&mut self, id: WindowId, offset_index: usize) {
        let main = tili_ax::main_display_frame();
        let x = main.x + main.width + PARK_MARGIN + (offset_index as f64 * PARK_OFFSET_STEP);
        let y = main.y;
        if let Some(window) = self.windows.get_mut(&id) {
            window.set_position(x, y);
        }
    }

    /// Recomputes every window's frame in the *active* workspace and
    /// applies it via the `WindowFrameSetter` seam — never a direct AX call
    /// from here. Windows in other workspaces are left exactly where they
    /// are (parked), since they're not visible right now.
    fn relayout_active(&mut self) {
        let area = tili_ax::main_display_frame();
        let gaps = self
            .workspace_gaps
            .get(&self.active_workspace)
            .copied()
            .unwrap_or(self.gaps);
        let placements = self.active_tree().layout(area, gaps);
        for (id, rect) in placements {
            if let Some(window) = self.windows.get_mut(&id) {
                self.frame_setter.set_frame(window, rect);
            }
        }
    }
}

fn to_tree_gaps(gaps: tili_config::Gaps) -> Gaps {
    let (top, right, bottom, left) = gaps.outer;
    Gaps {
        inner: f64::from(gaps.inner),
        outer: (
            f64::from(top),
            f64::from(right),
            f64::from(bottom),
            f64::from(left),
        ),
    }
}
