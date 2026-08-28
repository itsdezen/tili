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
    /// Cycles which connected monitor `Focus`/`Move`/`WorkspaceSwitch`/etc.
    /// operate on. A no-op with fewer than two monitors connected.
    FocusMonitor,
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
    pub active: bool,
    pub window_count: usize,
    /// Which monitor this workspace is currently shown on, if any (M9).
    /// `None` for a workspace that exists but is parked (not visible on
    /// any connected monitor right now).
    pub monitor: Option<u32>,
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
/// corner radius, more like an ordinary button).
#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq)]
pub enum MenubarShape {
    #[default]
    Pill,
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

/// A connected display as reported by `Command::ListMonitors` (M9).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MonitorInfo {
    /// `CGDirectDisplayID` — not guaranteed stable across sleep/wake or
    /// hot unplug+replug, see `tili_ax::Monitor`'s docs.
    pub id: u32,
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
/// `Response`. `tili-daemon` and `tili-cli` each implement this framing
/// directly (one async via tokio, one sync via `std::io`) rather than
/// sharing code here, since the contract is simpler than a shared
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
}
