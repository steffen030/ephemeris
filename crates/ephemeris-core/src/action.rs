//! Action system: pen-button modifier → contextual actions.
//!
//! Per ADR ephemeris-4ea, the pen side button is a *modifier*: while it is held
//! a pen tap dispatches a contextual [`Action`] (open the action ring, drop a
//! calendar/task item at the tapped point, switch tools, …) **instead of**
//! drawing ink.
//!
//! # Where this sits
//!
//! ```text
//!   InputEvent stream ──▶ ActionRouter ──▶ RouterOutput
//!                              │  (button up)  └─▶ Ink(InkUpdate)   — forwarded to InkEngine
//!                              └─ (button held) ──▶ Action(Action)  — no ink drawn
//! ```
//!
//! The router is the single seam between raw [`InputEvent`]s and the
//! [`InkEngine`](crate::ink::InkEngine). It **owns** an `InkEngine` and forwards
//! pen events to it when the button is up; when the button is held it interprets
//! a pen tap as an [`Action`] and emits it, drawing no ink.
//!
//! # Design: return actions, don't call back
//!
//! The router *returns* emitted [`Action`]s ([`RouterOutput::Action`]) rather
//! than invoking registered callbacks. Rationale:
//!
//! * **Purity / testability.** `ephemeris-core` is headless. A value-returning
//!   API is trivially unit-testable and free of interior mutability, `dyn`
//!   dispatch, or borrow gymnastics that a handler-map would impose on the core.
//! * **Ownership stays with the app.** *What* an action means (which view is
//!   focused, how a calendar entry materialises) is a UI concern. The app layer
//!   pattern-matches the returned [`Action`] and routes it to the focused view —
//!   exactly the dispatch the ADR calls for, kept out of the pure core.
//! * **Coordinates travel with the action.** Point-anchored actions carry the
//!   [`CanvasPoint`] of the tap, so the app has everything it needs without
//!   querying router state.
//!
//! A pluggable *mapping* from a tap to an action is still supported via the
//! [`ActionMap`] trait (default: [`DefaultActionMap`]), so bindings can be
//! customised without changing the routing logic.

use ephemeris_pal::input::{InputEvent, PenSample};

use crate::ink::{InkEngine, InkUpdate};
use crate::model::Tool;

/// A point on the canvas, in logical pixels (device-independent units), matching
/// [`PenSample`] coordinates.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct CanvasPoint {
    pub x: f32,
    pub y: f32,
}

impl CanvasPoint {
    pub fn new(x: f32, y: f32) -> Self {
        CanvasPoint { x, y }
    }
}

impl From<&PenSample> for CanvasPoint {
    fn from(s: &PenSample) -> Self {
        CanvasPoint { x: s.x, y: s.y }
    }
}

/// A contextual action triggered while the pen side button is held.
///
/// The initial set is grounded in ADR ephemeris-4ea ("opens action ring, or
/// materializes a calendar/task item at the tapped point") and the epic's
/// in-note action items. Point-anchored variants carry the [`CanvasPoint`] of
/// the tap so the app can materialise the item exactly where the pen touched.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Action {
    /// Open the radial action ring at the tapped point (capture note / audio /
    /// calendar / task). See ephemeris-idd.6.
    OpenActionRing { at: CanvasPoint },
    /// Materialise a new calendar entry anchored at the tapped point.
    NewCalendarEntry { at: CanvasPoint },
    /// Materialise a new task anchored at the tapped point.
    NewTask { at: CanvasPoint },
    /// Switch the active drawing tool.
    SwitchTool { tool: Tool },
    /// Undo the last operation.
    Undo,
    /// Redo the last undone operation.
    Redo,
}

/// Maps a pen tap (while the button is held) to an [`Action`].
///
/// The default binding ([`DefaultActionMap`]) emits [`Action::OpenActionRing`]
/// at the tap point — the canonical entry point of the ADR's action mode. Apps
/// can supply their own mapping (e.g. gesture recognition) without touching the
/// router's state machine.
pub trait ActionMap {
    /// Interpret a tap at `at` as an action, or `None` to swallow it.
    fn tap_to_action(&self, at: CanvasPoint) -> Option<Action>;
}

/// Default action mapping: a button-held tap opens the action ring at the point.
#[derive(Debug, Clone, Copy, Default)]
pub struct DefaultActionMap;

impl DefaultActionMap {
    pub fn new() -> Self {
        DefaultActionMap
    }
}

impl ActionMap for DefaultActionMap {
    fn tap_to_action(&self, at: CanvasPoint) -> Option<Action> {
        Some(Action::OpenActionRing { at })
    }
}

/// The result of feeding one [`InputEvent`] to an [`ActionRouter`].
#[derive(Debug, Clone, PartialEq)]
pub enum RouterOutput {
    /// The event was routed to the ink engine; carries its [`InkUpdate`].
    Ink(InkUpdate),
    /// The event was interpreted as a contextual [`Action`] (no ink drawn). The
    /// caller dispatches this to the focused view.
    Action(Action),
    /// The event produced neither ink nor an action (button state change,
    /// swallowed tap, hover, touch, …).
    Idle,
}

