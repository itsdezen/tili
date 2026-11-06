mod parse;

use serde::{Deserialize, Serialize};

pub use parse::{parse, parse_with_target};

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub enum Direction {
    Left,
    Right,
    Up,
    Down,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub enum LayoutKind {
    Tiles,
    Accordion,
}

/// A container's orientation axis — orthogonal to `LayoutKind` (a container
/// has both, independently; see `tili_tree::Node::Container`).
#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub enum OrientationKind {
    Horizontal,
    Vertical,
}

/// Which connected monitor `Command::MoveWorkspaceToMonitor` targets.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
pub enum MonitorTarget {
    /// A specific `CGDirectDisplayID` (see `MonitorInfo::id`).
    Id(u32),
    /// Whichever monitor `Command::FocusMonitor` would cycle to next.
    Next,
    /// Whichever monitor is one step before the current one in the
    /// deterministic display order (`main`, then left-to-right). Used by
    /// `focus-monitor prev` and `move-workspace-to-monitor prev`.
    Prev,
    /// The main display.
    Main,
}

/// Explicit target context for a command that would otherwise act on
/// "whatever's currently focused" — resolved once by `dispatch()` per the
/// blueprint's precedence (`window_id` > `workspace` > forwarded context >
/// current focus). `window_id` uses a plain `u32` rather than
/// `tili_tree::WindowId` since this crate doesn't depend on `tili-tree`
/// (see `WindowInfo.id` for the same convention).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Target {
    pub window_id: Option<u32>,
    pub workspace: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum Command {
    Focus(Direction),
    Move(Direction),
    /// Wraps the focused window and its neighbor in `dir` into a new,
    /// perpendicular container.
    Join(Direction),
    WorkspaceSwitch(String),
    /// Switches the currently focused monitor to `name`, pulling the
    /// workspace to this display (swapping with whatever is there) instead
    /// of navigating to the workspace's home display. This is the explicit
    /// "change this screen" counterpart to `WorkspaceSwitch`'s "go to X".
    WorkspaceHere(String),
    MoveNodeToWorkspace(String),
    /// `root: true` targets the workspace's root tiling container instead
    /// of the focused window's immediate parent. Still a single-container
    /// operation either way, not a recursive apply-to-every-container.
    LayoutSet(LayoutKind, bool),
    LayoutToggle(bool),
    /// Sets the focused window's parent container's orientation (or the
    /// workspace root's, if the bool is `true` — same `--root` convention
    /// as `LayoutSet`/`LayoutToggle`).
    OrientationSet(OrientationKind, bool),
    /// Flips the focused window's parent container's orientation between
    /// horizontal and vertical (or the workspace root's, if the bool is
    /// `true` — same `--root` convention as `OrientationSet`), instead of
    /// requiring two separate binds for each explicit direction.
    OrientationToggle(bool),
    /// Grows (positive) or shrinks (negative) the focused window's share of
    /// its nearest tiled container, taken from its siblings. `amount` is in
    /// the same weight-space as `tili_tree`'s container weights, not
    /// pixels — see `Tree::resize_weight`.
    ResizeRatio {
        amount: f32,
    },
    ModeEnter(String),
    ModeExit,
    /// The active keybindings mode's name (`"main"` unless a mode was
    /// entered and hasn't auto-exited/been exited yet) — read-only, so
    /// it's excluded from `main.rs`'s `WaitForChange` change-detection the
    /// same way `Ping`/`ListWindows`/etc. are.
    CurrentMode,
    ListWindows,
    ListWorkspaces,
    /// Resets every child weight of the focused window's parent container
    /// (or the workspace root, if `root`) evenly — see `Tree::balance_weights`.
    BalanceSizes {
        root: bool,
    },
    /// Re-runs the tree's own normalization pass and returns `Ok` — a
    /// distinct `flatten` command has no additional effect to implement
    /// since `Tree::normalize` already collapses one-child containers
    /// after every mutation (see the refactor plan's rationale). Deliberately
    /// a no-op placeholder: not wired into `dispatch()` yet.
    Flatten,
    /// Toggles the focused window fullscreen. `native` selects macOS's own
    /// `AXFullScreen` (a separate Space) versus tili laying the window out
    /// at the monitor's full frame while leaving the tree structurally
    /// intact for restoration.
    FullscreenToggle {
        native: bool,
    },
    /// Closes the focused window (presses its `AXCloseButton`,
    /// best-effort).
    Close,
    /// Focuses (and raises) the first window whose title or bundle id
    /// matches `_0`, launching nothing if none is found.
    Summon(String),
    /// Moves an entire workspace to a different monitor without switching
    /// focus to it. `workspace: None` means whatever's currently active on
    /// the focused monitor, rather than requiring an explicit name.
    MoveWorkspaceToMonitor {
        workspace: Option<String>,
        target: MonitorTarget,
    },
    /// Switches back to whichever workspace was active before the current
    /// one (back-and-forth toggle).
    WorkspaceBack,
    /// Toggles the focused window between tiled and floating at runtime
    /// (independent of any `floating-rules` match at creation time).
    SetFloating(bool),
    /// Flips the focused window between tiled and floating, without the
    /// caller having to know its current placement — the hotkey-friendly
    /// counterpart to `SetFloating`.
    ToggleFloating,
    /// Cycles which connected monitor `Focus`/`Move`/`WorkspaceSwitch`/etc.
    /// operate on. A no-op with fewer than two monitors connected.
    FocusMonitor,
    /// Focuses a specific connected monitor (`main`, `next`, `prev`, or a display id)
    /// instead of cycling. Used by `tili focus-monitor <target>`; the
    /// cycle-only `FocusMonitor` remains the hotkey-friendly default.
    FocusMonitorTarget(MonitorTarget),
    ListMonitors,
    /// Deliberately a no-op placeholder, same reasoning as `Flatten`: config
    /// already reloads automatically via file-watching
    /// (`spawn_config_reload_bridge` in `tili-daemon`), so there's no
    /// manual-reload behavior to wire into `dispatch()` yet. Kept in the
    /// wire protocol for a future explicit "reload now" use case.
    ReloadConfig,
    /// Health check for `tili doctor` — reports the daemon's current
    /// Accessibility/Input Monitoring permission grants and any warnings
    /// from its most recent config load (see `DoctorReport`). Read-only,
    /// so it goes through `dispatch()` like `ListWindows`/`ListWorkspaces`/
    /// `ListMonitors`, not the `Command::Shutdown` process-lifecycle path.
    Doctor,
    /// The current `menubar { }` config, read by `tili-menubar` on every
    /// poll (see `menu::poll_daemon`) — read-only, excluded from
    /// `WaitForChange` change-detection like `Doctor`/`CurrentMode`.
    /// `tili-menubar` has no `tili-config` dependency of its own; this is
    /// how it learns styling without a second, independent config-watch
    /// path (see `MenubarStyle`).
    MenubarStyle,
    /// One consolidated read of everything `tili-menubar`'s refresh needs —
    /// monitors, workspaces, current mode, and menubar style — in a single
    /// round trip (see `SnapshotInfo`). Each command costs a fresh socket
    /// connection, so the four separate queries this replaced cost four of
    /// those per refresh. Being read-only it also skips `dispatch()`'s focus
    /// sync (see `needs_focus_sync` in `tili-daemon`), so a refresh now
    /// costs no focus query at all. Read-only, so it's excluded from
    /// `WaitForChange` change-detection alongside those four.
    Snapshot,
    /// One versioned, machine-readable snapshot for `tili query state --json`
    /// and any other external integration. Unlike `Snapshot` (the compact
    /// menu-bar refresh payload), this includes every tracked window with
    /// its workspace, placement, identity, and AX role/subrole.
    QueryState,
    /// Keeps the connection open and streams versioned events as
    /// newline-delimited JSON from the CLI. Handled directly in the daemon's
    /// main loop (like `WaitForChange`), never through `dispatch()`: it
    /// installs a subscriber rather than producing one response.
    Subscribe,
    /// Gracefully stops the daemon: responds `Ok` (so a client waiting on
    /// the reply doesn't hang) before the process exits. Handled directly
    /// in `tili-daemon`'s main loop, not through `dispatch()` — it isn't a
    /// `WmState` mutation, it's process lifecycle.
    Shutdown,
    Ping,
    /// Blocks until *something* about `WmState` has changed since this
    /// request arrived (workspace switch, window created/closed/moved,
    /// monitor hot-plug, config reload, ...) — deliberately coarse, not
    /// "which field changed," so a client re-fetches whatever it actually
    /// cares about (e.g. `ListMonitors`/`ListWorkspaces`) after waking up
    /// rather than this command growing a payload shape of its own.
    /// Responds `Ok` either on a real change or after an internal
    /// timeout, whichever comes first — the timeout exists purely so a
    /// long-lived idle connection doesn't sit blocked forever if nothing
    /// ever changes, not so callers need to distinguish the two cases.
    /// Lets a client stay reachability- and change-aware with zero
    /// polling: the blocking read costs nothing while idle, unlike a
    /// sleep-and-recheck loop waking on a timer.
    WaitForChange,
    Raw {
        verb: String,
        args: Vec<String>,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum Response {
    Ok,
    OkWithPayload(serde_json::Value),
    Err { message: String },
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub struct RectInfo {
    pub x: f64,
    pub y: f64,
    pub width: f64,
    pub height: f64,
}

/// String-enum mirror of `tili-daemon`'s internal `PlacementKind`, so a
/// `--json` consumer reading an unrecognized variant (from a newer daemon)
/// degrades gracefully instead of failing to deserialize at all.
#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub enum PlacementInfo {
    Tiled,
    Floating,
    NativeFullscreen,
    Minimized,
    HiddenApplication,
    Popup,
}

/// A window as reported by `Command::ListWindows`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WindowInfo {
    /// The real macOS `CGWindowID`.
    pub id: u32,
    pub pid: i32,
    pub title: String,
    /// The owning application's bundle id, when resolvable. `None` for
    /// unbundled helpers and during the short window after launch before
    /// `NSRunningApplication` registers the process.
    #[serde(default)]
    pub bundle_id: Option<String>,
    /// The workspace this window is assigned to, even while parked.
    #[serde(default)]
    pub workspace: Option<String>,
    /// Whether this is tili's currently focused window.
    #[serde(default)]
    pub focused: bool,
    /// Accessibility role (normally `AXWindow`).
    #[serde(default)]
    pub role: String,
    /// Accessibility subrole (`AXStandardWindow`, `AXDialog`, ...), when
    /// the app reports one.
    #[serde(default)]
    pub subrole: Option<String>,
    /// Which of `tili-daemon`'s placement states this window is currently
    /// in — see `PlacementInfo`. Replaces the old plain `floating: bool`
    /// now that placement has more than two states.
    pub placement: PlacementInfo,
    pub frame: RectInfo,
}

/// A workspace as reported by `Command::ListWorkspaces`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WorkspaceInfo {
    pub name: String,
    /// User-facing label: a monitor's pretty name for synthetic workspaces,
    /// `None` for config-declared workspaces (clients fall back to `name`).
    #[serde(default)]
    pub display_name: Option<String>,
    /// Whether this workspace was auto-created for a connected display
    /// rather than declared in config.
    #[serde(default)]
    pub synthetic: bool,
    pub active: bool,
    pub window_count: usize,
    /// Which monitor this workspace is currently shown on, if any (M9).
    /// `None` for a workspace that exists but is parked (not visible on
    /// any connected monitor right now).
    pub monitor: Option<u32>,
    /// Last monitor this workspace was shown on, including while parked.
    /// `None` only if it has never been shown since startup.
    #[serde(default)]
    pub last_monitor: Option<u32>,
}

