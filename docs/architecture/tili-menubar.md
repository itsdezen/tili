# tili-menubar — the workspace badge

Part of the [architecture notes](../ARCHITECTURE.md).

An `NSStatusItem` badge showing the focused monitor's active workspace,
plus a dropdown to switch workspaces, open the config file, or quit.

- `src/badge.rs`'s `image_for` renders the badge as a solid rounded-pill
  `NSImage` with the text "knocked out"
  (`NSCompositingOperation::DestinationOut`) so the menu bar shows through
  in the shape of the letters, then marks it a template image so AppKit
  tints it correctly for light/dark/highlighted state — a leading
  filled-dot glyph is drawn at its own smaller font size with a computed
  baseline offset so it lines up with the workspace name's visual center
  instead of sitting low.
- `src/menu.rs`'s `MenuState` tracks the last-applied `(current workspace,
  workspace name list)` key so `apply_snapshot` only rebuilds the live
  `NSMenu` when that key actually changes (rebuilding unconditionally on
  every tick previously caused menu clicks to fire on their own with zero
  user interaction) — the badge title and visibility are still updated
  every tick regardless, since those are cheap. A daemon-unreachable poll
  result (`None`) hides the status item entirely rather than leaving a
  stale workspace name on screen.
- `src/ipc.rs` duplicates `tili-cli`'s own socket framing rather than
  sharing it (same precedent as `tili_ipc::default_socket_path`'s doc
  comment) and adds `wait_for_change`, sent as `Command::WaitForChange` —
  this blocks server-side (via `tili-daemon/src/main.rs`'s `change_notify:
  Arc<tokio::sync::Notify>`, spawned into its own task per connection so it
  doesn't block the main `select!` loop) until something actually changes
  or a 30s internal timeout fires, so `tili-menubar` learns about
  workspace/monitor changes the instant they happen instead of polling —
  `main.rs` runs this on a dedicated background thread in a loop, feeding
  results to the main thread over an `mpsc::channel` drained by a cheap
  50ms `NSTimer` tick (not itself a poll interval — the channel is normally
  empty; real work only happens right after `wait_for_change` unblocks).
  Server-side, the accept arm calls `change_notify.clone().notified_owned()`
  *synchronously*, before `tokio::spawn`-ing the task that awaits it — not
  lazily inside the spawned task — since `Notify` snapshots its
  wakeup-count baseline at the moment `notified()`/`notified_owned()` is
  called; capturing it only after the task actually starts running on a
  worker thread left a real gap where a rapid burst of changes (e.g.
  spamming a workspace-switch hotkey) could fire `notify_waiters()` between
  spawn and that first poll, silently missing the wakeup and leaving the
  badge stuck on a stale workspace until the next unrelated change or the
  30s timeout.
  Client-side, `wait_for_change`'s own loop spawns `menu::poll_daemon()`
  (the two follow-up round trips — `ListMonitors` then `ListWorkspaces` —
  that fetch the state a wakeup implies changed) onto its own short-lived
  thread instead of running it inline, so the loop re-issues the next
  `wait_for_change` immediately rather than after those two round trips
  finish. `Command::WaitForChange`'s server-side wakeup isn't a queue —
  nothing is subscribed while no connection is open — so a change firing
  during that gap used to be silently missed rather than merely delivered
  late; a rapid hotkey burst landed in it often enough to be noticeable
  even after the server-side fix above.
  The background thread also polls once, unconditionally, *before* ever
  calling `wait_for_change` the first time — otherwise the badge stays in
  `build_initial`'s hidden state until the first real change happens
  (which on a freshly-started daemon can be up to `WaitForChange`'s own
  30s idle timeout away), rather than showing the daemon's actual current
  state as soon as it's reachable.
  The one deliberate exception: a 1s backoff between reconnect attempts
  while the daemon is unreachable at all (not running, or between
  `tili stop`/`tili start`) — there's no notification to wait on when
  there's no connection to notify over. After `MAX_CONSECUTIVE_FAILURES`
  (60, roughly a minute) of these back-to-back, `main.rs` calls `stop_self`
  — unloads and removes this badge's own LaunchAgent, then exits — rather
  than retrying forever: the daemon and the badge are meant to run as a
  synchronized pair, so a daemon gone this long is treated as stopped on
  purpose, not a transient blip (e.g. the brief restart a Homebrew
  upgrade's `post_install` triggers). `stop_self` mirrors `tili-daemon`'s
  own function of the same name and `tili-cli`'s `stop_daemon` — plain
  `std::process::exit` alone wouldn't stick, since `KeepAlive` in the plist
  would just have launchd respawn this process right back into the same
  dead end.
  `ListMonitors`/`ListWorkspaces` (and `Ping`/`ListWindows`) are excluded
  from what counts as "changed" on the server side
  (`tili-daemon/src/main.rs`'s socket command arm) precisely because this
  poller calls the first two on every single wakeup: without the
  exclusion, each poll's own read-only queries re-notified every blocked
  `WaitForChange` connection — including whichever one this same poller
  had just re-subscribed — turning the long-poll design back into a
  continuously-spinning loop the instant the first real change of a
  session happened. Worth fixing on its own (a "long-poll" design spinning
  continuously defeats its entire purpose), but ruled out as the cause of
  the known issue below via `tili workspace <name>` from a terminal (goes
  through the identical socket path) staying fast throughout.
- `src/actions.rs` handles menu clicks (`workspace:<name>` switches,
  `open-settings` opens the config via `$EDITOR`/`open`/`open -a TextEdit`
  in that fallback order, `quit` runs `tili stop` before exiting this
  process too) on its own background thread, since none of those reactions
  need the main thread.

## Known issue: ~1s delay between clicking a menu item and its action firing

Clicking a workspace item in the already-open dropdown consistently takes
about a second before `actions::handle` (and thus `ipc::send`) even runs —
confirmed via direct instrumentation. Ruled out: the status item's dropdown
itself opens instantly (not a menu-build/layout cost); `ipc::send` measures
in single-digit milliseconds once called (not IPC/socket/daemon — the
`hotkey`/CLI paths, which skip `tili-menubar` and this crate's threads
entirely, are always fast); the delay doesn't shrink on a second click
fired immediately after the first (not App Nap throttling a backgrounded
thread waking up — that would predict the opposite). That leaves the gap
squarely inside AppKit's own `NSMenuItem` target-action dispatch
(`fireMenuItemAction:`, wired synchronously in `muda`'s
`fire_menu_item_click` — confirmed via that crate's source, both the
predefined-item and regular-item paths use the same selector) between
mouse-up on an open menu and that method actually being invoked — outside
what either this crate or `muda` control, and not reproducible via reading
source alone. Tried and reverted: calling
`NSApplication::activateIgnoringOtherApps` at startup (a known workaround
for a *different*, superficially similar accessory-app AppKit quirk —
menus staying unresponsive until the app is tabbed away from and back) had
no effect here. Unresolved; needs a profiler (e.g. Instruments) attached to
a running `tili-menubar` to go further, not more source reading.
