# tili-ax — the only crate that touches the Accessibility API

Part of the [architecture notes](../ARCHITECTURE.md).

Depends on `tili-tree` only for geometry types (`Rect`), never for the tree
itself.

## window.rs — the one private API call, classification, frame writes

Owns the single private API call used anywhere in the codebase
(`_AXUIElementGetWindow`, to resolve a window's real `CGWindowID`) — keep
that call isolated there; don't add other private API usage without a
strong reason, since staying public-API-only is what lets tili run without
disabling SIP.

`WindowKind` (`Standard`/`Dialog`/`Popup`) via `classify_window_kind` —
checked before subrole matching (M-fix 0.1.1): a non-regular-activation-
policy process (`workspace::is_regular_app`, i.e. no Dock icon/Cmd-Tab
entry — the Dock itself, `SecurityAgent`, the screenshot toolbar, ...)
presenting a window with no close button is always `Popup` regardless of AX
role/subrole, after live-hardware testing showed system-UI chrome
occasionally slipping through the old subrole-only check and getting
tiled/re-centered.
A standard subrole with a zoom button but no full-screen button is also
`Dialog` — real content windows are fullscreen-capable
(`AXFullScreenButton`), while AppKit gives non-fullscreenable utility
panels a classic zoom button instead (`AXZoomButton`); Preferences/
Settings-style windows are the textbook case, hardware-confirmed on
Safari's own Settings window, which reports `AXStandardWindow` — same as
its ordinary browser windows — but swaps full-screen for zoom. A
missing/ambiguous subrole otherwise falls back to whether the window has
*any* chrome button (close/fullscreen/zoom/minimize), not just fullscreen.
`tili-daemon/src/state.rs`'s `SYSTEM_UI_BUNDLE_IDS` is a second,
belt-and-suspenders bundle-id denylist forcing `FloatingRuleMode::Ignore`
for a few specific confirmed cases, in case the general signal above
doesn't apply to some future process.

`AxWindow::is_resizable` (populated in `from_element` from
`AXUIElement::is_attribute_settable(kAXSizeAttribute)`) is a deliberately
separate signal from `WindowKind` — subrole/chrome-button shape says what
*category* a window claims to be, not whether it can actually be resized.
Reported non-real, e.g. for a splash screen or a popup window torn off an
app's main window (`WindowKind::Standard` by chrome, but AX-refuses a
`kAXSizeAttribute` write) — common in Electron apps generally, not scoped
to any one bundle id. `Err` (attribute unqueryable) defaults to `true`:
wrongly giving up on tiling a genuinely resizable window is worse than
occasionally missing a truly non-resizable one. `tili-daemon`'s
`is_non_resizable_window` forces such a window to `FloatingRuleMode::Ignore`
regardless of `kind` or any matching floating rule — see
`docs/architecture/tili-daemon.md`.

`AxWindow::set_frame`/`set_position`/`set_size`/`focus` are the only place
real windows get moved/resized/raised — `set_frame` sets position before
size (some apps clamp size based on current position), `set_position` only
moves (used to park a window off-screen without needlessly resizing it,
M4), `set_size` only resizes (used by `tili-daemon`'s floating-window
centering to discover an app's real, possibly-clamped size *before* writing
a position, so centering a fixed-one-axis app like System Settings only
ever needs one position write instead of one wrong-then-corrected pair),
and all three writes are best-effort (a window that refuses a write is left
alone, matching every other AX-based WM). All three inspect the AX
`Result` and only advance the cached `frame` field on `Ok` — never
optimistically, so `WmState::list_windows` reflects reality without a
wasted AX read-back on the success path, and this is why
`WindowFrameSetter::set_frame` takes `&mut AxWindow`. A write that fails
(logged via `log_write_failure`, except `InvalidUIElement` — the window
having simply closed, which the normal destroy-notification path already
handles) leaves the cache exactly where it was, so the next call's
no-op-if-unchanged guard sees a real mismatch and retries the write
automatically; hardware-confirmed root cause of a bug where a workspace's
window stayed parked off-screen after unlock until switching workspaces
twice — the AX write briefly failed right after unlock (app still
reconnecting to AX/WindowServer) but the cache advanced anyway, so every
subsequent switch's no-op guard wrongly believed the window was already
where it needed to be and silently skipped writing it.

## frame_setter.rs — the animation seam

