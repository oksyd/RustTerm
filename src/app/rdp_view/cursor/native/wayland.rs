use super::Image;
use anyhow::{Result, bail};
use gpui::{Bounds, Pixels, Window, point, px};
use raw_window_handle::{HasDisplayHandle, HasWindowHandle, RawDisplayHandle, RawWindowHandle};
use std::{
    collections::BTreeMap,
    io::{ErrorKind, Write},
    os::fd::AsFd,
    ptr::NonNull,
};
use wayland_backend::client::{Backend, ObjectId, WaylandError};
use wayland_client::{
    Connection, Dispatch, EventQueue, Proxy, QueueHandle, WEnum,
    protocol::{
        wl_buffer, wl_compositor, wl_pointer, wl_registry, wl_seat, wl_shm, wl_shm_pool, wl_surface,
    },
};

pub(super) struct Cursor {
    connection: Connection,
    queue: EventQueue<State>,
    registry: wl_registry::WlRegistry,
    state: State,
}

struct Seat {
    proxy: wl_seat::WlSeat,
    pointer: Option<wl_pointer::WlPointer>,
    serial: Option<u32>,
    installed: bool,
}

struct State {
    target: wl_surface::WlSurface,
    compositor: Option<wl_compositor::WlCompositor>,
    shm: Option<wl_shm::WlShm>,
    surface: Option<wl_surface::WlSurface>,
    buffer: Option<wl_buffer::WlBuffer>,
    hotspot: (i32, i32),
    bounds: Option<Bounds<Pixels>>,
    seats: BTreeMap<u32, Seat>,
}

impl Cursor {
    pub fn new(window: &Window) -> Result<Self> {
        let RawDisplayHandle::Wayland(display) = window.display_handle()?.as_raw() else {
            bail!("cursor display is not Wayland");
        };
        let RawWindowHandle::Wayland(surface) = HasWindowHandle::window_handle(window)?.as_raw()
        else {
            bail!("cursor window is not Wayland");
        };
        // SAFETY: GPUI owns both handles. RdpView releases this cursor before
        // the window; the foreign connection never owns the shared display.
        unsafe { Self::from_handles(display.display, surface.surface) }
    }

    unsafe fn from_handles(
        display: NonNull<std::ffi::c_void>,
        surface: NonNull<std::ffi::c_void>,
    ) -> Result<Self> {
        // SAFETY: caller guarantees the lifetime and common display identity.
        let backend = unsafe { Backend::from_foreign_display(display.as_ptr().cast()) };
        let connection = Connection::from_backend(backend);
        // SAFETY: borrow GPUI's surface as an argument only, without replacing
        // its listener, registering ownership, or sending its destructor.
        let id = unsafe {
            ObjectId::from_ptr(wl_surface::WlSurface::interface(), surface.as_ptr().cast())
        }?;
        let target = wl_surface::WlSurface::from_id(&connection, id)?;
        let queue = connection.new_event_queue();
        let registry = connection.display().get_registry(&queue.handle(), ());
        let this = Self {
            connection,
            queue,
            registry,
            state: State {
                target,
                compositor: None,
                shm: None,
                surface: None,
                buffer: None,
                hotspot: (0, 0),
                bounds: None,
                seats: BTreeMap::new(),
            },
        };
        this.flush()?;
        Ok(this)
    }

    fn flush(&self) -> Result<()> {
        match self.connection.flush() {
            Err(WaylandError::Io(error)) if error.kind() == ErrorKind::WouldBlock => Ok(()),
            result => Ok(result?),
        }
    }

    pub fn poll(&mut self) -> Result<()> {
        // GPUI remains the sole display reader. Dispatch only our private
        // queue; never roundtrip or block the UI thread waiting for globals.
        self.queue.dispatch_pending(&mut self.state)?;
        self.flush()
    }

    pub fn prepare(&mut self, image: &Image) -> Result<bool> {
        let (Some(compositor), Some(shm)) = (&self.state.compositor, &self.state.shm) else {
            return Ok(false);
        };
        let qh = self.queue.handle();
        let surface = self
            .state
            .surface
            .get_or_insert_with(|| compositor.create_surface(&qh, ()));
        // Each immutable image has its own anonymous backing file. Destroying
        // the previous buffer/pool cannot overwrite pixels still in use by the
        // compositor; it owns the transferred fd until it releases the pool.
        let mut file = tempfile::tempfile()?;
        file.write_all(&image.pixels)?;
        let pool = shm.create_pool(file.as_fd(), image.pixels.len() as i32, &qh, ());
        let buffer = pool.create_buffer(
            0,
            image.width as i32,
            image.height as i32,
            image.width as i32 * 4,
            wl_shm::Format::Argb8888,
            &qh,
            (),
        );
        pool.destroy();
        surface.set_buffer_scale(image.scale);
        surface.attach(Some(&buffer), 0, 0);
        surface.damage_buffer(0, 0, image.width as i32, image.height as i32);
        surface.commit();
        if let Some(previous) = self.state.buffer.replace(buffer) {
            previous.destroy();
        }
        self.state.hotspot = image.hotspot;
        for seat in self.state.seats.values_mut() {
            seat.installed = false;
        }
        self.flush()?;
        Ok(true)
    }

