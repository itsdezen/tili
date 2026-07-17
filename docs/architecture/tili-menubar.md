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
  The one deliberate exception: a 1s backoff between reconnect attempts
  while the daemon is unreachable at all (not running, or between
  `tili stop`/`tili start`) — there's no notification to wait on when
  there's no connection to notify over.
- `src/actions.rs` handles menu clicks (`workspace:<name>` switches,
  `open-settings` opens the config via `$EDITOR`/`open`/`open -a TextEdit`
  in that fallback order, `quit` runs `tili stop` before exiting this
  process too) on its own background thread, since none of those reactions
  need the main thread.
