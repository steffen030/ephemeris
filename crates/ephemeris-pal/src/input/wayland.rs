//! Wayland input backend for `tablet_v2` (stylus) and `wl_touch` (fingers).
//!
//! This backend binds `zwp_tablet_manager_v2` for the stylus — pressure, tilt,
//! proximity and the tool button — and `wl_touch` for finger contacts, and
//! normalises everything to the [`InputEvent`] stream defined in
//! [`super`], per ADR ephemeris-4ea.
//!
//! # Structure
//!
//! * [`WaylandInput`] owns the [`Sender`] and a shared [`WaylandState`].
//! * The `on_*` handlers translate raw Wayland protocol callbacks into
//!   normalised [`InputEvent`]s and push them onto the channel.  They are the
//!   integration seam the Wayland dispatch loop drives; keeping them as
//!   standalone, unit-testable methods lets us validate the normalisation
//!   (pressure `[0, 1]`, tilt in degrees, touch slot bookkeeping) without a
//!   live compositor.
//! * [`WaylandInput::run`] connects to the compositor and pumps the event
//!   queue.  The compositor plumbing is not yet wired up (see the note in
//!   `run`); everything downstream of the `on_*` seam is implemented and
//!   tested.

// The `on_*` normalisation handlers, the shared state and the sender are
// driven by the Wayland compositor dispatch loop, which is not yet wired up in
// `run` (see the note there). Until it is, they are exercised only by the unit
// tests, so suppress dead-code warnings at the module level rather than
// annotating each item.
#![allow(dead_code)]

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use crossbeam_channel::{bounded, Receiver, Sender};

use super::{Input, InputError, InputEvent, PenSample, TouchSample};

/// Depth of the bounded event channel.  Large enough to absorb short bursts of
/// motion samples without dropping, small enough to apply back-pressure if a
/// consumer stalls.
const CHANNEL_CAPACITY: usize = 256;

/// Wayland tablet_v2 reports pressure as a `u32` in the range `[0, 65535]`.
const WL_PRESSURE_MAX: f32 = 65535.0;

/// Wayland input backend for `tablet_v2` and `wl_touch` support.
pub struct WaylandInput {
    /// Shared, mutable device state, guarded for access from the dispatch
    /// thread and (in tests) other threads.
    state: Arc<Mutex<WaylandState>>,
    /// Sender half of the event channel.  Held for the lifetime of the backend;
    /// dropping the backend closes the channel and signals consumers to stop.
    tx: Sender<InputEvent>,
    /// Consumer-facing receiver, cloned out via [`Input::receiver`].
    rx: Receiver<InputEvent>,
}

/// Running state of the tablet/touch devices.
///
/// Tracks the current stylus sample so that proximity/motion/pressure/tilt —
/// which arrive as independent Wayland events — can be coalesced into a single
/// [`PenSample`], and maps active touch slots to their last position so that
/// `move`/`up` events can carry coordinates.
#[derive(Debug, Default)]
struct WaylandState {
    pen_x: f32,
    pen_y: f32,
    pen_pressure: f32,
    pen_tilt: f32,
    /// Whether the pen is in the digitiser's detection range (proximity in).
    pen_in_range: bool,
    /// Whether the pen tip is currently in contact with the surface.
    pen_down: bool,
    /// Last known position of each active touch slot, keyed by slot id.
    touch_points: HashMap<u32, (f32, f32)>,
}

impl WaylandState {
    /// Snapshot the current stylus state as a [`PenSample`].
    fn pen_sample(&self) -> PenSample {
        PenSample {
            x: self.pen_x,
            y: self.pen_y,
            pressure: self.pen_pressure,
            tilt: self.pen_tilt,
            in_range: self.pen_in_range,
        }
    }
}

