use std::collections::{HashMap, HashSet};

use regex::Regex;
use tili_ax::{AxWindow, InstantFrameSetter, KeyCombo, Monitor, WindowFrameSetter};
use tili_ipc::{Command, MonitorInfo, RectInfo, WindowInfo, WorkspaceInfo};
use tili_tree::{Direction, Gaps, NodeId, Rect, Tree, WindowId};

/// The workspace new windows land in, and the one active at daemon startup.
/// Workspaces declared in config (M5) are created up front by `apply_config`;
/// this is just the one that's active before any config or explicit switch
/// says otherwise, and the fallback if config declares none.
const DEFAULT_WORKSPACE: &str = "main";

/// The keybinding mode active before any config declares modes, and the one
/// `ModeExit` always returns to. Doesn't need to exist in `mode_bindings` —
/// a config with no `keybindings mode="main" { ... }` block just means no
/// hotkeys are bound yet, not an error.
const DEFAULT_MODE: &str = "main";

/// macOS has no public API to enumerate/control Spaces, so workspaces are
/// virtual: only the active one's windows are actually laid out on screen.
/// Every other workspace's windows are "parked" — moved to a coordinate far
/// outside the display's bounds, offset per-window so they don't all stack
/// exactly on top of each other (irrelevant to the user, but keeps
/// `tili list-windows` output sane to eyeball while debugging).
const PARK_MARGIN: f64 = 10_000.0;
const PARK_OFFSET_STEP: f64 = 50.0;

/// Which workspace a window belongs to, and whether it's tiled (part of
/// that workspace's `Tree`) or floating (positioned once, outside the
/// tree). Indexed by `WindowId` in `WmState::placements` so "which
/// workspace owns this window" is an O(1) lookup instead of scanning every
/// workspace's tree (M4 through M7 did the latter).
struct Placement {
    workspace: String,
    floating: bool,
}

/// A `tili_config::FloatingRule` with its title pattern pre-compiled —
/// done once in `apply_config`, not on every window creation.
struct CompiledFloatingRule {
    app_id: String,
    title: Option<Regex>,
    width: Option<u32>,
    height: Option<u32>,
    center: Option<bool>,
}

/// Holds the daemon's entire mutable state. Both the socket handler and the
/// global-hotkey handler call `dispatch` against the same `WmState`, so
/// CLI-invoked and hotkey-invoked commands can never behave differently.
///
/// `windows` holds the live `AxWindow` handles themselves (not just cached
/// metadata) across *every* workspace — M3 needs the real `AXUIElement` to
/// move/focus/park a window. Each workspace has its own `Tree` for its
/// tiled windows; floating windows (M8) sit outside every `Tree`, tracked
/// only via `placements`. `workspace_focus` remembers the last-focused
/// node per workspace, so switching back to one restores where you left
/// off rather than defaulting to the root every time.
///
/// M9: each connected monitor shows at most one workspace at a time
/// (`active_workspace: monitor id -> workspace name` — a workspace absent
/// from this map is parked, wherever it last was). `focused_monitor` is
/// which one `Focus`/`Move`/`WorkspaceSwitch`/etc. target; `FocusMonitor`
/// is the only thing that changes it. Config-driven workspace-to-monitor
/// pinning (`WorkspaceConfig.monitor`) is intentionally left unwired here —
/// M9's bar is hot-plug/unplug safety, not that finer-grained UX.
pub struct WmState {
    windows: HashMap<WindowId, AxWindow>,
    placements: HashMap<WindowId, Placement>,
    workspaces: HashMap<String, Tree>,
    workspace_focus: HashMap<String, NodeId>,
    monitors: Vec<Monitor>,
    active_workspace: HashMap<u32, String>,
    focused_monitor: u32,
    frame_setter: Box<dyn WindowFrameSetter>,
    gaps: Gaps,
    workspace_gaps: HashMap<String, Gaps>,
    current_mode: String,
    /// mode name -> (key combo -> command), built fresh from config's
    /// `keybindings` on every `apply_config`.
    mode_bindings: HashMap<String, HashMap<KeyCombo, Command>>,
    /// Matched in order — first match wins.
    floating_rules: Vec<CompiledFloatingRule>,
    floating_defaults: tili_config::FloatingDefaults,
}

