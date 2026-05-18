#!/usr/bin/env bash
# prepare_python_runtime.sh — Download Termux Python + deps, create runtime.zip
#
# Downloads Termux aarch64 packages, extracts them, and creates a ZIP file
# that PythonBridge.kt extracts at app first launch.
#
# Output: dist/python-runtime.zip
#
# Packages included:
#   python, libandroid-support, libbz2, libcrypt, libffi, liblzma,
#   ncurses, openssl, readline, sqlite, zlib

set -euo pipefail

ARCH=aarch64
REPO="https://packages.termux.dev/apt/termux-main"
STAGING=$(mktemp -d)
RUNTIME_DIR="$STAGING/runtime"
DIST_DIR="$(cd "$(dirname "$0")/.." && pwd)/dist"

mkdir -p "$DIST_DIR" "$RUNTIME_DIR"

# ── Fetch package index ────────────────────────────────────────────
PACKAGES_URL="$REPO/dists/stable/main/binary-${ARCH}/Packages.gz"
echo "==> Fetching package index for ${ARCH}..."
curl -sL "$PACKAGES_URL" | gzip -d > "$STAGING/Packages"
PKG_COUNT=$(wc -l < "$STAGING/Packages")
echo "    Index: ${PKG_COUNT} lines"

# ── Helper: find package Filename in index ─────────────────────────
pkg_filename() {
    local pkg="$1"
    local packages_file="$2"
    # Use regex match ($0 ~ p) since p contains anchors ^ and $
    awk -v p="^Package: ${pkg}$" '
        $0 ~ p { found=1 }
        found && /^Filename:/ { sub(/^Filename: */, ""); print; exit }
    ' "$packages_file"
}

# ── Helper: download and extract a .deb package ────────────────────
download_and_extract() {
    local pkg="$1"
    local filename
    filename=$(pkg_filename "$pkg" "$STAGING/Packages")
    if [ -z "$filename" ]; then
        echo "    WARNING: Package '${pkg}' not found in index, skipping"
        return 0
    fi
    local url="${REPO}/${filename}"
    local deb="${STAGING}/${pkg}.deb"
    echo "    Downloading ${pkg}..."
    wget -q "$url" -O "$deb" || { echo "    FAIL: ${pkg}"; return 0; }

    # Extract using dpkg-deb (available on ubuntu-latest)
    dpkg-deb -x "$deb" "${STAGING}/installed/" 2>/dev/null
    rm -f "$deb"
}

# ── Core packages ──────────────────────────────────────────────────
# python + all shared library dependencies for C extensions
CORE_PACKAGES=(
    python
    libandroid-support
    libbz2
    libcrypt
    libffi
    liblzma
    ncurses
    openssl
    readline
    sqlite
    zlib
)

echo ""
echo "==> Downloading and extracting ${#CORE_PACKAGES[@]} packages..."
for pkg in "${CORE_PACKAGES[@]}"; do
    download_and_extract "$pkg"
done

# ── Copy from Termux prefix to runtime dir ────────────────────────
TERMUX_PREFIX="${STAGING}/installed/data/data/com.termux/files/usr"
if [ ! -d "$TERMUX_PREFIX" ]; then
    echo "ERROR: Termux prefix not found at ${TERMUX_PREFIX}"
    echo "Contents of installed/:"
    find "${STAGING}/installed/" -maxdepth 5 -type d | head -20
    rm -rf "$STAGING"
    exit 1
fi

echo ""
echo "==> Copying runtime files..."
cp -a "$TERMUX_PREFIX/"* "$RUNTIME_DIR/"

# ── Size optimization ─────────────────────────────────────────────
echo "==> Optimizing size..."
# Remove test suites
find "$RUNTIME_DIR" -type d -name "__pycache__" -exec rm -rf {} + 2>/dev/null || true
find "$RUNTIME_DIR" -type d -name "test" -path "*/python*/test" -exec rm -rf {} + 2>/dev/null || true
find "$RUNTIME_DIR" -type d -name "tests" -path "*/python*/tests" -exec rm -rf {} + 2>/dev/null || true
find "$RUNTIME_DIR" -type d -name "idlelib" -path "*/python*/idlelib" -exec rm -rf {} + 2>/dev/null || true
find "$RUNTIME_DIR" -type d -name "tkinter" -path "*/python*/tkinter" -exec rm -rf {} + 2>/dev/null || true
find "$RUNTIME_DIR" -type d -name "turtledemo" -path "*/python*/turtledemo" -exec rm -rf {} + 2>/dev/null || true
find "$RUNTIME_DIR" -type d -name "unittest" -path "*/python*/unittest/test*" -exec rm -rf {} + 2>/dev/null || true

# Remove compiled files where source exists
find "$RUNTIME_DIR" -name "*.pyo" -delete 2>/dev/null || true

# Remove static libraries and libtool archives
find "$RUNTIME_DIR" -name "*.a" -delete 2>/dev/null || true
find "$RUNTIME_DIR" -name "*.la" -delete 2>/dev/null || true

# Remove headers (not needed at runtime)
find "$RUNTIME_DIR" -name "include" -type d -path "*/usr/include" -exec rm -rf {} + 2>/dev/null || true

# Strip debug symbols from shared libraries
find "$RUNTIME_DIR" -name "*.so" -exec strip --strip-debug {} + 2>/dev/null || true
find "$RUNTIME_DIR" -name "python3*" -type f -exec strip --strip-debug {} + 2>/dev/null || true

# ── Handle symlinks for zip compatibility ──────────────────────────
# ZIP doesn't preserve symlinks well. Copy symlink targets instead.
echo "==> Resolving symlinks for ZIP compatibility..."
find "$RUNTIME_DIR" -type l | while read -r link; do
    target=$(readlink -f "$link" 2>/dev/null || true)
    if [ -n "$target" ] && [ -f "$target" ]; then
        # Replace symlink with a copy of the target
        rm -f "$link"
        cp -a "$target" "$link"
    else
        # Broken symlink — remove it
        rm -f "$link"
    fi
done

# ── Create ZIP for Android asset extraction ───────────────────────
OUTPUT="$DIST_DIR/python-runtime.zip"
echo ""
echo "==> Creating python-runtime.zip..."
cd "$RUNTIME_DIR"
zip -qr "$OUTPUT" .
SIZE=$(du -h "$OUTPUT" | cut -f1)
FILE_COUNT=$(zipinfo -1 "$OUTPUT" | wc -l)

echo ""
echo "==========================================="
echo "  python-runtime.zip"
echo "  Size:     ${SIZE}"
echo "  Files:    ${FILE_COUNT}"
echo "  Output:   ${OUTPUT}"
echo "==========================================="

# ── Cleanup ────────────────────────────────────────────────────────
rm -rf "$STAGING"
echo "Done!"
