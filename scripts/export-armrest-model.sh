#!/usr/bin/env bash
# Export bkirwi/armrest english_ascii.tflite → ONNX → RTen for Ephemeris stroke HTR.
#
# Source model: https://github.com/bkirwi/armrest (Apache-2.0, Ben Kirwin)
#   src/english_ascii.tflite
#
# Prerequisites:
#   - curl
#   - Python 3.9+ with tf2onnx (+ tensorflow or tensorflow-cpu) and onnx
#   - optional: rten-convert on PATH
#
# Usage:
#   ./scripts/export-armrest-model.sh
#   # writes dist/armrest-htr/{model.onnx,charset.txt,meta.json,SHA256SUMS}
#   # and optionally model.rten
#
# Install on device:
#   mkdir -p ~/.local/share/ephemeris/htr-armrest
#   cp dist/armrest-htr/* ~/.local/share/ephemeris/htr-armrest/
#
# Spike note (ephemeris-ktg.9): TFLite LSTM→ONNX→RTen may fail if tf2onnx/rten
# lack ops. Preprocessing + meta.json wiring still work without a converted model.
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
OUT="${OUT_DIR:-$ROOT/dist/armrest-htr}"
PYTHON="${PYTHON:-python3}"
ARMREST_TFLITE_URL="${ARMREST_TFLITE_URL:-https://raw.githubusercontent.com/bkirwi/armrest/master/src/english_ascii.tflite}"
# sha256 of upstream english_ascii.tflite (bkirwi/armrest @ master as of spike)
EXPECTED_SHA256="${ARMREST_TFLITE_SHA256:-0cdd4e247c9f85e282cbb8aa26f4deb37ae757a05a4419aab35fa20eab388a74}"

mkdir -p "$OUT"
TFLITE="$OUT/english_ascii.tflite"

echo "Downloading armrest english_ascii.tflite…"
curl -fsSL -A "ephemeris-export-armrest" -o "$TFLITE" "$ARMREST_TFLITE_URL"

GOT_SHA="$(shasum -a 256 "$TFLITE" | awk '{print $1}')"
if [[ "$GOT_SHA" != "$EXPECTED_SHA256" ]]; then
  echo "ERROR: tflite checksum mismatch" >&2
  echo "  expected $EXPECTED_SHA256" >&2
  echo "  got      $GOT_SHA" >&2
  exit 1
fi
echo "OK checksum $GOT_SHA"

# Charset + meta from armrest ml.rs Recognizer (blank-last = CHARSET.len() = 95).
# Written even when conversion fails so the install layout is documented.
CHARSET=' 0123456789abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQRSTUVWXYZ!"#$%&'\''()*+,-./:;<=>?@[\]^_`{|}~'
printf '%s\n' "$CHARSET" >"$OUT/charset.txt"
cat >"$OUT/meta.json" <<'EOF'
{
  "features": "armrest_spline",
  "blank_index": 95,
  "charset": " 0123456789abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQRSTUVWXYZ!\"#$%&'()*+,-./:;<=>?@[\\]^_`{|}~",
  "source": "bkirwi/armrest english_ascii.tflite",
  "license": "Apache-2.0",
  "input_shape": [1, 512, 4],
  "output_shape": [1, 512, 96]
}
EOF

ONNX="$OUT/model.onnx"
echo "Converting TFLite → ONNX via tf2onnx…"
set +e
"$PYTHON" -m tf2onnx.convert --tflite "$TFLITE" --output "$ONNX" --opset 13
CONV_RC=$?
set -e
if [[ $CONV_RC -ne 0 || ! -f "$ONNX" ]]; then
  (
    cd "$OUT"
    shasum -a 256 english_ascii.tflite charset.txt meta.json >SHA256SUMS
  )
  cat <<EOF >&2
BLOCKER: tf2onnx conversion failed (exit $CONV_RC).

Known failure (ephemeris-ktg.9 spike): BiLSTM TFLite graph breaks tf2onnx
rewriter with missing node 'model/bidi_4/forward_lstm_4/zeros_1'.
Alternate tflite2onnx fails on REVERSE_V2 (bidirectional reverse).

charset.txt + meta.json + tflite checksum were still written under:
  $OUT

Install deps if missing:  pip install 'tensorflow-cpu==2.15.1' 'tf2onnx' 'onnx==1.16.2'
Keep Carbune/OnlineHTR as default until a working ONNX/RTen export exists.
EOF
  exit 2
fi

if command -v rten-convert >/dev/null 2>&1; then
  echo "Converting ONNX → RTen…"
  set +e
  rten-convert "$ONNX" "$OUT/model.rten"
  RTEN_RC=$?
  set -e
  if [[ $RTEN_RC -ne 0 ]]; then
    echo "WARNING: rten-convert failed (exit $RTEN_RC). ONNX kept; RTen load may need a newer rten." >&2
  fi
else
  echo "rten-convert not on PATH — skipping .rten (RTen can load .onnx directly)."
fi

(
  cd "$OUT"
  shasum -a 256 english_ascii.tflite model.onnx charset.txt meta.json >SHA256SUMS
  [[ -f model.rten ]] && shasum -a 256 model.rten >>SHA256SUMS
)
echo "Checksums:"
cat "$OUT/SHA256SUMS"
echo "Wrote $OUT"
