//! Replay recorded pen captures through the [`InkEngine`].
//!
//! Captures are JSONL files produced by `EPHEMERIS_INK_RECORD` (see
//! `ephemeris_pal::input::ink_record`): one object per line with
//! `t_us`, `kind` (`down|move|up|hover`), `x`, `y`, `pressure`, `tilt`.
//!
//! Replay feeds [`InkEngine::update`] with the **real** timestamps (µs → ms,
//! relative to the first sample) rather than the engine's synthetic 8 ms
//! clock, so One-Euro smoothing and velocity-dependent behaviour can be
//! reproduced and tuned offline from device recordings.

use ephemeris_pal::input::{InputEvent, PenSample};
use serde::{Deserialize, Serialize};

use super::engine::{InkEngine, InkUpdate};

/// Event kind in a capture line.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum RecordKind {
    Down,
    Move,
    Up,
    Hover,
}

/// One captured pen sample.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct InkRecord {
    pub t_us: u64,
    pub kind: RecordKind,
    pub x: f32,
    pub y: f32,
    #[serde(default)]
    pub pressure: f32,
    #[serde(default)]
    pub tilt: f32,
}

impl InkRecord {
    /// Convert to the [`InputEvent`] the live pipeline would have produced.
    pub fn to_event(&self) -> InputEvent {
        let sample = PenSample {
            x: self.x,
            y: self.y,
            pressure: self.pressure,
            tilt: self.tilt,
            in_range: true,
            t_us: self.t_us,
        };
        match self.kind {
            RecordKind::Down => InputEvent::PenDown(sample),
            RecordKind::Move => InputEvent::PenMove(sample),
            RecordKind::Up => InputEvent::PenUp(sample),
            RecordKind::Hover => InputEvent::Hover {
                x: self.x,
                y: self.y,
                t_us: self.t_us,
            },
        }
    }
}

/// Parse a JSONL capture. Blank lines and `#` comment lines are skipped.
pub fn parse_jsonl(text: &str) -> Result<Vec<InkRecord>, String> {
    let mut out = Vec::new();
    for (i, line) in text.lines().enumerate() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let rec: InkRecord =
            serde_json::from_str(line).map_err(|e| format!("line {}: {e}", i + 1))?;
        out.push(rec);
    }
    Ok(out)
}

/// Millisecond timestamp for a record relative to the first sample.
fn rel_ms(t_us: u64, first_us: u64) -> u32 {
    (t_us.saturating_sub(first_us) / 1000).min(u32::MAX as u64) as u32
}

/// Feed every record into `engine` with real timestamps.
///
/// Returns `(t_ms, update)` per record, where `t_ms` is the timestamp passed
/// to [`InkEngine::update`] (ms since the first record).
pub fn replay(engine: &mut InkEngine, records: &[InkRecord]) -> Vec<(u32, InkUpdate)> {
    let first = records.first().map(|r| r.t_us).unwrap_or(0);
    records
        .iter()
        .map(|r| {
            let t_ms = rel_ms(r.t_us, first);
            (t_ms, engine.update(&r.to_event(), t_ms))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ink::InkConfig;
    use crate::model::Stroke;

    const FIXTURE: &str = include_str!("../../tests/fixtures/ink/synthetic_scribble.jsonl");

    #[test]
    fn parses_fixture() {
        let recs = parse_jsonl(FIXTURE).unwrap();
        assert_eq!(
            recs.iter().filter(|r| r.kind == RecordKind::Down).count(),
            2
        );
        assert_eq!(recs.iter().filter(|r| r.kind == RecordKind::Up).count(), 2);
        assert!(recs.iter().any(|r| r.kind == RecordKind::Hover));
    }

    #[test]
    fn parse_reports_line_number() {
        let err =
            parse_jsonl("{\"t_us\":1,\"kind\":\"move\",\"x\":0,\"y\":0}\nnot json\n").unwrap_err();
        assert!(err.starts_with("line 2:"), "{err}");
    }

    #[test]
    fn burst_timestamps_are_monotonic_and_distinct() {
        let recs = parse_jsonl(FIXTURE).unwrap();
        // First stroke burst: from first `down` to first `up`.
        let start = recs
            .iter()
            .position(|r| r.kind == RecordKind::Down)
            .unwrap();
        let end = recs.iter().position(|r| r.kind == RecordKind::Up).unwrap();
        let burst = &recs[start..=end];
        assert!(burst.len() > 10);
        for w in burst.windows(2) {
            assert!(w[1].t_us > w[0].t_us, "t_us must strictly increase");
        }

        let mut engine = InkEngine::new(InkConfig::default());
        let out = replay(&mut engine, &recs);
        let burst_ms: Vec<u32> = out[start..=end].iter().map(|(t, _)| *t).collect();
        for w in burst_ms.windows(2) {
            assert!(
                w[1] > w[0],
                "engine t_ms must be strictly increasing and distinct: {burst_ms:?}"
            );
        }
        // Not the synthetic 8 ms grid: real spacing is ~4 ms.
        let span = burst_ms.last().unwrap() - burst_ms.first().unwrap();
        assert!(span < 8 * (burst.len() as u32 - 1));
    }

    #[test]
    fn replay_produces_two_strokes_with_real_timing() {
        let recs = parse_jsonl(FIXTURE).unwrap();
        let mut engine = InkEngine::new(InkConfig::default());
        let strokes: Vec<Stroke> = replay(&mut engine, &recs)
            .into_iter()
            .filter_map(|(_, u)| match u {
                InkUpdate::Finished { stroke, .. } => Some(*stroke),
                _ => None,
            })
            .collect();
        assert_eq!(strokes.len(), 2);
        // Second stroke begins after the inter-stroke gap, so t0_ms differs.
        assert!(strokes[1].t0_ms > strokes[0].t0_ms + 100);
        for s in &strokes {
            let ts: Vec<u32> = s.points.iter().map(|p| p.t_ms).collect();
            assert!(ts.windows(2).all(|w| w[1] >= w[0]), "points non-monotonic");
            assert!(ts.last() > ts.first(), "stroke has duration");
        }
    }

    #[test]
    fn stroke_t0_is_absolute_pen_down_clock() {
        let mut engine = InkEngine::new(InkConfig::default());
        let s = |x: f32, t_us| PenSample {
            x,
            y: 0.0,
            pressure: 0.5,
            tilt: 0.0,
            in_range: true,
            t_us,
        };
        engine.update(&InputEvent::PenDown(s(0.0, 0)), 5_000);
        engine.update(&InputEvent::PenMove(s(20.0, 0)), 5_010);
        match engine.update(&InputEvent::PenUp(s(40.0, 0)), 5_020) {
            InkUpdate::Finished { stroke, .. } => assert_eq!(stroke.t0_ms, 5_000),
            other => panic!("expected Finished, got {other:?}"),
        }
    }
}
