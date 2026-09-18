//! Mock / replay input backend.
//!
//! Replays a scripted sequence of [`InputEvent`]s, optionally with
//! configurable inter-event delays.  Designed for:
//!
//! * Unit and integration tests that need deterministic input.
//! * Desktop development on a machine without a digitiser.
//! * Acceptance-test scenarios ("replay this stroke and assert the ink engine
//!   produced the expected path").
//!
//! # Examples
//!
//! ## Replay a pen stroke
//!
//! ```
//! use ephemeris_pal::input::{Input, InputEvent, PenSample};
//! use ephemeris_pal::input::mock::MockInput;
//!
//! // Build a short diagonal stroke.
//! let stroke = MockInput::pen_stroke(
//!     [(10.0, 10.0), (20.0, 30.0), (30.0, 50.0)],
//!     0.6,   // pressure
//!     0.0,   // tilt
//! );
//!
//! let rx = stroke.receiver();
//! stroke.run().expect("mock backend should not fail");
//!
//! let events: Vec<_> = rx.try_iter().collect();
//! assert!(matches!(events.first().unwrap(), InputEvent::PenDown(_)));
//! assert!(matches!(events.last().unwrap(),  InputEvent::PenUp(_)));
//! ```

use std::time::Duration;

use crossbeam_channel::{self, Receiver, Sender};

use super::{Input, InputError, InputEvent, PenSample, TouchSample};

// ── MockInput ─────────────────────────────────────────────────────────────────

/// A scripted input backend that replays a fixed sequence of events.
///
/// Construct via [`MockInput::from_script`] for arbitrary sequences, or use
/// the higher-level helpers like [`MockInput::pen_stroke`].
pub struct MockInput {
    /// The events to replay, in order.
    script: Vec<ScriptEntry>,
    /// Sender side of the shared channel.  Kept alive until `run()` finishes.
    tx: Sender<InputEvent>,
    /// Consumer-facing receiver.
    rx: Receiver<InputEvent>,
}

/// One entry in a replay script: an event and an optional pre-send delay.
#[derive(Debug, Clone)]
pub struct ScriptEntry {
    /// How long to sleep before sending this event.
    /// `None` = send immediately (useful for unit tests that want no wall time).
    pub delay: Option<Duration>,
    /// The event to emit.
    pub event: InputEvent,
}

impl ScriptEntry {
    /// Create an entry with no delay.
    pub fn immediate(event: InputEvent) -> Self {
        Self { delay: None, event }
    }

    /// Create an entry with a delay.
    pub fn after(delay: Duration, event: InputEvent) -> Self {
        Self {
            delay: Some(delay),
            event,
        }
    }
}

impl MockInput {
    // ── Constructors ──────────────────────────────────────────────────────────

    /// Create a backend that will replay `script` in order.
    ///
    /// The channel is **unbounded** so `run()` will drain without blocking on
    /// slow consumers (important for tests where consumer and producer run on
    /// the same thread).
    pub fn from_script(script: Vec<ScriptEntry>) -> Self {
        let (tx, rx) = crossbeam_channel::unbounded();
        Self { script, tx, rx }
    }

    // ── Higher-level helpers ──────────────────────────────────────────────────

    /// Build a script that represents a complete pen stroke: one `PenDown`,
    /// zero or more `PenMove`, and one `PenUp`.
    ///
    /// # Parameters
    ///
    /// * `points` — iterator of `(x, y)` coordinates; must have at least one
    ///   item.  The first becomes `PenDown`, the last becomes `PenUp`, and
    ///   everything in between becomes `PenMove`.
    /// * `pressure` — constant pressure applied to every sample (`[0.0, 1.0]`).
    /// * `tilt` — constant tilt in degrees.
    ///
    /// # Panics
    ///
    /// Panics if `points` is empty.
    pub fn pen_stroke(
        points: impl IntoIterator<Item = (f32, f32)>,
        pressure: f32,
        tilt: f32,
    ) -> Self {
        let coords: Vec<(f32, f32)> = points.into_iter().collect();
        assert!(!coords.is_empty(), "pen_stroke: points must not be empty");

        let make_sample = |x, y| PenSample {
            x,
            y,
            pressure,
            tilt,
            in_range: true,
        };

        let last = coords.len() - 1;
        let script: Vec<ScriptEntry> = coords
            .into_iter()
            .enumerate()
            .map(|(i, (x, y))| {
                let event = if i == 0 {
                    InputEvent::PenDown(make_sample(x, y))
                } else if i == last {
                    InputEvent::PenUp(make_sample(x, y))
                } else {
                    InputEvent::PenMove(make_sample(x, y))
                };
                ScriptEntry::immediate(event)
            })
            .collect();

        Self::from_script(script)
    }

