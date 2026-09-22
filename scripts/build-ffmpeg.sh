#!/bin/bash
# Builds FFmpeg from source (shared libraries) with libx264, zlib and NVENC
# into a local prefix. Used by build-appimage.sh.
#
# Usage: scripts/build-ffmpeg.sh <prefix>
set -euo pipefail

PREFIX="$(realpath -m "$1")"

FFMPEG_TAG="n9.0.1"
# NVENC SDK 12.2: an NVIDIA driver >= 550 is enough.
NV_HEADERS_TAG="n12.2.72.0"
X264_BRANCH="stable"

STAMP="$PREFIX/.build-stamp"
WANT="$FFMPEG_TAG $NV_HEADERS_TAG $X264_BRANCH"
if [ -f "$STAMP" ] && [ "$(cat "$STAMP")" = "$WANT" ]; then
    exit 0
fi

rm -rf "$PREFIX"
SRC="$PREFIX/src"
mkdir -p "$SRC"
export PKG_CONFIG_PATH="$PREFIX/lib/pkgconfig"
JOBS="$(nproc)"

git clone --depth 1 --branch "$NV_HEADERS_TAG" \
    https://github.com/FFmpeg/nv-codec-headers.git "$SRC/nv-codec-headers"
make -C "$SRC/nv-codec-headers" PREFIX="$PREFIX" install

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
        --enable-libx264 --enable-zlib \
        --enable-ffnvcodec --enable-nvenc
    make -j"$JOBS"
    make install
)

rm -rf "$SRC"
echo "$WANT" > "$STAMP"
