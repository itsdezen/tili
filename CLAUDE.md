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

Before committing, run the exact gate CI enforces (a red PR blocks merge):

```sh
cargo fmt --all --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
```

If `cargo fmt --check` fails, run `cargo fmt` and re-stage. Clippy warnings
are hard errors (`-D warnings`); don't `#[allow]` one without a one-line
comment explaining why (see the `#[allow(dead_code)]` on `Tree` in
`tili-tree` for the pattern).

`tili-ax` (and anything depending on it) only builds on macOS — it links
against `AXUIElement`/Core Graphics/Core Foundation, and needs full Xcode
(not just CLT; see CONTRIBUTING.md). `tili-tree` has zero macOS
dependencies by design; prefer adding logic there over `tili-ax` when
possible so it stays testable without a Mac.

## Comments

A comment stays scoped to the function/logic it sits next to: why *this*
code is written the way it is, not the history of how it got there. Don't
reference other bugs, session narrative ("this was reverted because...",
"an earlier attempt tried..."), other tools/projects, or issue-tracker-style
context — that belongs in a commit message or PR description, not in the
source. If a comment needs a sentence about a rejected alternative, phrase
it as a property of *this* code ("not X, because Y" is fine; "we used to do
X but changed it after Z happened" is not).

## Architecture

This is a Cargo workspace; the crate split and one-way dependency
direction are hard boundaries. **The full per-crate design notes — with
the hardware-confirmed findings and history behind every rule below —
live in one file per crate under
[docs/architecture/](docs/ARCHITECTURE.md). Read the relevant crate's
file there before changing event flow, window classification, parking,
focus sync, polling/timing, multi-monitor handling, or release signing.**

- **`tili-tree`** — the container tree and layout algorithms (Tiles/BSP,
  Accordion). No `AXUIElement`, no CoreFoundation, no `unsafe`; operates
  on plain `Rect`/`WindowId` so it's fully unit-testable without macOS.
  Callers use `focus_in_direction` (Accordion-aware), not plain `navigate`.
- **`tili-ax`** — the only crate allowed to touch the Accessibility API;
  depends on `tili-tree` only for geometry types. `src/window.rs` owns the
  single private API call in the codebase (`_AXUIElementGetWindow`) plus
  window classification (`WindowKind`); `src/frame_setter.rs` defines
  `WindowFrameSetter` — the seam every real frame write goes through.
  Each OS event source (`workspace.rs`, `watch.rs`, `display.rs`,
  `hotkey.rs`, `mouse.rs`) runs its own dedicated `CFRunLoop` thread and
  only sends messages.
- **`tili-config`** — KDL parsing/validation into `Config`, plus
  file-watch hot-reload. Runtime-agnostic (`std::sync::mpsc`, not tokio);
  no cross-section semantic validation (that's `tili-daemon`'s job).
  **KDL v2 booleans are `#true`/`#false`** — bare `true`/`false` is a
  parse error; easy to get wrong in test fixtures and example configs.
- **`tili-ipc`** — `Command`/`Response` types shared by daemon and CLI,
  plus socket path/framing. Protocol changes belong here, never duplicated
  in both binaries. `parse.rs` is infallible by design — unknown command
  strings become `Command::Raw` and fail at `dispatch()` time, so a typo'd
  config still loads.
- **`tili-daemon`** — the window manager process. `state.rs` holds
  `WmState`: live `AxWindow` handles, one `Tree` per workspace for tiled
  windows, a `placements` index for O(1) window→workspace lookup, and
  floating windows outside any `Tree`. `dispatch.rs` has the single
  `dispatch(&mut WmState, Command) -> Response` both the socket and hotkey
  paths call (`Command::Shutdown` is the one documented exception — it's
  process lifecycle, handled in `main.rs`'s loop). `dispatch()` syncs
  focus from real macOS frontmost state synchronously before every
  command — deliberately not a reactive background sync (race-prone;
  see docs/architecture/tili-daemon.md). `main.rs` is one `tokio::select!` loop; no
  locks around `WmState`, only one branch touches it at a time.
- **`tili-cli`** — thin socket client; the binary is named `tili`. No
  business logic here — new behavior belongs in `tili-daemon` behind a
  `Command`. Two documented exceptions: `tili start`/`stop` (LaunchAgent
  management, filesystem-only) and `tili status`'s custom wording.
- **`tili-menubar`** — `NSStatusItem` workspace badge; stays in sync via a
  server-side long-poll (`Command::WaitForChange`), not polling. Its
  LaunchAgent is managed by `tili start`/`stop`/`uninstall` alongside the
  daemon's.
- **`xtask`** — release/signing tooling: `bundle`/`codesign`/`package`
  build a signed `tili.app`. `xtask/entitlements.plist` must stay free of
  XML comments (`codesign`'s parser rejects them). Certificate generation
  is deliberately manual — regenerating the cert resets every user's
  Accessibility grant (see CONTRIBUTING.md's Release engineering).

## Project status

tili has shipped (v0.1.x) with a complete, daily-drivable feature set —
see [ROADMAP.md](ROADMAP.md) for what's shipped and what's planned, and
check that file before assuming a feature exists or doesn't.
[docs/BLUEPRINT.md](docs/BLUEPRINT.md) holds the design reference for
planned-but-unshipped features.

## Design invariants

Non-negotiable, from the architecture rather than style preference. The
rationale (and real-hardware evidence) behind each is in
[docs/architecture/invariants.md](docs/architecture/invariants.md):

- **No private Accessibility/window APIs** beyond the one documented
  `_AXUIElementGetWindow` call in `tili-ax/src/window.rs` — this is what
  lets tili run without disabling SIP.
- **No polling** — the daemon reacts to AXObserver/NSWorkspace/display
  notifications. Exactly four sanctioned, narrowly-scoped exceptions:
  `hotkey.rs`'s event-tap install retry (Input Monitoring can be granted
  at any time, with no notification); `watch.rs`'s 250ms watcher-resync
  backstop + capped full resync (both notification sources have been
  observed to occasionally never fire); `display.rs`'s 1s bounded run-loop
  re-diff (resolution-only changes never fire the reconfiguration
  callback in this process); and `main.rs`'s 30ms `maintenance_tick`
  (pure debounce/coalescing of already-pushed events, near-zero idle
  cost). Don't add a fifth without a similarly hard constraint.
- **Accessibility permission gets no in-process wait/poll/restart of any
  kind** — an already-running process never reliably observes the grant
  (confirmed across three mechanisms). The daemon checks once at startup
  and stops itself if not granted. Don't reintroduce a wait without new
  evidence.
- **All real window-frame mutations go through `WindowFrameSetter`**,
  never a direct AX call from daemon/tree code — the future-animation
  seam.
- **Hotkey- and socket-triggered commands both go through `dispatch()`**
  — no parallel command path. The hotkey tap's `active_bindings:
  Arc<Mutex<HashSet<KeyCombo>>>` is the *one* sanctioned lock (the tap
  callback must decide synchronously); don't add a second.

## Keeping docs in sync

Every change that alters behavior described in this file,
`docs/architecture/*.md`, `docs/BLUEPRINT.md`, `ROADMAP.md`, or a code
comment pointing at them must update that doc **in the same change** —
these files are trusted as accurate over reading the code, so a stale
sentence is worse than a missing one. When you ship a planned feature,
move its entry from ROADMAP.md's planned list (and prune the covered
design from docs/BLUEPRINT.md) into the relevant
`docs/architecture/<crate>.md`.

## Release process

The project ships continuously — a feature/fix set becomes a release when
it reaches a working, verifiable state. To cut one: update
[CHANGELOG.md](CHANGELOG.md) (`Unreleased` → dated version section), bump
`[workspace.package] version` in `Cargo.toml` to match, tag `vX.Y.Z`, and
push the tag — `release.yml` re-runs the full gate, builds signed
aarch64/x86_64 binaries, and opens a **draft** GitHub release for manual
review. Never hand-sign or ad-hoc-sign a release binary outside that
pipeline (see CONTRIBUTING.md's Release engineering for why).
