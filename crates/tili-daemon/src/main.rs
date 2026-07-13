mod dispatch;
mod socket;

use dispatch::{WmState, dispatch};

#[tokio::main]
async fn main() -> std::io::Result<()> {
    if !tili_ax::ensure_accessibility_permission() {
        eprintln!(
            "tili-daemon: waiting for Accessibility permission — grant it in \
             System Settings > Privacy & Security > Accessibility, then restart tili-daemon."
        );
    }

    let listener = socket::bind()?;
    println!(
        "tili-daemon: listening on {}",
        tili_ipc::default_socket_path().display()
    );

    let mut state = WmState::default();

    // M1 scope: handle one connection at a time on this single task. No
    // concurrent event sources exist yet (hotkeys/AX notifications land in
    // M2/M6), so there's nothing to multiplex and no need for a shared-state
    // lock — this stays true to the "one loop owns WmState" design.
    loop {
        let (mut stream, _addr) = listener.accept().await?;
        match socket::read_command(&mut stream).await {
            Ok(command) => {
                let response = dispatch(&mut state, command);
                if let Err(e) = socket::write_response(&mut stream, &response).await {
                    eprintln!("tili-daemon: failed to write response: {e}");
                }
            }
            Err(e) => {
                eprintln!("tili-daemon: failed to read command: {e}");
            }
        }
    }
}
