# Design invariants — full rationale

Part of the [architecture notes](../ARCHITECTURE.md). The rules themselves
are listed in AGENTS.md; this file holds the rationale and the
real-hardware evidence behind each.

## No private APIs

No private Accessibility/window APIs beyond the one documented
`_AXUIElementGetWindow` call in `tili-ax/src/window.rs`. Staying
public-API-only is what lets tili run without disabling SIP.

## No polling — and the four sanctioned exceptions

The daemon reacts to AXObserver/NSWorkspace/display notifications
(`tili-ax`'s `watch.rs`/`workspace.rs`), it doesn't loop and check state.
Four sanctioned, narrowly-scoped exceptions:

1. **`tili-ax`'s event-tap reinstall loops** — `hotkey.rs`'s
   `spawn_hotkey_tap` and `mouse.rs`'s `spawn_mouse_watcher` both retry
   installing their `CGEventTap` every few seconds for the process's whole
   lifetime. `tili-daemon`'s `async_daemon_main` now hard-stops (mirroring
   its Accessibility check) if Input Monitoring isn't already granted, so
   the daemon never reaches these loops without it — the retry instead
   covers Input Monitoring being revoked and re-granted while the daemon is
   already running, or a `CGEventTap` install failing for an unrelated
   transient reason, neither of which has an accompanying event to react
   to.

   The same loop is what recovers a tap macOS *disabled* after it was
   working (`TapDisabledByTimeout`/`TapDisabledByUserInput`, classically
   after a sleep/wake). That case does arrive as an event, delivered to the
   tap's own callback — but re-enabling in place needs the tap's
   `CFMachPort`, which `CGEventTap::with_enabled` owns and never exposes to
   the callback (and `CGEventTapProxy` can't do it either). Each callback
   therefore stops its thread's run loop, which makes `with_enabled` return
   into this same loop; the retry interval doubles as the backoff keeping a
   persistently-timing-out tap from reinstalling in a tight loop. Verified
   against `core-graphics` 0.25 that the run-loop source `with_enabled`
   leaves behind doesn't stop a later install from blocking normally — a
   reinstall that returned immediately would have turned this into a hot
   no-op loop. Left unhandled, a disabled hotkey tap means every hotkey
   silently stops working until the daemon is restarted, and a disabled
   mouse tap can strand `WmState::mouse_button_down` at `true` (see
   `mouse.rs`'s synthesized `ButtonUp`).
2. **`tili-ax/src/watch.rs`'s window/app-watcher resync backstop** — a
   `WATCHER_RESYNC_INTERVAL` (2s) tick running `resync_watchers` (attach/
   detach watchers, no relayout), plus a debounced-since-quiet full-window
   resync capped at 20s (`FULL_RESYNC_DEBOUNCE`/`FULL_RESYNC_MAX_INTERVAL`)
   — since `AXObserver` window-level notifications have been observed to
   occasionally never fire, and `NSWorkspace.runningApplications()` itself
   (not just its notifications) can transiently omit a still-running pid.
   This tick used to also poll `workspace::frontmost_app_pid()` for
   `WmEvent::FrontmostAppChanged` at a much shorter 250ms (kept fast for
   Cmd-Tab responsiveness despite having to poll for it); that poll is gone
   now that `workspace::register_on_main` registers
   `NSWorkspaceDidActivateApplicationNotification` and the tick reacts to
   the push notification directly instead — confirmed on real hardware to
   be reliably delivered, the same way `DidLaunchApplication`/
   `DidWakeNotification` were separately confirmed. `resync_watchers` itself
   used to also run on that same 250ms
   cadence, back when `NSWorkspace` launch/terminate notifications were
   also unreliable; now that those are confirmed reliably delivered
   (`tili-daemon` has a real `NSApplication`), it only needs to run as a
   rare-miss backstop, hence the slower shared cadence. An earlier version
   of this tick also carried a `SLEEP_GAP_THRESHOLD` wall-clock-gap
   wake-detection backstop, added while `NSWorkspaceDidWakeNotification`
   was confirmed undelivered to a `tili-daemon` with no `NSApplication`;
   removed once `main.rs`'s `NSApplication` restructuring (see
   `tili-daemon.md`/`tili-ax.md`'s `workspace.rs` section) was confirmed on
   real hardware, across several repeated sleep/wake cycles, to make that
   notification reliably delivered again without it.
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
4. **`tili-daemon/src/main.rs`'s `animation_tick`**, a `tokio::time::interval`
   driving `TweenedFrameSetter` (`Settings::animate`) steps at 16ms
   (`AnimationSpeed::Medium`, ~60fps) or 8ms (`::High`, ~120fps) —
   `main.rs`'s `sync_animation_tick` reconstructs it (tokio's `Interval`
   has no in-place period change) whenever `WmState::animation_tick_period`
   differs from what it's currently running at, checked right after every
   `apply_config` (including the initial load). Unlike the other three,
   this isn't compensating for an unreliable or nonexistent notification
   — it exists because interpolating a frame over wall-clock time has no
   event-driven alternative at all; there's no OS notification for "the
   next animation frame is due," tili has to generate that timing itself.
   What keeps it from being unbounded, always-on polling: its `select!`
   branch is guarded by `if state.is_animating_anything()`, so the branch
   isn't polled — the timer doesn't even tick — while nothing is
   mid-animation, which is always true under `AnimationSpeed::Off` and
   true the overwhelming majority of the time even when animating (each
   animation only runs for `ANIMATION_DURATION`, 90ms). Faster than
   `maintenance_tick` (30ms) because animation smoothness depends directly
   on this cadence, unlike `maintenance_tick`'s bookkeeping, where a
   coarser interval is invisible to the user.

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
daemon/tree code — this is the seam animation support (`TweenedFrameSetter`,
behind `Settings::animate`) plugs into. Three documented, narrow
exceptions bypass the seam and call `AxWindow` directly instead — `park`,
`place_floating_window`'s centered branch's *size-discovery* step only
(the actual placement, once the real size is known, still goes through
the seam), and `unpark_all`'s shutdown-time restore — each must call
`frame_setter.finish()` first (except `unpark_all`, which has nothing left
running afterward to fight) so a stale animation can't resume on a later
tick and undo the direct write. A fourth case still goes through
`set_frame` itself but forces it instant via `set_suppressed(true)`:
`switch_workspace` revealing the incoming workspace's windows, which are
showing at wherever `park` last left them — a parked position was never
meant to be seen, so there's no meaningful start point to animate from.

`TweenedFrameSetter`'s per-tick writes are driven by `tili-daemon`'s
dedicated `animation_tick` (16ms or 8ms, per `AnimationSpeed`) — see
exception 4 in "No polling" below for why this is sanctioned rather than
a plain new poll. An earlier version reused `maintenance_tick` (30ms)
instead of a dedicated timer, to avoid adding a fourth polling exception
at all; real-hardware testing showed 30ms/~6 steps over
`ANIMATION_DURATION` read as visibly choppy, so it was split into its own
faster, activity-gated timer instead — and then made speed-configurable
(`AnimationSpeed::Medium`/`::High`) once even 16ms/~60fps still read as
low-fps on real hardware.

## Session state is startup-only and never stores NodeIds

- The persisted session is a `WindowId`-based snapshot; `NodeId`s are
  SlotMap keys and are never serialized. `TreeSnapshot`/`Tree::from_snapshot`
  own the round trip and validate/normalize on load.
- Restore is startup-only. Saved state wins for **matched** startup windows
  during the grace window; unmatched windows, windows opened after the
  grace deadline, and all later activity use config rules normally.
- Ambiguous fallback matches are skipped, never guessed.
- A missing/corrupt/unsupported-version state file is ignored (primary may
  fall back to `session.json.bak`); it can never prevent daemon startup.
- Writes are atomic (temp + fsync + rename) with a rotated `.bak`, and run
  off the event loop via `spawn_blocking`. No save while restore is active
  or after a panic. A zero-match restore preserves the previous file until a
  real user mutation. Saved windows that were alive but unenumerable are
  kept as deferred records (and in the next snapshot) and retried through
  the existing `WindowsChanged` path, not treated as brand-new windows.
- `Popup`/ignored windows may be recorded but are never geometry-restored;
  tili does not own their placement.

## One active workspace per connected display

- Every connected display gets exactly one active workspace at startup and
  on `on_displays_changed`. Assignment order is pinned workspace, last
  workspace on that display, first free declared workspace in config order,
  then a synthetic `display-<ordinal>` fallback.
- A workspace is never active on two displays at once. Each workspace has a
  **home display**: the display showing it, a connected pin, or the display
  it last lived on. `switch_workspace` goes to that home (focusing an
  already-visible workspace, or showing a parked one there and parking the
  display's previous workspace). It never materializes a parked workspace on
  an unrelated display; a never-shown workspace is assigned to the focused
  display on first use.
- `switch_workspace_here` is the explicit local verb: it shows the workspace
  on the focused display, swapping with its occupant when the workspace lives
  elsewhere. `move-workspace-to-monitor next|prev|main` pushes the active
  workspace (swap) or only changes a parked workspace's home.
- `workspace-back` restores the previous `(display, workspace)` pair. It
  undoes a `workspace-here` swap locally when still on the same display, but
  returning from a cross-display jump only focuses the remembered display and
  never rearranges another screen.
- Pins (`workspace "x" monitor="main|id"`) reserve the workspace even while
  the target display is disconnected. A pinned workspace cannot be moved off
  its pin by `workspace-here` or `move-workspace-to-monitor`; a synthetic
  display fallback cannot be relocated at all.
- Synthetic fallbacks are marked, use the monitor's pretty name
  (`NSScreen.localizedName` fallback `Display N`), are renumbered when the
  layout changes, and are never persisted empty. On disconnect an empty
  fallback is deleted; a non-empty one has its windows migrated to the
  main/focused workspace first.
- Floating windows are re-anchored whenever their workspace changes
  display, including rule-placed floating windows that otherwise keep
  their one derived frame.

## Parking is global and re-applied after display changes

- Every parked `Tiled`/`Floating` window — from any workspace — targets the
  one bottom-right corner of the southeast-most connected display
  (`max x + width`, tie `max y + height`, tie id). No per-display or
  per-workspace parking coordinate exists.
- `park`/`park_concurrently` write that same coordinate, so parking and
  re-parking are idempotent (`set_position` no-ops on an unchanged frame).
- `on_displays_changed_with` calls `repark_parked_windows()` after updating
  monitors, workspaces, and active layouts. Connect, disconnect,
  arrangement, resolution/scale, display-id rename, and SE-display removal
  therefore all move parked windows to the new corner before they can be
  seen on another monitor.
- `finalize_restore` re-asserts the corner after session restore; a
  zero-monitor enumeration is sleep and skips parking.
- `Minimized`/`NativeFullscreen`/`HiddenApplication`/`Popup` placements are
  never parked or reparked (they were never moved off-screen by `park`).

## Native tabs are opaque to the daemon

- A brand-new window that matches a managed same-app/same-frame sibling
  (old window no longer enumerable, or an explicit `AXTabGroup`) is the
  active member of an app-owned tab group, not a new managed window.
- The daemon swaps the existing tree leaf's `WindowId` and per-window maps
  in place. It never inserts a tile/float, applies rules, cascades, or
  relayouts, so opening/closing/switching a tab cannot move or resize the
  app's window.
- Ambiguous candidates are skipped; `settings { native-tabs #false }` is the
  escape hatch. Background tabs are intentionally untracked (the app owns
  them; session persistence keeps only the visible tab).
- This is a compatibility guard, not full tab-group membership/restore.

## One dispatch path, one sanctioned Mutex

Hotkey-triggered and socket-triggered commands both go through `dispatch()`
— no parallel command-handling path. The hotkey tap's `active_bindings:
Arc<Mutex<HashSet<KeyCombo>>>` (`tili-ax/src/hotkey.rs`) is the *one*
sanctioned exception to "no locks, single owning loop" — a `CGEventTap`
callback must decide synchronously whether to consume a keystroke and can't
await a round-trip into `WmState`'s loop to find out. Don't add a second
one without a similarly hard constraint forcing it.