impl WaylandInput {
    /// Create a new Wayland input backend.
    pub fn new() -> std::result::Result<Self, Box<dyn std::error::Error>> {
        let (tx, rx) = bounded::<InputEvent>(CHANNEL_CAPACITY);
        Ok(WaylandInput {
            state: Arc::new(Mutex::new(WaylandState::default())),
            tx,
            rx,
        })
    }

    /// Send an event to the consumer, ignoring the error if the receiver has
    /// been dropped (the backend is being torn down).
    fn emit(&self, event: InputEvent) {
        // A disconnected receiver just means the consumer went away; there is
        // nothing useful to do here, so drop the event silently.
        let _ = self.tx.send(event);
    }

    /// Handle a stylus proximity change (`zwp_tablet_tool_v2::proximity_in` /
    /// `proximity_out`).
    ///
    /// Emits a [`InputEvent::Hover`] on entry and a synthetic [`InputEvent::PenUp`]
    /// if the pen leaves range while still in contact.
    fn on_pen_proximity(&self, in_range: bool) {
        let sample = {
            let mut state = self.state.lock().unwrap();
            state.pen_in_range = in_range;
            if !in_range {
                // Leaving range: any contact is implicitly lifted, pressure
                // returns to zero.
                state.pen_pressure = 0.0;
                let was_down = state.pen_down;
                state.pen_down = false;
                if was_down {
                    Some(state.pen_sample())
                } else {
                    None
                }
            } else {
                Some(state.pen_sample())
            }
        };

        match (in_range, sample) {
            (true, Some(s)) => self.emit(InputEvent::Hover { x: s.x, y: s.y }),
            (false, Some(s)) => self.emit(InputEvent::PenUp(s)),
            _ => {}
        }
    }

    /// Handle stylus motion (`zwp_tablet_tool_v2::motion`).
    ///
    /// Emits [`InputEvent::PenMove`] when in contact, otherwise
    /// [`InputEvent::Hover`].
    fn on_pen_motion(&self, x: f32, y: f32) {
        let (down, sample) = {
            let mut state = self.state.lock().unwrap();
            state.pen_x = x;
            state.pen_y = y;
            (state.pen_down, state.pen_sample())
        };
        if down {
            self.emit(InputEvent::PenMove(sample));
        } else {
            self.emit(InputEvent::Hover { x, y });
        }
    }

    /// Handle stylus pressure (`zwp_tablet_tool_v2::pressure`, `[0, 65535]`
    /// normalised to `[0.0, 1.0]`).
    ///
    /// A pressure transition through zero triggers the synthetic
    /// [`InputEvent::PenDown`] / [`InputEvent::PenUp`] pair, since tablet_v2 has
    /// no dedicated tip-contact event.
    fn on_pen_pressure(&self, pressure: u32) {
        let event = {
            let mut state = self.state.lock().unwrap();
            state.pen_pressure = (pressure as f32 / WL_PRESSURE_MAX).clamp(0.0, 1.0);
            let now_down = state.pen_pressure > 0.0;
            if now_down && !state.pen_down {
                state.pen_down = true;
                Some(InputEvent::PenDown(state.pen_sample()))
            } else if !now_down && state.pen_down {
                state.pen_down = false;
                Some(InputEvent::PenUp(state.pen_sample()))
            } else {
                None
            }
        };
        if let Some(e) = event {
            self.emit(e);
        }
    }

    /// Handle stylus tilt (`zwp_tablet_tool_v2::tilt`, degrees on X/Y axes).
    ///
    /// The two axes are combined into a single magnitude in degrees, clamped to
    /// the `[−90.0, 90.0]` range documented on [`PenSample::tilt`].
    fn on_pen_tilt(&self, tilt_x: f32, tilt_y: f32) {
        let mut state = self.state.lock().unwrap();
        state.pen_tilt = (tilt_x * tilt_x + tilt_y * tilt_y)
            .sqrt()
            .clamp(-90.0, 90.0);
    }

    /// Handle the stylus side button (`zwp_tablet_tool_v2::button`).
    fn on_pen_button(&self, pressed: bool) {
        self.emit(InputEvent::PenButton { pressed });
    }

