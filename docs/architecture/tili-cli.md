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

Three exceptions to "no business logic here," all intercepted in `main()`
before the socket-connecting code path (each `return`s instead of falling
through to the generic `send()`/`print_response` path):

- `tili start`/`stop` manage tili-daemon's LaunchAgent entirely on the
  local filesystem, never touching the daemon's socket. `start_daemon()`
  resolves `tili-daemon` relative to the running `tili` binary's own
  directory (`daemon_binary_path()`, via `std::env::current_exe()`, not
  `PATH` — a LaunchAgent's environment doesn't guarantee one), writes
  `~/Library/LaunchAgents/com.tili.daemon.plist` (`RunAtLoad` + `KeepAlive`
  both `true`), and `launchctl load -w`s it — this is the *only* way to run
  tili-daemon; there's no separate foreground mode. `install_launch_agent`
  (shared with the menu bar badge) unloads the label first if it's already
  loaded — `launchctl load` on an already-loaded label doesn't apply a
  rewritten plist (launchd keeps using what it cached at the earlier
  `load`), it just fails noisily (`Load failed: 5: Input/output error`)
  while still exiting 0; unloading first avoids that noise and makes a
  changed `ProgramArguments` path (e.g. after an upgrade) actually take
  effect. `stop_daemon()` is the reverse: `launchctl unload -w` then remove
  the plist. Unloading (not just killing the process) is load-bearing —
  `KeepAlive` only respawns the job while it stays loaded, so `tili stop`
  has to unload before the daemon can actually stay down.
- `tili status` *does* talk to the socket (via `Command::Ping`) but gets
  its own wording instead of the generic "couldn't reach daemon" error
  path.
- `tili doctor` (`fn doctor`) runs a fixed list of checks, prints one
  aligned line per check via `doctor_line`, then offers to fix whichever
  ones are safely automatable. Local checks need no daemon: config file
  existence + `tili_config::load` for a syntax check (the one dependency
  exception to "thin" — `tili-config` has zero macOS-specific deps, unlike
  `tili-ax`, so this doesn't compromise `tili-cli` staying buildable without
  Xcode), both LaunchAgents' presence/loaded state via the existing
  `launch_agent_path`/`launch_agent_is_loaded` helpers, a daemon/menu-bar
  pairing mismatch (one installed without the other), and a stale IPC
  socket file (exists but `daemon_is_reachable()` fails — left behind by an
  unclean shutdown, since a live daemon always removes it on exit). If the
  daemon *is* reachable, one `Command::Doctor` round trip adds permission
  grants and the last config load's warnings — both live only in the
  daemon's process, so `doctor` asks rather than re-implementing either
  check client-side (see [tili-daemon.md](tili-daemon.md)). Auto-fixable
  problems (a stale socket, an unloaded LaunchAgent, a missing half of the
  daemon/menu-bar pair) collect into a `Vec<(String, Box<dyn FnOnce()>)>`;
  a bad config file or an ungranted permission is reported only — never
  guessed at. One confirm gate for every fix at once (reusing
  `confirm_default_start`'s Enter-to-continue pattern), skippable with
  `tili doctor --fix` for scripting.

`tili start`/`stop`/`uninstall` manage `tili-menubar`'s LaunchAgent
alongside the daemon's own (see [tili-menubar.md](tili-menubar.md)), so the
badge's lifecycle never has to be driven separately. `start_daemon()`
requires the badge's install to succeed, not just the daemon's — a badge
install failure stops the daemon back down via `stop_daemon()` rather than
leaving it running alone, since the two are meant to run as a synchronized
pair. Runtime desyncs (one crashing outside `tili stop`) are handled on the
other two sides instead: `tili-daemon`'s shutdown paths tear the badge's
LaunchAgent down too, and `tili-menubar` stops itself if the daemon goes
unreachable for long enough — see [tili-menubar.md](tili-menubar.md).

`uninstall()`'s config-file removal checks `std::fs::symlink_metadata`
(doesn't follow the link, unlike `Path::exists()`) first — a symlinked
`tili.kdl` is left in place rather than unlinked, since a dotfiles manager
(stow, chezmoi, ...) likely put it there and removing even just the link
(not the real target file `remove_file` would leave untouched) still
breaks that tool's arrangement. `symlinked_ancestor` covers the other half
of this: a dotfiles tool symlinking the whole `~/.config/tili` directory
rather than just the file inside it — `symlink_metadata` on the file alone
resolves transparently through a symlinked parent and reports an ordinary
regular file, so `uninstall()` walks every ancestor directory first and
skips deletion if any of them is itself a symlink.

`sibling_binary_path` resolves the daemon/menubar's own sibling binary path
for `install_launch_agent`'s `ProgramArguments`, canonicalized so it lands
inside `tili.app/Contents/MacOS/` rather than Homebrew's flat `bin/`
symlink (needed for System Settings/the menu bar to resolve tili's name and
icon). Under a Homebrew install specifically, `homebrew_stable_equivalent`
rewrites that canonicalized path through `<prefix>/opt/tili` instead of the
literal, version-pinned Cellar path — `opt/tili` is a symlink Homebrew
relinks to whichever keg is current on every `brew upgrade`, so a
LaunchAgent plist built from it keeps pointing at the right binary across
upgrades. This matters because `Formula/tili.rb`'s `post_install` can only
restart the process (`pkill` + the plist's own `KeepAlive`), never rewrite
an already-loaded plist — without this, every upgrade kept relaunching
whatever binary path a prior `tili start` had baked in, silently running
stale code until a manual `tili stop && tili start`.
