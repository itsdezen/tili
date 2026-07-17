# tili-daemon — the window manager process

Part of the [architecture notes](../ARCHITECTURE.md).

## WmState, placements, and floating windows

`src/state.rs` holds `WmState`: the live `AxWindow` handles themselves (not
just cached metadata — M3 needs the real `AXUIElement` to move/focus/park a
window), one `tili_tree::Tree` **per workspace** (M4) for *tiled* windows,
and a `placements: HashMap<WindowId, Placement>` index (M8 — `Placement` is
just `{ workspace, floating }`) giving O(1) "which workspace owns this
window, and is it tiled or floating" instead of scanning every workspace's
tree (M4 through M7's approach).

Floating windows (M8: matched a `floating-rules` entry at creation time,
via `compute_floating_frame`, which checks `AxWindow::bundle_id()` against
each compiled rule in order and computes a centered/sized `Rect` from the
rule's or the config's `defaults`' width/height-ratio) join their
workspace's `Tree` too, as a `tili_tree::Node::Floating` leaf — a normal
child for insertion/removal/lookup, so `workspace_focus`/`focused_node()`
can address a floating window exactly like a tiled one, but excluded
entirely from `Tree::layout`'s `Tiles`/`Accordion` sizing (see
`tili-tree.md`). Their actual on-screen frame is still owned by
`placements`/`compute_floating_frame`, not the tree — floating windows only
get repositioned at creation and when their workspace becomes active again,
not on every layout-affecting event, so a user's manual drag of a floating
window isn't undone by, say, a gap change. `state.rs` functions whose
tree-topology operation is meaningless for a floating focus (`move_focused`,
`join`, `resize`, `set_orientation`/`toggle_orientation`,
`toggle_layout`/`set_layout`, `balance_sizes`, non-native
`toggle_fullscreen`) check `focused_window_is_floating` up front and error
instead of silently acting on the wrong node or having no visible effect;
`focus`, `move_focused_to_workspace`, `set_floating`, `close_focused`, and
native fullscreen all work correctly for a floating focus and don't guard.

`workspace_focus` remembers each workspace's last-focused node — tiled or
floating — so switching back restores where you left off. A new window
joins the active workspace next to the current focus (as a `Node::Window`
if tiled, a `Node::Floating` if floating — either way inserted into the
tree the same way) *unless* it matches a `workspace-rules` entry
(`matching_workspace_rule`, checked via `AxWindow::bundle_id()` — kept
entirely separate from `matching_floating_rule`, since which workspace a
window lands on has nothing to do with whether it tiles or floats).
`apply_windows_changed` resolves that into a `target_workspace` and hands
off to `place_new_window`, which inserts into that workspace's own `Tree`
(keying its focus-hint lookup off `target_workspace`, not whatever's
active, so it still respects where focus was last left there) — a floating
window additionally gets its initial frame computed/written there too, but
only if `target_workspace` is actually visible right now. If `target_workspace` isn't the
one active on the focused monitor, the window is parked immediately and
`resync_workspace_if_visible_elsewhere` (a fourth "thickness of relayout,"
alongside `relayout_active`/`relayout_monitor`/`relayout_all_visible`
below) checks whether it's visible on some *other* monitor and, if so,
relayouts/repositions it there right away instead of leaving it parked
until an unrelated later switch — `move_focused_to_workspace` uses the same
helper for the same reason.

## Multi-monitor (M9)

`active_workspace: HashMap<u32, String>` maps each connected monitor's id
(`tili_ax::Monitor::id`) to whichever workspace it's currently showing — a
workspace absent from this map is parked, wherever it last was.
`focused_monitor: u32` is which one
`Focus`/`Move`/`WorkspaceSwitch`/layout commands actually target;
`relayout_active`/`active_tree`/`active_tree_mut` all resolve through it
(via `active_workspace_name()`), so most of the pre-M9 code didn't need to
change — only `switch_workspace`, `apply_windows_changed`, and
`move_focused_to_workspace` needed to become monitor-name-aware.

`Command::FocusMonitor` (`focus_monitor_next`) is the *only* thing that
changes `focused_monitor` (besides M10's focus-follows-monitor); it cycles
through `self.monitors`, no-op under two. `switch_workspace` swaps with
whatever monitor is already showing the target workspace, if any — two
monitors can never display the same workspace at once, since each has its
own `Tree` layout computed against its own frame.

`on_displays_changed` (called from `main.rs` on every
`spawn_display_watcher` signal) is the hot-plug/unplug handler: a
disconnected monitor's workspace gets parked and its slot dropped (same
mechanics as switching away from it — nothing is lost, just no longer shown
anywhere); a newly connected monitor gets a fresh empty `"monitor-<id>"`
workspace; every still-visible workspace gets re-laid-out afterward since
frames may have changed even for monitors that stayed connected. It returns
immediately on a fully empty enumeration without touching `self.monitors`
(0.1.6) — the display callback fires a momentary zero-display enumeration
as the system sleeps, and committing it wiped the snapshot
`match_monitors`'s origin-distance rename-pairing needs to recognize "same
physical display, new `CGDirectDisplayID`" across sleep/wake.

`relayout_active`/`relayout_monitor`/`relayout_all_visible` are three
thicknesses of "recompute and apply frames" — most callers only need the
focused monitor (`relayout_active`), but anything that could touch a
workspace visible on a *different* monitor (app termination, config reload)
uses `relayout_all_visible`.

`relayout_monitor` writes every tiled placement straight through
`frame_setter.set_frame`, unconditionally, with no readback or retry —
deliberately fire-and-forget, so a tiled window's size always matches
exactly what `Tree::layout`'s weight model computed. A window whose app
rounds/snaps a resize to its own grid (some terminal emulators do this)
can therefore end up larger than its assigned rect and encroach on a
neighboring tiled window's gap — a known, accepted OS/app-level
limitation, not something the layout engine tries to correct: any
after-the-fact size correction would violate the same invariant this
function exists to uphold, and re-writing a frame in response to a
notification the write itself caused risks a self-sustaining relayout
loop (see `apply_windows_changed`'s unconditional `relayout_active()`
call below, which is what would drive it).

`park()` targets `tili_ax::parking_position` — a window's origin lands just
a point inside the main monitor's own bottom-right corner (not pushed
*outside* every monitor's bounds, `combined_bounds`'s original purpose):
confirmed on real hardware that AppKit clamps a `kAXPositionAttribute`
write requesting somewhere totally unreachable back to near a real screen's
edge regardless of how far outside it's requested (it only constrains the
origin, not the window's full frame). Keeping the origin legitimately
on-screen and letting the window's own size extend past the corner (a
technique other AX-based tiling WMs use too) avoids that clamp entirely
instead of fighting it. Every parked window — however many are parked at
once — targets this exact same coordinate; an earlier version offset each
additional one inward by a step so they wouldn't all land on the identical
point, but that shift moves the origin off the one spot the "hidden
regardless of size" guarantee depends on, exposing a real on-screen strip
as wide as the shift. Nothing actually needs parked windows to be
spatially distinct (they're all invisible at the same point regardless of
how many share it), so the offsetting was simply removed. This also keeps
`park`'s own `set_position` write properly idempotent for
`reconcile_existing_placement`'s re-assertion: that write fires a real
`AXWindowMoved` notification (self-triggered writes aren't suppressed
anywhere), which routes straight back through `apply_windows_changed` ->
`reconcile_existing_placement` within one `maintenance_tick` — since every
call now targets the same coordinate regardless of caller,
`AxWindow::set_position`'s no-op-if-unchanged guard genuinely no-ops on
that re-assertion instead of needing to track which offset a specific
call used.

