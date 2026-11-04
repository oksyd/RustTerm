//! X11 visibility is observed on a separate connection; rendering only reads an atomic.
//! Wayland deliberately stays visible here and relies on compositor frame callbacks.

use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};
use tokio::sync::watch;

struct State {
    visible: AtomicBool,
    stopped: AtomicBool,
    changes: watch::Sender<bool>,
}

impl State {
    fn new() -> Self {
        Self {
            visible: AtomicBool::new(true),
            stopped: AtomicBool::new(false),
            changes: watch::channel(true).0,
        }
    }

    fn set_visible(&self, visible: bool) {
        if self.visible.swap(visible, Ordering::AcqRel) != visible {
            self.changes.send_replace(visible);
        }
    }
}

struct Monitor {
    state: Arc<State>,
}

impl Drop for Monitor {
    fn drop(&mut self) {
        // The observer owns State, not Monitor. The last public clone stops observation.
        // There is no join or X11 round trip on the UI thread.
        self.state.stopped.store(true, Ordering::Release);
    }
}

#[derive(Clone)]
pub struct WindowVisibility {
    monitor: Arc<Monitor>,
}

impl WindowVisibility {
    pub fn new(_window: &gpui::Window) -> Self {
        let state = Arc::new(State::new());
        #[cfg(target_os = "linux")]
        if gpui::guess_compositor() == "X11" {
            let worker_state = state.clone();
            // Failure is conservative: a working window must never become permanently
            // frozen because its optional visibility observer could not connect.
            let _ = std::thread::Builder::new()
                .name("rustterm-x11-visibility".into())
                .spawn(move || {
                    if x11::observe(&worker_state).is_err() {
                        worker_state.set_visible(true);
                    }
                });
        }
        Self {
            monitor: Arc::new(Monitor { state }),
        }
    }

    /// False means X11 explicitly reports this window unmapped or hidden.
    /// Unsupported backends and observation errors return true, not a guessed state.
    pub fn is_visible(&self) -> bool {
        self.monitor.state.visible.load(Ordering::Acquire)
    }

    pub fn subscribe(&self) -> watch::Receiver<bool> {
        self.monitor.state.changes.subscribe()
    }
}

#[cfg(target_os = "linux")]
mod x11 {
    use super::{Ordering, State};
    use anyhow::{Context as _, Result, bail};
    use rustix::{
        event::{PollFd, PollFlags, Timespec, poll},
        net::{AddressFamily, SocketAddrUnix, SocketFlags, SocketType, socket_with},
    };
    use std::{
        collections::HashSet,
        io,
        net::{IpAddr, SocketAddr, TcpStream},
        os::{
            fd::{AsFd, OwnedFd},
            unix::net::UnixStream,
        },
        path::PathBuf,
        sync::Arc,
        time::{Duration, Instant},
    };
    use x11rb::{
        connection::Connection,
        protocol::{
            Event,
            xproto::{AtomEnum, ChangeWindowAttributesAux, ConnectionExt, EventMask, MapState},
        },
        reexports::x11rb_protocol::{
            parse_display::{ConnectAddress, parse_display},
            xauth::{Family, get_auth},
        },
        rust_connection::{DefaultStream, PollMode, RustConnection, Stream},
        utils::RawFdContainer,
    };

    const IO_TIMEOUT: Duration = Duration::from_secs(2);
    const CANCEL_INTERVAL: Duration = Duration::from_millis(100);
    const MAX_CLIENTS: u32 = 1024;
    const WM_CLASS: &[u8] = b"io.rustterm.RustTerm\0io.rustterm.RustTerm";
    type ConnectedStream = (DefaultStream, (Family, Vec<u8>));

    fn matches_class(value: &[u8]) -> bool {
        // GPUI 0.2.2 writes instance\0class without the optional trailing NUL.
        value == WM_CLASS || value.strip_suffix(b"\0") == Some(WM_CLASS)
    }

