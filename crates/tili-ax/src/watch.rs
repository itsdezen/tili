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

/// How often the background watcher thread's tick fires (via
/// `recv_timeout`'s timeout), and how often it calls `resync_watchers` to
/// re-derive "every pid that should currently have a window-notification
/// subscription," attaching watchers for newly-discovered ones and
/// detaching (with a synthetic `AppTerminated`) for ones no longer running.
///
/// This is a correctness backstop, not the primary mechanism: push-based
/// detection (`NSWorkspaceDidLaunchApplicationNotification`/
/// `DidTerminateApplication` for process launch/quit) is what makes this
/// feel instant in the common case, and is confirmed reliably delivered on
/// real hardware (`tili-daemon` has a real `NSApplication`; see
/// `workspace::register_on_main`'s doc comment). A missed launch/quit
/// notification is a rare-enough case that this backstop doesn't need to be
/// fast to still feel "close to instant" in practice. `pid_is_dead`'s own
/// check inside `resync_watchers` (guarding against
/// `NSWorkspace.runningApplications()` itself transiently omitting a
/// still-running pid, a separate concern from the notification) is covered
/// by this same cadence for the same reason — that's also a rare case, not
/// the common path.
///
/// This tick used to also drive a poll of `workspace::frontmost_app_pid()`
/// for `WmEvent::FrontmostAppChanged`, kept at a much shorter 250ms so
/// Cmd-Tab/Mission-Control detection would feel instant despite having to
/// poll for it — that's gone now that `register_on_main` registers
/// `NSWorkspaceDidActivateApplicationNotification` and this tick reacts to
/// the push notification directly (see `AppEvent::Activated`'s handling
/// below) instead of polling, so nothing left in this tick needs a fast
/// cadence — matches `FULL_RESYNC_DEBOUNCE`'s existing cadence rather than
/// inventing a new interval.
///
/// Deliberately *doesn't* also re-signal `WindowsChanged` for every
/// on-screen pid every time this runs (an earlier version did on every
/// tick): each one triggers `apply_windows_changed`, which unconditionally
/// relays out the active workspace at the end regardless of whether
/// anything for that pid actually changed — with several on-screen apps,
/// firing all of them back-to-back caused a visible relayout stutter for no
/// reason. See `FULL_RESYNC_DEBOUNCE`/`FULL_RESYNC_MAX_INTERVAL` for that
/// much rarer sweep.
///
/// This is one of the sanctioned exceptions to "no polling" — the full
/// list and rationale are documented in `docs/architecture/invariants.md`.
const WATCHER_RESYNC_INTERVAL: Duration = Duration::from_secs(2);

/// How much longer than `WATCHER_RESYNC_INTERVAL` a single `recv_timeout`
/// wait has to take before it's treated as proof the process was actually
/// suspended (a real sleep), not just an ordinary idle tick — see the
/// `Err(RecvTimeoutError::Timeout)` branch below. Comfortably above any
/// normal scheduling jitter on a live thread (which never overshoots a 2s
/// deadline by more than a few ms), and comfortably below the shortest sleep
/// a user would actually trigger (lid-close/`pmset sleepnow`/Apple menu
/// Sleep all suspend for well over this), so it can't false-positive on
/// ordinary load and won't miss a real one.
const SUSPECTED_SLEEP_GAP: Duration = Duration::from_secs(5);

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
/// notifications `WATCHER_RESYNC_INTERVAL` itself guards against) can be
/// deferred. Every genuine `AppEvent` pushes the deadline forward by
/// `FULL_RESYNC_DEBOUNCE`, but this cap guarantees a sweep still happens on
/// an otherwise-quiet system rather than deferring forever.
/// `apply_windows_changed` diffs against its cache and is a no-op if
/// nothing actually changed, but its unconditional relayout at the end
/// still costs a real pass over the active workspace, hence keeping this
/// infrequent rather than running it every `WATCHER_RESYNC_INTERVAL` cycle.
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
    /// forwarded directly from `AppEvent::Activated`
    /// (`NSWorkspaceDidActivateApplicationNotification`, registered via
    /// `workspace::register_on_main`), not polled. This is the only signal
    /// that catches Cmd-Tab or a Mission Control/Control Center click
    /// switching to an app whose window lives in a currently-parked
    /// workspace — the per-window `WindowFocused` event above doesn't react
    /// to a pure OS-level frontmost change that doesn't also move focus
    /// within an already on-screen app.
    FrontmostAppChanged {
        pid: i32,
    },
    /// The system just woke from sleep — usually
    /// `NSWorkspaceDidWakeNotification` (registered via
    /// `workspace::register_on_main` — see that function's doc comment for
    /// why main-thread/`queue: .main` registration was needed for this to be
    /// delivered reliably at all), but also sent synthetically by this
    /// module's own `spawn_event_watcher` loop the instant it detects its
    /// `recv_timeout` wait ran suspiciously long (see `SUSPECTED_SLEEP_GAP`)
    /// — that path can land *before* the real notification does, closing a
    /// real ordering race against `resync_watchers`. Every AX-observing
    /// process can take several seconds to reconnect to the WindowServer
    /// after wake, so a still-open window can easily miss one
    /// `apply_windows_changed` scan right after this fires — see
    /// `WmState::note_system_wake`'s doc comment for what reacting to this
    /// actually does about it. Both send sites in `spawn_event_watcher` also
    /// force a real full resync immediately, mirroring `ScreenUnlocked`
    /// below — see each send site's own comment. Harmless to receive more
    /// than once for the same real wake (`note_system_wake` is idempotent).
    SystemDidWake,
    /// The screen just locked (`com.apple.screenIsLocked`, forwarded from
    /// `AppEvent::ScreenLocked` — registered via
    /// `workspace::register_on_main`'s `NSDistributedNotificationCenter`
    /// setup, *not* the same notification center `SystemDidWake` above
    /// uses). Confirmed on real hardware to be the event that actually
    /// fires for how this project's own testing showed the user "puts the
    /// machine to sleep" day to day — locking and waiting for the display
    /// to blank, which never triggers a real `NSWorkspaceDidWakeNotification`
    /// pair at all. See `WmState::note_screen_locked`'s doc comment for
    /// what reacting to this does.
    ScreenLocked,
    /// The screen just unlocked (`com.apple.screenIsUnlocked`, forwarded
    /// from `AppEvent::ScreenUnlocked`) — see `ScreenLocked` and
    /// `WmState::note_screen_unlocked`.
    ScreenUnlocked,
}

