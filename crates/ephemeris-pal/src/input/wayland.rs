use crossbeam_channel::Sender;
use std::sync::{Arc, Mutex};
use super::InputEvent;

/// Wayland input backend for tablet_v2 and wl_touch support.
pub struct WaylandInput {
    #[allow(dead_code)]
    state: Arc<Mutex<WaylandState>>,
}

#[allow(dead_code)]
#[derive(Debug, Default)]
struct WaylandState {
    pen_x: f32,
    pen_y: f32,
    pen_pressure: f32,
    pen_tilt: f32,
    pen_active: bool,
    touch_points: std::collections::HashMap<u32, (f32, f32)>,
}

impl WaylandInput {
    /// Create a new Wayland input backend.
    pub fn new() -> std::result::Result<Self, Box<dyn std::error::Error>> {
        Ok(WaylandInput { state: Arc::new(Mutex::new(WaylandState::default())) })
    }

    /// Subscribe to input events (stub for future thread-based event loop).
    pub fn subscribe(&self, _tx: Sender<InputEvent>) {
        // Future: spawn thread to run Wayland event loop and emit events
    }

    /// Handle pen proximity change.
    #[allow(dead_code)]
    fn on_pen_proximity(&self, in_range: bool) {
        let mut state = self.state.lock().unwrap();
        state.pen_active = in_range;
    }

    /// Handle pen motion.
    #[allow(dead_code)]
    fn on_pen_motion(&self, x: f32, y: f32) {
        let mut state = self.state.lock().unwrap();
        state.pen_x = x;
        state.pen_y = y;
    }

    /// Handle pen pressure (0-1023, normalized to 0.0-1.0).
    #[allow(dead_code)]
    fn on_pen_pressure(&self, pressure: u32) {
        let mut state = self.state.lock().unwrap();
        state.pen_pressure = (pressure as f32 / 1023.0).min(1.0);
    }

    /// Handle pen tilt (degrees, normalized).
    #[allow(dead_code)]
    fn on_pen_tilt(&self, tx: f32, ty: f32) {
        let mut state = self.state.lock().unwrap();
        state.pen_tilt = (tx * tx + ty * ty).sqrt() / 90.0;
    }

    /// Handle touch down.
    #[allow(dead_code)]
    fn on_touch_down(&self, touch_id: u32, x: f32, y: f32) {
        let mut state = self.state.lock().unwrap();
        state.touch_points.insert(touch_id, (x, y));
    }

    /// Handle touch motion.
    #[allow(dead_code)]
    fn on_touch_motion(&self, touch_id: u32, x: f32, y: f32) {
        let mut state = self.state.lock().unwrap();
        state.touch_points.insert(touch_id, (x, y));
    }

    /// Handle touch up.
    #[allow(dead_code)]
    fn on_touch_up(&self, touch_id: u32) {
        let mut state = self.state.lock().unwrap();
        state.touch_points.remove(&touch_id);
    }
}

impl Default for WaylandInput {
    fn default() -> Self {
        WaylandInput { state: Arc::new(Mutex::new(WaylandState::default())) }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wayland_input_creation() {
        let input = WaylandInput::new().expect("should create");
        let state = input.state.lock().unwrap();
        assert!(!state.pen_active);
        assert_eq!(state.pen_pressure, 0.0);
    }

    #[test]
    fn wayland_pen_motion() {
        let input = WaylandInput::new().expect("should create");
        input.on_pen_motion(100.0, 200.0);
        let state = input.state.lock().unwrap();
        assert_eq!(state.pen_x, 100.0);
        assert_eq!(state.pen_y, 200.0);
    }

    #[test]
    fn wayland_pen_pressure_normalization() {
        let input = WaylandInput::new().expect("should create");
        input.on_pen_pressure(512);  // ~50%
        let state = input.state.lock().unwrap();
        assert!((state.pen_pressure - 0.5).abs() < 0.01);
    }

    #[test]
    fn wayland_pen_tilt() {
        let input = WaylandInput::new().expect("should create");
        input.on_pen_tilt(45.0, 45.0);
        let state = input.state.lock().unwrap();
        assert!(state.pen_tilt > 0.0);
    }

    #[test]
    fn wayland_touch_tracking() {
        let input = WaylandInput::new().expect("should create");
        input.on_touch_down(1, 100.0, 100.0);
        input.on_touch_motion(1, 150.0, 150.0);
        input.on_touch_up(1);
        let state = input.state.lock().unwrap();
        assert!(state.touch_points.is_empty());
    }

    #[test]
    fn wayland_input_thread_safe() {
        let input = Arc::new(WaylandInput::new().expect("should create"));
        let input_clone = Arc::clone(&input);
        std::thread::spawn(move || {
            input_clone.on_pen_motion(50.0, 50.0);
        });
        std::thread::sleep(std::time::Duration::from_millis(10));
        let state = input.state.lock().unwrap();
        assert!(state.pen_x > 0.0);
    }
}
