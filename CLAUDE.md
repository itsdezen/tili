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
  worth the risk for what M9 needs). `combined_bounds(&[Monitor])` is a
  pure, unit-tested helper giving the union of every connected monitor's
  bounds, used to keep parked windows outside of *all* real displays, not
  just main. `spawn_display_watcher()` registers a
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
  hold a pattern; `tili-daemon` compiles it. Unrecognized top-level
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
  node so switching back restores where you left off. New windows always
  join the active workspace next to the current focus (if tiled) or just
  get centered (if floating).

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
  `park()` targets `tili_ax::combined_bounds(&self.monitors)`, not just
  main's bounds, so a parked window can't land on a real second monitor.
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

## Project status and milestones

tili is built as a sequence of independently verifiable milestones (M0
through M11), tracked in [ROADMAP.md](ROADMAP.md) — check that file for
current status before assuming a feature exists. Code that's ahead of the
current milestone is marked with `TODO(M<n>): ...` comments and, where
there's nothing reasonable to do yet, `unimplemented!("...: wired up in
M<n>")` — these are intentional scaffolding stubs, not bugs, and should stay
unimplemented until their milestone comes up rather than being filled in
opportunistically out of order.

Key non-negotiable design invariants (from the architecture, not just style
preference):
- No private Accessibility/window APIs beyond the one documented
  `_AXUIElementGetWindow` call in `tili-ax/src/window.rs`.
- No polling — the daemon reacts to AXObserver/NSWorkspace/display
  notifications (`tili-ax`'s `watch.rs`/`workspace.rs`), it doesn't loop and
  check state. Two sanctioned, narrowly-scoped exceptions, both about
  macOS permission grants (there's no notification for "permission was
  just granted"): `tili-daemon/src/main.rs`'s startup sequence polls
  `tili_ax::has_accessibility_permission()` in a bounded loop (max 60s,
  once, before the daemon does anything else — gives up and stops itself
  rather than polling forever) while waiting for a first-time Accessibility
  grant; and `tili-ax/src/hotkey.rs`'s `spawn_hotkey_tap` retries installing
  the `CGEventTap` every few seconds for the process's whole lifetime,
  since Input Monitoring can be granted at any point after the daemon
  starts with no accompanying event to react to. Don't add a third
  polling loop without a similarly hard constraint forcing it.
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

Every milestone that reaches a working, verifiable state (per its ROADMAP.md
checkbox) is a release candidate — the project ships continuously rather
than batching everything up for v1. To cut a release: update
[CHANGELOG.md](CHANGELOG.md) (`Unreleased` → a dated version section), tag
`vX.Y.Z` following the versioning convention documented there, and push the
tag — `.github/workflows/release.yml` re-runs the full gate, builds
aarch64/x86_64 binaries, and opens a **draft** GitHub release for manual
review before publishing. Releases stay unsigned/prerelease until M11 lands
proper codesigning; don't hand-sign or ad-hoc-sign a release binary outside
that pipeline (see the Release Engineering section of the architecture
notes for why ad-hoc signing is specifically disallowed).
