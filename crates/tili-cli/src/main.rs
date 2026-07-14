use std::io::{self, Read, Write};
use std::os::unix::net::UnixStream;

use clap::{Parser, Subcommand, ValueEnum};
use tili_ipc::{
    Command, Direction, LayoutKind, MonitorInfo, PlacementInfo, Response, WindowInfo, WorkspaceInfo,
};

#[derive(Parser)]
#[command(name = "tili", about = "CLI for the tili tiling window manager daemon")]
struct Cli {
    #[command(subcommand)]
    command: Commands,
}

#[derive(Clone, Copy, ValueEnum)]
enum DirArg {
    Left,
    Right,
    Up,
    Down,
}

impl From<DirArg> for Direction {
    fn from(dir: DirArg) -> Self {
        match dir {
            DirArg::Left => Direction::Left,
            DirArg::Right => Direction::Right,
            DirArg::Up => Direction::Up,
            DirArg::Down => Direction::Down,
        }
    }
}

#[derive(Clone, Copy, ValueEnum)]
enum LayoutArg {
    Toggle,
    Tiles,
    Accordion,
    Horizontal,
    Vertical,
    ToggleOrientation,
}

#[derive(Clone, Copy, ValueEnum)]
enum OnOffArg {
    On,
    Off,
}

impl From<OnOffArg> for bool {
    fn from(state: OnOffArg) -> Self {
        matches!(state, OnOffArg::On)
    }
}

#[derive(Subcommand)]
enum Commands {
    /// Install and start tili-daemon as a LaunchAgent — it keeps running in
    /// the background, restarts if it crashes, and starts automatically at
    /// login.
    Start,
    /// Stop tili-daemon and remove its LaunchAgent, so it doesn't come back
    /// until `tili start` is run again.
    Stop,
    /// Report whether the daemon is running and reachable.
    Status,
    /// Check whether the daemon is reachable.
    Ping,
    /// List currently known windows.
    ListWindows,
    /// Move focus to the window in the given direction.
    Focus { direction: DirArg },
    /// Move the focused window one step in the given direction, re-parenting
    /// it through the tree rather than just swapping places.
    Move { direction: DirArg },
    /// Wrap the focused window and its neighbor in the given direction into
    /// a new, perpendicular container.
    Join { direction: DirArg },
    /// Grow (positive) or shrink (negative) the focused window's share of
    /// its nearest tiled container.
    Resize { amount: f32 },
    /// List workspaces, marking which one is active.
    ListWorkspaces,
    /// Switch the active workspace. Errors if `name` isn't declared in
    /// config — workspaces are never created on the fly.
    Workspace { name: String },
    /// Move the focused window to another workspace without following it.
    MoveToWorkspace { name: String },
    /// Toggle, or explicitly set, the focused window's container layout.
    Layout {
        mode: LayoutArg,
        /// Target the workspace's root container instead of the focused
        /// window's immediate parent — still one container, not applied to
        /// every container at once.
        #[arg(long)]
        root: bool,
    },
    /// Cycle which connected monitor commands act on.
    FocusMonitor,
    /// List connected monitors, marking which one is focused.
    ListMonitors,
    /// Reset every child weight of the focused window's parent container
    /// (or the workspace root, if --root) evenly, undoing any manual
    /// resizes.
    Balance {
        #[arg(long)]
        root: bool,
    },
    /// Re-normalize the tree. A no-op today: Tree::normalize already runs
    /// after every mutation and already collapses stray one-child
    /// containers.
    Flatten,
    /// Toggle the focused window fullscreen.
    Fullscreen {
        /// Use macOS's own native fullscreen (a separate Space) instead of
        /// tili's own tiled fullscreen.
        #[arg(long)]
        native: bool,
    },
    /// Close the focused window (best-effort AXCloseButton press).
    Close,
    /// Focus (and raise) the first known window whose title or bundle id
    /// contains the given text.
    Summon { query: String },
    /// Move a workspace to a different monitor without switching focus to
    /// it. Target is a monitor id, "next", or "main". Defaults to whatever
    /// workspace is currently active on the focused monitor; pass
    /// `--workspace` to name a different (possibly parked) one.
    MoveWorkspaceToMonitor {
        target: String,
        #[arg(long)]
        workspace: Option<String>,
    },
    /// Switch back to whichever workspace was active before this one.
    WorkspaceBack,
    /// Toggle the focused window between tiled and floating at runtime.
    SetFloating { state: OnOffArg },
}

