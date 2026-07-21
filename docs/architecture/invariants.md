# Design invariants — full rationale

Part of the [architecture notes](../ARCHITECTURE.md). The rules themselves
are listed in CLAUDE.md; this file holds the rationale and the
real-hardware evidence behind each.

## No private APIs

No private Accessibility/window APIs beyond the one documented
`_AXUIElementGetWindow` call in `tili-ax/src/window.rs`. Staying
public-API-only is what lets tili run without disabling SIP.

## No polling — and the three sanctioned exceptions

The daemon reacts to AXObserver/NSWorkspace/display notifications
(`tili-ax`'s `watch.rs`/`workspace.rs`), it doesn't loop and check state.
Three sanctioned, narrowly-scoped exceptions:

1. **`tili-ax/src/hotkey.rs`'s `spawn_hotkey_tap`** retries installing the
   `CGEventTap` every few seconds for the process's whole lifetime, since
   Input Monitoring can be granted at any point after the daemon starts
   with no accompanying event to react to.
2. **`tili-ax/src/watch.rs`'s window/app-watcher resync backstop** — a
   cheap 250ms tick (attach/detach watchers, no relayout) plus a
   debounced-since-quiet full-window resync capped at 20s
   (`FULL_RESYNC_DEBOUNCE`/`FULL_RESYNC_MAX_INTERVAL`) — since `NSWorkspace`
   launch/terminate notifications and `AXObserver` window-level
   notifications have both been observed to occasionally never fire. An
   earlier version of this tick also carried a `SLEEP_GAP_THRESHOLD`
   wall-clock-gap wake-detection backstop, added while
   `NSWorkspaceDidWakeNotification` was confirmed undelivered to a
   `tili-daemon` with no `NSApplication`; removed once `main.rs`'s
   `NSApplication` restructuring (see `tili-daemon.md`/`tili-ax.md`'s
   `workspace.rs` section) was confirmed on real hardware, across several
   repeated sleep/wake cycles, to make that notification reliably delivered
   again without it.
3. **`tili-daemon/src/main.rs`'s `maintenance_tick`**, an unconditional
   30ms `tokio::time::interval` branch of the main `select!` loop. Unlike
   the other two, this isn't a fallback for a notification that sometimes
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
   (see CHANGELOG.md) confirmed this tick wasn't a meaningful CPU cost.

**Not an exception to this invariant, but worth noting alongside it:**
`tili-ax/src/display.rs`'s `spawn_display_watcher` bounds its `CFRunLoopRun`
into `RUN_LOOP_PUMP_INTERVAL` (1s) chunks — not to poll anything, but
because a run loop pumped in a mode with no input source/timer registered
returns immediately instead of blocking (the same bug the v0.1.7
investigation, see CHANGELOG.md, found sustaining ~40% CPU here before it
was bounded+sleep-corrected). This function used to *also* run a genuine
resolution-change poll on the same cadence, added when
`CGDisplayRegisterReconfigurationCallback` was confirmed to never fire for
a resolution-only change in a `tili-daemon` with no `NSApplication`; removed
once `main.rs`'s `NSApplication` restructuring (see exception 2's
`SLEEP_GAP_THRESHOLD` note, and `tili-ax.md`'s `display.rs` section) was
confirmed on real hardware to make that callback fire reliably for
resolution-only changes too.

Don't add a fourth polling loop without a similarly hard constraint forcing
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
