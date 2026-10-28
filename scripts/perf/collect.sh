#!/usr/bin/env bash
#
# tili performance sampler.
#
# Default mode is read-only: it watches the already-running daemon and menu
# bar for a fixed window and reports CPU-time delta, RSS, and thread count.
#
# --switch is opt-in and drives real windows and workspaces. It refuses to
# run without an interactive `yes` or TILI_PERF_CONFIRM=1, and restores the
# workspace that was active when the switch phase started.
#
# No sudo, powermetrics, or Instruments: every number here comes from `ps`
# plus wall-clock timing of the public CLI, so the script also works on a
# machine where those tools aren't installed.
set -euo pipefail

SAMPLE_SECONDS=60
SWITCH_COUNT=0
COMMAND_COUNT=0
JSON_PATH=""
TILI_BIN="${TILI_BIN:-tili}"
DAEMON_ERR_LOG="${HOME}/Library/Logs/tili/daemon.err.log"

SCRIPT_DIR=$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)
REPO_ROOT=$(cd "${SCRIPT_DIR}/../.." && pwd)

SAMPLES_TSV=""
NEED_RESTORE=0
RESTORE_WORKSPACE=""
LOG_GROWTH=""
RESTORE_FAILED=0

usage() {
    cat <<'USAGE'
Usage: scripts/perf/collect.sh [--seconds N] [--commands N] [--switch N] [--json PATH]

  --seconds N   read-only sampling window (default: 60)
  --commands N  additionally send `tili ping` N times and report the
                round-trip p50/p95 plus daemon.err.log growth per command.
                Read-only: `ping` never moves windows, so this never prompts.
  --switch N    additionally switch workspace N times, with confirmation;
                this MOVES REAL WINDOWS. N must be > 0 to take effect.
  --json PATH   also write the same readings as JSON to PATH.

The command phase measures a full CLI round trip, same as the switch phase:
process startup, socket connect, daemon dispatch, and reading the reply.

The switch phase uses this repo's `tili workspace <name>` subcommand (there
is no `tili switch`; see crates/tili-cli/src/main.rs). It cycles through the
workspaces other than the active one and puts the originally active
workspace back afterwards.

Confirm interactively by typing `yes`, or set TILI_PERF_CONFIRM=1 for a
scripted run. Without either, the switch phase aborts before touching
anything. Sampling is always safe and never prompts.

Set TILI_BIN to time a dev build instead of the installed `tili` on PATH.
USAGE
}

die() {
    printf 'collect.sh: %s\n' "$*" >&2
    exit 1
}

is_positive_int() {
    case "$1" in
        '' | *[!0-9]*) return 1 ;;
        *) [ "$1" -gt 0 ] ;;
    esac
}

is_nonnegative_int() {
    case "$1" in
        '' | *[!0-9]*) return 1 ;;
        *) return 0 ;;
    esac
}

cleanup() {
    local status=$?
    if [ "$NEED_RESTORE" = "1" ]; then
        NEED_RESTORE=0
        if "$TILI_BIN" workspace "$RESTORE_WORKSPACE" >/dev/null 2>&1; then
            printf 'collect.sh: restored originally active workspace %s\n' \
                "'$RESTORE_WORKSPACE'" >&2
        else
            printf 'collect.sh: WARNING: failed to restore workspace %s — run `%s workspace %s` manually\n' \
                "'$RESTORE_WORKSPACE'" "$TILI_BIN" "$RESTORE_WORKSPACE" >&2
        fi
    fi
    if [ -n "$SAMPLES_TSV" ]; then
        rm -f "$SAMPLES_TSV" || true
    fi
    exit "$status"
}
trap cleanup EXIT