/// What shape of payload to expect back, so the CLI doesn't have to guess
/// from JSON structure alone.
enum ExpectedPayload {
    None,
    Windows,
    Workspaces,
    Monitors,
}

fn main() {
    let cli = Cli::parse();

    // None of these three go through the generic send()/print_response
    // path below: Start/Stop never talk to the socket at all (they manage
    // the LaunchAgent directly), and Status wants its own wording instead
    // of the generic "couldn't reach daemon" error (which is a hard
    // failure for every other command, but an expected, calmly-reported
    // outcome here).
    match &cli.command {
        Commands::Start => {
            start_daemon();
            return;
        }
        Commands::Stop => {
            stop_daemon();
            return;
        }
        Commands::Status => {
            print_status();
            return;
        }
        _ => {}
    }

    let (command, expected) = match cli.command {
        Commands::Ping => (Command::Ping, ExpectedPayload::None),
        Commands::ListWindows => (Command::ListWindows, ExpectedPayload::Windows),
        Commands::Focus { direction } => (Command::Focus(direction.into()), ExpectedPayload::None),
        Commands::Move { direction } => (Command::Move(direction.into()), ExpectedPayload::None),
        Commands::Join { direction } => (Command::Join(direction.into()), ExpectedPayload::None),
        Commands::Resize { amount } => (Command::ResizeRatio { amount }, ExpectedPayload::None),
        Commands::ListWorkspaces => (Command::ListWorkspaces, ExpectedPayload::Workspaces),
        Commands::Workspace { name } => (Command::WorkspaceSwitch(name), ExpectedPayload::None),
        Commands::MoveToWorkspace { name } => {
            (Command::MoveNodeToWorkspace(name), ExpectedPayload::None)
        }
        Commands::Layout { mode, root } => {
            let command = match mode {
                LayoutArg::Toggle => Command::LayoutToggle(root),
                LayoutArg::Tiles => Command::LayoutSet(LayoutKind::Tiles, root),
                LayoutArg::Accordion => Command::LayoutSet(LayoutKind::Accordion, root),
                LayoutArg::Horizontal => {
                    Command::OrientationSet(tili_ipc::OrientationKind::Horizontal, root)
                }
                LayoutArg::Vertical => {
                    Command::OrientationSet(tili_ipc::OrientationKind::Vertical, root)
                }
                LayoutArg::ToggleOrientation => Command::OrientationToggle(root),
            };
            (command, ExpectedPayload::None)
        }
        Commands::FocusMonitor => (Command::FocusMonitor, ExpectedPayload::None),
        Commands::ListMonitors => (Command::ListMonitors, ExpectedPayload::Monitors),
        Commands::Balance { root } => (Command::BalanceSizes { root }, ExpectedPayload::None),
        Commands::Flatten => (Command::Flatten, ExpectedPayload::None),
        Commands::Fullscreen { native } => {
            (Command::FullscreenToggle { native }, ExpectedPayload::None)
        }
        Commands::Close => (Command::Close, ExpectedPayload::None),
        Commands::Summon { query } => (Command::Summon(query), ExpectedPayload::None),
        Commands::MoveWorkspaceToMonitor { workspace, target } => {
            let target = parse_monitor_target(&target).unwrap_or_else(|| {
                eprintln!(
                    "tili: invalid monitor target '{target}' (expected a monitor id, 'next', or 'main')"
                );
                std::process::exit(1);
            });
            (
                Command::MoveWorkspaceToMonitor { workspace, target },
                ExpectedPayload::None,
            )
        }
        Commands::WorkspaceBack => (Command::WorkspaceBack, ExpectedPayload::None),
        Commands::SetFloating { state } => {
            (Command::SetFloating(state.into()), ExpectedPayload::None)
        }
        Commands::Start => unreachable!("handled above before the socket connection"),
        Commands::Stop => unreachable!("handled above before the socket connection"),
        Commands::Status => unreachable!("handled above before the socket connection"),
    };

    match send(command) {
        Ok(response) => print_response(response, expected),
        Err(e) => {
            eprintln!(
                "tili: couldn't reach tili-daemon at {}: {e}\n\
                 (is the daemon running? try `tili start`)",
                tili_ipc::default_socket_path().display()
            );
            std::process::exit(1);
        }
    }
}

