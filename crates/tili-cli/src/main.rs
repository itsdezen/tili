use std::io::{self, Read, Write};
use std::os::unix::net::UnixStream;

use clap::{Parser, Subcommand, ValueEnum};
use tili_ipc::{Command, Direction, LayoutKind, Response, WindowInfo, WorkspaceInfo};

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
}

/// What shape of payload to expect back, so the CLI doesn't have to guess
/// from JSON structure alone.
enum ExpectedPayload {
    None,
    Windows,
    Workspaces,
}

fn main() {
    let cli = Cli::parse();
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
                println!("{marker} {:<20} {} window(s)", w.name, w.window_count);
            }
        }
        Err(_) => println!("(response payload not recognized)"),
    }
}
