mod dispatch;
mod socket;
mod state;

use std::collections::HashSet;
use std::sync::{Arc, Mutex};

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
    let mut config_updates = spawn_config_reload_bridge();
    let active_combos = Arc::new(Mutex::new(HashSet::new()));
    let mut hotkeys = spawn_hotkey_bridge(active_combos.clone());
    let mut state = WmState::default();

    let config_path = tili_config::default_config_path();
    match tili_config::load(&config_path) {
        Ok(config) => {
            println!("tili-daemon: loaded config from {}", config_path.display());
            state.apply_config(&config);
        }
        Err(e) => eprintln!(
            "tili-daemon: failed to load {}: {e} (using defaults)",
            config_path.display()
        ),
    }
    sync_active_combos(&active_combos, &state);

    // Single loop, no locks around WmState: every source of change (client
    // connections, background AX/NSWorkspace events, config reloads,
    // resolved hotkey presses) is a branch of this same select!, so state
    // only ever mutates from one place at a time. The one exception is
    // `active_combos`, a small Mutex<HashSet<KeyCombo>> the hotkey tap's
    // callback reads synchronously (see `tili_ax::spawn_hotkey_tap`) — kept
    // in sync via `sync_active_combos` after anything that could change the
    // current mode or its bindings.
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
                        sync_active_combos(&active_combos, &state);
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
            Some(config) = config_updates.recv() => {
                println!("tili-daemon: config reloaded from {}", config_path.display());
                state.apply_config(&config);
                sync_active_combos(&active_combos, &state);
            }
            Some(combo) = hotkeys.recv() => {
                // Hotkey-triggered commands go through the exact same
                // dispatch() the socket handler uses above — see
                // CLAUDE.md's design invariants for why that's non-negotiable.
                if let Some(command) = state.resolve_hotkey(combo) {
                    let _ = dispatch(&mut state, command);
                }
                sync_active_combos(&active_combos, &state);
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

fn sync_active_combos(shared: &Arc<Mutex<HashSet<tili_ax::KeyCombo>>>, state: &WmState) {
    if let Ok(mut set) = shared.lock() {
        *set = state.active_key_combos();
    }
}

/// Bridges `tili_config`'s plain-`std::sync::mpsc` file-watcher (see its
/// module docs for why it isn't async itself) into a tokio channel this
/// daemon's `select!` loop can read from directly.
fn spawn_config_reload_bridge() -> tokio::sync::mpsc::UnboundedReceiver<tili_config::Config> {
    let sync_rx = tili_config::spawn_config_watcher(tili_config::default_config_path());
    let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
    std::thread::spawn(move || {
        while let Ok(config) = sync_rx.recv() {
            if tx.send(config).is_err() {
                break;
            }
        }
    });
    rx
}

/// Bridges `tili_ax::spawn_hotkey_tap`'s plain-`std::sync::mpsc` channel
/// into a tokio channel, same pattern as `spawn_config_reload_bridge`.
fn spawn_hotkey_bridge(
    active_combos: Arc<Mutex<HashSet<tili_ax::KeyCombo>>>,
) -> tokio::sync::mpsc::UnboundedReceiver<tili_ax::KeyCombo> {
    let sync_rx = tili_ax::spawn_hotkey_tap(active_combos);
    let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
    std::thread::spawn(move || {
        while let Ok(combo) = sync_rx.recv() {
            if tx.send(combo).is_err() {
                break;
            }
        }
    });
    rx
}
