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
`placements`/`compute_floating_frame`, not the tree — a floating window only
ever gets its rule-based frame computed *once*, whichever moment it's first
actually placed (immediately at creation if its workspace is already
visible, or at the first reactivation of its workspace afterward if it
wasn't). `WmState::floating_placed: HashSet<WindowId>` records that this has
happened; `reposition_floating_for_monitor` (run every time a workspace
becomes active again) leaves a window with no captured `manual` geometry
alone once it's in that set, rather than re-deriving the same rule-based
frame on every later switch — so neither a gap change nor a routine
workspace switch undoes a user's manual drag, or silently re-centers a
window the user never touched. A window *with* captured `manual` geometry
(the user dragged/resized it) is still restored proportionally on every
reactivation, so a monitor swap or resolution change scales it sensibly.

A centered placement also gets a small `cascade_offset` nudge from
`place_floating_window` — otherwise several same-sized floating windows
centered at the same time would land on the exact same pixel and fully
overlap, with no way to see or grab anything but the topmost one.
`WmState::floating_centered: HashSet<WindowId>` records which floating
windows are currently on screen via such a centered placement (mirroring
`floating_placed`'s lifecycle — inserted/removed by `place_floating_window`
as the `center` rule dictates, cleared in `remove_placement`);
`floating_cascade_index` counts how many *other* windows in the same
workspace are in that set right now, and `cascade_offset` turns that count
into a `(dx, dy)` pixel nudge that's symmetric around dead center — `0,0`
when nothing else is currently centered, then alternating `±step,±step` at
growing magnitude the more windows are centered concurrently — rather than
drifting monotonically toward one corner, wrapping back to `0,0` every
`FLOATING_CASCADE_CYCLE` concurrently-centered windows so it never grows
unbounded. The result is clamped back into `area` the same way
`restore_floating_frame` clamps a restored frame, in case the nudge would
otherwise push the window off-screen. The index is deliberately derived
from live state instead of a persistent counter: an earlier version
advanced a per-workspace counter on every placement and never reset it,
which meant repeatedly opening and closing a *single* floating window (no
other centered window ever present) still walked through the cascade
sequence on every reopen instead of staying dead-center.
`state.rs` functions whose
tree-topology operation is meaningless for a floating focus (`move_focused`,
`join`, `resize`, `set_orientation`/`toggle_orientation`,
`toggle_layout`/`set_layout`, `balance_sizes`, non-native
`toggle_fullscreen`) check `focused_window_is_floating` up front and error
instead of silently acting on the wrong node or having no visible effect;
`focus`, `move_focused_to_workspace`, `set_floating`, `close_focused`, and
native fullscreen all work correctly for a floating focus and don't guard.

A brand-new window's disposition (`Tile`/`Float`/`Ignore`, resolved once at
creation — see `resolve_disposition`/`classify_new_window`) is decided by
`apply_windows_changed` in priority order: `is_system_ui_bundle` first,
then `is_protected_finder_dialog`, then `is_transient_empty_dialog`, then
the user's own `floating-rules` (`matching_floating_rule`), then finally
`tili_ax::WindowKind`'s AX-derived fallback. The first three are
unconditional overrides — checked *before* `self.floating_rules` is
consulted at all, so no config entry can win against them.
`is_system_ui_bundle` force-`Ignore`s a small denylist of system UI bundle
ids (Dock, Spotlight, SecurityAgent, `OSDUIHelper` — the volume/brightness
HUD host — etc.; see its own doc comment for the confirmed cases and why
it's bundle-id-only). `is_protected_finder_dialog` does the same for
exactly two Finder windows — the "Copy" progress sheet and the "Connect to
Server" dialog — matched by bundle id *and* title, since (unlike the
system UI cases) the rest of Finder's windows must still tile/float per
the user's own config; only these two specific windows are unconditionally
`Ignore`d. This is a confirmed real quirk, not preemptive: Finder's Copy
dialog doesn't reliably self-report as a dialog via AX subrole (other
tiling window managers hit the identical issue), so `tili_ax::WindowKind`'s
structural classification alone can't be trusted to catch it — it's
`Ignore`, not `Float`, because the user's ask was "never touch these
windows at all," not "float them." Titles here are static
(system-assigned, not content-derived), so the general
title-not-yet-populated-at-window-creation risk that applies to
user-authored `title=` rules is low in practice for this specific pair.
Extend `is_protected_finder_dialog` only for another *confirmed* Finder
window with the same problem, not preemptively — same rule as
`is_system_ui_bundle`.

