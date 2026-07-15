# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## Commands

```sh
cargo build --workspace              # build every crate
cargo test --workspace               # run all tests
cargo test -p tili-tree              # test a single crate (the only one that runs on non-macOS)
cargo test -p tili-tree <test_name>  # run a single test
cargo run --bin tili-daemon          # run the daemon directly (not via `cargo install`)
cargo run --bin tili -- ping         # run the CLI directly
```

Before committing, run the exact gate CI enforces (a red PR blocks merge, so
run this locally first):

```sh
cargo fmt --all --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
```

If `cargo fmt` reformats something, that's expected — just run `cargo fmt`
(no `--check`) and re-stage. Clippy warnings are hard errors here (`-D
warnings`); don't `#[allow]` one without a one-line comment explaining why
(see the `#[allow(dead_code)]` on `Tree` in `tili-tree` for the pattern —
intentional scaffolding pending a specific milestone, not a shrug).

`tili-ax` (and anything depending on it) only builds on macOS — it links
against `AXUIElement`/Core Graphics/Core Foundation. `tili-tree` has zero
macOS dependencies by design; prefer adding logic there over `tili-ax` when
possible so it stays testable without a Mac.

## Architecture

This is a Cargo workspace, not a single crate. The split is deliberate and
the dependency direction is a hard boundary, not just organization:

- **`tili-tree`** — the container tree and layout algorithms (Tiles/BSP,
  Accordion). No `AXUIElement`, no CoreFoundation, no `unsafe`. Everything
  here operates on plain `Rect`/`WindowId` (a `u32` newtype around the real
  `CGWindowID`) so it's fully unit-testable without macOS — see
  `src/tree.rs`'s test module for the actual coverage (insert/remove with
  parent-split collapsing, i3-style direction `navigate`, `move`'s
  window-identity `swap_windows`, proportional `layout`, Accordion
  toggle/cycle/wrap). `insert_window` always wraps the target leaf in a
  fresh 2-child `Split` rather than flattening into an existing
  same-orientation split — a deliberate M3 simplification (still a valid,
  correctly-tiling tree; just not the shallowest possible one — also means
  a "flat" Accordion built via sequential inserts only ever has 2
  children, see the M7 accordion tests). `layout(area, gaps)` takes a
  `Gaps` (outer padding around the whole area, inner spacing between
  siblings, both `f64` — `tili-config`'s parsed `u32` gaps get converted
  at the `tili-daemon` boundary since this crate can't depend on
  `tili-config`). `toggle_layout(from)` (M7) converts `from`'s parent
  container between `Split` and `Accordion` in place — converting *to*
  Accordion sets `active` to `from`'s own position so the
  currently-visible window doesn't change. `focus_in_direction(from, dir)`
  is the Accordion-aware navigation entry point `WmState` actually calls
  (not plain `navigate`): if `from`'s parent is an `Accordion`, `dir`
  cycles (and wraps at the ends) which child is active instead of doing
  spatial `Split` navigation, since a stack of fully-overlapping children
  has no inherent left/right/up/down axis.
