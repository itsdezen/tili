use std::io::{self, Read, Write};
use std::os::unix::net::UnixStream;

use clap::{Parser, Subcommand, ValueEnum};
use tili_ipc::{Command, Direction, LayoutKind, MonitorInfo, Response, WindowInfo, WorkspaceInfo};

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
}

#[derive(Subcommand)]
enum Commands {
    /// Check whether the daemon is reachable.
    Ping,
    /// List currently known windows.
    ListWindows,
    /// Move focus to the window in the given direction.
    Focus { direction: DirArg },
    /// Swap the focused window with its neighbor in the given direction.
    Move { direction: DirArg },
    /// List workspaces, marking which one is active.
    ListWorkspaces,
    /// Switch the active workspace (creating it if it doesn't exist yet).
    Workspace { name: String },
    /// Move the focused window to another workspace without following it.
    MoveToWorkspace { name: String },
    /// Toggle, or explicitly set, the focused window's container layout.
    Layout { mode: LayoutArg },
    /// Cycle which connected monitor commands act on.
    FocusMonitor,
    /// List connected monitors, marking which one is focused.
    ListMonitors,
    /// Manage tili-daemon's LaunchAgent (auto-start at login).
    Daemon {
        #[command(subcommand)]
        action: DaemonAction,
    },
}

#[derive(Subcommand)]
enum DaemonAction {
    /// Install a LaunchAgent so tili-daemon starts automatically at login.
    /// Opt-in — never run automatically by `brew install`.
    Install,
    /// Remove the LaunchAgent.
    Uninstall,
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

    // Doesn't talk to the daemon's socket at all — a local filesystem +
    // launchctl operation, handled before the send()/dispatch() path below.
    if let Commands::Daemon { action } = &cli.command {
        match action {
            DaemonAction::Install => install_launch_agent(),
            DaemonAction::Uninstall => uninstall_launch_agent(),
        }
        return;
    }

    let (command, expected) = match cli.command {
        Commands::Ping => (Command::Ping, ExpectedPayload::None),
        Commands::ListWindows => (Command::ListWindows, ExpectedPayload::Windows),
        Commands::Focus { direction } => (Command::Focus(direction.into()), ExpectedPayload::None),
        Commands::Move { direction } => (Command::Move(direction.into()), ExpectedPayload::None),
        Commands::ListWorkspaces => (Command::ListWorkspaces, ExpectedPayload::Workspaces),
        Commands::Workspace { name } => (Command::WorkspaceSwitch(name), ExpectedPayload::None),
        Commands::MoveToWorkspace { name } => {
            (Command::MoveNodeToWorkspace(name), ExpectedPayload::None)
        }
        Commands::Layout { mode } => {
            let command = match mode {
                LayoutArg::Toggle => Command::LayoutToggle,
                LayoutArg::Tiles => Command::LayoutSet(LayoutKind::Tiles),
                LayoutArg::Accordion => Command::LayoutSet(LayoutKind::Accordion),
            };
            (command, ExpectedPayload::None)
        }
        Commands::FocusMonitor => (Command::FocusMonitor, ExpectedPayload::None),
        Commands::ListMonitors => (Command::ListMonitors, ExpectedPayload::Monitors),
        Commands::Daemon { .. } => unreachable!("handled above before the socket connection"),
    };

    match send(command) {
        Ok(response) => print_response(response, expected),
        Err(e) => {
            eprintln!(
                "tili: couldn't reach tili-daemon at {}: {e}\n\
                 (is the daemon running? try `cargo run --bin tili-daemon`)",
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
                let placement = if w.floating { "float" } else { "tile " };
                println!(
                    "{:>10}  pid={:<8} {placement} {:.0}x{:.0}+{:.0}+{:.0}  {}",
                    w.id, w.pid, w.frame.width, w.frame.height, w.frame.x, w.frame.y, w.title
                );
            }
        }
        Err(_) => println!("(response payload not recognized)"),
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

fn install_launch_agent() {
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
                "tili: installed LaunchAgent at {} — tili-daemon will now start \
                 automatically at login (and right now)",
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

fn uninstall_launch_agent() {
    let plist_path = launch_agent_path();
    if !plist_path.exists() {
        println!("tili: no LaunchAgent installed");
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
    println!("tili: removed LaunchAgent at {}", plist_path.display());
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
