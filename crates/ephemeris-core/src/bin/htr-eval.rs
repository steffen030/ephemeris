//! `htr-eval` — score every handwriting recogniser on a ground-truth corpus
//! (ephemeris-ktg.2).
//!
//! ```text
//! htr-eval [CORPUS_DIR] [--json] [--out FILE] [--ignore-case] [--cold]
//!          [--only stroke-htr,line-htr,ocr,cascade]
//!          [--htr-model-dir DIR] [--line-htr-model-dir DIR]
//!          [--ocr-langs eng+deu] [--tessdata DIR]
//! ```
//!
//! For each sample (`strokes.json` + `truth.txt`, see `docs/htr-corpus.md`) it
//! runs the stroke HTR, line-image HTR, Tesseract OCR and the cascade
//! individually and reports CER / WER / empty-rate / confidence / wall time.
//! Missing models or Tesseract are reported as `n/a`, never as a failure, so the
//! same invocation works in model-less CI. The binary is plain aarch64-clean
//! Rust: copy it to the PineNote and run it over ssh for on-device timings.

use std::path::PathBuf;
use std::time::Instant;

use ephemeris_core::htr_eval::{
    load_corpus, render_table, score_lines, spans_to_pred_lines, EnvInfo, EvalReport,
    RecogniserReport, Sample, SampleResult, ScoreOptions,
};
use ephemeris_core::model::Stroke;
use ephemeris_core::transcriber::{
    rasterize_strokes, CascadingTranscriber, LineImageHtrTranscriber, RasterOcrTranscriber,
    StrokeHtrTranscriber, TextSpan, Transcriber,
};
use ephemeris_core::Config;

const USAGE: &str = "\
Usage: htr-eval [CORPUS_DIR] [OPTIONS]

  CORPUS_DIR               corpus root (default: ./corpus); a dir holding
                           strokes.json directly is treated as one sample
  --json                   print the JSON report to stdout instead of the table
  --out FILE               also write the JSON report to FILE
  --ignore-case            compare case-insensitively
  --cold                   skip the untimed warm-up (first call includes model load)
  --only IDS               comma list of: stroke-htr,line-htr,ocr,cascade
  --htr-model-dir DIR      stroke HTR model dir (model.onnx|model.rten + charset.txt)
  --line-htr-model-dir DIR line-image HTR model dir
  --ocr-langs LANGS        Tesseract -l value (default from config, e.g. eng)
  --tessdata DIR           TESSDATA_PREFIX for Tesseract
  -h, --help               show this help

Models default to the app's locations (~/.local/share/ephemeris/{htr,line-htr})
and honour EPHEMERIS_HTR_MODEL_DIR / EPHEMERIS_LINE_HTR_MODEL_DIR.";

const ALL_IDS: [&str; 4] = ["stroke-htr", "line-htr", "ocr", "cascade"];

#[derive(Debug, Default)]
struct Args {
    corpus: Option<PathBuf>,
    json: bool,
    out: Option<PathBuf>,
    ignore_case: bool,
    cold: bool,
    only: Option<Vec<String>>,
    htr_model_dir: Option<PathBuf>,
    line_htr_model_dir: Option<PathBuf>,
    ocr_langs: Option<String>,
    tessdata: Option<PathBuf>,
}

fn parse_args(argv: &[String]) -> Result<Option<Args>, String> {
    let mut a = Args::default();
    let mut it = argv.iter();
    fn val<'a>(it: &mut std::slice::Iter<'a, String>, flag: &str) -> Result<&'a String, String> {
        it.next().ok_or_else(|| format!("{flag} needs a value"))
    }
    while let Some(arg) = it.next() {
        match arg.as_str() {
            "-h" | "--help" => return Ok(None),
            "--json" => a.json = true,
            "--ignore-case" => a.ignore_case = true,
            "--cold" => a.cold = true,
            "--out" => a.out = Some(PathBuf::from(val(&mut it, "--out")?)),
            "--only" => {
                let ids: Vec<String> = val(&mut it, "--only")?
                    .split(',')
                    .map(|s| s.trim().to_string())
                    .filter(|s| !s.is_empty())
                    .collect();
                for id in &ids {
                    if !ALL_IDS.contains(&id.as_str()) {
                        return Err(format!("unknown recogniser {id:?} (one of {ALL_IDS:?})"));
                    }
                }
                a.only = Some(ids);
            }
            "--htr-model-dir" => {
                a.htr_model_dir = Some(PathBuf::from(val(&mut it, "--htr-model-dir")?))
            }
            "--line-htr-model-dir" => {
                a.line_htr_model_dir = Some(PathBuf::from(val(&mut it, "--line-htr-model-dir")?))
            }
            "--ocr-langs" => a.ocr_langs = Some(val(&mut it, "--ocr-langs")?.clone()),
            "--tessdata" => a.tessdata = Some(PathBuf::from(val(&mut it, "--tessdata")?)),
            s if s.starts_with('-') => return Err(format!("unknown option {s}")),
            s => {
                if a.corpus.is_some() {
                    return Err("only one CORPUS_DIR may be given".into());
                }
                a.corpus = Some(PathBuf::from(s));
            }
        }
    }
    Ok(Some(a))
}

