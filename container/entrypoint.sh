#!/bin/bash
# Starts Xvfb in the background and waits for it to be ready before running the
# requested command — without the wait, a `cargo run` launched right after
# may connect to an X server not listening yet and fail
# intermittently (it depends on how long Xvfb takes to start on the host
# machine).
set -euo pipefail

Xvfb "$DISPLAY" -screen 0 "$XVFB_RESOLUTION" -nolisten tcp >/tmp/xvfb.log 2>&1 &
XVFB_PID=$!
trap 'kill "$XVFB_PID" 2>/dev/null || true' EXIT

for _ in $(seq 1 50); do
    if xdpyinfo -display "$DISPLAY" >/dev/null 2>&1; then
        break
    fi
    sleep 0.2
done
if ! xdpyinfo -display "$DISPLAY" >/dev/null 2>&1; then
    echo "Xvfb did not start within the timeout, log:" >&2
    cat /tmp/xvfb.log >&2
    exit 1
fi

exec "$@"
