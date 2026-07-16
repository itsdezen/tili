# Development blueprint — planned features

Design reference for features tili hasn't shipped yet. What's shipped (and
the backlog order) lives in [ROADMAP.md](../ROADMAP.md); how the shipped
system actually works lives in [ARCHITECTURE.md](ARCHITECTURE.md). This
document preserves proven design decisions and real operating-system
constraints for the work still ahead, so a future implementation doesn't
have to rediscover them.

## Tree and layout semantics not yet implemented

### Adaptive weights

Each child of a `tiles` container has a positive adaptive weight. A weight
is meaningful only when its parent is a `tiles` container on the matching
axis; never assign weights to floating or special nodes. For a container
with `n` children, main-axis available length `L`, and weights `w_i`:

```text
delta = (L - Σw_i) / n
w'_i = w_i + delta
virtual_i = segment from w'_i, without gaps
physical_i = virtual_i minus symmetrically distributed inner gaps
```

Before layout, handle `n = 0`, non-finite/non-positive weights, and
undersized areas; clamp to a useful minimum or rebalance evenly rather than
create negative frames. Store `virtual_rect` (without inner gaps) for
resize/drag, and `physical_rect` for hit testing, mouse operations, and
focus-follow. Resize changes only valid siblings in the matching `tiles`
orientation; normalize ratios and enforce a minimum pixel size.
`balance-sizes` resets weights evenly within the requested
container/subtree. When display size changes, distribute rounding error
evenly so one child cannot absorb it all.

### Orientation normalization and tree representation

When orientation normalization is enabled, directly nested containers must
have opposite orientations; changing one orientation propagates alternating
orientations through ancestors. Flatten one-child containers and remove
empty ones (except a valid root) after every mutation.

Longer-term: move the tree to a persistent/single-linked representation or
a transaction-safe arena. Avoid a mutable doubly linked object graph —
parent pointers plus asynchronous mutation are a major source of dangling
or inconsistent state. Audit every `await` so no mutable borrow/parent
pointer survives it.

### MRU and target context

Maintain a per-container MRU child: it both chooses the descent branch
during navigation and selects the visible child in an accordion. Focus is a
lightweight snapshot (`window_id?`, `workspace_name`, monitor ID) resolved
when needed, never a mutable object that can go stale.

Target-aware commands should accept explicit target context with this
precedence:

1. `--window-id`;
2. `--workspace`;
3. forwarded window context in the request/callback;
4. forwarded workspace context;
5. current focus (window, or an empty workspace).

This lets a multi-command callback continue operating on a newly created
window even if its first command shifts focus elsewhere. A target-aware
command must not read global focus until target context has been resolved.

### Floating, drag, and fullscreen refinements

- While a user is dragging/resizing with the mouse, never overwrite that
  window's frame. Coalesce events and relayout on mouse-up. If managed
  resize/drag is supported, update weights/position from the virtual
  rectangle.
- Integrate floating windows into focus navigation through the smallest
  tiling container containing their center; don't require a separate
  focus-floating keybinding set.
- Non-native fullscreen is layout state: the focused window occupies the
  visible frame while the sibling tree remains intact for restoration.
  Native fullscreen/minimized/hidden apps are never force-laid out;
  subscribe to state changes and reclassify when they return to normal.
- Store a floating window's position as a proportion of its visible
  rectangle (plus its size) before parking; map that proportion to the new
  monitor and clamp on restore.

### Parking refinements

- Choose the parking corner per display by probing a few points near each
  corner and selecting the one least covered by another display, using
  `combined_bounds` of all displays.
- When the daemon disables, quits, or enters a controlled crash path,
  unpark all windows before exit — a first-class safety requirement.
- UX docs must state that every display needs free space at a bottom
  corner for parking; "Displays have separate Spaces" often produces bad
  focus/performance behavior through public APIs.

## Reconciliation robustness

- Never subscribe to title changes by default: terminals and browsers emit
  them constantly, causing rescan/layout backlogs. A cached title may be
  temporarily stale; refresh it on lifecycle events or explicit
  debugging/rule paths. Title-regex rules can miss because a title may not
  be initialized when the window appears — prefer app-ID rules and make
  delayed title re-evaluation optional and explicit.
- Serialize AX reads/writes for the same app on a dedicated per-app
  executor/thread because the API may block. Heavy refresh should be
  cancellable; a direct command cancels heavy refresh to prioritize input.
  Cancellation must never leave half-committed state: build a plan first,
  commit atomically, then apply best-effort side effects.
- On destruction: cache closed-window identity before normalization; if
  the focused window dies, choose valid fallback focus and optionally
  native-focus it to repair macOS's "active app with no window" behavior;
  never change focus because a popup/minimized window was destroyed.
  During lock screen or transient AX failure, don't garbage-collect
  prematurely; use a closed cache plus a defensive second pass.
- Application-specific compatibility must not leak into layout. Put every
  workaround behind a named reason and test it. Distinguish invalid
  target, config/user error, AX unavailable, cancellation, socket/protocol
  failure, and invariant violation — only the last is a serious internal
  bug.

## Commands, shell, IPC, and event stream

The daemon parses commands again server-side to preserve authority.
Handshake should be versioned before the payload; every response carries
stdout, stderr, exit code, and server version/build ID. Requests serialize
context fields even as `null`; old clients that omit fields get a
compatibility warning.

Target command surface beyond what's shipped:

- swap/join-with/split/flatten variants, `list-apps`/`list-modes`/
  `list-config`, debug AX/window, trigger binding, reload/enable,
  mouse/volume/native minimize, callback runner, exec-and-forget, and
  `subscribe`.