/// Starts watching for window/app lifecycle events and returns a channel
/// that receives a `WmEvent` each time something changes. Must be called
/// from within a Tokio runtime (it captures the current `Handle` to spawn
/// per-app watch tasks from a plain OS thread that isn't itself part of the
/// runtime).
///
/// `app_rx` is the `NSWorkspace` notification receiver from
/// `workspace::register_on_main` — registered by the caller on the real
/// main thread *before* this function (and the Tokio runtime it needs) even
/// exist, since that registration must happen before `NSApplication::run()`
/// starts pumping the main run loop that delivers it. This function no
/// longer registers those notifications itself.
///
/// Already-running apps are seeded as synthetic `WindowsChanged` events
/// immediately, so callers don't need a separate "initial full scan" path —
/// starting from an empty cache and reacting to this channel is sufficient
/// both at startup and for the rest of the process's life.
pub fn spawn_event_watcher(
    app_rx: std::sync::mpsc::Receiver<AppEvent>,
) -> mpsc::UnboundedReceiver<WmEvent> {
    let rt = tokio::runtime::Handle::current();
    let (event_tx, event_rx) = mpsc::unbounded_channel();

    let mut watched: HashMap<i32, tokio::task::JoinHandle<()>> = HashMap::new();
    // Pids a subscription attempt has already failed for (e.g. WindowServer,
    // pid 403 — a system compositor process, not a real app, that shows up
    // in `onscreen_owner_pids` as the owner of some overlay windows but will
    // never support an AX subscription). Without this, `resync_watchers`
    // would retry — and log a failure for — the exact same permanently
    // unwatchable pid on every single `WATCHER_RESYNC_INTERVAL` cycle
    // forever, wasting AX calls and flooding the log. Cleared when the pid
    // actually disappears (see the `retain` below and
    // `AppEvent::Terminated`), so a *new* process reusing the same pid gets
    // a fresh attempt.
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
        // Edge-triggered: only a pid that actually differs from the last one
        // seen emits `FrontmostAppChanged`, so revealing a parked workspace
        // (which ends by raising/focusing the same pid already recorded
        // here) doesn't cause the next `AppEvent::Activated` for it to
        // re-detect a "change" and loop. A cheap defense-in-depth safety net
        // more than a load-bearing one now — `NSWorkspaceDidActivateApplicationNotification`
        // is push-based and edge-triggered by nature (the system doesn't
        // notify unless the frontmost app genuinely changed), unlike the
        // polled `AXFocusedApplication` query this replaced, which needed
        // this same dedup to survive its own read-timing quirks.
        let mut last_frontmost_pid: Option<i32> = None;
        // True from `AppEvent::ScreenLocked` until the matching
        // `AppEvent::ScreenUnlocked` — confirmed on real hardware (see
        // `docs/architecture/tili-daemon.md`'s lock/unlock section) that AX
        // enumeration for *other* apps can silently come back empty while
        // the session is switched to loginwindow, which made this thread's
        // own periodic resync (below) misread a still-open window as closed
        // and hand `WmState` a false `WindowsChanged` for it — indistinguishable
        // downstream from a real close. Gating the resync itself here, not
        // just its downstream effects, means a locked screen produces no AX
        // window reads for other apps at all, so there's nothing left to
        // misread.
        let mut screen_locked = false;
        loop {
            let tick_started_at = Instant::now();
            let recv_result = app_rx.recv_timeout(WATCHER_RESYNC_INTERVAL);
            match recv_result {
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
                Ok(AppEvent::Activated { pid }) => {
                    if last_frontmost_pid != Some(pid) {
                        let _ = event_tx.send(WmEvent::FrontmostAppChanged { pid });
                    }
                    last_frontmost_pid = Some(pid);
                }
                Ok(AppEvent::SystemDidWake) => {
                    let _ = event_tx.send(WmEvent::SystemDidWake);
                    // Mirrors `AppEvent::ScreenUnlocked` below: force one
                    // real full resync immediately rather than waiting for
                    // the next periodic tick, so a still-open window isn't
                    // misread as closed for up to `WATCHER_RESYNC_INTERVAL`
                    // longer than necessary — see `WmState::note_system_wake`'s
                    // doc comment for the other half of closing that race
                    // (resetting each `pending_removal` entry's grace-period
                    // clock). Usually redundant with the `Err(Timeout)`
                    // branch below, which real hardware shows is routinely
                    // first to detect a real sleep — but not guaranteed:
                    // this is the only branch that fires at all for a real
                    // sleep too short for `SUSPECTED_SLEEP_GAP` to catch.
                    // Skipped while the screen is locked, same reasoning as
                    // `screen_locked`'s doc comment above: this wake may
                    // have landed while still sitting at the lock screen,
                    // where AX reads for other apps can't be trusted either.
                    if !screen_locked {
                        last_full_resync = Instant::now();
                        debounce_deadline = None;
                        resync_watchers(&rt, &event_tx, &mut watched, &mut unwatchable, true);
                    }
                }
                Ok(AppEvent::ScreenLocked) => {
                    screen_locked = true;
                    let _ = event_tx.send(WmEvent::ScreenLocked);
                }
                Ok(AppEvent::ScreenUnlocked) => {
                    screen_locked = false;
                    let _ = event_tx.send(WmEvent::ScreenUnlocked);
                    // The AX server is expected to be genuinely back by now
                    // (same assumption `WmState::note_screen_unlocked`
                    // documents) — run one real full resync immediately,
                    // rather than waiting for the next periodic tick, so
                    // `WmState` gets actual ground truth (a real
                    // `list_windows_for_pid` scan, not just a per-pid AX
                    // probe) as soon as possible after unlock. This is what
                    // lets `apply_windows_changed` un-pend any
                    // `pending_removal` entry that predates the lock (see
                    // its "present in this scan after all" removal) before
                    // `finalize_expired_removals` gets a chance to act on
                    // it — see `WmState::note_screen_unlocked`'s doc comment
                    // for the other half of closing that race (resetting
                    // each entry's grace-period clock).
                    last_full_resync = Instant::now();
                    debounce_deadline = None;
                    resync_watchers(&rt, &event_tx, &mut watched, &mut unwatchable, true);
                }
                Err(RecvTimeoutError::Timeout) => {
                    // A `recv_timeout` wait that took meaningfully longer
                    // than the `WATCHER_RESYNC_INTERVAL` it was given means
                    // this thread (and the whole process) was suspended for
                    // a real system sleep, not just idling — `Instant`
                    // keeps advancing across suspend on macOS, so resuming
                    // lands here with the deadline already long expired.
                    // Confirmed on real hardware that this branch — not
                    // `AppEvent::SystemDidWake` below — is routinely the
                    // *first* thing this thread observes after a real wake:
                    // `AppEvent::SystemDidWake` has to cross an extra hop
                    // (main thread -> `register_on_main`'s channel -> this
                    // thread's next `app_rx.recv`) that `resync_watchers`
                    // below doesn't wait for, so it can easily still be
                    // sitting unread in `app_rx` at this exact moment.
                    // `resync_watchers` is about to do a live AX read for
                    // every watched pid, and immediately after a real wake
                    // those reads can come back empty simply because the
                    // app hasn't reconnected to the WindowServer/AX yet —
                    // exactly what `WmState::note_system_wake` exists to
                    // protect against, but only once it's actually run.
                    // Sending the synthetic wake here, before
                    // `resync_watchers`, guarantees `note_system_wake` has
                    // already marked every currently-tracked window
                    // unconfirmed by the time any `WindowsChanged` this
                    // sweep produces gets processed — both land in the same
                    // `event_tx` channel, in send order, and the daemon's
                    // single-consumer select loop only ever processes one
                    // to completion before the next. The real notification
                    // still arrives moments later and is harmless to react
                    // to twice (`note_system_wake`'s probe is idempotent).
                    let suspected_sleep = tick_started_at.elapsed() > SUSPECTED_SLEEP_GAP;
                    if suspected_sleep {
                        let _ = event_tx.send(WmEvent::SystemDidWake);
                    }
                    // Skip the resync entirely while the screen is locked —
                    // see `screen_locked`'s doc comment above for why any AX
                    // window read here can't be trusted right now. Leaves
                    // `last_full_resync`/`debounce_deadline` untouched;
                    // `AppEvent::ScreenUnlocked` above resets both itself
                    // once it runs its own forced resync, so the normal
                    // cadence just picks back up from there.
                    if !screen_locked {
                        let now = Instant::now();
                        let debounce_ready = debounce_deadline.is_some_and(|due| now >= due);
                        let cap_exceeded =
                            now.duration_since(last_full_resync) >= FULL_RESYNC_MAX_INTERVAL;
                        // A suspected real sleep forces a full resync right
                        // now too, same as the synthetic wake just sent
                        // above — mirrors `AppEvent::ScreenUnlocked`'s forced
                        // resync rather than leaving this to the ordinary
                        // debounce/cap cadence, which could otherwise still
                        // defer up to `FULL_RESYNC_DEBOUNCE` longer before
                        // actually re-reading anything post-wake.
                        let full_window_resync = debounce_ready || cap_exceeded || suspected_sleep;
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

/// Correctness backstop — see `WATCHER_RESYNC_INTERVAL`'s doc comment.
/// Watches any pid that should be watched but isn't yet (a missed/never-
/// fired launch notification), and drops watchers for pids that are no
/// longer running (a missed termination notification). Only re-signals
/// `WindowsChanged` for every *already-watched* on-screen pid — the rarer
/// safety net for a missed window-level notification — when
/// `full_window_resync` is set; see
/// `FULL_RESYNC_DEBOUNCE`/`FULL_RESYNC_MAX_INTERVAL`.
///
/// A pid newly discovered *by this function* (as opposed to via the
/// `AppEvent::Launched` push notification, which already signals
/// `WindowsChanged` itself) always gets an immediate `WindowsChanged` the
/// moment its watcher is attached, regardless of `full_window_resync` —
/// mirrors `seed_watchers`. Without this, a missed launch notification
/// still got a watcher attached promptly (within one
/// `WATCHER_RESYNC_INTERVAL` cycle) but its already-existing window(s) sat
/// untiled until the next full resync — up to `FULL_RESYNC_MAX_INTERVAL`
/// later — since attaching a watcher only catches *future* per-window
/// notifications, not windows that already existed before the subscription
/// began.
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
        if pid_is_dead(pid) {
            // The pid is gone but no `AppTerminated` ever arrived (the same
            // notification-delivery gap as a missed launch) — without this,
            // `WmState` never learns the app quit, so its windows are never
            // cleaned out of the tree (the "ghost gap" left behind).
            let _ = event_tx.send(WmEvent::AppTerminated { pid });
            handle.abort();
            false
        } else if !current.contains(&pid) {
            // Still genuinely alive, just not currently watchable (its only
            // window(s) closed and it isn't a `Regular`-policy app kept in
            // `current` unconditionally by `all_regular_app_pids`) —
            // unsubscribe, but deliberately *without* the synthetic
            // `AppTerminated` above: `WmEvent::AppTerminated` routes to
            // `WmState::remove_app`, which purges every window the state
            // still has for this pid immediately, with no grace period at
            // all (unlike a real window close, which goes through
            // `pending_removal`/`removal_grace`). Confirmed on real
            // hardware that `current` — partly sourced from
            // `NSWorkspace.runningApplications()` — can transiently drop a
            // still-running app's pid for a tick, which previously meant a
            // real app's still-open windows got force-purged and
            // re-discovered as brand new moments later, re-triggering a
            // `workspace-rules` match with none of the wake-grace
            // protection `WmState::note_system_wake` provides (that only
            // covers the `pending_removal` path this bypasses). Re-attached
            // automatically once the pid is back in `current`.
            handle.abort();
            false
        } else {
            true
        }
    });
    unwatchable.retain(|&pid| {
        // Deliberately keyed on liveness alone, not `current.contains(&pid)`
        // like `watched.retain` above — confirmed on real hardware that a
        // system compositor process (WindowServer, Dock) can own an
        // on-screen window (menu bar, cursor layer, ...) on some ticks and
        // not others, so gating eviction on `current` here made a
        // permanently-unwatchable pid drop out of the set and get retried
        // — and fail, and log, and get re-inserted — every time its
        // on-screen ownership happened to flicker, defeating the whole
        // point of this set (see this function's doc comment) and flooding
        // the log.
        if pid_is_dead(pid) {
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
        } else {
            true
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
