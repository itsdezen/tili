#!/usr/bin/env bash
#
# Automated verification for a performance scope: the exact CI gate plus the
# layout bench, with an optional saved-baseline comparison that fails on a
# regression.
#
# This exists so a perf change can be accepted or rejected on evidence
# instead of a hand-run checklist. It never touches windows or workspaces;
# the opt-in switch measurement lives in collect.sh.
#
# Baseline storage: criterion keeps baselines under `target/criterion`,
# which is per-worktree and gitignored. `--save NAME` copies the matching
# directories to a cache outside the repo (default
# `~/.cache/tili-perf/criterion`) so a later scope in a fresh worktree can
# still compare against it with `--compare NAME`.
set -euo pipefail

MAX_REGRESSION_PCT=30
SAVE_NAME=""
COMPARE_NAME=""
RUN_TESTS=1
RUN_SAMPLE=0
SAMPLE_SECONDS=60
REPORT_PATH=""

SCRIPT_DIR=$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)
REPO_ROOT=$(cd "${SCRIPT_DIR}/../.." && pwd)
CACHE_ROOT="${TILI_PERF_CACHE:-${HOME}/.cache/tili-perf}/criterion"
BENCH_ARGS=()

usage() {
    cat <<'USAGE'
Usage: scripts/perf/verify.sh [OPTIONS]

Runs, in order:
  1. cargo fmt --all --check
  2. cargo clippy --workspace --all-targets -- -D warnings
  3. cargo test --workspace            (skip with --no-tests)
  4. cargo bench -p tili-tree --bench layout
  5. optional read-only process sample (--sample)

Options:
  --save NAME           save this run's criterion results as baseline NAME
  --compare NAME        compare against baseline NAME; exit 1 when any bench
                        regresses more than --max-regression
  --max-regression PCT  regression threshold in percent (default: 30)
  --no-tests            skip cargo test (fmt and clippy still run)
  --sample              also run collect.sh's read-only sampling window
  --seconds N           sampling window for --sample (default: 60)
  --report PATH         write the markdown summary to PATH as well
  -h, --help            this help

A regression only fails the run when criterion itself reports one AND the
point estimate exceeds the threshold. The default is deliberately loose:
these are sub-microsecond benches, and a busy machine can move one by 20%
between runs. Re-run a flagged id alone before believing it.

Baselines are cached under ~/.cache/tili-perf/criterion (override with
TILI_PERF_CACHE) because target/ is per-worktree and gitignored.

Examples:
  scripts/perf/verify.sh --save scope0
  scripts/perf/verify.sh --compare scope0
  scripts/perf/verify.sh --compare scope0 --report docs/perf/runs/s1.md
USAGE
}

die() {
    printf 'verify.sh: %s\n' "$*" >&2
    exit 1
}

while [ "$#" -gt 0 ]; do
    case "$1" in
        --save)
            [ "$#" -ge 2 ] || die "--save needs a name"
            SAVE_NAME=$2
            shift 2
            ;;
        --compare)
            [ "$#" -ge 2 ] || die "--compare needs a name"
            COMPARE_NAME=$2
            shift 2
            ;;
        --max-regression)
            [ "$#" -ge 2 ] || die "--max-regression needs a value"
            MAX_REGRESSION_PCT=$2
            shift 2
            ;;
        --no-tests)
            RUN_TESTS=0
            shift
            ;;
        --sample)
            RUN_SAMPLE=1
            shift
            ;;
        --seconds)
            [ "$#" -ge 2 ] || die "--seconds needs a value"
            SAMPLE_SECONDS=$2
            shift 2
            ;;
        --report)
            [ "$#" -ge 2 ] || die "--report needs a path"
            REPORT_PATH=$2
            shift 2
            ;;
        -h | --help)
            usage
            exit 0
            ;;
        *)
            die "unknown argument '$1' (try --help)"
            ;;
    esac
done

[ -z "$SAVE_NAME" ] || [ -z "$COMPARE_NAME" ] \
    || die "--save and --compare are mutually exclusive"
case "$MAX_REGRESSION_PCT" in
    '' | *[!0-9.]*) die "--max-regression must be a number" ;;
esac

cd "$REPO_ROOT"

WORK_DIR=$(mktemp -d -t tili-perf-verify) || die "could not create a temp dir"
cleanup() {
    local status=$?
    rm -rf "$WORK_DIR" || true
    exit "$status"
}
trap cleanup EXIT