    fn find_window(
        connection: &RustConnection<CancellableStream>,
        clients_atom: u32,
        pid_atom: u32,
        state: &State,
    ) -> Result<Option<u32>> {
        let mut clients = HashSet::new();
        for screen in &connection.setup().roots {
            check_stopped(state)?;
            let property = connection
                .get_property(
                    false,
                    screen.root,
                    clients_atom,
                    AtomEnum::WINDOW,
                    0,
                    MAX_CLIENTS,
                )?
                .reply()?;
            if property.type_ == u32::from(AtomEnum::NONE) {
                continue;
            }
            if property.bytes_after != 0
                || property.format != 32
                || property.type_ != u32::from(AtomEnum::WINDOW)
            {
                bail!("invalid X11 client list");
            }
            clients.extend(property.value32().context("invalid X11 client list")?);
            if clients.len() > MAX_CLIENTS as usize {
                bail!("X11 client list exceeds discovery limit");
            }
        }
        let mut found = None;
        for window in clients {
            check_stopped(state)?;
            let pid = connection
                .get_property(false, window, pid_atom, AtomEnum::CARDINAL, 0, 2)?
                .reply()?;
            // GPUI writes std::process::id(), including inside a PID namespace.
            if pid.bytes_after != 0
                || pid.type_ != u32::from(AtomEnum::CARDINAL)
                || pid.format != 32
                || !pid.value32().is_some_and(|mut values| {
                    values.next() == Some(std::process::id()) && values.next().is_none()
                })
            {
                continue;
            }
            let class = connection
                .get_property(false, window, AtomEnum::WM_CLASS, AtomEnum::STRING, 0, 64)?
                .reply()?;
            if class.bytes_after == 0
                && class.type_ == u32::from(AtomEnum::STRING)
                && class.format == 8
                && matches_class(&class.value)
                && found.replace(window).is_some()
            {
                bail!("multiple RustTerm windows share this process identity");
            }
        }
        Ok(found)
    }

    fn check_stopped(state: &State) -> io::Result<()> {
        if state.stopped.load(Ordering::Acquire) {
            Err(io::Error::new(
                io::ErrorKind::ConnectionAborted,
                "visibility observer stopped",
            ))
        } else {
            Ok(())
        }
    }

