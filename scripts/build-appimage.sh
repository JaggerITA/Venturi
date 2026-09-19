#!/bin/bash
# AppImage con FFmpeg (libx264, NVENC) e libx264 inclusi come librerie
# condivise. glibc, ALSA, Vulkan e il driver NVIDIA restano quelli di sistema.
#
# Uso: scripts/build-appimage.sh
set -euo pipefail
cd "$(dirname "${BASH_SOURCE[0]}")/.."

ARCH="$(uname -m)"
TARGET_DIR="$(realpath -m "${CARGO_TARGET_DIR:-target}")"
FFMPEG_PREFIX="$TARGET_DIR/ffmpeg-shared"
APPDIR="$TARGET_DIR/appimage/AppDir"
OUT="$TARGET_DIR/appimage/VibeVideo-$ARCH.AppImage"

scripts/build-ffmpeg.sh "$FFMPEG_PREFIX"
export PKG_CONFIG_PATH="$FFMPEG_PREFIX/lib/pkgconfig"

# DT_RPATH (non RUNPATH) perché valga anche per le dipendenze indirette,
# es. libavcodec -> libx264.
cargo rustc -p vv-app --release -- \
    -C link-arg=-Wl,--disable-new-dtags \
    -C 'link-arg=-Wl,-rpath,$ORIGIN/../lib'

rm -rf "$APPDIR"
mkdir -p "$APPDIR/usr/bin" "$APPDIR/usr/lib"
cp "$TARGET_DIR/release/vv-app" "$APPDIR/usr/bin/"
cp -a "$FFMPEG_PREFIX"/lib/*.so.* "$APPDIR/usr/lib/"
cp packaging/appimage/vibevideo.desktop packaging/appimage/vibevideo.svg "$APPDIR/"
ln -s usr/bin/vv-app "$APPDIR/AppRun"

MISSING="$(ldd "$APPDIR/usr/bin/vv-app" | grep 'not found' || true)"
if [ -n "$MISSING" ]; then
    echo "librerie mancanti nell'AppDir:" >&2
    echo "$MISSING" >&2
    exit 1
fi

TOOL="$TARGET_DIR/appimage/appimagetool-$ARCH.AppImage"
if [ ! -x "$TOOL" ]; then
    curl -fL -o "$TOOL" \
        "https://github.com/AppImage/appimagetool/releases/download/continuous/appimagetool-$ARCH.AppImage"
    chmod +x "$TOOL"
fi
# Niente FUSE nei container: appimagetool si estrae da solo.
APPIMAGE_EXTRACT_AND_RUN=1 ARCH="$ARCH" "$TOOL" "$APPDIR" "$OUT"
echo "AppImage: $OUT"