while [ "$#" -gt 0 ]; do
    case "$1" in
        --seconds)
            [ "$#" -ge 2 ] || die "--seconds needs a value"
            SAMPLE_SECONDS=$2
            shift 2
            ;;
        --seconds=*)
            SAMPLE_SECONDS=${1#*=}
            shift
            ;;
        --commands)
            [ "$#" -ge 2 ] || die "--commands needs a value"
            COMMAND_COUNT=$2
            shift 2
            ;;
        --commands=*)
            COMMAND_COUNT=${1#*=}
            shift
            ;;
        --switch)
            [ "$#" -ge 2 ] || die "--switch needs a value"
            SWITCH_COUNT=$2
            shift 2
            ;;
        --switch=*)
            SWITCH_COUNT=${1#*=}
            shift
            ;;
        --json)
            [ "$#" -ge 2 ] || die "--json needs a path"
            JSON_PATH=$2
            shift 2
            ;;
        --json=*)
            JSON_PATH=${1#*=}
            shift
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

is_positive_int "$SAMPLE_SECONDS" || die "--seconds must be a positive integer"
is_nonnegative_int "$COMMAND_COUNT" || die "--commands must be a non-negative integer"
is_nonnegative_int "$SWITCH_COUNT" || die "--switch must be a non-negative integer"

command -v python3 >/dev/null 2>&1 \
    || die "python3 is required (elapsed timing, percentiles, and --json)"

pid_of() {
    local pid
    pid=$(pgrep -x "$1" 2>/dev/null | head -n 1 || true)
    printf '%s' "$pid"
}

# `ps` reports cumulative CPU time as [[dd-]hh:]mm:ss[.ss]; normalise it to
# seconds so a delta is just subtraction.
cpu_seconds() {
    awk -v value="$1" 'BEGIN {
        days = 0
        if (index(value, "-") > 0) {
            split(value, parts, "-")
            days = parts[1] + 0
            value = parts[2]
        }
        fields = split(value, clock, ":")
        if (fields == 3) {
            seconds = clock[1] * 3600 + clock[2] * 60 + clock[3]
        } else if (fields == 2) {
            seconds = clock[1] * 60 + clock[2]
        } else {
            seconds = clock[1] + 0
        }
        printf "%.2f", days * 86400 + seconds
    }'
}

cpu_time_of() {
    ps -o time= -p "$1" 2>/dev/null | tr -d '[:space:]'
}

rss_kb_of() {
    ps -o rss= -p "$1" 2>/dev/null | tr -d '[:space:]'
}

# RSS counts shared framework pages a process has merely touched, which is why
# two AppKit processes can differ by tens of MB while their private memory is
# identical. `vmmap`'s physical footprint is the number that tracks what the
# process actually costs; best-effort, since it needs task access.
footprint_mb_of() {
    local raw
    raw=$(vmmap -summary "$1" 2>/dev/null \
        | awk '/^Physical footprint:/ { print $3; exit }')
    case "$raw" in
        '' | *[!0-9.]*[MGK]) printf 'n/a' ;;
        *M) printf '%.1f' "${raw%M}" ;;
        *K) awk -v kb="${raw%K}" 'BEGIN { printf "%.1f", kb / 1024 }' ;;
        *G) awk -v gb="${raw%G}" 'BEGIN { printf "%.1f", gb * 1024 }' ;;
        *) printf 'n/a' ;;
    esac
}

# `ps -M` prints one header row followed by one row per thread.
thread_count_of() {
    local rows
    rows=$(ps -M "$1" 2>/dev/null | wc -l | tr -d '[:space:]' || true)
    case "$rows" in
        '' | 0 | 1) printf 'n/a' ;;
        *) printf '%s' "$((rows - 1))" ;;
    esac
}

file_size_of() {
    if [ -f "$1" ]; then
        stat -f%z "$1" 2>/dev/null || printf '0'
    else
        printf '0'
    fi
}

now_epoch() {
    python3 -c 'import time; print("%.3f" % time.time())'
}

iso_now() {
    date -u '+%Y-%m-%dT%H:%M:%SZ'
}

emit_sample_row() {
    local name=$1 pid=$2 started_at=$3
    local finished_at cpu_delta cpu_pct rss_mb footprint_mb threads

    if [ -z "$pid" ] || [ -z "$started_at" ]; then
        printf '| %s | — | not running (skipped) | — | — | — | — |\n' "$name"
        printf '%s\t\t\t\t\t\t\n' "$name" >>"$SAMPLES_TSV"
        return
    fi

    finished_at=$(cpu_time_of "$pid")
    if [ -z "$finished_at" ]; then
        printf '| %s | %s | exited during sampling (skipped) | — | — | — | — |\n' \
            "$name" "$pid"
        printf '%s\t\t\t\t\t\t\n' "$name" >>"$SAMPLES_TSV"
        return
    fi

    cpu_delta=$(awk -v a="$(cpu_seconds "$started_at")" \
        -v b="$(cpu_seconds "$finished_at")" 'BEGIN { printf "%.2f", b - a }')
    cpu_pct=$(awk -v delta="$cpu_delta" -v window="$ELAPSED" \
        'BEGIN { if (window > 0) printf "%.2f", 100 * delta / window; else printf "0.00" }')
    rss_mb=$(awk -v kb="$(rss_kb_of "$pid")" 'BEGIN { printf "%.1f", kb / 1024 }')
    footprint_mb=$(footprint_mb_of "$pid")
    threads=$(thread_count_of "$pid")

    printf '| %s | %s | %s | %s | %s | %s | %s |\n' \
        "$name" "$pid" "$cpu_delta" "$cpu_pct" "$rss_mb" "$footprint_mb" "$threads"
    printf '%s\t%s\t%s\t%s\t%s\t%s\t%s\n' \
        "$name" "$pid" "$cpu_delta" "$cpu_pct" "$rss_mb" "$footprint_mb" "$threads" \
        >>"$SAMPLES_TSV"
}