step() {
    printf '\n==> %s\n' "$*"
}

run_logged() {
    local log=$1
    shift
    if ! "$@" >"$log" 2>&1; then
        printf 'FAILED: %s\n\n' "$*" >&2
        tail -n 40 "$log" >&2
        return 1
    fi
    tail -n 3 "$log" | sed 's/^/    /'
    return 0
}

started_at=$(date -u '+%Y-%m-%dT%H:%M:%SZ')
commit=$(git rev-parse --short HEAD 2>/dev/null || printf 'unknown')

# ---------------------------------------------------------------------------
# 1-3. The CI gate
# ---------------------------------------------------------------------------

step "cargo fmt --all --check"
run_logged "$WORK_DIR/fmt.log" cargo fmt --all --check \
    || die "formatting is not clean; run cargo fmt"

step "cargo clippy --workspace --all-targets -- -D warnings"
run_logged "$WORK_DIR/clippy.log" \
    cargo clippy --workspace --all-targets -- -D warnings \
    || die "clippy failed"

if [ "$RUN_TESTS" = "1" ]; then
    step "cargo test --workspace"
    run_logged "$WORK_DIR/test.log" cargo test --workspace \
        || die "tests failed"
    grep -h '^test result:' "$WORK_DIR/test.log" \
        | awk -F'[ ;]' '{p+=$4; f+=$6} END {printf "    %d passed, %d failed\n", p, f}'
else
    step "cargo test --workspace (skipped)"
fi

# ---------------------------------------------------------------------------
# 4. Layout bench, optionally against a cached baseline
# ---------------------------------------------------------------------------

if [ -n "$COMPARE_NAME" ]; then
    step "restoring cached baseline '$COMPARE_NAME'"
    if [ ! -f "$CACHE_ROOT/$COMPARE_NAME.tar" ]; then
        die "no cached baseline '$COMPARE_NAME' (run --save $COMPARE_NAME first)"
    fi
    # A fresh worktree has no target/criterion yet — criterion only creates
    # it on the first bench run — so the extraction target has to exist
    # before tar will chdir into it.
    mkdir -p target/criterion
    tar -xf "$CACHE_ROOT/$COMPARE_NAME.tar" -C target/criterion \
        || die "could not restore baseline '$COMPARE_NAME'"
    BENCH_ARGS+=(--baseline "$COMPARE_NAME")
fi

if [ -n "$SAVE_NAME" ]; then
    BENCH_ARGS+=(--save-baseline "$SAVE_NAME")
fi

step "cargo bench -p tili-tree --bench layout ${BENCH_ARGS[*]:-}"
if ! cargo bench -p tili-tree --bench layout -- "${BENCH_ARGS[@]}" \
    >"$WORK_DIR/bench.log" 2>&1; then
    tail -n 40 "$WORK_DIR/bench.log" >&2
    die "bench failed"
fi

if [ -n "$SAVE_NAME" ]; then
    step "caching baseline '$SAVE_NAME'"
    mkdir -p "$CACHE_ROOT"
    baseline_dirs=$(find target/criterion -type d -name "$SAVE_NAME" | sort)
    [ -n "$baseline_dirs" ] \
        || die "criterion wrote no '$SAVE_NAME' baseline directories"
    baseline_count=$(printf '%s\n' "$baseline_dirs" | wc -l | tr -d '[:space:]')
    # Deliberate word splitting: tar takes the relative paths as separate
    # arguments, and criterion's bench ids never contain whitespace.
    # shellcheck disable=SC2046
    tar -cf "$CACHE_ROOT/$SAVE_NAME.tar" -C target/criterion \
        $(printf '%s\n' "$baseline_dirs" | sed 's|^target/criterion/||')
    printf '    %s bench results cached in %s\n' \
        "$baseline_count" "$CACHE_ROOT/$SAVE_NAME.tar"
fi

# ---------------------------------------------------------------------------
# 5. Optional read-only process sample
# ---------------------------------------------------------------------------

if [ "$RUN_SAMPLE" = "1" ]; then
    step "process sample (${SAMPLE_SECONDS}s, read-only)"
    if ! "$SCRIPT_DIR/collect.sh" --seconds "$SAMPLE_SECONDS" \
        >"$WORK_DIR/sample.md" 2>&1; then
        tail -n 20 "$WORK_DIR/sample.md" >&2
        die "collect.sh failed"
    fi
    cat "$WORK_DIR/sample.md"
