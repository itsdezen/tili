mod parse;

use serde::{Deserialize, Serialize};

pub use parse::parse;

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

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum Command {
    Focus(Direction),
    Move(Direction),
    /// Wraps the focused window and its neighbor in `dir` into a new,
    /// perpendicular container — AeroSpace's `join-with`.
    Join(Direction),
    WorkspaceSwitch(String),
    MoveNodeToWorkspace(String),
    /// `root: true` targets the workspace's root tiling container instead
    /// of the focused window's immediate parent — matches AeroSpace's
    /// `layout --root` flag. Still a single-container operation either way,
    /// not a recursive apply-to-every-container.
    LayoutSet(LayoutKind, bool),
    LayoutToggle(bool),
    /// Sets the focused window's parent container's orientation (or the
    /// workspace root's, if the bool is `true` — same `--root` convention
    /// as `LayoutSet`/`LayoutToggle`).
    OrientationSet(OrientationKind, bool),
    /// Grows (positive) or shrinks (negative) the focused window's share of
    /// its nearest tiled container, taken from its siblings. `amount` is in
    /// the same weight-space as `tili_tree`'s container weights, not
    /// pixels — see `Tree::resize_weight`.
    ResizeRatio {
        amount: f32,
    },
    ModeEnter(String),
    ModeExit,
    ListWindows,
    ListWorkspaces,
    /// Cycles which connected monitor `Focus`/`Move`/`WorkspaceSwitch`/etc.
    /// operate on. A no-op with fewer than two monitors connected.
    FocusMonitor,
    ListMonitors,
    ReloadConfig,
    /// Gracefully stops the daemon: responds `Ok` (so a client waiting on
    /// the reply doesn't hang) before the process exits. Handled directly
    /// in `tili-daemon`'s main loop, not through `dispatch()` — it isn't a
    /// `WmState` mutation, it's process lifecycle.
    Shutdown,
    Ping,
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
