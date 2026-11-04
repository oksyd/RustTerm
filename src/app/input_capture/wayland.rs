use super::CaptureStatus;
use super::release_gesture::PhysicalReleaseGesture;
use anyhow::{Context as _, Result, bail};
use gpui::Window;
use raw_window_handle::{HasDisplayHandle, HasWindowHandle, RawDisplayHandle, RawWindowHandle};
use rustterm::i18n::{Message, messages as i18n_key};
use std::{
    cell::RefCell,
    collections::BTreeMap,
    io::ErrorKind,
    marker::PhantomData,
    ptr::NonNull,
    rc::{Rc, Weak},
    time::Instant,
};
use wayland_backend::client::{Backend, ObjectId, WaylandError};
use wayland_client::{
    Connection, Dispatch, EventQueue, Proxy, QueueHandle, WEnum,
    protocol::{wl_callback, wl_keyboard, wl_registry, wl_seat, wl_surface},
};
use wayland_protocols::wp::keyboard_shortcuts_inhibit::zv1::client::{
    zwp_keyboard_shortcuts_inhibit_manager_v1 as manager,
    zwp_keyboard_shortcuts_inhibitor_v1 as inhibitor,
};

// A compositor may ask the user to approve the request. Do not leave the RDP
// click waiting indefinitely when that request is ignored or never answered.
const REQUEST_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(15);

thread_local! {
    static SURFACE_LEASES: RefCell<BTreeMap<(usize, usize), Weak<()>>> = const {
        RefCell::new(BTreeMap::new())
    };
}

struct SurfaceLease {
    key: (usize, usize),
    _token: Rc<()>,
}

impl SurfaceLease {
    fn acquire(display: usize, surface: usize) -> Result<Self> {
        let key = (display, surface);
        let token = Rc::new(());
        SURFACE_LEASES.with_borrow_mut(|leases| {
            if leases
                .get(&key)
                .is_some_and(|lease| lease.strong_count() != 0)
            {
                bail!(Message::new(
                    i18n_key::RDP_KEYBOARD_CAPTURE_ALREADY_REQUESTED
                ));
            }
            leases.insert(key, Rc::downgrade(&token));
            Ok(Self { key, _token: token })
        })
    }
}

impl Drop for SurfaceLease {
    fn drop(&mut self) {
        SURFACE_LEASES.with_borrow_mut(|leases| {
            leases.remove(&self.key);
        });
    }
}

pub(super) struct Capture {
    queue: EventQueue<State>,
    state: State,
    registry: wl_registry::WlRegistry,
    connection: Connection,
    requested_at: Instant,
    released: bool,
    lease: Option<SurfaceLease>,
    // All interaction stays on GPUI's UI thread, including final destruction.
    _ui_thread: PhantomData<Rc<()>>,
}

struct Seat {
    proxy: wl_seat::WlSeat,
    keyboard: Option<wl_keyboard::WlKeyboard>,
}

struct State {
    surface: wl_surface::WlSurface,
    manager: Option<(u32, manager::ZwpKeyboardShortcutsInhibitManagerV1)>,
    seats: BTreeMap<u32, Seat>,
    focused_seat: Option<u32>,
    inhibitor: Option<inhibitor::ZwpKeyboardShortcutsInhibitorV1>,
    status: CaptureStatus,
    stopped: bool,
    release_gesture: PhysicalReleaseGesture,
    release_requested: bool,
    release_blocked: bool,
}

impl Capture {
    pub(super) fn request(window: &Window) -> Result<Self> {
        let RawDisplayHandle::Wayland(display) = window.display_handle()?.as_raw() else {
            bail!(Message::new(i18n_key::RDP_NATIVE_DISPLAY_NOT_WAYLAND));
        };
        let RawWindowHandle::Wayland(surface) = HasWindowHandle::window_handle(window)?.as_raw()
        else {
            bail!(Message::new(i18n_key::RDP_NATIVE_WINDOW_NOT_WAYLAND));
        };
        // SAFETY: these are live handles from the same GPUI window. Its owner
        // releases the guard before destroying the native window/display.
        unsafe { Self::from_handles(display.display, surface.surface) }
    }

