#!/usr/bin/env bash
set -e

# Folio Installation Script
# Installs Folio binary, desktop entry, and icon assets without requiring cargo.

APP_NAME="folio"
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"

# Detect root / system-wide vs user installation
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
echo " Installing Folio (Mode: $MODE)"
echo "=========================================="

# 1. Locate binary
BINARY_SOURCE=""
if [ -f "$SCRIPT_DIR/bin/folio" ]; then
    BINARY_SOURCE="$SCRIPT_DIR/bin/folio"
elif [ -f "$SCRIPT_DIR/folio" ]; then
    BINARY_SOURCE="$SCRIPT_DIR/folio"
elif [ -f "$SCRIPT_DIR/target/release/folio" ]; then
    BINARY_SOURCE="$SCRIPT_DIR/target/release/folio"
elif command -v cargo >/dev/null 2>&1; then
    echo "No pre-built binary found, building release with cargo..."
    cargo build --release --manifest-path "$SCRIPT_DIR/Cargo.toml"
    BINARY_SOURCE="$SCRIPT_DIR/target/release/folio"
else
    echo "Error: Could not find pre-built 'folio' binary in:"
    echo "  - $SCRIPT_DIR/bin/folio"
    echo "  - $SCRIPT_DIR/folio"
    echo "  - $SCRIPT_DIR/target/release/folio"
    echo "Please place the pre-built 'folio' executable in this directory or build with cargo."
    exit 1
fi

# 2. Install executable
echo "-> Installing binary to $BIN_DIR/folio..."
mkdir -p "$BIN_DIR"
install -m 755 "$BINARY_SOURCE" "$BIN_DIR/$APP_NAME"

# 3. Install icon assets
echo "-> Installing icon assets..."
ICON_SRC_DIR="$SCRIPT_DIR/assets/icons"

# Scalable SVG
if [ -f "$ICON_SRC_DIR/folio-icon-dark.svg" ]; then
    mkdir -p "$ICON_BASE_DIR/scalable/apps"
    install -m 644 "$ICON_SRC_DIR/folio-icon-dark.svg" "$ICON_BASE_DIR/scalable/apps/$APP_NAME.svg"
fi

# Fixed-size PNGs
for SZ in 16 32 48 64 128 256 512; do
    PNG_FILE="$ICON_SRC_DIR/png/folio-$SZ.png"
    if [ -f "$PNG_FILE" ]; then
        mkdir -p "$ICON_BASE_DIR/${SZ}x${SZ}/apps"
        install -m 644 "$PNG_FILE" "$ICON_BASE_DIR/${SZ}x${SZ}/apps/$APP_NAME.png"
    fi
done

# Pixmap fallback
mkdir -p "$PIXMAPS_DIR"
if [ -f "$ICON_SRC_DIR/png/folio-512.png" ]; then
    install -m 644 "$ICON_SRC_DIR/png/folio-512.png" "$PIXMAPS_DIR/$APP_NAME.png"
elif [ -f "$ICON_SRC_DIR/folio-icon-dark.svg" ]; then
    install -m 644 "$ICON_SRC_DIR/folio-icon-dark.svg" "$PIXMAPS_DIR/$APP_NAME.svg"
fi

# 4. Install desktop entry
echo "-> Installing desktop entry to $DESKTOP_DIR/folio.desktop..."
mkdir -p "$DESKTOP_DIR"
if [ -f "$SCRIPT_DIR/assets/folio.desktop" ]; then
    install -m 644 "$SCRIPT_DIR/assets/folio.desktop" "$DESKTOP_DIR/$APP_NAME.desktop"
fi

# 5. Update system caches
echo "-> Updating desktop database and icon caches..."
if command -v update-desktop-database >/dev/null 2>&1; then
    update-desktop-database "$DESKTOP_DIR" 2>/dev/null || true
fi

if command -v gtk-update-icon-cache >/dev/null 2>&1; then
    gtk-update-icon-cache -f -t "$ICON_BASE_DIR" 2>/dev/null || true
fi

echo ""
echo "=========================================="
echo " Folio has been installed successfully!"
echo "=========================================="
echo ""

if [[ ":$PATH:" != *":$BIN_DIR:"* ]]; then
    echo "Note: $BIN_DIR is not in your current PATH."
    echo "Add it to your shell config (~/.bashrc or ~/.zshrc):"
    echo "  export PATH=\"\$PATH:$BIN_DIR\""
    echo ""
fi

echo "You can now launch Folio from your application menu or by running:"
echo "  $APP_NAME"