Config-driven workspace-to-monitor pinning (`WorkspaceConfig.monitor`,
parsed since M5) is intentionally still unwired — M9's bar is
hot-plug/unplug safety, not that finer-grained UX.

## Mouse-follows-focus / focus-follows-monitor (M10)

`mouse_follows_focus`/`focus_follows_monitor` are plain `bool`s set from
`config.settings` in `apply_config` (previously parsed but never read
anywhere, since M5). `raise_focused` is the single place that warps the
cursor when `mouse_follows_focus` is on — every focus-changing path
(`focus`, `move_focused`, `switch_workspace`'s restore step) already
funnels through it, so this didn't need duplicating per call site.

`on_mouse_moved(x, y)`, called from `main.rs` on every throttled position
report from `tili_ax::spawn_mouse_watcher`, is a no-op unless
`focus_follows_monitor` is on; when it is, a cheap point-in-rect check
against the already-cached `self.monitors` (no AX/CG call on the hot path)
updates `focused_monitor` if the cursor's now over a different connected
monitor — same effect as an explicit `Command::FocusMonitor`.

## Focus, and syncing with real macOS focus

`focus`/`move_focused` are the only places that call `AxWindow::focus()`
(real OS focus/raise); nothing calls it automatically on window creation,
specifically to avoid focus-stealing every already-open window when the
daemon starts up and gets seeded with the apps already running.
`focus`/`move_focused` go through `tili_tree::Tree::focus_in_direction`
(not plain `navigate`), and both always call `relayout_active` afterward
(M7) — cycling an Accordion's active child changes what's actually visible,
so it's not just a focus-pointer update anymore the way plain `Split`
navigation is.

`dispatch()` calls `WmState::sync_focus_from_frontmost()` before the
command match — resolves which window real macOS currently considers
focused (via `tili_ax::workspace::frontmost_app_pid`, an
`AXUIElementCreateSystemWide`-based query) and updates `workspace_focus`
synchronously, immediately before that command runs. This is deliberately
not a reactive background sync triggered by an event arriving whenever —
confirmed on real hardware that a background poll/notification updating
focus asynchronously has an unavoidable race against the very next hotkey
press, since there's no ordering guarantee between "the background sync
noticed the click" and "the keypress got processed." Other AX-based tiling
WMs resolve this the same way, synchronously at the top of every command —
this is the fix for a long-reported "the first direction key press after
switching windows manually does nothing/goes the wrong way" bug that
several reactive-sync attempts (an AX per-window notification, then an
`NSWorkspaceDidActivateApplicationNotification` subscription — confirmed to
never fire for a process like this one with no `NSApplication` instance,
unlike the process-lifecycle Launch/Terminate notifications, which don't
depend on window-server UI-activation machinery — then a poll on
`watch.rs`'s resync tick) all failed to fully close. `sync_focus_from_pid`
(the function `sync_focus_from_frontmost` actually calls) updates
`workspace_focus` for both `Tiled` and `Floating` placements — since
`Node::Floating` gave floating windows a tree node too (see the
"WmState, placements, and floating windows" section above), a real click
into a floating window is now correctly reflected before the next command
runs, not just a tiled one.

`handle_event`'s `WmEvent::FrontmostAppChanged { pid }` arm (0.1.1) calls
`WmState::reveal_frontmost(pid)`, the only reaction to that event — it
mirrors `summon`'s body (resolve a window, switch to/reveal its workspace
or just retarget `focused_monitor` if already visible elsewhere, then raise
it) but resolves the target window via `AxWindow::focused_id_for_pid(pid)`
instead of a title/bundle-id text query, and silently no-ops instead of
erroring since there's no CLI caller to report a failure to. One
exception to "always follow": macOS reactivates the previously-frontmost
app when the current one closes its last window, producing the same kind
of pid-change edge a real Cmd-Tab does — `reveal_frontmost` tracks
`last_frontmost_pid` across calls and skips the workspace switch if that
previous pid no longer owns any live window, so closing the last window
on a workspace doesn't silently jump the display to wherever that
reactivated app lives (confirmed on real hardware to otherwise land back
on `default_workspace` more often than not, since that's typically
wherever the user was before switching away). That "still owns a live
window" check itself excludes `PlacementKind::Popup` windows (0.1.9) —
system UI chrome like Spotlight's search panel gets tracked like any
other window (landing in whatever workspace was active when it opened),
but it's transient, not a real window the user is looking at, so a
still-open one shouldn't count as "the previous pid is still alive" and
defeat the suppression; opening Spotlight over an empty workspace and
dismissing it with Esc was otherwise enough to trigger the same spurious
jump. `Minimized`/`NativeFullscreen`/`HiddenApplication` placements are
deliberately not excluded the same way — those represent a genuinely
still-open window in a special display state, not transient chrome.

That 0.1.9 exclusion overshot, though: Spotlight, the Dock, and
Notification Center (`SYSTEM_UI_BUNDLE_IDS`) *only ever* own `Popup`
windows, so "still owns a live non-popup window" reads as false for them
unconditionally — every transition away from one was suppressed
regardless of whether the user picked a result/icon (a real switch that
should follow) or just dismissed it passively (Esc, a Dock click that
resolved to nothing new, or a notification banner's close button). Both
produce the identical previous-pid-owns-nothing signal, so the check
alone can't tell them apart. A pid-history-based attempt at disambiguating
them (tracking a "last real, non-system-UI frontmost pid" and suppressing
only on an exact match) correctly caught the passive-dismissal case, but
that tracked pid goes stale whenever a workspace switch in between never
actually changed the OS-level frontmost app (e.g. switching to an empty
workspace) — the next reactivation of the same app then reads as a match
and gets suppressed, permanently, until some other real frontmost change
resets it. `reveal_frontmost` resolves this the other way instead: a
`SYSTEM_UI_BUNDLE_IDS` previous pid always means "follow," full stop.
This reopens the original, narrower 0.1.9 symptom (a one-frame flicker
when dismissing one of these helpers over a literally empty workspace,
before settling back) as an accepted trade-off — better than a stuck
suppression on a routine, repeated action. The original
`previous_lost_its_last_window` check is untouched for the case where the
previous pid was a normal app (the actual 0.1.8 "closed its last window"
scenario, unrelated to any of this).

None of the above ever runs at all for a Dock icon click, confirmed on
real hardware by grepping a diagnostic build's log for the whole
interaction: unlike Spotlight, `Dock.app` never becomes the AX/`NSWorkspace`
frontmost application while handling a click. If the clicked app was
already the OS's nominal frontmost app — the common case when the current
workspace is empty, since nothing else is competing for that status —
`workspace::frontmost_app_pid()` reads identically before and after the
click, so `watch.rs`'s poll (however tight `RESYNC_INTERVAL` is) never sees
an edge and `FrontmostAppChanged` never fires; `reveal_frontmost` never
runs. `WmState::reveal_current_frontmost` covers this instead: `main.rs`'s
`MouseSignal::ButtonUp` arm (already wired for M10.1's drag-resize
debounce) also calls it on every left click — a real `CGEventTap` signal,
not a poll, so a Dock click's mouse down+up always triggers a check
regardless of whether any pid ever "changed." It re-resolves whatever
`frontmost_app_pid()` reports *right now* through the same
`reveal_frontmost` logic, deliberately without checking whether that pid
differs from last time. `reveal_frontmost` treats a same-pid,
already-fully-visible call as a true no-op via `pid_unchanged`/`did_reveal`
(only skipping the final `raise_focused_window` when *both* the pid didn't
change *and* nothing was actually revealed/moved) — a real pid transition
(Cmd-Tab, Mission Control) still always raises, so `mouse_follows_focus`
keeps tracking those even when the target was already visible on the
current monitor; only a click that turns out to have nothing to do with
switching apps costs one extra AX query and does nothing further.

## Layout commands, config, and dispatch

`toggle_layout`/`set_layout` (M7) wrap `Tree::toggle_layout` for
`Command::LayoutToggle`/`LayoutSet`; `set_layout` is a no-op if the
container's already the requested kind, since there are only two kinds and
"set" is just "toggle away from the other one."

`apply_config` updates `gaps`/`workspace_gaps` from a loaded or
hot-reloaded `tili_config::Config`, creates any workspace it declares
(without switching to it, so a reload never yanks focus off whatever's on
screen), and rebuilds `mode_bindings` (M6: `HashMap<mode name,
HashMap<KeyCombo, Command>>`) from `config.keybindings`.
`Command::ModeEnter`/`ModeExit` switch `current_mode`;
`resolve_hotkey(combo)` looks a press up in the current mode's table, and
`active_key_combos()` returns just the keys (for syncing the `Mutex` the
hotkey tap reads — see [tili-ax.md](tili-ax.md)'s hotkey section).

`src/dispatch.rs` has the single `dispatch(&mut WmState, Command) ->
Response` function — both the Unix-socket handler and the global-hotkey
handler must call this same function, never a separate code path, or
CLI-invoked and hotkey-invoked behavior can drift apart.
`Command::Shutdown` is the one deliberate exception — it's process
lifecycle, not a `WmState` mutation, so both `main.rs`'s socket-accept and
hotkey `select!` arms check for it and `break` the loop directly instead of
routing it through `dispatch()` (which would have nowhere to signal "please
exit the process" from).

## main.rs — the event loop

One `tokio::select!` loop merging socket accepts,
`tili_ax::spawn_event_watcher()`'s channel, the config-reload bridge, the
hotkey-tap bridge, the display-watcher bridge (M9), and the mouse-watcher
bridge (M10) — no locks around `WmState` itself, because only one branch of
the loop ever touches it at a time; `sync_active_combos` is called after
every branch that could change the active mode/bindings, to keep the hotkey
tap's `Mutex<HashSet<KeyCombo>>` from drifting out of sync with what
`WmState` actually has bound.

`ensure_starter_config_exists` (M10) writes `example/tili.kdl` (via
`include_str!`) to `~/.config/tili/tili.kdl` before the first
`tili_config::load` if nothing's there yet — best-effort, a write failure
just falls back to `Config::default()` like before M10.

`maintenance_tick` is an unconditional 30ms `tokio::time::interval` branch
of the main `select!` loop — see [invariants.md](invariants.md)'s
polling-exceptions section for why it exists and what it costs.
