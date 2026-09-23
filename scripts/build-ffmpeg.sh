#!/bin/bash
# Builds FFmpeg from source (shared libraries) with libx264, zlib and the
# platform hardware encoder (NVENC on Linux, VideoToolbox on macOS) into a
# local prefix. Used by build-appimage.sh and build-macos.sh.
#
# Usage: scripts/build-ffmpeg.sh <prefix>
set -euo pipefail

# No `realpath -m`: macOS has the BSD one.
mkdir -p "$1"
PREFIX="$(cd "$1" && pwd)"

FFMPEG_TAG="n9.0.1"
# NVENC SDK 12.2: an NVIDIA driver >= 550 is enough.
NV_HEADERS_TAG="n12.2.72.0"
X264_BRANCH="stable"

OS="$(uname -s)"
if [ "$OS" = Darwin ]; then
    WANT="$FFMPEG_TAG videotoolbox $X264_BRANCH ${MACOSX_DEPLOYMENT_TARGET:-}"
    JOBS="$(sysctl -n hw.ncpu)"
    HW_FLAGS=(--enable-videotoolbox)
else
    WANT="$FFMPEG_TAG $NV_HEADERS_TAG $X264_BRANCH"
    JOBS="$(nproc)"
    HW_FLAGS=(--enable-ffnvcodec --enable-nvenc)
fi

STAMP="$PREFIX/.build-stamp"
if [ -f "$STAMP" ] && [ "$(cat "$STAMP")" = "$WANT" ]; then
    exit 0
fi

rm -rf "$PREFIX"
SRC="$PREFIX/src"
mkdir -p "$SRC"
export PKG_CONFIG_PATH="$PREFIX/lib/pkgconfig"

if [ "$OS" != Darwin ]; then
    git clone --depth 1 --branch "$NV_HEADERS_TAG" \
        https://github.com/FFmpeg/nv-codec-headers.git "$SRC/nv-codec-headers"
    make -C "$SRC/nv-codec-headers" PREFIX="$PREFIX" install
fi

git clone --depth 1 --branch "$X264_BRANCH" \
    https://code.videolan.org/videolan/x264.git "$SRC/x264"
(
    cd "$SRC/x264"
    ./configure --prefix="$PREFIX" --enable-shared --enable-pic --disable-cli
    make -j"$JOBS"
    make install
)

git clone --depth 1 --branch "$FFMPEG_TAG" \
    https://github.com/FFmpeg/FFmpeg.git "$SRC/ffmpeg"
(
    cd "$SRC/ffmpeg"
    # --disable-autodetect: no dependencies picked up at random from the build
    # machine; the ones we need are enabled explicitly.
    ./configure --prefix="$PREFIX" --enable-shared --disable-static \
        --enable-gpl --disable-autodetect --disable-programs --disable-doc \
        --enable-libx264 --enable-zlib "${HW_FLAGS[@]}"
    make -j"$JOBS"
    make install
)

rm -rf "$SRC"
echo "$WANT" > "$STAMP"
