mod dispatch;
mod socket;
mod state;

use std::collections::HashSet;
use std::sync::{Arc, Mutex};

use dispatch::dispatch;
use state::WmState;
use tili_ax::WmEvent;
use tili_ipc::{Command, Response};

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
    let mut displays_changed = spawn_display_watcher_bridge();
    let mut mouse_moves = spawn_mouse_watcher_bridge();
    let mut state = WmState::default();

    let config_path = tili_config::default_config_path();
    ensure_starter_config_exists(&config_path);
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
                    Ok(Command::Shutdown) => {
                        // Respond before exiting so a client waiting on the
                        // reply (`tili stop`) doesn't hang — not routed
                        // through dispatch(), since it's process lifecycle,
                        // not a WmState mutation.
                        let _ = socket::write_response(&mut stream, &Response::Ok).await;
                        println!("tili-daemon: shutting down");
                        break;
                    }
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
                // Shutdown is the one exception, same reasoning as the
                // socket branch above.
                if let Some(command) = state.resolve_hotkey(combo) {
                    if matches!(command, Command::Shutdown) {
                        println!("tili-daemon: shutting down (hotkey)");
                        break;
                    }
                    let _ = dispatch(&mut state, command);
                }
                sync_active_combos(&active_combos, &state);
            }
            Some(()) = displays_changed.recv() => {
                // A monitor connected/disconnected/reconfigured (M9) — the
                // callback doesn't say which, so just re-enumerate.
                state.on_displays_changed();
            }
            Some((x, y)) = mouse_moves.recv() => {
                // Throttled cursor positions (M10, focus-follows-monitor) —
                // a no-op inside `on_mouse_moved` unless that setting is on.
                state.on_mouse_moved(x, y);
            }
        }
    }

    let _ = std::fs::remove_file(tili_ipc::default_socket_path());
    Ok(())
}

/// M10: a brand-new install has no `~/.config/tili/tili.kdl` yet — without
/// this, a first run silently applies `Config::default()` (no workspaces,
/// no keybindings, nothing to edit) with no clue that a starter file
/// exists to build from. Writes the same config shipped as
/// `example/tili.kdl` so the daemon still starts fine even if this write
/// fails for some reason (permissions, read-only home, etc.) — this is a
/// convenience, not a requirement to run.
fn ensure_starter_config_exists(path: &std::path::Path) {
    if path.exists() {
        return;
    }
    const STARTER_CONFIG: &str = include_str!("../../../example/tili.kdl");
    if let Some(parent) = path.parent()
        && let Err(e) = std::fs::create_dir_all(parent)
    {
        eprintln!("tili-daemon: couldn't create {}: {e}", parent.display());
        return;
    }
    match std::fs::write(path, STARTER_CONFIG) {
        Ok(()) => println!(
            "tili-daemon: no config found — wrote a starter config to {}",
            path.display()
        ),
        Err(e) => eprintln!(
            "tili-daemon: couldn't write starter config to {}: {e} (using built-in defaults)",
            path.display()
        ),
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

/// Bridges `tili_ax::spawn_display_watcher`'s plain-`std::sync::mpsc`
/// channel into a tokio channel, same pattern as the other bridges (M9).
fn spawn_display_watcher_bridge() -> tokio::sync::mpsc::UnboundedReceiver<()> {
    let sync_rx = tili_ax::spawn_display_watcher();
    let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
    std::thread::spawn(move || {
        while let Ok(()) = sync_rx.recv() {
            if tx.send(()).is_err() {
                break;
            }
        }
    });
    rx
}

/// Bridges `tili_ax::spawn_mouse_watcher`'s plain-`std::sync::mpsc` channel
/// into a tokio channel, same pattern as the other bridges (M10). Runs
/// unconditionally regardless of `focus-follows-monitor`, same as the
/// hotkey tap running regardless of whether any keybindings are
/// configured — `WmState::on_mouse_moved` is what actually gates on the
/// setting.
fn spawn_mouse_watcher_bridge() -> tokio::sync::mpsc::UnboundedReceiver<(f64, f64)> {
    let sync_rx = tili_ax::spawn_mouse_watcher();
    let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
    std::thread::spawn(move || {
        while let Ok(point) = sync_rx.recv() {
            if tx.send(point).is_err() {
                break;
            }
        }
    });
    rx
}
