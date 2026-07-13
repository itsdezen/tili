# tili

An i3-like tiling window manager for macOS, written in Rust. Uses only the
public macOS Accessibility API (plus one documented private call to resolve a
window's `CGWindowID`) — no need to disable System Integrity Protection.

Status: **M0 — scaffolding**. See `docs/plan.md`-equivalent design notes for
the full architecture and phased roadmap; not yet functional.

## Workspace layout

- `crates/tili-tree` — pure container-tree + layout algorithms (Tiles/BSP, Accordion)
- `crates/tili-ax` — Accessibility API integration
- `crates/tili-config` — KDL config parsing
- `crates/tili-ipc` — shared daemon/CLI protocol types
- `crates/tili-daemon` — the window manager daemon
- `crates/tili-cli` — the `tili` command-line client
- `xtask` — release/signing helper tooling

## Development

```sh
cargo build --workspace
cargo test --workspace
```

## License

MIT