    /// The display and surface must belong to the same connection and outlive
    /// the capture. Kept separate so a socket-only protocol test can exercise
    /// exactly the same foreign-connection path without creating a GUI window.
    unsafe fn from_handles(
        display: NonNull<std::ffi::c_void>,
        surface: NonNull<std::ffi::c_void>,
    ) -> Result<Self> {
        // The protocol treats a second inhibitor on the same seat/surface as a
        // fatal error for the entire display. Protect against overlapping pane
        // requests, including while the first one is still awaiting approval.
        let lease = SurfaceLease::acquire(display.as_ptr() as usize, surface.as_ptr() as usize)?;

        // SAFETY: GPUI owns this display and surface. The view releases this
        // guard before destroying its window. from_foreign_display creates a
        // separate event queue and does not take ownership of the display.
        let backend = unsafe { Backend::from_foreign_display(display.as_ptr().cast()) };
        let connection = Connection::from_backend(backend);
        // SAFETY: the handle is GPUI's live wl_surface. We only borrow it as a
        // protocol argument; never manage_object, replace its listener, or send
        // a destructor on it.
        let surface_id = unsafe {
            ObjectId::from_ptr(wl_surface::WlSurface::interface(), surface.as_ptr().cast())
        }
        .context(Message::new(i18n_key::RDP_INVALID_WAYLAND_SURFACE))?;
        let surface = wl_surface::WlSurface::from_id(&connection, surface_id)
            .context(Message::new(i18n_key::RDP_CANNOT_BORROW_WAYLAND_SURFACE))?;
        let queue = connection.new_event_queue();
        let handle = queue.handle();
        let registry = connection.display().get_registry(&handle, ());
        // Asynchronous completion identifies unsupported compositors without a
        // blocking registry roundtrip on the UI thread.
        connection.display().sync(&handle, ());
        let capture = Self {
            queue,
            state: State {
                surface,
                manager: None,
                seats: BTreeMap::new(),
                focused_seat: None,
                inhibitor: None,
                status: CaptureStatus::Pending,
                stopped: false,
                release_gesture: PhysicalReleaseGesture::default(),
                release_requested: false,
                release_blocked: false,
            },
            registry,
            connection,
            requested_at: Instant::now(),
            released: false,
            lease: Some(lease),
            _ui_thread: PhantomData,
        };
        capture
            .flush()
            .context(Message::new(i18n_key::RDP_CANNOT_REQUEST_KEYBOARD_CAPTURE))?;
        Ok(capture)
    }

    fn flush(&self) -> Result<(), WaylandError> {
        match self.connection.flush() {
            Err(WaylandError::Io(error)) if error.kind() == ErrorKind::WouldBlock => Ok(()),
            result => result,
        }
    }

    pub(super) fn poll_status(&mut self) -> CaptureStatus {
        if self.released {
            return CaptureStatus::Denied;
        }
        // GPUI reads the shared wl_display as usual. libwayland places only our
        // objects' events into this private queue. No second reader, roundtrip,
        // or manipulation of GPUI's queue is needed here.
        if self.queue.dispatch_pending(&mut self.state).is_err()
            || self.flush().is_err()
            || (self.state.status == CaptureStatus::Pending
                && self.requested_at.elapsed() >= REQUEST_TIMEOUT)
        {
            self.state.deny();
        }
        if self.state.status == CaptureStatus::Denied {
            self.release();
        }
        self.state.status
    }

    pub(super) fn take_release_request(&mut self) -> bool {
        std::mem::take(&mut self.state.release_requested)
            && self.state.status != CaptureStatus::Denied
    }

    pub(super) fn has_held_keys(&self) -> bool {
        self.state.release_gesture.has_held_keys()
    }

    pub(super) fn interrupt_release_gesture(&mut self) {
        // Drain keys already read by GPUI before applying a later mouse/IME
        // interruption. Otherwise stale queued key presses could re-arm the
        // gesture immediately after the caller has cancelled it.
        self.poll_status();
        self.state.release_gesture.interrupt();
        self.state.release_requested = false;
    }

    pub(super) fn set_release_blocked(&mut self, blocked: bool) {
        self.interrupt_release_gesture();
        self.state.release_blocked = blocked;
    }

    pub(super) fn release(&mut self) {
        if self.released {
            return;
        }
        self.released = true;
        self.state.deny();
        for (_, seat) in std::mem::take(&mut self.state.seats) {
            if let Some(keyboard) = seat.keyboard {
                keyboard.release();
            }
            seat.proxy.release();
        }
        if let Some((_, manager)) = self.state.manager.take() {
            manager.destroy();
        }
        let _ = self
            .connection
            .backend()
            .destroy_object(&self.registry.id());
        let _ = self.flush();
        self.lease = None;
    }
}

#[cfg(test)]
mod tests;

impl Drop for Capture {
    fn drop(&mut self) {
        self.release();
    }
}

impl State {
    fn deny(&mut self) {
        self.status = CaptureStatus::Denied;
        self.stopped = true;
        self.release_gesture.reset();
        self.release_requested = false;
        if let Some(inhibitor) = self.inhibitor.take() {
            inhibitor.destroy();
        }
    }

    fn maybe_inhibit(&mut self, handle: &QueueHandle<Self>) {
        if self.stopped || self.inhibitor.is_some() {
            return;
        }
        let Some((_, manager)) = &self.manager else {
            return;
        };
        let Some(seat) = self.focused_seat.and_then(|id| self.seats.get(&id)) else {
            return;
        };
        self.inhibitor = Some(manager.inhibit_shortcuts(&self.surface, &seat.proxy, handle, ()));
        // Sending the request is not proof of inhibition. Only Active below is.
    }
}

