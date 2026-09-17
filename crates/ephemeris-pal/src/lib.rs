//! Ephemeris Platform Abstraction Layer (PAL).
//!
//! This crate defines the interfaces between the application and the host
//! platform (display hardware, input devices, etc.).  Concrete backends live
//! in sibling crates or in this crate behind feature flags.

pub mod display;
pub mod input;

pub use display::{Display, PixelBuf, Rect, RefreshMode};