/// `Command::Doctor`'s payload. `tili-cli` combines this (when the daemon
/// is reachable) with its own filesystem/LaunchAgent-level checks, which
/// don't need a running daemon at all — so a `tili doctor` run while the
/// daemon is down still reports what it can, just without these fields.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DoctorReport {
    pub accessibility_granted: bool,
    pub input_monitoring_granted: bool,
    /// Rules skipped by the daemon's most recent config load — an
    /// undeclared workspace in `workspace-rules`, or an invalid regex in
    /// `floating-rules`. Empty if the last load had none to skip.
    pub config_warnings: Vec<String>,
    /// What the most recent startup session restore did, if one ran.
    #[serde(default)]
    pub session_restore: Option<SessionRestoreReport>,
}

/// `tili doctor`'s view of the last session restore.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionRestoreReport {
    pub matched: usize,
    pub missing_normal: usize,
    pub deferred_windows: usize,
    pub zero_match: bool,
}

/// Whether the badge pill is drawn solid (with the text/glyph knocked out
/// as transparency) or as a stroked outline (with the text/glyph drawn
/// normally inside it) — see `tili-menubar/src/badge.rs`'s `pill_image`.
#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq)]
pub enum MenubarFill {
    #[default]
    Filled,
    Outlined,
}

/// The badge's corner rounding — `Pill` (radius = half the badge height,
/// fully rounded ends, the original look) or `Rounded` (a smaller, fixed
/// corner radius, more like an ordinary button — the default). Only the
/// daemon-unreachable "connecting" badge stays hardcoded to `Pill`, so it
/// can't be mistaken for the normal badge.
#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq)]
pub enum MenubarShape {
    Pill,
    #[default]
    Rounded,
}

