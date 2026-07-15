mod dispatch;
mod socket;
mod state;

use std::collections::HashSet;
use std::sync::{Arc, Mutex};

use dispatch::dispatch;
use state::WmState;
use tili_ax::WmEvent;
use tili_ipc::{Command, Response};

/// How long a `Command::WaitForChange` connection blocks before getting an
/// `Ok` response anyway, even if nothing changed — purely so a long-idle
/// connection doesn't sit blocked forever; callers don't need to
/// distinguish a timeout from a real change (see the command's own doc
/// comment in `tili-ipc`).
const WAIT_FOR_CHANGE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(30);

/// The daemon's bundle id, duplicated from `xtask/src/main.rs`'s `BUNDLE_ID`
/// const (used when wrapping the daemon in `tili.app`) — keep the two in
/// sync if either changes. Used only to target `tccutil reset` at our own
/// TCC entry; see `reset_accessibility_tcc`.
const ACCESSIBILITY_BUNDLE_ID: &str = "com.tili.daemon";

#[tokio::main]
async fn main() -> std::io::Result<()> {
    // IOHIDRequestAccess (Input Monitoring) must run before anything calls
    // into Accessibility's AXIsProcessTrustedWithOptions in this process's
    // lifetime — rdar://7381305: once Accessibility's check has run once,
    // Input Monitoring's prompt silently stops appearing for a first-time
    // grant. Don't reorder this below ensure_accessibility_permission(),
    // and don't add any AX call before it.
    tili_ax::request_input_monitoring_permission();

    let listener = socket::bind()?;
    println!(
        "tili-daemon: listening on {}",
        tili_ipc::default_socket_path().display()
    );

    if !tili_ax::ensure_accessibility_permission() {
        // No in-process wait/retry of any kind — confirmed on real hardware,
        // across several different mechanisms (plain sleep-based polling,
        // a run-loop-serviced polling thread, and re-testing with a stable
        // (non-ad-hoc) signing identity), that this process never reliably
        // observes a grant made after it started. Only a freshly *launched*
        // process's own check reflects reality. So: ask once (the prompting
        // call above already showed the system dialog), unload our own
        // LaunchAgent so launchd doesn't respawn us into the same dialog
        // again, and tell the user to re-run `tili start` themselves once
        // they've granted it — that next invocation is a genuinely fresh
        // process, which is the one case already proven to work.
        //
        // A dev binary rebuilt across iterations can shift code identity,
        // leaving TCC holding a stale grant record tied to a previous
        // signature that a plain trust check gets stuck against —
        // resetting here clears that before the user's next `tili start`
        // attempt (the same fix other AX-based tiling WMs apply for the
        // same dev-signing-churn problem). Best-effort: a raw unsigned
        // dev binary has no real bundle id for tccutil to match, so this
        // silently no-ops in that case; harmless either way.
        reset_accessibility_tcc();
        eprintln!(
            "tili-daemon: Accessibility permission not granted — grant it in System Settings \
             > Privacy & Security > Accessibility, then run `tili start` again."
        );
        stop_self();
        return Ok(());
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
    // Fired once per loop iteration below (deliberately coarse — see
    // `Command::WaitForChange`'s doc comment) so a `WaitForChange`
    // connection's dedicated task (spawned in the accept arm, never
    // touching `WmState` itself) can wake up without polling.
    let change_notify = Arc::new(tokio::sync::Notify::new());

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
        // Set by whichever branch below did something a `WaitForChange`
        // caller would plausibly care about — checked once after the
        // select! and only then fires `change_notify`. This is the reason
        // `maintenance_tick` (an unconditional 30ms timer, unrelated to
        // real activity) can't just notify unconditionally on every
        // branch: doing so would wake every blocked `WaitForChange`
        // connection ~33 times/sec regardless of whether anything
        // happened, defeating the entire point of replacing polling with
        // this. `Ping`/`ListWindows`/etc. reaching the generic socket arm
        // still count as "changed" even though they're read-only — rare
        // enough not to bother distinguishing, per `WaitForChange`'s own
        // "deliberately coarse" doc comment. `MouseSignal::Moved` is the
        // one left out entirely (not just gated): it fires every ~80ms
        // during any cursor movement, which is exactly the kind of
        // activity-independent-of-real-change cadence this is trying to
        // avoid; a `focus-follows-monitor` switch it triggers is picked up
        // on whatever the next genuinely-notifying event is, or worst
        // case `WAIT_FOR_CHANGE_TIMEOUT`.
        let mut changed = false;
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
                    Ok(Command::WaitForChange) => {
                        // Spawned rather than handled inline: this can
                        // legitimately block for up to
                        // WAIT_FOR_CHANGE_TIMEOUT, and the single select!
                        // loop must never stall on one connection while
                        // others (including ordinary commands) are
                        // waiting. Never touches `state` — only awaits
                        // `change_notify` and writes its own owned
                        // stream — so spawning it doesn't violate the
                        // "WmState mutates from one place only" invariant
                        // the rest of this loop relies on. Deliberately
                        // not counted as `changed` itself.
                        tokio::spawn(handle_wait_for_change(stream, change_notify.clone()));
                    }
                    Ok(command) => {
                        let response = dispatch(&mut state, command);
                        if let Err(e) = socket::write_response(&mut stream, &response).await {
                            eprintln!("tili-daemon: failed to write response: {e}");
                        }
                        sync_active_combos(&active_combos, &state);
                        changed = true;
                    }
                    Err(e) => eprintln!("tili-daemon: failed to read command: {e}"),
                }
            }
            event = events.recv() => {
                match event {
                    Some(event) => {
                        // AppLaunched/WindowFocused are no-ops in
                        // handle_event (see its own doc comments) — not
                        // worth waking a blocked WaitForChange caller for.
                        changed = matches!(
                            event,
                            WmEvent::WindowsChanged { .. } | WmEvent::AppTerminated { .. }
                        );
                        handle_event(&mut state, &mut pending_pids, event);
                    }
                    None => eprintln!("tili-daemon: event watcher channel closed unexpectedly"),
                }
            }
            _ = maintenance_tick.tick() => {
                changed = !pending_pids.is_empty();
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
                changed = true;
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
                    changed = true;
                }
                sync_active_combos(&active_combos, &state);
            }
            Some(()) = displays_changed.recv() => {
                // A monitor connected/disconnected/reconfigured (M9) — the
                // callback doesn't say which, so just re-enumerate.
                state.on_displays_changed();
                changed = true;
            }
            Some(signal) = mouse_events.recv() => {
                match signal {
                    // Throttled cursor positions (M10, focus-follows-monitor)
                    // — a no-op inside `on_mouse_moved` unless that setting
                    // is on. Deliberately excluded from `changed` — see this
                    // loop's own top-of-body comment for why.
                    tili_ax::MouseSignal::Moved(x, y) => state.on_mouse_moved(x, y),
                    // Suppresses relayout for the duration of a drag-resize
                    // (M10.1) so `apply_windows_changed` doesn't fight the
                    // user's drag; button-up relays out once to snap back
                    // to the tiled layout.
                    tili_ax::MouseSignal::ButtonDown => {
                        state.on_mouse_button_down();
                        changed = true;
                    }
                    tili_ax::MouseSignal::ButtonUp => {
                        state.on_mouse_button_up();
                        changed = true;
                    }
                }
            }
        }
        if changed {
            change_notify.notify_waiters();
        }
    }

    let _ = std::fs::remove_file(tili_ipc::default_socket_path());
    Ok(())
}

