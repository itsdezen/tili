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
  window-identity `swap_windows`, proportional `layout`). `insert_window`
  always wraps the target leaf in a fresh 2-child `Split` rather than
  flattening into an existing same-orientation split — a deliberate M3
  simplification (still a valid, correctly-tiling tree; just not the
  shallowest possible one). `layout(area, gaps)` takes a `Gaps` (outer
  padding around the whole area, inner spacing between siblings, both
  `f64` — `tili-config`'s parsed `u32` gaps get converted at the
  `tili-daemon` boundary since this crate can't depend on `tili-config`).
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
  plugs into without touching layout code. `src/display.rs` gets the main
  display's usable frame for layout — full `CGDisplay` bounds minus a
  hardcoded menu-bar inset; there's no `CGDisplay`-level equivalent of
  `NSScreen.visibleFrame`, so this is a known-approximate stand-in until M9
  brings in real `NSScreen`-based per-monitor frames. `src/workspace.rs`
  bridges `NSWorkspace` app-launch/quit notifications via `objc2`/
  `objc2-app-kit` — note it spawns its own dedicated `CFRunLoop` thread,
  since a process without `NSApplication` needs *some* thread pumping a run
  loop to receive Cocoa notifications at all (same reason `axuielement`'s own
  `AXNotificationStream` does the same for AX notifications). `src/watch.rs`
  ties both together into `spawn_event_watcher()`, which subscribes each
  running app to window lifecycle notifications and emits a single coarse
  `WmEvent::WindowsChanged { pid }` per change — callers re-read that
  process's windows via `list_windows_for_pid` rather than trying to
  interpret individual notification payloads (this sidesteps having to
  reason about whether a specific `AXUIElement` is still valid to query at
  the exact moment its destroyed-notification fires).
- **`tili-config`** — KDL parsing/validation into a `Config` struct, plus
  file-watch hot-reload. `src/schema.rs` has the types and `parse()`;
  unrecognized top-level sections (e.g. `keybindings`/`floating-rules` ahead
  of M6/M8) are silently ignored, not rejected, so a config can be written
  against the full target schema before the parser catches up — see
  README.md's config preview vs. `example/tili.kdl` for "aspirational
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
  here, not duplicated in both binaries.
- **`tili-daemon`** — the actual window manager process. `src/state.rs` holds
  `WmState`: the live `AxWindow` handles themselves (not just cached
  metadata — M3 needs the real `AXUIElement` to move/focus/park a window),
  one `tili_tree::Tree` **per workspace** (M4), and a `workspace_focus`
  map remembering each workspace's last-focused node so switching back
  restores where you left off. Only the *active* workspace's tree ever gets
  laid out on real screen coordinates (`relayout_active`); every other
  workspace's windows sit wherever `switch_workspace` last parked them
  (off-screen, since macOS has no public Spaces API to actually hide them).
  New windows always join the active workspace next to the current focus.
  `focus`/`move_focused` are the only places that call `AxWindow::focus()`
  (real OS focus/raise); nothing calls it automatically on window creation,
  specifically to avoid focus-stealing every already-open window when the
  daemon starts up and gets seeded with the apps already running.
  `apply_config` (M5) updates `gaps`/`workspace_gaps` from a loaded or
  hot-reloaded `tili_config::Config` and creates any workspace it declares
  — without switching to it, so a reload never yanks focus off whatever's
  on screen. `src/dispatch.rs` has the single `dispatch(&mut WmState,
  Command) -> Response` function — both the Unix-socket handler and the
  global-hotkey handler (a `CGEventTap`, not yet implemented) must call
  this same function, never a separate code path, or CLI-invoked and
  hotkey-invoked behavior can drift apart. `src/main.rs` is one
  `tokio::select!` loop merging socket accepts,
  `tili_ax::spawn_event_watcher()`'s channel, and the config-reload bridge
  (hotkeys join this same select in a later milestone) — no locks around
  `WmState`, because only one branch of the loop ever touches it at a time.
- **`tili-cli`** — thin socket client only (`ping`, `list-windows`,
  `focus <dir>`, `move <dir>`, `list-workspaces`, `workspace <name>`,
  `move-to-workspace <name>`). The package is named `tili-cli` but the
  binary itself is named `tili` (see the `[[bin]]` section in its
  `Cargo.toml`). No business logic belongs here — if you're tempted to add
  logic to the CLI, it probably belongs in `tili-daemon` behind a `Command`
  instead. `print_response` needs an `ExpectedPayload` hint per subcommand
  since `Response::OkWithPayload` carries an untyped `serde_json::Value` —
  add a new variant there (not JSON-shape sniffing) when a command gets a
  new payload type.
- **`xtask`** — release/signing tooling (codesign, eventually notarize,
  Homebrew bottle packaging). Not implemented yet.

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
  check state.
- All real window-frame mutations go through `WindowFrameSetter`, never a
  direct AX API call from daemon/tree code.
- Hotkey-triggered and socket-triggered commands both go through
  `dispatch()` — no parallel command-handling path.

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