    /// Handle a new touch contact (`wl_touch::down`).
    fn on_touch_down(&self, touch_id: u32, x: f32, y: f32) {
        {
            let mut state = self.state.lock().unwrap();
            state.touch_points.insert(touch_id, (x, y));
        }
        self.emit(InputEvent::TouchBegin(TouchSample { id: touch_id, x, y }));
    }

    /// Handle touch motion (`wl_touch::motion`).
    fn on_touch_motion(&self, touch_id: u32, x: f32, y: f32) {
        {
            let mut state = self.state.lock().unwrap();
            state.touch_points.insert(touch_id, (x, y));
        }
        self.emit(InputEvent::TouchMove(TouchSample { id: touch_id, x, y }));
    }

    /// Handle a touch lift (`wl_touch::up`).
    ///
    /// The `up` event carries no coordinates, so the last known position for
    /// the slot is reused.
    fn on_touch_up(&self, touch_id: u32) {
        let pos = {
            let mut state = self.state.lock().unwrap();
            state.touch_points.remove(&touch_id)
        };
        let (x, y) = pos.unwrap_or((0.0, 0.0));
        self.emit(InputEvent::TouchEnd(TouchSample { id: touch_id, x, y }));
    }
}

impl Default for WaylandInput {
    fn default() -> Self {
        WaylandInput::new().expect("Failed to create default WaylandInput")
    }
}

impl Input for WaylandInput {
    fn receiver(&self) -> Receiver<InputEvent> {
        self.rx.clone()
    }

