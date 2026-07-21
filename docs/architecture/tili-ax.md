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
A missing/ambiguous subrole otherwise falls back to whether the window has
*any* chrome button (close/fullscreen/zoom/minimize), not just fullscreen.
`tili-daemon/src/state.rs`'s `SYSTEM_UI_BUNDLE_IDS` is a second,
belt-and-suspenders bundle-id denylist forcing `FloatingRuleMode::Ignore`
for a few specific confirmed cases, in case the general signal above
doesn't apply to some future process.

`AxWindow::set_frame`/`set_position`/`set_size`/`focus` are the only place
real windows get moved/resized/raised — `set_frame` sets position before
size (some apps clamp size based on current position), `set_position` only
moves (used to park a window off-screen without needlessly resizing it,
M4), `set_size` only resizes (used by `tili-daemon`'s floating-window
centering to discover an app's real, possibly-clamped size *before* writing
a position, so centering a fixed-one-axis app like System Settings only
ever needs one position write instead of one wrong-then-corrected pair),
and all three writes are best-effort (`let _ =` on the AX result; a window
that refuses a write is left alone, matching every other AX-based WM). All
three also update the cached `frame` field to match what was just written,
so `WmState::list_windows` reflects reality without a wasted AX read-back —
this is why `WindowFrameSetter::set_frame` takes `&mut AxWindow`.

## frame_setter.rs — the animation seam

Defines the `WindowFrameSetter` trait — every place that moves/resizes a
real window must go through `dyn WindowFrameSetter`, not call
`AxWindow::set_frame` directly. v1 only implements `InstantFrameSetter`;
this trait is the seam a future animated setter plugs into without touching
layout code.

## display.rs — monitors and the display watcher (M9)

Enumerates every connected display via `CGDisplay::active_displays()` —
`list_monitors()` is re-run fresh on every call (nothing cached) so
hot-plug/unplug just falls out of calling it again; each `Monitor`'s usable
`frame` is its full `CGDisplay` bounds minus a hardcoded menu-bar inset
applied only when `is_main` (secondary displays don't carry a menu bar).
This is a deliberate, documented simplification over real
`NSScreen.visibleFrame` (which would be more precise about notches/Dock
placement but requires flipping between `NSScreen`'s bottom-left-origin
coordinate space and AX/`CGDisplay`'s top-left-origin one — judged not
worth the risk for what M9 needs).

`spawn_display_watcher()` registers a
`CGDisplayRegisterReconfigurationCallback` on its own dedicated `CFRunLoop`
thread and just signals "something changed, re-enumerate" per callback — it
doesn't interpret `CGDisplayChangeSummaryFlags`. It also bounds its
`CFRunLoopRun` into `RESOLUTION_POLL_INTERVAL` (1s) chunks and re-diffs
`list_monitors()` after every wake — confirmed on real hardware (temporary
debug logging, since removed) that `CGDisplayRegisterReconfigurationCallback`
reliably fires for hot-plug/unplug and sleep/wake but never fires at all for
a resolution-only change (same monitor id, no add/remove) in this process —
at the time, attributed to `tili-daemon` having no `NSApplication`/
UI-session-activation context, the same explanation given for
`NSWorkspaceDidActivateApplicationNotification` (and, later,
`DidLaunchApplication`/`DidWakeNotification`) never firing (see
`workspace.rs`'s section below). `main.rs` has since given `tili-daemon` a
real `NSApplication` context to fix those; whether that also fixes
resolution-only reconfiguration delivery hasn't been separately
re-verified, so this remains one of the four sanctioned polling exceptions
(see [invariants.md](invariants.md)) pending that check, not removed
opportunistically alongside an unrelated change.

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

The 250ms reconciliation tick (`resync_watchers`) also drives two fixes
added in 0.1.1:

- it cross-checks each watched pid's kernel-level liveness via
  `libc::kill(pid, 0)` (independent of `NSWorkspace`, which the primary
  termination notification and this tick's own pre-existing backstop both
  already depend on — closing a gap where both could go stale together for
  a backgrounded, windowless pre-existing app);
- it tracks `workspace::frontmost_app_pid()` across ticks, emitting
  `WmEvent::FrontmostAppChanged { pid }` on an edge-triggered change — the
  only signal that catches Cmd-Tab or a Mission Control/Control Center
  click switching to an app whose window lives in a parked workspace, since
  neither `NSWorkspaceDidActivateApplicationNotification` (dead for this
  process, see the focus-sync section in [tili-daemon.md](tili-daemon.md))
  nor per-window `WindowFocused` reacts to a pure OS-level frontmost
  change.

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

## hotkey.rs — global hotkey capture (M6)

A `CGEventTap` on its own dedicated `CFRunLoop` thread (same reasoning as
the NSWorkspace/AX watchers), which consumes (drops) a keypress if it's in
the caller-supplied `active_bindings` set and passes everything else
through untouched. `parse_key_combo` turns a KDL key string like
`"alt-shift-h"` into a `KeyCombo` (`key_code_for_name` is the exhaustive
keycode table — extend it there if a config references a key name it
doesn't recognize).

`active_bindings` is an `Arc<Mutex<HashSet<KeyCombo>>>` because the
event-tap callback must decide Keep-vs-Drop *synchronously* — it can't
`.await` a round-trip to `tili-daemon`'s single owning loop to ask "is this
bound?" This is the one place in the codebase with a shared `Mutex` instead
of message-passing into one owner; see `tili-daemon`'s
`sync_active_combos` for how it's kept from drifting.

## mouse.rs — cursor warp and mouse watcher (M10)

`warp_cursor_to` (`CGDisplay::warp_mouse_cursor_position`, for
`mouse-follows-focus`) and `spawn_mouse_watcher` — another `CGEventTap`,
this one `ListenOnly` on `kCGEventMouseMoved` for `focus-follows-monitor`,
throttled to one position report per 80ms via a *thread-local*
`Cell<Instant>` (not a shared `Mutex` — this callback only ever runs on its
own dedicated OS thread, so there's nothing to synchronize) so mouse
activity in general can't flood the daemon's `select!` loop with one
message per pixel of travel.
