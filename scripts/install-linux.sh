#!/bin/bash
# Installa vv-app, il .desktop e le icone nel prefix indicato.
#
# Uso: scripts/install-linux.sh [--uninstall] [--prefix DIR]
# Default: ~/.local, oppure /usr/local se lanciato da root.
set -euo pipefail
cd "$(dirname "${BASH_SOURCE[0]}")/.."

PREFIX="${PREFIX:-}"
UNINSTALL=0
while [ $# -gt 0 ]; do
    case "$1" in
        --uninstall) UNINSTALL=1 ;;
        --prefix) PREFIX="$2"; shift ;;
        --prefix=*) PREFIX="${1#--prefix=}" ;;
        *) echo "argomento sconosciuto: $1" >&2; exit 1 ;;
    esac
    shift
done
if [ -z "$PREFIX" ]; then
    if [ "$(id -u)" -eq 0 ]; then PREFIX=/usr/local; else PREFIX="$HOME/.local"; fi
fi

BIN="$PREFIX/bin/vv-app"
DESKTOP="$PREFIX/share/applications/venturi.desktop"
ICONS="$PREFIX/share/icons/hicolor"

refresh_caches() {
    # Senza questo i menu continuano a mostrare la voce/icona vecchia.
    command -v gtk-update-icon-cache >/dev/null && \
        gtk-update-icon-cache -q -t -f "$ICONS" 2>/dev/null || true
    command -v update-desktop-database >/dev/null && \
        update-desktop-database -q "$PREFIX/share/applications" 2>/dev/null || true
}

if [ "$UNINSTALL" -eq 1 ]; then
    rm -f "$BIN" "$DESKTOP"
    find "$ICONS" -name 'venturi.png' -o -name 'venturi.svg' 2>/dev/null \
        | while read -r f; do rm -f "$f"; done
    refresh_caches
    echo "rimosso da $PREFIX"
    exit 0
fi

TARGET_DIR="$(realpath -m "${CARGO_TARGET_DIR:-target}")"
if [ ! -x "$TARGET_DIR/release/vv-app" ]; then
    cargo build -p vv-app --release
fi

install -Dm755 "$TARGET_DIR/release/vv-app" "$BIN"
install -Dm644 packaging/appimage/venturi.desktop "$DESKTOP"

# Il nome del file icona deve combaciare con la chiave Icon= del .desktop.
for dir in media/icons/linux/hicolor/*/apps; do
    size="$(basename "$(dirname "$dir")")"
    for f in "$dir"/venturi-video.*; do
        install -Dm644 "$f" "$ICONS/$size/apps/venturi.${f##*.}"
    done
done

refresh_caches

echo "installato in $PREFIX"
case ":$PATH:" in
    *":$PREFIX/bin:"*) ;;
    *) echo "nota: $PREFIX/bin non è nel PATH" ;;
esac