fn data_dir(leaf: &str) -> PathBuf {
    let home = std::env::var("HOME").unwrap_or_else(|_| ".".to_string());
    let base = std::env::var("XDG_DATA_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from(home).join(".local").join("share"));
    base.join("ephemeris").join(leaf)
}

type RunFn<'a> =
    Box<dyn Fn(&[Stroke], u32, u32, &[u8]) -> ephemeris_core::Result<Vec<TextSpan>> + 'a>;

struct Recogniser<'a> {
    id: &'static str,
    /// `Err(reason)` when it cannot run.
    availability: Result<(), String>,
    run: RunFn<'a>,
}

fn model_status(available: bool, dir: &Option<PathBuf>, what: &str) -> Result<(), String> {
    if available {
        Ok(())
    } else {
        Err(format!(
            "model missing: {what} in {}",
            dir.as_ref()
                .map(|d| d.display().to_string())
                .unwrap_or_else(|| "<unset>".into())
        ))
    }
}

fn evaluate(rec: &Recogniser<'_>, samples: &[Sample], opts: ScoreOptions) -> Vec<SampleResult> {
    samples
        .iter()
        .map(|s| {
            let (w, h, gray) = rasterize_strokes(&s.strokes);
            let t0 = Instant::now();
            let res = (rec.run)(&s.strokes, w, h, &gray);
            let wall_ms = t0.elapsed().as_secs_f64() * 1000.0;
            let (spans, error) = match res {
                Ok(spans) => (spans, None),
                Err(e) => (Vec::new(), Some(e.to_string())),
            };
            let pred = spans_to_pred_lines(&spans);
            let (lines, extra_pred_lines) = score_lines(&s.truth, &pred, opts);
            SampleResult {
                sample: s.name.clone(),
                lang: s.meta.lang.clone(),
                style: s.meta.style.clone(),
                wall_ms,
                error,
                extra_pred_lines,
                lines,
            }
        })
        .collect()
}

