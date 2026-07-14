# Tili — Development Blueprint

This is the technical foundation for a macOS tiling window manager. It preserves proven design decisions, real operating-system constraints, and the improvements best suited to a Rust implementation. It is an internal design document; the name and history of any predecessor are intentionally outside its scope.

## 1. Goals and boundaries

- Keyboard-driven, CLI-first, text-configured, and automation-friendly.
- i3-like n-ary tree tiling: independent workspaces, arbitrarily nested containers, windows as leaves.
- Virtual workspaces rather than native Spaces: instant switching, no mandatory animation, and no practical API-imposed workspace limit.
- Event-driven operation; no screen polling. Idle CPU use should be near zero.
- Prefer public Accessibility APIs; isolate the one necessary operation that maps an AX window to `CGWindowID`. Never require SIP to be disabled, injection, or private desktop APIs.
- Practical over decorative: gaps, callbacks, and an event stream are sufficient for bar integration; borders, animation, and ricing must not complicate the core.
- All pure logic—tree operations, layout, navigation, normalization, and parsing—must be independently unit-testable without macOS.

Do not promise perfect compatibility with Spaces, native fullscreen, or every broken AX application. AX writes are best-effort. The system must recover to a usable state instead of assuming macOS always responds correctly.

## 2. Target architecture

```text
CLI / hotkeys / callbacks / socket subscribers
                  │ request + target context
                  ▼
          single-owner WM state / transaction loop
       ┌──────────┼───────────┬───────────────┐
       ▼          ▼           ▼               ▼
  pure tree   workspace    config/shell    event broadcaster
  + layout    virtualization  + rules
       │          │
       └──────┬───┘ layout plan
              ▼
       WindowFrameSetter (instant now; animated later)
              ▼
     AX adapter: apps, windows, observers, displays, mouse
```

Dependencies must flow in one direction:

1. `tree`: IDs, rectangles, tree, layout, and navigation; no AX, async runtime, or `unsafe`.
2. `config` and `ipc`: schema/validation, command AST, and wire types; neither owns window-manager state.
3. `ax`: the only layer aware of macOS/AX/CG. It returns raw data and events and owns no tiling policy.
4. `daemon`: owns all mutable WM state and turns events/commands into transactions.
5. `cli`: parses user-facing input, connects to the socket, and prints stdout/stderr/exit code; it must not duplicate daemon logic.

There must be **one sequential state owner**. AX workers, file watchers, hotkey taps, and display callbacks send messages; none mutates the tree directly. This is the key barrier against races between a late AX notification, a move command, and a monitor change.

## 3. Data model and invariants

### Workspace tree

Each workspace has one root tiling tree plus window sets outside that tree:

- `Tiled`: leaves in the tree.
- `Floating`: belong to a workspace but are not routinely relaid out.
- `NativeFullscreen`, `Minimized`, `HiddenApplication`, `Popup/Dialog`: special state groups that must not be treated as ordinary tiled leaves.

A container has two independent properties:

- `orientation`: horizontal or vertical.
- `layout`: `tiles` or `accordion`.

Each child has a positive adaptive weight. A weight is meaningful only when its parent is a `tiles` container on the matching axis; never assign weights to floating or special nodes. Maintain a per-container MRU child: it both chooses the descent branch during navigation and selects the visible child in an accordion.

Invariants after **every mutation**:

1. A window is a leaf; a workspace never directly contains a window, only a root/special container.
2. Every parent-child relation is valid by type; no relation may remain temporarily invalid across an await point.
3. Remove empty containers (except a valid root); flatten one-child containers. A root may collapse to a leaf or remain a one-child root container, but the representation must be consistent.
4. When orientation normalization is enabled, directly nested containers must have opposite orientations. Changing one orientation propagates alternating orientations through ancestors to preserve the invariant.
5. Focus always resolves to `(workspace, Option<window>)`; an empty workspace is valid focus.
6. Every `WindowId` has exactly one O(1) placement index: workspace plus tiled/floating/special state. Never scan every tree to locate a window.
7. A workspace appears on at most one monitor; each monitor has exactly one active workspace.

Rust should use a persistent/immutable tree or a transaction builder that validates at commit. Avoid a mutable doubly linked object graph: parent pointers plus asynchronous mutation are a major source of dangling or inconsistent state. Use `NodeId`/an arena or a persistent single-linked tree; derive a parent map inside a transaction when required.

### Focus and MRU

Do not retain a mutable focus object that can become stale. Store a lightweight snapshot (`window_id?`, `workspace_name`, and monitor ID for events), then resolve it when needed:

