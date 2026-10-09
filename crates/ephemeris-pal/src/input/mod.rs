//! Unified input abstraction for Ephemeris.
//!
//! # Overview
//!
//! All platform backends (Wayland tablet_v2, evdev, desktop mock) translate
//! raw hardware signals into [`InputEvent`] values and deliver them through an
//! [`Input`] implementation.
//!
//! Consumers only interact with [`Input`] and [`InputEvent`] — never with
//! backend-specific types.
//!
//! # Design notes
//!
//! * Coordinates are in **logical pixels** (device-independent units).
//!   Backends are responsible for applying any HiDPI scaling before emitting
//!   events.
//! * `pressure` and `tilt` are normalised to `[0.0, 1.0]` and
//!   `[−90.0, 90.0]` degrees respectively (matching Wayland tablet_v2
//!   semantics).
//! * The `Input` trait uses a **channel-based pull model**: the backend sends
//!   events into a [`crossbeam_channel::Receiver`] which the consumer polls or
//!   blocks on.  This avoids forcing async onto consumers while still
//!   supporting background threads.

pub mod mock;
pub mod palm;
pub mod stylus_map;
pub mod wayland;

#[cfg(target_os = "linux")]
pub mod evdev_pen;

pub use palm::{PalmRejector, PalmRejectorConfig};
pub use stylus_map::{digitizer_to_window, orient_axes, window_to_ui};
pub use wayland::WaylandInput;

#[cfg(target_os = "linux")]
pub use evdev_pen::{EvdevPenSource, PenTarget, PenWake};

use crossbeam_channel::Receiver;
use thiserror::Error;

// ── Event types ──────────────────────────────────────────────────────────────

/// A normalised input event emitted by any [`Input`] backend.
///
/// All coordinate fields (`x`, `y`) are in logical pixels relative to the
/// top-left corner of the application surface.
#[derive(Debug, Clone, PartialEq)]
pub enum InputEvent {
    // ── Pen / stylus ─────────────────────────────────────────────────────────
    /// The pen tip made contact with the surface (first touch-down sample).
    PenDown(PenSample),

    /// The pen tip is moving while in contact with the surface.
    PenMove(PenSample),

    /// The pen tip lifted from the surface.
    PenUp(PenSample),

    /// The side button on the pen was pressed or released.
    ///
    /// The pen-button-held modifier enables the *action mode* described in
    /// ADR ephemeris-4ea: a tap while the button is held dispatches a
    /// contextual [`Action`] instead of drawing ink.
    PenButton {
        /// `true` = pressed, `false` = released.
        pressed: bool,
    },

    /// The pen is hovering above (in proximity of) the surface but not
    /// touching.  Used for cursor preview and palm rejection.
    Hover { x: f32, y: f32 },

    // ── Finger touch ─────────────────────────────────────────────────────────
    /// A new finger contact began.
    TouchBegin(TouchSample),

    /// An existing finger contact moved.
    TouchMove(TouchSample),

    /// A finger contact ended (lift).
    TouchEnd(TouchSample),
}

/// One sample from the pen sensor while the tip is in contact or hovering.
#[derive(Debug, Clone, PartialEq)]
pub struct PenSample {
    /// Horizontal position in logical pixels.
    pub x: f32,
    /// Vertical position in logical pixels.
    pub y: f32,
    /// Tip pressure, normalised to `[0.0, 1.0]`.
    /// `0.0` when the pen is hovering.
    pub pressure: f32,
    /// Tilt angle in degrees, range `[−90.0, 90.0]`.
    /// Positive values tilt toward the right/bottom.
    pub tilt: f32,
    /// `true` while the pen is in the digitiser's detection range (includes
    /// hover), `false` when it has left the proximity zone entirely.
    pub in_range: bool,
}

/// One sample from a finger touch contact.
#[derive(Debug, Clone, PartialEq)]
pub struct TouchSample {
    /// Slot identifier used to correlate begin/move/end for the same finger.
    pub id: u32,
    /// Horizontal position in logical pixels.
    pub x: f32,
    /// Vertical position in logical pixels.
    pub y: f32,
}

// ── Error type ───────────────────────────────────────────────────────────────

/// Errors that can occur while using an [`Input`] backend.
#[derive(Debug, Error)]
pub enum InputError {
    #[error("input backend initialisation failed: {0}")]
    Init(String),

    #[error("input event channel disconnected")]
    Disconnected,

    #[error("input backend I/O error: {0}")]
    Io(#[from] std::io::Error),
}

// ── Trait ─────────────────────────────────────────────────────────────────────

/// Platform abstraction for input devices.
///
/// Implementations deliver a stream of [`InputEvent`] values through a
/// [`Receiver`].  The receiver is **multi-consumer safe** (use
/// [`Receiver::clone`] if you need fan-out) and can be polled, iterated, or
/// selected with other channels.
///
/// # Lifecycle
///
/// 1. Create a backend via its own constructor (e.g.
///    [`mock::MockInput::from_script`]).
/// 2. Call [`Input::receiver`] to obtain a handle to the event stream.
/// 3. Drive the backend by calling [`Input::run`] (typically on a dedicated
///    thread) or by using a backend that spawns its own thread internally.
/// 4. Drop the backend to stop the stream; the receiver will return
///    `Err(RecvError)` once the channel is empty and closed.
pub trait Input: Send + 'static {
    /// Returns a clone of the shared receiver end of the event channel.
    ///
    /// May be called before [`Input::run`]; the receiver will simply block
    /// until events start flowing.
    fn receiver(&self) -> Receiver<InputEvent>;

    /// Drive the backend, sending events into the channel.
    ///
    /// This method **blocks** until the event source is exhausted or an
    /// unrecoverable error occurs.  Run it on a dedicated thread:
    ///
    /// ```ignore
    /// let backend = MockInput::from_script(events);
    /// let rx = backend.receiver();
    /// std::thread::spawn(move || backend.run());
    /// for event in rx { /* … */ }
    /// ```
    fn run(self) -> Result<(), InputError>;
}