    /// Every socket wait observes cancellation. Readiness wakes immediately; idle windows
    /// generate no X requests and only a small cancellation check every 100 ms.
    fn wait(
        fd: &impl AsFd,
        flags: PollFlags,
        state: &State,
        timeout: Duration,
    ) -> io::Result<bool> {
        let deadline = Instant::now() + timeout;
        loop {
            check_stopped(state)?;
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                return Ok(false);
            }
            let slice = remaining.min(CANCEL_INTERVAL);
            let timeout = Timespec {
                tv_sec: slice.as_secs() as _,
                tv_nsec: slice.subsec_nanos() as _,
            };
            let mut descriptors = [PollFd::new(fd, flags)];
            match poll(&mut descriptors, Some(&timeout)) {
                Ok(0) => {}
                Ok(_) => return Ok(true),
                Err(rustix::io::Errno::INTR) => {}
                Err(error) => return Err(error.into()),
            }
        }
    }

    struct CancellableStream {
        stream: DefaultStream,
        state: Arc<State>,
    }

    impl Stream for CancellableStream {
        fn poll(&self, mode: PollMode) -> io::Result<()> {
            let mut flags = PollFlags::empty();
            if mode.readable() {
                flags |= PollFlags::IN;
            }
            if mode.writable() {
                flags |= PollFlags::OUT;
            }
            if wait(&self.stream, flags, &self.state, IO_TIMEOUT)? {
                Ok(())
            } else {
                Err(io::Error::new(
                    io::ErrorKind::TimedOut,
                    "visibility X11 request timed out",
                ))
            }
        }
        fn read(&self, buffer: &mut [u8], fds: &mut Vec<RawFdContainer>) -> io::Result<usize> {
            check_stopped(&self.state)?;
            self.stream.read(buffer, fds)
        }
        fn write(&self, buffer: &[u8], fds: &mut Vec<RawFdContainer>) -> io::Result<usize> {
            check_stopped(&self.state)?;
            self.stream.write(buffer, fds)
        }
    }

    fn finish_connect(fd: &OwnedFd, result: rustix::io::Result<()>, state: &State) -> Result<()> {
        match result {
            Ok(()) => {}
            Err(rustix::io::Errno::INPROGRESS) | Err(rustix::io::Errno::INTR) => {
                if !wait(fd, PollFlags::OUT, state, IO_TIMEOUT)? {
                    bail!("X11 connection timed out");
                }
                rustix::net::sockopt::socket_error(fd)??;
            }
            Err(error) => return Err(error.into()),
        }
        Ok(())
    }

    fn unix_connection(path: &str, abstract_name: bool, state: &State) -> Result<ConnectedStream> {
        check_stopped(state)?;
        let address = if abstract_name {
            SocketAddrUnix::new_abstract_name(path.as_bytes())?
        } else {
            SocketAddrUnix::new(path)?
        };
        let fd = socket_with(
            AddressFamily::UNIX,
            SocketType::STREAM,
            SocketFlags::CLOEXEC | SocketFlags::NONBLOCK,
            None,
        )?;
        finish_connect(&fd, rustix::net::connect(&fd, &address), state)?;
        Ok(DefaultStream::from_unix_stream(UnixStream::from(fd))?)
    }

    fn tcp_connection(address: SocketAddr, state: &State) -> Result<ConnectedStream> {
        check_stopped(state)?;
        let family = if address.is_ipv4() {
            AddressFamily::INET
        } else {
            AddressFamily::INET6
        };
        let fd = socket_with(
            family,
            SocketType::STREAM,
            SocketFlags::CLOEXEC | SocketFlags::NONBLOCK,
            None,
        )?;
        finish_connect(&fd, rustix::net::connect(&fd, &address), state)?;
        Ok(DefaultStream::from_tcp_stream(TcpStream::from(fd))?)
    }

    fn connection(state: &Arc<State>) -> Result<RustConnection<CancellableStream>> {
        let display = parse_display(None)?;
        let mut connected = None;
        for address in display.connect_instruction() {
            let candidate = match address {
                ConnectAddress::Socket(path) => unix_connection(&path, true, state)
                    .or_else(|_| unix_connection(&path, false, state)),
                ConnectAddress::Hostname(host, port) => {
                    // Do not introduce an uncancellable DNS resolver into a lifetime-bound
                    // observer. Local and numeric remote DISPLAYs work; named remote
                    // displays conservatively retain visible=true.
                    let ip = if host == "localhost" {
                        Some(IpAddr::from([127, 0, 0, 1]))
                    } else {
                        host.parse().ok()
                    };
                    let Some(ip) = ip else {
                        continue;
                    };
                    tcp_connection(SocketAddr::new(ip, port), state)
                }
                _ => continue,
            };
            if let Ok(stream) = candidate {
                connected = Some(stream);
                break;
            }
            check_stopped(state)?;
        }
        let (stream, (family, address)) =
            connected.context("visibility X11 connection unavailable")?;
        // Reject FIFOs/devices and oversized auth files before the upstream blocking reader.
        let authority = std::env::var_os("XAUTHORITY")
            .map(PathBuf::from)
            .or_else(|| {
                std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".Xauthority"))
            });
        if let Some(path) = authority
            && let Ok(metadata) = std::fs::metadata(path)
            && (!metadata.is_file() || metadata.len() > 1024 * 1024)
        {
            bail!("visibility X11 authority file is not a bounded regular file");
        }
        let (auth_name, auth_data) = get_auth(family, &address, display.display)
            .unwrap_or(None)
            .unwrap_or_default();
        Ok(RustConnection::connect_to_stream_with_auth_info(
            CancellableStream {
                stream,
                state: state.clone(),
            },
            display.screen.into(),
            auth_name,
            auth_data,
        )?)
    }

    fn refresh(
        connection: &RustConnection<CancellableStream>,
        window: u32,
        state_atom: u32,
        hidden_atom: u32,
        state: &State,
    ) -> Result<()> {
        let attributes = connection.get_window_attributes(window)?;
        let properties =
            connection.get_property(false, window, state_atom, AtomEnum::ATOM, 0, 256)?;
        let attributes = attributes.reply()?;
        let properties = properties.reply()?;
        if properties.bytes_after != 0 || !matches!(properties.format, 0 | 32) {
            bail!("invalid X11 state property");
        }
        if ![u32::from(AtomEnum::NONE), u32::from(AtomEnum::ATOM)].contains(&properties.type_) {
            bail!("invalid X11 state property type");
        }
        if ![MapState::VIEWABLE, MapState::UNMAPPED, MapState::UNVIEWABLE]
            .contains(&attributes.map_state)
        {
            bail!("unknown X11 map state");
        }
        let hidden = properties
            .value32()
            .is_some_and(|mut atoms| atoms.any(|atom| atom == hidden_atom));
        state.set_visible(attributes.map_state == MapState::VIEWABLE && !hidden);
        Ok(())
    }

    pub(super) fn observe(state: &Arc<State>) -> Result<()> {
        let connection = connection(state)?;
        let clients_atom = connection
            .intern_atom(false, b"_NET_CLIENT_LIST")?
            .reply()?
            .atom;
        let pid_atom = connection.intern_atom(false, b"_NET_WM_PID")?.reply()?.atom;
        // X11Window::window_handle is unimplemented in GPUI 0.2.2. Discover only
        // this application's unique window on the worker, without native handles
        // or synchronous X11 requests on the UI thread.
        for screen in &connection.setup().roots {
            connection
                .change_window_attributes(
                    screen.root,
                    &ChangeWindowAttributesAux::new().event_mask(EventMask::PROPERTY_CHANGE),
                )?
                .check()?;
        }
        let deadline = Instant::now() + IO_TIMEOUT;
        let window = loop {
            if let Some(window) = find_window(&connection, clients_atom, pid_atom, state)? {
                break window;
            }
            if Instant::now() >= deadline {
                bail!("RustTerm window is not available for visibility observation");
            }
            // The window manager may register the new client after Workspace::new.
            check_stopped(state)?;
            std::thread::sleep(CANCEL_INTERVAL);
        };
        // Subscribe before the initial read so a transition between them cannot be lost.
        connection
            .change_window_attributes(
                window,
                &ChangeWindowAttributesAux::new()
                    .event_mask(EventMask::PROPERTY_CHANGE | EventMask::STRUCTURE_NOTIFY),
            )?
            .check()?;
        let state_atom = connection
            .intern_atom(false, b"_NET_WM_STATE")?
            .reply()?
            .atom;
        let hidden_atom = connection
            .intern_atom(false, b"_NET_WM_STATE_HIDDEN")?
            .reply()?
            .atom;
        refresh(&connection, window, state_atom, hidden_atom, state)?;
        while !state.stopped.load(Ordering::Acquire) {
            let mut changed = false;
            let mut identity_changed = false;
            // Bound a burst so continuous property traffic cannot starve cancellation.
            for _ in 0..256 {
                let Some(event) = connection.poll_for_event()? else {
                    break;
                };
                match event {
                    Event::PropertyNotify(event)
                        if event.window == window && event.atom == state_atom =>
                    {
                        changed = true
                    }
                    Event::PropertyNotify(event)
                        if (event.atom == clients_atom
                            && connection
                                .setup()
                                .roots
                                .iter()
                                .any(|screen| screen.root == event.window))
                            || (event.window == window
                                && (event.atom == pid_atom
                                    || event.atom == u32::from(AtomEnum::WM_CLASS))) =>
                    {
                        identity_changed = true;
                    }
                    Event::MapNotify(event) if event.window == window => changed = true,
                    Event::UnmapNotify(event) if event.window == window => changed = true,
                    Event::ReparentNotify(event) if event.window == window => changed = true,
                    Event::DestroyNotify(event) if event.window == window => {
                        state.set_visible(false);
                        return Ok(());
                    }
                    _ => {}
                }
            }
            if identity_changed
                && find_window(&connection, clients_atom, pid_atom, state)? != Some(window)
            {
                bail!("RustTerm visibility window identity changed");
            }
            if changed {
                refresh(&connection, window, state_atom, hidden_atom, state)?;
            }
            // poll_for_event drains X11's userspace event queue before waiting on its fd.
            // A short upper bound also lets the final monitor Drop stop an idle observer.
            let _ = wait(
                &connection.stream().stream,
                PollFlags::IN,
                state,
                Duration::from_millis(250),
            )?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn last_monitor_clone_requests_stop_without_a_display() {
        let state = Arc::new(State::new());
        let first = WindowVisibility {
            monitor: Arc::new(Monitor {
                state: state.clone(),
            }),
        };
        let second = first.clone();
        drop(first);
        assert!(!state.stopped.load(Ordering::Acquire));
        assert!(second.is_visible());
        state.set_visible(false);
        assert!(!second.is_visible());
        drop(second);
        assert!(state.stopped.load(Ordering::Acquire));
    }

    #[test]
    fn visibility_changes_wake_consumers_without_repeating_the_current_state() {
        let state = State::new();
        let mut updates = state.changes.subscribe();
        state.set_visible(false);
        assert!(updates.has_changed().unwrap());
        assert!(!*updates.borrow_and_update());
        state.set_visible(false);
        assert!(!updates.has_changed().unwrap());
        state.set_visible(true);
        assert!(updates.has_changed().unwrap());
        assert!(*updates.borrow_and_update());
        assert!(state.visible.load(Ordering::Acquire));
        state.set_visible(false);
        state.set_visible(true);
        assert!(updates.has_changed().unwrap());
        assert!(*updates.borrow_and_update());
    }
}