fi

# ---------------------------------------------------------------------------
# Summary
# ---------------------------------------------------------------------------

SUMMARY="$WORK_DIR/summary.md"
set +e
python3 - "$WORK_DIR/bench.log" "$SUMMARY" "$started_at" "$commit" \
    "$SAVE_NAME" "$COMPARE_NAME" "$MAX_REGRESSION_PCT" <<'PY'
import json
import re
import sys
from pathlib import Path

bench_log, summary_path, started_at, commit, save_name, compare_name, threshold = sys.argv[1:]
threshold = float(threshold)

text = Path(bench_log).read_text(encoding="utf-8", errors="replace")
# Criterion 0.8 splits the change into two lines (`change:` then an indented
# `time: [...]`); older output kept it inline. Fold both into one line so a
# single pattern covers either format.
text = re.sub(r"change:\s*\n\s*time:\s*\[([^\]]+)\]", r"change: [\1]", text)
current = None
rows = []
for line in text.splitlines():
    match = re.match(r"^(\S.*?)\s+time:\s+\[", line)
    if match:
        current = match.group(1).strip()
        continue
    match = re.match(r"^\s+change:\s+\[([^\]]+)\]", line)
    if match and current:
        parts = [part.strip().replace("\u2212", "-") for part in match.group(1).split()]
        if len(parts) >= 2 and parts[1].endswith("%"):
            # [id, point-estimate change, criterion's own verdict]
            rows.append([current, float(parts[1].rstrip("%")), "no change"])
        continue
    # Criterion prints its statistical verdict on the line after the change
    # interval; that verdict, not the raw percentage, decides a regression.
    match = re.match(r"^\s+Performance has (regressed|improved)\.", line)
    if match and rows:
        rows[-1][2] = match.group(1)

regressed = [
    (name, pct)
    for name, pct, verdict in rows
    if verdict == "regressed" and pct > threshold
]
improved = [
    (name, pct)
    for name, pct, verdict in rows
    if verdict == "improved" and pct < -threshold
]

lines = ["## perf verify", ""]
lines.append(f"- at: {started_at}")
lines.append(f"- commit: `{commit}`")
if save_name:
    lines.append(f"- saved baseline: `{save_name}`")
if compare_name:
    lines.append(f"- compared against: `{compare_name}` (fails only on a criterion-confirmed "
                 f"regression above {threshold:g}%)")
lines.append("")

if rows:
    lines.append("| bench id | change | verdict |")
    lines.append("| --- | ---: | --- |")
    for name, pct, verdict in rows:
        shown = verdict
        if verdict == "regressed" and pct <= threshold:
            shown = "regressed, under threshold"
        elif verdict == "improved" and pct >= -threshold:
            shown = "improved, under threshold"
        lines.append(f"| `{name}` | {pct:+.1f}% | {shown} |")
else:
    lines.append("No `change:` lines in the bench output — this was a baseline run, "
                 "or criterion found no comparable baseline.")
lines.append("")
if regressed:
    lines.append(f"**{len(regressed)} bench id(s) regressed: criterion confirmed a change "
                 f"above {threshold:g}%.**")
elif compare_name:
    lines.append(f"No criterion-confirmed regression above {threshold:g}%.")
if improved:
    lines.append(f"{len(improved)} bench id(s) improved by more than {threshold:g}%.")

document = "\n".join(lines) + "\n"
Path(summary_path).write_text(document, encoding="utf-8")
print(document)

result = {
    "timestamp": started_at,
    "commit": commit,
    "saved_baseline": save_name or None,
    "compared_against": compare_name or None,
    "max_regression_pct": threshold,
    "regressions": [{"id": name, "change_pct": pct} for name, pct in regressed],
}
Path(summary_path).with_suffix(".json").write_text(
    json.dumps(result, indent=2) + "\n", encoding="utf-8"
)
sys.exit(1 if regressed else 0)
PY
verify_status=$?
set -e

if [ -n "$REPORT_PATH" ]; then
    mkdir -p "$(dirname "$REPORT_PATH")"
    cp "$SUMMARY" "$REPORT_PATH"
    printf '\nsummary written to %s\n' "$REPORT_PATH"
fi

exit "$verify_status"
