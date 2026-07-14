mod dispatch;
mod socket;
mod state;

use std::collections::HashSet;
use std::os::unix::process::CommandExt;
use std::sync::{Arc, Mutex};

use dispatch::dispatch;
use state::WmState;
use tili_ax::WmEvent;
use tili_ipc::{Command, Response};

/// How long `main` waits for a first-time Accessibility grant before
/// giving up and fully stopping itself (`stop_self`) — bounded so a user
/// who never grants it doesn't end up with the daemon stuck running in the
/// same half-broken state (no window watching, ever) this whole startup
/// sequence exists to avoid falling into.
const ACCESSIBILITY_GRANT_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(60);
/// How often `main` re-execs itself while waiting for a first-time
/// Accessibility grant. Deliberately *not* a poll of
/// `has_accessibility_permission()` in place — empirically, checking trust
/// status from *within* a process that was already running when permission
/// was granted does not reliably observe the change, no matter which
/// Accessibility API is used or how often it's polled. A **freshly started**
/// process's own check at the top of `main`, by contrast, reliably reflects
/// current reality. So instead of polling in place, this just periodically
/// restarts — the moment permission is actually granted, the very next
/// restart's fresh check sees it immediately and this whole wait block is
/// skipped entirely in that new instance.
const ACCESSIBILITY_RESTART_INTERVAL: std::time::Duration = std::time::Duration::from_secs(2);
/// Carries the wall-clock time this wait originally started across
/// self-restarts (`Instant` can't cross a process boundary, so this uses a
/// Unix timestamp instead) — without it, every restart would reset its own
/// elapsed-time clock to zero and `ACCESSIBILITY_GRANT_TIMEOUT` would never
/// actually elapse.
const ACCESSIBILITY_WAIT_STARTED_ENV: &str = "TILI_ACCESSIBILITY_WAIT_STARTED_AT";