- If its window closed or left the snapshot workspace, prefer the snapshot workspace.
- When focus leaves a workspace, update its old MRU state; when focus enters a window, update MRU through its ancestors.
- A target-aware command must not read global focus until target context has been resolved.

Required target precedence:

1. `--window-id`;
2. `--workspace`;
3. forwarded window context in the request/callback;
4. forwarded workspace context;
5. current focus (window, or an empty workspace).

This lets a multi-command callback continue operating on a newly created window even if its first command shifts focus elsewhere.

## 4. Layout and geometry

Every window-frame mutation must go through `WindowFrameSetter`. `InstantFrameSetter` is the current implementation; animation changes the implementation rather than tree or command code. Set position before size because some applications clamp size based on position. A failed AX write must not corrupt transaction state; log it with rate limiting and retry only during an appropriate refresh/event.

### Weighted tiles

For a container with `n` children, main-axis available length `L`, and weights `w_i`:

```text
delta = (L - Σw_i) / n
w'_i = w_i + delta
virtual_i = segment from w'_i, without gaps
physical_i = virtual_i minus symmetrically distributed inner gaps
```

Before layout, handle `n = 0`, non-finite/non-positive weights, and undersized areas; clamp to a useful minimum or rebalance evenly rather than create negative frames. Store `virtual_rect` (without inner gaps) for resize/drag, and `physical_rect` for hit testing, mouse operations, and focus-follow. Apply outer gaps at the root using the monitor’s usable frame.

Resize changes only valid siblings in the matching `tiles` orientation; normalize ratios and enforce a minimum pixel size. `balance-sizes` resets weights evenly within the requested container/subtree. When display size changes, distribute rounding error evenly so one child cannot absorb it all.

### Accordion

Children overlap; the MRU/active child occupies almost the whole area. Other children leave an `accordion_padding` reveal strip on both sides of the orientation axis to indicate ordering. Navigation along the accordion axis cycles and wraps siblings and changes the active child; always relayout after focus because visible frames change. Perpendicular navigation leaves the container through normal spatial rules.

`layout toggle/set` must retain the focused child as active after conversion to accordion. `set` must be idempotent and distinct from `toggle`.

### Fullscreen, floating, and drag

- Non-native fullscreen is layout state: the focused window occupies the visible frame (with or without outer gaps by configuration), while the sibling tree remains intact for restoration.
- Native fullscreen/minimized/hidden applications are never force-laid out; subscribe to state changes and reclassify when they return to normal.
- Floating windows must not be relaid out merely because the tree or gaps changed; doing so destroys manual dragging. Reposition only at rule application, workspace/monitor transfer, an explicit command, or parking restore.
- While a user is dragging/resizing with the mouse, never overwrite that window’s frame. Coalesce events and relayout on mouse-up. If managed resize/drag is supported, update weights/position from the virtual rectangle.
- Integrate floating windows into focus navigation through the smallest tiling container containing their center; do not require a separate focus-floating keybinding set.

## 5. Virtual workspaces and multiple monitors

An inactive workspace is parked just outside a screen corner instead of relying on Spaces. Switching lifecycle:

1. Choose the target monitor and validate pinning/assignment.
2. Unpark and lay out **workspaces becoming visible first** to reduce flicker.
3. Update the monitor ↔ workspace map and focus snapshot.
4. Park leaves of all now-invisible workspaces afterward.
5. Restore native focus to the target window when one exists; publish events/callbacks after the transaction.

Parking must use `combined_bounds` of **all** displays, not just the main display. Choose bottom-left or bottom-right by probing a few points near each corner and selecting the corner least covered by another display. macOS commonly refuses to place a window entirely outside a display; retaining a 1-pixel draggable recovery strip is a recovery feature, not a defect. Store floating position as a proportion of its visible rectangle and its size before parking; map that proportion to the new monitor and clamp on restore.

When the daemon disables, quits, or enters a controlled crash path, unpark all windows before exit. This is a first-class safety requirement.

### Monitor lifecycle

- Key active workspaces by stable display ID, never only by list index.
- On hot-plug/reconfiguration, re-enumerate fully; park the workspace of a disconnected monitor without deleting it; give a new monitor an empty/stub workspace; relayout every remaining visible workspace because frames may change.
- If display positions change, match old and new screen maps by frame-origin proximity; on collision retain the closer pairing. Never assume display enumeration is stable.
- Switching to a workspace already visible on another monitor swaps workspaces; never duplicate visibility.
- Workspaces are a shared pool, and each monitor displays one independently. Focused monitor is input context, not an inherent workspace property.
- Persistent/configured workspaces are never garbage-collected; empty temporary ones may be collected except when focused, visible, or assigned.

