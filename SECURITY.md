# Security Policy

tili runs with macOS Accessibility permission, which gives it broad ability
to read and control windows across every app running on the machine. Please
treat vulnerability reports here with the seriousness that implies.

## Reporting a vulnerability

**Do not open a public issue for security vulnerabilities.**

Preferred: use GitHub's private vulnerability reporting
(Security tab → **Report a vulnerability**) on this repository, which opens
a private advisory visible only to maintainers until it's resolved.

If that's not available, email **hello@dezen.me** with details. Please
include:

- A description of the vulnerability and its potential impact
- Steps to reproduce (or a proof-of-concept)
- The tili version/commit and macOS version you tested against

We'll acknowledge reports as soon as possible and aim to keep you updated as
a fix is developed. Coordinated disclosure is appreciated — please give us a
reasonable window to ship a fix before any public write-up.

## Scope notes specific to tili's design

Some things that are **not** considered vulnerabilities in themselves,
because they're inherent to how any Accessibility-API-based window manager
works on macOS:

- tili being able to read window titles/positions of other apps — this is
  what the Accessibility permission grants by design.
- The daemon needing to run continuously to manage windows.

Things that **are** in scope and worth reporting:

- Any code path that uses a private/undocumented API beyond the single
  documented `_AXUIElementGetWindow` call in `crates/tili-ax/src/window.rs`
  (this would itself be a design-invariant violation, not just a security
  issue — see CONTRIBUTING.md).
- Anything that could let a local process influence tili's behavior over
  its Unix socket (`~/Library/Application Support/tili/tili.sock`) without
  appropriate access — e.g. missing permission checks on the socket file.
- Supply-chain concerns in the release pipeline (see the Release
  Engineering notes referenced from ROADMAP.md for the current codesigning
  approach and its known trade-offs).
