mod actions;
mod badge;
mod ipc;
mod menu;

use std::ptr::NonNull;
use std::sync::mpsc;
use std::time::Duration;

use objc2::MainThreadMarker;
use objc2_app_kit::{NSApplication, NSApplicationActivationPolicy};
use objc2_foundation::NSTimer;
use tray_icon::menu::MenuEvent;

/// How long to back off between reconnect attempts while the daemon is
/// unreachable — the *only* time this binary does anything resembling
/// polling. Once connected, `ipc::wait_for_change` blocks (zero CPU, no
/// timer) until the daemon actually reports something changed, so this
/// constant only matters during an outage, not steady-state operation.
const RECONNECT_BACKOFF: Duration = Duration::from_secs(1);

/// Consecutive failed reconnect attempts (at `RECONNECT_BACKOFF` apart)
/// before giving up and stopping this process for good — see `stop_self`.
/// The daemon and this badge are meant to run as a synchronized pair, not
/// one without the other, so a daemon that's been gone this long (roughly
/// a minute) is treated as "stopped on purpose," not a transient blip
/// (e.g. the brief restart a Homebrew upgrade's `post_install` triggers).
const MAX_CONSECUTIVE_FAILURES: u32 = 60;

/// How often the main-thread timer drains the change channel and applies
/// any pending snapshot. This is not a poll interval — the channel is
/// normally empty and this tick is a cheap no-op; it only has real work
/// to do right after `ipc::wait_for_change` unblocks on the background
/// thread, which can happen anywhere from instantly (a workspace switch)
/// up to the daemon's own 30s idle timeout.
const DRAIN_INTERVAL: f64 = 0.05;