#[tokio::main]
async fn main() -> std::io::Result<()> {
    // IOHIDRequestAccess (Input Monitoring) must run before anything calls
    // into Accessibility's AXIsProcessTrustedWithOptions in this process's
    // lifetime — rdar://7381305: once Accessibility's check has run once,
    // Input Monitoring's prompt silently stops appearing for a first-time
    // grant. Don't reorder this below ensure_accessibility_permission()/
    // has_accessibility_permission(), and don't add any AX call before it.
    tili_ax::request_input_monitoring_permission();

    let listener = socket::bind()?;
    println!(
        "tili-daemon: listening on {}",
        tili_ipc::default_socket_path().display()
    );

    if !tili_ax::ensure_accessibility_permission() {
        let wait_started = accessibility_wait_started();
        eprintln!(
            "tili-daemon: waiting for Accessibility permission — grant it in System Settings \
             > Privacy & Security > Accessibility (giving up {}s after the first attempt if \
             not granted).",
            ACCESSIBILITY_GRANT_TIMEOUT.as_secs()
        );
        // Only relevant when tili-daemon is run directly in a terminal
        // (not via the LaunchAgent `tili start` installs) — Ctrl-C or the
        // terminal closing are just as much "not granting permission" as
        // the timeout below, and should stop cleanly the same way rather
        // than leaving the process to die from the default signal
        // disposition without ever reaching `stop_self`.
        let mut sigint = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::interrupt())?;
        let mut sighup = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::hangup())?;
        loop {
            if elapsed_since(wait_started) >= ACCESSIBILITY_GRANT_TIMEOUT {
                eprintln!(
                    "tili-daemon: Accessibility permission not granted within {}s — stopping. \
                     Run `tili start` again after granting it.",
                    ACCESSIBILITY_GRANT_TIMEOUT.as_secs()
                );
                stop_self();
                return Ok(());
            }
            tokio::select! {
                () = tokio::time::sleep(ACCESSIBILITY_RESTART_INTERVAL) => {
                    // Unconditional: a fresh process's own check at the top
                    // of `main` is what reliably reflects reality, not
                    // polling in place — see `ACCESSIBILITY_RESTART_INTERVAL`'s
                    // doc comment. If permission was granted since this
                    // process started, the next instance sees it
                    // immediately and skips this whole block entirely; if
                    // not, `exec` never returns here either way, so falling
                    // through to retry only happens when the restart
                    // itself failed to launch.
                    let err = restart_self(Some(wait_started));
                    eprintln!("tili-daemon: failed to restart while waiting for permission: {err}");
                }
                _ = sigint.recv() => {
                    eprintln!(
                        "tili-daemon: interrupted while waiting for Accessibility permission — \
                         stopping."
                    );
                    stop_self();
                    return Ok(());
                }
                _ = sighup.recv() => {
                    eprintln!(
                        "tili-daemon: terminal closed while waiting for Accessibility \
                         permission — stopping."
                    );
                    stop_self();
                    return Ok(());
                }
            }
        }
    }

    if !tili_ax::has_input_monitoring_permission() {
        eprintln!(
            "tili-daemon: Input Monitoring permission not granted yet — hotkeys will start \
             working automatically once you grant it in System Settings > Privacy & Security \
             > Input Monitoring, no restart needed."
        );
    }

    let mut events = tili_ax::spawn_event_watcher();
    let mut config_updates = spawn_config_reload_bridge();
    let active_combos = Arc::new(Mutex::new(HashSet::new()));
    let mut hotkeys = spawn_hotkey_bridge(active_combos.clone());
    let mut displays_changed = spawn_display_watcher_bridge();
    let mut mouse_events = spawn_mouse_watcher_bridge();
    let mut state = WmState::default();

    // Debounces `WmEvent::WindowsChanged`: a pid lands here instead of
    // being rescanned immediately, and every pid queued gets exactly one
    // rescan per `maintenance_tick`, using whichever state is current at
    // that moment — a pid re-signaled before its tick naturally coalesces
    // into that one pass rather than triggering a second rescan.
    let mut pending_pids: HashSet<i32> = HashSet::new();
    // Drains `pending_pids` and rechecks Phase 5's `pending_removal` grace
    // period — one combined periodic-maintenance branch rather than two,
    // since both are just "a little time has passed, go recheck something."
    let mut maintenance_tick = tokio::time::interval(std::time::Duration::from_millis(30));

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
                        state.unpark_all();
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
                    Some(event) => handle_event(&mut state, &mut pending_pids, event),
                    None => eprintln!("tili-daemon: event watcher channel closed unexpectedly"),
                }
            }
            _ = maintenance_tick.tick() => {
                for pid in pending_pids.drain() {
                    let windows = tili_ax::list_windows_for_pid(pid);
                    state.apply_windows_changed(pid, windows);
                }
                state.finalize_expired_removals();
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
                        state.unpark_all();
                        println!("tili-daemon: shutting down (hotkey)");
                        break;
                    }
                    // Captured before dispatch: the mode the key was
                    // pressed in, not whatever it becomes after (e.g. a
                    // bind that itself switches modes).
                    let auto_exits = state.current_mode_auto_exits();
                    let _ = dispatch(&mut state, command);
                    if auto_exits {
                        state.exit_mode();
                    }
                }
                sync_active_combos(&active_combos, &state);
            }
            Some(()) = displays_changed.recv() => {
                // A monitor connected/disconnected/reconfigured (M9) — the
                // callback doesn't say which, so just re-enumerate.
                state.on_displays_changed();
            }
            Some(signal) = mouse_events.recv() => {
                match signal {
                    // Throttled cursor positions (M10, focus-follows-monitor)
                    // — a no-op inside `on_mouse_moved` unless that setting
                    // is on.
                    tili_ax::MouseSignal::Moved(x, y) => state.on_mouse_moved(x, y),
                    // Suppresses relayout for the duration of a drag-resize
                    // (M10.1) so `apply_windows_changed` doesn't fight the
                    // user's drag; button-up relays out once to snap back
                    // to the tiled layout.
                    tili_ax::MouseSignal::ButtonDown => state.on_mouse_button_down(),
                    tili_ax::MouseSignal::ButtonUp => state.on_mouse_button_up(),
                }
            }
        }
    }

    let _ = std::fs::remove_file(tili_ipc::default_socket_path());
    Ok(())
}

/// Re-executes this exact binary with the same arguments, replacing the
/// current process image in place (same pid — `execve` under the hood, not
/// an exit-and-respawn) — called periodically while waiting for a
/// first-time Accessibility grant (see `ACCESSIBILITY_RESTART_INTERVAL`).
///
/// Empirically, macOS does not reliably activate full AX capabilities
/// (`AXObserver` notifications in particular — window-created/destroyed
/// events observed to be missed or delayed) for a process that was already
/// running at the moment permission was granted, even though
/// permission-check APIs correctly report it as trusted afterward — only a
/// process that *launches* after the grant behaves reliably. Self-restarting
/// is what makes "grant permission, then it just works" true without the
/// user ever running `tili stop`/`start` by hand. Same pid means launchd's
/// `KeepAlive` tracking is completely undisturbed by this.
///
/// `wait_started`, if given, is propagated to the new process via
/// `ACCESSIBILITY_WAIT_STARTED_ENV` so `accessibility_wait_started` in the
/// new instance resumes the same overall countdown rather than restarting
/// a fresh `ACCESSIBILITY_GRANT_TIMEOUT` budget on every restart.
///
/// Only returns on failure (a successful `exec` never returns — the
/// process becomes the new instance); the caller falls back to continuing
/// in this same process rather than getting stuck.
fn restart_self(wait_started: Option<std::time::SystemTime>) -> std::io::Error {
    let exe = match std::env::current_exe() {
        Ok(path) => path,
        Err(e) => return e,
    };
    let args: Vec<std::ffi::OsString> = std::env::args_os().skip(1).collect();
    let mut command = std::process::Command::new(exe);
    command.args(args);
    if let Some(started) = wait_started {
        let secs = started
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();
        command.env(ACCESSIBILITY_WAIT_STARTED_ENV, secs.to_string());
    }
    command.exec()
}

