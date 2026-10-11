#!/usr/bin/env bash
# Export Carbune/OnlineHTR (IAM-OnDB) weights to ONNX for Ephemeris stroke HTR.
#
# Prerequisites:
#   - Clone https://github.com/PellelNitram/OnlineHTR and place the
#     dataIAMOnDB_featuresLinInterpol20DxDyDtN_decoderGreedy checkpoint under
#     models/.../checkpoints/*.ckpt (plus alphabet.json beside it)
#   - Python 3.10+ with numpy + onnx (torch NOT required)
#
# Usage:
#   ONLINE_HTR_ROOT=/path/to/OnlineHTR ./scripts/export-htr-model.sh
#   # writes dist/htr/model.onnx + charset.txt + dist/ephemeris-htr-v1.zip
#
# Install on device:
#   mkdir -p ~/.local/share/ephemeris/htr
#   cp dist/htr/model.onnx dist/htr/charset.txt ~/.local/share/ephemeris/htr/
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
OUT="${OUT_DIR:-$ROOT/dist/htr}"
ONLINE_HTR_ROOT="${ONLINE_HTR_ROOT:-}"
PYTHON="${PYTHON:-python3}"

mkdir -p "$OUT"

if [[ -z "$ONLINE_HTR_ROOT" || ! -d "$ONLINE_HTR_ROOT" ]]; then
  cat <<EOF
ONLINE_HTR_ROOT must point at a PellelNitram/OnlineHTR checkout with trained weights.

Example:
  git clone https://github.com/PellelNitram/OnlineHTR
  # download weights into models/dataIAMOnDB_featuresLinInterpol20DxDyDtN_decoderGreedy/
  ONLINE_HTR_ROOT=\$PWD/OnlineHTR $0
EOF
  exit 1
fi

# Locate alphabet.json + checkpoints (layout varies by OnlineHTR zip).
ALPHABET="$(find "$ONLINE_HTR_ROOT/models" -name alphabet.json 2>/dev/null | head -1 || true)"
CKPT_DIR="$(find "$ONLINE_HTR_ROOT/models" -type d -name checkpoints 2>/dev/null | head -1 || true)"
if [[ -z "$CKPT_DIR" ]]; then
  # Flat layout: models/<name>/*.ckpt
  CKPT_DIR="$(find "$ONLINE_HTR_ROOT/models" -name '*.ckpt' -exec dirname {} \; 2>/dev/null | head -1 || true)"
fi
if [[ -z "$CKPT_DIR" || -z "$ALPHABET" ]]; then
  echo "Could not find checkpoints/ and alphabet.json under $ONLINE_HTR_ROOT/models" >&2
  exit 1
fi

export HTR_CKPT_DIR="$CKPT_DIR"
export HTR_OUT_DIR="$OUT"
export HTR_ALPHABET="$ALPHABET"
"$PYTHON" "$ROOT/scripts/export_htr_onnx_notorch.py"

# Optional: convert to RTen format when rten-convert is on PATH
if command -v rten-convert >/dev/null 2>&1; then
  rten-convert "$OUT/model.onnx" "$OUT/model.rten"
fi

SHA="$OUT/SHA256SUMS"
(
  cd "$OUT"
  shasum -a 256 model.onnx charset.txt >SHA256SUMS
  [[ -f meta.json ]] && shasum -a 256 meta.json >>SHA256SUMS
  [[ -f model.rten ]] && shasum -a 256 model.rten >>SHA256SUMS
)
echo "Checksums:"
cat "$SHA"

ZIP="$ROOT/dist/ephemeris-htr-v1.zip"
rm -f "$ZIP"
mkdir -p "$ROOT/dist"
(
  cd "$OUT"
  zip -q "$ZIP" model.onnx charset.txt SHA256SUMS
  [[ -f meta.json ]] && zip -q "$ZIP" meta.json
  [[ -f model.rten ]] && zip -q "$ZIP" model.rten
)
echo "Packaged $ZIP — upload as GitHub Release asset htr-model-v1/ephemeris-htr-v1.zip"