    fn run(self) -> Result<(), InputError> {
        // Full backend wiring (tracked as follow-up):
        // 1. Connect to the Wayland display and get the registry.
        // 2. Bind wl_seat, zwp_tablet_manager_v2 (→ tablet_seat → tablet_tool)
        //    and wl_touch.
        // 3. Register listeners that forward protocol callbacks to the `on_*`
        //    methods above (the normalisation seam, already implemented/tested).
        // 4. Block dispatching the event queue until the connection closes,
        //    then return.
        //
        // The compositor connection is not yet established. Rather than
        // silently succeeding (which would make consumers believe a live
        // stream had ended), report that initialisation is incomplete so the
        // caller can fall back to another backend.
        Err(InputError::Init(
            "Wayland compositor connection not yet implemented".to_string(),
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Drain everything currently queued on the backend's receiver.
    fn drain(input: &WaylandInput) -> Vec<InputEvent> {
        input.receiver().try_iter().collect()
    }

    #[test]
    fn wayland_input_creation() {
        let input = WaylandInput::new().expect("should create");
        let state = input.state.lock().unwrap();
        assert!(!state.pen_in_range);
        assert!(!state.pen_down);
        assert_eq!(state.pen_pressure, 0.0);
    }

    #[test]
    fn pen_pressure_normalization() {
        let input = WaylandInput::new().expect("should create");
        input.on_pen_pressure(WL_PRESSURE_MAX as u32 / 2); // ~50%
        let state = input.state.lock().unwrap();
        assert!((state.pen_pressure - 0.5).abs() < 0.01);
    }

    #[test]
    fn pen_pressure_clamped_to_one() {
        let input = WaylandInput::new().expect("should create");
        input.on_pen_pressure(u32::MAX);
        let state = input.state.lock().unwrap();
        assert_eq!(state.pen_pressure, 1.0);
    }

    #[test]
    fn pen_pressure_emits_down_then_up() {
        let input = WaylandInput::new().expect("should create");
        input.on_pen_proximity(true);
        input.on_pen_motion(10.0, 20.0);
        input.on_pen_pressure(30000); // contact
        input.on_pen_motion(11.0, 21.0);
        input.on_pen_pressure(0); // lift
        let events = drain(&input);

        // Hover (proximity) + Hover (motion) + PenDown + PenMove + PenUp.
        assert!(matches!(events[0], InputEvent::Hover { .. }));
        assert!(events.iter().any(|e| matches!(e, InputEvent::PenDown(_))));
        assert!(events.iter().any(|e| matches!(e, InputEvent::PenMove(_))));
        assert!(matches!(events.last().unwrap(), InputEvent::PenUp(_)));
    }

    #[test]
    fn pen_move_vs_hover_depends_on_contact() {
        let input = WaylandInput::new().expect("should create");
        input.on_pen_proximity(true);
        input.on_pen_motion(1.0, 1.0); // hovering
        input.on_pen_pressure(5000); // contact -> PenDown
        input.on_pen_motion(2.0, 2.0); // drawing -> PenMove
        let events = drain(&input);
        assert!(matches!(events[0], InputEvent::Hover { .. }));
        assert!(matches!(events[1], InputEvent::Hover { .. }));
        assert!(events.iter().any(|e| matches!(e, InputEvent::PenMove(_))));
    }

    #[test]
    fn proximity_out_lifts_pen_in_contact() {
        let input = WaylandInput::new().expect("should create");
        input.on_pen_proximity(true);
        input.on_pen_pressure(20000); // in contact
        let _ = drain(&input);
        input.on_pen_proximity(false); // leaves range while drawing
        let events = drain(&input);
        assert!(matches!(events.last().unwrap(), InputEvent::PenUp(_)));
        let state = input.state.lock().unwrap();
        assert!(!state.pen_down);
        assert!(!state.pen_in_range);
        assert_eq!(state.pen_pressure, 0.0);
    }

    #[test]
    fn pen_tilt_magnitude_and_clamp() {
        let input = WaylandInput::new().expect("should create");
        input.on_pen_tilt(30.0, 40.0); // magnitude 50
        assert!((input.state.lock().unwrap().pen_tilt - 50.0).abs() < 1e-4);
        input.on_pen_tilt(80.0, 80.0); // magnitude > 90 -> clamped
        assert_eq!(input.state.lock().unwrap().pen_tilt, 90.0);
    }

    #[test]
    fn pen_button_emits_event() {
        let input = WaylandInput::new().expect("should create");
        input.on_pen_button(true);
        input.on_pen_button(false);
        let events = drain(&input);
        assert!(matches!(events[0], InputEvent::PenButton { pressed: true }));
        assert!(matches!(
            events[1],
            InputEvent::PenButton { pressed: false }
        ));
    }

    #[test]
    fn touch_lifecycle_emits_and_tracks_slots() {
        let input = WaylandInput::new().expect("should create");
        input.on_touch_down(1, 100.0, 100.0);
        input.on_touch_motion(1, 150.0, 150.0);
        input.on_touch_up(1);
        let events = drain(&input);

        assert!(matches!(
            events[0],
            InputEvent::TouchBegin(TouchSample { id: 1, .. })
        ));
        assert!(matches!(
            events[1],
            InputEvent::TouchMove(TouchSample { id: 1, .. })
        ));
        // up reuses the last tracked position (150, 150).
        assert!(matches!(
            &events[2],
            InputEvent::TouchEnd(TouchSample { id: 1, x, y }) if *x == 150.0 && *y == 150.0
        ));
        assert!(input.state.lock().unwrap().touch_points.is_empty());
    }

    #[test]
    fn run_reports_unimplemented_backend() {
        let input = WaylandInput::new().expect("should create");
        assert!(matches!(input.run(), Err(InputError::Init(_))));
    }

    #[test]
    fn receiver_shares_channel_across_threads() {
        let input = Arc::new(WaylandInput::new().expect("should create"));
        let clone = Arc::clone(&input);
        std::thread::spawn(move || {
            clone.on_touch_down(9, 50.0, 50.0);
        })
        .join()
        .unwrap();
        let events = drain(&input);
        assert!(matches!(
            events[0],
            InputEvent::TouchBegin(TouchSample { id: 9, .. })
        ));
    }
}
