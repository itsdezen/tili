<div align="center">

# tili

**A tiling window manager for macOS, built for speed.**

i3-style workflow · public Accessibility API only · Rust · no SIP disable

[![CI](https://github.com/itsdezen/tili/actions/workflows/ci.yml/badge.svg)](https://github.com/itsdezen/tili/actions/workflows/ci.yml)
[![License: MIT](https://img.shields.io/badge/license-MIT-blue.svg)](LICENSE)
[![Rust](https://img.shields.io/badge/rust-2024-orange.svg)](https://www.rust-lang.org)
[![Status](https://img.shields.io/badge/status-early%20development-yellow.svg)](ROADMAP.md)

[Roadmap](ROADMAP.md) · [Contributing](#contributing) · [Architecture](#architecture)

</div>

---

## Why tili

Most tiling window managers on macOS force a trade-off: either they poll the
screen and burn CPU doing it, or they reach for private, undocumented APIs
that break every time Apple ships a system update. tili takes neither
shortcut.

- **Event-driven, not polling.** The daemon subscribes to Accessibility and
  workspace notifications and reacts to them — it does no work while your
  windows aren't changing. That's the whole idea behind lower idle CPU/RAM
  than the alternatives.
- **Public API only.** Exactly one narrowly-scoped private call is used
  anywhere in the codebase (to resolve a window's real `CGWindowID`) —
  everything else is the public Accessibility API. You never disable System
  Integrity Protection to run tili.
- **A config format that matches how you think.** [KDL](https://kdl.dev)
  instead of flat TOML tables — workspaces, keybinding modes, and floating
  rules nest the way they actually relate to each other.
- **Animation-ready architecture, not animation-as-an-afterthought.** Every
  window-frame mutation goes through a single seam (`WindowFrameSetter`).
  v1 ships instant moves; smooth animated transitions plug into that same
  seam later without a rewrite.
- **No hotkey daemon required.** tili owns its own global hotkeys. One binary,
  one daemon, one config file.
- **Open source, installed via Homebrew.** No App Store, no sandbox
  restrictions on what a window manager needs to do.

## Status

tili is in **early development** (see [ROADMAP.md](ROADMAP.md) for the full
milestone breakdown). It is not yet daily-drivable. Star/watch the repo if
you want to follow along — contributions and design feedback are welcome
well before v1.

## Preview: config

Configuration is [KDL](https://kdl.dev), read from `~/.config/tili/tili.kdl`
and hot-reloaded on save — no restart needed. `workspaces`, `gaps` (global
and per-workspace), and `settings.auto-reload` are parsed and applied today;
`keybindings`/`floating-rules` are part of the target schema shown below but
not parsed yet (M6/M8) — unrecognized sections are ignored rather than
rejected, so it's safe to write the full schema ahead of time. See
[`example/tili.kdl`](example/tili.kdl) for a copy-pasteable starting point
that reflects what's actually functional right now.

This is a trimmed example of what a full, eventual setup looks like:

```kdl
workspaces {
    workspace "work" monitor="main"
    workspace "entertain" monitor="main"
    workspace "random"
}

default-layout "tiles"

gaps {
    inner 4
    outer 8 8 8 8
}

keybindings mode="main" {
    bind "alt-h" "focus left"
    bind "alt-shift-h" "move left"
    bind "alt-w" "workspace work"
    bind "alt-slash" "layout toggle"
}

floating-rules {
    rule app-id="com.apple.finder"
    rule app-id="com.apple.systempreferences" { width 900; height 600; center #true }

    defaults { center #true; width-ratio 0.6; height-ratio 0.6 }
}
```

## Architecture

tili is a Cargo workspace split along strict boundaries so the hardest parts
(the container tree, the layout algorithms) can be tested without a Mac at
all, and the parts that touch the OS (`AXUIElement`, `NSWorkspace`, display
reconfiguration) stay isolated in one place.

```
crates/
├── tili-tree     pure container-tree + layout algorithms (Tiles/BSP, Accordion)
├── tili-ax        Accessibility API integration, the WindowFrameSetter seam
├── tili-config    KDL parsing + validation, hot-reload
├── tili-ipc        shared daemon/CLI protocol types
├── tili-daemon     the window manager: event loop, state, hotkeys
└── tili-cli        the `tili` command-line client
```

The daemon is single-threaded around one piece of state: every command,
whether it comes from a global hotkey or the CLI over a Unix socket, flows
through the same `dispatch(&mut WmState, Command) -> Response` function. No
locks, no drift between "what the hotkey does" and "what the CLI does."

Workspaces are virtual — macOS exposes no public API to control Spaces, so
inactive-workspace windows are parked off-screen rather than relying on real
Spaces, the same technique other public-API-only tools in this space use.

## Installation

Not yet published. Once M11 (release engineering) lands, installation will
be:

```sh
brew install tili/tap/tili
```

Until then, build from source:

```sh
git clone https://github.com/itsdezen/tili
cd tili
cargo build --release --workspace
```

## Contributing

tili is early enough that architectural feedback is as valuable as code.
Check [ROADMAP.md](ROADMAP.md) for what's next — milestones are scoped to be
independently pickup-able. See [CONTRIBUTING.md](CONTRIBUTING.md) for dev
setup, the pre-PR test gate, and the design invariants PRs are expected to
respect. Bug reports and feature requests use the issue templates; general
questions go in [Discussions](https://github.com/itsdezen/tili/discussions).

Releases are cut continuously as milestones land — see
[`.github/workflows/release.yml`](.github/workflows/release.yml) and
[CHANGELOG.md](CHANGELOG.md) for the process and versioning convention.

## Security

Found a vulnerability? See [SECURITY.md](SECURITY.md) — please don't open a
public issue for security reports.

## Code of Conduct

This project follows the [Contributor Covenant](CODE_OF_CONDUCT.md).

## License

[MIT](LICENSE)