UX documentation must state that every display needs free space at a bottom corner for parking; “Displays have separate Spaces” often produces bad focus/performance behavior through public APIs. Native fullscreen on multiple monitors still carries unavoidable trade-offs.

## 6. Event loop, AX, and concurrency

Subscribe to AX lifecycle per application (created, destroyed, moved, resized), NSWorkspace launch/termination, display reconfiguration, hotkeys, and mouse movement. Give every source its own CFRunLoop thread when its framework requires one; callbacks only send messages.

AX events should be coarse-grained: `WindowsChanged { pid }`, followed by a process window rescan. Do not depend on the AX element carried by a destroyed event—it may already be invalid. Coalesce/debounce per PID to avoid bursts, and cancel stale heavy refreshes when a newer event arrives.

Do not subscribe to title changes by default: terminals and browsers can emit them constantly, causing rescan/layout backlogs. A cached title may be temporarily stale; refresh it on lifecycle events or explicit debugging/rule paths. Serialize AX reads/writes for the same app on a dedicated per-app executor/thread because the API may block. Refresh apps concurrently with cancellation, but return every state commit to the single owner loop.

Recommended refresh transaction:

```text
capture native focus → optional optimistic visible prelayout
→ enumerate/reconcile app/window model → garbage-collect dead windows/apps
→ apply command/event mutation → normalize tree → relayout visible workspaces
→ intentionally restore/sync OS focus → publish events/callbacks/UI
```

Heavy refresh is cancellable. A direct command/light session cancels heavy refresh to prioritize input. Cancellation must never leave half-committed state: build a plan first, commit state atomically, then apply best-effort side effects. Reconcile after both commands and refreshes because AX can change while awaiting.

Do not focus windows en masse at startup: initial scanning must not steal focus. Only explicit `focus`/`move`/workspace-restore paths may invoke native focus/raise.

## 7. Window classification and real-world exceptions

Classify using AX role/subrole, window level, and attributes before inserting into the tree:

- Standard window → tiled by default.
- Dialog → floating.
- Popup/menu/transient → special container; never run tiling callbacks.
- Native minimized/fullscreen/hidden app → special container; watch for reattachment.

Window detection must be idempotent. Insert a new tiled window next to the MRU/focused tiled leaf, or at root if the tree is empty. A title may not be initialized when the window appears, so title-regex rules can miss: prefer app-ID rules and make delayed title re-evaluation optional and explicit. Some applications use normal windows as popups; never assume the newest window is focused.

On destruction:

- remove registry/placement/tree state and cache closed-window identity before normalization;
- if the focused window dies in the current (or very recently focused) workspace, choose valid fallback focus and optionally native-focus it to repair macOS’s “active app with no window” behavior;
- never change focus because a popup/minimized window was destroyed;
- during lock screen or transient AX failure, do not garbage-collect prematurely; use a closed cache plus a defensive second pass to prevent flicker.

Application-specific compatibility must not leak into layout. Put every workaround behind a named reason and test it (for example, a special parking offset for an app that jumps offscreen on a one-pixel shift). Never silence a panic with `unwrap`: distinguish invalid target, config/user error, AX unavailable, cancellation, socket/protocol failure, and invariant violation. Only the last is a serious internal bug; user/AX failures need useful stderr and exit codes.

## 8. Commands, shell, IPC, and event stream

The CLI sends requests to a Unix-domain socket; the daemon parses commands again to preserve server-side authority. Handshake is versioned before the payload. Every response contains stdout, stderr, exit code, and server version/build ID. Requests must serialize context fields even as `null`; old clients that omit fields receive a compatibility warning.

The command surface should cover:

- focus/move/swap/join/split, resize, balance, flatten, layout, fullscreen, close;
- workspace switch/back-and-forth/summon, move node/workspace to monitor, focus monitor;
- list windows/workspaces/monitors/apps/modes/config; debug AX/window;
- mode, trigger binding, reload/enable; mouse/volume/native minimize/fullscreen;
- callback runner, exec-and-forget, and subscribe.

Every target-aware command should accept `--window-id`/`--workspace` when meaningful. Provide stable machine-readable output (documented JSON field ordering/schema) and script-friendly plain text. `list-*` must reflect a consistent model and must not silently trigger an AX scan unless explicitly documented.

The shell command language needs an AST, not string splitting. Precedence:

```text
pipe |  >  and &&  >  or ||  >  sequence ;
```

Pipeline buffers stdout between commands, forwards stderr, and returns the rightmost non-zero status like `pipefail`. `&&`/`||` short-circuit; `;` always runs. The parser must report quote/escape/parenthesis/unknown-command errors with locations. Callback configuration can forbid re-entry-prone constructs, while CLI `eval` may allow them explicitly.

