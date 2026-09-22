#!/bin/bash
# Costruisce (se serve) l'immagine Debian 13 di build e ci esegue dentro
# scripts/build-appimage.sh, che ne esce con l'AppImage in
# target/appimage/ sull'host.
#
# Uso: container/build-appimage.sh
set -euo pipefail
cd "$(dirname "${BASH_SOURCE[0]}")/.."
REPO_ROOT="$(pwd)"

IMAGE=vibevideo-appimage
podman image exists "$IMAGE" ||
    podman build -t "$IMAGE" -f container/Containerfile.appimage container/

# Artefatti cargo e FFmpeg compilato in un volume nominato: restano tra
# una run e l'altra e non si mescolano al target/ dell'host, costruito con
# un'altra toolchain. Solo l'AppImage finita viene scritta sull'host.
podman volume create vibevideo-appimage-cargo >/dev/null 2>&1 || true
podman volume create vibevideo-appimage-target >/dev/null 2>&1 || true
mkdir -p "$REPO_ROOT/target/appimage"

TTY=(); [ -t 0 ] && TTY=(-it)

exec podman run --rm "${TTY[@]}" \
    -v "$REPO_ROOT:/workspace:z" \
    -v "$REPO_ROOT/target/appimage:/out:z" \
    -v vibevideo-appimage-cargo:/usr/local/cargo/registry \
    -v vibevideo-appimage-target:/cargo-target \
    -e CARGO_TARGET_DIR=/cargo-target \
    -e VV_APPIMAGE_CONTAINER=1 \
    -e VV_APPIMAGE_OUT=/out \
    "$IMAGE" \
    scripts/build-appimage.sh "$@"
