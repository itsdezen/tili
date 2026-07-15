use std::collections::{BTreeSet, HashMap, HashSet};
use std::sync::mpsc::RecvTimeoutError;
use std::time::{Duration, Instant};

use axuielement::AXUIElement;
use axuielement::async_api::AXNotificationStream;
use axuielement::ax_notification::{
    AX_FOCUSED_WINDOW_CHANGED_NOTIFICATION, AX_MAIN_WINDOW_CHANGED_NOTIFICATION,
    AX_WINDOW_CREATED_NOTIFICATION, AX_WINDOW_MOVED_NOTIFICATION, AX_WINDOW_RESIZED_NOTIFICATION,
    AXUI_ELEMENT_DESTROYED_NOTIFICATION,
};
use tokio::sync::mpsc;

use crate::enumerate;
use crate::workspace::{self, AppEvent};

/// One notification name registered per watched app; any of these firing
/// means "re-read this process's windows," so the exact notification
/// content doesn't need to be threaded through — see the module docs below.
/// `AX_FOCUSED_WINDOW_CHANGED_NOTIFICATION`/`AX_MAIN_WINDOW_CHANGED_NOTIFICATION`
/// are dispatched separately, as `WmEvent::WindowFocused` — see `watch_app`.
///
/// Deliberately excludes `AX_TITLE_CHANGED_NOTIFICATION`: title has no
/// bearing on layout (it's only read once, at window creation, for
/// floating-rule title matching), and some apps — terminal emulators with a
/// prompt that embeds cwd/git-branch/clock, in particular — fire it
/// extremely often. Watching it turned every keystroke-driven title update
/// into a full rescan-and-relayout cycle for that app, which was fast
/// enough to flood the daemon's event loop and starve/delay everything else
/// (including workspace-switch commands) behind a backlog of no-op
/// relayouts. `list-windows`'s cached title just goes briefly stale between
/// real window events instead — an acceptable trade.
const WINDOW_NOTIFICATIONS: &[&str] = &[
    AX_WINDOW_CREATED_NOTIFICATION,
    AXUI_ELEMENT_DESTROYED_NOTIFICATION,
    AX_WINDOW_MOVED_NOTIFICATION,
    AX_WINDOW_RESIZED_NOTIFICATION,
    AX_FOCUSED_WINDOW_CHANGED_NOTIFICATION,
    AX_MAIN_WINDOW_CHANGED_NOTIFICATION,
];

/// How often the background watcher thread re-derives "every pid that
/// should currently have a window-notification subscription," attaching
/// watchers for newly-discovered ones and detaching (with a synthetic
/// `AppTerminated`) for ones no longer running.
///
/// This is a correctness backstop, not the primary mechanism: push-based
/// detection (`NSWorkspaceDidLaunchApplicationNotification`/
/// `DidTerminateApplication` for process launch/quit) is still what makes
/// this feel instant in the common case. But both have been observed to
/// simply not fire in practice for some processes (no code-level bug
/// found — a real notification-delivery gap), which would otherwise leave
/// such an app permanently unwatched, or its windows never cleaned out of
/// the tree after it quit (a "ghost gap"), for the rest of the daemon's
/// life. Kept short (250ms) since a missed launch/quit should feel close
/// to instant, not just "eventually correct" — cheap to do this often
/// because this tick, unlike the full-window resync below, never triggers a
/// relayout: it's just pid enumeration plus attaching/detaching watchers.
///
/// Deliberately *doesn't* also re-signal `WindowsChanged` for every
/// on-screen pid on every tick (an earlier version did): each one triggers
/// `apply_windows_changed`, which unconditionally relays out the active
/// workspace at the end regardless of whether anything for that pid
/// actually changed — with several on-screen apps, firing all of them
/// back-to-back every tick caused a visible relayout stutter for no
/// reason. See `FULL_RESYNC_DEBOUNCE`/`FULL_RESYNC_MAX_INTERVAL` for that
/// much rarer sweep.
///
/// This is the third sanctioned exception to "no polling" (see
/// `tili-daemon/src/main.rs`'s Accessibility-grant wait and
/// `tili-ax/src/hotkey.rs`'s hotkey-tap retry for the other two) —
/// documented in `CLAUDE.md`.
const RESYNC_INTERVAL: Duration = Duration::from_millis(250);