/// `Command::MenubarStyle`'s payload — mirrors `tili-config`'s `menubar { }`
/// block (see `tili_config::MenubarConfig`), converted at the daemon's
/// config-load boundary rather than shared as one type, since `tili-ipc`
/// doesn't depend on `tili-config` (wire types stay independent of the KDL
/// schema that produces them, same as every other `Command` payload here).
#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize)]
pub struct MenubarStyle {
    pub fill: MenubarFill,
    pub shape: MenubarShape,
    /// Uppercases the workspace name shown in the badge itself — not the
    /// dropdown menu's item labels, which stay as-typed for readability.
    pub uppercase: bool,
}

/// `Command::Snapshot`'s payload: the four reads `tili-menubar`'s refresh
/// previously issued as separate commands, bundled so one connection serves
/// the whole refresh (and, being read-only, with no focus sync at all).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SnapshotInfo {
    pub monitors: Vec<MonitorInfo>,
    pub workspaces: Vec<WorkspaceInfo>,
    /// The daemon's active keybindings mode — same value
    /// `Command::CurrentMode` returns.
    pub mode: String,
    /// The current `menubar { }` style. `Option` keeps the client's
    /// best-effort rule expressible: `None` means "no style data in this
    /// payload", so the badge keeps the previously applied style rather
    /// than resetting to `MenubarStyle::default()`. The daemon always
    /// fills this in (`WmState::menubar_style` is never absent).
    pub style: Option<MenubarStyle>,
}