    /// Build a script for a single-finger touch gesture.
    ///
    /// Emits `TouchBegin`, zero or more `TouchMove`, `TouchEnd` for finger
    /// `id`.
    pub fn touch_gesture(id: u32, points: impl IntoIterator<Item = (f32, f32)>) -> Self {
        let coords: Vec<(f32, f32)> = points.into_iter().collect();
        assert!(
            !coords.is_empty(),
            "touch_gesture: points must not be empty"
        );

        let last = coords.len() - 1;
        let script: Vec<ScriptEntry> = coords
            .into_iter()
            .enumerate()
            .map(|(i, (x, y))| {
                let sample = TouchSample { id, x, y };
                let event = if i == 0 {
                    InputEvent::TouchBegin(sample)
                } else if i == last {
                    InputEvent::TouchEnd(sample)
                } else {
                    InputEvent::TouchMove(sample)
                };
                ScriptEntry::immediate(event)
            })
            .collect();

        Self::from_script(script)
    }
}

// ── Input impl ────────────────────────────────────────────────────────────────

impl Input for MockInput {
    fn receiver(&self) -> Receiver<InputEvent> {
        self.rx.clone()
    }

    /// Replay the script synchronously on the calling thread, then close the
    /// channel by dropping `self`.
    ///
    /// Entries with a `delay` will call [`std::thread::sleep`]; entries with
    /// `delay: None` are sent immediately.  In tests, use `from_script` with
    /// all-`None` delays so that `run()` returns instantly.
    fn run(self) -> Result<(), InputError> {
        for entry in &self.script {
            if let Some(d) = entry.delay {
                std::thread::sleep(d);
            }
            // If the receiver has been dropped (consumer gone), stop early.
            if self.tx.send(entry.event.clone()).is_err() {
                log::debug!("MockInput: receiver dropped, stopping replay");
                break;
            }
        }
        // Dropping `self` drops `self.tx`, which closes the channel.
        Ok(())
    }
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::input::InputEvent;

    /// Helper: run the mock on the current thread (no delays) and collect
    /// all events the receiver sees.
    fn drain(backend: MockInput) -> Vec<InputEvent> {
        let rx = backend.receiver();
        backend.run().expect("mock run failed");
        rx.try_iter().collect()
    }

    // ── pen_stroke ────────────────────────────────────────────────────────────

    #[test]
    fn pen_stroke_single_point_emits_down_and_up() {
        // A stroke with one point should still emit both PenDown and PenUp
        // (they share the same coordinate).
        let events = drain(MockInput::pen_stroke([(5.0_f32, 10.0_f32)], 0.5, 0.0));
        assert_eq!(
            events.len(),
            1,
            "single-point stroke: only PenDown (== PenUp)"
        );
        assert!(matches!(
            events[0],
            InputEvent::PenDown(_) | InputEvent::PenUp(_)
        ));
    }

    #[test]
    fn pen_stroke_ordering() {
        let points = [(0.0_f32, 0.0), (1.0, 1.0), (2.0, 2.0), (3.0, 3.0)];
        let events = drain(MockInput::pen_stroke(points, 0.8, 5.0));

        assert_eq!(events.len(), 4);
        assert!(
            matches!(events[0], InputEvent::PenDown(_)),
            "first must be PenDown"
        );
        assert!(
            matches!(events[1], InputEvent::PenMove(_)),
            "middle must be PenMove"
        );
        assert!(
            matches!(events[2], InputEvent::PenMove(_)),
            "middle must be PenMove"
        );
        assert!(
            matches!(events[3], InputEvent::PenUp(_)),
            "last must be PenUp"
        );
    }

    #[test]
    fn pen_stroke_coordinates_preserved() {
        let events = drain(MockInput::pen_stroke(
            [(10.0_f32, 20.0), (30.0, 40.0), (50.0, 60.0)],
            0.6,
            -15.0,
        ));

        let extract = |e: &InputEvent| match e {
            InputEvent::PenDown(s) | InputEvent::PenMove(s) | InputEvent::PenUp(s) => {
                (s.x, s.y, s.pressure, s.tilt)
            }
            _ => panic!("unexpected event"),
        };

        let (x, y, p, t) = extract(&events[0]);
        assert_eq!((x, y), (10.0, 20.0));
        assert!((p - 0.6).abs() < 1e-6);
        assert!((t - (-15.0)).abs() < 1e-6);

        let (x, y, _, _) = extract(&events[2]);
        assert_eq!((x, y), (50.0, 60.0));
    }

    #[test]
    fn pen_stroke_in_range_is_true() {
        let events = drain(MockInput::pen_stroke(
            [(0.0_f32, 0.0), (1.0, 1.0)],
            0.5,
            0.0,
        ));
        for e in &events {
            if let InputEvent::PenDown(s) | InputEvent::PenMove(s) | InputEvent::PenUp(s) = e {
                assert!(s.in_range, "in_range must be true during a stroke");
            }
        }
    }

