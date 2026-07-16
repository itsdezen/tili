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
  The one deliberate exception: a 1s backoff between reconnect attempts
  while the daemon is unreachable at all (not running, or between
  `tili stop`/`tili start`) — there's no notification to wait on when
  there's no connection to notify over.
- `src/actions.rs` handles menu clicks (`workspace:<name>` switches,
  `open-settings` opens the config via `$EDITOR`/`open`/`open -a TextEdit`
  in that fallback order, `quit` runs `tili stop` before exiting this
  process too) on its own background thread, since none of those reactions
  need the main thread.
