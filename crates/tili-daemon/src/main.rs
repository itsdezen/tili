mod dispatch;
mod socket;
mod state;

use std::collections::HashSet;
use std::sync::{Arc, Mutex};

use dispatch::dispatch;
use objc2::MainThreadMarker;
use objc2_app_kit::{NSApplication, NSApplicationActivationPolicy};
use state::WmState;
use tili_ax::WmEvent;
use tili_ipc::{Command, Response};

/// How long a `Command::WaitForChange` connection blocks before getting an
/// `Ok` response anyway, even if nothing changed — purely so a long-idle
/// connection doesn't sit blocked forever; callers don't need to
/// distinguish a timeout from a real change (see the command's own doc
/// comment in `tili-ipc`).
const WAIT_FOR_CHANGE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(30);

/// How long `pending_reveal_deadline` waits before `maintenance_tick`
/// actually runs the deferred `WmState::reveal_current_frontmost`. Kept
/// short: `WmEvent::AppLaunched` doesn't reliably fire for every app launch
/// that ends up racing this reveal, so `pending_launch_pids` can't be
/// counted on to be populated by the time this deadline expires — the
/// actual defense against a spurious switch is `reveal_frontmost`'s
/// `suppress` check (a previous pid owning zero live windows), which is
/// independent of this delay. A longer delay buys no proven benefit against
/// that gap while making ordinary Cmd-Tab/Dock-click-reveal feel laggy, so
/// this stays short rather than growing to cover a full app launch.
const REVEAL_DEBOUNCE: std::time::Duration = std::time::Duration::from_millis(100);

/// The daemon's bundle id, duplicated from `xtask/src/main.rs`'s `BUNDLE_ID`
/// const (used when wrapping the daemon in `tili.app`) — keep the two in
/// sync if either changes. Used only to target `tccutil reset` at our own
/// TCC entry; see `reset_accessibility_tcc`.
const ACCESSIBILITY_BUNDLE_ID: &str = "com.tili.daemon";

/// The real process entry point — deliberately not `#[tokio::main]`. A real
/// `NSApplication` instance needs its `run()` to own the actual OS process
/// main thread (the way `tili-menubar/src/main.rs` already does) for
/// `NSWorkspace` notifications to be delivered reliably: confirmed on real
/// hardware that `NSWorkspaceDidLaunchApplicationNotification`/
/// `DidWakeNotification` were never delivered to this process when it had
/// no `NSApplication` at all and Tokio's `block_on` occupied the main
/// thread instead (see `tili_ax::workspace::register_on_main`'s doc comment
/// for the full history). So the whole existing async daemon body moves to
/// `async_daemon_main`, run on a background thread with its own Tokio
/// runtime, while this real `fn main()` sets up `NSApplication` and parks
/// the actual main thread in `app.run()`.
///
/// Every exit path of `async_daemon_main` must reach `std::process::exit`
/// below — unlike before, simply returning from it no longer ends the
/// process, since `app.run()` on the main thread never returns on its own.
/// Without this, the background thread would end quietly while `app.run()`
/// stayed parked forever: a zombie process that answers no commands and
/// never disappears from `ps`.
fn main() {
    // IOHIDRequestAccess (Input Monitoring) must run before anything calls
    // into Accessibility's AXIsProcessTrustedWithOptions in this process's
    // lifetime — rdar://7381305: once Accessibility's check has run once,
    // Input Monitoring's prompt silently stops appearing for a first-time
    // grant. Kept as the literal first statement of the real process (not
    // just of `async_daemon_main`'s body) so this ordering guarantee holds
    // unconditionally, rather than trusting that `NSApplication` setup
    // below makes no AX call of its own.
    tili_ax::request_input_monitoring_permission();

    let mtm =
        MainThreadMarker::new().expect("tili-daemon must start on the real process main thread");
    let app = NSApplication::sharedApplication(mtm);
    // No Dock icon, no app-switcher entry — matches tili-daemon's own
    // Info.plist LSUIElement marking and tili-menubar's identical policy.
    app.setActivationPolicy(NSApplicationActivationPolicy::Accessory);

    // Must happen here, on the real main thread, before `app.run()` starts
    // pumping it — see `register_on_main`'s doc comment.
    let app_event_rx = tili_ax::register_on_main(mtm);

    std::thread::spawn(move || {
        let rt = tokio::runtime::Runtime::new().expect("failed to build tokio runtime");
        let result = rt.block_on(async_daemon_main(app_event_rx));
        if let Err(e) = &result {
            eprintln!("tili-daemon: fatal error: {e}");
        }
        std::process::exit(if result.is_ok() { 0 } else { 1 });
    });

    app.run(); // never returns — parks the real main thread in Cocoa's event loop
}

