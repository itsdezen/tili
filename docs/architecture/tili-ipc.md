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
