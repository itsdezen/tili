## What

<!-- What does this PR change, in one or two sentences? -->

## Which milestone

<!-- Link the ROADMAP.md milestone this belongs to, if any. If this is ahead
of the current milestone or crosses milestone boundaries, say so — see the
"Scope discipline" section of CONTRIBUTING.md. -->

## How to verify

<!-- The concrete steps a reviewer can take to check this works — a command
to run, a milestone's verification step, etc. "Trust me" isn't enough here. -->

## Checklist

- [ ] `cargo fmt --all --check` passes
- [ ] `cargo clippy --workspace --all-targets -- -D warnings` passes
- [ ] `cargo test --workspace` passes
- [ ] This doesn't introduce a private API call beyond the one documented in
      `crates/tili-ax/src/window.rs`, add polling, bypass `WindowFrameSetter`,
      or give hotkeys a separate code path from `dispatch()` (see
      CONTRIBUTING.md's design invariants) — or if it does, I've explained why
      above.
