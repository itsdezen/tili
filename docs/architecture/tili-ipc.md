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
permission grants, and the previous config load's skipped-rule warnings) —
`tili doctor` combines it with checks it can run without a daemon at all
(LaunchAgent files, the socket, config syntax). See
[tili-cli.md](tili-cli.md) and [tili-daemon.md](tili-daemon.md).

`Command::Snapshot`'s payload, `SnapshotInfo`, bundles monitors,
workspaces, the active mode, and the menubar style into one read. It exists
purely so `tili-menubar`'s refresh costs one socket connection and one
`dispatch()` (whose focus sync is one system-wide AX query) instead of four
— the individual `ListMonitors`/`ListWorkspaces`/`CurrentMode`/
`MenubarStyle` commands stay in the protocol for every other client. Its
`style` field is `Option` so a payload without style data keeps the badge's
previously applied style rather than resetting it to the default, the same
best-effort rule the four separate queries used to give. Read-only, so it
joins the others in `tili-daemon`'s `command_is_read_only` exclusion from
`WaitForChange` change-detection. See [tili-menubar.md](tili-menubar.md).
