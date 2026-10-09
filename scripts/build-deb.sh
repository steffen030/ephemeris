#!/usr/bin/env bash
# Build an arm64 .deb from a pre-built aarch64 ephemeris binary.
#
# Usage:
#   scripts/build-deb.sh <binary-path> <version> [output-dir]
#
# Optional env:
#   WHISPER_BUNDLE_DIR  Pre-extracted whisper.cpp ubuntu-arm64 tree containing
#                       whisper-cli + shared libs. When unset, downloads
#                       WHISPER_VERSION from GitHub releases (needs network).
#   WHISPER_VERSION     Tag to fetch when bundling (default: v1.9.1).
#   SKIP_WHISPER=1      Do not bundle whisper-cli (OCR-only package).
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
WHISPER_VERSION="${WHISPER_VERSION:-v1.9.1}"

if [[ ! -f "$BINARY" ]]; then
  echo "error: binary not found: $BINARY" >&2
  exit 1
fi

DEB_ROOT="$STAGE/${PKG_NAME}_${VERSION}_${ARCH}"
mkdir -p "$DEB_ROOT/DEBIAN" \
         "$DEB_ROOT/usr/bin" \
         "$DEB_ROOT/usr/lib/ephemeris/whisper" \
         "$DEB_ROOT/usr/share/applications" \
         "$DEB_ROOT/usr/share/icons/hicolor/scalable/apps" \
         "$DEB_ROOT/usr/share/doc/ephemeris"

install -m 0755 "$BINARY" "$DEB_ROOT/usr/bin/ephemeris"
install -m 0644 "$ROOT/packaging/debian/ephemeris.desktop" \
  "$DEB_ROOT/usr/share/applications/ephemeris.desktop"

if [[ -f "$ROOT/packaging/debian/ephemeris.svg" ]]; then
  install -m 0644 "$ROOT/packaging/debian/ephemeris.svg" \
    "$DEB_ROOT/usr/share/icons/hicolor/scalable/apps/ephemeris.svg"
fi

# ── Bundle whisper-cli (not in Debian trixie; PineNote needs it for STT) ─────
bundle_whisper() {
  local src="${WHISPER_BUNDLE_DIR:-}"
  local fetch_dir="$STAGE/whisper-fetch"

  if [[ -z "$src" ]]; then
    mkdir -p "$fetch_dir"
    local url="https://github.com/ggml-org/whisper.cpp/releases/download/${WHISPER_VERSION}/whisper-bin-ubuntu-arm64.tar.gz"
    echo "Fetching whisper.cpp ${WHISPER_VERSION} arm64 tools…"
    curl -fsSL -o "$fetch_dir/whisper.tar.gz" "$url"
    tar xzf "$fetch_dir/whisper.tar.gz" -C "$fetch_dir"
    src="$fetch_dir/whisper-bin-ubuntu-arm64"
  fi

  if [[ ! -x "$src/whisper-cli" ]]; then
    echo "error: whisper-cli not found under $src" >&2
    exit 1
  fi

  # Runtime libs must sit next to the binary (LD_LIBRARY_PATH wrapper).
  # Preserve soname symlinks (cp -a); `install` would flatten each link into a
  # full copy and balloon the .deb.
  install -m 0755 "$src/whisper-cli" "$DEB_ROOT/usr/lib/ephemeris/whisper/whisper-cli"
  (
    cd "$src"
    # shellcheck disable=SC2086
    cp -a libwhisper.so* libggml*.so* "$DEB_ROOT/usr/lib/ephemeris/whisper/" 2>/dev/null || true
  )

  # PATH-visible wrapper so find_whisper_binary / users can run whisper-cli.
  cat > "$DEB_ROOT/usr/bin/whisper-cli" <<'WRAP'
#!/bin/sh
# Ephemeris-bundled whisper.cpp CLI (aarch64). Sets library path for .so files
# shipped under /usr/lib/ephemeris/whisper/.
DIR=/usr/lib/ephemeris/whisper
export LD_LIBRARY_PATH="$DIR${LD_LIBRARY_PATH:+:$LD_LIBRARY_PATH}"
exec "$DIR/whisper-cli" "$@"
WRAP
  chmod 0755 "$DEB_ROOT/usr/bin/whisper-cli"

  if [[ -f "$src/LICENSE" ]]; then
    install -m 0644 "$src/LICENSE" \
      "$DEB_ROOT/usr/share/doc/ephemeris/copyright-whisper.cpp"
  fi

  echo "Bundled whisper-cli from $src"
}

if [[ "${SKIP_WHISPER:-0}" != "1" ]]; then
  bundle_whisper
fi

sed "s/@VERSION@/${VERSION}/g" "$ROOT/packaging/debian/control.in" \
  > "$DEB_ROOT/DEBIAN/control"

# Installed-Size in KiB
INSTALLED_SIZE=$(du -sk "$DEB_ROOT/usr" | cut -f1)
echo "Installed-Size: $INSTALLED_SIZE" >> "$DEB_ROOT/DEBIAN/control"

mkdir -p "$OUT_DIR"
OUT_DEB="$(cd "$OUT_DIR" && pwd)/${PKG_NAME}_${VERSION}_${ARCH}.deb"

dpkg-deb --root-owner-group --build "$DEB_ROOT" "$OUT_DEB"
echo "Built $OUT_DEB"