- Every target-aware command accepts `--window-id`/`--workspace` when
  meaningful. Provide stable machine-readable output (documented JSON
  schema) and script-friendly plain text. `list-*` must reflect a
  consistent model and never silently trigger an AX scan unless documented.

The shell command language needs an AST, not string splitting. Precedence:

```text
pipe |  >  and &&  >  or ||  >  sequence ;
```

Pipeline buffers stdout between commands, forwards stderr, and returns the
rightmost non-zero status like `pipefail`. `&&`/`||` short-circuit; `;`
always runs. The parser must report quote/escape/parenthesis/
unknown-command errors with locations. Callback configuration can forbid
re-entry-prone constructs, while CLI `eval` may allow them explicitly.

`subscribe` keeps a socket open and broadcasts typed
focus/workspace/monitor/window/mode/config events. A slow subscriber must
not block the WM owner: use a bounded queue, coalesce state events, or
disconnect lagging clients.

## Config v2, rules, and callbacks

- Schema version, path-aware parser diagnostics, migration/deprecation
  warnings. Don't silently ignore unknown keys: a forward-compatible
  namespace is acceptable, but warn so typos can't disappear. When policy
  permits, drop only an invalid rule/regex with a warning instead of
  rejecting the whole reload.
- A mode may be declared `auto-exit`, making it one-shot: dispatching any
  command bound while that mode is active automatically returns to the
  default mode, without a dedicated exit bind. Secure Input can disable
  hotkeys: expose a state/diagnostic rather than retrying in a loop.
- Window rules/callbacks run in order. The first matching rule may stop
  processing; a `continue` flag enables layering (e.g. float then move
  workspace). Conditions are command exit statuses, not a separate boolean
  parser. Callbacks run with dedicated window/workspace context; focus
  callbacks must be recursion-resistant. Don't promise ordering between
  different callback types.
- The execution environment supports opt-in/out inheritance, overrides,
  and `${VAR}` interpolation; default PATH must be sufficient for GUI
  processes. Don't invoke a shell implicitly for untrusted input.
  Workspace callbacks expose focused/previous workspace through
  environment variables, but the event stream is the preferred long-term
  integration API.

## Commands with easy-to-break semantics

| Area | Contract to preserve |
|---|---|
| `move` | Swap window/node identity with the directional neighbor; do not copy frames into a tree with different identity. |
| `join-with` | Group target and focus in a container of the proper orientation; preserve weight/MRU and normalize afterward. |
| `split` | Legacy/compatibility behavior; conflicts with flatten normalization, so warn or deprecate in favor of join. |
| `resize` | Resize only valid siblings on the requested axis; clamp minimums and error when no compatible sibling/container exists. |
| `layout floating/tiling` | Change placement atomically, preserving or deriving an insertion point; floating must retain manually chosen size. |
| `workspace` | Create dynamically only when policy permits; a declared-workspace policy should reject typos/undeclared names. Back-and-forth remembers the actual previous workspace. |
| `summon` | Bring a workspace to the focused monitor, swapping the currently visible workspace when necessary. |
| `move-node-to-workspace` | Keep target context stable; immediately park a hidden destination or relayout a visible one. |
| `move-workspace-to-monitor` | Validate assignment/pinning and never duplicate workspace visibility; an omitted workspace name targets whatever's currently active on the focused monitor. |
| `close` | Send AX close then reconcile; never assume a destroy notification is immediate. |
| native minimize/fullscreen | Change OS state, then move to/reconcile the special container; never force tile during transition. |

## Improvement backlog

1. Move `tree` to a persistent/single-linked representation or
   transaction-safe arena; audit every `await`.
2. Complete command parity from the table above, JSON schemas/versioning,
   and `subscribe` event streaming.
3. Strengthen workspace virtualization: per-monitor parking-corner choice,
   unpark-on-exit/crash, proportional floating restoration, and
   monitor-arrangement diagnostics.
4. Make reconciliation robust: special containers, window-level/dialog
   heuristics, closed-window cache, lock-screen protection, and idempotent
   AX rescans per PID.
5. Add config v2: strict diagnostics and migration, ordered callback/rule
   AST, target-context propagation, and safe environment expansion.
6. Improve multi-monitor correctness: pinned workspace policy, swap
   semantics, hot-plug race tests, and a real usable display rectangle
   once coordinate conversion is thoroughly tested.
7. Add animation only after transaction/layout stability: implement
   `TweenedFrameSetter` with cancellation/coalescing and never let
   animation race AX events.
8. Add advanced features after the core: tabbed/stacked variants, sticky
   windows, per-app default workspace, and native-tab support only when
   its semantics are reliable.

## Test matrix and definition of done

Test the pure crate for every mutation: insert/remove/flatten, nested
orientations, MRU, directional navigation, accordion wrapping,
weights/gaps/rounding, zero/one child, resize limits, and focus snapshot
fallback. Add property/fuzz tests for parsing and random tree sequences so
invariants always hold.

Fake-AX integration tests should cover: create/destroy bursts, invalidated
AX handles, app quit while focused, a callback changing focus, config
save-by-rename/bad reload, slow subscribers, protocol-version mismatch, and
hotkey mode switching. The manual macOS matrix includes:

- single/multiple monitors, hot-plug/unplug, non-rectangular arrangements;
- Dock on every edge, notch/menu bar, mixed scale factors;
- native fullscreen/minimized/hidden app, lock screen, Secure Input;
- dialogs/popups, constantly changing titles, apps refusing resize/focus;
- rapid workspace switching, daemon disable/quit/crash recovery;
- idle CPU/RAM and latency with many applications/windows.

A change is complete only when invariant tests pass; it introduces no
polling; errors carry contextual diagnostics; every frame write uses the
setter; hot reload never discards the previous valid config; and no window
can remain stranded offscreen after shutdown or recovery.
