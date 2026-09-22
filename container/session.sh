#!/bin/bash
# Persistent session of the test container (Xvfb + mounted repo) so that
# vv-app can be started once and interacted with in several steps (mouse,
# screenshots) using separate commands — a one-shot `--rm` container per
# command (like run.sh) does not work here: the app and Xvfb need to stay
# alive between one `podman exec` and the next.
#
# Usage:
#   container/session.sh start          # starts the session in the background
#   container/session.sh exec <cmd...>  # runs a command in the session
#   container/session.sh shot <name>    # screenshot into container/shots/<name>.png
#   container/session.sh stop           # stops and removes the session
#
# Example of a full round:
#   container/session.sh start
#   container/session.sh exec cargo build -p vv-app
#   container/session.sh exec bash -c 'DISPLAY=:99 ./target-container.../vv-app &'
#   (it actually uses CARGO_TARGET_DIR=/cargo-target, see Containerfile)
#   container/session.sh shot startup
#   container/session.sh exec xdotool mousemove 640 400 click 1
#   container/session.sh shot after-click
#   container/session.sh stop

set -euo pipefail
cd "$(dirname "${BASH_SOURCE[0]}")/.."
REPO_ROOT="$(pwd)"
NAME=venturi-session

case "${1:-}" in
    start)
        podman volume create venturi-cargo-registry >/dev/null 2>&1 || true
        podman volume create venturi-target >/dev/null 2>&1 || true
        mkdir -p container/shots
        if podman container exists "$NAME"; then
            echo "Session '$NAME' already running (container/session.sh stop to stop it)." >&2
            exit 1
        fi
        podman run -d --name "$NAME" \
            -v "$REPO_ROOT:/workspace:Z" \
            -v venturi-cargo-registry:/root/.cargo/registry \
            -v venturi-target:/cargo-target \
            venturi-test \
            sleep infinity
        # The entrypoint normally does `exec "$@"` after starting Xvfb, but
        # here the main process is `sleep infinity` (to stay alive
        # between one `exec` and the next): Xvfb must be started explicitly as
        # the first command inside the freshly started container.
        podman exec -d "$NAME" bash -c '
            Xvfb "$DISPLAY" -screen 0 "$XVFB_RESOLUTION" -nolisten tcp >/tmp/xvfb.log 2>&1 &
            for _ in $(seq 1 50); do
                xdpyinfo -display "$DISPLAY" >/dev/null 2>&1 && exit 0
                sleep 0.2
            done
            exit 1
        '
        for _ in $(seq 1 50); do
            if podman exec "$NAME" xdpyinfo -display :99 >/dev/null 2>&1; then
                echo "Session '$NAME' ready (Xvfb on :99)."
                exit 0
            fi
            sleep 0.2
        done
        echo "Xvfb not ready within the timeout, see 'podman exec $NAME cat /tmp/xvfb.log'." >&2
        exit 1
        ;;
    exec)
        shift
        podman exec -e DISPLAY=:99 "$NAME" "$@"
        ;;
    shot)
        name="${2:-shot-$(date +%s)}"
        podman exec -e DISPLAY=:99 "$NAME" \
            import -window root -display :99 "/workspace/container/shots/$name.png"
        echo "container/shots/$name.png"
        ;;
    stop)
        podman rm -f "$NAME" >/dev/null 2>&1 || true
        echo "Session '$NAME' stopped."
        ;;
    *)
        echo "Usage: $0 {start|exec <cmd...>|shot [name]|stop}" >&2
        exit 1
        ;;
esac