confirm_switch() {
    local count=$1
    {
        printf '\n'
        printf '##############################################################\n'
        printf 'WARNING: --switch MOVES REAL WINDOWS AND WORKSPACES.\n'
        printf '\n'
        printf 'It will switch workspace %s times, then put back whichever\n' "$count"
        printf 'workspace was active when the switch phase started.\n'
        printf 'Do not use the machine while it runs.\n'
        printf '##############################################################\n'
        printf '\n'
    } >&2

    if [ "${TILI_PERF_CONFIRM:-}" = "1" ]; then
        printf 'collect.sh: proceeding because TILI_PERF_CONFIRM=1\n' >&2
        return 0
    fi
    if [ ! -t 0 ]; then
        printf 'collect.sh: refusing to move windows non-interactively; set TILI_PERF_CONFIRM=1 or run from a terminal\n' >&2
        return 1
    fi
    printf "Type 'yes' to continue: " >&2
    local answer=""
    IFS= read -r answer || true
    if [ "$answer" = "yes" ]; then
        return 0
    fi
    return 1
}

# ---------------------------------------------------------------------------
# Preconditions
# ---------------------------------------------------------------------------

DAEMON_PID=$(pid_of tili-daemon)
MENUBAR_PID=$(pid_of tili-menubar)

if [ "$COMMAND_COUNT" -gt 0 ]; then
    if [ -z "$DAEMON_PID" ]; then
        die "--commands requires a running tili-daemon to ping"
    fi
    if [ ! -x "$TILI_BIN" ] && ! command -v "$TILI_BIN" >/dev/null 2>&1; then
        die "--commands requires the tili CLI, but '$TILI_BIN' was not found"
    fi
fi

if [ "$SWITCH_COUNT" -gt 0 ] && [ -n "$DAEMON_PID" ] \
    && [ ! -x "$TILI_BIN" ] && ! command -v "$TILI_BIN" >/dev/null 2>&1; then
    die "--switch requires the tili CLI, but '$TILI_BIN' was not found"
fi

SWITCH_READY=0
if [ "$SWITCH_COUNT" -gt 0 ]; then
    if [ -z "$DAEMON_PID" ]; then
        printf 'collect.sh: tili-daemon is not running — skipping the switch phase.\n' >&2
        SWITCH_COUNT=0
    else
        confirm_switch "$SWITCH_COUNT" || die "aborted at the workspace-switch confirmation"
        SWITCH_READY=1
    fi
fi

STARTED_AT=$(iso_now)
HOST=$(hostname -s 2>/dev/null || printf 'unknown')
MACOS_VERSION=$(sw_vers -productVersion 2>/dev/null || printf 'unknown')
MACOS_BUILD=$(sw_vers -buildVersion 2>/dev/null || printf 'unknown')
COMMIT=$(git -C "$REPO_ROOT" rev-parse --short HEAD 2>/dev/null || printf 'unknown')
TILI_VERSION=$("$TILI_BIN" --version 2>/dev/null | head -n 1 || true)
if [ -z "$TILI_VERSION" ]; then
    TILI_VERSION="unknown"
fi

SAMPLES_TSV=$(mktemp -t tili-perf-samples) || die "could not create a temp file"

# ---------------------------------------------------------------------------
# Read-only sampling window
# ---------------------------------------------------------------------------

DAEMON_T0=""
MENUBAR_T0=""
if [ -n "$DAEMON_PID" ]; then
    DAEMON_T0=$(cpu_time_of "$DAEMON_PID")
fi
if [ -n "$MENUBAR_PID" ]; then
    MENUBAR_T0=$(cpu_time_of "$MENUBAR_PID")
