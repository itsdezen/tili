# Contributing to tili

tili is early enough that architectural feedback is as valuable as code.
Before writing a large patch, consider opening a Discussion or Issue first —
milestones (see [ROADMAP.md](ROADMAP.md)) are scoped to be independently
pickup-able, and it's worth confirming an approach before investing time in
one that conflicts with a design invariant.

## Development setup

```sh
git clone https://github.com/itsdezen/tili
cd tili
cargo build --workspace
cargo test --workspace
```

`tili-ax` (and anything depending on it, i.e. `tili-daemon`) needs **full
Xcode installed, not just Command Line Tools** — `axuielement`'s safe API
links against a Swift runtime bridge, and the Swift compatibility shims it
needs (`swiftCompatibility56` etc.) only ship with Xcode.app. With CLT
alone, `cargo build -p tili-daemon` fails at the final link step with
undefined `__swift_FORCE_LOAD_*` symbols — everything up to and including
`cargo clippy --workspace` still works without Xcode (linking a binary and
type-checking one are different steps), so that failure specifically means
"install Xcode," not "something's broken."

If you've already installed full Xcode and still hit undefined
`__swift_FORCE_LOAD_$_swiftCompatibility56` (etc.) symbols, rustc is likely
falling back to a stale `/Library/Developer/CommandLineTools/.../swift-5.5/`
search path instead of Xcode's own toolchain. Point the linker at the real
one directly:

```sh
export RUSTFLAGS="-L $(xcode-select -p)/Toolchains/XcodeDefault.xctoolchain/usr/lib/swift/macosx"
cargo build -p tili-daemon
```

`tili-tree` has no macOS dependencies and is the easiest crate to
contribute to without a Mac.

## Before opening a PR

Run the exact gate CI enforces — a red check blocks merge, so it's faster to
catch it locally:

```sh
cargo fmt --all --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
```

If `cargo fmt --all --check` fails, run `cargo fmt` (without `--check`) and
re-stage. Clippy warnings are hard errors (`-D warnings`) — if you need an
`#[allow(...)]`, add a one-line comment explaining why (see
`#[allow(dead_code)]` on `Tree` in `tili-tree` for the pattern: intentional
scaffolding pending a specific milestone, not a shrug).

## Design invariants

These aren't style preferences — they're load-bearing for the project's
goals (see the README for the "why"). PRs that violate them will need a
strong justification:

- **No private Accessibility/window APIs** beyond the one documented
  `_AXUIElementGetWindow` call in `crates/tili-ax/src/window.rs`. This is
  what lets tili run without disabling System Integrity Protection.
- **No polling.** The daemon reacts to AXObserver/NSWorkspace/display
  notifications; it doesn't loop and check state.
- **All window-frame mutations go through `WindowFrameSetter`**
  (`crates/tili-ax/src/frame_setter.rs`), never a direct AX API call from
  daemon/tree code — this is the seam future animation support plugs into.
- **Hotkey-triggered and socket-triggered commands both go through
  `dispatch()`** in `crates/tili-daemon/src/dispatch.rs` — no parallel
  command-handling path.

## Scope discipline

Code that's ahead of the current milestone is marked with `TODO(M<n>): ...`
comments and often `unimplemented!(...)`. These are intentional — please
don't fill one in opportunistically out of order; open an issue/discussion
first if you think a milestone should be reordered.

## Commit style

Commit messages use an emoji prefix indicating the kind of change:
🚀 feature · 🐞 fix · 🔧 tooling/config · ♻️ refactor · 📝 docs · 🗑️ removal ·
⬆️ dependency bump.
