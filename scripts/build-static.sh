#!/bin/bash
# Binario redistribuibile: FFmpeg e libx264 compilati da sorgente e linkati
# statici. x264 va preparato prima di cargo perché il configure di FFmpeg
# gira nel build script di ffmpeg-sys-next e lo cerca via pkg-config.
#
# Uso: scripts/build-static.sh [argomenti extra per cargo build]
set -euo pipefail
cd "$(dirname "${BASH_SOURCE[0]}")/.."

TARGET_DIR="$(realpath -m "${CARGO_TARGET_DIR:-target}")"
X264_SRC="$TARGET_DIR/x264-src"
X264_PREFIX="$TARGET_DIR/x264"

if [ ! -f "$X264_PREFIX/lib/libx264.a" ]; then
    rm -rf "$X264_SRC"
    git clone --depth 1 --branch stable \
        https://code.videolan.org/videolan/x264.git "$X264_SRC"
    (
        cd "$X264_SRC"
        ./configure --prefix="$X264_PREFIX" \
            --enable-static --enable-pic --disable-cli
        make -j"$(nproc)"
        make install
    )
fi

# Solo libx264.a in quella directory: il linker non può ripiegare sulla .so.
export PKG_CONFIG_PATH="$X264_PREFIX/lib/pkgconfig${PKG_CONFIG_PATH:+:$PKG_CONFIG_PATH}"

cargo build -p vv-app --release --features static-ffmpeg "$@"
