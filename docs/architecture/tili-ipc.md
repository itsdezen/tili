# tili-ipc — shared protocol

Part of the [architecture notes](../ARCHITECTURE.md).

`Command`/`Response` types shared by the daemon and CLI, plus the socket
path/framing convention. This is the only crate both `tili-daemon` and
`tili-cli` depend on in common — protocol changes belong here, not
duplicated in both binaries.

`src/parse.rs`'s `parse(s: &str) -> Command` (M6) turns a keybinding's
command string (`"focus left"`, `"mode resize"`) into a `Command` —
infallible by design, an unrecognized string becomes `Command::Raw` rather
than a parse error, so a config referencing a command ahead of its
milestone (or with a typo) still loads and just fails at `dispatch()` time
with "not implemented yet" instead of refusing to start the daemon.

`Command::Doctor`'s payload, `DoctorReport`, is the one `Command` whose
answer only the daemon can give (current Accessibility/Input Monitoring
permission grants, the previous config load's skipped-rule warnings, and the
last startup restore's matched/missing/deferred counts) — `tili doctor`
combines it with checks it can run without a daemon at all (LaunchAgent
files, the socket, config syntax, the state file). See
[tili-cli.md](tili-cli.md) and [tili-daemon.md](tili-daemon.md).

`Command::QueryState`'s payload, `QueryState`, is the external-integration
snapshot: versioned, and unlike `SnapshotInfo` it includes every tracked
window with `bundle_id`, `workspace`, `focused`, accessibility
`role`/`subrole`, placement, and frame. `tili query state --json` prints it
verbatim; `tili query windows|workspaces|monitors` print the matching
sub-array as JSON.

`Command::Subscribe` is a streaming command handled directly by the
daemon's event loop, not `dispatch()`: the daemon sends an initial
`SubscribeEvent { event: "snapshot" }`, then a `state_changed` event per
coalesced change. Each event carries a monotonic `seq` and the compact
`SubscribeState` (monitors, workspaces, mode, focused window) rather than
the full window list, so a busy workspace doesn't reserialize every window
on every change. The CLI prints each event as one JSON line; consumers that
need windows issue `QueryState` on demand. Subscribers use a small bounded
broadcast channel and are independent spawned tasks, so a slow client can't
block the main loop.

`Command::Snapshot`'s payload, `SnapshotInfo`, bundles monitors,
workspaces, the active mode, and the menubar style into one read. It exists
purely so `tili-menubar`'s refresh costs one socket connection and one
`dispatch()` (and, being read-only, no focus sync at all) instead of four —
the individual `ListMonitors`/`ListWorkspaces`/`CurrentMode`/
`MenubarStyle` commands stay in the protocol for every other client. Its
`style` field is `Option` so a payload without style data keeps the badge's
previously applied style rather than resetting it to the default, the same
best-effort rule the four separate queries used to give. Read-only, so it
joins the others in `tili-daemon`'s `command_is_read_only` exclusion from
`WaitForChange` change-detection. See [tili-menubar.md](tili-menubar.md).
