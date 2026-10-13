# HTR evaluation corpus (PineNote capture guide)

"Recognition is useless" can't be fixed without a number. This corpus is your
own PineNote handwriting plus typed ground truth; the `htr-eval` CLI scores
every recogniser against it (bead `ephemeris-ktg.2`).

## Layout

```text
corpus/                       # personal data — git-ignored (/corpus/)
  2026-10-13-sentences-de/
    strokes.json              # the app's strokes_{note}_p{page}.json, unchanged
    truth.txt                 # one line per handwritten line, top-to-bottom
    meta.json                 # optional: {"lang": "de", "style": "fast"}
```

* `strokes.json` — exactly the `StrokePage` JSON the app writes
  (`{"strokes":[…]}`), copied verbatim.
* `truth.txt` — what you wrote, **one text line per handwritten line,
  top-to-bottom**. Blank lines are ignored. Type exactly what is on the page
  (case, umlauts, punctuation): scoring is exact unless `--ignore-case`.
  Scoring aligns predicted line *i* to truth line *i*, so keep one
  handwritten line per `truth.txt` line and don't wrap or split words across
  lines.
* `meta.json` (optional) — `lang`: `"de"` | `"en"`; `style`:
  `"neat"` | `"fast"` | `"cursive"`. Echoed per sample in the JSON report.

The personal corpus stays out of git. Only three tiny synthetic fixtures are
committed, for CI, in `crates/ephemeris-core/tests/fixtures/htr-corpus/`.

## Capturing on the PineNote

Aim for ~30 pages. Mix, per page, 3–8 lines of:

| Dimension | Cover |
|-----------|-------|
| Language  | roughly half DE, half EN (plus a few mixed lines) |
| Style     | neat (print-like), fast (normal note-taking), some cursive |
| Content   | single words, short sentences, numbers & dates (`13.10.2026`, `10:00`), task-like lines (`Call Anna tomorrow 10:00`, `Milch kaufen`), punctuation and umlauts |

Tips:

* One page ≈ one `sample` directory. Write on straight, well separated lines —
  line segmentation is not what you are measuring here (yet).
* Write like you actually do; don't "perform" for the recogniser.
* Don't erase mid-page if you can avoid it; erased strokes are ignored but make
  the truth harder to type out.
* Vary pen speed and slant across pages, not within a single line.

## Copying the data off the device

Stroke pages live in the app data dir (default `~/.local/share/ephemeris/`) as
`strokes_{note_id}_p{page}.json`.

```bash
# on your computer
mkdir -p corpus/2026-10-13-sentences-de
scp pinenote:.local/share/ephemeris/strokes_12_p0.json \
    corpus/2026-10-13-sentences-de/strokes.json
$EDITOR corpus/2026-10-13-sentences-de/truth.txt   # type the truth
echo '{"lang":"de","style":"fast"}' > corpus/2026-10-13-sentences-de/meta.json
```

## Running the evaluation

```bash
# desktop (models under ~/.local/share/ephemeris/{htr,line-htr}; add ocr for Tesseract)
cargo run --release -p ephemeris-core --features htr,fs,ocr --bin htr-eval -- corpus/

# machine-readable report
cargo run --release -p ephemeris-core --features htr,fs,ocr --bin htr-eval -- corpus/ --json > baseline.json
# or: table on stdout + JSON on disk
cargo run --release -p ephemeris-core --features htr,fs,ocr --bin htr-eval -- corpus/ --out baseline.json
```

Useful flags: `--only stroke-htr,cascade`, `--ignore-case`,
`--htr-model-dir`, `--line-htr-model-dir`, `--ocr-langs eng+deu`, `--tessdata`,
`--cold` (include model-load time in sample 1). See `htr-eval --help`.

For each sample it runs **stroke-htr**, **line-htr**, **ocr** (Tesseract) and the
**cascade** individually. A recogniser whose model/binary is missing is shown as
`n/a (reason)` rather than failing, so the same command works in CI with no
models.

### Metrics

| Column | Meaning |
|--------|---------|
| `CER%` | micro character error rate: total Levenshtein edits / total truth chars |
| `lineCER%` | mean of per-line CER (long lines count the same as short ones) |
| `WER%` | micro word error rate (whitespace-separated words) |
| `empty%` | truth lines for which the recogniser returned nothing |
| `conf` | mean confidence of non-empty lines that report one |
| `calib` | `conf − mean(1 − CER)` on those lines; `> 0` = over-confident |
| `ms/line`, `total_ms` | wall time per truth line / total, after an untimed warm-up |

Missing predicted lines count as empty (CER 100%); surplus predicted lines are
counted in `extra_pred_lines` (JSON only). Per-line truth/prediction/CER/WER/
confidence are in the JSON report for debugging.

> The recognisers are fed a raster rendered from the strokes (the app feeds the
> screen raster), so the line-HTR / OCR numbers here track the vector-rendered
> path planned in `ephemeris-ktg.10`, not pixel-identical on-device ink.

## On-device timing (aarch64)

The same binary runs on the PineNote: build for aarch64 (or on-device), copy
`htr-eval` and the `corpus/` directory over, then

```bash
ssh pinenote ./htr-eval corpus/ --out baseline-pinenote.json
```

The report records `arch`, `os`, Cargo features and model dirs so desktop and
on-device timings are not confused.

## Recording a baseline

Run on the device with the full model set before any HTR fix lands, and paste
the table (and keep the JSON) in the relevant bead notes. Baseline for the
epic is the commit `48411a7`.
