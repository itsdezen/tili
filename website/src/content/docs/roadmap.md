---
title: Roadmap
description: What tili has shipped, what is planned next, and how the roadmap maps to releases and the changelog.
---

## Shipped

- **[v0.15.0](/docs/changelog#0150---2026-11-09)** — native-tab guard fixes
  for Terminal's tab-bar resize and the 2→1 close, new-window entry
  animation, configurable animation duration (120ms default), and immediate
  placement on window creation.
- **[v0.14.0](/docs/changelog#0140---2026-11-07)** — absolute floating sizes
  and the rounded menu bar default.
- **[v0.13.0](/docs/changelog#0130---2026-11-05)** — home-display workspace
  routing, global southeast parking, JSON query/subscribe, window metadata
  and rule ergonomics, native-tab guard, `toggle-floating`, and configurable
  AX timeout.
- **[v0.12.0](/docs/changelog#0120---2026-11-05)** — session state and
  recovery.
- Earlier releases — see the [Changelog](/docs/changelog).

## Planned

- **Tabbed/stacked containers.** A third container kind; design spike first.
- **Full native-tab support.** Only if the guard proves insufficient;
  depends on reliable AX tab semantics.
- **Sticky windows.** Stay visible across workspace switches.

Have a use case for one of these, or a feature that is missing? Open an
issue on [GitHub](https://github.com/itsdezen/tili/issues) rather than
assuming it is already planned.

## Design principles

- **Public API only.** One documented private call; no SIP disable, ever.
- **Event-driven, not polling.**
- **The animation seam stays a seam.**

Full detail lives in
[ROADMAP.md](https://github.com/itsdezen/tili/blob/main/ROADMAP.md) and
[docs/BLUEPRINT.md](https://github.com/itsdezen/tili/blob/main/docs/BLUEPRINT.md)
on GitHub.
