#!/bin/bash
# Sessione persistente del container di test (Xvfb + repo montato) per
# poter avviare vv-app una volta e interagirci in più passi (mouse,
# screenshot) con comandi separati — un container `--rm` one-shot per
# comando (come run.sh) non va bene qui: l'app e Xvfb dovrebbero restare
# vivi tra un `podman exec` e l'altro.
#
# Uso:
#   container/session.sh start          # avvia la sessione in background
#   container/session.sh exec <cmd...>  # esegue un comando nella sessione
#   container/session.sh shot <nome>    # screenshot in container/shots/<nome>.png
#   container/session.sh stop           # ferma e rimuove la sessione
#
# Esempio di giro completo:
#   container/session.sh start
#   container/session.sh exec cargo build -p vv-app
#   container/session.sh exec bash -c 'DISPLAY=:99 ./target-container.../vv-app &'
#   (in realtà usa CARGO_TARGET_DIR=/cargo-target, vedi Containerfile)
#   container/session.sh shot avvio
#   container/session.sh exec xdotool mousemove 640 400 click 1
#   container/session.sh shot dopo-click
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
            echo "Sessione '$NAME' già attiva (container/session.sh stop per fermarla)." >&2
            exit 1
        fi
        podman run -d --name "$NAME" \
            -v "$REPO_ROOT:/workspace:Z" \
            -v venturi-cargo-registry:/root/.cargo/registry \
            -v venturi-target:/cargo-target \
            venturi-test \
            sleep infinity
        # L'entrypoint di norma fa `exec "$@"` dopo aver avviato Xvfb, ma
        # qui il processo principale è `sleep infinity` (per restare vivo
        # tra un `exec` e l'altro): Xvfb va avviato esplicitamente come
        # primo comando dentro il container appena partito.
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
                echo "Sessione '$NAME' pronta (Xvfb su :99)."
                exit 0
            fi
            sleep 0.2
        done
        echo "Xvfb non pronto entro il timeout, vedi 'podman exec $NAME cat /tmp/xvfb.log'." >&2
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
        echo "Sessione '$NAME' fermata."
        ;;
    *)
        echo "Uso: $0 {start|exec <cmd...>|shot [nome]|stop}" >&2
        exit 1
        ;;
esac