/// Routes raw [`InputEvent`]s to either the ink engine or the action system,
/// gated by the pen-button-held modifier (ADR ephemeris-4ea).
///
/// * Button **up**: pen events are forwarded to the owned [`InkEngine`] and the
///   resulting [`InkUpdate`] is returned as [`RouterOutput::Ink`].
/// * Button **held**: a pen tap ([`PenDown`](InputEvent::PenDown) …
///   [`PenUp`](InputEvent::PenUp)) is interpreted via the [`ActionMap`] and
///   returned as [`RouterOutput::Action`]; **no stroke is drawn**.
///
/// The modifier is tracked purely from
/// [`PenButton`](InputEvent::PenButton) events, so a `PenUp` that arrives while
/// the button is up can never leak into action mode.
pub struct ActionRouter<M = DefaultActionMap> {
    engine: InkEngine,
    map: M,
    /// `true` while the pen side button is held.
    button_held: bool,
    /// The `PenDown` point captured while the button is held, awaiting a
    /// `PenUp` to complete the tap. `None` when no action-mode press is active.
    pending_tap: Option<CanvasPoint>,
    /// Monotonic fallback clock (ms) for engine updates lacking a timestamp.
    clock_ms: u32,
}

impl ActionRouter<DefaultActionMap> {
    /// Create a router around `engine` using the [`DefaultActionMap`].
    pub fn new(engine: InkEngine) -> Self {
        ActionRouter::with_map(engine, DefaultActionMap)
    }
}

impl<M: ActionMap> ActionRouter<M> {
    /// Create a router around `engine` with a custom [`ActionMap`].
    pub fn with_map(engine: InkEngine, map: M) -> Self {
        Self {
            engine,
            map,
            button_held: false,
            pending_tap: None,
            clock_ms: 0,
        }
    }

    /// Whether the pen side button is currently held (action mode active).
    pub fn is_action_mode(&self) -> bool {
        self.button_held
    }

    /// Read-only access to the owned ink engine.
    pub fn engine(&self) -> &InkEngine {
        &self.engine
    }

    /// Mutable access to the owned ink engine (e.g. to change tool/colour).
    pub fn engine_mut(&mut self) -> &mut InkEngine {
        &mut self.engine
    }

    /// Feed one input event using the router's internal monotonic clock
    /// (advances by 8 ms per pen event ≈ 120 Hz), mirroring
    /// [`InkEngine::update_now`].
    pub fn handle(&mut self, event: &InputEvent) -> RouterOutput {
        let t = self.clock_ms;
        self.clock_ms = self.clock_ms.wrapping_add(8);
        self.handle_at(event, t)
    }

