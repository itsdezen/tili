# Architecture notes

The full technical rationale behind tili's design — per-crate detail,
hardware-confirmed findings, and the history behind each invariant — lives
in one file per crate under [architecture/](architecture/). Read the
relevant file before changing the area it covers; for the compact
rules-only summary, see [CLAUDE.md](../CLAUDE.md); for what's shipped vs.
planned, see [ROADMAP.md](../ROADMAP.md).

tili is a Cargo workspace, not a single crate. The split is deliberate and
the dependency direction is a hard boundary, not just organization.

| File | Covers |
| --- | --- |
| [architecture/tili-tree.md](architecture/tili-tree.md) | Container tree, Tiles/BSP + Accordion layout, insert/navigate semantics |
| [architecture/tili-ax.md](architecture/tili-ax.md) | Accessibility API layer: window classification, frame writes, display/workspace/hotkey/mouse watchers |
| [architecture/tili-config.md](architecture/tili-config.md) | KDL parsing (`#true`/`#false`!), rules sections, hot-reload watcher |
| [architecture/tili-ipc.md](architecture/tili-ipc.md) | Shared `Command`/`Response` protocol, infallible `parse.rs` |
| [architecture/tili-daemon.md](architecture/tili-daemon.md) | `WmState`, placements/floating, multi-monitor, parking, focus sync, `dispatch()`, the event loop |
| [architecture/tili-cli.md](architecture/tili-cli.md) | CLI surface, LaunchAgent start/stop, the two business-logic exceptions |
| [architecture/tili-menubar.md](architecture/tili-menubar.md) | `NSStatusItem` badge, long-poll sync via `WaitForChange` |
| [architecture/xtask-release.md](architecture/xtask-release.md) | Bundle/codesign/package, entitlements pitfalls, cert policy |
| [architecture/invariants.md](architecture/invariants.md) | Design invariants — full rationale and real-hardware evidence |
