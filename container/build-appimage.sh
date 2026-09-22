#!/bin/bash
# Builds (if needed) the Debian 13 build image and runs
# scripts/build-appimage.sh inside it, which comes out with the AppImage in
# target/appimage/ on the host.
#
# Usage: container/build-appimage.sh
set -euo pipefail
cd "$(dirname "${BASH_SOURCE[0]}")/.."
REPO_ROOT="$(pwd)"

IMAGE=venturi-appimage
podman image exists "$IMAGE" ||
    podman build -t "$IMAGE" -f container/Containerfile.appimage container/

# Cargo artifacts and the compiled FFmpeg in a named volume: they persist
# between runs and do not mix with the host's target/, built with
# another toolchain. Only the finished AppImage is written to the host.
podman volume create venturi-appimage-cargo >/dev/null 2>&1 || true
podman volume create venturi-appimage-target >/dev/null 2>&1 || true
mkdir -p "$REPO_ROOT/target/appimage"

TTY=(); [ -t 0 ] && TTY=(-it)

exec podman run --rm "${TTY[@]}" \
    -v "$REPO_ROOT:/workspace:z" \
    -v "$REPO_ROOT/target/appimage:/out:z" \
    -v venturi-appimage-cargo:/usr/local/cargo/registry \
    -v venturi-appimage-target:/cargo-target \
    -e CARGO_TARGET_DIR=/cargo-target \
    -e VV_APPIMAGE_CONTAINER=1 \
    -e VV_APPIMAGE_OUT=/out \
    "$IMAGE" \
    scripts/build-appimage.sh "$@"