    pub fn set_bounds(&mut self, bounds: Option<Bounds<Pixels>>, _device_scale: f32) {
        self.state.bounds = bounds;
    }

    pub fn show(&mut self, hovered: bool) -> Result<bool> {
        let mut active = false;
        for seat in self.state.seats.values_mut() {
            let (Some(pointer), Some(serial), Some(surface)) =
                (&seat.pointer, seat.serial, &self.state.surface)
            else {
                continue;
            };
            if hovered {
                if !seat.installed {
                    pointer.set_cursor(
                        serial,
                        Some(surface),
                        self.state.hotspot.0,
                        self.state.hotspot.1,
                    );
                    seat.installed = true;
                }
                active = true;
            } else {
                seat.installed = false;
            }
        }
        self.flush()?;
        Ok(active)
    }

    pub fn hide(&mut self, hovered: bool) -> Result<()> {
        for seat in self.state.seats.values_mut() {
            if seat.installed
                && hovered
                && let (Some(pointer), Some(serial)) = (&seat.pointer, seat.serial)
            {
                pointer.set_cursor(serial, None, 0, 0);
            }
            seat.installed = false;
        }
        self.flush()
    }

    pub fn suspend(&mut self) {
        self.state.bounds = None;
        // GPUI restores the hit-tested local cursor when remote control ends.
        for seat in self.state.seats.values_mut() {
            seat.installed = false;
        }
    }
}

impl Drop for Cursor {
    fn drop(&mut self) {
        for (_, seat) in std::mem::take(&mut self.state.seats) {
            if let Some(pointer) = seat.pointer {
                pointer.release();
            }
            seat.proxy.release();
        }
        if let Some(surface) = self.state.surface.take() {
            surface.destroy();
        }
        if let Some(buffer) = self.state.buffer.take() {
            buffer.destroy();
        }
        // These globals have no destructor at the versions we bind.
        for id in self
            .state
            .compositor
            .take()
            .map(|p| p.id())
            .into_iter()
            .chain(self.state.shm.take().map(|p| p.id()))
            .chain(std::iter::once(self.registry.id()))
        {
            let _ = self.connection.backend().destroy_object(&id);
        }
        let _ = self.flush();
    }
}

impl Dispatch<wl_registry::WlRegistry, ()> for State {
    fn event(
        state: &mut Self,
        registry: &wl_registry::WlRegistry,
        event: wl_registry::Event,
        _: &(),
        _: &Connection,
        qh: &QueueHandle<Self>,
    ) {
        match event {
            wl_registry::Event::Global {
                name,
                interface,
                version,
            } => match interface.as_str() {
                "wl_compositor" if version >= 4 => {
                    state.compositor = Some(registry.bind(name, 4, qh, ()))
                }
                "wl_shm" => state.shm = Some(registry.bind(name, 1, qh, ())),
                "wl_seat" if version >= 5 => {
                    state.seats.insert(
                        name,
                        Seat {
                            proxy: registry.bind(name, 5, qh, name),
                            pointer: None,
                            serial: None,
                            installed: false,
                        },
                    );
                }
                _ => {}
            },
            wl_registry::Event::GlobalRemove { name } => {
                if let Some(seat) = state.seats.remove(&name) {
                    if let Some(pointer) = seat.pointer {
                        pointer.release();
                    }
                    seat.proxy.release();
                }
            }
            _ => {}
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
        qh: &QueueHandle<Self>,
    ) {
        if let wl_seat::Event::Capabilities {
            capabilities: WEnum::Value(capabilities),
        } = event
            && let Some(seat) = state.seats.get_mut(id)
        {
            if capabilities.contains(wl_seat::Capability::Pointer) {
                if seat.pointer.is_none() {
                    seat.pointer = Some(seat.proxy.get_pointer(qh, *id));
                }
            } else if let Some(pointer) = seat.pointer.take() {
                pointer.release();
                seat.serial = None;
                seat.installed = false;
            }
        }
    }
}

impl Dispatch<wl_pointer::WlPointer, u32> for State {
    fn event(
        state: &mut Self,
        _: &wl_pointer::WlPointer,
        event: wl_pointer::Event,
        id: &u32,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        let Some(seat) = state.seats.get_mut(id) else {
            return;
        };
        match event {
            wl_pointer::Event::Enter {
                serial, surface, ..
            } => {
                seat.serial = (surface.id() == state.target.id()).then_some(serial);
                seat.installed = false;
            }
            wl_pointer::Event::Leave { .. } => {
                seat.serial = None;
                seat.installed = false;
            }
            wl_pointer::Event::Motion {
                surface_x,
                surface_y,
                ..
            } if !state.bounds.is_some_and(|bounds| {
                bounds.contains(&point(px(surface_x as f32), px(surface_y as f32)))
            }) =>
            {
                seat.installed = false;
            }
            _ => {}
        }
    }
}

wayland_client::delegate_noop!(State: ignore wl_compositor::WlCompositor);
wayland_client::delegate_noop!(State: ignore wl_shm::WlShm);
wayland_client::delegate_noop!(State: ignore wl_shm_pool::WlShmPool);
wayland_client::delegate_noop!(State: ignore wl_surface::WlSurface);
wayland_client::delegate_noop!(State: ignore wl_buffer::WlBuffer);

#[cfg(test)]
mod tests;