impl Dispatch<wl_registry::WlRegistry, ()> for State {
    fn event(
        state: &mut Self,
        registry: &wl_registry::WlRegistry,
        event: wl_registry::Event,
        _: &(),
        _: &Connection,
        handle: &QueueHandle<Self>,
    ) {
        if state.stopped {
            return;
        }
        match event {
            wl_registry::Event::Global {
                name,
                interface,
                version,
            } => {
                if interface == manager::ZwpKeyboardShortcutsInhibitManagerV1::interface().name {
                    state.manager = Some((name, registry.bind(name, version.min(1), handle, ())));
                    state.maybe_inhibit(handle);
                } else if interface == wl_seat::WlSeat::interface().name && version >= 5 {
                    // v5 supports explicit seat/keyboard release. Each seat gets
                    // its own listener so we identify the actual focused seat,
                    // rather than assuming the first advertised seat is GPUI's.
                    let proxy = registry.bind(name, version.min(7), handle, name);
                    state.seats.insert(
                        name,
                        Seat {
                            proxy,
                            keyboard: None,
                        },
                    );
                }
            }
            wl_registry::Event::GlobalRemove { name } => {
                if state.manager.as_ref().is_some_and(|(id, _)| *id == name)
                    || state.focused_seat == Some(name)
                {
                    state.deny();
                }
                if let Some(seat) = state.seats.remove(&name) {
                    if let Some(keyboard) = seat.keyboard {
                        keyboard.release();
                    }
                    seat.proxy.release();
                }
            }
            _ => {}
        }
    }
}

impl Dispatch<wl_callback::WlCallback, ()> for State {
    fn event(
        state: &mut Self,
        _: &wl_callback::WlCallback,
        _: wl_callback::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if state.manager.is_none() || state.seats.is_empty() {
            state.deny();
        }
    }
}

impl Dispatch<wl_seat::WlSeat, u32> for State {
    fn event(
        state: &mut Self,
        _: &wl_seat::WlSeat,
        event: wl_seat::Event,
        id: &u32,
        _: &Connection,
        handle: &QueueHandle<Self>,
    ) {
        if state.stopped {
            return;
        }
        if let wl_seat::Event::Capabilities {
            capabilities: WEnum::Value(capabilities),
        } = event
        {
            let Some(seat) = state.seats.get_mut(id) else {
                return;
            };
            if capabilities.contains(wl_seat::Capability::Keyboard) {
                if seat.keyboard.is_none() {
                    seat.keyboard = Some(seat.proxy.get_keyboard(handle, *id));
                }
            } else if let Some(keyboard) = seat.keyboard.take() {
                keyboard.release();
                if state.focused_seat == Some(*id) {
                    state.deny();
                }
            }
        }
    }
}

impl Dispatch<wl_keyboard::WlKeyboard, u32> for State {
    fn event(
        state: &mut Self,
        _: &wl_keyboard::WlKeyboard,
        event: wl_keyboard::Event,
        id: &u32,
        _: &Connection,
        handle: &QueueHandle<Self>,
    ) {
        if state.stopped {
            return;
        }
        match event {
            wl_keyboard::Event::Enter { surface, keys, .. }
                if surface.id() == state.surface.id() =>
            {
                if state.focused_seat.is_some_and(|focused| focused != *id) {
                    // GPUI exposes one keyboard. Without its seat identity,
                    // simultaneous focus from multiple seats is ambiguous;
                    // never claim exclusive capture for the wrong keyboard.
                    state.deny();
                    return;
                }
                let (keys, remainder) = keys.as_chunks::<4>();
                if !remainder.is_empty() {
                    state.deny();
                    return;
                }
                let held = keys
                    .iter()
                    .map(|bytes| u32::from_ne_bytes(*bytes))
                    .collect::<Vec<_>>();
                state.release_gesture.prime(&held);
                state.release_requested = false;
                state.focused_seat = Some(*id);
                state.maybe_inhibit(handle);
            }
            wl_keyboard::Event::Key {
                key,
                state: WEnum::Value(key_state),
                ..
            } if state.focused_seat == Some(*id) => {
                let pressed = match key_state {
                    wl_keyboard::KeyState::Pressed => true,
                    wl_keyboard::KeyState::Released => false,
                    _ => return,
                };
                if state.release_blocked {
                    state.release_gesture.interrupt();
                }
                if state.release_gesture.key_changed(key, pressed) {
                    state.release_requested = true;
                }
            }
            wl_keyboard::Event::Leave { surface, .. }
                if surface.id() == state.surface.id() && state.focused_seat == Some(*id) =>
            {
                state.focused_seat = None;
                state.deny();
            }
            // GPUI keeps its own wl_keyboard and handles all ordinary input,
            // layout, repeat and IME events. Raw keys here are only for the
            // release gesture, never a second path to the remote desktop.
            _ => {}
        }
    }
}

impl Dispatch<manager::ZwpKeyboardShortcutsInhibitManagerV1, ()> for State {
    fn event(
        _: &mut Self,
        _: &manager::ZwpKeyboardShortcutsInhibitManagerV1,
        _: manager::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<inhibitor::ZwpKeyboardShortcutsInhibitorV1, ()> for State {
    fn event(
        state: &mut Self,
        _: &inhibitor::ZwpKeyboardShortcutsInhibitorV1,
        event: inhibitor::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if state.stopped {
            return;
        }
        match event {
            inhibitor::Event::Active => state.status = CaptureStatus::Active,
            inhibitor::Event::Inactive => state.deny(),
            _ => {}
        }
    }
}
