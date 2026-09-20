use ephemeris_core::Result;
use std::sync::mpsc::{channel, Receiver};
use tokio::runtime::Runtime;
use tokio::sync::mpsc::UnboundedSender;
use winit::application::ApplicationHandler;
use winit::event::WindowEvent;
use winit::event_loop::{ActiveEventLoop, EventLoop};
use winit::window::WindowId;

use ephemeris_pal::display::{DesktopWindow, PixelBuf};
use ephemeris_ui::{EphemerisUi, STATUS_BAR_H, TOOLBAR_H};

/// Commands from UI to async core.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Command {
    Ping,
    Shutdown,
}

/// Events from async core to UI.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Event {
    Pong,
    Ready,
}

/// Main application controller.
pub struct App {
    version: String,
    #[allow(dead_code)]
    rt: Runtime,
    cmd_tx: UnboundedSender<Command>,
    evt_rx: Receiver<Event>,
}

impl App {
    /// Create a new App instance with async runtime and channel pair.
    pub fn new() -> Result<Self> {
        tracing::info!("Initializing Ephemeris app");

        let rt = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .thread_name("ephemeris-core")
            .enable_all()
            .build()?;

        let (cmd_tx, mut cmd_rx) = tokio::sync::mpsc::unbounded_channel::<Command>();
        let (evt_tx, evt_rx) = channel::<Event>();
        let evt_tx_clone = evt_tx.clone();

        // Spawn async core task
        rt.spawn(async move {
            tracing::info!("Async core task started");
            while let Some(cmd) = cmd_rx.recv().await {
                match cmd {
                    Command::Ping => {
                        tracing::debug!("Received ping, sending pong");
                        let _ = evt_tx_clone.send(Event::Pong);
                    }
                    Command::Shutdown => {
                        tracing::info!("Shutdown command received");
                        break;
                    }
                }
            }
            tracing::info!("Async core task ended");
        });

        let _ = evt_tx.send(Event::Ready);

        Ok(App {
            version: env!("CARGO_PKG_VERSION").to_string(),
            rt,
            cmd_tx,
            evt_rx,
        })
    }

    /// Run the application.
    pub fn run(&self) -> Result<()> {
        tracing::info!("Ephemeris {} started", self.version);
        tracing::debug!("App is running");
        Ok(())
    }

    /// Send a command to the async core.
    pub fn send_command(&self, cmd: Command) -> std::result::Result<(), String> {
        self.cmd_tx
            .send(cmd)
            .map_err(|_| "Failed to send command".to_string())
    }

    /// Receive an event from the async core (non-blocking).
    pub fn try_recv_event(&self) -> Option<Event> {
        self.evt_rx.try_recv().ok()
    }
}

impl Default for App {
    fn default() -> Self {
        App::new().expect("Failed to create default App")
    }
}

impl App {
    /// Launch the live winit window with the Ephemeris UI and a test pattern.
    ///
    /// Blocks until the window is closed.  Consumes `self` because the tokio
    /// runtime is transferred into the winit event-loop handler.
    pub fn run_windowed(self) -> std::result::Result<(), Box<dyn std::error::Error>> {
        let ui = EphemerisUi::new(800, 600)?;
        ui.set_page_title("Ephemeris");
        desktop_test_pattern(&ui, 800, 600);

        let mut handler = WinitHandler {
            _rt: self.rt,
            ui,
            display: None,
        };

        EventLoop::new()?.run_app(&mut handler)?;
        Ok(())
    }
}

// ── Winit ApplicationHandler ──────────────────────────────────────────────────

struct WinitHandler {
    _rt: Runtime,
    ui: EphemerisUi,
    display: Option<DesktopWindow>,
}

impl ApplicationHandler for WinitHandler {
    fn resumed(&mut self, event_loop: &ActiveEventLoop) {
        match DesktopWindow::new(event_loop, 800, 600) {
            Ok(d) => {
                d.request_redraw();
                self.display = Some(d);
            }
            Err(e) => {
                tracing::error!("DesktopWindow creation failed: {e}");
                event_loop.exit();
            }
        }
    }

    fn window_event(&mut self, event_loop: &ActiveEventLoop, _id: WindowId, event: WindowEvent) {
        match event {
            WindowEvent::CloseRequested => event_loop.exit(),
            WindowEvent::RedrawRequested => {
                if let Some(display) = &mut self.display {
                    if let Err(e) = self.ui.render_frame(display) {
                        tracing::error!("render_frame failed: {e}");
                    }
                }
            }
            _ => {}
        }
    }
}

// ── Test pattern ──────────────────────────────────────────────────────────────

/// Push a 64-px checkerboard into the canvas so visual rendering is immediately
/// verifiable without any user input.
fn desktop_test_pattern(ui: &EphemerisUi, width: u32, height: u32) {
    let canvas_h = height - STATUS_BAR_H - TOOLBAR_H;
    let mut buf = PixelBuf::new(width, canvas_h);
    for y in 0..canvas_h as usize {
        for x in 0..width as usize {
            let checker = ((x / 64) + (y / 64)) % 2;
            buf.data[y * width as usize + x] = if checker == 0 { 180 } else { 230 };
        }
    }
    ui.push_canvas_pixels(&buf);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn app_creation() {
        let app = App::new().expect("should create app");
        assert!(!app.version.is_empty());
    }

    #[test]
    fn app_run() {
        let app = App::new().expect("should create app");
        app.run().expect("should run without error");
    }

    #[test]
    fn app_receives_ready_event() {
        let app = App::new().expect("should create app");
        // After creation, Ready event should be available
        let evt = app.try_recv_event();
        assert_eq!(evt, Some(Event::Ready));
    }

    #[test]
    fn app_ping_pong_communication() {
        let app = App::new().expect("should create app");
        // Consume the Ready event
        let _ = app.try_recv_event();

        // Send ping
        app.send_command(Command::Ping).expect("should send ping");

        // Give async core a moment to process
        std::thread::sleep(std::time::Duration::from_millis(100));

        // Receive pong
        let evt = app.try_recv_event();
        assert_eq!(evt, Some(Event::Pong));
    }
}
