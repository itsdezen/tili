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

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum Command {
    Focus(Direction),
    Move(Direction),
    WorkspaceSwitch(String),
    MoveNodeToWorkspace(String),
    LayoutSet(LayoutKind),
    LayoutToggle,
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

/// A window as reported by `Command::ListWindows`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WindowInfo {
    /// The real macOS `CGWindowID`.
    pub id: u32,
    pub pid: i32,
    pub title: String,
    /// Whether a floating rule matched this window (M8) — floating
    /// windows are excluded from tiling and only positioned once, on
    /// creation (or when their workspace becomes active again).
    pub floating: bool,
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
