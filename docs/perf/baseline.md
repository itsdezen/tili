# Performance baseline

Scope 0 of the perf program: the measurement harness and a first baseline.
This change adds no product behavior — everything here observes the existing
0.11.1 code.

**Status: preliminary.** The process readings below were supplied by the
owner from the installed 0.11.1 build, not produced by the script yet; they
are replaced once the owner runs `scripts/perf/collect.sh` on a quiet
machine. The layout numbers come from the harness in this worktree and are
recorded so later scopes have a fixed comparison point.

| Field | Value |
| --- | --- |
| Machine | Apple M4 (Mac mini), 16 GB |
| macOS | 27.0.1 (26A434) |
| Commit | `0021af0` (base main; harness changes uncommitted) |
| tili | 0.11.1 (installed Homebrew build) |
| Toolchain | rustc 1.99.1, criterion 0.8.2 |

## How to reproduce

Read-only process sample (safe on a machine in use; takes 60 s):

```sh
scripts/perf/collect.sh
```

Longer window, plus a JSON copy:

```sh
scripts/perf/collect.sh --seconds 120 --json /tmp/tili-perf.json
```

Command round-trip. Read-only — `ping` never moves windows, so this never
prompts:

```sh
scripts/perf/collect.sh --commands 100
scripts/perf/collect.sh --commands 100 --json /tmp/tili-perf-commands.json
```

One sample is the full wall-clock round trip of `tili ping`: CLI process
startup, Unix-socket connect, daemon dispatch, and the CLI reading its reply.
The same run reports `daemon.err.log` growth over the whole phase; the table's
last column is that total divided by the command count. A run against the
installed daemon and the harness's own build differ in exactly the logging
cost this mode exists to expose, so record which binary `TILI_BIN` pointed at.

Example output (installed 0.11.1 daemon; the diagnostic lines still on at that
version are the "before" side of the gating change):

```markdown
| commands | p50 (ms) | p95 (ms) | daemon.err.log growth (B) | per command (B) |
| ---: | ---: | ---: | ---: | ---: |
| 100 | 6.182 | 7.686 | 4280 | 42.8 |
```

Workspace-switch round-trip. **This moves real windows and workspaces**;
it prints a warning and needs an interactive `yes`, or
`TILI_PERF_CONFIRM=1` for a scripted run, then restores the previously
active workspace:

```sh
scripts/perf/collect.sh --switch 20
TILI_PERF_CONFIRM=1 scripts/perf/collect.sh --switch 20 --json /tmp/tili-perf-switch.json
```

The CLI subcommand is `tili workspace <name>` (there is no `tili switch`);
the script cycles through every declared workspace other than the active
one.

Pure-tree layout bench (no macOS APIs, no running daemon required):

```sh
cargo bench -p tili-tree --bench layout

# Save this scope's numbers, then compare a later scope against them:
cargo bench -p tili-tree --bench layout -- --save-baseline scope0
cargo bench -p tili-tree --bench layout -- --baseline scope0
```

Automated verification for a scope — the CI gate plus the bench, with a
regression threshold. Baselines are cached outside the repo
(`~/.cache/tili-perf/criterion`) because `target/` is per-worktree:

```sh
scripts/perf/verify.sh --save scope0      # gate + bench, cache baseline
scripts/perf/verify.sh --compare scope0   # gate + bench, exit 1 on >10% regression
scripts/perf/verify.sh --compare scope0 --report docs/perf/runs/s1.md
```

## What each metric means

### Layout bench (criterion)

`crates/tili-tree/benches/layout.rs` builds fixtures through the crate's
public API and times one call on a fixed area (1512×945), inner gap 8,
outer gap 8, solo gap 12, accordion peek 24. Fixtures have the flat-root
shape repeated `insert_window` calls produce (windows as siblings in the
root container), with one resized leaf so layout does non-uniform weight
division. `ns/window` is `time ÷ N` and is the figure to compare across N;
raw `ns` is what criterion reports in its confidence interval.

- `layout/tiles/{1,2,4,8,16,32,64}` — one `Tree::layout` on a Tiles root.
- `layout/accordion/{1,2,4,8,16,32,64}` — same tree shape with an
  Accordion root (peek padding path).
- `insert/{8,32,64}` — one `insert_window` beside the MRU leaf of a fresh
  N-window tree, i.e. the daemon's normal new-window placement path. Fresh
  per batch so the fixture never grows.
- `remove/{8,32,64}` — one `remove_window` of the middle window (forces the
  MRU/gap fix-up rather than removing the last child).
- `focus/right/32` — one `focus_in_direction` on a 32-window tree,
  including its `record_focus` walk.
- `balance/root/32` — one root `balance_weights` starting from a skewed
  tree.

Criterion config: 300 ms warm-up, 1 s measurement, 50 samples per id. The
whole run is about 33 s once dependencies are compiled; a first-ever run
also compiles criterion and the release bench.

These benches cover only the pure algorithms. They say nothing about AX
frame writes, daemon dispatch, event latency, or animation tween steps.

### RSS

`ps -o rss=` for each process, in KiB, divided by 1024 and shown as MB. It
is a single snapshot at the end of the sampling window, so it reflects the
process's steady state but not allocator churn during the window. It
includes shared library pages the process has touched.

### Threads

