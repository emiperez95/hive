#!/usr/bin/env bash
# Install the latest prebuilt hive binary from GitHub Releases.
#
# Usage:
#   ./install.sh              # install latest
#   ./install.sh v0.1.0       # install a specific tag
#
# Requires: curl, tar. macOS only (Apple Silicon or Intel).
# Contributors building from source: use `cargo install --path . --root ~/.local`.

set -euo pipefail

REPO="emiperez95/hive"
INSTALL_DIR="${HIVE_INSTALL_DIR:-$HOME/.local/bin}"
TAG="${1:-latest}"

# --- Platform check ---
UNAME_S=$(uname -s)
if [ "$UNAME_S" != "Darwin" ]; then
    echo "Error: hive v0.1.0 is macOS only. You are on $UNAME_S." >&2
    echo "Build from source with 'cargo install --path .' if you're on Linux." >&2
    exit 1
fi

UNAME_M=$(uname -m)
case "$UNAME_M" in
    arm64|aarch64) TARGET="aarch64-apple-darwin" ;;
    x86_64)        TARGET="x86_64-apple-darwin" ;;
    *)
        echo "Error: unsupported arch $UNAME_M. Expected arm64 or x86_64." >&2
        exit 1
        ;;
esac

ASSET="hive-${TARGET}.tar.gz"
if [ "$TAG" = "latest" ]; then
    URL="https://github.com/${REPO}/releases/latest/download/${ASSET}"
else
    URL="https://github.com/${REPO}/releases/download/${TAG}/${ASSET}"
fi

echo "Installing hive (${TARGET}, ${TAG})..."

# --- Dependency check ---
for cmd in curl tar; do
    if ! command -v "$cmd" >/dev/null 2>&1; then
        echo "Error: '$cmd' is required but not found on PATH." >&2
        exit 1
    fi
done

# --- Download ---
TMP=$(mktemp -d)
trap 'rm -rf "$TMP"' EXIT

echo "Downloading $URL"
if ! curl -fsSL "$URL" -o "$TMP/hive.tar.gz"; then
    echo "Error: download failed. Check that release '$TAG' exists at:" >&2
    echo "  https://github.com/${REPO}/releases" >&2
    exit 1
fi

tar xzf "$TMP/hive.tar.gz" -C "$TMP"
if [ ! -x "$TMP/hive" ]; then
    echo "Error: extracted tarball did not contain a 'hive' binary." >&2
    exit 1
fi

# --- Install ---
mkdir -p "$INSTALL_DIR"
mv "$TMP/hive" "$INSTALL_DIR/hive"
chmod +x "$INSTALL_DIR/hive"

# Strip the macOS quarantine bit so Gatekeeper doesn't refuse to run
# the binary for users who downloaded via a browser previously. curl
# doesn't set the bit so this is a no-op on this path, but cheap.
xattr -d com.apple.quarantine "$INSTALL_DIR/hive" 2>/dev/null || true

# --- Verify ---
if ! "$INSTALL_DIR/hive" --version >/dev/null 2>&1; then
    echo "Error: installed binary failed 'hive --version'." >&2
    echo "Binary is at $INSTALL_DIR/hive — inspect manually." >&2
    exit 1
fi

INSTALLED=$("$INSTALL_DIR/hive" --version)
echo "Installed: $INSTALLED → $INSTALL_DIR/hive"

# --- PATH check ---
case ":$PATH:" in
    *":$INSTALL_DIR:"*)
        echo
        echo "Next steps:"
        echo "  hive setup      # register Claude Code hooks"
        echo "  hive --help     # list commands"
        ;;
    *)
        echo
        echo "Warning: $INSTALL_DIR is not on your PATH."
        echo "Add this to ~/.zshrc or ~/.bashrc:"
        echo "  export PATH=\"\$HOME/.local/bin:\$PATH\""
        echo
        echo "Then run: source ~/.zshrc  (or restart your shell)"
        echo
        echo "Once PATH is set:"
        echo "  hive setup      # register Claude Code hooks"
        ;;
esac
