#!/bin/sh
# Install script for gator
# Usage: curl -fsSL https://raw.githubusercontent.com/tftio/gator/main/scripts/install.sh | bash
#
# Environment variables:
#   GATOR_VERSION  - version to install (e.g. v0.1.0). Default: latest release.
#   INSTALL_DIR    - where to put the binary. Default: ~/.local/bin or /usr/local/bin.

set -eu

REPO="tftio/gator"

main() {
    detect_platform
    resolve_version
    resolve_install_dir
    download_and_install
    printf '\ngator %s installed to %s/gator\n' "$VERSION" "$INSTALL_DIR"
    check_path
}

detect_platform() {
    OS="$(uname -s)"
    ARCH="$(uname -m)"

    case "$OS" in
        Linux)  OS_SUFFIX="linux" ;;
        Darwin) OS_SUFFIX="darwin" ;;
        *)      error "Unsupported OS: $OS" ;;
    esac

    case "$ARCH" in
        x86_64|amd64)   ARCH_SUFFIX="x86_64" ;;
        aarch64|arm64)  ARCH_SUFFIX="aarch64" ;;
        *)              error "Unsupported architecture: $ARCH" ;;
    esac

    ARTIFACT="gator-${ARCH_SUFFIX}-${OS_SUFFIX}"
}

resolve_version() {
    if [ -n "${GATOR_VERSION:-}" ]; then
        VERSION="$GATOR_VERSION"
    else
        VERSION="$(curl -fsSL "https://api.github.com/repos/${REPO}/releases/latest" \
            | grep '"tag_name"' \
            | sed -E 's/.*"tag_name": *"([^"]+)".*/\1/')" \
            || error "Failed to fetch latest release version"
    fi

    if [ -z "$VERSION" ]; then
        error "Could not determine version to install"
    fi
}

resolve_install_dir() {
    if [ -n "${INSTALL_DIR:-}" ]; then
        : # use provided value
    elif [ -d "$HOME/.local/bin" ]; then
        INSTALL_DIR="$HOME/.local/bin"
    else
        INSTALL_DIR="/usr/local/bin"
    fi

    mkdir -p "$INSTALL_DIR" 2>/dev/null || true
}

download_and_install() {
    TARBALL="${ARTIFACT}.tar.gz"
    URL="https://github.com/${REPO}/releases/download/${VERSION}/${TARBALL}"
    CHECKSUM_URL="https://github.com/${REPO}/releases/download/${VERSION}/sha256sums.txt"

    TMPDIR="$(mktemp -d)"
    trap 'rm -rf "$TMPDIR"' EXIT

    printf 'Downloading gator %s for %s/%s...\n' "$VERSION" "$OS_SUFFIX" "$ARCH_SUFFIX"
    curl -fsSL "$URL" -o "$TMPDIR/$TARBALL" \
        || error "Failed to download $URL"
    curl -fsSL "$CHECKSUM_URL" -o "$TMPDIR/sha256sums.txt" \
        || error "Failed to download checksums"

    # Verify checksum
    printf 'Verifying checksum...\n'
    EXPECTED="$(grep "$TARBALL" "$TMPDIR/sha256sums.txt" | awk '{print $1}')"
    if [ -z "$EXPECTED" ]; then
        error "No checksum found for $TARBALL in sha256sums.txt"
    fi

    if command -v sha256sum >/dev/null 2>&1; then
        ACTUAL="$(sha256sum "$TMPDIR/$TARBALL" | awk '{print $1}')"
    elif command -v shasum >/dev/null 2>&1; then
        ACTUAL="$(shasum -a 256 "$TMPDIR/$TARBALL" | awk '{print $1}')"
    else
        printf 'Warning: no sha256sum or shasum found, skipping checksum verification\n'
        ACTUAL="$EXPECTED"
    fi

    if [ "$ACTUAL" != "$EXPECTED" ]; then
        error "Checksum mismatch: expected $EXPECTED, got $ACTUAL"
    fi

    # Extract and install
    tar xzf "$TMPDIR/$TARBALL" -C "$TMPDIR"
    if [ -w "$INSTALL_DIR" ]; then
        mv "$TMPDIR/$ARTIFACT" "$INSTALL_DIR/gator"
        chmod +x "$INSTALL_DIR/gator"
    else
        printf 'Installing to %s requires elevated permissions.\n' "$INSTALL_DIR"
        sudo mv "$TMPDIR/$ARTIFACT" "$INSTALL_DIR/gator"
        sudo chmod +x "$INSTALL_DIR/gator"
    fi
}

check_path() {
    case ":$PATH:" in
        *":$INSTALL_DIR:"*) ;;
        *)
            printf '\nNote: %s is not in your PATH.\n' "$INSTALL_DIR"
            printf 'Add it with: export PATH="%s:$PATH"\n' "$INSTALL_DIR"
            ;;
    esac
}

error() {
    printf 'Error: %s\n' "$1" >&2
    exit 1
}

main