/// How much a genuine `AppEvent` (app launch/terminate) pushes back the next
/// expensive full-window resync (see `FULL_RESYNC_MAX_INTERVAL`) — real
/// activity means the cheap tick and push notifications are already
/// actively reconciling state, so the expensive sweep can afford to wait a
/// couple seconds rather than firing immediately in the middle of a burst.
const FULL_RESYNC_DEBOUNCE: Duration = Duration::from_secs(2);

/// Hard cap on how long the expensive full-window resync (re-signaling
/// `WindowsChanged` for every on-screen pid — a rare safety net for a
/// missed `AXObserver` window-level notification, which empirically fires
/// reliably in the common case, unlike the app-level launch/terminate
/// notifications `RESYNC_INTERVAL` itself guards against) can be deferred.
/// Every genuine `AppEvent` pushes the deadline forward by
/// `FULL_RESYNC_DEBOUNCE`, but this cap guarantees a sweep still happens on
/// an otherwise-quiet system rather than deferring forever.
/// `apply_windows_changed` diffs against its cache and is a no-op if
/// nothing actually changed, but its unconditional relayout at the end
/// still costs a real pass over the active workspace, hence keeping this
/// infrequent rather than running it every `RESYNC_INTERVAL` tick.
const FULL_RESYNC_MAX_INTERVAL: Duration = Duration::from_secs(20);

/// Something the daemon should react to. `WindowsChanged` is deliberately
/// coarse-grained: it doesn't say *what* changed about a process's windows
/// (created vs moved vs resized vs destroyed) — callers re-read that
/// process's windows via `enumerate::list_windows_for_pid` in response,
/// which is cheap (a handful of AX calls) and sidesteps having to reason
/// about whether a specific `AXUIElement` handle is still valid to query at
/// the moment its destroyed-notification arrives. `WindowFocused` is kept
/// separate since nothing about the window set itself changed — just which
/// window is focused/main within the app.
#[derive(Debug, Clone)]
pub enum WmEvent {
    AppLaunched {
        pid: i32,
        bundle_id: Option<String>,
    },
    AppTerminated {
        pid: i32,
    },
    WindowsChanged {
        pid: i32,
    },
    WindowFocused {
        pid: i32,
    },
    /// The system-wide frontmost application changed to a *different* pid —
    /// checked on the same `RESYNC_INTERVAL` tick as `resync_watchers`
    /// (`workspace::frontmost_app_pid`, a direct AX query, not a
    /// notification). This is the only signal that catches Cmd-Tab or a
    /// Mission Control/Control Center click switching to an app whose
    /// window lives in a currently-parked workspace — neither
    /// `NSWorkspaceDidActivateApplicationNotification` (dead for this
    /// process, see `workspace::frontmost_app_pid`'s doc comment) nor the
    /// per-window `WindowFocused` event above reacts to a pure OS-level
    /// frontmost change that doesn't also move focus within an already
    /// on-screen app.
    FrontmostAppChanged {
        pid: i32,
    },
}