Defines the `WindowFrameSetter` trait — every place that moves/resizes a
real window must go through `dyn WindowFrameSetter`, not call
`AxWindow::set_frame` directly. Two implementations: `InstantFrameSetter`
(the original v1, still the default when `Settings::animate` is off) and
`TweenedFrameSetter`, which eases a window from its old frame to its new
one over a caller-supplied duration (an ease-out curve) instead of jumping
straight there — `tili-daemon` constructs it with its own
`ANIMATION_DURATION` (90ms). `tili-daemon::WmState::apply_config` swaps
which one is boxed behind `frame_setter` only when `Settings::animate`
actually changes, so an unrelated reload never resets an animation
already in flight.

The trait has five more methods beyond `set_frame`, all no-op-defaulted
so `InstantFrameSetter` needs no changes:

- `tick` — advances every in-flight tween by one step, called from
  `tili-daemon`'s dedicated `animation_tick` (16ms/8ms depending on
  `AnimationSpeed::Medium`/`::High` — see
  `docs/architecture/invariants.md`'s "No polling" section for why a
  dedicated timer is sanctioned here). A no-op the instant nothing is
  animating.
- `has_active_animations` — whether *any* window is mid-tween, gating
  `animation_tick`'s `select!` branch (`if
  state.is_animating_anything()`) so that timer is never even polled
  while nothing is animating.
- `is_animating` — lets `WmState::maybe_capture_manual_geometry` skip its
  drift check for a window mid-tween: every animation step is a real,
  tili-initiated `AXWindowMoved`/`AXWindowResized`-firing write, and
  without this guard that function's own invariant ("drift only ever
  means a user drag") would misfire on the animation's own writes.
- `finish` — instantly writes a window's true target and drops its tween,
  for the writes that still bypass this seam (`park`; the *size-discovery*
  step of `place_floating_window`'s centered branch — its actual placement
  still goes through `set_frame`, so it animates) and for `unpark_all`'s
  shutdown-time restore (which also bypasses the seam entirely — an
  animated write there would never get the later ticks it needs, since
  nothing calls `tick` again before the process exits). Without `finish`,
  a stale tween could resume on a later tick and drag the window away
  from wherever one of these direct writes just put it.
- `set_suppressed` — while true, `set_frame` writes instantly (same as
  `InstantFrameSetter`) and drops any tween already running for that
  window, regardless of `Settings::animate`. `WmState::switch_workspace`
  wraps its reveal-the-incoming-workspace calls
  (`relayout_active`/`reposition_floating_in_active_workspace`) in
  `set_suppressed(true)`/`set_suppressed(false)`: those calls are showing
  windows at wherever `park` last left them, and a parked position was
  never meant to be seen, so there's no meaningful start point to ease
  from — animating it would just flash a slide in from a hidden corner.

`TweenedFrameSetter::set_frame`'s coalescing has one subtlety worth
knowing before touching it: a target that matches the tween already in
flight is treated as a no-op *on the tween*, not a restart. Every tween
step is a real write, so it fires a real `AXWindowMoved`/`AXWindowResized`
notification — `main.rs`'s `WindowsChanged` handling already coalesces a
burst of these into `pending_pids` (a `HashSet<i32>`, drained once per
`maintenance_tick`, 30ms — see its own doc comment), so in practice
`apply_windows_changed` → `relayout_active` → `set_frame` re-fires with
the *same* target roughly once per `maintenance_tick` during an
animation's lifetime (≈3 times over `ANIMATION_DURATION`'s 90ms), not
once per animation tick. Still enough to matter: restarting the tween's
clock on any one of those self-triggered calls would push convergence out
by another `maintenance_tick` each time, and since the notification these
calls exist to (mostly) ignore is a *guaranteed* by-product of the
animation itself, not a rare edge case, the guard has to be unconditional
rather than something a short debounce alone would paper over.

## display.rs — monitors and the display watcher (M9)

Enumerates every connected display via `CGDisplay::active_displays()` —
`list_monitors()` is re-run fresh on every call (nothing cached) so
hot-plug/unplug just falls out of calling it again; each `Monitor`'s usable
`frame` is its full `CGDisplay` bounds minus a hardcoded menu-bar inset
applied only when `is_main` (secondary displays don't carry a menu bar).
This is a deliberate, documented simplification over real
`NSScreen.visibleFrame` (which would be more precise about Dock placement
but requires flipping between `NSScreen`'s bottom-left-origin coordinate
space and AX/`CGDisplay`'s top-left-origin one — judged not worth the risk
for what M9 needs).

Each `Monitor` also carries `notch: f64` — how much *additional* top inset
(beyond whatever `frame` already excludes) is needed to also clear that
display's notch, `0.0` if it has none. `list_monitors()` computes this as
`(safeAreaInsets.top - baseline_inset).max(0.0)`, where `baseline_inset` is
`MENU_BAR_HEIGHT` on `is_main` and `0.0` otherwise — deliberately *not* the
raw `safeAreaInsets.top`, since on a notched display that value already
covers the same top-of-screen zone `MENU_BAR_HEIGHT` accounts for and
`frame` has already excluded; adding the raw value on top of `frame`
double-counted that overlap and inflated the effective top gap by a full
extra `MENU_BAR_HEIGHT` on every notched Mac. Unlike `visibleFrame`, this is
a plain scalar with no coordinate-flip risk, so it sidesteps the concern
above entirely. The complication instead is thread-affinity: `NSScreen` is
`MainThreadOnly`, but `list_monitors()` runs on `tili-daemon`'s background
Tokio thread (see `main.rs`'s doc comment on why the real `NSApplication`
lives on the actual process main thread instead). `notch_heights()` hops
onto that real main thread via `dispatch2::DispatchQueue::main().exec_async`,
joined back on the calling thread through a channel with a
`NOTCH_QUERY_TIMEOUT` (50ms) rather than `exec_sync` directly — a real
`NSApplication` event loop services its dispatch main queue essentially
instantly, but a bare (non-Cocoa) process like a `cargo test` binary never
pumps that queue at all, and `exec_sync` would block the calling thread
forever waiting for a reply nobody will ever send. The timeout is a
deadlock guard, not a legitimate-latency allowance — on a timeout every
display just falls back to `notch: 0.0`, same as before this existed.
Mapping an `NSScreen` back to its `CGDirectDisplayID` goes through its
`deviceDescription`'s `"NSScreenNumber"` key — a string built by hand,
since `objc2-app-kit`'s header-translator doesn't generate a constant for
it. `tili-daemon`'s `tiled_layout_inputs` is what actually folds this
height into the effective top gap (gated by the `gaps` config's
`ignore-notch` flag) — `Monitor.notch` is already the correct amount to
add there, with no further adjustment needed on the daemon side.

`spawn_display_watcher()` registers a
`CGDisplayRegisterReconfigurationCallback` on its own dedicated `CFRunLoop`
thread and just signals "something changed, re-enumerate" per callback — it
doesn't interpret `CGDisplayChangeSummaryFlags`. It bounds its
`CFRunLoopRun` into `RUN_LOOP_PUMP_INTERVAL` (1s) chunks purely to avoid a
run-loop spin-forever bug (a mode with no input source/timer registered
returns immediately instead of blocking — confirmed on real hardware via
CPU sampling, see [invariants.md](invariants.md)), not to poll anything.

This function used to *also* run a genuine resolution-change poll on that
same cadence: `CGDisplayRegisterReconfigurationCallback` had been confirmed
on real hardware to reliably fire for hot-plug/unplug and sleep/wake but
never for a resolution-only change (same monitor id, no add/remove), at the
time attributed to `tili-daemon` having no `NSApplication`/UI-session-
activation context — the same explanation given for
`NSWorkspaceDidActivateApplicationNotification` (and, later,
`DidLaunchApplication`/`DidWakeNotification`) never firing (see
`workspace.rs`'s section below). Once `main.rs` gave `tili-daemon` a real
`NSApplication` context, re-testing confirmed the callback now fires
reliably for resolution-only changes too (two calls per change:
`kCGDisplayBeginConfigurationFlag`, then `kCGDisplaySetModeFlag`/
`kCGDisplayDesktopShapeChangedFlag` once it completes) — so the polling
fallback was removed; `spawn_display_watcher` is no longer one of the
sanctioned polling exceptions (see [invariants.md](invariants.md)). Sends
directly on a `tokio::sync::mpsc` channel from this thread, same as
`hotkey.rs`/`mouse.rs` above — no separate bridge thread.

## workspace.rs — NSWorkspace bridge

Bridges `NSWorkspace` app-launch/quit notifications, plus
`NSWorkspaceDidWakeNotification` (forwarded as `AppEvent::SystemDidWake` →
`WmEvent::SystemDidWake` → `WmState::note_system_wake`, see
[tili-daemon.md](tili-daemon.md) for what that does), via
`objc2`/`objc2-app-kit`. Also has `bundle_id_for_pid` (M8, via
`NSRunningApplication`) — `enumerate.rs` resolves this once per process and
shares it across all of that process's `AxWindow`s, rather than once per
window, since it's used to match floating rules.

`register_on_main(mtm: MainThreadMarker)` registers the three `NSWorkspace`
observers directly on the real process main thread, with `queue: .main`
delivery, and returns immediately — no thread spawned, no `CFRunLoop`
pumped here. `tili-daemon/src/main.rs`'s real `fn main()` calls this after
creating a real `NSApplication` and before calling `app.run()`, which is
what actually pumps the main run loop that delivers these blocks
afterward. Main-thread registration with `queue: .main` is load-bearing,
not incidental: confirmed on real hardware that `DidLaunchApplication`/
`DidWakeNotification` are not reliably delivered to a process with no
`NSApplication` pumping its main run loop (only `DidTerminateApplication`
was, evidently via some other delivery path) — registering from an
arbitrary background thread with a bare `CFRunLoopRun()` and `queue: nil`
(the pattern `display.rs`'s watcher and `axuielement`'s own
`AXNotificationStream` both still use, and which still holds for AX
notifications specifically) isn't sufficient for these two `NSWorkspace`
notifications.

The same function also registers `com.apple.screenIsLocked`/
`screenIsUnlocked` (`AppEvent::ScreenLocked`/`ScreenUnlocked` →
`WmEvent::ScreenLocked`/`ScreenUnlocked` → `WmState::note_screen_locked`/
`note_screen_unlocked` — see [tili-daemon.md](tili-daemon.md)) — but through
`NSDistributedNotificationCenter::defaultCenter()`, not `NSWorkspace`'s own
`notificationCenter()`. These two are undocumented, system-wide
notifications posted by `loginwindow` when the screen lock engages/
disengages, not `NSWorkspace`, so they don't exist on `NSWorkspace`'s
center at all. Confirmed via real `daemon.err.log` output that this
project's own day-to-day "sleep" gesture — lock the screen, wait for the
display to blank — never produces an `NSWorkspace SystemDidWake received`
line at all, since the machine itself never actually suspends; that these
two distributed notifications are what *does* fire for a screen lock/
unlock is the long-standing, community-documented behavior of
`loginwindow`, not something this project independently re-verified with
its own logging yet. Registration
still uses the same block/`queue: .main` shape as the `NSWorkspace`
observers above, for consistency: `NSDistributedNotificationCenter` is a
subclass of `NSNotificationCenter`, so
`addObserverForName:object:queue:usingBlock:` is inherited unchanged.
Whether main-thread/`NSApplication`-pumping is as load-bearing for a
*distributed* notification as it is for `DidWakeNotification` hasn't been
separately hardware-tested — registering it the same way `DidWakeNotification`
needed to be registered was the conservative default, not an independently
confirmed requirement.

`watch.rs::spawn_event_watcher` takes the `Receiver<AppEvent>` this
function returns as a parameter now, rather than spawning its own
registration — registration must happen on the main thread before the
Tokio runtime (and `spawn_event_watcher`, which needs
`tokio::runtime::Handle::current()`) even exists, so `tili-daemon/src/main.rs`
does it first and passes the receiver down.

## watch.rs — event watcher and reconciliation tick

`spawn_event_watcher()` ties the NSWorkspace and AX sources together: it
subscribes each running app to window lifecycle notifications and emits a
single coarse `WmEvent::WindowsChanged { pid }` per change — callers
re-read that process's windows via `list_windows_for_pid` rather than
trying to interpret individual notification payloads (this sidesteps having
to reason about whether a specific `AXUIElement` is still valid to query at
the exact moment its destroyed-notification fires).

An earlier version of this loop also carried a `SystemTime`-based
wall-clock-gap wake-detection backstop (`SLEEP_GAP_THRESHOLD`), added
because real hardware confirmed `NSWorkspaceDidWakeNotification` could
silently never reach the daemon for an entire sleep/wake cycle, even though
the observing thread stayed alive throughout — `workspace.rs`'s
`register_on_main` section above describes the fix for the underlying
cause (giving `tili-daemon` a real `NSApplication`). That backstop has
since been removed: real-hardware testing across several repeated
sleep/wake cycles, after the `NSApplication` restructuring landed,
confirmed the notification is delivered reliably again on its own — see
[invariants.md](invariants.md)'s polling-exceptions section.

`WmEvent::FrontmostAppChanged { pid }` — the only signal that catches
Cmd-Tab or a Mission Control/Control Center click switching to an app whose
window lives in a parked workspace, since per-window `WindowFocused` above
doesn't react to a pure OS-level frontmost change — is forwarded directly
from `AppEvent::Activated` (`NSWorkspaceDidActivateApplicationNotification`,
registered via `workspace::register_on_main`). This replaced a 250ms poll
of `workspace::frontmost_app_pid()` (`AXFocusedApplication`, a direct AX
query) kept that fast specifically because `DidActivateApplication` was
confirmed dead for a `tili-daemon` with no `NSApplication`; **pending
real-hardware confirmation that the notification is now reliably delivered**
the same way `DidLaunchApplication`/`DidWakeNotification` were separately
confirmed (see `workspace.rs`'s section above). `frontmost_app_pid` itself
still exists and is still called elsewhere, on demand rather than
periodically — see its own doc comment.

`resync_watchers` — attach/detach watchers for the current pid set — used
to run on that same 250ms cadence too, back when `NSWorkspace` launch/
terminate notifications were also unreliable; it now runs on its own
`WATCHER_RESYNC_INTERVAL` (2s), the tick's only remaining cadence now that
the frontmost poll is gone, since those launch/terminate notifications are
confirmed reliably delivered (`tili-daemon` has a real `NSApplication`) and
this is now a rare-miss backstop rather than the primary detection path —
see that constant's doc comment. It drives one fix added in 0.1.1: it
cross-checks each watched pid's kernel-level liveness via `libc::kill(pid,
0)` (independent of `NSWorkspace`, which the primary termination
notification and this backstop both already depend on — closing a gap
where both could go stale together for a backgrounded, windowless
pre-existing app).

`resync_watchers`' `unwatchable` set caches a pid whose AX subscription
already failed once (a system compositor process like WindowServer or Dock,
which owns on-screen windows — menu bar, cursor layer — but isn't a real
AX-subscribable app), so it isn't retried and re-logged every tick forever.
That cache is deliberately evicted only when the pid actually dies
(`pid_is_dead`), not merely when it stops appearing in the current
on-screen-owner set — a compositor process's on-screen ownership flickers
tick to tick, and gating eviction on it (as an earlier version did) meant
the pid fell out of the cache, got retried, failed, and got re-inserted on
every single flicker, flooding the log with the same subscription failure
indefinitely.

`watched.retain` mirrors that same liveness-first ordering, for a sharper
reason than log noise: it used to send a synthetic `WmEvent::AppTerminated`
for any pid that dropped out of `current` for even one tick, and that event
routes to `WmState::remove_app` — an *immediate*, ungraced purge of every
window the daemon still has for that pid, with none of the `pending_removal`/
`removal_grace` (or wake-boosted `WAKE_REMOVAL_GRACE`) protection a real
window close goes through. `current` is partly sourced from
`NSWorkspace.runningApplications()`, and real hardware confirmed that list
can transiently omit a still-genuinely-running app's pid — most readily
right after waking from sleep, while the WindowServer/AX subsystem is still
settling — which force-purged that app's still-open windows and had them
rediscovered moments later as brand new, re-triggering any matching
`workspace-rules` entry with no grace period able to stop it (see the wake
section in [tili-daemon.md](tili-daemon.md) — this bug produced the same
"workspace silently changes after wake" symptom as the two fixes documented
there, just through a path neither one touched). `watched.retain` now only
sends `AppTerminated` on a confirmed-dead pid; one that's merely off
`current` this tick just loses its AXNotificationStream subscription
(silently re-attached once it reappears), never triggering `remove_app`.

`spawn_event_watcher`'s own thread tracks `screen_locked`, set/cleared by
`AppEvent::ScreenLocked`/`ScreenUnlocked` (see `workspace.rs`'s section
above), and skips the `WATCHER_RESYNC_INTERVAL` resync (both its
watcher-attach/detach pass and any `WindowsChanged` sweep) entirely while
it's set. Real `daemon.err.log` output showed two genuinely still-open
windows finalized as closed ~93s after a lock/unlock cycle — the periodic
resync's live re-enumeration of an already-tracked window's process was
running unmodified throughout the whole lock, and AX enumeration for other
apps can silently come back empty while the session is on loginwindow (the
same reconnect instability `note_screen_locked`/`note_system_wake` guard
against, but that guard only blocks `finalize_expired_removals`, not the
enumeration that feeds `pending_removal` in the first place — see
[tili-daemon.md](tili-daemon.md)'s lock/unlock section for the rest of the
fix). `AppEvent::ScreenUnlocked` itself still always forwards (never
suppressed, so `WmState` always learns lock state changes), and
additionally forces one real full resync immediately — rather than
skipping the tick and waiting for the next 2s/20s cadence — resetting
`last_full_resync`/`debounce_deadline` as if that resync had happened
through the normal timer.

`Ok(AppEvent::SystemDidWake)` and the `Err(RecvTimeoutError::Timeout)`
suspected-sleep branch both do the same forced-immediate-full-resync as
`AppEvent::ScreenUnlocked` above, for the same reason applied to a real
sleep instead of a lock: a long sleep advances `Instant` just as much as a
lock sitting frozen does, so a window already in `pending_removal` before
the machine suspended needs the same fast re-verification a lock/unlock
cycle does — see [tili-daemon.md](tili-daemon.md)'s sleep/wake section for
the daemon-side half (`WmState::note_system_wake` restarting
`pending_removal`'s clocks). Both send sites force their own resync rather
than relying on one to cover the other: real hardware shows the
`Err(Timeout)` branch routinely detects a real sleep first (the real
notification has an extra hop to cross), but a real sleep shorter than
`SUSPECTED_SLEEP_GAP` never trips that branch's detection at all and only
ever reaches `Ok(AppEvent::SystemDidWake)`, so skipping either one would
leave a real gap, not just redundant work on the common path. Unlike
lock/unlock, there's no `screen_locked`-style suppression on the sleep side
of this — this codebase doesn't listen for `NSWorkspaceWillSleepNotification`,
so there's no "before" event to gate a resync-skipping window against, only
the wake-side forced resync mirrored from `ScreenUnlocked`.

## hotkey.rs — global hotkey capture (M6)

A `CGEventTap` on its own dedicated `CFRunLoop` thread — mandatory
regardless of `NSApplication`: `CGEventTap::with_enabled(...,
CFRunLoop::run_current)` blocks its calling thread forever once the tap
installs, so it can't share a run loop with anything else (installing it on
the real main thread would block `NSApplication::run()` forever, before the
Cocoa event loop ever starts pumping) — which consumes (drops) a keypress
if it's in the caller-supplied `active_bindings` set and passes everything
else through untouched. `parse_key_combo` turns a KDL key string like
`"alt-shift-h"` into a `KeyCombo` (`key_code_for_name` is the exhaustive
keycode table — extend it there if a config references a key name it
doesn't recognize). `spawn_hotkey_tap` sends directly on a
`tokio::sync::mpsc` channel from this thread — no separate bridge thread
needed (`tili-daemon.md`'s main.rs section explains why this differs from
the config-reload bridge).

`active_bindings` is an `Arc<Mutex<HashSet<KeyCombo>>>` because the
event-tap callback must decide Keep-vs-Drop *synchronously* — it can't
`.await` a round-trip to `tili-daemon`'s single owning loop to ask "is this
bound?" This is the one place in the codebase with a shared `Mutex` instead
of message-passing into one owner; see `tili-daemon`'s
`sync_active_combos` for how it's kept from drifting.

## mouse.rs — cursor warp and mouse watcher (M10)

`warp_cursor_to` (`CGDisplay::warp_mouse_cursor_position`, for
`mouse-follows-focus`) and `spawn_mouse_watcher` — another `CGEventTap`
(same mandatory-dedicated-thread constraint as `hotkey.rs`'s tap, see
above), this one `ListenOnly` on `kCGEventMouseMoved` for
`focus-follows-monitor`, throttled to one position report per 80ms via a
*thread-local* `Cell<Instant>` (not a shared `Mutex` — this callback only
ever runs on its own dedicated OS thread, so there's nothing to
synchronize) so mouse activity in general can't flood the daemon's
`select!` loop with one message per pixel of travel. Sends directly on a
`tokio::sync::mpsc` channel from this thread, same as `spawn_hotkey_tap` —
no separate bridge thread.
