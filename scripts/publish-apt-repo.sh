#!/usr/bin/env bash
# Rebuild a GPG-signed apt repository tree for GitHub Pages.
#
# Usage:
#   scripts/publish-apt-repo.sh <repo-root> <deb-file> <codename>
#
# Environment:
#   APT_GPG_PRIVATE_KEY   Armored private key (required)
#   APT_GPG_PASSPHRASE    Passphrase for the key (optional)
#   APT_GPG_NAME          Key user-id used to select the signing key
#                         (default: apt@ephemeris.local)
#
# Layout written under <repo-root>:
#   pool/main/e/ephemeris/*.deb
#   dists/<codename>/main/binary-arm64/Packages{,.gz}
#   dists/<codename>/Release
#   dists/<codename>/InRelease
#   dists/<codename>/Release.gpg
#   ephemeris-archive-keyring.gpg
set -euo pipefail

REPO_ROOT="${1:?repo root required}"
DEB_FILE="${2:?deb file required}"
CODENAME="${3:-stable}"
COMPONENT="main"
ARCH="arm64"
GPG_NAME="${APT_GPG_NAME:-apt@ephemeris.local}"

hash_file() {
  local algo="$1" file="$2"
  case "$algo" in
    md5)    md5sum "$file" 2>/dev/null | awk '{print $1}' || md5 -q "$file" ;;
    sha1)   sha1sum "$file" 2>/dev/null | awk '{print $1}' || shasum -a 1 "$file" | awk '{print $1}' ;;
    sha256) sha256sum "$file" 2>/dev/null | awk '{print $1}' || shasum -a 256 "$file" | awk '{print $1}' ;;
    *) echo "unknown hash algo: $algo" >&2; return 1 ;;
  esac
}

append_checksums() {
  local label="$1" algo="$2"
  echo "$label"
  local f size sum rel
  for f in \
    "${COMPONENT}/binary-${ARCH}/Packages" \
    "${COMPONENT}/binary-${ARCH}/Packages.gz"
  do
    if [[ -f "$DIST_DIR/$f" ]]; then
      size=$(wc -c < "$DIST_DIR/$f" | tr -d ' ')
      sum=$(hash_file "$algo" "$DIST_DIR/$f")
      printf ' %s %8s %s\n' "$sum" "$size" "$f"
    fi
  done
}

if [[ ! -f "$DEB_FILE" ]]; then
  echo "error: deb not found: $DEB_FILE" >&2
  exit 1
fi
if [[ -z "${APT_GPG_PRIVATE_KEY:-}" ]]; then
  echo "error: APT_GPG_PRIVATE_KEY is not set" >&2
  exit 1
fi

DEB_BASENAME="$(basename "$DEB_FILE")"
POOL_DIR="$REPO_ROOT/pool/${COMPONENT}/e/ephemeris"
DIST_DIR="$REPO_ROOT/dists/${CODENAME}"
BIN_DIR="$DIST_DIR/${COMPONENT}/binary-${ARCH}"

mkdir -p "$POOL_DIR" "$BIN_DIR"

cp -f "$DEB_FILE" "$POOL_DIR/$DEB_BASENAME"

GNUPGHOME="$(mktemp -d)"
export GNUPGHOME
chmod 700 "$GNUPGHOME"
trap 'rm -rf "$GNUPGHOME"' EXIT

KEY_FILE="$GNUPGHOME/key.asc"
printf '%s\n' "$APT_GPG_PRIVATE_KEY" > "$KEY_FILE"
if [[ -n "${APT_GPG_PASSPHRASE:-}" ]]; then
  gpg --batch --yes --pinentry-mode loopback \
    --passphrase "$APT_GPG_PASSPHRASE" \
    --import "$KEY_FILE"
else
  gpg --batch --yes --import "$KEY_FILE"
fi

gpg --batch --yes --export --output "$REPO_ROOT/ephemeris-archive-keyring.gpg" "$GPG_NAME"

(
  cd "$REPO_ROOT"
  dpkg-scanpackages --multiversion --arch "$ARCH" "pool/${COMPONENT}" /dev/null \
    > "$BIN_DIR/Packages"
)
gzip -9fk "$BIN_DIR/Packages"

{
  cat <<EOF
Origin: Ephemeris
Label: Ephemeris
Suite: ${CODENAME}
Codename: ${CODENAME}
Architectures: ${ARCH}
Components: ${COMPONENT}
Description: Ephemeris apt repository
Date: $(date -u '+%a, %d %b %Y %H:%M:%S UTC')
EOF
  append_checksums "MD5Sum:" md5
  append_checksums "SHA1:" sha1
  append_checksums "SHA256:" sha256
} > "$DIST_DIR/Release"

SIGN_ARGS=(--batch --yes --pinentry-mode loopback --local-user "$GPG_NAME")
if [[ -n "${APT_GPG_PASSPHRASE:-}" ]]; then
  SIGN_ARGS+=(--passphrase "$APT_GPG_PASSPHRASE")
fi

gpg "${SIGN_ARGS[@]}" --clearsign --output "$DIST_DIR/InRelease" "$DIST_DIR/Release"
gpg "${SIGN_ARGS[@]}" --detach-sign --armor --output "$DIST_DIR/Release.gpg" "$DIST_DIR/Release"

echo "Apt repo updated at $REPO_ROOT (codename=$CODENAME, package=$DEB_BASENAME)"
