#!/usr/bin/env bash
# Measures the "native frame age" of newly created windows: how long the
# app's own first frame stays on screen before tili places it. Requires the
# daemon to be running with TILI_TRACE_TIMING=1, and Ghostty (the app is
# driven through its AppleScript dictionary, so no keystrokes are needed).
#
# Usage: scripts/dev/window-created-latency.sh [count]
#
# Opens `count` (default 3) new Ghostty windows in the currently active
# workspace, lets each settle, then closes exactly the windows it created.
# Existing windows are resized while the test runs and restored afterwards;
# don't touch the keyboard/mouse during the run.
#
# Enabling the trace (once per daemon start):
#   tili-dev stop
#   launchctl setenv TILI_TRACE_TIMING 1
#   tili-dev start
# Turn it off afterwards with `launchctl unsetenv TILI_TRACE_TIMING` and
# another `tili-dev stop`/`tili-dev start`.
set -uo pipefail

COUNT=${1:-3}
LOG="$HOME/Library/Logs/tili/daemon.err.log"

if [ "$(launchctl getenv TILI_TRACE_TIMING)" != "1" ]; then
    cat >&2 <<'MSG'
TILI_TRACE_TIMING is not enabled for the daemon.
Restart it with trace on:
  tili-dev stop
  launchctl setenv TILI_TRACE_TIMING 1
  tili-dev start
MSG
    exit 1
fi

if ! pgrep -x ghostty >/dev/null; then
    echo "Ghostty is not running; open it first." >&2
    exit 1
fi

if [ ! -f "$LOG" ]; then
    echo "trace log not found: $LOG" >&2
    exit 1
fi

ghostty_pid=$(pgrep -x ghostty | head -1)
baseline=$(wc -l <"$LOG")
created=()

cleanup() {
    for id in "${created[@]:-}"; do
        [ -n "$id" ] || continue
        osascript -e "tell application \"Ghostty\" to close window (window id \"$id\")" \
            >/dev/null 2>&1 || true
    done
}
trap cleanup EXIT INT TERM

echo "creating $COUNT Ghostty window(s) (each is closed again)..."
for _ in $(seq 1 "$COUNT"); do
    id=$(osascript -e 'tell application "Ghostty"
        set w to new window
        return id of w
    end tell' 2>/dev/null)
    if [ -z "$id" ]; then
        echo "could not create a Ghostty window" >&2
        exit 1
    fi
    created+=("$id")
    # Let tili place it and the 200ms entry animation finish before the
    # next one, then close exactly this window.
    sleep 1
    osascript -e "tell application \"Ghostty\" to close window (window id \"$id\")" \
        >/dev/null 2>&1 || true
    created=("${created[@]/$id/}")
    sleep 0.6
done

echo
echo "window-created trace lines:"
sed -n "$((baseline + 1)),\$p" "$LOG" \
    | grep "window-created pid=${ghostty_pid} " || true

stats() {
    sort -n | awk '
        { a[NR] = $1; sum += $1 }
        END {
            if (NR == 0) { print "  (no samples)"; exit }
            printf "  n=%d  min=%.2f  median=%.2f  max=%.2f  mean=%.2f (ms)\n",
                NR, a[1], a[int((NR + 1) / 2)], a[NR], sum / NR
        }'
}

samples=$(sed -n "$((baseline + 1)),\$p" "$LOG" \
    | grep "window-created pid=${ghostty_pid} " \
    | grep -v coalesced)

echo
echo "queue_ms (AX notification → immediate drain start):"
printf '%s\n' "$samples" | sed -n 's/.* queue_ms=\([0-9.]*\).*/\1/p' | stats
echo "total_ms (AX notification → placement write):"
printf '%s\n' "$samples" | sed -n 's/.* total_ms=\([0-9.]*\).*/\1/p' | stats
