//! Linux evdev stylus reader for devices (e.g. PineNote) where the pen is a
//! separate digitiser and does **not** appear as `wl_touch` / winit `Touch`.
//!
//! Scans `/dev/input/event*` for a device that looks like a tablet/pen
//! (`BTN_TOOL_PEN` / `INPUT_PROP_DIRECT` + absolute axes), then streams
//! normalised [`InputEvent`]s. Coordinates are mapped into the UI logical
//! size supplied via [`PenTarget`].

#![cfg(target_os = "linux")]

use std::fs;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::Arc;
use std::thread::{self, JoinHandle};
use std::time::Duration;

use crossbeam_channel::{bounded, Receiver, Sender};
use evdev::{AbsoluteAxisCode, Device, EventSummary, KeyCode, PropType};

use super::{InputEvent, PenSample};

const CHANNEL_CAPACITY: usize = 256;

/// Live UI size the pen coordinates should be mapped into.
#[derive(Debug)]
pub struct PenTarget {
    pub width: AtomicU32,
    pub height: AtomicU32,
}

impl PenTarget {
    pub fn new(width: u32, height: u32) -> Arc<Self> {
        Arc::new(Self {
            width: AtomicU32::new(width.max(1)),
            height: AtomicU32::new(height.max(1)),
        })
    }

    pub fn set(&self, width: u32, height: u32) {
        self.width.store(width.max(1), Ordering::Relaxed);
        self.height.store(height.max(1), Ordering::Relaxed);
    }
}

/// Optional wake callback (e.g. winit `EventLoopProxy::send_event`) so the UI
/// thread leaves `Wait` when pen samples arrive.
pub type PenWake = Arc<dyn Fn() + Send + Sync>;

/// Background pen reader. Dropping this requests the worker to stop.
pub struct EvdevPenSource {
    rx: Receiver<InputEvent>,
    stop: Arc<AtomicBool>,
    _join: Option<JoinHandle<()>>,
}

impl EvdevPenSource {
    /// Start scanning for a pen device and streaming events.
    ///
    /// Returns `None` when no suitable device is found (desktop without a
    /// digitiser). The caller should keep finger/mouse paths working.
    pub fn spawn(target: Arc<PenTarget>, wake: Option<PenWake>) -> Option<Self> {
        let path = find_pen_device()?;
        let path_log = path.display().to_string();
        let (tx, rx) = bounded(CHANNEL_CAPACITY);
        let stop = Arc::new(AtomicBool::new(false));
        let stop_w = Arc::clone(&stop);
        let join = thread::Builder::new()
            .name("ephemeris-evdev-pen".into())
            .spawn(move || {
                if let Err(e) = run_device(&path, target, tx, stop_w, wake) {
                    log::warn!("evdev pen reader stopped: {e}");
                }
            })
            .ok()?;
        log::info!("evdev pen reader using {path_log}");
        Some(Self {
            rx,
            stop,
            _join: Some(join),
        })
    }

    pub fn receiver(&self) -> &Receiver<InputEvent> {
        &self.rx
    }

    /// Drain all pending events (non-blocking).
    pub fn try_iter(&self) -> crossbeam_channel::TryIter<'_, InputEvent> {
        self.rx.try_iter()
    }
}

impl Drop for EvdevPenSource {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(handle) = self._join.take() {
            let _ = handle.join();
        }
    }
}

fn find_pen_device() -> Option<PathBuf> {
    let mut candidates = Vec::new();
    let dir = fs::read_dir("/dev/input").ok()?;
    for entry in dir.flatten() {
        let path = entry.path();
        let name = path.file_name()?.to_string_lossy();
        if !name.starts_with("event") {
            continue;
        }
        let Ok(device) = Device::open(&path) else {
            continue;
        };
        if looks_like_pen(&device) {
            let score = pen_score(&device);
            candidates.push((score, path));
        }
    }
    candidates.sort_by_key(|a| std::cmp::Reverse(a.0));
    candidates.into_iter().map(|(_, p)| p).next()
}

fn looks_like_pen(device: &Device) -> bool {
    let Some(keys) = device.supported_keys() else {
        return false;
    };
    let Some(axes) = device.supported_absolute_axes() else {
        return false;
    };
    let has_xy = axes.contains(AbsoluteAxisCode::ABS_X) && axes.contains(AbsoluteAxisCode::ABS_Y);
    if !has_xy {
        return false;
    }
    keys.contains(KeyCode::BTN_TOOL_PEN)
        || keys.contains(KeyCode::BTN_STYLUS)
        || (keys.contains(KeyCode::BTN_TOUCH) && device.properties().contains(PropType::DIRECT))
}

fn pen_score(device: &Device) -> i32 {
    let mut score = 0;
    if let Some(keys) = device.supported_keys() {
        if keys.contains(KeyCode::BTN_TOOL_PEN) {
            score += 10;
        }
        if keys.contains(KeyCode::BTN_STYLUS) {
            score += 5;
        }
        if keys.contains(KeyCode::BTN_TOUCH) {
            score += 2;
        }
    }
    if device.properties().contains(PropType::DIRECT) {
        score += 3;
    }
    score
}

fn emit(tx: &Sender<InputEvent>, wake: &Option<PenWake>, event: InputEvent) {
    if tx.try_send(event).is_ok() {
        if let Some(w) = wake {
            w();
        }
    }
}