fi

WINDOW_STARTED=$(now_epoch)
sleep "$SAMPLE_SECONDS"
WINDOW_FINISHED=$(now_epoch)
ELAPSED=$(awk -v a="$WINDOW_STARTED" -v b="$WINDOW_FINISHED" \
    'BEGIN { printf "%.3f", b - a }')

printf '## tili perf sample\n\n'
printf -- '- sampled: %s (%ss window, %.2fs elapsed)\n' "$STARTED_AT" "$SAMPLE_SECONDS" "$ELAPSED"
printf -- '- host: `%s` (%s, macOS %s build %s)\n' "$HOST" "$(uname -m)" "$MACOS_VERSION" "$MACOS_BUILD"
printf -- '- commit: `%s`\n' "$COMMIT"
printf -- '- tili: `%s`\n' "$TILI_VERSION"
printf -- '- daemon log: `%s`\n\n' "$DAEMON_ERR_LOG"

if [ -z "$DAEMON_PID" ] && [ -z "$MENUBAR_PID" ]; then
    printf '> No tili processes are running — nothing sampled.\n\n'
else
    printf '| process | pid | cpu time delta (s) | avg cpu (%%) | rss (MB) | footprint (MB) | threads |\n'
    printf '| --- | ---: | ---: | ---: | ---: | ---: | ---: |\n'
    emit_sample_row "tili-daemon" "$DAEMON_PID" "$DAEMON_T0"
    emit_sample_row "tili-menubar" "$MENUBAR_PID" "$MENUBAR_T0"
    printf '\n'
fi

# ---------------------------------------------------------------------------
# Read-only command round-trip (`ping` never moves windows)
# ---------------------------------------------------------------------------

COMMANDS_MEASURED=0
CMD_MEASURED_COUNT=""
CMD_P50_MS=""
CMD_P95_MS=""
CMD_LOG_BEFORE=0
CMD_LOG_AFTER=0
CMD_LOG_GROWTH=0
CMD_PER_COMMAND=""