async fn async_daemon_main(
    app_event_rx: std::sync::mpsc::Receiver<tili_ax::AppEvent>,
) -> std::io::Result<()> {
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

    let mut events = tili_ax::spawn_event_watcher(app_event_rx);
    let mut config_updates = spawn_config_reload_bridge();
    let active_combos = Arc::new(Mutex::new(HashSet::new()));
    let mut hotkeys = tili_ax::spawn_hotkey_tap(active_combos.clone());
    let mut displays_changed = tili_ax::spawn_display_watcher();
    let mut mouse_events = tili_ax::spawn_mouse_watcher();
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
    // Armed by `MouseSignal::ButtonUp` *and* `WmEvent::FrontmostAppChanged`
    // (both funnel into this one deferred check rather than calling
    // `WmState::reveal_frontmost`/`reveal_current_frontmost` synchronously)
    // — gives a same-trigger `WmEvent::AppLaunched` (a separate,
    // independently-timed async source with no ordering guarantee against
    // either trigger) a window to land in `pending_launch_pids` before the
    // deferred check runs, on the chance it closes a race where a
    // stale/transient `frontmost_app_pid()` read during a cold app launch
    // causes a spurious workspace switch. A real bounded deadline rather
    // than "next `maintenance_tick`" (30ms), since `AppLaunched`'s latency
    // isn't bounded by that tick's own interval. See `REVEAL_DEBOUNCE` and
    // `WmState::reveal_current_frontmost`'s doc comment for why this isn't
    // the primary defense against the spurious switch.
    let mut pending_reveal_deadline: Option<tokio::time::Instant> = None;
    // Snapshot of `WmState::switch_epoch()` taken whenever
    // `pending_reveal_deadline` above is armed — only meaningful while
    // that deadline is `Some`. Lets `maintenance_tick` detect a newer,
    // explicit `switch_workspace` call (a rapid workspace-switch hotkey,
    // e.g.) that superseded whatever triggered this deferred reveal, and
    // skip running it instead of reverting the user's later navigation.
    let mut pending_reveal_epoch: u64 = 0;
    // Whether the *most recent* arm of `pending_reveal_deadline` was a real
    // `MouseSignal::ButtonUp` click rather than a notification-detected
    // `WmEvent::FrontmostAppChanged` edge — forwarded to
    // `WmState::reveal_current_frontmost` as `allow_unchanged_pid`. A click
    // needs `true` (see that function's doc comment for the legitimate
    // Dock-icon-reactivation case this covers); a notification edge needs
    // `false`, since by the time the deferred check actually runs a
    // same-pid read means nothing really changed and chasing it would
    // revert whatever workspace switch the user made in the meantime.
    let mut pending_reveal_allow_unchanged = false;
    // Drains `pending_pids`, rechecks Phase 5's `pending_removal` grace
    // period, and now also the reveal debounce above and
    // `pending_launch_pids`'s grace period — one combined periodic-
    // maintenance branch rather than several, since all of these are just
    // "a little time has passed, go recheck something."
    let mut maintenance_tick = tokio::time::interval(std::time::Duration::from_millis(30));
    // `Delay` instead of tokio's default `Burst`: if this loop is ever
    // transiently busy long enough to miss a tick, `Burst` fires every
    // missed interval back-to-back the moment the loop frees up, which
    // just adds a redundant catch-up spike of otherwise-identical
    // maintenance work; `Delay` schedules the next tick relative to when
    // the late one actually fired instead.
    maintenance_tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    // Drives `TweenedFrameSetter` animation steps, at whatever period
    // `Settings::animate` (`Medium`/`High`) currently calls for — a much
    // shorter period than `maintenance_tick` since smoothness directly
    // depends on it, and gated by `if state.is_animating_anything()` on
    // its `select!` branch below so it's never even polled, let alone
    // fires, while nothing is animating (the common case, and always the
    // case with `Settings::animate` off) — this is what keeps it from
    // being a "fourth" always-on polling exception alongside the three in
    // `docs/architecture/invariants.md`: it isn't polling for a state
    // change that could be event-driven instead, it's the only way to
    // drive a process (interpolating over wall-clock time) that has no
    // event-driven alternative in the first place, and it costs nothing
    // at all outside the short window an animation is actually running.
    // Placeholder period here — `sync_animation_tick` (called right after
    // every `apply_config`, including the initial load below) corrects it
    // to whatever the just-loaded config actually calls for before this
    // is ever polled.
    let mut animation_tick = tokio::time::interval(std::time::Duration::from_millis(16));
    animation_tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    let mut animation_tick_period = std::time::Duration::from_millis(16);

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
    sync_animation_tick(&mut animation_tick, &mut animation_tick_period, &state);

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
        // this. The generic socket arm below excludes read-only commands
        // (`Ping`/`ListWindows`/`ListWorkspaces`/`ListMonitors`) from this
        // for the same reason, not just "rare enough not to bother" —
        // `tili-menubar`'s own steady-state polling calls exactly those
        // commands every time it wakes up, so counting them as "changed"
        // made every poll re-wake itself: a self-sustaining loop that
        // turned this long-poll design back into continuous polling (and
        // starved a real command, like a menu click, queued behind it on
        // this single-threaded accept loop) the instant the first real
        // change ever happened. `MouseSignal::Moved` is excluded entirely
        // (not just gated): it fires every ~80ms during any cursor
        // movement, which is exactly the kind of activity-independent-of-
        // real-change cadence this is trying to avoid; a
        // `focus-follows-monitor` switch it triggers is picked up on
        // whatever the next genuinely-notifying event is, or worst case
        // `WAIT_FOR_CHANGE_TIMEOUT`.
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
                        //
                        // `notified_owned()` is called *here*, synchronously,
                        // rather than inside the spawned task — `Notify`
                        // snapshots how many times `notify_waiters()` has
                        // fired at the exact moment it's called, so a rapid
                        // burst of changes (e.g. spamming a workspace-switch
                        // hotkey) firing `notify_waiters()` between this
                        // task being spawned and actually reaching its
                        // `.await` would otherwise be invisible to it — a
                        // silently missed wakeup that leaves the caller (the
                        // menubar badge) stuck showing a stale workspace
                        // until the next unrelated change or the timeout.
                        // Capturing the baseline before `tokio::spawn` closes
                        // that window; `Notify::notified_owned` exists
                        // specifically for moving a pre-captured wait into a
                        // spawned task like this.
                        let notified = change_notify.clone().notified_owned();
                        tokio::spawn(handle_wait_for_change(stream, notified));
                    }
                    Ok(command) => {
                        // Read-only queries never mutate `WmState`, so they
                        // can never be a change a blocked `WaitForChange`
                        // caller would care about — see this loop's
                        // top-of-body comment for why that distinction
                        // matters now (it didn't always).
                        let mutates = !matches!(
                            command,
                            Command::Ping
                                | Command::ListWindows
                                | Command::ListWorkspaces
                                | Command::ListMonitors
                        );
                        let response = dispatch(&mut state, command);
                        if let Err(e) = socket::write_response(&mut stream, &response).await {
                            eprintln!("tili-daemon: failed to write response: {e}");
                        }
                        sync_active_combos(&active_combos, &state);
                        changed = mutates;
                    }
                    Err(e) => eprintln!("tili-daemon: failed to read command: {e}"),
                }
            }
            event = events.recv() => {
                match event {
                    Some(event) => {
                        // AppLaunched only updates internal bookkeeping
                        // (`pending_launch_pids`) and WindowFocused is a
                        // true no-op (see handle_event's own doc comments)
                        // — neither is worth waking a blocked WaitForChange
                        // caller for.
                        changed = matches!(
                            event,
                            WmEvent::WindowsChanged { .. } | WmEvent::AppTerminated { .. }
                        );
                        handle_event(
                            &mut state,
                            &mut pending_pids,
                            &mut pending_reveal_deadline,
                            &mut pending_reveal_epoch,
                            &mut pending_reveal_allow_unchanged,
                            event,
                        );
                    }
                    None => eprintln!("tili-daemon: event watcher channel closed unexpectedly"),
                }
            }
            _ = animation_tick.tick(), if state.is_animating_anything() => {
                state.step_animations();
            }
            _ = maintenance_tick.tick() => {
                let pids_changed = !pending_pids.is_empty();
                for pid in pending_pids.drain() {
                    let windows = tili_ax::list_windows_for_pid(pid);
                    state.apply_windows_changed(pid, windows);
                }
                state.finalize_expired_removals();
                state.finalize_expired_launches();

                // Checked after the pids above so a WindowsChanged for a
                // just-launched pid that lands in the same tick the
                // deadline expires already cleared it from
                // `pending_launch_pids` before this decides whether to skip.
                let reveal_due = pending_reveal_deadline.is_some_and(|d| tokio::time::Instant::now() >= d);
                if reveal_due {
                    pending_reveal_deadline = None;
                    // A newer, explicit `switch_workspace` call (e.g. a
                    // rapid workspace-switch hotkey) already superseded
                    // whatever armed this deferred reveal — that call is
                    // authoritative, so drop the stale reveal instead of
                    // reverting the user's later navigation.
                    if pending_reveal_epoch == state.switch_epoch() {
                        state.reveal_current_frontmost(pending_reveal_allow_unchanged);
                    }
                }
                changed = pids_changed || reveal_due;
            }
            Some(config) = config_updates.recv() => {
                println!("tili-daemon: config reloaded from {}", config_path.display());
                state.apply_config(&config);
                sync_active_combos(&active_combos, &state);
                sync_animation_tick(&mut animation_tick, &mut animation_tick_period, &state);
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
                        // Catches a Dock icon click reactivating an app
                        // that was already the OS's nominal frontmost
                        // application (common when the current workspace
                        // is empty) — see `WmState::reveal_current_frontmost`'s
                        // doc comment for why `FrontmostAppChanged` never
                        // fires for that case on its own. Deferred by
                        // `REVEAL_DEBOUNCE` rather than called here directly
                        // — see `pending_reveal_deadline`'s doc comment.
                        pending_reveal_deadline = Some(tokio::time::Instant::now() + REVEAL_DEBOUNCE);
                        pending_reveal_epoch = state.switch_epoch();
                        pending_reveal_allow_unchanged = true;
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
/// select! loop: blocks on `notified` (or `WAIT_FOR_CHANGE_TIMEOUT`,
/// whichever comes first), then responds `Ok` — never touches `WmState`,
/// so running many of these concurrently alongside the main loop is safe.
/// `notified` is an already-captured `OwnedNotified` (see the accept arm's
/// comment for why it must be captured before this task is spawned, not
/// inside it).
async fn handle_wait_for_change(
    mut stream: tokio::net::UnixStream,
    notified: tokio::sync::futures::OwnedNotified,
) {
    let _ = tokio::time::timeout(WAIT_FOR_CHANGE_TIMEOUT, notified).await;
    let _ = socket::write_response(&mut stream, &Response::Ok).await;
}

fn handle_event(
    state: &mut WmState,
    pending_pids: &mut HashSet<i32>,
    pending_reveal_deadline: &mut Option<tokio::time::Instant>,
    pending_reveal_epoch: &mut u64,
    pending_reveal_allow_unchanged: &mut bool,
    event: WmEvent,
) {
    match event {
        // Debounced (M-Phase 6): queued for `maintenance_tick` rather than
        // rescanned right here — a burst of notifications for the same pid
        // (common during a window's open/resize/move sequence) coalesces
        // into a single rescan instead of one per notification.
        WmEvent::WindowsChanged { pid } => {
            pending_pids.insert(pid);
        }
        WmEvent::AppTerminated { pid } => {
            eprintln!("tili-daemon: NSWorkspace AppTerminated pid={pid}");
            // The process is gone — no point rescanning a pid that was
            // merely queued for one.
            pending_pids.remove(&pid);
            state.remove_app(pid);
        }
        WmEvent::AppLaunched { pid, .. } => {
            eprintln!("tili-daemon: NSWorkspace AppLaunched pid={pid}");
            // The watcher always follows this with a `WindowsChanged` for
            // the same pid once it has windows to report — this just marks
            // `pid` in `pending_launch_pids` in the meantime, so
            // `reveal_current_frontmost` knows not to trust a
            // `frontmost_app_pid()` read until then (see that field's doc
            // comment on `WmState`).
            state.note_app_launched(pid);
        }
        WmEvent::SystemDidWake => {
            eprintln!("tili-daemon: NSWorkspace SystemDidWake received");
            state.note_system_wake();
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
        WmEvent::FrontmostAppChanged { pid } => {
            eprintln!("tili-daemon: NSWorkspace FrontmostAppChanged pid={pid}");
            // Unlike `WindowFocused` above, this fires for a pure OS-level
            // frontmost-app change (Cmd-Tab, Mission Control/Control Center)
            // that would otherwise never route through `dispatch()` at all
            // — the only path that reveals a parked workspace. Deferred
            // through the same `pending_reveal_deadline` mechanism as
            // `MouseSignal::ButtonUp` (re-deriving whatever's frontmost
            // fresh via `reveal_current_frontmost` once the deadline fires,
            // rather than trusting this event's own `pid`) instead of
            // calling `WmState::reveal_frontmost(pid)` directly — see
            // `REVEAL_DEBOUNCE`'s doc comment for why: this event fires for
            // *real* AX transitions (unlike a stale click-time read), but
            // dismissing Spotlight right after launching a cold app
            // produces a real, transient transition back to whatever was
            // frontmost before Spotlight, and following that unconditionally
            // (`reveal_frontmost` always follows a system-UI previous pid —
            // see its doc comment) raced the same way a stale click read
            // did.
            *pending_reveal_deadline = Some(tokio::time::Instant::now() + REVEAL_DEBOUNCE);
            *pending_reveal_epoch = state.switch_epoch();
            // Unlike a real click, a notification-detected edge alone never
            // justifies chasing a same-pid read once the deferred check
            // actually runs — see `WmState::reveal_frontmost`'s
            // `allow_unchanged_pid` doc comment.
            *pending_reveal_allow_unchanged = false;
        }
    }
}

fn sync_active_combos(shared: &Arc<Mutex<HashSet<tili_ax::KeyCombo>>>, state: &WmState) {
    if let Ok(mut set) = shared.lock() {
        *set = state.active_key_combos();
    }
}

/// Reconstructs `animation_tick` with `state`'s current
/// `animation_tick_period` if it's changed since the last sync — `Off`'s
/// `None` is skipped entirely since that period is meaningless (the
/// tick's own `select!` guard already keeps it from firing under `Off`
/// regardless of period). `tokio::time::Interval` has no API to change an
/// existing interval's period in place, so a period change means building
/// a new one and reapplying `MissedTickBehavior::Delay`.
fn sync_animation_tick(
    animation_tick: &mut tokio::time::Interval,
    current_period: &mut std::time::Duration,
    state: &WmState,
) {
    let Some(period) = state.animation_tick_period() else {
        return;
    };
    if period == *current_period {
        return;
    }
    *animation_tick = tokio::time::interval(period);
    animation_tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    *current_period = period;
}

/// Bridges `tili_config`'s plain-`std::sync::mpsc` file-watcher (see its
/// module docs for why it isn't async itself) into a tokio channel this
/// daemon's `select!` loop can read from directly. The only remaining
/// bridge of this shape — `tili-ax`'s own watchers
/// (`spawn_hotkey_tap`/`spawn_display_watcher`/`spawn_mouse_watcher`) build
/// and send on a `tokio::sync::mpsc` channel directly from their own
/// dedicated thread instead, since `tili-ax` already depends on Tokio (see
/// each function's doc comment); `tili_config` deliberately does not, so it
/// still needs this separate relay thread.
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
