#!/bin/bash
set -euo pipefail

# Install deskd — download prebuilt binary from GitHub Releases
#
# Usage:
#   curl -fsSL https://raw.githubusercontent.com/kgatilin/deskd/main/install.sh | bash
#   DESKD_VERSION=v0.2.0 curl -fsSL ... | bash

VERSION="${DESKD_VERSION:-latest}"
INSTALL_DIR="${DESKD_INSTALL_DIR:-$HOME/.local/bin}"

OS="$(uname -s | tr '[:upper:]' '[:lower:]')"
ARCH="$(uname -m)"
case "$ARCH" in
    x86_64|amd64) ARCH="amd64" ;;
    aarch64|arm64) ARCH="arm64" ;;
    *) echo "Error: unsupported architecture: $ARCH" >&2; exit 1 ;;
esac

case "$OS" in
    linux)  ARTIFACT="deskd-linux-${ARCH}" ;;
    darwin) ARTIFACT="deskd-darwin-${ARCH}" ;;
    *) echo "Error: unsupported OS: $OS" >&2; exit 1 ;;
esac

if [ "$VERSION" = "latest" ]; then
    URL="https://github.com/kgatilin/deskd/releases/latest/download/${ARTIFACT}"
else
    URL="https://github.com/kgatilin/deskd/releases/download/${VERSION}/${ARTIFACT}"
fi

echo "Installing deskd (${OS}/${ARCH}) to ${INSTALL_DIR}..."
mkdir -p "$INSTALL_DIR"

if ! curl -fsSL "$URL" -o "${INSTALL_DIR}/deskd"; then
    echo "Error: download failed. Check https://github.com/kgatilin/deskd/releases" >&2
    exit 1
fi

chmod +x "${INSTALL_DIR}/deskd"

# macOS: re-sign to avoid Gatekeeper kill
if [ "$OS" = "darwin" ]; then
    codesign --force --sign - "${INSTALL_DIR}/deskd" 2>/dev/null || true
fi

# Check PATH
if ! echo "$PATH" | grep -q "$INSTALL_DIR"; then
    echo ""
    echo "Add to your shell profile:"
    echo "  export PATH=\"${INSTALL_DIR}:\$PATH\""
fi

echo "Installed: ${INSTALL_DIR}/deskd"
"${INSTALL_DIR}/deskd" --version 2>/dev/null || true
