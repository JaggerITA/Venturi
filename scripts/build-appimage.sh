#!/bin/bash
# AppImage con FFmpeg (libx264, NVENC) e libx264 inclusi come librerie
# condivise. glibc, ALSA, Vulkan e il driver NVIDIA restano quelli di sistema.
#
# Uso: scripts/build-appimage.sh
set -euo pipefail
cd "$(dirname "${BASH_SOURCE[0]}")/.."

# La build va fatta su Debian 13: un binario glibc richiede a runtime una
# glibc >= a quella di build, e la distro di sviluppo (Fedora) ne ha una
# troppo recente per l'AppImage. Se non siamo già dentro, ci rientriamo.
if [ -z "${VV_APPIMAGE_CONTAINER:-}" ]; then
    exec container/build-appimage.sh "$@"
fi

ARCH="$(uname -m)"
TARGET_DIR="$(realpath -m "${CARGO_TARGET_DIR:-target}")"
FFMPEG_PREFIX="$TARGET_DIR/ffmpeg-shared"
APPDIR="$TARGET_DIR/appimage/AppDir"
OUT="$TARGET_DIR/appimage/Venturi-$ARCH.AppImage"

scripts/build-ffmpeg.sh "$FFMPEG_PREFIX"

# FFMPEG_DIR punta ffmpeg-sys-next al nostro prefix (bindings dai suoi header);
# PKG_CONFIG_PATH resta come ripiego.
export FFMPEG_DIR="$FFMPEG_PREFIX"
export PKG_CONFIG_PATH="$FFMPEG_PREFIX/lib/pkgconfig"

# La macchina di build ha ffmpeg di sistema (serve ai test): senza questo -L
# esplicito, sul comando di link di vv-app il path /usr/lib64 (emesso da altri
# crate -sys, es. alsa-sys, via pkg-config) precede quello di ffmpeg-sys-next e
# il linker risolve -lavcodec & co. contro le libav di sistema, producendo un
# binario con soname diversi da quelli bundlati. I -L da RUSTFLAGS li mette
# rustc prima di quelli dei build-script, così vince il nostro prefix.
export RUSTFLAGS="-L native=$FFMPEG_PREFIX/lib${RUSTFLAGS:+ $RUSTFLAGS}"

# Cambiare RUSTFLAGS/FFMPEG_DIR non basta a far rilinkare vv-app se un artefatto
# release è già in cache: lo forziamo pulendo la catena ffmpeg.
cargo clean --release -p ffmpeg-sys-next -p vv-media -p vv-app
cargo build -p vv-app --release

rm -rf "$APPDIR"
mkdir -p "$APPDIR/usr/bin" "$APPDIR/usr/lib"
cp "$TARGET_DIR/release/vv-app" "$APPDIR/usr/bin/"
cp -a "$FFMPEG_PREFIX"/lib/*.so.* "$APPDIR/usr/lib/"
cp packaging/appimage/venturi.desktop "$APPDIR/"
mkdir -p "$APPDIR/usr/share/applications"
cp packaging/appimage/venturi.desktop "$APPDIR/usr/share/applications/"

# Il nome dei file icona deve combaciare con la chiave Icon= del .desktop.
cp media/icons/svg/vv-icon.svg "$APPDIR/venturi.svg"
for dir in media/icons/linux/hicolor/*/apps; do
    size="$(basename "$(dirname "$dir")")"
    dest="$APPDIR/usr/share/icons/hicolor/$size/apps"
    mkdir -p "$dest"
    for f in "$dir"/venturi-video.*; do
        cp "$f" "$dest/venturi.${f##*.}"
    done
done
cp media/icons/png/vv-icon-256.png "$APPDIR/.DirIcon"
ln -s usr/bin/vv-app "$APPDIR/AppRun"

# L'rpath va impostato qui, non via link-arg di cargo: gli argomenti dopo
# `--` a `cargo rustc` non entrano nel fingerprint, quindi se il crate è
# già compilato cargo salta il link e li ignora in silenzio, producendo un
# binario senza rpath. patchelf agisce sul file copiato, a prescindere
# dalla cache. --force-rpath => DT_RPATH (non RUNPATH), così vale anche per
# le dipendenze indirette, es. libavcodec -> libx264.
patchelf --force-rpath --set-rpath '$ORIGIN/../lib' "$APPDIR/usr/bin/vv-app"

# Verifica onesta del bundle: la macchina di build ha ffmpeg di sistema, per
# cui `ldd` risolverebbe le libav da lì mascherando un bundle rotto (rpath
# assente o libreria non copiata) che poi esplode su una distro diversa.
# Controlliamo invece che ogni soname libav*/libx264 richiesto sia davvero
# dentro l'AppDir.
for so in $(patchelf --print-needed "$APPDIR/usr/bin/vv-app"); do
    case "$so" in
        libav*|libsw*|libpostproc*|libx264*)
            if [ ! -e "$APPDIR/usr/lib/$so" ]; then
                echo "libreria richiesta non presente nel bundle: $so" >&2
                exit 1
            fi ;;
    esac
done

TOOL="$TARGET_DIR/appimage/appimagetool-$ARCH.AppImage"
if [ ! -x "$TOOL" ]; then
    curl -fL -o "$TOOL" \
        "https://github.com/AppImage/appimagetool/releases/download/continuous/appimagetool-$ARCH.AppImage"
    chmod +x "$TOOL"
fi
# Niente FUSE nei container: appimagetool si estrae da solo.
APPIMAGE_EXTRACT_AND_RUN=1 ARCH="$ARCH" "$TOOL" "$APPDIR" "$OUT"

# Nel container il target dir è un volume: l'AppImage va portata fuori.
if [ -n "${VV_APPIMAGE_OUT:-}" ]; then
    cp "$OUT" "$VV_APPIMAGE_OUT/"
    OUT="$VV_APPIMAGE_OUT/$(basename "$OUT")"
fi
echo "AppImage: $OUT"