`subscribe` keeps a socket open and broadcasts typed focus/workspace/monitor/window/mode/config events. A slow subscriber must not block the WM owner: use a bounded queue, coalesce state events, or disconnect lagging clients.

## 9. Configuration, hotkeys, rules, and callbacks

Configuration needs a schema version, path-aware parser diagnostics, migration/deprecation warnings, and explicit defaults. Reload is transactional:

1. watch the **directory** (editors that save by rename must not orphan the watch);
2. read/parse/validate/compile regexes and key combinations into a candidate configuration;
3. on syntax/severe failure, retain the previous configuration intact and report diagnostics;
4. when policy permits, drop only an invalid rule/regex and issue a warning;
5. atomically apply the configuration, update active hotkeys, and relayout affected visible workspaces.

Do not silently ignore unknown keys in production config: a forward-compatible namespace is acceptable, but warn so typos cannot disappear. KDL booleans are `#true`/`#false`; tests must guard this common mistake.

Keybindings are mode-based. The event tap synchronously decides consume/pass using a snapshot `Arc<Mutex<HashSet<KeyCombo>>>`; the daemon remains the authority that resolves combo → command in the current mode and updates the snapshot immediately after a mode/config change. Unbound keys pass through. Secure Input can disable hotkeys: expose a state/diagnostic rather than retrying in a loop.

Window rules/callbacks run in order. The first matching rule may stop processing; a `continue` flag enables layering (for example, float then move workspace). Conditions are command exit statuses, not a separate boolean parser. Callbacks run with dedicated window/workspace context; focus callbacks must be recursion-resistant so focus performed by a callback cannot retrigger indefinitely. Do not promise ordering between different callback types.

The execution environment supports opt-in/out inheritance, overrides, and `${VAR}` interpolation; default PATH must be sufficient for GUI processes. Do not invoke a shell implicitly for untrusted input. Workspace callbacks expose focused/previous workspace through environment variables, but the event stream is the preferred long-term integration API.

## 10. Commands with easy-to-break semantics

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
| `move-workspace-to-monitor` | Validate assignment/pinning and never duplicate workspace visibility. |
| `close` | Send AX close then reconcile; never assume a destroy notification is immediate. |
| native minimize/fullscreen | Change OS state, then move to/reconcile the special container; never force tile during transition. |

## 11. Improvement backlog for tili

1. Move `tree` to a persistent/single-linked representation or transaction-safe arena; audit every `await` so no mutable borrow/parent pointer survives it.
2. Complete command parity from the table above, JSON schemas/versioning, and `subscribe` event streaming.
3. Strengthen workspace virtualization: per-monitor parking-corner choice, unpark-on-exit/crash, proportional floating restoration, and monitor-arrangement diagnostics.
4. Make reconciliation robust: special containers, window-level/dialog heuristics, closed-window cache, lock-screen protection, and idempotent AX rescans per PID.
5. Add config v2: strict diagnostics and migration, ordered callback/rule AST, target-context propagation, and safe environment expansion.
6. Improve multi-monitor correctness: pinned workspace policy, swap semantics, hot-plug race tests, and a real usable display rectangle once coordinate conversion is thoroughly tested.
7. Add animation only after transaction/layout stability: implement `TweenedFrameSetter` with cancellation/coalescing and never let animation race AX events.
8. Add advanced features after the core: tabbed/stacked variants, sticky windows, per-app default workspace, and native-tab support only when its semantics are reliable.

## 12. Test matrix and definition of done

Test the pure crate for every mutation: insert/remove/flatten, nested orientations, MRU, directional navigation, accordion wrapping, weights/gaps/rounding, zero/one child, resize limits, and focus snapshot fallback. Add property/fuzz tests for parsing and random tree sequences so invariants always hold.

Fake-AX integration tests should cover: create/destroy bursts, invalidated AX handles, app quit while focused, a callback changing focus, config save-by-rename/bad reload, slow subscribers, protocol-version mismatch, and hotkey mode switching. The manual macOS matrix includes:

- single/multiple monitors, hot-plug/unplug, non-rectangular arrangements;
- Dock on every edge, notch/menu bar, mixed scale factors;
- native fullscreen/minimized/hidden app, lock screen, Secure Input;
- dialogs/popups, constantly changing titles, apps refusing resize/focus;
- rapid workspace switching, daemon disable/quit/crash recovery;
- idle CPU/RAM and latency with many applications/windows.

A change is complete only when invariant tests pass; it introduces no polling; errors carry contextual diagnostics; every frame write uses the setter; hot reload never discards the previous valid config; and no window can remain stranded offscreen after shutdown or recovery.