`ps -M <pid> | wc -l`, minus its one header row: the kernel-visible thread
count, including the tokio worker pool and the dedicated CFRunLoop threads
for event taps/displays. It is a snapshot, and it can differ by a thread or
two between launches of the same build — compare medians or ranges, not one
sample.

### CPU-time delta and average CPU %

`ps -o time=` is cumulative CPU time (`[[dd-]hh:]mm:ss[.ss]`). The script
subtracts the value at the start of the window from the value at the end,
and divides by elapsed wall time to get average CPU % (100% = one fully
busy core, all threads summed). macOS reports this with 10 ms resolution,
so a 60 s window quantises to ~0.017 percentage points; a reading of
`0.1%` is within one tick of idle and should not be over-interpreted.

### Command round-trip (p50 / p95)

One sample is the full wall-clock round trip of `tili ping`, measured with
Python `time.perf_counter` around `subprocess.run`: CLI process startup,
Unix-socket connect, daemon dispatch, and the CLI reading its reply. `ping`
is the cheapest possible command (no AX/window work), so this isolates the
per-command fixed cost — including any stderr writing the daemon does before
it replies. p50/p95 use the same nearest-rank definition as the switch phase.

`--commands N` also reports `daemon.err.log` growth over the phase, total and
per command. That second figure is the one a logging change has to move: on
0.11.1 it is about 43 B per `ping`, one `dispatch` line apiece.

### Workspace-switch round-trip (p50 / p95)

One sample is the full wall-clock round trip of `tili workspace <name>`:
CLI process startup, Unix-socket connect, daemon dispatch (including
parking/unparking windows), and the CLI reading its reply. Python
`time.perf_counter` measures it around `subprocess.run`. p50/p95 use the
nearest-rank definition on the collected samples; with 20 switches a p95 is
the 19th of 20 values, so it is coarse by construction. This is the metric
that later scopes should move, and it needs the opt-in switch run.

### `daemon.err.log` growth

Byte-size delta of `~/Library/Logs/tili/daemon.err.log` across the command
and/or switch phase (`stat -f%z`). Zero means that load produced no new
warnings or errors — the target once timing/diagnostic lines are off by
default. A negative value means the log was rotated or truncated during the
run.

## Preliminary baseline

Owner-supplied readings from the installed 0.11.1 build, idle machine:

| Process | RSS | Threads | Idle CPU |
| --- | ---: | ---: | ---: |
| `tili-daemon` | 28.5 MB | 24 | ~0.1% |
| `tili-menubar` | 42.0 MB | 5 | ~0.1% |

Replace this table with a `scripts/perf/collect.sh` run on the same machine
in the same macOS/commit state, and record the window length and switch
count used.

## Layout bench baseline (this worktree)

Criterion median, `0021af0` + uncommitted harness, Apple M4, one run of
`cargo bench -p tili-tree --bench layout -- --save-baseline scope0`. The
cached copy lives at `~/.cache/tili-perf/criterion/scope0.tar`.

| Bench id | Median | ns/window |
| --- | ---: | ---: |
| `layout/tiles/1` | 23.8 ns | 23.8 |
| `layout/tiles/2` | 41.2 ns | 20.6 |
| `layout/tiles/4` | 52.3 ns | 13.1 |
| `layout/tiles/8` | 151.6 ns | 19.0 |
| `layout/tiles/16` | 286.9 ns | 17.9 |
| `layout/tiles/32` | 477.6 ns | 14.9 |
| `layout/tiles/64` | 810.9 ns | 12.7 |
| `layout/accordion/1` | 23.8 ns | 23.8 |
| `layout/accordion/2` | 39.7 ns | 19.9 |
| `layout/accordion/4` | 51.9 ns | 13.0 |
| `layout/accordion/8` | 147.8 ns | 18.5 |
| `layout/accordion/16` | 273.1 ns | 17.1 |
| `layout/accordion/32` | 463.2 ns | 14.5 |
| `layout/accordion/64` | 784.2 ns | 12.3 |
| `insert/8` | 131.1 ns | — |
| `insert/32` | 204.3 ns | — |
| `insert/64` | 307.1 ns | — |
| `remove/8` | 73.5 ns | — |
| `remove/32` | 134.5 ns | — |
| `remove/64` | 208.4 ns | — |
| `focus/right/32` | 26.5 ns | — |
| `balance/root/32` | 52.8 ns | — |

Criterion prints a confidence interval for every id; treat differences under
roughly 20% on the `insert`/`remove` ids as noise without a repeat run.

Layout is sub-microsecond even at 64 windows, while one AX round trip costs
milliseconds — so a tree-level allocation change is not worth a scope on its
own. The numbers exist to prove that and to catch an accidental regression.

## Deliberately not measured

- AX/window-server work: frame reads/writes, classification, observer
  callbacks. None of it is reachable from `tili-tree`, which is the point of
  the crate split.
- Deeply nested trees produced by `join`/`move`. Fixtures use the flat root
  that sequential inserts produce, so a scope optimizing nested traversal
  should add a fixture instead of assuming this table covers it.
- Real workspace-switch cost in the committed baseline: the switch phase is
  opt-in because it moves windows, so the owner has to supply those numbers
  separately.
- Energy, wakeups, allocations, and syscall counts. No `powermetrics`, no
  Instruments, no `sudo` — the harness is intentionally runnable by anyone.