if [ "$COMMAND_COUNT" -gt 0 ]; then
    printf '## command round-trip\n\n'
    CMD_LOG_BEFORE=$(file_size_of "$DAEMON_ERR_LOG")
    if SUMMARY=$(python3 - "$TILI_BIN" "$COMMAND_COUNT" <<'PY'
import subprocess
import sys
import time

tili = sys.argv[1]
count = int(sys.argv[2])
times = []
for _ in range(count):
    started = time.perf_counter()
    result = subprocess.run(
        [tili, "ping"],
        stdout=subprocess.DEVNULL,
        stderr=subprocess.PIPE,
        text=True,
    )
    elapsed_ms = (time.perf_counter() - started) * 1000.0
    if result.returncode != 0:
        sys.stderr.write(
            "collect.sh: tili ping failed: %s\n"
            % (result.stderr.strip() or "no error output")
        )
        sys.exit(1)
    times.append(elapsed_ms)

def nearest_rank(values, percentile):
    ordered = sorted(values)
    rank = max(1, -(-len(ordered) * percentile // 100))
    return ordered[rank - 1]

p50 = nearest_rank(times, 50)
p95 = nearest_rank(times, 95)
print("%d %.3f %.3f" % (len(times), p50, p95))
PY
    ); then
        COMMANDS_MEASURED=1
        read -r CMD_MEASURED_COUNT CMD_P50_MS CMD_P95_MS <<<"$SUMMARY"
    else
        printf '> Command measurement failed; see stderr above.\n\n' >&2
    fi
    CMD_LOG_AFTER=$(file_size_of "$DAEMON_ERR_LOG")
    CMD_LOG_GROWTH=$((CMD_LOG_AFTER - CMD_LOG_BEFORE))
    CMD_PER_COMMAND=$(awk -v bytes="$CMD_LOG_GROWTH" -v count="$CMD_MEASURED_COUNT" \
        'BEGIN { if (count > 0) printf "%.1f", bytes / count; else printf "n/a" }')

    printf '| commands | p50 (ms) | p95 (ms) | daemon.err.log growth (B) | per command (B) |\n'
    printf '| ---: | ---: | ---: | ---: | ---: |\n'
    if [ "$COMMANDS_MEASURED" = "1" ]; then
        printf '| %s | %s | %s | %s | %s |\n\n' \
            "$CMD_MEASURED_COUNT" "$CMD_P50_MS" "$CMD_P95_MS" \
            "$CMD_LOG_GROWTH" "$CMD_PER_COMMAND"
    else
        printf '| %s | n/a | n/a | %s | %s |\n\n' \
            "$COMMAND_COUNT" "$CMD_LOG_GROWTH" "$CMD_PER_COMMAND"
    fi
fi

# ---------------------------------------------------------------------------
# Opt-in workspace-switch round-trip
# ---------------------------------------------------------------------------

SWITCH_MEASURED=0
P50_MS=""
P95_MS=""
LOG_BEFORE=0
LOG_AFTER=0

if [ "$SWITCH_READY" = "1" ]; then
    printf '## workspace switch round-trip\n\n'

    if ! WS_LIST=$("$TILI_BIN" list-workspaces 2>/dev/null); then
        printf '> `tili list-workspaces` failed — switch phase skipped.\n\n'
        SWITCH_COUNT=0
    fi

    ACTIVE=""
    TARGETS=()
    if [ "$SWITCH_COUNT" -gt 0 ]; then
        while IFS= read -r line; do
            [ -n "$line" ] || continue
            marker=${line:0:1}
            # Names can contain spaces; the "N window(s)" suffix is the
            # stable delimiter in `tili list-workspaces` output.
            name=$(printf '%s' "${line:1}" | sed -E \
                's/^[[:space:]]+//; s/[[:space:]]+[0-9]+ window\(s\).*$//')
            [ -n "$name" ] || continue
            if [ "$marker" = "*" ]; then
                ACTIVE=$name
            else
                TARGETS+=("$name")
            fi
        done <<<"$WS_LIST"
    fi

    if [ "$SWITCH_COUNT" -gt 0 ]; then
        if [ -z "$ACTIVE" ]; then
            printf '> Could not determine the active workspace — switch phase skipped so nothing is left switched.\n\n'
            SWITCH_COUNT=0
        elif [ "${#TARGETS[@]}" -eq 0 ]; then
            printf '> No workspace other than the active `%s` is declared — nothing to switch to.\n\n' "$ACTIVE"
            SWITCH_COUNT=0
        else
            RESTORE_WORKSPACE=$ACTIVE
            LOG_BEFORE=$(file_size_of "$DAEMON_ERR_LOG")
            NEED_RESTORE=1

            SUMMARY=""
            if SUMMARY=$(python3 - "$TILI_BIN" "$SWITCH_COUNT" "${TARGETS[@]}" <<'PY'
import subprocess
import sys
import time

tili = sys.argv[1]
count = int(sys.argv[2])
targets = sys.argv[3:]
times = []
for index in range(count):
    target = targets[index % len(targets)]
    started = time.perf_counter()
    result = subprocess.run(
        [tili, "workspace", target],
        stdout=subprocess.DEVNULL,
        stderr=subprocess.PIPE,
        text=True,
    )
    elapsed_ms = (time.perf_counter() - started) * 1000.0
    if result.returncode != 0:
        sys.stderr.write(
            "collect.sh: switch to '%s' failed: %s\n"
            % (target, result.stderr.strip() or "no error output")
        )
        sys.exit(1)
    times.append(elapsed_ms)

def nearest_rank(values, percentile):
    ordered = sorted(values)
    rank = max(1, -(-len(ordered) * percentile // 100))
    return ordered[rank - 1]

p50 = nearest_rank(times, 50)
p95 = nearest_rank(times, 95)
print("%d %.3f %.3f" % (len(times), p50, p95))
PY
            ); then
                SWITCH_MEASURED=1
                read -r SWITCH_MEASURED_COUNT P50_MS P95_MS <<<"$SUMMARY"
            else
                printf '> Switch measurement failed; see stderr above.\n\n' >&2
            fi

            LOG_AFTER=$(file_size_of "$DAEMON_ERR_LOG")
            LOG_GROWTH=$((LOG_AFTER - LOG_BEFORE))

            if [ "$SWITCH_MEASURED" = "1" ]; then
                printf '| switches | p50 (ms) | p95 (ms) | daemon.err.log growth (B) |\n'
                printf '| ---: | ---: | ---: | ---: |\n'
                printf '| %s | %s | %s | %s |\n\n' \
                    "$SWITCH_MEASURED_COUNT" "$P50_MS" "$P95_MS" "$LOG_GROWTH"
            else
                printf '| switches | p50 (ms) | p95 (ms) | daemon.err.log growth (B) |\n'
                printf '| ---: | ---: | ---: | ---: |\n'
                printf '| %s | n/a | n/a | %s |\n\n' "$SWITCH_COUNT" "$LOG_GROWTH"
            fi

            # The trap covers interrupts during the measurement; by this
            # point a failure has already been reported, so don't retry it.
            if "$TILI_BIN" workspace "$ACTIVE" >/dev/null 2>&1; then
                NEED_RESTORE=0
                printf '> Restored the originally active workspace `%s`.\n\n' "$ACTIVE"
            else
                NEED_RESTORE=0
                RESTORE_FAILED=1
                printf 'collect.sh: WARNING: failed to restore workspace %s — run `%s workspace %s` manually\n' \
                    "'$ACTIVE'" "$TILI_BIN" "$ACTIVE" >&2
                printf '> WARNING: could not restore the originally active workspace `%s`.\n\n' "$ACTIVE"
            fi
        fi
    fi
fi

# ---------------------------------------------------------------------------
# Optional JSON copy
# ---------------------------------------------------------------------------

if [ -n "$JSON_PATH" ]; then
    python3 - "$JSON_PATH" "$SAMPLES_TSV" "$STARTED_AT" "$HOST" "$MACOS_VERSION" \
        "$MACOS_BUILD" "$COMMIT" "$TILI_VERSION" "$SAMPLE_SECONDS" "$ELAPSED" \
        "$COMMANDS_MEASURED" "$CMD_MEASURED_COUNT" "$CMD_P50_MS" "$CMD_P95_MS" \
        "$CMD_LOG_BEFORE" "$CMD_LOG_AFTER" "$SWITCH_MEASURED" "$P50_MS" \
        "$P95_MS" "$LOG_BEFORE" "$LOG_AFTER" "${RESTORE_WORKSPACE:-}" <<'PY'
import json
import sys

(
    path,
    samples_path,
    timestamp,
    host,
    macos_version,
    macos_build,
    commit,
    tili_version,
    sample_seconds,
    elapsed,
    commands_measured,
    cmd_count,
    cmd_p50_ms,
    cmd_p95_ms,
    cmd_log_before,
    cmd_log_after,
    switch_measured,
    p50_ms,
    p95_ms,
    log_before,
    log_after,
    restored_workspace,
) = sys.argv[1:]

samples = []
with open(samples_path, encoding="utf-8") as samples_file:
    for line in samples_file:
        (
            name,
            pid,
            cpu_delta,
            cpu_pct,
            rss_mb,
            footprint_mb,
            threads,
        ) = line.rstrip("\n").split("\t")
        running = bool(pid)
        samples.append(
            {
                "process": name,
                "running": running,
                "pid": int(pid) if running else None,
                "cpu_time_delta_s": float(cpu_delta) if cpu_delta else None,
                "avg_cpu_pct": float(cpu_pct) if cpu_pct else None,
                "rss_mb": float(rss_mb) if rss_mb else None,
                "physical_footprint_mb": (
                    float(footprint_mb) if footprint_mb not in ("", "n/a") else None
                ),
                "threads": int(threads) if threads.isdigit() else None,
            }
        )

commands = None
if commands_measured == "1":
    count = int(cmd_count)
    growth = int(cmd_log_after) - int(cmd_log_before)
    commands = {
        "count": count,
        "p50_ms": float(cmd_p50_ms),
        "p95_ms": float(cmd_p95_ms),
        "daemon_err_log_growth_bytes": growth,
        "daemon_err_log_growth_per_command_bytes": round(growth / count, 1),
    }

switch = None
if switch_measured == "1":
    switch = {
        "p50_ms": float(p50_ms),
        "p95_ms": float(p95_ms),
        "daemon_err_log_growth_bytes": int(log_after) - int(log_before),
        "restored_workspace": restored_workspace or None,
    }

document = {
    "timestamp": timestamp,
    "host": host,
    "macos": "%s (%s)" % (macos_version, macos_build),
    "commit": commit,
    "tili_version": tili_version,
    "sample_seconds": int(sample_seconds),
    "elapsed_seconds": float(elapsed),
    "samples": samples,
    "commands": commands,
    "switch": switch,
}
with open(path, "w", encoding="utf-8") as output:
    json.dump(document, output, indent=2)
    output.write("\n")
PY
    printf 'Wrote JSON to %s\n' "$JSON_PATH"
fi

if [ "$RESTORE_FAILED" = "1" ]; then
    exit 1
fi
