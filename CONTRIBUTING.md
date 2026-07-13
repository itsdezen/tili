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

## Release engineering

tili starts with a **self-signed certificate** rather than a paid Apple
Developer ID ($99/yr), to keep pre-v1.0 costs at zero. This works, but has
one sharp edge worth understanding before touching any of this: **TCC (the
Accessibility permission system) grants permission per signing identity.**
Regenerate the certificate — even for a legitimate reason like expiry — and
every user's Accessibility grant resets, forcing them to re-approve tili
after an update that otherwise changed nothing. This is *the* failure mode
this whole setup exists to avoid, and it's the reason a couple of things
below are phrased as "never" rather than "avoid."

**Never use ad-hoc signing (`codesign -s -`) for release artifacts.** It's
free too, but generates a new identity with no stable Team ID on *every*
build — meaning every single release would reset TCC permissions, which is
strictly worse than the self-signed-cert tradeoff below.

### One-time setup (a human does this, not CI, not an agent)

1. Generate one self-signed code-signing certificate with a fixed Common
   Name and the longest validity period `certtool`/Keychain Access allows
   (aim for 10+ years). Something like:
   ```sh
   security create-keychain -p "" tili-signing.keychain
   # Keychain Access.app > Certificate Assistant > Create a Certificate:
   #   Name: a fixed, memorable CN (e.g. "tili Self-Signed")
   #   Identity Type: Self Signed Root
   #   Certificate Type: Code Signing
   ```
2. Export it as a `.p12` (with a password) and store that file + password
   somewhere durable outside CI (password manager, encrypted backup) —
   this is the one artifact that must never be lost or regenerated except
   on forced expiry.
3. Base64-encode the `.p12` and add two **repository secrets** (Settings >
   Secrets and variables > Actions):
   - `TILI_SIGNING_CERTIFICATE_P12` — `base64 -i tili-signing.p12 | pbcopy`
   - `TILI_SIGNING_CERTIFICATE_PASSWORD` — the export password
4. That's it — `.github/workflows/release.yml`'s `build` job already checks
   for `TILI_SIGNING_CERTIFICATE_P12` and imports/signs automatically once
   it exists; no workflow changes needed. Until these secrets are added,
   releases ship unsigned (Gatekeeper-blocked, `xattr -d
   com.apple.quarantine tili.app` or right-click → Open to run) — see the
   warning in each draft release's notes.

### What CI actually does (once the secrets above exist)

`xtask` (`xtask/src/main.rs`) is the single place this logic lives —
release.yml just calls `cargo run -p xtask -- package --target <triple>
--version <ver>` per target:
1. `bundle` — wraps `tili-daemon`/`tili` in a minimal `tili.app` (gives
   Accessibility permission and codesigning a stable, nameable bundle
   identifier — `com.tili.daemon` — instead of a bare Unix executable).
2. `codesign` — hardened runtime + `xtask/entitlements.plist` (minimal;
   tili isn't sandboxed and needs no special entitlements), only if
   `TILI_SIGN_IDENTITY` is set in the environment.
3. tarball + sha256, matching what the Homebrew formula template
   (`Formula/tili.rb`) expects.

**Notarization is deliberately skipped for now** — accept first-launch
Gatekeeper friction rather than the added cost/complexity, and revisit once
a trigger condition is hit (see below).

**Triggers to upgrade to a paid Apple Developer ID + notarization later** —
worth it once *any* of:
- Gatekeeper friction is generating real "app won't open" reports from new
  users at meaningful install volume.
- The project wants `homebrew-core` listing (expects more verifiable
  signing than a self-signed cert).
- The self-signed certificate is nearing its expiry anyway — a natural
  moment to switch outright instead of minting another self-signed one.

### Homebrew tap

`Formula/tili.rb` in this repo is a **template** — Homebrew taps live in a
separate `<owner>/homebrew-tap` repository by convention, so publishing a
release means copying this file's current contents into that repo's
`Formula/tili.rb` with the real `sha256` values from the just-built
tarballs (printed by `xtask package`, or read from the `*.tar.gz.sha256`
files attached to the GitHub release) substituted in. That repository
doesn't exist as part of this codebase and isn't created automatically by
any tili tooling — creating it (and deciding whether to automate the copy
step above) is a separate, deliberate step outside this repo's scope.
