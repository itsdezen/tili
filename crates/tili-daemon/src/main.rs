mod dispatch;
mod socket;
mod state;

use dispatch::dispatch;
use state::WmState;
use tili_ax::WmEvent;

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

    let mut events = tili_ax::spawn_event_watcher();
    let mut state = WmState::default();

    // Single loop, no locks: every source of change (client connections now,
    // hotkeys/config-reload in later milestones) is a branch of this same
    // select!, so WmState only ever mutates from one place at a time.
    loop {
        tokio::select! {
            accepted = listener.accept() => {
                let (mut stream, _addr) = accepted?;
                match socket::read_command(&mut stream).await {
                    Ok(command) => {
                        let response = dispatch(&mut state, command);
                        if let Err(e) = socket::write_response(&mut stream, &response).await {
                            eprintln!("tili-daemon: failed to write response: {e}");
                        }
                    }
                    Err(e) => eprintln!("tili-daemon: failed to read command: {e}"),
                }
            }
            event = events.recv() => {
                match event {
                    Some(event) => handle_event(&mut state, event),
                    None => eprintln!("tili-daemon: event watcher channel closed unexpectedly"),
                }
            }
        }
    }
}

fn handle_event(state: &mut WmState, event: WmEvent) {
    match event {
        WmEvent::WindowsChanged { pid } => {
            let windows = tili_ax::list_windows_for_pid(pid);
            state.apply_windows_changed(pid, windows);
        }
        WmEvent::AppTerminated { pid } => state.remove_app(pid),
        WmEvent::AppLaunched { .. } => {
            // No-op: the watcher always follows this with a WindowsChanged
            // for the same pid once it has windows to report.
        }
    }
}