    /// Feed one input event with an explicit timestamp `t_ms`.
    pub fn handle_at(&mut self, event: &InputEvent, t_ms: u32) -> RouterOutput {
        match event {
            // The button is the modifier: track it, never draw or act on it.
            InputEvent::PenButton { pressed } => {
                self.button_held = *pressed;
                // Releasing mid-press abandons any pending action tap.
                if !*pressed {
                    self.pending_tap = None;
                }
                RouterOutput::Idle
            }

            // In action mode, a pen tap becomes an Action instead of ink. We
            // capture the PenDown point and fire on PenUp so the action carries
            // the release location (the "tapped point").
            InputEvent::PenDown(s) if self.button_held => {
                self.pending_tap = Some(CanvasPoint::from(s));
                RouterOutput::Idle
            }
            InputEvent::PenMove(_) if self.button_held => {
                // Movement during an action press is ignored (no ink, no drag
                // gesture yet — reserved for future gesture recognition).
                RouterOutput::Idle
            }
            InputEvent::PenUp(s) if self.button_held => {
                // Complete the tap. Prefer the release point; fall back to the
                // captured down point if we somehow missed the PenDown.
                let at = CanvasPoint::from(s);
                self.pending_tap = None;
                match self.map.tap_to_action(at) {
                    Some(action) => RouterOutput::Action(action),
                    None => RouterOutput::Idle,
                }
            }

            // Button up: everything flows to the ink engine as normal.
            _ => RouterOutput::Ink(self.engine.update(event, t_ms)),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ink::InkConfig;

    fn sample(x: f32, y: f32) -> PenSample {
        PenSample {
            x,
            y,
            pressure: 0.5,
            tilt: 0.0,
            in_range: true,
        }
    }

    fn router() -> ActionRouter {
        ActionRouter::new(InkEngine::new(InkConfig::default()))
    }

    #[test]
    fn default_map_taps_open_action_ring() {
        let map = DefaultActionMap::new();
        assert_eq!(
            map.tap_to_action(CanvasPoint::new(3.0, 4.0)),
            Some(Action::OpenActionRing {
                at: CanvasPoint::new(3.0, 4.0)
            })
        );
    }

    #[test]
    fn button_press_release_toggles_modifier() {
        let mut r = router();
        assert!(!r.is_action_mode());
        assert_eq!(
            r.handle(&InputEvent::PenButton { pressed: true }),
            RouterOutput::Idle
        );
        assert!(r.is_action_mode());
        assert_eq!(
            r.handle(&InputEvent::PenButton { pressed: false }),
            RouterOutput::Idle
        );
        assert!(!r.is_action_mode());
    }

    #[test]
    fn button_held_tap_yields_action_not_ink() {
        let mut r = router();
        r.handle(&InputEvent::PenButton { pressed: true });

        // PenDown while held is captured silently (no stroke started).
        assert_eq!(
            r.handle(&InputEvent::PenDown(sample(10.0, 20.0))),
            RouterOutput::Idle
        );
        assert!(
            !r.engine().is_drawing(),
            "no stroke may start in action mode"
        );

        // PenUp completes the tap → an Action carrying the tapped point.
        let out = r.handle(&InputEvent::PenUp(sample(11.0, 21.0)));
        assert_eq!(
            out,
            RouterOutput::Action(Action::OpenActionRing {
                at: CanvasPoint::new(11.0, 21.0)
            })
        );
        assert!(!r.engine().is_drawing(), "action mode must not draw ink");
    }

    #[test]
    fn button_up_tap_draws_ink() {
        let mut r = router();

        // PenDown with button up → ink engine starts a stroke.
        assert_eq!(
            r.handle(&InputEvent::PenDown(sample(5.0, 6.0))),
            RouterOutput::Ink(InkUpdate::Started)
        );
        assert!(r.engine().is_drawing());

        // PenUp finalises a real stroke.
        match r.handle(&InputEvent::PenUp(sample(5.0, 6.0))) {
            RouterOutput::Ink(InkUpdate::Finished { stroke, .. }) => {
                assert!(!stroke.points.is_empty());
            }
            other => panic!("expected finished ink stroke, got {other:?}"),
        }
        assert!(!r.engine().is_drawing());
    }

    #[test]
    fn pen_up_without_button_never_enters_action_mode() {
        let mut r = router();
        // A bare PenUp with no active stroke and button up: must be plain ink
        // routing (Idle from the engine), never an Action.
        let out = r.handle(&InputEvent::PenUp(sample(1.0, 2.0)));
        assert_eq!(out, RouterOutput::Ink(InkUpdate::Idle));
        assert!(!r.is_action_mode());
    }

    #[test]
    fn releasing_button_mid_press_abandons_pending_tap() {
        let mut r = router();
        r.handle(&InputEvent::PenButton { pressed: true });
        assert_eq!(
            r.handle(&InputEvent::PenDown(sample(7.0, 8.0))),
            RouterOutput::Idle
        );
        // Button released before PenUp: the pending action tap is dropped.
        r.handle(&InputEvent::PenButton { pressed: false });

        // The subsequent PenUp now flows to ink (button is up), not an action.
        let out = r.handle(&InputEvent::PenUp(sample(7.0, 8.0)));
        assert!(
            matches!(out, RouterOutput::Ink(_)),
            "post-release PenUp must route to ink, got {out:?}"
        );
    }

    #[test]
    fn moves_in_action_mode_are_idle_and_draw_nothing() {
        let mut r = router();
        r.handle(&InputEvent::PenButton { pressed: true });
        r.handle(&InputEvent::PenDown(sample(0.0, 0.0)));
        for i in 0..3 {
            let out = r.handle(&InputEvent::PenMove(sample(i as f32, i as f32)));
            assert_eq!(out, RouterOutput::Idle);
        }
        assert!(!r.engine().is_drawing());
    }

    #[test]
    fn custom_map_is_honoured() {
        struct TaskMap;
        impl ActionMap for TaskMap {
            fn tap_to_action(&self, at: CanvasPoint) -> Option<Action> {
                Some(Action::NewTask { at })
            }
        }
        let mut r = ActionRouter::with_map(InkEngine::new(InkConfig::default()), TaskMap);
        r.handle(&InputEvent::PenButton { pressed: true });
        r.handle(&InputEvent::PenDown(sample(2.0, 3.0)));
        let out = r.handle(&InputEvent::PenUp(sample(2.0, 3.0)));
        assert_eq!(
            out,
            RouterOutput::Action(Action::NewTask {
                at: CanvasPoint::new(2.0, 3.0)
            })
        );
    }

    #[test]
    fn swallowing_map_yields_idle() {
        struct SwallowMap;
        impl ActionMap for SwallowMap {
            fn tap_to_action(&self, _at: CanvasPoint) -> Option<Action> {
                None
            }
        }
        let mut r = ActionRouter::with_map(InkEngine::new(InkConfig::default()), SwallowMap);
        r.handle(&InputEvent::PenButton { pressed: true });
        r.handle(&InputEvent::PenDown(sample(1.0, 1.0)));
        assert_eq!(
            r.handle(&InputEvent::PenUp(sample(1.0, 1.0))),
            RouterOutput::Idle
        );
    }
}