fn run(args: Args) -> Result<(), String> {
    let corpus_dir = args
        .corpus
        .clone()
        .unwrap_or_else(|| PathBuf::from("corpus"));
    let (samples, load_errors) = load_corpus(&corpus_dir)?;
    if samples.is_empty() {
        let mut msg = format!("no samples found under {}", corpus_dir.display());
        for e in &load_errors {
            msg.push_str(&format!("\n  {e}"));
        }
        return Err(msg);
    }

    let config = Config::load().unwrap_or_default();
    let htr_dir = Some(
        args.htr_model_dir
            .clone()
            .or_else(|| config.htr_model_dir.clone())
            .unwrap_or_else(|| data_dir("htr")),
    );
    let line_dir = Some(
        args.line_htr_model_dir
            .clone()
            .or_else(|| config.line_htr_model_dir.clone())
            .unwrap_or_else(|| data_dir("line-htr")),
    );
    let ocr_langs = args
        .ocr_langs
        .clone()
        .unwrap_or_else(|| config.ocr_languages.clone());
    let tessdata = args
        .tessdata
        .clone()
        .or_else(|| config.ocr_tessdata.clone());

    // Evaluation forces every recogniser on regardless of app config switches.
    let stroke = StrokeHtrTranscriber::from_parts(true, htr_dir.clone());
    let line = LineImageHtrTranscriber::from_parts(true, line_dir.clone());
    let ocr = RasterOcrTranscriber::from_parts(true, ocr_langs.clone(), tessdata);
    let cascade = CascadingTranscriber::with_line_htr(stroke.clone(), line.clone(), ocr.clone());

    let stroke_status = model_status(stroke.model_available(), &htr_dir, "model.onnx|model.rten");
    let line_status = model_status(line.model_available(), &line_dir, "model.onnx|model.rten");
    let ocr_status = if ocr.tesseract_available() {
        Ok(())
    } else if cfg!(feature = "ocr") {
        Err("tesseract not found on PATH".to_string())
    } else {
        Err("built without the `ocr` feature (cargo run --features htr,fs,ocr)".to_string())
    };
    let cascade_status = if stroke_status.is_ok() || line_status.is_ok() || ocr_status.is_ok() {
        Ok(())
    } else {
        Err("no component available (cascade would return nothing)".to_string())
    };

    let recognisers: Vec<Recogniser<'_>> = vec![
        Recogniser {
            id: "stroke-htr",
            availability: stroke_status,
            run: Box::new(|strokes, _, _, _| stroke.transcribe(strokes)),
        },
        Recogniser {
            id: "line-htr",
            availability: line_status,
            run: Box::new(|_, w, h, gray| line.transcribe_raster(w, h, gray)),
        },
        Recogniser {
            id: "ocr",
            availability: ocr_status,
            run: Box::new(|_, w, h, gray| ocr.transcribe_raster(w, h, gray)),
        },
        Recogniser {
            id: "cascade",
            availability: cascade_status,
            run: Box::new(|strokes, w, h, gray| cascade.transcribe_page(strokes, w, h, gray)),
        },
    ];

    let opts = ScoreOptions {
        ignore_case: args.ignore_case,
    };
    let mut reports = Vec::new();
    for rec in &recognisers {
        if let Some(only) = &args.only {
            if !only.iter().any(|o| o == rec.id) {
                continue;
            }
        }
        if let Err(reason) = &rec.availability {
            reports.push(RecogniserReport::unavailable(rec.id, reason.clone()));
            continue;
        }
        if !args.cold {
            // Untimed pass so model load / session init is not billed to sample 1.
            let first = &samples[0];
            let (w, h, gray) = rasterize_strokes(&first.strokes);
            let _ = (rec.run)(&first.strokes, w, h, &gray);
        }
        reports.push(RecogniserReport::from_results(
            rec.id,
            evaluate(rec, &samples, opts),
        ));
    }

    let mut features = vec![];
    if cfg!(feature = "htr") {
        features.push("htr".to_string());
    }
    if cfg!(feature = "fs") {
        features.push("fs".to_string());
    }
    if cfg!(feature = "ocr") {
        features.push("ocr".to_string());
    }
    let report = EvalReport {
        schema_version: 1,
        corpus: corpus_dir.display().to_string(),
        samples: samples.len(),
        truth_lines: samples.iter().map(|s| s.truth.len()).sum(),
        env: EnvInfo {
            arch: std::env::consts::ARCH.to_string(),
            os: std::env::consts::OS.to_string(),
            version: env!("CARGO_PKG_VERSION").to_string(),
            features,
            htr_model_dir: htr_dir.map(|d| d.display().to_string()),
            line_htr_model_dir: line_dir.map(|d| d.display().to_string()),
            ocr_languages: ocr_langs,
            ignore_case: args.ignore_case,
            warmup: !args.cold,
        },
        load_errors,
        recognisers: reports,
    };

    let json = serde_json::to_string_pretty(&report).map_err(|e| e.to_string())?;
    if let Some(path) = &args.out {
        std::fs::write(path, &json).map_err(|e| format!("{}: {e}", path.display()))?;
    }
    if args.json {
        println!("{json}");
    } else {
        print!("{}", render_table(&report));
    }
    Ok(())
}

fn main() {
    let argv: Vec<String> = std::env::args().skip(1).collect();
    let args = match parse_args(&argv) {
        Ok(Some(a)) => a,
        Ok(None) => {
            println!("{USAGE}");
            return;
        }
        Err(e) => {
            eprintln!("htr-eval: {e}\n\n{USAGE}");
            std::process::exit(2);
        }
    };
    if let Err(e) = run(args) {
        eprintln!("htr-eval: {e}");
        std::process::exit(1);
    }
}
