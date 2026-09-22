#!/bin/bash
# Wrapper per lanciare comandi dentro il container di test di Venturi
# (vedi README.md in questa directory). Monta il repo dal path reale
# (sempre lo stato locale corrente, l'immagine non contiene il sorgente)
# e due volumi nominati per cache/target di cargo, così le build
# successive sono incrementali invece di ripartire da zero a ogni run.
#
# Uso: container/run.sh <comando...>
# Esempi:
#   container/run.sh cargo build -p vv-app
#   container/run.sh cargo test -p vv-app -- --test-threads=1
#   container/run.sh bash   # shell interattiva dentro il container

set -euo pipefail
cd "$(dirname "${BASH_SOURCE[0]}")/.."
REPO_ROOT="$(pwd)"

podman volume create venturi-cargo-registry >/dev/null 2>&1 || true
podman volume create venturi-target >/dev/null 2>&1 || true

exec podman run --rm -it \
    -v "$REPO_ROOT:/workspace:Z" \
    -v venturi-cargo-registry:/root/.cargo/registry \
    -v venturi-target:/cargo-target \
    venturi-test \
    "$@"
