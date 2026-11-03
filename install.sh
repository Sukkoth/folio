#!/usr/bin/env bash
set -e

# Folio Universal Installation Script
# Supports:
# 1. One-line curl pipe: curl -fsSL https://raw.githubusercontent.com/Sukkoth/folio/main/install.sh | bash
# 2. Local execution: ./install.sh inside the repository or release archive

REPO="Sukkoth/folio"
APP_NAME="folio"

# Detect execution context
SCRIPT_DIR=""
if [ -n "${BASH_SOURCE[0]}" ] && [ -f "${BASH_SOURCE[0]}" ]; then
    SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
fi

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

TEMP_DIR=""
cleanup() {
    if [ -n "$TEMP_DIR" ] && [ -d "$TEMP_DIR" ]; then
        rm -rf "$TEMP_DIR"
    fi
}
trap cleanup EXIT

# 1. Locate binary and assets
BINARY_SOURCE=""
ICON_SRC_DIR=""
DESKTOP_SRC=""

if [ -n "$SCRIPT_DIR" ] && [ -f "$SCRIPT_DIR/bin/folio" ]; then
    BINARY_SOURCE="$SCRIPT_DIR/bin/folio"
    ICON_SRC_DIR="$SCRIPT_DIR/assets/icons"
    DESKTOP_SRC="$SCRIPT_DIR/assets/folio.desktop"
elif [ -n "$SCRIPT_DIR" ] && [ -f "$SCRIPT_DIR/target/release/folio" ]; then
    BINARY_SOURCE="$SCRIPT_DIR/target/release/folio"
    ICON_SRC_DIR="$SCRIPT_DIR/assets/icons"
    DESKTOP_SRC="$SCRIPT_DIR/assets/folio.desktop"
elif [ -n "$SCRIPT_DIR" ] && [ -f "$SCRIPT_DIR/folio" ]; then
    BINARY_SOURCE="$SCRIPT_DIR/folio"
    ICON_SRC_DIR="$SCRIPT_DIR/assets/icons"
    DESKTOP_SRC="$SCRIPT_DIR/assets/folio.desktop"
else
    # Remote / Curl piped mode: download latest release tarball from GitHub
    echo "-> Downloading latest release from GitHub ($REPO)..."
    OS="$(uname -s | tr '[:upper:]' '[:lower:]')"
    ARCH="$(uname -m)"

    if [ "$OS" != "linux" ]; then
        echo "Error: Automated installer currently supports Linux. Found: $OS"
        exit 1
    fi

    case "$ARCH" in
        x86_64|amd64)
            TARBALL_ARCH="linux-x86_64"
            ;;
        *)
            echo "Error: Architecture $ARCH is not yet supported for pre-built binaries."
            exit 1
            ;;
    esac

    # Find latest release tag
    LATEST_TAG=$(curl -sSL -H "Accept: application/vnd.github.v3+json" "https://api.github.com/repos/$REPO/releases/latest" 2>/dev/null | grep -o '"tag_name": *"[^"]*"' | head -n1 | cut -d'"' -f4)

    if [ -z "$LATEST_TAG" ]; then
        # Fallback to direct release URL or default tag
        LATEST_TAG="v0.0.1"
    fi

    VERSION_NUM="${LATEST_TAG#v}"
    DOWNLOAD_URL="https://github.com/$REPO/releases/download/$LATEST_TAG/folio-$VERSION_NUM-$TARBALL_ARCH.tar.gz"

    TEMP_DIR=$(mktemp -d)
    TAR_PATH="$TEMP_DIR/folio.tar.gz"

    echo "-> Fetching $DOWNLOAD_URL..."
    if ! curl -fSL --progress-bar -o "$TAR_PATH" "$DOWNLOAD_URL"; then
        echo "Error: Failed to download release bundle from $DOWNLOAD_URL."
        echo "Please verify GitHub releases exist for $REPO."
        exit 1
    fi

    echo "-> Extracting archive..."
    tar -xzf "$TAR_PATH" -C "$TEMP_DIR"

    EXTRACTED_DIR="$TEMP_DIR/folio-$VERSION_NUM-$TARBALL_ARCH"
    if [ ! -d "$EXTRACTED_DIR" ]; then
        EXTRACTED_DIR="$TEMP_DIR"
    fi

    BINARY_SOURCE="$EXTRACTED_DIR/bin/folio"
    ICON_SRC_DIR="$EXTRACTED_DIR/assets/icons"
    DESKTOP_SRC="$EXTRACTED_DIR/assets/folio.desktop"
fi

if [ ! -f "$BINARY_SOURCE" ]; then
    echo "Error: Binary not found at $BINARY_SOURCE"
    exit 1
fi

# 2. Install executable
echo "-> Installing binary to $BIN_DIR/folio..."
mkdir -p "$BIN_DIR"
install -m 755 "$BINARY_SOURCE" "$BIN_DIR/$APP_NAME"

# 3. Install icon assets
echo "-> Installing icon assets..."
if [ -d "$ICON_SRC_DIR" ]; then
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
fi

# 4. Install desktop entry
echo "-> Installing desktop entry to $DESKTOP_DIR/folio.desktop..."
mkdir -p "$DESKTOP_DIR"
if [ -f "$DESKTOP_SRC" ]; then
    install -m 644 "$DESKTOP_SRC" "$DESKTOP_DIR/$APP_NAME.desktop"
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
