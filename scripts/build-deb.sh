#!/usr/bin/env bash
# Build an arm64 .deb from a pre-built aarch64 ephemeris binary.
#
# Usage:
#   scripts/build-deb.sh <binary-path> <version> [output-dir]
#
# Example:
#   scripts/build-deb.sh target/aarch64-unknown-linux-gnu/release/ephemeris 0.2.0 dist/
set -euo pipefail

BINARY="${1:?binary path required}"
VERSION="${2:?version required}"
OUT_DIR="${3:-dist}"
ARCH="arm64"
PKG_NAME="ephemeris"
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
STAGE="$(mktemp -d)"
trap 'rm -rf "$STAGE"' EXIT

VERSION="${VERSION#v}"

if [[ ! -f "$BINARY" ]]; then
  echo "error: binary not found: $BINARY" >&2
  exit 1
fi

DEB_ROOT="$STAGE/${PKG_NAME}_${VERSION}_${ARCH}"
mkdir -p "$DEB_ROOT/DEBIAN" "$DEB_ROOT/usr/bin"

install -m 0755 "$BINARY" "$DEB_ROOT/usr/bin/ephemeris"

sed "s/@VERSION@/${VERSION}/g" "$ROOT/packaging/debian/control.in" \
  > "$DEB_ROOT/DEBIAN/control"

# Installed-Size in KiB
INSTALLED_SIZE=$(du -sk "$DEB_ROOT/usr" | cut -f1)
echo "Installed-Size: $INSTALLED_SIZE" >> "$DEB_ROOT/DEBIAN/control"

mkdir -p "$OUT_DIR"
OUT_DEB="$(cd "$OUT_DIR" && pwd)/${PKG_NAME}_${VERSION}_${ARCH}.deb"

dpkg-deb --root-owner-group --build "$DEB_ROOT" "$OUT_DEB"
echo "Built $OUT_DEB"
