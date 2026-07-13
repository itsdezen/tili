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
  `CGWindowID`) so it's fully unit-testable without macOS.
- **`tili-ax`** — the only crate allowed to touch the Accessibility API.
  Depends on `tili-tree` only for geometry types (`Rect`), never for the tree
  itself. `src/window.rs` owns the single private API call used anywhere in
  the codebase (`_AXUIElementGetWindow`, to resolve a window's real
  `CGWindowID`) — keep that call isolated there; don't add other private API
  usage without a strong reason, since staying public-API-only is what lets
  tili run without disabling SIP. `src/frame_setter.rs` defines the
  `WindowFrameSetter` trait — every place that moves/resizes a real window
  must go through `dyn WindowFrameSetter`, not call the AX API directly. v1
  only implements `InstantFrameSetter`; this trait is the seam a future
  animated setter plugs into without touching layout code. `src/workspace.rs`
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
  file-watch hot-reload. Schema types live in `src/lib.rs`.
- **`tili-ipc`** — `Command`/`Response` types shared by the daemon and CLI,
  plus the socket path/framing convention. This is the only crate both
  `tili-daemon` and `tili-cli` depend on in common — protocol changes belong
  here, not duplicated in both binaries.
- **`tili-daemon`** — the actual window manager process. `src/state.rs` holds
  `WmState`, including the live window cache kept current by reacting to
  `tili_ax::WmEvent`s rather than by re-scanning (see M2). `src/dispatch.rs`
  has the single `dispatch(&mut WmState, Command) -> Response` function —
  both the Unix-socket handler and the global-hotkey handler (a
  `CGEventTap`, not yet implemented) must call this same function, never a
  separate code path, or CLI-invoked and hotkey-invoked behavior can drift
  apart. `src/main.rs` is one `tokio::select!` loop merging socket accepts
  and `tili_ax::spawn_event_watcher()`'s channel (hotkeys/config-reload join
  this same select in later milestones) — no locks around `WmState`, because
  only one branch of the loop ever touches it at a time.
- **`tili-cli`** — thin socket client only. The package is named `tili-cli`
  but the binary itself is named `tili` (see the `[[bin]]` section in its
  `Cargo.toml`). No business logic belongs here — if you're tempted to add
  logic to the CLI, it probably belongs in `tili-daemon` behind a `Command`
  instead.
- **`xtask`** — release/signing tooling (codesign, eventually notarize,
  Homebrew bottle packaging). Not implemented yet.

## Project status and milestones

tili is built as a sequence of independently verifiable milestones (M0
through M11), tracked in [ROADMAP.md](ROADMAP.md) — check that file for
current status before assuming a feature exists. Code that's ahead of the
current milestone is marked with `TODO(M<n>): ...` comments (e.g.
`unimplemented!("set_frame: wired up in M3 (single-workspace tiling)")`) —
these are intentional scaffolding stubs, not bugs, and should stay
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