fn main() {
    let mtm = MainThreadMarker::new().expect("tili-menubar must start on the main thread");
    let app = NSApplication::sharedApplication(mtm);
    // No Dock icon, no app-switcher entry — this is a background menu
    // bar accessory, matching how tili-daemon's own Info.plist marks
    // itself LSUIElement.
    app.setActivationPolicy(NSApplicationActivationPolicy::Accessory);

    let tray = menu::build_initial(mtm);

    // Only `Send` data (`Option<menu::Snapshot>`) ever crosses the thread
    // boundary — `tray` itself (an AppKit object, thread-affine to the
    // main thread) is never moved or referenced from the background
    // thread. The background thread blocks on `ipc::wait_for_change`
    // (real event-driven wakeups, not a sleep timer) and sends results
    // through the channel; a main-thread timer (below) drains it and
    // applies updates to `tray` — the standard Cocoa pattern for
    // background work feeding UI, rather than dispatching a block from a
    // non-main thread onto a main queue.
    let (tx, rx) = mpsc::channel::<Option<menu::Snapshot>>();
    std::thread::spawn(move || {
        // Polled once immediately, before ever blocking on
        // `wait_for_change` — otherwise the very first poll only happens
        // once *something changes*, which on a freshly-started daemon can
        // be up to `Command::WaitForChange`'s own 30s idle timeout away.
        // The badge would sit hidden (its `build_initial` state) that
        // whole time, only appearing once the user did something like
        // switch a workspace, instead of showing the daemon's actual
        // current state right away.
        let _ = tx.send(menu::poll_daemon());
        let mut consecutive_failures: u32 = 0;
        loop {
            match ipc::wait_for_change() {
                // Either something really changed, or the daemon's own
                // internal wait just timed out (both look the same here
                // by design — see Command::WaitForChange's doc comment)
                // — either way, fetch the real current state. Spawned onto
                // its own short-lived thread rather than run inline: this
                // loop needs to re-issue `wait_for_change` immediately, not
                // after `poll_daemon`'s two extra round trips finish —
                // `Command::WaitForChange`'s server-side wakeup isn't a
                // queue (see its doc comment), so any change firing during
                // that gap, while nothing here is subscribed yet, is missed
                // outright rather than just delivered late. Spamming a
                // workspace-switch hotkey landed in that gap often enough
                // to leave the badge stuck.
                Ok(_) => {
                    consecutive_failures = 0;
                    let tx = tx.clone();
                    std::thread::spawn(move || {
                        let _ = tx.send(menu::poll_daemon());
                    });
                }
                // Connection failed outright — the daemon isn't reachable
                // at all (not running, or between `tili stop`/`tili
                // start`). Report that immediately so the badge hides
                // (see menu::apply_snapshot), then back off before
                // retrying rather than hammering `connect()` in a tight
                // loop. After enough consecutive failures, stop trying —
                // see `MAX_CONSECUTIVE_FAILURES`.
                Err(_) => {
                    let _ = tx.send(None);
                    consecutive_failures += 1;
                    if consecutive_failures >= MAX_CONSECUTIVE_FAILURES {
                        eprintln!(
                            "tili-menubar: tili-daemon unreachable for \
                             {MAX_CONSECUTIVE_FAILURES} consecutive attempts — stopping \
                             rather than running without it."
                        );
                        stop_self();
                    }
                    std::thread::sleep(RECONNECT_BACKOFF);
                }
            }
        }
    });

    // Menu-click events: a plain channel, no AppKit calls needed to
    // receive them — every reaction (socket send, `open`, `tili stop`,
    // process exit) is thread-agnostic, so this stays off the main
    // thread deliberately, same reasoning as the poller above.
    std::thread::spawn(move || {
        let rx = MenuEvent::receiver();
        while let Ok(event) = rx.recv() {
            actions::handle(event.id());
        }
    });

    // Created and only ever invoked on the main thread (NSTimer fires on
    // the run loop it's scheduled on, here the main run loop `app.run()`
    // pumps below), so this closure never needs to be `Send` — it's free
    // to hold `tray`/`mtm`/`state` directly. `RcBlock::new` requires
    // `Fn`, not `FnMut` (a block can in principle be invoked reentrantly),
    // so `state` needs interior mutability rather than a plain `mut`
    // capture. See `menu::MenuState`'s doc comment for what it tracks and
    // why.
    let state = std::cell::RefCell::new(menu::MenuState::default());
    let drain = block2::RcBlock::new(move |_timer: NonNull<NSTimer>| {
        while let Ok(snapshot) = rx.try_recv() {
            menu::apply_snapshot(&tray, mtm, &snapshot, &mut state.borrow_mut());
        }
    });
    unsafe {
        NSTimer::scheduledTimerWithTimeInterval_repeats_block(DRAIN_INTERVAL, true, &drain);
    }

    app.run(); // never returns — parks the main thread in Cocoa's event loop
}

/// Unloads and removes this badge's own LaunchAgent, called once the daemon
/// has been unreachable for `MAX_CONSECUTIVE_FAILURES` attempts. A plain
/// `std::process::exit` alone wouldn't stick — `KeepAlive` in the plist
/// would just have launchd respawn this process right back into the same
/// dead end. Mirrors `tili-daemon`'s own `stop_self`
/// (`crates/tili-daemon/src/main.rs`) and `tili-cli`'s `stop_daemon`
/// (`crates/tili-cli/src/main.rs`); duplicated rather than shared, same
/// reasoning as those two — keep the plist path/label in sync with both if
/// either changes. Best-effort: `launchctl unload` may terminate this very
/// process before it returns.
fn stop_self() {
    let Ok(home) = std::env::var("HOME") else {
        std::process::exit(0);
    };
    let plist_path = std::path::PathBuf::from(home)
        .join("Library/LaunchAgents")
        .join("com.tili.menubar.plist");
    let _ = std::process::Command::new("launchctl")
        .args(["unload", "-w"])
        .arg(&plist_path)
        .status();
    let _ = std::fs::remove_file(&plist_path);
    std::process::exit(0);
}