fn run_device(
    path: &PathBuf,
    target: Arc<PenTarget>,
    tx: Sender<InputEvent>,
    stop: Arc<AtomicBool>,
    wake: Option<PenWake>,
) -> Result<(), String> {
    let mut device = Device::open(path).map_err(|e| format!("open {}: {e}", path.display()))?;
    // Optional exclusive grab (EPHEMERIS_GRAB_PEN=1). Default off so the
    // compositor still sees the stylus when Ephemeris is not focused.
    if std::env::var_os("EPHEMERIS_GRAB_PEN").is_some() {
        match device.grab() {
            Ok(()) => log::info!("evdev pen: exclusive grab on {}", path.display()),
            Err(e) => log::warn!(
                "evdev pen: could not grab {} ({e}); continuing without exclusive access",
                path.display()
            ),
        }
    }

    let (x_min, x_max) = abs_range(&device, AbsoluteAxisCode::ABS_X)?;
    let (y_min, y_max) = abs_range(&device, AbsoluteAxisCode::ABS_Y)?;
    let (p_min, p_max) = abs_range(&device, AbsoluteAxisCode::ABS_PRESSURE).unwrap_or((0, 4096));

    let mut raw_x = x_min;
    let mut raw_y = y_min;
    let mut raw_p = 0;
    let mut tip_down = false;
    let mut in_range = false;
    let mut have_pos = false;

    while !stop.load(Ordering::Relaxed) {
        let events = match device.fetch_events() {
            Ok(evs) => evs,
            Err(err) if err.kind() == std::io::ErrorKind::WouldBlock => {
                thread::sleep(Duration::from_millis(4));
                continue;
            }
            Err(err) => return Err(format!("fetch_events: {err}")),
        };

        for ev in events {
            match EventSummary::from(ev) {
                EventSummary::AbsoluteAxis(_, AbsoluteAxisCode::ABS_X, val) => {
                    raw_x = val;
                    have_pos = true;
                }
                EventSummary::AbsoluteAxis(_, AbsoluteAxisCode::ABS_Y, val) => {
                    raw_y = val;
                    have_pos = true;
                }
                EventSummary::AbsoluteAxis(_, AbsoluteAxisCode::ABS_PRESSURE, val) => {
                    raw_p = val;
                }
                EventSummary::Key(_, KeyCode::BTN_TOOL_PEN | KeyCode::BTN_TOOL_RUBBER, val) => {
                    in_range = val != 0;
                    if val == 0 && tip_down {
                        tip_down = false;
                        let sample = sample(
                            raw_x, raw_y, raw_p, false, &target, x_min, x_max, y_min, y_max, p_min,
                            p_max,
                        );
                        emit(&tx, &wake, InputEvent::PenUp(sample));
                    }
                }
                EventSummary::Key(_, KeyCode::BTN_TOUCH | KeyCode::BTN_LEFT, val) => {
                    if !have_pos {
                        continue;
                    }
                    let pressed = val != 0;
                    let s = sample(
                        raw_x, raw_y, raw_p, pressed, &target, x_min, x_max, y_min, y_max, p_min,
                        p_max,
                    );
                    if pressed && !tip_down {
                        tip_down = true;
                        emit(&tx, &wake, InputEvent::PenDown(s));
                    } else if !pressed && tip_down {
                        tip_down = false;
                        emit(&tx, &wake, InputEvent::PenUp(s));
                    }
                }
                EventSummary::Key(_, KeyCode::BTN_STYLUS | KeyCode::BTN_STYLUS2, val) => {
                    emit(&tx, &wake, InputEvent::PenButton { pressed: val != 0 });
                }
                EventSummary::Synchronization(_, _, _) if have_pos && (tip_down || in_range) => {
                    let s = sample(
                        raw_x,
                        raw_y,
                        raw_p,
                        tip_down || in_range,
                        &target,
                        x_min,
                        x_max,
                        y_min,
                        y_max,
                        p_min,
                        p_max,
                    );
                    if tip_down {
                        emit(&tx, &wake, InputEvent::PenMove(s));
                    } else if in_range {
                        emit(&tx, &wake, InputEvent::Hover { x: s.x, y: s.y });
                    }
                }
                _ => {}
            }
        }
    }
    Ok(())
}

fn abs_range(device: &Device, axis: AbsoluteAxisCode) -> Result<(i32, i32), String> {
    let info = device
        .get_abs_state()
        .map_err(|e| format!("get_abs_state: {e}"))?;
    let axis_info = info
        .get(axis.0 as usize)
        .ok_or_else(|| format!("missing abs axis {axis:?}"))?;
    Ok((
        axis_info.minimum,
        axis_info.maximum.max(axis_info.minimum + 1),
    ))
}

#[allow(clippy::too_many_arguments)]
fn sample(
    raw_x: i32,
    raw_y: i32,
    raw_p: i32,
    in_contact: bool,
    target: &PenTarget,
    x_min: i32,
    x_max: i32,
    y_min: i32,
    y_max: i32,
    p_min: i32,
    p_max: i32,
) -> PenSample {
    let ui_w = target.width.load(Ordering::Relaxed).max(1) as f32;
    let ui_h = target.height.load(Ordering::Relaxed).max(1) as f32;
    let nx = ((raw_x - x_min) as f32) / ((x_max - x_min) as f32).max(1.0);
    let ny = ((raw_y - y_min) as f32) / ((y_max - y_min) as f32).max(1.0);
    let pressure = if in_contact {
        ((raw_p - p_min) as f32 / ((p_max - p_min) as f32).max(1.0)).clamp(0.05, 1.0)
    } else {
        0.0
    };
    PenSample {
        x: nx.clamp(0.0, 1.0) * ui_w,
        y: ny.clamp(0.0, 1.0) * ui_h,
        pressure,
        tilt: 0.0,
        in_range: true,
    }
}