/// Starts watching for window/app lifecycle events and returns a channel
/// that receives a `WmEvent` each time something changes. Must be called
/// from within a Tokio runtime (it captures the current `Handle` to spawn
/// per-app watch tasks from a plain OS thread that isn't itself part of the
/// runtime).
///
/// Already-running apps are seeded as synthetic `WindowsChanged` events
/// immediately, so callers don't need a separate "initial full scan" path —
/// starting from an empty cache and reacting to this channel is sufficient
/// both at startup and for the rest of the process's life.
pub fn spawn_event_watcher() -> mpsc::UnboundedReceiver<WmEvent> {
    let rt = tokio::runtime::Handle::current();
    let (event_tx, event_rx) = mpsc::unbounded_channel();

    let (app_tx, app_rx) = std::sync::mpsc::channel();
    workspace::spawn_workspace_watcher(app_tx);

    let mut watched: HashMap<i32, tokio::task::JoinHandle<()>> = HashMap::new();
    // Pids a subscription attempt has already failed for (e.g. WindowServer,
    // pid 403 — a system compositor process, not a real app, that shows up
    // in `onscreen_owner_pids` as the owner of some overlay windows but will
    // never support an AX subscription). Without this, `resync_watchers`
    // would retry — and log a failure for — the exact same permanently
    // unwatchable pid on every single `RESYNC_INTERVAL` tick forever,
    // wasting AX calls and flooding the log. Cleared when the pid actually
    // disappears (see the `retain` below and `AppEvent::Terminated`), so a
    // *new* process reusing the same pid gets a fresh attempt.
    let mut unwatchable: HashSet<i32> = HashSet::new();
    seed_watchers(&rt, &event_tx, &mut watched, &mut unwatchable);

    std::thread::spawn(move || {
        // Debounce-since-quiet with a hard cap: a burst of real `AppEvent`s
        // pushes `debounce_deadline` 2s past the *last* one (so the cheap
        // tick + push notifications get first crack at reconciling state
        // before paying for a full sweep), but `last_full_resync` bounds
        // that indefinitely — a quiet system still gets swept at least
        // every `FULL_RESYNC_MAX_INTERVAL`.
        let mut last_full_resync = Instant::now();
        let mut debounce_deadline: Option<Instant> = None;
        // Edge-triggered: only `Some(pid)` values that actually differ from
        // the last tick emit `FrontmostAppChanged`, so revealing a parked
        // workspace (which ends by raising/focusing the same pid already
        // recorded here) doesn't cause the very next tick to re-detect a
        // "change" and loop. Deliberately never overwritten with a bare
        // `None` read (see below) — confirmed on real hardware that
        // `frontmost_app_pid()` (`AXFocusedApplication` off the system-wide
        // element) transiently reads `None` for one tick right after
        // `park()` moves a still-real-macOS-frontmost app's window into its
        // barely-on-screen corner sliver, even though no other app actually
        // took focus. If that transient `None` were allowed to overwrite
        // this, the very next tick reading the *same* still-frontmost pid
        // would look like a fresh change and wrongly fire
        // `FrontmostAppChanged` — which `reveal_frontmost` treats as "the
        // user Cmd-Tabbed to this app," yanking the display straight back
        // to whatever (possibly now-empty) workspace that pid's window
        // belongs to and undoing a manual switch to an empty workspace.
        let mut last_frontmost_pid: Option<i32> = None;
        loop {
            match app_rx.recv_timeout(RESYNC_INTERVAL) {
                Ok(AppEvent::Launched { pid, bundle_id }) => {
                    let _ = event_tx.send(WmEvent::AppLaunched { pid, bundle_id });
                    match watch_app(&rt, pid, event_tx.clone()) {
                        Some(handle) => {
                            watched.insert(pid, handle);
                        }
                        None => {
                            unwatchable.insert(pid);
                        }
                    }
                    let _ = event_tx.send(WmEvent::WindowsChanged { pid });
                    debounce_deadline = Some(Instant::now() + FULL_RESYNC_DEBOUNCE);
                }
                Ok(AppEvent::Terminated { pid }) => {
                    let _ = event_tx.send(WmEvent::AppTerminated { pid });
                    unwatchable.remove(&pid);
                    if let Some(handle) = watched.remove(&pid) {
                        // Aborting drops the owned AXNotificationStream, which
                        // stops its dedicated CFRunLoop thread (see
                        // axuielement's `ObserverThreadHandle::drop`).
                        handle.abort();
                    }
                    debounce_deadline = Some(Instant::now() + FULL_RESYNC_DEBOUNCE);
                }
                Err(RecvTimeoutError::Timeout) => {
                    let now = Instant::now();
                    let debounce_ready = debounce_deadline.is_some_and(|due| now >= due);
                    let cap_exceeded =
                        now.duration_since(last_full_resync) >= FULL_RESYNC_MAX_INTERVAL;
                    let full_window_resync = debounce_ready || cap_exceeded;
                    if full_window_resync {
                        last_full_resync = now;
                        debounce_deadline = None;
                    }
                    resync_watchers(
                        &rt,
                        &event_tx,
                        &mut watched,
                        &mut unwatchable,
                        full_window_resync,
                    );

                    if let Some(pid) = workspace::frontmost_app_pid() {
                        if last_frontmost_pid != Some(pid) {
                            let _ = event_tx.send(WmEvent::FrontmostAppChanged { pid });
                        }
                        last_frontmost_pid = Some(pid);
                    }
                }
                Err(RecvTimeoutError::Disconnected) => break,
            }
        }
    });

    event_rx
}

