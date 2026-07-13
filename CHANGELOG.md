# Changelog

All notable changes to this project are documented here. Format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/).

**Versioning convention (pre-1.0):** `v0.<milestone>.<patch>` — the minor
version tracks the milestone number from [ROADMAP.md](ROADMAP.md) (e.g.
`v0.1.x` ships once M1 is done), patch bumps are fixes within a milestone
that don't add new milestone scope. This resets to standard SemVer at v1.0.

## [Unreleased]

## [0.1.0] - 2026-07-13

### Added

- M1: real window enumeration in `tili-ax` — finds on-screen windows' owner
  PIDs via public `CGWindowListCopyWindowInfo`, then reads each process's
  `AXWindows` through the public Accessibility API, resolving each window's
  real `CGWindowID` via the one private `_AXUIElementGetWindow` call.
  `tili-daemon` binds a real Unix socket and serves `Command::ListWindows`;
  `tili list-windows` is a real socket client. Verified end-to-end on real
  hardware: Accessibility permission granted, `tili list-windows` correctly
  lists real open windows.

### Notes

- Building `tili-daemon` requires full Xcode (not just Command Line Tools)
  — `axuielement`'s safe API links a Swift runtime bridge. See
  CONTRIBUTING.md.

## [0.0.0] - 2026-07-13

### Added

- Workspace scaffolding (M0): `tili-tree`, `tili-ax`, `tili-config`,
  `tili-ipc`, `tili-daemon`, `tili-cli`, `xtask` crates wired per the planned
  architecture. `cargo build --workspace` and `cargo test --workspace` pass.
- MIT license, community README, milestone roadmap, project dev context.
- CI: `fmt` + `clippy -D warnings` + `test` + `release build` gate on every
  push/PR to `main`.