/// Sends one length-prefixed JSON `Command` and reads back one
/// length-prefixed JSON `Response`. Synchronous/blocking, matching the
/// wire format documented on `tili_ipc::default_socket_path` — this CLI is
/// a short-lived one-shot process, so plain `std::io` is simpler than
/// pulling in an async runtime just to talk to the daemon.
fn send(command: Command) -> io::Result<Response> {
    let mut stream = UnixStream::connect(tili_ipc::default_socket_path())?;

    let payload = serde_json::to_vec(&command)?;
    let len = u32::try_from(payload.len())
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "command too large"))?;
    stream.write_all(&len.to_be_bytes())?;
    stream.write_all(&payload)?;

    let mut len_buf = [0_u8; 4];
    stream.read_exact(&mut len_buf)?;
    let len = u32::from_be_bytes(len_buf) as usize;
    let mut response_buf = vec![0_u8; len];
    stream.read_exact(&mut response_buf)?;
    serde_json::from_slice(&response_buf).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))
}

fn print_response(response: Response, expected: ExpectedPayload) {
    match response {
        Response::Ok => println!("ok"),
        Response::Err { message } => {
            eprintln!("tili: {message}");
            std::process::exit(1);
        }
        Response::OkWithPayload(payload) => match expected {
            ExpectedPayload::Windows => print_windows(payload),
            ExpectedPayload::Workspaces => print_workspaces(payload),
            ExpectedPayload::Monitors => print_monitors(payload),
            ExpectedPayload::None => println!("{payload}"),
        },
    }
}

fn print_windows(payload: serde_json::Value) {
    match serde_json::from_value::<Vec<WindowInfo>>(payload) {
        Ok(windows) if windows.is_empty() => println!("no windows found"),
        Ok(windows) => {
            for w in windows {
                let placement = placement_label(w.placement);
                println!(
                    "{:>10}  pid={:<8} {placement} {:.0}x{:.0}+{:.0}+{:.0}  {}",
                    w.id, w.pid, w.frame.width, w.frame.height, w.frame.x, w.frame.y, w.title
                );
            }
        }
        Err(_) => println!("(response payload not recognized)"),
    }
}

fn placement_label(placement: PlacementInfo) -> &'static str {
    match placement {
        PlacementInfo::Tiled => "tile ",
        PlacementInfo::Floating => "float",
        PlacementInfo::NativeFullscreen => "fullscr",
        PlacementInfo::Minimized => "min  ",
        PlacementInfo::HiddenApplication => "hidden",
        PlacementInfo::Popup => "popup",
    }
}

fn print_workspaces(payload: serde_json::Value) {
    match serde_json::from_value::<Vec<WorkspaceInfo>>(payload) {
        Ok(workspaces) => {
            for w in workspaces {
                let marker = if w.active { "*" } else { " " };
                let monitor = w
                    .monitor
                    .map(|id| format!("monitor={id}"))
                    .unwrap_or_default();
                println!(
                    "{marker} {:<20} {} window(s)  {monitor}",
                    w.name, w.window_count
                );
            }
        }
        Err(_) => println!("(response payload not recognized)"),
    }
}

const LAUNCH_AGENT_LABEL: &str = "com.tili.daemon";

fn launch_agent_path() -> std::path::PathBuf {
    let home = std::env::var("HOME").expect("HOME must be set");
    std::path::PathBuf::from(home)
        .join("Library/LaunchAgents")
        .join(format!("{LAUNCH_AGENT_LABEL}.plist"))
}

/// `tili-daemon` ships alongside `tili` itself (both land in the same bin
/// directory, whether that's a Homebrew prefix or `cargo build`'s
/// `target/debug`) — resolved relative to this running binary rather than
/// relying on `tili-daemon` being on `PATH`, which a LaunchAgent's minimal
/// environment doesn't guarantee.
fn daemon_binary_path() -> std::path::PathBuf {
    std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(|dir| dir.join("tili-daemon")))
        .filter(|p| p.exists())
        .unwrap_or_else(|| std::path::PathBuf::from("tili-daemon"))
}