/// `Command::QueryState`'s payload — a complete, versioned snapshot for
/// scripts and status bars. `tili query state --json` prints this verbatim.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct QueryState {
    /// Snapshot schema version. Bump only for a breaking shape change.
    pub version: u32,
    pub monitors: Vec<MonitorInfo>,
    pub workspaces: Vec<WorkspaceInfo>,
    pub windows: Vec<WindowInfo>,
    pub mode: String,
    pub focused_window: Option<WindowInfo>,
}

/// The compact state included with every `SubscribeEvent`: enough to drive
/// a status bar without shipping the full window list on every event.
/// Consumers that need windows can issue `QueryState` on demand.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SubscribeState {
    pub monitors: Vec<MonitorInfo>,
    pub workspaces: Vec<WorkspaceInfo>,
    pub mode: String,
    pub focused_window: Option<WindowInfo>,
}

/// One event in `Command::Subscribe`'s stream. The CLI decodes the
/// daemon's length-prefixed frames and prints each event as one JSON line.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SubscribeEvent {
    pub version: u32,
    /// Monotonic per-daemon sequence number. Consumers can use it to detect
    /// missed events (for example after a slow-client lag).
    pub seq: u64,
    /// `"snapshot"` for the initial event, `"state_changed"` thereafter.
    pub event: String,
    pub state: SubscribeState,
}

/// A connected display as reported by `Command::ListMonitors` (M9).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MonitorInfo {
    /// `CGDirectDisplayID` — not guaranteed stable across sleep/wake or
    /// hot unplug+replug, see `tili_ax::Monitor`'s docs.
    pub id: u32,
    /// One-based display position (main is always 1).
    #[serde(default)]
    pub ordinal: u32,
    /// Human-readable display name (`Sidecar Display`, `DELL U2720Q`, ...).
    #[serde(default)]
    pub pretty_name: String,
    pub is_main: bool,
    /// Whether `Focus`/`Move`/`WorkspaceSwitch`/etc. currently target this
    /// monitor.
    pub focused: bool,
    pub active_workspace: Option<String>,
    pub frame: RectInfo,
}

/// Default location of the daemon's Unix socket.
///
/// Wire format over this socket (both directions): a 4-byte big-endian
/// length prefix followed by that many bytes of JSON-encoded `Command` or
/// `Response`. Normal commands get exactly one response; `Command::Subscribe`
/// instead streams one framed `SubscribeEvent` payload after another until
/// the client disconnects. `tili-daemon` and `tili-cli` each implement this
/// framing directly (one async via tokio, one sync via `std::io`) rather
/// than sharing code here, since the contract is simpler than a shared
/// sync/async abstraction would be.
pub fn default_socket_path() -> std::path::PathBuf {
    dirs_socket_dir().join("tili.sock")
}