/// Best-effort `tccutil reset Accessibility <bundle-id>` for our own bundle
/// id — see the call site in `main` for why. Discards the result entirely:
/// a raw unsigned dev binary has no real bundle id for tccutil to match, so
/// this silently no-ops in that case, and a failure here has no fallback
/// worth taking beyond continuing to poll normally.
fn reset_accessibility_tcc() {
    let _ = std::process::Command::new("tccutil")
        .args(["reset", "Accessibility", ACCESSIBILITY_BUNDLE_ID])
        .status();
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

/// Handles one `Command::WaitForChange` connection entirely off the main
/// select! loop: blocks on `change_notify` (or `WAIT_FOR_CHANGE_TIMEOUT`,
/// whichever comes first), then responds `Ok` — never touches `WmState`,
/// so running many of these concurrently alongside the main loop is safe.
async fn handle_wait_for_change(
    mut stream: tokio::net::UnixStream,
    change_notify: Arc<tokio::sync::Notify>,
) {
    let _ = tokio::time::timeout(WAIT_FOR_CHANGE_TIMEOUT, change_notify.notified()).await;
    let _ = socket::write_response(&mut stream, &Response::Ok).await;
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
            // No-op: `WmState`'s own focus tracking is instead resolved
            // synchronously at the top of every `dispatch()` call (see
            // `WmState::sync_focus_from_frontmost`) — confirmed on real
            // hardware that resyncing reactively, whenever this event
            // happens to arrive, has an unavoidable race against the next
            // hotkey press, since there's no ordering guarantee between the
            // two. Reacting to this event too would be redundant with that
            // synchronous resync, not a fallback for it.
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