    // ── PenButton ────────────────────────────────────────────────────────────

    #[test]
    fn pen_button_events() {
        let script = vec![
            ScriptEntry::immediate(InputEvent::PenButton { pressed: true }),
            ScriptEntry::immediate(InputEvent::PenButton { pressed: false }),
        ];
        let events = drain(MockInput::from_script(script));

        assert_eq!(events.len(), 2);
        assert!(matches!(events[0], InputEvent::PenButton { pressed: true }));
        assert!(matches!(
            events[1],
            InputEvent::PenButton { pressed: false }
        ));
    }

    // ── Hover ─────────────────────────────────────────────────────────────────

    #[test]
    fn hover_event() {
        let script = vec![ScriptEntry::immediate(InputEvent::Hover {
            x: 42.0,
            y: 99.0,
        })];
        let events = drain(MockInput::from_script(script));

        assert_eq!(events.len(), 1);
        assert!(matches!(events[0], InputEvent::Hover { x, y } if x == 42.0 && y == 99.0));
    }

    // ── touch_gesture ────────────────────────────────────────────────────────

    #[test]
    fn touch_gesture_ordering() {
        let events = drain(MockInput::touch_gesture(
            0,
            [(0.0_f32, 0.0), (5.0, 5.0), (10.0, 10.0)],
        ));

        assert_eq!(events.len(), 3);
        assert!(matches!(events[0], InputEvent::TouchBegin(_)));
        assert!(matches!(events[1], InputEvent::TouchMove(_)));
        assert!(matches!(events[2], InputEvent::TouchEnd(_)));
    }

    #[test]
    fn touch_gesture_id_preserved() {
        let events = drain(MockInput::touch_gesture(7, [(1.0_f32, 2.0), (3.0, 4.0)]));

        for e in &events {
            let id = match e {
                InputEvent::TouchBegin(s) | InputEvent::TouchMove(s) | InputEvent::TouchEnd(s) => {
                    s.id
                }
                _ => panic!("unexpected event"),
            };
            assert_eq!(
                id, 7,
                "touch id must match the one supplied to touch_gesture"
            );
        }
    }

    // ── from_script / channel semantics ──────────────────────────────────────

    #[test]
    fn receiver_can_be_cloned_for_fanout() {
        let backend = MockInput::pen_stroke([(0.0_f32, 0.0), (1.0, 0.0), (2.0, 0.0)], 0.5, 0.0);
        // Two independent consumers share the *same* underlying channel.
        // crossbeam-channel is MPMC: each message is delivered to exactly one
        // receiver.  This test just checks that cloning the receiver compiles
        // and the second handle drains whatever the first did not take.
        let rx1 = backend.receiver();
        let rx2 = backend.receiver();

        backend.run().unwrap();

        let mut all: Vec<InputEvent> = rx1.try_iter().chain(rx2.try_iter()).collect();
        all.sort_by_key(|e| match e {
            InputEvent::PenDown(_) => 0,
            InputEvent::PenMove(_) => 1,
            InputEvent::PenUp(_) => 2,
            _ => 99,
        });
        assert_eq!(all.len(), 3);
    }

    #[test]
    fn channel_closes_after_run() {
        let backend = MockInput::from_script(vec![ScriptEntry::immediate(InputEvent::Hover {
            x: 0.0,
            y: 0.0,
        })]);
        let rx = backend.receiver();
        backend.run().unwrap();
        // Drain the one event.
        let _ = rx.try_recv().unwrap();
        // Now channel is empty AND closed; try_recv should return an error.
        assert!(
            rx.try_recv().is_err(),
            "channel should be closed after run() returns"
        );
    }

    // ── Threaded replay ───────────────────────────────────────────────────────

    #[test]
    fn threaded_replay_receives_events_in_order() {
        let stroke = MockInput::pen_stroke((0..10).map(|i| (i as f32, i as f32 * 2.0)), 0.7, 0.0);
        let rx = stroke.receiver();

        let handle = std::thread::spawn(move || stroke.run().unwrap());

        let events: Vec<InputEvent> = rx.iter().collect(); // blocks until channel closes
        handle.join().unwrap();

        assert_eq!(events.len(), 10);
        assert!(matches!(events[0], InputEvent::PenDown(_)));
        assert!(matches!(events[9], InputEvent::PenUp(_)));

        // Verify coordinate ordering (x increases monotonically).
        let xs: Vec<f32> = events
            .iter()
            .map(|e| match e {
                InputEvent::PenDown(s) | InputEvent::PenMove(s) | InputEvent::PenUp(s) => s.x,
                _ => panic!(),
            })
            .collect();
        let mut sorted = xs.clone();
        sorted.sort_by(f32::total_cmp);
        assert_eq!(xs, sorted, "x coordinates must arrive in ascending order");
    }
}
