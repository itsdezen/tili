# Design invariants — full rationale

Part of the [architecture notes](../ARCHITECTURE.md). The rules themselves
are listed in CLAUDE.md; this file holds the rationale and the
real-hardware evidence behind each.

## No private APIs

No private Accessibility/window APIs beyond the one documented
`_AXUIElementGetWindow` call in `tili-ax/src/window.rs`. Staying
public-API-only is what lets tili run without disabling SIP.

## No polling — and the four sanctioned exceptions

The daemon reacts to AXObserver/NSWorkspace/display notifications
(`tili-ax`'s `watch.rs`/`workspace.rs`), it doesn't loop and check state.
Four sanctioned, narrowly-scoped exceptions:

1. **`tili-ax/src/hotkey.rs`'s `spawn_hotkey_tap`** retries installing the
   `CGEventTap` every few seconds for the process's whole lifetime, since
   Input Monitoring can be granted at any point after the daemon starts
   with no accompanying event to react to.
2. **`tili-ax/src/watch.rs`'s window/app-watcher resync backstop** — a
   cheap 250ms tick (attach/detach watchers, no relayout) plus a
   debounced-since-quiet full-window resync capped at 20s
   (`FULL_RESYNC_DEBOUNCE`/`FULL_RESYNC_MAX_INTERVAL`) — since `NSWorkspace`
   launch/terminate notifications and `AXObserver` window-level
   notifications have both been observed to occasionally never fire.
3. **`tili-ax/src/display.rs`'s `spawn_display_watcher`**, which bounds its
   `CFRunLoopRun` into `RESOLUTION_POLL_INTERVAL` (1s) chunks and re-diffs
   `list_monitors()` after every wake — confirmed on real hardware
   (temporary debug logging, since removed) that
   `CGDisplayRegisterReconfigurationCallback` reliably fires for
   hot-plug/unplug and sleep/wake but never fires at all for a
   resolution-only change (same monitor id, no add/remove) in this process,
   which has no `NSApplication`/UI-session-activation context by design.
4. **`tili-daemon/src/main.rs`'s `maintenance_tick`**, an unconditional
   30ms `tokio::time::interval` branch of the main `select!` loop. Unlike
   the other three, this isn't a fallback for a notification that sometimes
   doesn't fire — every pid it processes already arrived via a real
   AXObserver/NSWorkspace push into `pending_pids`; the tick is purely a
   debounce/coalescing point (a pid re-signaled before its tick folds into
   that one rescan instead of triggering a second) shared with rechecking
   `pending_removal`'s grace-period expiry, `pending_launch_pids`'s
   grace-period expiry, and polling whether `pending_reveal_deadline`
   (armed by `MouseSignal::ButtonUp` or `WmEvent::FrontmostAppChanged`, a
   bounded `REVEAL_DEBOUNCE` — a fixed duration, not tied to this tick's own
   30ms interval — after either) has passed, to run the deferred
   `WmState::reveal_current_frontmost` (see `tili-daemon.md`'s notes on
   `reveal_current_frontmost`/`REVEAL_DEBOUNCE` for what this debounce does
   and doesn't actually protect against) — several "a little time has
   passed, go recheck something" concerns
   sharing one branch rather than each getting its own. Per-tick cost when
   idle (`pending_pids` empty, nothing due for removal, no launch pending,
   no reveal pending) is a couple of cheap emptiness checks, normally
   zero — CPU sampling on real hardware during the v0.1.7 investigation
   (see CHANGELOG.md) confirmed this tick wasn't a meaningful CPU cost;
   `display.rs`'s `spawn_display_watcher` was (that release fixed a bug
   where its `CFRunLoop::run_in_mode` call returned immediately instead of
   blocking for `RESOLUTION_POLL_INTERVAL`, sustaining ~40% CPU).

Don't add a fifth polling loop without a similarly hard constraint forcing
it. The invariant is scoped to `tili-daemon`'s own event loop specifically
— `tili-cli`'s `wait_for_daemon_ready` (a short-lived foreground wait with
clear exit conditions, watching a *separate* process finish starting) and
`tili-menubar`'s reconnect backoff (only active while the daemon is
genuinely unreachable, see its own module docs) are outside this invariant
by construction, not exceptions to it.

## Accessibility permission: no in-process wait, ever

Accessibility permission deliberately has **no** in-process wait/poll of
any kind, despite being a permission grant with no accompanying
notification either — confirmed on real hardware, across three different
mechanisms (plain sleep-based polling, a run-loop-serviced polling thread,
and a stable non-ad-hoc signing identity), that an already-running process
never reliably observes a grant made after it started; only a freshly
launched process's own check reflects reality.
`tili-daemon/src/main.rs` checks once at startup and, if not granted,
unloads its own LaunchAgent (`stop_self`) and tells the user to run
`tili start` again after granting it — no restart loop, no wait, no fifth
polling exception. Don't reintroduce an in-process wait/retry/restart for
this specific permission without new evidence that changes the above.

## All frame writes go through WindowFrameSetter

All real window-frame mutations go through `WindowFrameSetter`
(`tili-ax/src/frame_setter.rs`), never a direct AX API call from
daemon/tree code — this is the seam future animation support plugs into.

## One dispatch path, one sanctioned Mutex

Hotkey-triggered and socket-triggered commands both go through `dispatch()`
— no parallel command-handling path. The hotkey tap's `active_bindings:
Arc<Mutex<HashSet<KeyCombo>>>` (`tili-ax/src/hotkey.rs`) is the *one*
sanctioned exception to "no locks, single owning loop" — a `CGEventTap`
callback must decide synchronously whether to consume a keystroke and can't
await a round-trip into `WmState`'s loop to find out. Don't add a second
one without a similarly hard constraint forcing it.