/// The wall-clock time the current Accessibility-grant wait originally
/// started — read from `ACCESSIBILITY_WAIT_STARTED_ENV` if this process was
/// itself the result of a `restart_self` (so the overall
/// `ACCESSIBILITY_GRANT_TIMEOUT` countdown survives across restarts),
/// otherwise `SystemTime::now()` for a genuinely first attempt.
fn accessibility_wait_started() -> std::time::SystemTime {
    std::env::var(ACCESSIBILITY_WAIT_STARTED_ENV)
        .ok()
        .and_then(|s| s.parse::<u64>().ok())
        .map(|secs| std::time::UNIX_EPOCH + std::time::Duration::from_secs(secs))
        .unwrap_or_else(std::time::SystemTime::now)
}

fn elapsed_since(started: std::time::SystemTime) -> std::time::Duration {
    std::time::SystemTime::now()
        .duration_since(started)
        .unwrap_or_default()
}

/// Unloads and removes this daemon's own LaunchAgent, the same end-state as
/// running `tili stop` — called when `main` gives up waiting for
/// Accessibility permission, so a timed-out daemon doesn't linger in a
/// half-broken state that only a manual restart could fix, and so the next
/// `tili start` is a clean retry. Mirrors `tili-cli`'s `stop_daemon`/
/// `launch_agent_path` (`crates/tili-cli/src/main.rs`) — duplicated rather
/// than shared, since it's a handful of lines; keep the plist path/label in
/// sync with that file if either changes. Best-effort: `launchctl unload`
/// may terminate this very process before it returns, so there's no
/// meaningful error handling to add beyond letting each step fail silently.
fn stop_self() {
    let Ok(home) = std::env::var("HOME") else {
        return;
    };
    let plist_path = std::path::PathBuf::from(home)
        .join("Library/LaunchAgents")
        .join("com.tili.daemon.plist");
    let _ = std::process::Command::new("launchctl")
        .args(["unload", "-w"])
        .arg(&plist_path)
        .status();
    let _ = std::fs::remove_file(&plist_path);
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

fn handle_event(state: &mut WmState, pending_pids: &mut HashSet<i32>, event: WmEvent) {
    match event {
        // Debounced (M-Phase 6): queued for `maintenance_tick` rather than
        // rescanned right here — a burst of notifications for the same pid
        // (common during a window's open/resize/move sequence) coalesces
        // into a single rescan instead of one per notification.
        WmEvent::WindowsChanged { pid } => {
            pending_pids.insert(pid);
        }
        WmEvent::AppTerminated { pid } => {
            // The process is gone — no point rescanning a pid that was
            // merely queued for one.
            pending_pids.remove(&pid);
            state.remove_app(pid);
        }
        WmEvent::AppLaunched { .. } => {
            // No-op: the watcher always follows this with a WindowsChanged
            // for the same pid once it has windows to report.
        }
        WmEvent::WindowFocused { .. } => {
            // No-op for now: nothing about the window set changed, so no
            // rescan/relayout is needed. `WmState`'s own focus tracking is
            // driven entirely by explicit focus/move commands, not synced
            // from real OS focus changes — this is exposed for a future
            // "follow the user's manual clicks" feature, not acted on yet.
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
/// configured — `WmState::on_mouse_moved`/`on_mouse_button_down`/
/// `on_mouse_button_up` are what actually gate on settings/state.
fn spawn_mouse_watcher_bridge() -> tokio::sync::mpsc::UnboundedReceiver<tili_ax::MouseSignal> {
    let sync_rx = tili_ax::spawn_mouse_watcher();
    let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
    std::thread::spawn(move || {
        while let Ok(signal) = sync_rx.recv() {
            if tx.send(signal).is_err() {
                break;
            }
        }
    });
    rx
}
