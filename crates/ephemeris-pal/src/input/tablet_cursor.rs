//! Hide the Mutter/GNOME tablet-tool cursor on PineNote.
//!
//! Pen ink is read via evdev (`EvdevPenSource`). Mutter still sees the same
//! digitiser through libinput and draws a **tablet-tool** cursor. Winit's
//! `set_cursor` / `set_cursor_visible` only affect `wl_pointer`, so they cannot
//! hide it.
//!
//! The focused client must bind `zwp_tablet_manager_v2` on **winit's**
//! `wl_display` and call `zwp_tablet_tool_v2.set_cursor(serial, NULL)` on every
//! `proximity_in`. Binding tablet_v2 also stops Mutter from pointer-emulating
//! the pen for this window (evdev remains the ink path).

#[cfg(all(feature = "wayland", target_os = "linux"))]
mod live {
    use wayland_client::backend::Backend;
    use wayland_client::globals::{registry_queue_init, GlobalListContents};
    use wayland_client::protocol::{wl_registry, wl_seat};
    use wayland_client::{Connection, Dispatch, EventQueue, QueueHandle};
    use wayland_protocols::wp::tablet::zv2::client::{
        zwp_tablet_manager_v2, zwp_tablet_seat_v2, zwp_tablet_tool_v2, zwp_tablet_v2,
    };
    use winit::raw_window_handle::{HasDisplayHandle, RawDisplayHandle};

    struct State {
        /// Kept alive so tool_added / proximity events keep flowing.
        _tablet_seat: Option<zwp_tablet_seat_v2::ZwpTabletSeatV2>,
    }

    /// Separate Wayland event queue on winit's display that only hides the
    /// tablet-tool cursor. Does not feed ink events.
    pub struct TabletCursorHider {
        conn: Connection,
        queue: EventQueue<State>,
        state: State,
    }

    impl TabletCursorHider {
        /// Attach to the same `wl_display` as `window` (must be a Wayland window).
        pub fn new(window: &impl HasDisplayHandle) -> Result<Self, String> {
            let handle = window
                .display_handle()
                .map_err(|e| format!("display handle: {e}"))?;
            let RawDisplayHandle::Wayland(w) = handle.as_raw() else {
                return Err("not a Wayland display".into());
            };
            // SAFETY: `window` (and thus winit's connection) outlives this hider;
            // DesktopWindow owns both.
            let backend = unsafe { Backend::from_foreign_display(w.display.as_ptr().cast()) };
            let conn = Connection::from_backend(backend);

            let (globals, mut queue) = registry_queue_init::<State>(&conn)
                .map_err(|e| format!("registry_queue_init: {e}"))?;
            let qh = queue.handle();

            let seat: wl_seat::WlSeat = globals
                .bind(&qh, 1..=7, ())
                .map_err(|e| format!("bind wl_seat: {e}"))?;
            let manager: zwp_tablet_manager_v2::ZwpTabletManagerV2 =
                globals
                    .bind(&qh, 1..=1, ())
                    .map_err(|e| format!("bind zwp_tablet_manager_v2: {e}"))?;
            let tablet_seat = manager.get_tablet_seat(&seat, &qh, ());

            let mut state = State {
                _tablet_seat: Some(tablet_seat),
            };

            // Drain tool_added for tools already in proximity.
            queue
                .roundtrip(&mut state)
                .map_err(|e| format!("tablet cursor roundtrip: {e}"))?;

            Ok(Self { conn, queue, state })
        }

        /// Non-blocking: dispatch pending tablet events and flush `set_cursor`.
        ///
        /// Winit/softbuffer already read the shared `wl_display` socket; we only
        /// need to drain our queue. Opportunistically try a zero-timeout read
        /// when the fd is already readable.
        pub fn poll(&mut self) {
            let _ = self.queue.dispatch_pending(&mut self.state);
            if let Some(guard) = self.queue.prepare_read() {
                let fd = guard.connection_fd();
                let mut fds = [rustix::event::PollFd::new(
                    &fd,
                    rustix::event::PollFlags::IN | rustix::event::PollFlags::ERR,
                )];
                let timeout = rustix::event::Timespec {
                    tv_sec: 0,
                    tv_nsec: 0,
                };
                match rustix::event::poll(&mut fds, Some(&timeout)) {
                    Ok(0) => drop(guard),
                    Ok(_) => {
                        let _ = guard.read();
                    }
                    Err(_) => drop(guard),
                }
            }
            let _ = self.queue.dispatch_pending(&mut self.state);
            let _ = self.conn.flush();
        }
    }

    impl Dispatch<wl_registry::WlRegistry, GlobalListContents> for State {
        fn event(
            _: &mut Self,
            _: &wl_registry::WlRegistry,
            _: wl_registry::Event,
            _: &GlobalListContents,
            _: &Connection,
            _: &QueueHandle<Self>,
        ) {
        }
    }

    impl Dispatch<wl_seat::WlSeat, ()> for State {
        fn event(
            _: &mut Self,
            _: &wl_seat::WlSeat,
            _: wl_seat::Event,
            _: &(),
            _: &Connection,
            _: &QueueHandle<Self>,
        ) {
        }
    }

    impl Dispatch<zwp_tablet_manager_v2::ZwpTabletManagerV2, ()> for State {
        fn event(
            _: &mut Self,
            _: &zwp_tablet_manager_v2::ZwpTabletManagerV2,
            _: zwp_tablet_manager_v2::Event,
            _: &(),
            _: &Connection,
            _: &QueueHandle<Self>,
        ) {
        }
    }

    impl Dispatch<zwp_tablet_seat_v2::ZwpTabletSeatV2, ()> for State {
        fn event(
            _: &mut Self,
            _: &zwp_tablet_seat_v2::ZwpTabletSeatV2,
            _: zwp_tablet_seat_v2::Event,
            _: &(),
            _: &Connection,
            _: &QueueHandle<Self>,
        ) {
            // tool_added / tablet_added proxies are routed to their own Dispatch.
        }
    }

    impl Dispatch<zwp_tablet_v2::ZwpTabletV2, ()> for State {
        fn event(
            _: &mut Self,
            _: &zwp_tablet_v2::ZwpTabletV2,
            _: zwp_tablet_v2::Event,
            _: &(),
            _: &Connection,
            _: &QueueHandle<Self>,
        ) {
        }
    }

    impl Dispatch<zwp_tablet_tool_v2::ZwpTabletToolV2, ()> for State {
        fn event(
            _: &mut Self,
            tool: &zwp_tablet_tool_v2::ZwpTabletToolV2,
            event: zwp_tablet_tool_v2::Event,
            _: &(),
            _: &Connection,
            _: &QueueHandle<Self>,
        ) {
            if let zwp_tablet_tool_v2::Event::ProximityIn { serial, .. } = event {
                // NULL surface → hide the tablet-tool cursor for this proximity.
                tool.set_cursor(serial, None, 0, 0);
            }
        }
    }
}

#[cfg(all(feature = "wayland", target_os = "linux"))]
pub use live::TabletCursorHider;

/// Stub when Wayland tablet protocols are unavailable (macOS / no feature).
#[cfg(not(all(feature = "wayland", target_os = "linux")))]
pub struct TabletCursorHider;

#[cfg(not(all(feature = "wayland", target_os = "linux")))]
impl TabletCursorHider {
    pub fn new(_window: &impl winit::raw_window_handle::HasDisplayHandle) -> Result<Self, String> {
        Err("tablet cursor hider requires Linux + wayland feature".into())
    }

    pub fn poll(&mut self) {}
}
