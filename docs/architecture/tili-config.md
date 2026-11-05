# tili-config — KDL parsing and hot-reload

Part of the [architecture notes](../ARCHITECTURE.md).

`src/schema.rs` has the types and `parse()`, including
`keybindings mode="..." { bind "key" "command" }` blocks (M6) and
`floating-rules { rule app-id="..." title="regex"? { ... } ... defaults
{ ... } }` (M8) — `title` stays a plain `String` here, not a compiled
`Regex`, so this crate doesn't need a regex dependency just to hold a
pattern; `tili-daemon` compiles it.

Both rule sections also accept optional `role` and `subrole` attributes,
matched exactly against the window's AX role/subrole (for example
`subrole="AXDialog"` to float only an app's dialogs while its standard
windows keep tiling). `title` remains a regex only on `floating-rules`;
`workspace-rules` still has no sizing/`mode` field.

`workspace-rules { rule app-id="..." workspace="name" always=#bool? ... }`
is a separate, independent section — `app-id`/`workspace` required, no
`title`/sizing/`mode`, since it's a purely event-driven "which workspace
does this app land on" rule with nothing to do with tile-vs-float — parsed
by its own `parse_workspace_rules`, not folded into `parse_floating_rules`.
`always` is optional (defaults to `#false`): by default `tili-daemon` only
auto-routes an app's first window this way, not every window it opens —
`always` opts a rule back into the older, unconditional "route every new
window" behavior. Neither section validates `workspace` names here (this
crate has no cross-section validation anywhere, and no error-reporting
path for semantic issues, only KDL-syntax ones) — `tili-daemon` checks it
names a declared workspace, the same way it already resolves
`settings.default-workspace`.

The top-level `default-layout "..."` and a workspace's own
`layout="..."` attribute (`workspace "name" layout="accordion"`) are both
parsed as raw, unvalidated `String`s too — same reasoning as above.
`tili-daemon`'s `apply_config` resolves the two-level fallback (workspace
`layout` → top-level `default-layout` → `Tiles`) and validates against
`"tiles"`/`"accordion"`, warning (via `config_warnings`) rather than
silently discarding an unrecognized value.

Unrecognized top-level sections are still silently ignored, not rejected,
so a config can be written against the full target schema before the parser
catches up — see README.md's config preview vs. `example/tili.kdl` for
"aspirational full schema" vs. "what's actually parsed today."

`settings { ax-timeout-ms; native-tabs }` carries the per-app AX messaging
timeout (default 500ms, clamped by `tili-ax` to 50–5000) and whether the
native-tab guard is enabled (default `#true`). `tili-daemon` applies both;
this crate only parses them.

`session { enabled; state-path; restore-grace-ms; save-debounce-ms;
periodic-save-seconds }` is parsed into `SessionConfig`; every field is
optional and defaults to on, the platform state path, 2s grace, 3s
debounce, and 60s safety interval. `tili-daemon` owns the actual state
file/store and matching — this crate only carries the settings.

**KDL v2 booleans are `#true`/`#false`** (a `#`-prefixed keyword, to
disambiguate from bare identifiers) — bare `true`/`false` is a parse error,
easy to get wrong when writing test fixtures or example configs; there's a
test guarding against forgetting this
(`parses_settings_and_default_layout`).

`src/watch.rs`'s `spawn_config_watcher` is deliberately synchronous
(`std::sync::mpsc`, not tokio) so this crate stays runtime-agnostic —
`tili-daemon` bridges it into its `tokio::select!` loop itself via its own
relay thread (`spawn_config_reload_bridge`). This is now the only bridge of
that shape left: `tili-ax`'s own watchers build and send on a
`tokio::sync::mpsc` channel directly from their own thread instead, since
that crate already depends on Tokio (see [tili-daemon.md](tili-daemon.md)'s
main.rs section) — not an option here, since staying decoupled from any
particular async runtime is the whole point of this crate's own watcher.
It watches the
config file's *containing directory* (after resolving symlinks via
`canonicalize`, 0.1.4), not the file itself, since editors that save via
temp-file-then-rename can otherwise orphan the watch on the old inode. A
parse error during a reload is logged and dropped — the caller's previous
`Config` keeps applying.
