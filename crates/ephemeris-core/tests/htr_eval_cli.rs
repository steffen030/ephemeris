//! `htr-eval` smoke test on the committed fixtures (ephemeris-ktg.2).
//!
//! Runs with no models installed (model dirs point at an empty temp dir) and
//! must still succeed, reporting the model-backed recognisers as `n/a`.

#![cfg(feature = "htr")]

use std::path::Path;
use std::process::Command;

fn fixtures() -> std::path::PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/htr-corpus")
}

fn run(extra: &[&str]) -> std::process::Output {
    let empty = tempfile::tempdir().unwrap();
    Command::new(env!("CARGO_BIN_EXE_htr-eval"))
        .arg(fixtures())
        .arg("--htr-model-dir")
        .arg(empty.path())
        .arg("--line-htr-model-dir")
        .arg(empty.path())
        .args(extra)
        .output()
        .expect("run htr-eval")
}

#[test]
fn table_reports_missing_models_gracefully() {
    let out = run(&[]);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let text = String::from_utf8_lossy(&out.stdout);
    assert!(text.contains("samples=3"), "{text}");
    assert!(text.contains("stroke-htr"), "{text}");
    assert!(text.contains("model missing"), "{text}");
    assert!(text.contains("cascade"), "{text}");
}

#[test]
fn json_report_is_valid() {
    let out = run(&["--json"]);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let v: serde_json::Value = serde_json::from_slice(&out.stdout).expect("valid JSON");
    assert_eq!(v["schema_version"], 1);
    assert_eq!(v["samples"], 3);
    assert_eq!(v["truth_lines"], 4);
    let recs = v["recognisers"].as_array().unwrap();
    assert_eq!(recs.len(), 4);
    let stroke = recs.iter().find(|r| r["id"] == "stroke-htr").unwrap();
    assert_eq!(stroke["available"], false);
}

#[test]
fn empty_corpus_is_an_error() {
    let dir = tempfile::tempdir().unwrap();
    let out = Command::new(env!("CARGO_BIN_EXE_htr-eval"))
        .arg(dir.path())
        .output()
        .unwrap();
    assert!(!out.status.success());
}