- **`tili-ax`** — the only crate allowed to touch the Accessibility API.
  Depends on `tili-tree` only for geometry types (`Rect`), never for the tree
  itself. `src/window.rs` owns the single private API call used anywhere in
  the codebase (`_AXUIElementGetWindow`, to resolve a window's real
  `CGWindowID`) — keep that call isolated there; don't add other private API
  usage without a strong reason, since staying public-API-only is what lets
  tili run without disabling SIP. `AxWindow::set_frame`/`set_position`/
  `focus` (also in `window.rs`) are the only place real windows get
  moved/resized/raised — `set_frame` sets position before size (some apps
  clamp size based on current position), `set_position` only moves (used to
  park a window off-screen without needlessly resizing it, M4), and both
  writes are best-effort (`let _ =` on the AX result; a window that refuses
  a write is left alone, matching every other AX-based WM). Both also update
  the cached `frame` field to match what was just written, so
  `WmState::list_windows` reflects reality without a wasted AX read-back —
  this is why `WindowFrameSetter::set_frame` takes `&mut AxWindow`.
  `src/frame_setter.rs` defines the `WindowFrameSetter` trait — every place
  that moves/resizes a real window must go through `dyn WindowFrameSetter`,
  not call `AxWindow::set_frame` directly. v1 only implements
  `InstantFrameSetter`; this trait is the seam a future animated setter
  plugs into without touching layout code. `src/display.rs` (M9)
  enumerates every connected display via `CGDisplay::active_displays()` —
  `list_monitors()` is re-run fresh on every call (nothing cached) so
  hot-plug/unplug just falls out of calling it again; each `Monitor`'s
  usable `frame` is its full `CGDisplay` bounds minus a hardcoded menu-bar
  inset applied only when `is_main` (secondary displays don't carry a menu
  bar). This is a deliberate, documented simplification over real
  `NSScreen.visibleFrame` (which would be more precise about notches/Dock
  placement but requires flipping between `NSScreen`'s bottom-left-origin
  coordinate space and AX/`CGDisplay`'s top-left-origin one — judged not
  worth the risk for what M9 needs). `spawn_display_watcher()` registers a
  `CGDisplayRegisterReconfigurationCallback` on its own dedicated
  `CFRunLoop` thread (same reasoning as the NSWorkspace/AX watchers) and
  just signals "something changed, re-enumerate" per callback — it doesn't
  interpret `CGDisplayChangeSummaryFlags`. `src/workspace.rs`
  bridges `NSWorkspace` app-launch/quit notifications via `objc2`/
  `objc2-app-kit` — note it spawns its own dedicated `CFRunLoop` thread,
  since a process without `NSApplication` needs *some* thread pumping a run
  loop to receive Cocoa notifications at all (same reason `axuielement`'s own
  `AXNotificationStream` does the same for AX notifications). It also has
  `bundle_id_for_pid` (M8, via `NSRunningApplication`) — `enumerate.rs`
  resolves this once per process and shares it across all of that process's
  `AxWindow`s, rather than once per window, since it's used to match
  floating rules. `src/watch.rs` ties both together into
  `spawn_event_watcher()`, which subscribes each running app to window
  lifecycle notifications and emits a single coarse
  `WmEvent::WindowsChanged { pid }` per change — callers re-read that
  process's windows via `list_windows_for_pid` rather than trying to
  interpret individual notification payloads (this sidesteps having to
  reason about whether a specific `AXUIElement` is still valid to query at
  the exact moment its destroyed-notification fires). `src/hotkey.rs` (M6)
  is the global hotkey capture: a `CGEventTap` on its own dedicated
  `CFRunLoop` thread (same reasoning as the NSWorkspace/AX watchers above),
  which consumes (drops) a keypress if it's in the caller-supplied
  `active_bindings` set and passes everything else through untouched.
  `parse_key_combo` turns a KDL key string like `"alt-shift-h"` into a
  `KeyCombo` (`key_code_for_name` is the exhaustive keycode table — extend
  it there if a config references a key name it doesn't recognize).
  `active_bindings` is an `Arc<Mutex<HashSet<KeyCombo>>>` because the
  event-tap callback must decide Keep-vs-Drop *synchronously* — it can't
  `.await` a round-trip to `tili-daemon`'s single owning loop to ask "is
  this bound?" This is the one place in the codebase with a shared `Mutex`
  instead of message-passing into one owner; see `tili-daemon`'s
  `sync_active_combos` for how it's kept from drifting. `src/mouse.rs`
  (M10) has `warp_cursor_to` (`CGDisplay::warp_mouse_cursor_position`, for
  `mouse-follows-focus`) and `spawn_mouse_watcher` — another
  `CGEventTap`, this one `ListenOnly` on `kCGEventMouseMoved` for
  `focus-follows-monitor`, throttled to one position report per 80ms via a
  *thread-local* `Cell<Instant>` (not a shared `Mutex` — this callback
  only ever runs on its own dedicated OS thread, so there's nothing to
  synchronize) so mouse activity in general can't flood the daemon's
  `select!` loop with one message per pixel of travel.
- **`tili-config`** — KDL parsing/validation into a `Config` struct, plus
  file-watch hot-reload. `src/schema.rs` has the types and `parse()`,
  including `keybindings mode="..." { bind "key" "command" }` blocks (M6)
  and `floating-rules { rule app-id="..." title="regex"? { ... } ...
  defaults { ... } }` (M8) — `title` stays a plain `String` here, not a
  compiled `Regex`, so this crate doesn't need a regex dependency just to
  hold a pattern; `tili-daemon` compiles it. `workspace-rules { rule
  app-id="..." workspace="name" ... }` is a separate, independent section
  — both fields required, no `title`/sizing/`mode`, since it's a purely
  event-driven "which workspace does this app land on" rule with nothing
  to do with tile-vs-float — parsed by its own `parse_workspace_rules`,
  not folded into `parse_floating_rules`. Neither section validates
  `workspace` names here (this crate has no cross-section validation
  anywhere, and no error-reporting path for semantic issues, only KDL-
  syntax ones) — `tili-daemon` checks it names a declared workspace, the
  same way it already resolves `settings.default-workspace`. Unrecognized
  top-level
  sections are still silently ignored, not rejected, so a config can be
  written against the full target schema before the parser catches up —
  see README.md's config preview vs. `example/tili.kdl` for "aspirational
  full schema" vs. "what's actually parsed today." **KDL v2 booleans are
  `#true`/`#false`** (a `#`-prefixed keyword, to disambiguate from bare
  identifiers) — bare `true`/`false` is a parse error, easy to get wrong
  when writing test fixtures or example configs; there's a test guarding
  against forgetting this (`parses_settings_and_default_layout`).
  `src/watch.rs`'s `spawn_config_watcher` is deliberately synchronous
  (`std::sync::mpsc`, not tokio) so this crate stays runtime-agnostic —
  `tili-daemon` bridges it into its `tokio::select!` loop itself, the same
  pattern used for `tili-ax`'s NSWorkspace/AX event sources. It watches the
  config file's *containing directory*, not the file itself, since editors
  that save via temp-file-then-rename can otherwise orphan the watch on the
  old inode. A parse error during a reload is logged and dropped — the
  caller's previous `Config` keeps applying.
- **`tili-ipc`** — `Command`/`Response` types shared by the daemon and CLI,
  plus the socket path/framing convention. This is the only crate both
  `tili-daemon` and `tili-cli` depend on in common — protocol changes belong
  here, not duplicated in both binaries. `src/parse.rs`'s `parse(s: &str) ->
  Command` (M6) turns a keybinding's command string (`"focus left"`,
  `"mode resize"`) into a `Command` — infallible by design, an unrecognized
  string becomes `Command::Raw` rather than a parse error, so a config
  referencing a command ahead of its milestone (or with a typo) still loads
  and just fails at `dispatch()` time with "not implemented yet" instead of
  refusing to start the daemon.
- **`tili-daemon`** — the actual window manager process. `src/state.rs` holds
  `WmState`: the live `AxWindow` handles themselves (not just cached
  metadata — M3 needs the real `AXUIElement` to move/focus/park a window),
  one `tili_tree::Tree` **per workspace** (M4) for *tiled* windows, and a
  `placements: HashMap<WindowId, Placement>` index (M8 — `Placement` is
  just `{ workspace, floating }`) giving O(1) "which workspace owns this
  window, and is it tiled or floating" instead of scanning every
  workspace's tree (M4 through M7's approach). Floating windows (M8:
  matched a `floating-rules` entry at creation time, via
  `compute_floating_frame`, which checks `AxWindow::bundle_id()` against
  each compiled rule in order and computes a centered/sized `Rect` from the
  rule's or the config's `defaults`' width/height-ratio) live entirely
  outside any `Tree` — floating ones only get repositioned at creation and
  when their workspace becomes active again, not on every layout-affecting
  event, so a user's manual drag of a floating window isn't undone by, say,
  a gap change. `workspace_focus` remembers each workspace's last-focused
  node so switching back restores where you left off. A new window joins
  the active workspace next to the current focus (if tiled) or just gets
  centered (if floating) *unless* it matches a `workspace-rules` entry
  (`matching_workspace_rule`, checked via `AxWindow::bundle_id()` — kept
  entirely separate from `matching_floating_rule`, since which workspace a
  window lands on has nothing to do with whether it tiles or floats).
  `apply_windows_changed` resolves that into a `target_workspace` and
  hands off to `place_new_window`, which inserts into that workspace's own
  `Tree` (keying its focus-hint lookup off `target_workspace`, not
  whatever's active, so it still respects where focus was last left there)
  or, for a floating window, just records the `Placement` against it. If
  `target_workspace` isn't the one active on the focused monitor, the
  window is parked immediately and `resync_workspace_if_visible_elsewhere`
  (a fourth "thickness of relayout," alongside
  `relayout_active`/`relayout_monitor`/`relayout_all_visible` below)
  checks whether it's visible on some *other* monitor and, if so,
  relayouts/repositions it there right away instead of leaving it parked
  until an unrelated later switch — `move_focused_to_workspace` uses the
  same helper for the same reason.

  **M9 — multi-monitor.** `active_workspace: HashMap<u32, String>` maps
  each connected monitor's id (`tili_ax::Monitor::id`) to whichever
  workspace it's currently showing — a workspace absent from this map is
  parked, wherever it last was. `focused_monitor: u32` is which one
  `Focus`/`Move`/`WorkspaceSwitch`/layout commands actually target;
  `relayout_active`/`active_tree`/`active_tree_mut` all resolve through it
  (via `active_workspace_name()`), so most of the pre-M9 code didn't need
  to change — only `switch_workspace`, `apply_windows_changed`, and
  `move_focused_to_workspace` needed to become monitor-name-aware.
  `Command::FocusMonitor` (`focus_monitor_next`) is the *only* thing that
  changes `focused_monitor`; it cycles through `self.monitors`, no-op
  under two. `switch_workspace` swaps with whatever monitor is already
  showing the target workspace, if any — two monitors can never display
  the same workspace at once, since each has its own `Tree` layout
  computed against its own frame. `on_displays_changed` (called from
  `main.rs` on every `spawn_display_watcher` signal) is the hot-plug/
  unplug handler: a disconnected monitor's workspace gets parked and its
  slot dropped (same mechanics as switching away from it — nothing is
  lost, just no longer shown anywhere); a newly connected monitor gets a
  fresh empty `"monitor-<id>"` workspace; every still-visible workspace
  gets re-laid-out afterward since frames may have changed even for
  monitors that stayed connected. `relayout_active`/`relayout_monitor`/
  `relayout_all_visible` are three thicknesses of "recompute and apply
  frames" — most callers only need the focused monitor (`relayout_active`),
  but anything that could touch a workspace visible on a *different*
  monitor (app termination, config reload) uses `relayout_all_visible`.
  `park()` targets `tili_ax::parking_position` — a window's origin lands
  just a point inside the main monitor's own bottom-right corner (not
  pushed *outside* every monitor's bounds, `combined_bounds`'s original
  purpose): confirmed on real hardware that AppKit clamps a
  `kAXPositionAttribute` write requesting somewhere totally unreachable
  back to near a real screen's edge regardless of how far outside it's
  requested (it only constrains the origin, not the window's full frame).
  Keeping the origin legitimately on-screen and letting the window's own
  size extend past the corner (a technique other AX-based tiling WMs use
  too) avoids that clamp entirely instead of fighting it.
  Config-driven workspace-to-monitor pinning (`WorkspaceConfig.monitor`,
  parsed since M5) is intentionally still unwired — M9's bar is
  hot-plug/unplug safety, not that finer-grained UX.

  **M10 — mouse-follows-focus / focus-follows-monitor.**
  `mouse_follows_focus`/`focus_follows_monitor` are plain `bool`s set from
  `config.settings` in `apply_config` (previously parsed but never read
  anywhere, since M5). `raise_focused` is the single place that warps the
  cursor when `mouse_follows_focus` is on — every focus-changing path
  (`focus`, `move_focused`, `switch_workspace`'s restore step) already
  funnels through it, so this didn't need duplicating per call site.
  `on_mouse_moved(x, y)`, called from `main.rs` on every throttled
  position report from `tili_ax::spawn_mouse_watcher`, is a no-op unless
  `focus_follows_monitor` is on; when it is, a cheap point-in-rect check
  against the already-cached `self.monitors` (no AX/CG call on the hot
  path) updates `focused_monitor` if the cursor's now over a different
  connected monitor — same effect as an explicit `Command::FocusMonitor`.

  `focus`/`move_focused` are the only places that call `AxWindow::focus()`
  (real OS focus/raise); nothing calls it automatically on window creation,
  specifically to avoid focus-stealing every already-open window when the
  daemon starts up and gets seeded with the apps already running.
  `focus`/`move_focused` go through `tili_tree::Tree::focus_in_direction`
  (not plain `navigate`), and both now always call `relayout_active`
  afterward (M7) — cycling an Accordion's active child changes what's
  actually visible, so it's not just a focus-pointer update anymore the
  way plain `Split` navigation is. `toggle_layout`/`set_layout` (M7) wrap
  `Tree::toggle_layout` for `Command::LayoutToggle`/`LayoutSet`; `set_layout`
  is a no-op if the container's already the requested kind, since there
  are only two kinds and "set" is just "toggle away from the other one."
  `apply_config` updates `gaps`/`workspace_gaps` from a loaded or
  hot-reloaded `tili_config::Config`, creates any workspace it declares
  (without switching to it, so a reload never yanks focus off whatever's on
  screen), and rebuilds `mode_bindings` (M6: `HashMap<mode name,
  HashMap<KeyCombo, Command>>`) from `config.keybindings`.
  `Command::ModeEnter`/`ModeExit` switch `current_mode`;
  `resolve_hotkey(combo)` looks a press up in the current mode's table, and
  `active_key_combos()` returns just the keys (for syncing the `Mutex` the
  hotkey tap reads — see `tili-ax`'s `hotkey.rs`). `src/dispatch.rs` has
  the single `dispatch(&mut WmState, Command) -> Response` function — both
  the Unix-socket handler and the global-hotkey handler must call this
  same function, never a separate code path, or CLI-invoked and
  hotkey-invoked behavior can drift apart. `Command::Shutdown` is the one
  deliberate exception — it's process lifecycle, not a `WmState` mutation,
  so both `main.rs`'s socket-accept and hotkey `select!` arms check for it
  and `break` the loop directly instead of routing it through `dispatch()`
  (which would have nowhere to signal "please exit the process" from).
  `dispatch()` itself calls `WmState::sync_focus_from_frontmost()` before
  the command match — resolves which window real macOS currently considers
  focused (via `tili_ax::workspace::frontmost_app_pid`, an
  `AXUIElementCreateSystemWide`-based query) and updates `workspace_focus`
  synchronously, immediately before that command runs. This is deliberately
  not a reactive background sync triggered by an event arriving whenever —
  confirmed on real hardware that a background poll/notification updating
  focus asynchronously has an unavoidable race against the very next
  hotkey press, since there's no ordering guarantee between "the
  background sync noticed the click" and "the keypress got processed."
  Other AX-based tiling WMs resolve this the same way, synchronously at
  the top of every command — this is the fix for a long-reported "the
  first direction key press after switching windows manually does
  nothing/goes the wrong way" bug that several
  reactive-sync attempts (an AX per-window notification, then an
  `NSWorkspaceDidActivateApplicationNotification` subscription — confirmed
  to never fire for a process like this one with no `NSApplication`
  instance, unlike the process-lifecycle Launch/Terminate notifications,
  which don't depend on window-server UI-activation machinery — then a
  poll on `watch.rs`'s resync tick) all failed to fully close.
  `src/main.rs` is one
  `tokio::select!` loop merging socket accepts,
  `tili_ax::spawn_event_watcher()`'s channel, the config-reload bridge, the
  hotkey-tap bridge, the display-watcher bridge (M9), and the mouse-watcher
  bridge (M10) — no locks around `WmState` itself, because only one branch
  of the loop ever touches it at a time; `sync_active_combos` is called
  after every branch that could change the active mode/bindings, to keep
  the hotkey tap's `Mutex<HashSet<KeyCombo>>` from drifting out of sync
  with what `WmState` actually has bound. `ensure_starter_config_exists`
  (M10) writes `example/tili.kdl` (via `include_str!`) to
  `~/.config/tili/tili.kdl` before the first `tili_config::load` if
  nothing's there yet — best-effort, a write failure just falls back to
  `Config::default()` like before M10.
- **`tili-cli`** — thin socket client only (`ping`, `list-windows`,
  `focus <dir>`, `move <dir>`, `list-workspaces`, `workspace <name>`,
  `move-to-workspace <name>`, `layout <toggle|tiles|accordion>`,
  `focus-monitor`, `list-monitors`, `stop`, `status`). The
  package is named `tili-cli` but the binary itself is named `tili` (see
  the `[[bin]]` section in its
  `Cargo.toml`). No business logic belongs here — if you're tempted to add
  logic to the CLI, it probably belongs in `tili-daemon` behind a `Command`
  instead. `print_response` needs an `ExpectedPayload` hint per subcommand
  since `Response::OkWithPayload` carries an untyped `serde_json::Value` —
  add a new variant there (not JSON-shape sniffing) when a command gets a
  new payload type. Two exceptions to "no business logic here," both
  intercepted in `main()` before the socket-connecting code path (each
  `return`s instead of falling through to the generic `send()`/
  `print_response` path):
  - `tili start`/`stop` manage tili-daemon's LaunchAgent entirely on the
    local filesystem, never touching the daemon's socket. `start_daemon()`
    resolves `tili-daemon` relative to the running `tili` binary's own
    directory (`daemon_binary_path()`, via `std::env::current_exe()`, not
    `PATH` — a LaunchAgent's environment doesn't guarantee one), writes
    `~/Library/LaunchAgents/com.tili.daemon.plist` (`RunAtLoad` +
    `KeepAlive` both `true`), and `launchctl load -w`s it — this is the
    *only* way to run tili-daemon; there's no separate foreground mode.
    `stop_daemon()` is the reverse: `launchctl unload -w` then remove the
    plist. Unloading (not just killing the process) is load-bearing —
    `KeepAlive` only respawns the job while it stays loaded, so `tili stop`
    has to unload before the daemon can actually stay down.
  - `tili status` *does* talk to the socket (via `Command::Ping`) but gets
    its own wording instead of the generic "couldn't reach daemon" error
    path.
- **`tili-menubar`** — an `NSStatusItem` badge showing the focused
  monitor's active workspace, plus a dropdown to switch workspaces, open
  the config file, or quit. `src/badge.rs`'s `image_for` renders the
  badge as a solid rounded-pill `NSImage` with the text "knocked out"
  (`NSCompositingOperation::DestinationOut`) so the menu bar shows
  through in the shape of the letters, then marks it a template image so
  AppKit tints it correctly for light/dark/highlighted state — a leading
  filled-dot glyph is drawn at its own smaller font size with a computed
  baseline offset so it lines up with the workspace name's visual center
  instead of sitting low. `src/menu.rs`'s `MenuState` tracks the last-
  applied `(current workspace, workspace name list)` key so `apply_snapshot`
  only rebuilds the live `NSMenu` when that key actually changes (rebuilding
  unconditionally on every tick previously caused menu clicks to fire on
  their own with zero user interaction) — the badge title and visibility
  are still updated every tick regardless, since those are cheap. A
  daemon-unreachable poll result (`None`) hides the status item entirely
  rather than leaving a stale workspace name on screen. `src/ipc.rs`
  duplicates `tili-cli`'s own socket framing rather than sharing it (same
  precedent as `tili_ipc::default_socket_path`'s doc comment) and adds
  `wait_for_change`, sent as `Command::WaitForChange` — this blocks
  server-side (via `tili-daemon/src/main.rs`'s `change_notify:
  Arc<tokio::sync::Notify>`, spawned into its own task per connection so
  it doesn't block the main `select!` loop) until something actually
  changes or a 30s internal timeout fires, so `tili-menubar` learns about
  workspace/monitor changes the instant they happen instead of polling —
  `main.rs` runs this on a dedicated background thread in a loop, feeding
  results to the main thread over an `mpsc::channel` drained by a cheap
  50ms `NSTimer` tick (not itself a poll interval — the channel is
  normally empty; real work only happens right after `wait_for_change`
  unblocks). The one deliberate exception: a 1s backoff between reconnect
  attempts while the daemon is unreachable at all (not running, or
  between `tili stop`/`tili start`) — there's no notification to wait on
  when there's no connection to notify over. `src/actions.rs` handles
  menu clicks (`workspace:<name>` switches, `open-settings` opens the
  config via `$EDITOR`/`open`/`open -a TextEdit` in that fallback order,
  `quit` runs `tili stop` before exiting this process too) on its own
  background thread, since none of those reactions need the main thread.
  `tili-cli`'s `tili start`/`stop`/`uninstall` manage this binary's
  LaunchAgent alongside the daemon's own, so the badge's lifecycle never
  has to be driven separately — see `tili-cli`'s own entry below.
- **`xtask`** — release/signing tooling (M11). `bundle` wraps
  `tili-daemon`/`tili` in a minimal `tili.app` at
  `target/<target>/release/tili.app` (bundle id `com.tili.daemon` — the
  same id `tili-cli`'s LaunchAgent uses, M10). `codesign` signs it with
  hardened runtime + `xtask/entitlements.plist` (a bare `<dict/>` —
  **keep it free of XML comments**; `codesign`'s entitlements parser,
  `AMFIUnserializeXML`, is much stricter than a normal XML parser and
  rejects well-formed comments with an opaque "syntax error near line N").
  `package` runs `bundle`, then `codesign` only if `TILI_SIGN_IDENTITY` is
  set in the environment, then tars + sha256s — the single command
  `release.yml`'s `build` job calls per target. Certificate generation
  itself is deliberately *not* automated anywhere (see CONTRIBUTING.md's
  "Release Engineering" section) — it's a one-time, human, Keychain
  Access step, because the entire point of the self-signed-cert strategy
  is that the identity never changes; automating its creation would make
  it too easy to accidentally regenerate (which resets every user's
  Accessibility grant). `Formula/tili.rb` here is a copy of the real
  formula that lives in the separate `itsdezen/homebrew-tap` repo (not
  auto-published — see that file's own header comment for the sync
  process).

## Project status

tili shipped its first release (v0.1.0) with a complete, daily-drivable
feature set — see [ROADMAP.md](ROADMAP.md) for what's shipped and what's
planned next, and check that file before assuming a feature exists or
doesn't.

Key non-negotiable design invariants (from the architecture, not just style
preference):
- No private Accessibility/window APIs beyond the one documented
  `_AXUIElementGetWindow` call in `tili-ax/src/window.rs`.
- No polling — the daemon reacts to AXObserver/NSWorkspace/display
  notifications (`tili-ax`'s `watch.rs`/`workspace.rs`), it doesn't loop and
  check state. Two sanctioned, narrowly-scoped exceptions:
  `tili-ax/src/hotkey.rs`'s `spawn_hotkey_tap` retries installing the
  `CGEventTap` every few seconds for the process's whole lifetime, since
  Input Monitoring can be granted at any point after the daemon starts
  with no accompanying event to react to; and `tili-ax/src/watch.rs`'s
  window/app-watcher resync backstop — a cheap 250ms tick (attach/detach
  watchers, no relayout) plus a debounced-since-quiet full-window resync
  capped at 20s (`FULL_RESYNC_DEBOUNCE`/`FULL_RESYNC_MAX_INTERVAL`) — since
  `NSWorkspace` launch/terminate notifications and `AXObserver`
  window-level notifications have both been observed to occasionally
  never fire. Don't add a third polling loop without a similarly hard
  constraint forcing it. Scoped to `tili-daemon`'s own event loop
  specifically — `tili-cli`'s `wait_for_daemon_ready` (a short-lived
  foreground wait with clear exit conditions, watching a *separate*
  process finish starting) and `tili-menubar`'s reconnect backoff (only
  active while the daemon is genuinely unreachable, see its own module
  docs) are outside this invariant by construction, not exceptions to it.

  Accessibility permission deliberately has **no** in-process wait/poll of
  any kind, despite being a permission grant with no accompanying
  notification either — confirmed on real hardware, across three
  different mechanisms (plain sleep-based polling, a run-loop-serviced
  polling thread, and a stable non-ad-hoc signing identity), that an
  already-running process never reliably observes a grant made after it
  started; only a freshly launched process's own check reflects reality.
  `tili-daemon/src/main.rs` checks once at startup and, if not granted,
  unloads its own LaunchAgent (`stop_self`) and tells the user to run
  `tili start` again after granting it — no restart loop, no wait, no
  fourth polling exception. Don't reintroduce an in-process
  wait/retry/restart for this specific permission without new evidence
  that changes the above.
- All real window-frame mutations go through `WindowFrameSetter`, never a
  direct AX API call from daemon/tree code.
- Hotkey-triggered and socket-triggered commands both go through
  `dispatch()` — no parallel command-handling path. The hotkey tap's
  `active_bindings: Arc<Mutex<HashSet<KeyCombo>>>` (`tili-ax/src/hotkey.rs`)
  is the *one* sanctioned exception to "no locks, single owning loop" — a
  `CGEventTap` callback must decide synchronously whether to consume a
  keystroke and can't await a round-trip into `WmState`'s loop to find out.
  Don't add a second one without a similarly hard constraint forcing it.

## Release process

The project ships continuously — a new feature or fix set becomes a release
whenever it reaches a working, verifiable state, not on a fixed schedule.
To cut a release: update [CHANGELOG.md](CHANGELOG.md) (`Unreleased` → a
dated version section), tag `vX.Y.Z` per the plain pre-1.0 SemVer convention
documented there, and push the tag — `.github/workflows/release.yml`
re-runs the full gate, builds aarch64/x86_64 binaries, codesigns them, and
opens a **draft** GitHub release for manual review before publishing. Don't
hand-sign or ad-hoc-sign a release binary outside that pipeline (see the
Release Engineering section of the architecture notes for why ad-hoc
signing is specifically disallowed).
