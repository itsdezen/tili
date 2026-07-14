use std::collections::HashMap;

use axuielement::AXUIElement;
use axuielement::async_api::AXNotificationStream;
use axuielement::ax_notification::{
    AX_WINDOW_CREATED_NOTIFICATION, AX_WINDOW_MOVED_NOTIFICATION, AX_WINDOW_RESIZED_NOTIFICATION,
    AXUI_ELEMENT_DESTROYED_NOTIFICATION,
};
use tokio::sync::mpsc;

use crate::enumerate;
use crate::workspace::{self, AppEvent};

/// One notification name registered per watched app; any of these firing
/// means "re-read this process's windows," so the exact notification
/// content doesn't need to be threaded through — see the module docs below.
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
];

/// Something the daemon should react to. Deliberately coarse-grained:
/// `WindowsChanged` doesn't say *what* changed about a process's windows
/// (created vs moved vs title-changed vs destroyed) — callers re-read that
/// process's windows via `enumerate::list_windows_for_pid` in response,
/// which is cheap (a handful of AX calls) and sidesteps having to reason
/// about whether a specific `AXUIElement` handle is still valid to query at
/// the moment its destroyed-notification arrives.
#[derive(Debug, Clone)]
pub enum WmEvent {
    AppLaunched { pid: i32, bundle_id: Option<String> },
    AppTerminated { pid: i32 },
    WindowsChanged { pid: i32 },
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

    let seed_tx = event_tx.clone();
    let seed_rt = rt.clone();
    let mut watched: HashMap<i32, tokio::task::JoinHandle<()>> = HashMap::new();
    for pid in enumerate::onscreen_owner_pids() {
        if let Some(handle) = watch_app(&seed_rt, pid, seed_tx.clone()) {
            watched.insert(pid, handle);
        }
        let _ = seed_tx.send(WmEvent::WindowsChanged { pid });
    }

    std::thread::spawn(move || {
        while let Ok(app_event) = app_rx.recv() {
            match app_event {
                AppEvent::Launched { pid, bundle_id } => {
                    let _ = event_tx.send(WmEvent::AppLaunched { pid, bundle_id });
                    if let Some(handle) = watch_app(&rt, pid, event_tx.clone()) {
                        watched.insert(pid, handle);
                    }
                    let _ = event_tx.send(WmEvent::WindowsChanged { pid });
                }
                AppEvent::Terminated { pid } => {
                    let _ = event_tx.send(WmEvent::AppTerminated { pid });
                    if let Some(handle) = watched.remove(&pid) {
                        // Aborting drops the owned AXNotificationStream, which
                        // stops its dedicated CFRunLoop thread (see
                        // axuielement's `ObserverThreadHandle::drop`).
                        handle.abort();
                    }
                }
            }
        }
    });

    event_rx
}

/// Subscribes to one app's window lifecycle notifications and forwards a
/// `WindowsChanged` signal for every one that fires, via a task spawned on
/// `rt` (not the ambient runtime, since the caller may not be on a runtime
/// thread — see `spawn_event_watcher`).
fn watch_app(
    rt: &tokio::runtime::Handle,
    pid: i32,
    tx: mpsc::UnboundedSender<WmEvent>,
) -> Option<tokio::task::JoinHandle<()>> {
    let app = AXUIElement::from_pid(pid)?;
    let stream = AXNotificationStream::subscribe_many(&app, WINDOW_NOTIFICATIONS, 32).ok()?;
    Some(rt.spawn(async move {
        while stream.next().await.is_some() {
            if tx.send(WmEvent::WindowsChanged { pid }).is_err() {
                break;
            }
        }
    }))
}