`is_transient_empty_dialog` handles the input-source-switch HUD glyph
(Globe/Ctrl-Space) — confirmed via diagnostic logging that, unlike the
volume/brightness HUD, it isn't owned by a dedicated system helper process
at all: it's attributed to whichever app happens to be frontmost at the
moment (e.g. `com.mitchellh.ghostty`), so a bundle-id denylist entry can't
catch it without also force-`Ignore`ing that app's real windows. Matched by
shape instead — `WindowKind::Dialog` with an empty title — confirmed to
exist for well under `REMOVAL_GRACE_PERIOD` (closed ~100-120ms after
creation) each time. Deliberately not scoped to a specific bundle id: a
real floating dialog with no title at all is the rare case, not the
common one.

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
one active on the focused monitor, `place_new_window` immediately calls
`switch_workspace` on it, so a window auto-placed by a `workspace-rules`
match is never left off-screen — `move_focused_to_workspace` does the same
after moving the focused window into the target tree, for the same reason.

`apply_windows_changed` also re-runs `sync_focus_from_pid` once, right after
its loop, whenever this pass actually placed a brand-new window — closing a
real race where a window that's already real-OS-focused the instant it's
created can beat this function's own placement-registration: the reactive
sync paths (`dispatch()`'s `sync_focus_from_frontmost`, and
`reveal_frontmost`) resolve the focused window via a live AX query and then
look it up in `self.placements`, which has no entry yet for a window this
function hasn't finished processing, so that lookup silently no-ops with
nothing to retry it later. Without this, a workspace kept alive by one
long-running app (e.g. a terminal) could keep restoring focus to that app
instead of a just-opened, just-focused new window on the next switch away
and back — until some later, unrelated real focus change happened to
re-sync it.

