//! Pen sample recorder (`EPHEMERIS_INK_RECORD=/path/to/capture.jsonl`).
//!
//! When the env var is set, pen events are serialised as one JSON object per
//! line and written by a **background thread**; the UI thread only does a
//! non-blocking `try_send` (samples are dropped, never blocked on, if the
//! writer falls behind).
//!
//! Line format (fields in this order):
//!
//! ```json
//! {"t_us":1234567,"kind":"move","x":10.5,"y":20.0,"pressure":0.42,"tilt":0.0}
//! ```
//!
//! `kind` is one of `down`, `move`, `up`, `hover`. `t_us` is the evdev event
//! timestamp in µs (`0` = unknown). The matching parser / replayer lives in
//! `ephemeris_core::ink::replay`.

use std::fs::File;
use std::io::{BufWriter, Write};
use std::path::Path;
use std::thread::{self, JoinHandle};

use crossbeam_channel::{bounded, Sender, TrySendError};

use super::InputEvent;

/// Env var naming the JSONL output path.
pub const ENV_VAR: &str = "EPHEMERIS_INK_RECORD";

const QUEUE_CAPACITY: usize = 8192;

/// Render a pen event as a single JSON line (no trailing newline).
///
/// Returns `None` for non-pen events (buttons, touch).
pub fn event_to_json_line(ev: &InputEvent) -> Option<String> {
    let (kind, x, y, pressure, tilt, t_us) = match ev {
        InputEvent::PenDown(s) => ("down", s.x, s.y, s.pressure, s.tilt, s.t_us),
        InputEvent::PenMove(s) => ("move", s.x, s.y, s.pressure, s.tilt, s.t_us),
        InputEvent::PenUp(s) => ("up", s.x, s.y, s.pressure, s.tilt, s.t_us),
        InputEvent::Hover { x, y, t_us } => ("hover", *x, *y, 0.0, 0.0, *t_us),
        _ => return None,
    };
    Some(format!(
        r#"{{"t_us":{t_us},"kind":"{kind}","x":{},"y":{},"pressure":{},"tilt":{}}}"#,
        json_f32(x),
        json_f32(y),
        json_f32(pressure),
        json_f32(tilt),
    ))
}

/// JSON has no NaN/Inf; clamp those to `0`.
fn json_f32(v: f32) -> String {
    if v.is_finite() {
        format!("{v}")
    } else {
        "0".to_string()
    }
}

/// Handle to the background JSONL writer. Dropping it flushes and joins.
pub struct InkRecorder {
    tx: Option<Sender<String>>,
    join: Option<JoinHandle<()>>,
}

impl InkRecorder {
    /// Start a recorder if [`ENV_VAR`] is set to a non-empty path.
    pub fn from_env() -> Option<Self> {
        let path = std::env::var_os(ENV_VAR)?;
        if path.is_empty() {
            return None;
        }
        match Self::create(Path::new(&path)) {
            Ok(r) => {
                log::info!("ink recorder writing to {}", Path::new(&path).display());
                Some(r)
            }
            Err(e) => {
                log::warn!("{ENV_VAR}: cannot open {}: {e}", Path::new(&path).display());
                None
            }
        }
    }

    /// Open `path` (truncating) and spawn the writer thread.
    pub fn create(path: &Path) -> std::io::Result<Self> {
        let file = File::create(path)?;
        let (tx, rx) = bounded::<String>(QUEUE_CAPACITY);
        let join = thread::Builder::new()
            .name("ephemeris-ink-record".into())
            .spawn(move || {
                let mut out = BufWriter::new(file);
                while let Ok(line) = rx.recv() {
                    if writeln!(out, "{line}").is_err() {
                        return;
                    }
                    // Flush when idle so a crash/power-off loses little.
                    if rx.is_empty() {
                        let _ = out.flush();
                    }
                }
                let _ = out.flush();
            })?;
        Ok(Self {
            tx: Some(tx),
            join: Some(join),
        })
    }

    /// Enqueue a pen event (non-blocking; ignores non-pen events).
    pub fn record(&self, ev: &InputEvent) {
        let (Some(tx), Some(line)) = (&self.tx, event_to_json_line(ev)) else {
            return;
        };
        if let Err(TrySendError::Full(_)) = tx.try_send(line) {
            log::warn!("ink recorder queue full; dropping sample");
        }
    }
}

impl Drop for InkRecorder {
    fn drop(&mut self) {
        self.tx.take(); // close channel so the writer drains and exits
        if let Some(j) = self.join.take() {
            let _ = j.join();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::input::PenSample;

    fn s(x: f32, t_us: u64) -> PenSample {
        PenSample {
            x,
            y: 2.0,
            pressure: 0.5,
            tilt: 0.0,
            in_range: true,
            t_us,
        }
    }

    #[test]
    fn json_line_shape() {
        let line = event_to_json_line(&InputEvent::PenMove(s(1.5, 42))).unwrap();
        assert_eq!(
            line,
            r#"{"t_us":42,"kind":"move","x":1.5,"y":2,"pressure":0.5,"tilt":0}"#
        );
        assert!(event_to_json_line(&InputEvent::PenButton { pressed: true }).is_none());
    }

    #[test]
    fn writer_thread_flushes_on_drop() {
        let dir = std::env::temp_dir().join(format!("ephemeris-ink-rec-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("cap.jsonl");
        {
            let rec = InkRecorder::create(&path).unwrap();
            rec.record(&InputEvent::PenDown(s(1.0, 100)));
            rec.record(&InputEvent::Hover {
                x: 3.0,
                y: 4.0,
                t_us: 200,
            });
            rec.record(&InputEvent::PenUp(s(5.0, 300)));
        }
        let text = std::fs::read_to_string(&path).unwrap();
        let lines: Vec<_> = text.lines().collect();
        assert_eq!(lines.len(), 3);
        assert!(lines[0].contains(r#""kind":"down""#));
        assert!(lines[1].contains(r#""kind":"hover""#));
        assert!(lines[2].contains(r#""t_us":300"#));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
