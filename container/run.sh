#!/bin/bash
# Wrapper to run commands inside Venturi's test container
# (see README.md in this directory). It mounts the repo from its real path
# (always the current local state, the image does not contain the source)
# and two named volumes for cargo's cache/target, so later builds
# are incremental instead of starting from scratch on every run.
#
# Usage: container/run.sh <command...>
# Examples:
#   container/run.sh cargo build -p vv-app
#   container/run.sh cargo test -p vv-app -- --test-threads=1
#   container/run.sh bash   # interactive shell inside the container

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
