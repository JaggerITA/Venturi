#!/bin/bash
# Avvia Xvfb in background e aspetta che sia pronto prima di eseguire il
# comando richiesto — senza l'attesa, un `cargo run` lanciato subito dopo
# può connettersi a un X server non ancora in ascolto e fallire in modo
# intermittente (dipende da quanto Xvfb impiega ad avviarsi sulla macchina
# ospite).
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
    echo "Xvfb non si è avviato entro il timeout, log:" >&2
    cat /tmp/xvfb.log >&2
    exit 1
fi

exec "$@"
