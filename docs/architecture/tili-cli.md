# tili-cli — thin socket client

Part of the [architecture notes](../ARCHITECTURE.md).

`ping`, `list-windows`, `focus <dir>`, `move <dir>`, `list-workspaces`,
`workspace <name>`, `move-to-workspace <name>`,
`layout <toggle|tiles|accordion>`, `focus-monitor`, `list-monitors`,
`stop`, `status`. The package is named `tili-cli` but the binary itself is
named `tili` (see the `[[bin]]` section in its `Cargo.toml`). No business
logic belongs here — if you're tempted to add logic to the CLI, it probably
belongs in `tili-daemon` behind a `Command` instead.

`print_response` needs an `ExpectedPayload` hint per subcommand since
`Response::OkWithPayload` carries an untyped `serde_json::Value` — add a
new variant there (not JSON-shape sniffing) when a command gets a new
payload type.

Two exceptions to "no business logic here," both intercepted in `main()`
before the socket-connecting code path (each `return`s instead of falling
through to the generic `send()`/`print_response` path):

- `tili start`/`stop` manage tili-daemon's LaunchAgent entirely on the
  local filesystem, never touching the daemon's socket. `start_daemon()`
  resolves `tili-daemon` relative to the running `tili` binary's own
  directory (`daemon_binary_path()`, via `std::env::current_exe()`, not
  `PATH` — a LaunchAgent's environment doesn't guarantee one), writes
  `~/Library/LaunchAgents/com.tili.daemon.plist` (`RunAtLoad` + `KeepAlive`
  both `true`), and `launchctl load -w`s it — this is the *only* way to run
  tili-daemon; there's no separate foreground mode. `stop_daemon()` is the
  reverse: `launchctl unload -w` then remove the plist. Unloading (not just
  killing the process) is load-bearing — `KeepAlive` only respawns the job
  while it stays loaded, so `tili stop` has to unload before the daemon can
  actually stay down.
- `tili status` *does* talk to the socket (via `Command::Ping`) but gets
  its own wording instead of the generic "couldn't reach daemon" error
  path.

`tili start`/`stop`/`uninstall` manage `tili-menubar`'s LaunchAgent
alongside the daemon's own (see [tili-menubar.md](tili-menubar.md)), so the
badge's lifecycle never has to be driven separately.
