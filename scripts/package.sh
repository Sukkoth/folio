#!/usr/bin/env bash
set -e

# Builds standalone distributions:
# 1. dist/folio_0.0.1_amd64.deb (Debian/Ubuntu package)
# 2. dist/folio-0.0.1-linux-x86_64.tar.gz (Universal tarball with install.sh)

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
VERSION=$(grep '^version' "$SCRIPT_DIR/Cargo.toml" | head -n1 | cut -d'"' -f2)
VERSION="${VERSION:-0.0.2}"
DIST_DIR="$SCRIPT_DIR/dist"

echo "=========================================="
echo " Building Folio v$VERSION Distribution"
echo "=========================================="

mkdir -p "$DIST_DIR"

# 1. Compile Release Binary
echo "-> Compiling release binary..."
if [ "${CARGO_OFFLINE:-}" = "1" ]; then
    cargo build --release --offline --manifest-path "$SCRIPT_DIR/Cargo.toml"
else
    cargo build --release --manifest-path "$SCRIPT_DIR/Cargo.toml"
fi

BINARY="$SCRIPT_DIR/target/release/folio"

# 2. Build Universal Tarball
TAR_DIR="$DIST_DIR/folio-$VERSION-linux-x86_64"
echo "-> Assembling portable tarball in $TAR_DIR..."
rm -rf "$TAR_DIR"
mkdir -p "$TAR_DIR/bin"
mkdir -p "$TAR_DIR/assets/icons/png"

cp "$BINARY" "$TAR_DIR/bin/folio"
cp "$SCRIPT_DIR/assets/folio.desktop" "$TAR_DIR/assets/"
cp "$SCRIPT_DIR/assets/icons/folio-icon-dark.svg" "$TAR_DIR/assets/icons/"
cp "$SCRIPT_DIR/assets/icons/png/"*.png "$TAR_DIR/assets/icons/png/"
cp "$SCRIPT_DIR/install.sh" "$TAR_DIR/"
cp "$SCRIPT_DIR/uninstall.sh" "$TAR_DIR/"
cp "$SCRIPT_DIR/README.md" "$TAR_DIR/" 2>/dev/null || true

cd "$DIST_DIR"
tar -czf "folio-$VERSION-linux-x86_64.tar.gz" "folio-$VERSION-linux-x86_64"
rm -rf "$TAR_DIR"
echo "-> Created $DIST_DIR/folio-$VERSION-linux-x86_64.tar.gz"

# 3. Build .deb package if dpkg-deb is available
if command -v dpkg-deb >/dev/null 2>&1; then
    echo "-> Building Debian (.deb) package..."
    DEB_ROOT="$DIST_DIR/deb_root"
    rm -rf "$DEB_ROOT"
    mkdir -p "$DEB_ROOT/DEBIAN"
    mkdir -p "$DEB_ROOT/usr/bin"
    mkdir -p "$DEB_ROOT/usr/share/applications"
    mkdir -p "$DEB_ROOT/usr/share/icons/hicolor/scalable/apps"
    mkdir -p "$DEB_ROOT/usr/share/pixmaps"

    install -m 755 "$BINARY" "$DEB_ROOT/usr/bin/folio"
    install -m 644 "$SCRIPT_DIR/assets/folio.desktop" "$DEB_ROOT/usr/share/applications/folio.desktop"
    install -m 644 "$SCRIPT_DIR/assets/icons/folio-icon-dark.svg" "$DEB_ROOT/usr/share/icons/hicolor/scalable/apps/folio.svg"
    install -m 644 "$SCRIPT_DIR/assets/icons/png/folio-512.png" "$DEB_ROOT/usr/share/pixmaps/folio.png"

    for SZ in 16 32 48 64 128 256 512; do
        mkdir -p "$DEB_ROOT/usr/share/icons/hicolor/${SZ}x${SZ}/apps"
        install -m 644 "$SCRIPT_DIR/assets/icons/png/folio-$SZ.png" "$DEB_ROOT/usr/share/icons/hicolor/${SZ}x${SZ}/apps/folio.png"
    done

    cat << EOF > "$DEB_ROOT/DEBIAN/control"
Package: folio
Version: $VERSION
Section: utils
Priority: optional
Architecture: amd64
Maintainer: Folio <support@folio.local>
Description: Fast, modern PDF reader & annotation studio
 High-performance PDF reader with smooth continuous scrolling, dual-page
 book view, rich highlights, notes, and eye-care paper themes.
EOF

    cat << 'EOF' > "$DEB_ROOT/DEBIAN/postinst"
#!/bin/sh
set -e
if command -v update-desktop-database >/dev/null 2>&1; then
    update-desktop-database /usr/share/applications 2>/dev/null || true
fi
if command -v gtk-update-icon-cache >/dev/null 2>&1; then
    gtk-update-icon-cache -f -t /usr/share/icons/hicolor 2>/dev/null || true
fi
EOF
    chmod 755 "$DEB_ROOT/DEBIAN/postinst"

    cat << 'EOF' > "$DEB_ROOT/DEBIAN/postrm"
#!/bin/sh
set -e
if command -v update-desktop-database >/dev/null 2>&1; then
    update-desktop-database /usr/share/applications 2>/dev/null || true
fi
if command -v gtk-update-icon-cache >/dev/null 2>&1; then
    gtk-update-icon-cache -f -t /usr/share/icons/hicolor 2>/dev/null || true
fi
EOF
    chmod 755 "$DEB_ROOT/DEBIAN/postrm"

    dpkg-deb --build "$DEB_ROOT" "$DIST_DIR/folio_${VERSION}_amd64.deb" >/dev/null
    rm -rf "$DEB_ROOT"
    echo "-> Created $DIST_DIR/folio_${VERSION}_amd64.deb"
fi

echo "=========================================="
echo " Packaging complete!"
echo " Packages available in: $DIST_DIR"
ls -lh "$DIST_DIR"
echo "=========================================="