fn dirs_socket_dir() -> std::path::PathBuf {
    let home = std::env::var("HOME").expect("HOME must be set");
    std::path::PathBuf::from(home).join("Library/Application Support/tili")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn command_roundtrips_through_json() {
        let cmd = Command::Focus(Direction::Left);
        let json = serde_json::to_string(&cmd).unwrap();
        let back: Command = serde_json::from_str(&json).unwrap();
        assert!(matches!(back, Command::Focus(Direction::Left)));
    }

    #[test]
    fn query_state_and_subscribe_event_roundtrip_through_json() {
        let info = QueryState {
            version: 1,
            monitors: Vec::new(),
            workspaces: Vec::new(),
            windows: vec![WindowInfo {
                id: 7,
                pid: 42,
                title: "x".to_string(),
                bundle_id: Some("com.example.app".to_string()),
                workspace: Some("work".to_string()),
                focused: true,
                role: "AXWindow".to_string(),
                subrole: Some("AXStandardWindow".to_string()),
                placement: PlacementInfo::Tiled,
                frame: RectInfo {
                    x: 0.0,
                    y: 0.0,
                    width: 10.0,
                    height: 20.0,
                },
            }],
            mode: "main".to_string(),
            focused_window: None,
        };
        let json = serde_json::to_string(&info).unwrap();
        let back: QueryState = serde_json::from_str(&json).unwrap();
        assert_eq!(back.version, 1);
        assert_eq!(
            back.windows[0].bundle_id.as_deref(),
            Some("com.example.app")
        );
        assert_eq!(back.windows[0].workspace.as_deref(), Some("work"));
        assert!(back.windows[0].focused);
        assert_eq!(back.windows[0].subrole.as_deref(), Some("AXStandardWindow"));

        let event = SubscribeEvent {
            version: 1,
            seq: 3,
            event: "state_changed".to_string(),
            state: SubscribeState {
                monitors: Vec::new(),
                workspaces: Vec::new(),
                mode: "main".to_string(),
                focused_window: None,
            },
        };
        let json = serde_json::to_string(&event).unwrap();
        let back: SubscribeEvent = serde_json::from_str(&json).unwrap();
        assert_eq!(back.seq, 3);
        assert_eq!(back.event, "state_changed");
    }

    #[test]
    fn snapshot_command_and_payload_roundtrip_through_json() {
        let cmd = Command::Snapshot;
        let json = serde_json::to_string(&cmd).unwrap();
        let back: Command = serde_json::from_str(&json).unwrap();
        assert!(matches!(back, Command::Snapshot));

        let info = SnapshotInfo {
            monitors: vec![MonitorInfo {
                id: 7,
                ordinal: 1,
                pretty_name: "Main Display".to_string(),
                is_main: true,
                focused: true,
                active_workspace: Some("work".to_string()),
                frame: RectInfo {
                    x: 0.0,
                    y: 0.0,
                    width: 1440.0,
                    height: 900.0,
                },
            }],
            workspaces: vec![WorkspaceInfo {
                name: "work".to_string(),
                display_name: None,
                synthetic: false,
                active: true,
                window_count: 3,
                monitor: Some(7),
                last_monitor: Some(7),
            }],
            mode: "resize".to_string(),
            style: Some(MenubarStyle {
                fill: MenubarFill::Outlined,
                shape: MenubarShape::Rounded,
                uppercase: true,
            }),
        };
        let json = serde_json::to_string(&info).unwrap();
        let back: SnapshotInfo = serde_json::from_str(&json).unwrap();
        assert_eq!(back.monitors.len(), 1);
        assert_eq!(back.monitors[0].id, 7);
        assert!(back.monitors[0].focused);
        assert_eq!(back.monitors[0].active_workspace.as_deref(), Some("work"));
        assert_eq!(back.monitors[0].frame.width, 1440.0);
        assert_eq!(back.workspaces.len(), 1);
        assert_eq!(back.workspaces[0].name, "work");
        assert_eq!(back.workspaces[0].window_count, 3);
        assert_eq!(back.workspaces[0].monitor, Some(7));
        assert_eq!(back.mode, "resize");
        let style = back.style.expect("style should survive the round trip");
        assert_eq!(style.fill, MenubarFill::Outlined);
        assert_eq!(style.shape, MenubarShape::Rounded);
        assert!(style.uppercase);
    }
}