/// Parses `tili move-workspace-to-monitor <workspace> <target>`'s target
/// argument — a monitor id, or the literal `next`/`main`.
fn parse_monitor_target(s: &str) -> Option<tili_ipc::MonitorTarget> {
    match s {
        "next" => Some(tili_ipc::MonitorTarget::Next),
        "main" => Some(tili_ipc::MonitorTarget::Main),
        _ => s.parse::<u32>().ok().map(tili_ipc::MonitorTarget::Id),
    }
}

/// `tili status` — same underlying check as `tili ping`, worded for a
/// human glancing at it rather than for scripting.
fn print_status() {
    match send(Command::Ping) {
        Ok(_) => println!(
            "tili-daemon is running (socket: {})",
            tili_ipc::default_socket_path().display()
        ),
        Err(_) => println!("tili-daemon is not running"),
    }
}

/// `tili start` — writes a LaunchAgent plist and `launchctl load`s it, so
/// `launchd` (not this short-lived CLI process) owns tili-daemon from here
/// on: it starts it right now, restarts it if it crashes (`KeepAlive`),
/// and starts it again at every login (`RunAtLoad`).
fn start_daemon() {
    let plist_path = launch_agent_path();
    let Some(parent) = plist_path.parent() else {
        return;
    };
    if let Err(e) = std::fs::create_dir_all(parent) {
        eprintln!("tili: couldn't create {}: {e}", parent.display());
        std::process::exit(1);
    }

    let home = std::env::var("HOME").unwrap_or_default();
    let log_dir = format!("{home}/Library/Logs/tili");
    let _ = std::fs::create_dir_all(&log_dir);

    let binary = daemon_binary_path();
    let plist = format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>Label</key>
    <string>{LAUNCH_AGENT_LABEL}</string>
    <key>ProgramArguments</key>
    <array>
        <string>{}</string>
    </array>
    <key>RunAtLoad</key>
    <true/>
    <key>KeepAlive</key>
    <true/>
    <key>StandardOutPath</key>
    <string>{log_dir}/daemon.log</string>
    <key>StandardErrorPath</key>
    <string>{log_dir}/daemon.err.log</string>
</dict>
</plist>
"#,
        binary.display()
    );

    if let Err(e) = std::fs::write(&plist_path, plist) {
        eprintln!("tili: couldn't write {}: {e}", plist_path.display());
        std::process::exit(1);
    }

    match std::process::Command::new("launchctl")
        .args(["load", "-w"])
        .arg(&plist_path)
        .status()
    {
        Ok(status) if status.success() => {
            println!(
                "tili: daemon started (LaunchAgent installed at {})",
                plist_path.display()
            );
        }
        Ok(status) => {
            eprintln!("tili: `launchctl load` exited with {status}");
            std::process::exit(1);
        }
        Err(e) => {
            eprintln!("tili: couldn't run launchctl: {e}");
            std::process::exit(1);
        }
    }
}

/// `tili stop` — `launchctl unload`s and removes the LaunchAgent plist, so
/// `launchd` won't respawn tili-daemon (its `KeepAlive` only applies while
/// the job stays loaded). A daemon that's already not running (no plist)
/// is reported calmly, not as an error.
fn stop_daemon() {
    let plist_path = launch_agent_path();
    if !plist_path.exists() {
        println!("tili: daemon is not running");
        return;
    }
    let _ = std::process::Command::new("launchctl")
        .args(["unload", "-w"])
        .arg(&plist_path)
        .status();
    if let Err(e) = std::fs::remove_file(&plist_path) {
        eprintln!("tili: couldn't remove {}: {e}", plist_path.display());
        std::process::exit(1);
    }
    println!("tili: daemon stopped");
}

fn print_monitors(payload: serde_json::Value) {
    match serde_json::from_value::<Vec<MonitorInfo>>(payload) {
        Ok(monitors) if monitors.is_empty() => println!("no monitors found"),
        Ok(monitors) => {
            for m in monitors {
                let marker = if m.focused { "*" } else { " " };
                let main = if m.is_main { "main" } else { "    " };
                let workspace = m.active_workspace.unwrap_or_else(|| "-".to_string());
                println!(
                    "{marker} {:<10} {main}  {:.0}x{:.0}+{:.0}+{:.0}  {workspace}",
                    m.id, m.frame.width, m.frame.height, m.frame.x, m.frame.y
                );
            }
        }
        Err(_) => println!("(response payload not recognized)"),
    }
}