/// Every pid that should currently have a window-notification subscription
/// — on-screen owners (`enumerate::onscreen_owner_pids`) unioned with every
/// running "regular" app (`workspace::all_regular_app_pids`), since an app
/// running backgrounded with zero windows (all closed, not quit) needs a
/// watcher set up proactively: it never re-fires
/// `NSWorkspaceDidLaunchApplicationNotification` when it later opens a
/// *new* window, since its process never actually launches afresh.
fn watchable_pids() -> BTreeSet<i32> {
    enumerate::onscreen_owner_pids()
        .into_iter()
        .chain(workspace::all_regular_app_pids())
        .collect()
}

fn seed_watchers(
    rt: &tokio::runtime::Handle,
    event_tx: &mpsc::UnboundedSender<WmEvent>,
    watched: &mut HashMap<i32, tokio::task::JoinHandle<()>>,
    unwatchable: &mut HashSet<i32>,
) {
    let onscreen = enumerate::onscreen_owner_pids();
    for pid in watchable_pids() {
        match watch_app(rt, pid, event_tx.clone()) {
            Some(handle) => {
                watched.insert(pid, handle);
            }
            None => {
                unwatchable.insert(pid);
            }
        }
        if onscreen.contains(&pid) {
            let _ = event_tx.send(WmEvent::WindowsChanged { pid });
        }
    }
}

/// Correctness backstop — see `RESYNC_INTERVAL`'s doc comment. Watches any
/// pid that should be watched but isn't yet (a missed/never-fired launch
/// notification), and drops watchers for pids that are no longer running
/// (a missed termination notification). Only re-signals `WindowsChanged`
/// for every *already-watched* on-screen pid — the rarer safety net for a
/// missed window-level notification — when `full_window_resync` is set; see
/// `FULL_RESYNC_DEBOUNCE`/`FULL_RESYNC_MAX_INTERVAL`.
///
/// A pid newly discovered *by this function* (as opposed to via the
/// `AppEvent::Launched` push notification, which already signals
/// `WindowsChanged` itself) always gets an immediate `WindowsChanged` the
/// moment its watcher is attached, regardless of `full_window_resync` —
/// mirrors `seed_watchers`. Without this, a missed launch notification
/// still got a watcher attached promptly (within one `RESYNC_INTERVAL`
/// tick) but its already-existing window(s) sat untiled until the next
/// full resync — up to `FULL_RESYNC_MAX_INTERVAL` later — since attaching
/// a watcher only catches *future* per-window notifications, not windows
/// that already existed before the subscription began.
///
/// `unwatchable` skips re-attempting a subscription for a pid that already
/// failed once (see its doc comment at the call site) — otherwise a
/// permanently unwatchable pid (WindowServer and similar system processes
/// that show up as on-screen window owners but aren't real AX-subscribable
/// apps) would retry and log a failure every single tick forever.
fn resync_watchers(
    rt: &tokio::runtime::Handle,
    event_tx: &mpsc::UnboundedSender<WmEvent>,
    watched: &mut HashMap<i32, tokio::task::JoinHandle<()>>,
    unwatchable: &mut HashSet<i32>,
    full_window_resync: bool,
) {
    let current = watchable_pids();
    let onscreen = enumerate::onscreen_owner_pids();

    for &pid in &current {
        if !watched.contains_key(&pid) && !unwatchable.contains(&pid) {
            match watch_app(rt, pid, event_tx.clone()) {
                Some(handle) => {
                    watched.insert(pid, handle);
                }
                None => {
                    unwatchable.insert(pid);
                }
            }
            if onscreen.contains(&pid) {
                let _ = event_tx.send(WmEvent::WindowsChanged { pid });
            }
        }
    }

    if full_window_resync {
        for &pid in &onscreen {
            let _ = event_tx.send(WmEvent::WindowsChanged { pid });
        }
    }

    watched.retain(|&pid, handle| {
        if current.contains(&pid) && !pid_is_dead(pid) {
            true
        } else {
            // The pid is gone but no `AppTerminated` ever arrived (the same
            // notification-delivery gap as a missed launch) — without this,
            // `WmState` never learns the app quit, so its windows are never
            // cleaned out of the tree (the "ghost gap" left behind).
            let _ = event_tx.send(WmEvent::AppTerminated { pid });
            handle.abort();
            false
        }
    });
    unwatchable.retain(|&pid| {
        if current.contains(&pid) && !pid_is_dead(pid) {
            true
        } else {
            // Mirrors `watched.retain` above — a pid whose AXObserver
            // subscription failed at discovery time can still have real
            // windows recorded in `WmState` (`seed_watchers`/the launch
            // arm both fire `WindowsChanged` for it regardless of
            // subscription success), so it needs the same synthetic
            // `AppTerminated` when it disappears, or its windows are
            // never cleaned out of the tree if the primary NSWorkspace
            // termination notification also doesn't fire for it (the
            // same real, occasional delivery gap `watched.retain` above
            // already accounts for).
            let _ = event_tx.send(WmEvent::AppTerminated { pid });
            false
        }
    });
}

