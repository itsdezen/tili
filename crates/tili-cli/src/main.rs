use clap::{Parser, Subcommand};
use tili_ipc::default_socket_path;

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

    // TODO(M1): connect to `default_socket_path()`, send the real Command,
    // print the Response. Scaffolding only for now.
    match cli.command {
        Commands::Ping => {
            println!(
                "tili-cli: socket IPC lands in M1 (socket path would be {:?})",
                default_socket_path()
            );
        }
        Commands::ListWindows => {
            println!("tili-cli: list-windows lands in M1");
        }
    }
}
