use std::collections::{BTreeSet, HashMap};
use std::sync::mpsc::RecvTimeoutError;
use std::time::Duration;

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
/// because this tick, unlike `WINDOW_RESYNC_EVERY`'s, never triggers a
/// relayout: it's just pid enumeration plus attaching/detaching watchers.
///
/// Deliberately *doesn't* also re-signal `WindowsChanged` for every
/// on-screen pid on every tick (an earlier version did): each one triggers
/// `apply_windows_changed`, which unconditionally relays out the active
/// workspace at the end regardless of whether anything for that pid
/// actually changed — with several on-screen apps, firing all of them
/// back-to-back every tick caused a visible relayout stutter for no
/// reason. See `WINDOW_RESYNC_EVERY` for that much rarer sweep.
///
/// This is the third sanctioned exception to "no polling" (see
/// `tili-daemon/src/main.rs`'s Accessibility-grant wait and
/// `tili-ax/src/hotkey.rs`'s hotkey-tap retry for the other two) —
/// documented in `CLAUDE.md`.
const RESYNC_INTERVAL: Duration = Duration::from_millis(250);

/// Every `WINDOW_RESYNC_EVERY`th `RESYNC_INTERVAL` tick (~10s wall-clock,
/// at the 250ms `RESYNC_INTERVAL` above), additionally re-signals
/// `WindowsChanged` for every on-screen pid — a rare safety net
/// for a missed `AXObserver` window-level notification (created/destroyed/
/// moved/resized), which empirically fires reliably in the common case
/// (unlike the app-level launch/terminate notifications `RESYNC_INTERVAL`
/// itself guards against), so this doesn't need to run anywhere near as
/// often. `apply_windows_changed` diffs against its cache and is a no-op if
/// nothing actually changed, but its unconditional relayout at the end
/// still costs a real pass over the active workspace, hence keeping this
/// infrequent.
const WINDOW_RESYNC_EVERY: u32 = 40;

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
    AppLaunched { pid: i32, bundle_id: Option<String> },
    AppTerminated { pid: i32 },
    WindowsChanged { pid: i32 },
    WindowFocused { pid: i32 },
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
    seed_watchers(&rt, &event_tx, &mut watched);

    std::thread::spawn(move || {
        let mut tick: u32 = 0;
        loop {
            match app_rx.recv_timeout(RESYNC_INTERVAL) {
                Ok(AppEvent::Launched { pid, bundle_id }) => {
                    let _ = event_tx.send(WmEvent::AppLaunched { pid, bundle_id });
                    if let Some(handle) = watch_app(&rt, pid, event_tx.clone()) {
                        watched.insert(pid, handle);
                    }
                    let _ = event_tx.send(WmEvent::WindowsChanged { pid });
                }
                Ok(AppEvent::Terminated { pid }) => {
                    let _ = event_tx.send(WmEvent::AppTerminated { pid });
                    if let Some(handle) = watched.remove(&pid) {
                        // Aborting drops the owned AXNotificationStream, which
                        // stops its dedicated CFRunLoop thread (see
                        // axuielement's `ObserverThreadHandle::drop`).
                        handle.abort();
                    }
                }
                Err(RecvTimeoutError::Timeout) => {
                    tick = tick.wrapping_add(1);
                    let full_window_resync = tick.is_multiple_of(WINDOW_RESYNC_EVERY);
                    resync_watchers(&rt, &event_tx, &mut watched, full_window_resync);
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
) {
    let onscreen = enumerate::onscreen_owner_pids();
    for pid in watchable_pids() {
        if let Some(handle) = watch_app(rt, pid, event_tx.clone()) {
            watched.insert(pid, handle);
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
/// for every on-screen pid — the rarer safety net for a missed
/// window-level notification — when `full_window_resync` is set; see
/// `WINDOW_RESYNC_EVERY`.
fn resync_watchers(
    rt: &tokio::runtime::Handle,
    event_tx: &mpsc::UnboundedSender<WmEvent>,
    watched: &mut HashMap<i32, tokio::task::JoinHandle<()>>,
    full_window_resync: bool,
) {
    let current = watchable_pids();

    for &pid in &current {
        if !watched.contains_key(&pid)
            && let Some(handle) = watch_app(rt, pid, event_tx.clone())
        {
            watched.insert(pid, handle);
        }
    }

    if full_window_resync {
        for pid in enumerate::onscreen_owner_pids() {
            let _ = event_tx.send(WmEvent::WindowsChanged { pid });
        }
    }

    watched.retain(|&pid, handle| {
        if current.contains(&pid) {
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