/// Kernel-level liveness check, independent of `NSWorkspace` — `kill(pid,
/// 0)` sends no signal, just checks whether the pid still refers to a live
/// process (`ESRCH` means it doesn't). This exists because `current`
/// (`watchable_pids()`) is itself partly sourced from
/// `workspace::all_regular_app_pids()`, i.e. `NSWorkspace.runningApplications()`
/// — the very subsystem whose termination notification this whole resync
/// loop is a backstop for. A backgrounded, windowless pre-existing app can
/// have both its primary termination notification *and* this NSWorkspace-
/// derived liveness read go stale together, since they share one underlying
/// source — leaving a permanent ghost tile behind with no path to ever
/// synthesize `AppTerminated`. This check gives `resync_watchers` a second,
/// genuinely independent signal for that case.
fn pid_is_dead(pid: i32) -> bool {
    // SAFETY: signal 0 sends nothing and only checks process existence/
    // permission; passing a plain pid is always sound.
    unsafe {
        libc::kill(pid, 0) == -1
            && std::io::Error::last_os_error().raw_os_error() == Some(libc::ESRCH)
    }
}

/// Subscribes to one app's window lifecycle notifications and forwards a
/// `WmEvent` for every one that fires, via a task spawned on `rt` (not the
/// ambient runtime, since the caller may not be on a runtime thread — see
/// `spawn_event_watcher`). Best-effort: logs and returns `None` if either
/// step fails (matching every other AX subscription in this codebase), so
/// one unwatchable app never blocks watching the rest.
fn watch_app(
    rt: &tokio::runtime::Handle,
    pid: i32,
    tx: mpsc::UnboundedSender<WmEvent>,
) -> Option<tokio::task::JoinHandle<()>> {
    let app = AXUIElement::from_pid(pid)?;
    let stream = match AXNotificationStream::subscribe_many(&app, WINDOW_NOTIFICATIONS, 32) {
        Ok(stream) => stream,
        Err(e) => {
            eprintln!("tili-ax: failed to subscribe to window notifications for pid {pid}: {e}");
            return None;
        }
    };
    Some(rt.spawn(async move {
        while let Some(event) = stream.next().await {
            let wm_event = if event.notification == AX_FOCUSED_WINDOW_CHANGED_NOTIFICATION
                || event.notification == AX_MAIN_WINDOW_CHANGED_NOTIFICATION
            {
                WmEvent::WindowFocused { pid }
            } else {
                WmEvent::WindowsChanged { pid }
            };
            if tx.send(wm_event).is_err() {
                break;
            }
        }
    }))
}