impl Default for WmState {
    fn default() -> Self {
        let mut workspaces = HashMap::new();
        workspaces.insert(DEFAULT_WORKSPACE.to_string(), Tree::new());

        let monitors = tili_ax::list_monitors();
        let focused_monitor = monitors.first().map(|m| m.id).unwrap_or(0);
        let mut active_workspace = HashMap::new();
        active_workspace.insert(focused_monitor, DEFAULT_WORKSPACE.to_string());

        Self {
            windows: HashMap::new(),
            placements: HashMap::new(),
            workspaces,
            workspace_focus: HashMap::new(),
            monitors,
            active_workspace,
            focused_monitor,
            frame_setter: Box::new(InstantFrameSetter),
            gaps: Gaps::default(),
            workspace_gaps: HashMap::new(),
            current_mode: DEFAULT_MODE.to_string(),
            mode_bindings: HashMap::new(),
            floating_rules: Vec::new(),
            floating_defaults: tili_config::FloatingDefaults::default(),
        }
    }
}

impl WmState {
    /// Replaces one process's windows with a freshly-scanned set, in
    /// response to a `WmEvent::WindowsChanged { pid }`: whatever isn't in
    /// the fresh scan is gone (removed from whichever workspace/placement
    /// it had), whatever's new either joins the *active* workspace's tiled
    /// tree next to the current focus, or — if it matches a floating rule
    /// (M8) — gets centered/sized once and left out of the tree entirely.
    /// Whatever already existed just has its cached handle/frame
    /// refreshed. Re-lays-out the active workspace's tiled windows
    /// afterward.
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
            self.remove_placement(id);
        }

        let active_workspace = self.active_workspace_name();
        for window in fresh {
            let id = window.id();
            let is_new = !self.placements.contains_key(&id);
            let floating_frame = if is_new {
                self.compute_floating_frame(&window)
            } else {
                None
            };

            self.windows.insert(id, window);

            if !is_new {
                continue;
            }

            if let Some(frame) = floating_frame {
                self.placements.insert(
                    id,
                    Placement {
                        workspace: active_workspace.clone(),
                        floating: true,
                    },
                );
                if let Some(w) = self.windows.get_mut(&id) {
                    self.frame_setter.set_frame(w, frame);
                }
            } else {
                let near = self.workspace_focus.get(&active_workspace).copied();
                let node = self.active_tree_mut().insert_window(id, near);
                self.workspace_focus
                    .entry(active_workspace.clone())
                    .or_insert(node);
                self.placements.insert(
                    id,
                    Placement {
                        workspace: active_workspace.clone(),
                        floating: false,
                    },
                );
            }
        }

        self.relayout_active();
    }

    /// Drops every window belonging to a process that just terminated,
    /// wherever (whichever workspace, tiled or floating) it was.
    pub fn remove_app(&mut self, pid: i32) {
        let ids: Vec<WindowId> = self
            .windows
            .iter()
            .filter(|(_, w)| w.pid() == pid)
            .map(|(&id, _)| id)
            .collect();
        for id in ids {
            self.windows.remove(&id);
            self.remove_placement(id);
        }
        self.relayout_all_visible();
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
                    floating: self.placements.get(&w.id()).is_some_and(|p| p.floating),
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
    /// (global + per-workspace overrides), rebuilds the keybinding table,
    /// recompiles floating rules (M8 — an invalid title regex logs a
    /// warning and drops just that rule, not the whole config), and
    /// ensures every workspace it declares exists (creating empty ones as
    /// needed) — without switching to any of them, so a config edit never
    /// yanks focus away from whatever workspace the user is actually
    /// looking at. Re-lays-out the active workspace afterward so a gap
    /// change is visible immediately.
    ///
    /// Floating rules only apply to windows created *after* this call —
    /// an already-tiled window that a newly-added rule would now match
    /// stays tiled until it's recreated (matches the M8 acceptance bar:
    /// "auto-center/size on window creation").
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

        self.mode_bindings = config
            .keybindings
            .iter()
            .map(|mode| {
                let bindings = mode
                    .bindings
                    .iter()
                    .filter_map(|kb| {
                        let combo = tili_ax::parse_key_combo(&kb.key)?;
                        Some((combo, tili_ipc::parse(&kb.command)))
                    })
                    .collect();
                (mode.name.clone(), bindings)
            })
            .collect();
        // A reload that drops the mode we were in (e.g. the config no
        // longer declares it) falls back to the default rather than
        // leaving hotkeys pointing at a table that no longer exists.
        if self.current_mode != DEFAULT_MODE && !self.mode_bindings.contains_key(&self.current_mode)
        {
            self.current_mode = DEFAULT_MODE.to_string();
        }

        self.floating_rules = config
            .floating_rules
            .iter()
            .filter_map(|rule| {
                let title = match &rule.title {
                    Some(pattern) => match Regex::new(pattern) {
                        Ok(re) => Some(re),
                        Err(e) => {
                            eprintln!(
                                "tili-daemon: skipping floating rule for '{}' — invalid title regex '{pattern}': {e}",
                                rule.app_id
                            );
                            return None;
                        }
                    },
                    None => None,
                };
                Some(CompiledFloatingRule {
                    app_id: rule.app_id.clone(),
                    title,
                    width: rule.width,
                    height: rule.height,
                    center: rule.center,
                })
            })
            .collect();
        self.floating_defaults = config.floating_defaults;

        self.relayout_all_visible();
    }

    /// Switches which mode's keybindings are active. Returns an error if
    /// `name` isn't a mode the config declares (the default mode, `"main"`,
    /// is always valid even with no keybindings configured for it yet).
    pub fn enter_mode(&mut self, name: &str) -> Result<(), String> {
        if name == DEFAULT_MODE || self.mode_bindings.contains_key(name) {
            self.current_mode = name.to_string();
            Ok(())
        } else {
            Err(format!("unknown keybinding mode '{name}'"))
        }
    }

    pub fn exit_mode(&mut self) {
        self.current_mode = DEFAULT_MODE.to_string();
    }

    /// Looks up the `Command` bound to `combo` in the current mode, if any
    /// — called when a hotkey press arrives from `tili_ax::spawn_hotkey_tap`.
    pub fn resolve_hotkey(&self, combo: KeyCombo) -> Option<Command> {
        self.mode_bindings
            .get(&self.current_mode)?
            .get(&combo)
            .cloned()
    }

    /// Every key combo bound in the current mode — kept in sync with the
    /// `Arc<Mutex<_>>` the hotkey tap reads synchronously (see
    /// `tili_ax::spawn_hotkey_tap`'s docs for why that's a `Mutex` and not
    /// routed through this state's normal single-owner-loop model).
    pub fn active_key_combos(&self) -> HashSet<KeyCombo> {
        self.mode_bindings
            .get(&self.current_mode)
            .map(|bindings| bindings.keys().copied().collect())
            .unwrap_or_default()
    }

    pub fn list_workspaces(&self) -> Vec<WorkspaceInfo> {
        let mut workspaces: Vec<WorkspaceInfo> = self
            .workspaces
            .iter()
            .map(|(name, tree)| WorkspaceInfo {
                name: name.clone(),
                active: self.active_workspace.values().any(|n| n == name),
                window_count: tree.window_ids().len(),
                monitor: self
                    .active_workspace
                    .iter()
                    .find(|(_, n)| *n == name)
                    .map(|(&id, _)| id),
            })
            .collect();
        workspaces.sort_by(|a, b| a.name.cmp(&b.name));
        workspaces
    }

    /// Every connected monitor and what it's currently showing (M9).
    pub fn list_monitors(&self) -> Vec<MonitorInfo> {
        self.monitors
            .iter()
            .map(|m| MonitorInfo {
                id: m.id,
                is_main: m.is_main,
                focused: m.id == self.focused_monitor,
                active_workspace: self.active_workspace.get(&m.id).cloned(),
                frame: RectInfo {
                    x: m.frame.x,
                    y: m.frame.y,
                    width: m.frame.width,
                    height: m.frame.height,
                },
            })
            .collect()
    }

    /// Cycles `focused_monitor` to the next connected monitor, wrapping —
    /// a no-op with fewer than two monitors connected.
    pub fn focus_monitor_next(&mut self) {
        if self.monitors.len() < 2 {
            return;
        }
        let ids: Vec<u32> = self.monitors.iter().map(|m| m.id).collect();
        let current_idx = ids
            .iter()
            .position(|&id| id == self.focused_monitor)
            .unwrap_or(0);
        self.focused_monitor = ids[(current_idx + 1) % ids.len()];
    }

    /// Re-enumerates connected monitors in response to a hot-plug/unplug
    /// signal from `tili_ax::spawn_display_watcher`. A disconnected
    /// monitor's active workspace is parked (its windows aren't lost, just
    /// no longer shown anywhere, exactly like switching away from it); a
    /// newly connected monitor gets a fresh, empty workspace. Every
    /// still-visible workspace is re-laid-out afterward since frames may
    /// have changed even for monitors that stayed connected (resolution or
    /// arrangement change).
    pub fn on_displays_changed(&mut self) {
        let new_monitors = tili_ax::list_monitors();
        let new_ids: HashSet<u32> = new_monitors.iter().map(|m| m.id).collect();
        let old_ids: HashSet<u32> = self.monitors.iter().map(|m| m.id).collect();
        let disconnected: Vec<u32> = old_ids.difference(&new_ids).copied().collect();
        let connected: Vec<u32> = new_ids.difference(&old_ids).copied().collect();

        self.monitors = new_monitors;

        for id in disconnected {
            if let Some(name) = self.active_workspace.remove(&id) {
                let outgoing: Vec<WindowId> = self
                    .workspaces
                    .get(&name)
                    .map(Tree::window_ids)
                    .unwrap_or_default()
                    .into_iter()
                    .chain(self.floating_windows_in(&name))
                    .collect();
                for (i, wid) in outgoing.into_iter().enumerate() {
                    self.park(wid, i);
                }
            }
        }

        if !self.monitors.iter().any(|m| m.id == self.focused_monitor) {
            self.focused_monitor = self.monitors.first().map(|m| m.id).unwrap_or(0);
        }

        for id in connected {
            let name = format!("monitor-{id}");
            self.workspaces.entry(name.clone()).or_default();
            self.active_workspace.insert(id, name);
        }

        for id in self.active_workspace.keys().copied().collect::<Vec<_>>() {
            self.relayout_monitor(id);
            self.reposition_floating_for_monitor(id);
        }
    }

    /// Moves the focus pointer to the window adjacent to the current focus
    /// in `dir`, and raises/focuses it for real. Returns an error message
    /// (suitable for `Response::Err`) if there's nothing focused yet or
    /// nothing in that direction. Goes through `focus_in_direction`, not
    /// plain `navigate`, so this also cycles an Accordion container's
    /// active child when the focused window is one of its members (M7).
    pub fn focus(&mut self, dir: Direction) -> Result<(), String> {
        let current = self.focused_node().ok_or("no window is focused")?;
        let target = self
            .active_tree_mut()
            .focus_in_direction(current, dir)
            .ok_or("no window in that direction")?;
        self.set_focused_node(target);
        self.relayout_active();
        self.raise_focused();
        Ok(())
    }

    /// Swaps the focused window with its neighbor in `dir` — the focused
    /// window ends up physically where the neighbor was (and vice versa),
    /// and focus follows it there.
    pub fn move_focused(&mut self, dir: Direction) -> Result<(), String> {
        let current = self.focused_node().ok_or("no window is focused")?;
        let target = self
            .active_tree_mut()
            .focus_in_direction(current, dir)
            .ok_or("no window in that direction")?;
        self.active_tree_mut().swap_windows(current, target);
        self.set_focused_node(target);
        self.relayout_active();
        self.raise_focused();
        Ok(())
    }

    /// Toggles the focused window's parent container between `Split`
    /// (tiled) and `Accordion` (stacked, one visible at a time). Errors if
    /// nothing's focused, or if the focused window is alone at the tree's
    /// root with no container to toggle.
    pub fn toggle_layout(&mut self) -> Result<(), String> {
        let current = self.focused_node().ok_or("no window is focused")?;
        if self.active_tree_mut().toggle_layout(current) {
            self.relayout_active();
            Ok(())
        } else {
            Err("nothing to toggle — only one window here".to_string())
        }
    }

    /// Sets the focused window's parent container to a specific layout
    /// kind — a no-op if it's already that kind, otherwise the same
    /// toggle `toggle_layout` does (there are only two kinds, so "set" and
    /// "toggle away from the other one" are the same operation).
    pub fn set_layout(&mut self, kind: tili_ipc::LayoutKind) -> Result<(), String> {
        let current = self.focused_node().ok_or("no window is focused")?;
        let is_accordion = self.active_tree().is_accordion_container(current);
        let want_accordion = matches!(kind, tili_ipc::LayoutKind::Accordion);
        if is_accordion == want_accordion {
            return Ok(());
        }
        if self.active_tree_mut().toggle_layout(current) {
            self.relayout_active();
            Ok(())
        } else {
            Err("nothing to set — only one window here".to_string())
        }
    }

    /// Switches which workspace is active on `focused_monitor`: parks every
    /// window in the outgoing workspace off-screen (tiled and floating
    /// alike), lays out the incoming one's tiled tree for real and
    /// re-centers its floating windows, and restores its remembered focus.
    /// Creates the target workspace (empty) if it doesn't exist yet. A
    /// no-op if `name` is already active on `focused_monitor`.
    ///
    /// If `name` is currently shown on a *different* monitor, that monitor
    /// swaps to whatever was on `focused_monitor` — two monitors never show
    /// the same workspace at once, since each has its own `Tree` layout
    /// computed against its own frame.
    pub fn switch_workspace(&mut self, name: &str) {
        let monitor_id = self.focused_monitor;
        let current = self.active_workspace.get(&monitor_id).cloned();
        if current.as_deref() == Some(name) {
            return;
        }

        let swap_monitor = self
            .active_workspace
            .iter()
            .find(|(id, n)| **id != monitor_id && n.as_str() == name)
            .map(|(&id, _)| id);

        if let Some(outgoing_name) = &current {
            let outgoing: Vec<WindowId> = self
                .workspaces
                .get(outgoing_name)
                .map(Tree::window_ids)
                .unwrap_or_default()
                .into_iter()
                .chain(self.floating_windows_in(outgoing_name))
                .collect();
            for (i, id) in outgoing.into_iter().enumerate() {
                self.park(id, i);
            }
        }

        if let Some(swap_id) = swap_monitor {
            match &current {
                Some(outgoing_name) => {
                    self.active_workspace.insert(swap_id, outgoing_name.clone());
                }
                None => {
                    self.active_workspace.remove(&swap_id);
                }
            }
        }

        self.workspaces.entry(name.to_string()).or_default();
        self.active_workspace.insert(monitor_id, name.to_string());

        self.relayout_active();
        self.reposition_floating_in_active_workspace();
        if let Some(swap_id) = swap_monitor {
            self.relayout_monitor(swap_id);
            self.reposition_floating_for_monitor(swap_id);
        }

        let restore = self
            .workspace_focus
            .get(name)
            .copied()
            .or_else(|| self.active_tree().default_focus());
        if let Some(node) = restore {
            self.set_focused_node(node);
            self.raise_focused();
        }
    }

    /// Moves the focused window into a different workspace's tree and
    /// parks it immediately, unless the target workspace happens to
    /// already be visible on some other monitor — in that case it's
    /// relaid-out there right away instead of sitting parked. Focus moves
    /// to whatever the active workspace suggests next.
    pub fn move_focused_to_workspace(&mut self, target_name: &str) -> Result<(), String> {
        let active_workspace = self.active_workspace_name();
        if target_name == active_workspace {
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
                self.workspace_focus.insert(active_workspace.clone(), n);
            }
            None => {
                self.workspace_focus.remove(&active_workspace);
            }
        }

        let target_focus_hint = self.workspace_focus.get(target_name).copied();
        let target_tree = self.workspaces.entry(target_name.to_string()).or_default();
        let new_node = target_tree.insert_window(id, target_focus_hint);
        self.workspace_focus
            .insert(target_name.to_string(), new_node);
        self.placements.insert(
            id,
            Placement {
                workspace: target_name.to_string(),
                floating: false,
            },
        );

        self.park(id, 0);
        let visible_elsewhere = self
            .active_workspace
            .iter()
            .find(|(mid, n)| **mid != self.focused_monitor && n.as_str() == target_name)
            .map(|(&mid, _)| mid);
        if let Some(mid) = visible_elsewhere {
            self.relayout_monitor(mid);
        }
        self.relayout_active();
        Ok(())
    }

    fn active_workspace_name(&self) -> String {
        self.active_workspace
            .get(&self.focused_monitor)
            .cloned()
            .unwrap_or_else(|| DEFAULT_WORKSPACE.to_string())
    }

    fn active_tree(&self) -> &Tree {
        // Always present: every place that changes `active_workspace` also
        // ensures the corresponding `workspaces` entry exists first, and the
        // `DEFAULT_WORKSPACE` fallback is inserted in `Default::default()`.
        self.workspaces
            .get(&self.active_workspace_name())
            .expect("active workspace always has a tree entry")
    }

    fn active_tree_mut(&mut self) -> &mut Tree {
        let name = self.active_workspace_name();
        self.workspaces.entry(name).or_default()
    }

    fn floating_windows_in(&self, workspace: &str) -> Vec<WindowId> {
        self.placements
            .iter()
            .filter(|(_, p)| p.floating && p.workspace == workspace)
            .map(|(&id, _)| id)
            .collect()
    }

    /// Drops a window's placement entirely — from its workspace's `Tree`
    /// if it was tiled, or just from `placements` if it was floating
    /// (floating windows were never in a `Tree` to begin with).
    fn remove_placement(&mut self, id: WindowId) {
        let Some(placement) = self.placements.remove(&id) else {
            return;
        };
        if placement.floating {
            return;
        }
        let Some(tree) = self.workspaces.get_mut(&placement.workspace) else {
            return;
        };
        let removed_leaf = tree.find_node(id);
        let suggested = tree.remove_window(id);
        if removed_leaf.is_some()
            && self.workspace_focus.get(&placement.workspace) == removed_leaf.as_ref()
        {
            match suggested {
                Some(n) => {
                    self.workspace_focus.insert(placement.workspace, n);
                }
                None => {
                    self.workspace_focus.remove(&placement.workspace);
                }
            }
        }
    }

    fn focused_node(&self) -> Option<NodeId> {
        self.workspace_focus
            .get(&self.active_workspace_name())
            .copied()
    }

    fn set_focused_node(&mut self, node: NodeId) {
        self.workspace_focus
            .insert(self.active_workspace_name(), node);
    }

    fn raise_focused(&self) {
        if let Some(node) = self.focused_node()
            && let Some(id) = self.active_tree().window_at(node)
            && let Some(window) = self.windows.get(&id)
        {
            window.focus();
        }
    }

    /// Moves a window off-screen without resizing it, outside the combined
    /// bounds of every *currently connected* monitor so a parked window can
    /// never land on a real display no matter how many are attached or how
    /// they're arranged. `offset_index` spreads multiple simultaneously-
    /// parked windows apart so they don't all land on the exact same
    /// off-screen coordinate.
    fn park(&mut self, id: WindowId, offset_index: usize) {
        let bounds = tili_ax::combined_bounds(&self.monitors);
        let x = bounds.x + bounds.width + PARK_MARGIN + (offset_index as f64 * PARK_OFFSET_STEP);
        let y = bounds.y;
        if let Some(window) = self.windows.get_mut(&id) {
            window.set_position(x, y);
        }
    }

    fn monitor_frame(&self, monitor_id: u32) -> Option<Rect> {
        self.monitors
            .iter()
            .find(|m| m.id == monitor_id)
            .map(|m| m.frame)
    }

    /// Recomputes every tiled window's frame on `focused_monitor` and
    /// applies it via the `WindowFrameSetter` seam — never a direct AX call
    /// from here. See `relayout_monitor` for the actual per-monitor logic;
    /// most callers only ever need to affect the focused monitor since
    /// that's the only one their own mutation touched.
    fn relayout_active(&mut self) {
        self.relayout_monitor(self.focused_monitor);
    }

    /// Re-lays-out every monitor that's currently showing something —
    /// used after a change (config reload, app termination) that could
    /// plausibly have touched a workspace visible on a *non*-focused
    /// monitor.
    fn relayout_all_visible(&mut self) {
        for id in self.active_workspace.keys().copied().collect::<Vec<_>>() {
            self.relayout_monitor(id);
        }
    }

    /// Recomputes every tiled window's frame in whatever workspace is
    /// active on `monitor_id` and applies it via the `WindowFrameSetter`
    /// seam. A no-op if `monitor_id` isn't connected or has no active
    /// workspace assigned. Windows in other (parked) workspaces are left
    /// exactly where they are. Only touches *tiled* windows — floating
    /// windows keep whatever position they were last centered/parked at
    /// (see `reposition_floating_for_monitor`).
    fn relayout_monitor(&mut self, monitor_id: u32) {
        let Some(name) = self.active_workspace.get(&monitor_id).cloned() else {
            return;
        };
        let Some(area) = self.monitor_frame(monitor_id) else {
            return;
        };
        let Some(tree) = self.workspaces.get(&name) else {
            return;
        };
        let gaps = self.workspace_gaps.get(&name).copied().unwrap_or(self.gaps);
        let placements = tree.layout(area, gaps);
        for (id, rect) in placements {
            if let Some(window) = self.windows.get_mut(&id) {
                self.frame_setter.set_frame(window, rect);
            }
        }
    }

    /// Re-centers/sizes every floating window belonging to whatever
    /// workspace is active on `focused_monitor` — called when a workspace
    /// becomes active again after being parked, so floating windows land
    /// back where they should rather than staying at their off-screen
    /// parked coordinates.
    fn reposition_floating_in_active_workspace(&mut self) {
        self.reposition_floating_for_monitor(self.focused_monitor);
    }

    fn reposition_floating_for_monitor(&mut self, monitor_id: u32) {
        let Some(name) = self.active_workspace.get(&monitor_id).cloned() else {
            return;
        };
        let Some(area) = self.monitor_frame(monitor_id) else {
            return;
        };
        let ids = self.floating_windows_in(&name);
        for id in ids {
            let frame = self
                .windows
                .get(&id)
                .and_then(|w| self.compute_floating_frame_in(w, area));
            if let Some(frame) = frame
                && let Some(window) = self.windows.get_mut(&id)
            {
                self.frame_setter.set_frame(window, frame);
            }
        }
    }

    /// Returns the frame a floating window should be placed at if `window`
    /// matches a floating rule (first match wins), or `None` if it doesn't
    /// match any — meaning it should be tiled instead. Sized/centered
    /// against `focused_monitor`'s frame, since new windows always land on
    /// the focused monitor's active workspace.
    fn compute_floating_frame(&self, window: &AxWindow) -> Option<Rect> {
        let area = self.monitor_frame(self.focused_monitor).unwrap_or(Rect {
            x: 0.0,
            y: 0.0,
            width: 1440.0,
            height: 900.0,
        });
        self.compute_floating_frame_in(window, area)
    }

    fn compute_floating_frame_in(&self, window: &AxWindow, area: Rect) -> Option<Rect> {
        let bundle_id = window.bundle_id()?;
        let rule = self.floating_rules.iter().find(|rule| {
            rule.app_id == bundle_id
                && rule
                    .title
                    .as_ref()
                    .is_none_or(|re| re.is_match(window.title()))
        })?;

        let width = rule
            .width
            .map(f64::from)
            .unwrap_or(area.width * f64::from(self.floating_defaults.width_ratio));
        let height = rule
            .height
            .map(f64::from)
            .unwrap_or(area.height * f64::from(self.floating_defaults.height_ratio));
        let center = rule.center.unwrap_or(self.floating_defaults.center);
        let (x, y) = if center {
            (
                area.x + (area.width - width) / 2.0,
                area.y + (area.height - height) / 2.0,
            )
        } else {
            (area.x, area.y)
        };

        Some(Rect {
            x,
            y,
            width,
            height,
        })
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
