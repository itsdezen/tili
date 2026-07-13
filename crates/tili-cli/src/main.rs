use std::io::{self, Read, Write};
use std::os::unix::net::UnixStream;

use clap::{Parser, Subcommand};
use tili_ipc::{Command, Response, WindowInfo};

#[derive(Parser)]
#[command(name = "tili", about = "CLI for the tili tiling window manager daemon")]
struct Cli {
    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand)]
enum Commands {
    /// Check whether the daemon is reachable.
    Ping,
    /// List currently known windows.
    ListWindows,
}

fn main() {
    let cli = Cli::parse();
    let command = match cli.command {
        Commands::Ping => Command::Ping,
        Commands::ListWindows => Command::ListWindows,
    };

    match send(command) {
        Ok(response) => print_response(response),
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

fn print_response(response: Response) {
    match response {
        Response::Ok => println!("ok"),
        Response::Err { message } => {
            eprintln!("tili: {message}");
            std::process::exit(1);
        }
        Response::OkWithPayload(payload) => {
            match serde_json::from_value::<Vec<WindowInfo>>(payload) {
                Ok(windows) if windows.is_empty() => println!("no windows found"),
                Ok(windows) => {
                    for w in windows {
                        println!(
                            "{:>10}  pid={:<8} {:.0}x{:.0}+{:.0}+{:.0}  {}",
                            w.id,
                            w.pid,
                            w.frame.width,
                            w.frame.height,
                            w.frame.x,
                            w.frame.y,
                            w.title
                        );
                    }
                }
                Err(_) => println!("(response payload not recognized)"),
            }
        }
    }
}