The mirror-image gap exists on removal: `remove_from_tree` (shared by
`remove_placement`, `demote_to_special`, and `set_floating`) already
reassigns `workspace_focus` when the removed leaf was its workspace's
recorded focus, but that alone is only internal bookkeeping — it never
called `AxWindow::focus()` to make real macOS focus follow. Quitting the
real-focused app in a still-visible workspace left macOS free to reactivate
whatever app its *own*, tili-oblivious app-activation history points to
(commonly whatever was frontmost right before the quit app, which can live
on an entirely different, possibly-parked workspace) instead of the
sibling window still sitting in the same, on-screen workspace.
`remove_from_tree` now returns the reassigned node when the removed leaf
was the recorded focus *and* its workspace is currently visible on some
monitor (`None` otherwise, including once the tree empties out);
`remove_placement` — the only one of the three callers where the window is
genuinely gone rather than about to be reinserted — uses that to call
`raise_focused_window` for real. `demote_to_special` (minimize/hide/native
fullscreen) and `set_floating` deliberately ignore the return value: both
immediately reinsert the same window and re-focus it themselves right
after, so raising a sibling mid-flight there would just be a spurious
flash (and would be outright wrong for native fullscreen, where the same
window legitimately keeps real focus on its own Space).

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
exactly what `Tree::layout`'s weight model computed (under
`InstantFrameSetter`, in one write; under `TweenedFrameSetter`, eased
into over its animation duration — either way the *target* is always
`Tree::layout`'s own output, never adjusted after the fact). A window
whose app rounds/snaps a resize to its own grid (some terminal emulators
do this) can therefore end up larger than its assigned rect and encroach
on a neighboring tiled window's gap — a known, accepted OS/app-level
limitation, not something the layout engine tries to correct: any
after-the-fact size correction would violate the same invariant this
function exists to uphold, and re-writing a frame in response to a
notification the write itself caused risks a self-sustaining relayout
loop (see `apply_windows_changed`'s unconditional `relayout_active()`
call below, which is what would drive it) — `TweenedFrameSetter` hits
this exact loop roughly once per `maintenance_tick` for as long as an
animation runs (each intermediate write is real, unlike
`InstantFrameSetter`'s single no-op-guarded write, but `main.rs`'s
`pending_pids` already coalesces a notification burst down to one
`apply_windows_changed` per `maintenance_tick`) and handles it by
treating a `set_frame` call matching the tween already in flight's target
as a no-op on the tween, not a restart (see
`tili-ax/src/frame_setter.rs`'s module docs).

Mouse-based tile resize (dragging a tiled window's real native edge/corner,
not the keyboard shortcut) piggybacks on the same `mouse_button_down`-suppression
machinery `apply_windows_changed` already uses for floating-window native
drags — no new `CGEventTap` needed. `on_mouse_button_down` additionally
captures `resize_drag: Option<ResizeDragSnapshot>` (via
`capture_resize_snapshot`): the focused monitor's before-drag
`Tree::layout` output, or `None` if there's nothing valid to resize
against (no active workspace, a `fullscreen_focus` tiled-fullscreen window
showing, or fewer than 2 tiled windows — the same "alone" guarantee
`Tree::resize_handle_at` already enforces structurally). Because
`apply_windows_changed` keeps refreshing each window's cached `AxWindow`
frame throughout the drag even while relayout itself is suppressed, by the
time `on_mouse_button_up` fires, whichever window the user actually
dragged already has its real post-drag frame sitting in `self.windows` —
no separate observation plumbing needed, just a diff against the snapshot.
`on_mouse_button_up` runs that diff through `apply_mouse_resize`, which
finds the one window that moved and, for each edge that changed,
magnet-snaps a tree weight change via `magnet_resize_edge`: convert the
pixel delta to weight-space via `Tree::resize_handle_at`'s
`weight_per_pixel`, then either round it to the nearest whole multiple of
`Settings::mouse_resize_step` (the normal case, checked against
`Tree::resize_delta_bounds` so rounding can never itself overflow), or, if
the drag asked for more than `resize_delta_bounds` allows at all, overflow
straight to that boundary instead of refusing or rounding down to a
smaller whole step — exactly like spamming the keyboard shortcut past its
limit, which `apply_resize`'s own clamp always keeps honoring. A released
size therefore always matches either some whole number of
`resize <mouse_resize_step>` keypresses, or the tree's true max/min for
that border; a sub-half-step drag that's still within bounds is dropped
entirely rather than landing off-grid. This all runs *before* the existing
unconditional `relayout_active()` call, so siblings only ever move once —
straight to their final frames — on mouse-up, never live during the drag
itself.

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

`park()` writes straight to `AxWindow` (`set_position`), bypassing
`frame_setter` entirely — parking's destination isn't meant to be seen
mid-transition. `place_floating_window`'s centered branch only bypasses
`frame_setter` for its *size-discovery* step (`set_size` then
`live_frame()`, needed to learn a fixed-one-axis app's real, possibly
app-clamped size before a position can even be computed) — the actual
placement, once that real size is known, goes through
`frame_setter.set_frame` like any other floating placement, so it
animates too. Both call `frame_setter.finish()` before their direct write
so a tween left running from an *earlier* placement of the same window
can't resume on a later `animation_tick` and fight it; the centered
branch additionally calls `AxWindow::sync_frame` right after discovering
the real size, correcting the cache `set_size` left pointing at the
*requested* (possibly never-actually-on-screen) size, so the animated
move that follows interpolates from the window's true current frame
instead of a wrong one. `unpark_all` (shutdown-only) goes further and
always bypasses `frame_setter` outright, even when `Settings::animate` is
on: it's a one-shot restore that runs once, immediately before the
process exits, with no later tick left to actually finish an animated
write — routing it through `frame_setter` would leave windows stuck
off-screen instead of restored.

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
focused (via `tili_ax::AxWindow::system_focused_id`, an
`AXUIElementCreateSystemWide`-based query of the system-wide
`kAXFocusedWindowAttribute` directly, not "which app is frontmost, then
that app's own focused window") and updates `workspace_focus`
synchronously, immediately before that command runs. The direct
system-wide query matters: a floating panel/utility window can hold real
AX focus without its owning app ever becoming
`NSWorkspace.frontmostApplication` (confirmed for some non-activating
panels), which an app-first two-hop lookup — `tili_ax::workspace::
frontmost_app_pid()` then that pid's own focused window — would silently
miss, leaving `workspace_focus` on whatever a *different* app's window was
before. `apply_windows_changed`'s own re-sync (`sync_focus_from_pid`) still
uses the per-pid, app-first lookup, since it already knows the exact pid it
just placed windows for — both funnel into the same `sync_focus_to_window`
bookkeeping. This is deliberately
not a reactive background sync triggered by an event arriving whenever —
confirmed on real hardware that a background poll/notification updating
focus asynchronously has an unavoidable race against the very next hotkey
press, since there's no ordering guarantee between "the background sync
noticed the click" and "the keypress got processed." Other AX-based tiling
WMs resolve this the same way, synchronously at the top of every command —
this is the fix for a long-reported "the first direction key press after
switching windows manually does nothing/goes the wrong way" bug that
several reactive-sync attempts (an AX per-window notification, an
`NSWorkspaceDidActivateApplicationNotification` subscription — confirmed
dead at the time for a process like this one with no `NSApplication`
instance, since fixed and now used elsewhere for `WmEvent::FrontmostAppChanged`
(see `tili-ax.md`'s `watch.rs` section) — then a poll on `watch.rs`'s
resync tick) all failed to fully close: even a reliably-delivered push
notification still races the very next hotkey press, since there's no
ordering guarantee between "the notification arrived" and "the keypress got
processed" — only a synchronous, on-demand resolve at the top of every
command closes that gap. `sync_focus_to_window` (the shared core both
`sync_focus_from_frontmost` and `sync_focus_from_pid` funnel into) updates
`workspace_focus` for both `Tiled` and `Floating` placements — since
`Node::Floating` gave floating windows a tree node too (see the
"WmState, placements, and floating windows" section above), a real click
into a floating window is correctly reflected before the next command
runs, not just a tiled one, including one belonging to a different app
than whatever macOS still considers frontmost (see above).

`handle_event`'s `WmEvent::FrontmostAppChanged { .. }` arm (0.1.1) reacts by
(eventually — see the debounce note further down) calling
`WmState::reveal_frontmost(pid)` with whatever's actually frontmost at
execution time. `reveal_frontmost` itself mirrors `summon`'s body (resolve
a window, switch to/reveal its workspace
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
click: there's no real OS-level transition for
`NSWorkspaceDidActivateApplicationNotification` to fire on (see
[tili-ax.md](tili-ax.md)'s `watch.rs` section — this used to be a poll,
same conclusion either way), so `FrontmostAppChanged` never fires and
`reveal_frontmost` never runs. `WmState::reveal_current_frontmost` covers
this instead: `main.rs`'s
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

`reveal_current_frontmost`'s "re-resolve whatever's frontmost *right now*"
approach above has its own race: launching a brand-new app (a Dock click or
Spotlight selection that's a cold launch, not a reveal-already-running one)
also produces a `MouseSignal::ButtonUp`/`FrontmostAppChanged`, but at that
instant `frontmost_app_pid()` can still report the *previous* app — the new
process hasn't taken over AX-frontmost status yet, cold-launch latency
being real (Spotlight genuinely does become AX-frontmost while handling a
selection, unlike the Dock, so dismissing it right after launching a cold
app produces a real, if transient, frontmost-app edge back to whatever was
frontmost *before* Spotlight). `reveal_frontmost` then "reveals" that
stale/transient, unrelated pid, switching the display to wherever *it*
lives; moments later the new app's window actually appears and its own
placement (`place_new_window`) switches again to the real target, producing
a visible double-jump.

`WmEvent::AppLaunched { pid, .. }` (from `NSWorkspaceDidLaunchApplicationNotification`
via `tili-ax`'s workspace watcher) calls `WmState::note_app_launched(pid)`,
recording `pid` in `pending_launch_pids: HashMap<i32, Instant>` until it
either gets a real window (`apply_windows_changed` clears it once `fresh`
is non-empty), terminates (`remove_app`), or `launch_grace`
(`LAUNCH_GRACE_PERIOD`, 2s — a bound against a launched-but-windowless
process permanently wedging this) elapses (`finalize_expired_launches`).
`reveal_frontmost` won't switch workspaces while this set is non-empty —
the guard sits alongside the existing `suppress` check in its
not-currently-visible (`None`) branch, the only branch that can switch
workspaces, so it protects both callers (`reveal_current_frontmost` and
`handle_event`'s `WmEvent::FrontmostAppChanged` arm) from one place.
Neither caller invokes `reveal_frontmost` synchronously: both just arm
`pending_reveal_deadline: Option<tokio::time::Instant>` to `now +
REVEAL_DEBOUNCE`, and `maintenance_tick` runs the deferred
`state.reveal_current_frontmost()` once that deadline passes (after that
tick's own `pending_pids` processing, so a same-tick `WindowsChanged` for a
just-launched pid already clears `pending_launch_pids` first) — re-deriving
whatever's actually frontmost fresh at execution time rather than trusting
either trigger's own captured pid, which also lets `FrontmostAppChanged`'s
handler ignore the `pid` its event carries entirely.

`AppLaunched` isn't a reliable signal in practice, though:
`NSWorkspaceDidLaunchApplicationNotification` doesn't fire for every launch
this race can involve, so `pending_launch_pids` can end up empty for the
exact case it exists to catch — `REVEAL_DEBOUNCE` waiting longer buys
nothing against that gap, only added latency on ordinary Cmd-Tab/
Dock-click-reveal. What does reliably prevent the spurious switch is the
pre-existing `suppress` check above (a previous pid owning zero live
windows) — unrelated to anything in this section, already there before any
of this. `pending_launch_pids`/`REVEAL_DEBOUNCE` stay in place as a
secondary layer for whichever launches *do* get an `AppLaunched` event, at
low fixed cost (a `HashMap` lookup, a short deadline), but `suppress` is
the mechanism actually load-bearing for the common case. `REVEAL_DEBOUNCE`
is kept short (100ms) rather than grown to reliably beat
`NSWorkspaceDidLaunchApplicationNotification` latency, since that would
mean covering a full cold launch (hundreds of ms) to be dependable — the
same "two different timescales" problem `apply_config`'s doc comment
elsewhere argues against conflating. The cost: every reveal (Dock-click
*and* Cmd-Tab/Mission Control) is delayed by up to 100ms instead of the
click case's old ~0-30ms or the `FrontmostAppChanged` case's old 0ms, for
an unconfirmed reliability benefit against the launch race — an accepted
trade given `pending_launch_pids` costs little either way, not a value
tuned to be imperceptible.

A third race lives in the same debounce window: rapid, deliberate
workspace-switch hotkey presses landing on an empty target workspace.
`WmState::switch_workspace` never calls `raise_focused`/
`AxWindow::focus()` when the target has nothing to restore or
default-focus (its `restore` block only runs `if let Some(node) =
restore`), so real macOS frontmost-app state is left pointing at
whatever was focused on the *previous* workspace. If `pending_reveal_deadline`
was armed (a `FrontmostAppChanged` notification or a click) before the user
starts hopping through empty workspaces, it can still be pending when it
fires — `REVEAL_DEBOUNCE` plus one `maintenance_tick` after the trigger,
plus whatever `NSWorkspaceDidActivateApplicationNotification` delivery
itself took to arrive — by which point one or more explicit, synchronous
`Command::WorkspaceSwitch`
calls (via `dispatch()`) have already moved the display on.
`reveal_current_frontmost` re-derives the still-unchanged frontmost pid,
finds its workspace no longer visible, and calls `switch_workspace` back
to it, reverting the user's later navigation. `WmState::switch_epoch: u64`
closes this: incremented once per real (non-no-op, non-error)
`switch_workspace` call. Both `pending_reveal_deadline` call sites in
`main.rs` snapshot `state.switch_epoch()` into a sibling
`pending_reveal_epoch` when arming the deadline; `maintenance_tick` only
calls `reveal_current_frontmost()` if that snapshot still matches
`state.switch_epoch()` once the deadline fires — otherwise a newer,
authoritative switch has already superseded whatever triggered the
reveal, and it's dropped (the deadline itself is still always cleared). A
reveal that *does* still run and switches workspaces bumps the epoch
again itself, which is inert — nothing reads it again that tick.

`switch_epoch` alone doesn't close every variant of the same race, though:
confirmed on real hardware, hopping to an empty workspace can transiently
reassign AX-frontmost to a windowless system process (WindowServer, Dock)
during `park()`'s off-screen window move, before reverting back to the
real app moments later — two genuine, non-`None` pid edges,
`FrontmostAppChanged` firing for each. `reveal_frontmost` used to update
`last_frontmost_pid` unconditionally at the top of the function, before
checking whether `pid` even owns a focused window
(`AxWindow::focused_id_for_pid`) — so the windowless system pid's
transient edge would overwrite `last_frontmost_pid`, making the *next*
call (for the real, unchanged app reverting back) wrongly compute
`pid_unchanged = false` and chase it as a "genuine transition," even
though nothing the user did actually changed. Fixed in two parts:
`last_frontmost_pid` is now only updated once `pid` is confirmed to own a
real window, so a windowless system pid can't poison it; and
`reveal_frontmost`/`reveal_current_frontmost` take an `allow_unchanged_pid`
bool — the `None`/not-visible branch only switches workspaces when either
that's `true` or `pid_unchanged` is `false`. `main.rs` tracks this
alongside `pending_reveal_epoch` as `pending_reveal_allow_unchanged`: a
`MouseSignal::ButtonUp` arm sets it `true` (the legitimate Dock-icon
reactivation case above genuinely needs `pid_unchanged` to be true and
still switch); a `WmEvent::FrontmostAppChanged` arm sets it `false` (a
notification edge alone never justifies chasing a same-pid read once the
deferred check actually runs).

The actual dominant cause of the rapid-workspace-switch flicker, confirmed
via temporary diagnostic logging while reproducing it on real hardware,
turned out to be simpler than either race above: `switch_workspace` itself
raises/focuses a window (`raise_focused`) when entering a workspace that
already has one, which changes real macOS frontmost state — but
`last_frontmost_pid` used to only get updated *reactively*, inside
`reveal_frontmost`, whenever that function next happened to run. Between
"tili raises app X" and "watch.rs is told X is now frontmost" (at the time
this was found, a poll bounded by `RESYNC_INTERVAL`, 250ms — now a
push notification, see [tili-ax.md](tili-ax.md)'s `watch.rs` section, but
the delivery is still asynchronous, so the same shape of gap remains, just
smaller and unbounded rather than a fixed 250ms) there's a lag; if the user
has already hotkeyed onward to a different (often empty) workspace by the
time that late, self-inflicted edge is detected, `reveal_frontmost`
computed `pid_unchanged` against a `last_frontmost_pid` that was still
whatever it was *before* tili's own raise — reading `false`, i.e. "a
genuine transition," and chasing back to the workspace the raise happened
on. `raise_focused`/`raise_focused_window` (both now `&mut self`) set
`self.last_frontmost_pid = Some(window.pid())` synchronously at the same
point they call `window.focus()`, so `reveal_frontmost` sees the
already-known, unchanged pid whenever the (now push-based) detection of a
tili-caused focus change eventually arrives — `pid_unchanged` correctly
reads `true`, and (per `allow_unchanged_pid` above) that reveal skips it
instead of chasing. `switch_epoch` and `allow_unchanged_pid` remain valid
defense-in-depth for the races described above (a real external Cmd-Tab
racing a rapid switch, and the windowless-system-pid edge respectively) —
this fix closes the specific mechanism that was actually firing in the
reported repro, and still applies unchanged now that the signal is
push-based rather than polled.

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

A `workspace-rules` entry naming an undeclared workspace, or a
`floating-rules` entry with a title regex that fails to compile, is skipped
rather than rejecting the whole config — both sites `eprintln!` (for the
log file) *and* push the same message into `WmState::config_warnings`,
cleared and rebuilt on every `apply_config` call so it always reflects only
the *last* load. `Command::Doctor` (below) is what makes this reachable
from outside the log file.

`Command::Doctor`, requested by `tili doctor` (see
[tili-cli.md](tili-cli.md)), reports `config_warnings()` alongside a fresh
read of both permission grants (`tili_ax::ensure_accessibility_permission()`
and `has_input_monitoring_permission()`). Calling the former again here
doesn't re-prompt: the daemon is only alive to answer this at all because
it already passed that exact check once at its own startup (see
`stop_self` below for what happens when it doesn't), and macOS only shows
the system dialog for a *not-yet-decided* grant — a re-check against an
already-trusted process just reports `true` with no dialog. Read-only, so
it's excluded from `mutates` in `main.rs`'s socket arm the same way
`Ping`/`ListWindows`/`ListWorkspaces`/`ListMonitors` are.

`src/dispatch.rs` has the single `dispatch(&mut WmState, Command) ->
Response` function — both the Unix-socket handler and the global-hotkey
handler must call this same function, never a separate code path, or
CLI-invoked and hotkey-invoked behavior can drift apart.
`Command::Shutdown` is the one deliberate exception — it's process
lifecycle, not a `WmState` mutation, so both `main.rs`'s socket-accept and
hotkey `select!` arms check for it and `break` the loop directly instead of
routing it through `dispatch()` (which would have nowhere to signal "please
exit the process" from). Both arms also call `stop_menubar()` before
breaking — the daemon and `tili-menubar` are meant to run as a synchronized
pair, so any path that shuts this process down on purpose tears the badge's
LaunchAgent down too, rather than leaving it running (and polling a socket
that's about to disappear) alone. `stop_self` (called when Accessibility/
Input Monitoring permission isn't granted) does the same. This only covers
*intentional* shutdown paths — an unexpected crash runs no code at all — so
`tili-menubar` separately guards against that case by giving up on its own
after a sustained run of unreachable-daemon retries (see
[tili-menubar.md](tili-menubar.md)).

## main.rs — process structure and the event loop

The real process entry point, `fn main()`, is deliberately *not*
`#[tokio::main]`. It sets up a real `NSApplication` (matching
`tili-menubar`'s identical pattern), registers `NSWorkspace` notifications
on the real main thread via `tili_ax::register_on_main` (see
[tili-ax.md](tili-ax.md) — this exists because that registration reliably
delivers `NSWorkspaceDidLaunchApplicationNotification`/`DidWakeNotification`
only when a real `NSApplication.run()` is pumping the main run loop that
receives it, confirmed on real hardware after those notifications were
silently never delivered to an `NSApplication`-less process), then spawns a
background thread that builds its own `tokio::runtime::Runtime` and
`block_on`s `async_daemon_main` — the whole daemon body that used to be
`main()` itself, essentially unchanged otherwise. The real main thread then
parks in `app.run()`, which never returns.

Because `app.run()` never returns on its own, `async_daemon_main` returning
no longer ends the process the way it used to when it *was* `main()` —
every exit path (the accessibility-not-granted early return, and the
`Command::Shutdown` `break`) must reach the `std::process::exit` call after
`block_on` returns in `fn main()`, or the process would become a zombie:
background thread gone, `app.run()` still parked, answering nothing.

One `tokio::select!` loop inside `async_daemon_main` merges socket accepts,
`tili_ax::spawn_event_watcher()`'s channel (fed by the main-thread-registered
`NSWorkspace` receiver, passed in as a parameter), the config-reload bridge,
and `tili-ax`'s hotkey-tap/display-watcher (M9)/mouse-watcher (M10) channels
— no locks around `WmState` itself, because only one branch of the loop ever
touches it at a time; `sync_active_combos` is called after every branch that
could change the active mode/bindings, to keep the hotkey tap's
`Mutex<HashSet<KeyCombo>>` from drifting out of sync with what `WmState`
actually has bound.

The hotkey arm's dispatch result is checked, unlike the socket arm it
mirrors having a `Response` to write back to — there's no connection to
reply on for a hotkey, so `eprintln!("tili-daemon: hotkey command failed:
{message}")` on a `Response::Err` is the only feedback channel available;
a hotkey that legitimately fails (no focused window, an undeclared
workspace) at least leaves a trace now instead of vanishing silently.

`socket.rs::read_command` caps an incoming frame's declared length at
`MAX_COMMAND_LEN` (1 MiB) before allocating a buffer for it — the 4-byte
length prefix comes from whatever connects to this per-user socket, so
without a cap a stray or malformed frame can request an allocation up to
~4 GiB. A real `Command` is a few hundred bytes; the cap is headroom, not a
tight fit. Over-length frames are rejected with a descriptive `io::Error`,
surfaced through the same generic `eprintln!("tili-daemon: failed to read
command: {e}")` the accept arm already had for any other read failure.

`tili_ax::spawn_hotkey_tap`/`spawn_display_watcher`/`spawn_mouse_watcher`
each build and send on a `tokio::sync::mpsc` channel directly from their own
dedicated thread, so `main.rs` calls them straight — no separate relay-
thread bridge for these three (an earlier version had one per watcher,
purely forwarding `std::sync::mpsc` into `tokio::sync::mpsc`; removed since
`tili-ax` already depends on Tokio and `UnboundedSender::send` is a plain
synchronous call, legal from any thread). `spawn_config_reload_bridge` is
the one bridge that's still a separate relay thread, because
`tili_config::spawn_config_watcher` deliberately stays runtime-agnostic
(`std::sync::mpsc`, not `tokio`) — see [tili-config.md](tili-config.md).

`ensure_starter_config_exists` (M10) writes `example/tili.kdl` (via
`include_str!`) to `~/.config/tili/tili.kdl` before the first
`tili_config::load` if nothing's there yet — best-effort, a write failure
just falls back to `Config::default()` like before M10.

`maintenance_tick` is an unconditional 30ms `tokio::time::interval` branch
of the main `select!` loop — see [invariants.md](invariants.md)'s
polling-exceptions section for why it exists and what it costs.

`WmEvent::SystemDidWake` (from `NSWorkspaceDidWakeNotification`, registered
via `tili_ax::register_on_main` — see [tili-ax.md](tili-ax.md), confirmed on
real hardware across several repeated sleep/wake cycles to be reliably
delivered now that `tili-daemon` has a real `NSApplication`) calls
`WmState::note_system_wake`, which for the following `WAKE_REMOVAL_GRACE`
(90s) does two things:

- Boosts `finalize_expired_removals`'s effective grace period from
  `REMOVAL_GRACE_PERIOD` (100ms) to `WAKE_REMOVAL_GRACE`. Without this, a
  still-open window whose owning app simply hasn't reconnected to the
  WindowServer/AX yet after wake (observed to take several seconds, far
  longer than 100ms — apps don't all reconnect at once) missed a scan, got
  `finalize_expired_removals`'d as closed, then reappeared on the next scan
  and was treated as a brand-new window — re-triggering any matching
  `workspace-rules` entry and yanking the active workspace out from under
  whatever the user was looking at right before sleep.
- Makes `reveal_frontmost` distrust the `frontmost_app_pid()` read it's
  given — that function is a synchronous `AXFocusedApplication` query, and
  it too can transiently misread which app is frontmost while AX/WindowServer
  is still reconnecting post-wake. A misread pid still owns live windows (so
  `reveal_frontmost`'s "previous app has zero windows left" suppression
  doesn't catch it), so left unguarded it looks exactly like a genuine
  Cmd-Tab and force-switches the active workspace to that pid's workspace.

Both failure modes produce the same user-visible symptom — the active
workspace silently jumping away sometime after waking — just through
different call paths, so both are gated on the same `wake_grace_until`
window rather than two separate timers. `REMOVAL_GRACE_PERIOD` itself stays
at 100ms for the ordinary case (a genuinely-closed window should disappear
promptly, not linger up to 90s); both boosts only apply in the bounded
window right after a real wake, so a genuine post-wake app switch or window
close isn't permanently ignored. `WAKE_REMOVAL_GRACE` was raised from an
initial 8s to 90s after real-hardware confirmation that 8s was still too
short — `NSWorkspaceDidWakeNotification` fires at the hardware wake itself,
which can precede the user finishing unlock (password/Touch ID) by close to
a minute, and apps don't meaningfully resume reconnecting to the
WindowServer/AX until after that, not from the wake instant.

`note_system_wake`, `finalize_expired_removals` (when it actually finalizes
something), and both `switch_workspace` auto-trigger call sites
(`place_new_window`, `reveal_frontmost`) each log a line via `eprintln!` —
this class of bug has recurred across several releases and only reproduces
on real hardware after a real sleep/wake cycle, so a log trail of which path
fired and when is worth more than trying to reason about it statically.
