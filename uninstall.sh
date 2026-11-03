#!/usr/bin/env bash
set -e

# Folio Uninstaller Script

APP_NAME="folio"

if [ "$EUID" -eq 0 ]; then
    INSTALL_PREFIX="/usr/local"
    DESKTOP_DIR="/usr/share/applications"
    ICON_BASE_DIR="/usr/share/icons/hicolor"
    PIXMAPS_DIR="/usr/share/pixmaps"
    BIN_DIR="$INSTALL_PREFIX/bin"
    MODE="system-wide"
else
    INSTALL_PREFIX="$HOME/.local"
    DESKTOP_DIR="$HOME/.local/share/applications"
    ICON_BASE_DIR="$HOME/.local/share/icons/hicolor"
    PIXMAPS_DIR="$HOME/.local/share/pixmaps"
    BIN_DIR="$INSTALL_PREFIX/bin"
    MODE="user ($USER)"
fi

echo "=========================================="
echo " Uninstalling Folio (Mode: $MODE)"
echo "=========================================="

echo "-> Removing binary..."
rm -f "$BIN_DIR/$APP_NAME"

echo "-> Removing desktop file..."
rm -f "$DESKTOP_DIR/$APP_NAME.desktop"

echo "-> Removing icons..."
rm -f "$ICON_BASE_DIR/scalable/apps/$APP_NAME.svg"
for SZ in 16 32 48 64 128 256 512; do
    rm -f "$ICON_BASE_DIR/${SZ}x${SZ}/apps/$APP_NAME.png"
done
rm -f "$PIXMAPS_DIR/$APP_NAME.png"
rm -f "$PIXMAPS_DIR/$APP_NAME.svg"

echo "-> Updating caches..."
if command -v update-desktop-database >/dev/null 2>&1; then
    update-desktop-database "$DESKTOP_DIR" 2>/dev/null || true
fi

if command -v gtk-update-icon-cache >/dev/null 2>&1; then
    gtk-update-icon-cache -f -t "$ICON_BASE_DIR" 2>/dev/null || true
fi

echo "Folio has been uninstalled."
