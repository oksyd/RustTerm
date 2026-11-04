//! Owns an X11 clipboard selection while downloaded files are offered to the desktop.

use rustterm::i18n::messages as i18n_key;
use std::{path::PathBuf, sync::Arc};
use tempfile::TempDir;
use tokio_util::sync::CancellationToken;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ExportError {
    Unavailable,
    Changed,
    Failed,
    Invalid,
    TooLarge,
}

impl ExportError {
    pub(crate) fn message(self) -> &'static str {
        match self {
            Self::Unavailable => {
                i18n_key::RDP_FILE_CLIPBOARD_IS_UNAVAILABLE_IN_THIS_DESKTOP_SESSION
            }
            Self::Changed => i18n_key::RDP_THE_LOCAL_CLIPBOARD_CHANGED_COPY_THE_REMOTE_FILES_AGAIN,
            Self::Failed => i18n_key::RDP_COULD_NOT_OFFER_THE_REMOTE_FILES_TO_THE_LOCAL_CLIPBOARD,
            Self::Invalid => i18n_key::RDP_THE_DOWNLOADED_FILE_LIST_IS_INVALID,
            Self::TooLarge => i18n_key::RDP_THE_CLIPBOARD_FILE_LIST_IS_TOO_LARGE_SELECT_FEWER_FILES,
        }
    }
}

impl std::fmt::Display for ExportError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(rustterm::i18n::tr(self.message()))
    }
}
impl std::error::Error for ExportError {}

/// A live selection observer. Capture, begin and publish run on a background executor.
pub(crate) struct Snapshot {
    #[cfg(target_os = "linux")]
    worker: linux::Worker,
}

impl Snapshot {
    pub(crate) fn capture() -> Result<Self, ExportError> {
        #[cfg(target_os = "linux")]
        {
            Ok(Self {
                worker: linux::Worker::capture()?,
            })
        }
        #[cfg(not(target_os = "linux"))]
        {
            Err(ExportError::Unavailable)
        }
    }
}

pub(crate) struct Export {
    #[cfg(target_os = "linux")]
    owner: linux::Owner,
}

impl Export {
    /// Claims an empty selection only if no copy occurred since the snapshot.
    pub(crate) fn begin(snapshot: Snapshot) -> Result<Self, ExportError> {
        #[cfg(target_os = "linux")]
        {
            Ok(Self {
                owner: snapshot.worker.begin()?,
            })
        }
        #[cfg(not(target_os = "linux"))]
        {
            let _ = snapshot;
            Err(ExportError::Unavailable)
        }
    }

    /// The caller retains another directory reference until its chosen cleanup boundary:
    /// URI clipboard protocols do not acknowledge destination file-copy completion.
    pub(crate) fn publish(
        &self,
        directory: Arc<TempDir>,
        paths: Vec<PathBuf>,
    ) -> Result<(), ExportError> {
        let (gnome, uris) = encode(&paths)?;
        if paths.iter().any(|path| {
            !path.starts_with(directory.path())
                || path
                    .components()
                    .any(|component| matches!(component, std::path::Component::ParentDir))
                || !std::fs::symlink_metadata(path)
                    .is_ok_and(|metadata| metadata.is_file() || metadata.is_dir())
        }) {
            return Err(ExportError::Invalid);
        }
        #[cfg(target_os = "linux")]
        {
            self.owner.publish(linux::Payload {
                _directory: Some(directory),
                formats: vec![
                    ("x-special/gnome-copied-files", gnome.into()),
                    ("text/uri-list", uris.into()),
                ],
            })
        }
        #[cfg(not(target_os = "linux"))]
        {
            let _ = (directory, gnome, uris);
            Err(ExportError::Unavailable)
        }
    }

    /// Publishes all native representations under one selection ownership claim.
    pub(crate) fn publish_content(
        &self,
        formats: super::content::MimeData,
    ) -> Result<(), ExportError> {
        #[cfg(target_os = "linux")]
        {
            self.owner.publish(linux::Payload {
                _directory: None,
                formats,
            })
        }
        #[cfg(not(target_os = "linux"))]
        {
            let _ = formats;
            Err(ExportError::Unavailable)
        }
    }

    pub(crate) fn is_current(&self) -> bool {
        #[cfg(target_os = "linux")]
        {
            self.owner.is_current()
        }
        #[cfg(not(target_os = "linux"))]
        {
            false
        }
    }

    pub(crate) fn cancellation(&self) -> CancellationToken {
        #[cfg(target_os = "linux")]
        {
            self.owner.cancellation()
        }
        #[cfg(not(target_os = "linux"))]
        {
            let token = CancellationToken::new();
            token.cancel();
            token
        }
    }

    pub(crate) fn cancel(&self) {
        #[cfg(target_os = "linux")]
        self.owner.cancel();
    }
}

fn encode(paths: &[PathBuf]) -> Result<(Vec<u8>, Vec<u8>), ExportError> {
    if paths.is_empty() || paths.len() > super::MAX_FILES {
        return Err(ExportError::Invalid);
    }
    let mut gnome = b"copy".to_vec();
    let mut uris = Vec::new();
    for path in paths {
        if !path.is_absolute() {
            return Err(ExportError::Invalid);
        }
        #[cfg(unix)]
        let bytes = {
            use std::os::unix::ffi::OsStrExt;
            path.as_os_str().as_bytes()
        };
        #[cfg(not(unix))]
        let bytes = path.to_str().ok_or(ExportError::Invalid)?.as_bytes();
        let mut uri = b"file://".to_vec();
        for &byte in bytes {
            if byte == 0 {
                return Err(ExportError::Invalid);
            }
            if byte.is_ascii_alphanumeric() || matches!(byte, b'/' | b'-' | b'.' | b'_' | b'~') {
                uri.push(byte);
            } else {
                const HEX: &[u8; 16] = b"0123456789ABCDEF";
                uri.extend_from_slice(&[
                    b'%',
                    HEX[(byte >> 4) as usize],
                    HEX[(byte & 15) as usize],
                ]);
            }
            if uri.len() > super::MAX_LIST_BYTES {
                return Err(ExportError::TooLarge);
            }
        }
        if gnome.len() + uri.len() + 1 > super::MAX_LIST_BYTES
            || uris.len() + uri.len() + 2 > super::MAX_LIST_BYTES
        {
            return Err(ExportError::TooLarge);
        }
        // Nautilus rejects empty lines, including a trailing newline. This is
        // distinct from text/uri-list, whose entries end with CRLF.
        gnome.push(b'\n');
        gnome.extend_from_slice(&uri);
        uris.extend_from_slice(&uri);
        uris.extend_from_slice(b"\r\n");
    }
    Ok((gnome, uris))
}

#[cfg(target_os = "linux")]
mod linux {
    use super::*;
    use anyhow::{Result, bail};
    use rustix::{
        event::{PollFd, PollFlags, Timespec, poll},
        net::{AddressFamily, SocketAddrUnix, SocketFlags, SocketType, socket_with},
    };
    use std::{
        collections::VecDeque,
        io::{self, Read, Write},
        os::unix::net::UnixStream,
        sync::{
            Mutex,
            atomic::{AtomicBool, Ordering},
            mpsc,
        },
        time::{Duration, Instant},
    };
    use x11rb::{
        COPY_DEPTH_FROM_PARENT, COPY_FROM_PARENT, CURRENT_TIME, NONE,
        connection::Connection,
        protocol::{
            Event,
            xfixes::{ConnectionExt as _, SelectionEventMask},
            xproto::{
                AtomEnum, ChangeWindowAttributesAux, ConnectionExt as _, CreateWindowAux,
                EventMask, PropMode, Property, SELECTION_NOTIFY_EVENT, SelectionNotifyEvent,
                SelectionRequestEvent, WindowClass,
            },
        },
        reexports::x11rb_protocol::{
            parse_display::{ConnectAddress, parse_display},
            xauth::get_auth,
        },
        rust_connection::{DefaultStream, PollMode, RustConnection, Stream},
        utils::RawFdContainer,
        wrapper::ConnectionExt as _,
    };

    const OPERATION_TIMEOUT: Duration = Duration::from_secs(3);
    const TRANSFER_TIMEOUT: Duration = Duration::from_secs(30);
    const MAX_TRANSFERS: usize = 8;

    struct BoundedStream {
        stream: DefaultStream,
        cancelled: CancellationToken,
        deadline: Mutex<Instant>,
    }

    impl BoundedStream {
        fn renew(&self) {
            *self.deadline.lock().unwrap() = Instant::now() + OPERATION_TIMEOUT;
        }
        fn check(&self) -> io::Result<()> {
            if self.cancelled.is_cancelled() {
                Err(io::Error::new(
                    io::ErrorKind::ConnectionAborted,
                    "clipboard export cancelled",
                ))
            } else if Instant::now() >= *self.deadline.lock().unwrap() {
                Err(io::Error::new(
                    io::ErrorKind::TimedOut,
                    "clipboard export timed out",
                ))
            } else {
                Ok(())
            }
        }
    }

    impl Stream for BoundedStream {
        fn poll(&self, mode: PollMode) -> io::Result<()> {
            let mut flags = PollFlags::empty();
            if mode.readable() {
                flags |= PollFlags::IN;
            }
            if mode.writable() {
                flags |= PollFlags::OUT;
            }
            loop {
                self.check()?;
                let mut descriptors = [PollFd::new(&self.stream, flags)];
                match poll(
                    &mut descriptors,
                    Some(&Timespec {
                        tv_sec: 0,
                        tv_nsec: 50_000_000,
                    }),
                ) {
                    Ok(0) | Err(rustix::io::Errno::INTR) => {}
                    Ok(_) => return Ok(()),
                    Err(error) => return Err(error.into()),
                }
            }
        }
        fn read(&self, bytes: &mut [u8], fds: &mut Vec<RawFdContainer>) -> io::Result<usize> {
            self.check()?;
            self.stream.read(bytes, fds)
        }
        fn write(&self, bytes: &[u8], fds: &mut Vec<RawFdContainer>) -> io::Result<usize> {
            self.check()?;
            self.stream.write(bytes, fds)
        }
    }

    fn connect(cancelled: CancellationToken) -> Result<RustConnection<BoundedStream>> {
        let display = parse_display(None)?;
        for address in display.connect_instruction() {
            let ConnectAddress::Socket(path) = address else {
                continue;
            };
            for abstract_name in [true, false] {
                let address = if abstract_name {
                    SocketAddrUnix::new_abstract_name(path.as_bytes())?
                } else {
                    SocketAddrUnix::new(path.as_str())?
                };
                let fd = socket_with(
                    AddressFamily::UNIX,
                    SocketType::STREAM,
                    SocketFlags::CLOEXEC | SocketFlags::NONBLOCK,
                    None,
                )?;
                match rustix::net::connect(&fd, &address) {
                    Ok(()) => {}
                    Err(rustix::io::Errno::INPROGRESS) | Err(rustix::io::Errno::INTR) => {
                        let mut descriptors = [PollFd::new(&fd, PollFlags::OUT)];
                        if poll(
                            &mut descriptors,
                            Some(&Timespec {
                                tv_sec: 3,
                                tv_nsec: 0,
                            }),
                        )? == 0
                        {
                            bail!("X11 connection timed out");
                        }
                        rustix::net::sockopt::socket_error(&fd)??;
                    }
                    Err(_) => continue,
                }
                let (stream, (family, address)) =
                    DefaultStream::from_unix_stream(UnixStream::from(fd))?;
                let authority = std::env::var_os("XAUTHORITY")
                    .map(PathBuf::from)
                    .or_else(|| {
                        std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".Xauthority"))
                    });
                if let Some(path) = authority
                    && let Ok(metadata) = std::fs::metadata(path)
                    && (!metadata.is_file() || metadata.len() > super::super::MAX_LIST_BYTES as u64)
                {
                    bail!("invalid X11 authority file");
                }
                let (auth_name, auth_data) = match get_auth(family, &address, display.display) {
                    Ok(auth) => auth.unwrap_or_default(),
                    Err(error) if error.kind() == io::ErrorKind::NotFound => Default::default(),
                    Err(error) => return Err(error.into()),
                };
                return Ok(RustConnection::connect_to_stream_with_auth_info(
                    BoundedStream {
                        stream,
                        cancelled,
                        deadline: Mutex::new(Instant::now() + OPERATION_TIMEOUT),
                    },
                    display.screen.into(),
                    auth_name,
                    auth_data,
                )?);
            }
        }
        bail!("local X11 display is unavailable")
    }

    pub(super) struct Payload {
        pub(super) _directory: Option<Arc<TempDir>>,
        pub(super) formats: super::super::content::MimeData,
    }

    struct Publish {
        payload: Payload,
        reply: mpsc::SyncSender<Result<(), ExportError>>,
    }

    pub(super) struct Owner {
        commands: mpsc::SyncSender<Publish>,
        wake: UnixStream,
        current: Arc<AtomicBool>,
        cancelled: CancellationToken,
    }

    impl Owner {
        pub(super) fn is_current(&self) -> bool {
            self.current.load(Ordering::Acquire) && !self.cancelled.is_cancelled()
        }
        pub(super) fn cancellation(&self) -> CancellationToken {
            self.cancelled.clone()
        }
        pub(super) fn cancel(&self) {
            self.current.store(false, Ordering::Release);
            self.cancelled.cancel();
            wake(&self.wake);
        }
        pub(super) fn publish(&self, payload: Payload) -> Result<(), ExportError> {
            if !self.is_current() {
                return Err(ExportError::Changed);
            }
            let (reply, result) = mpsc::sync_channel(1);
            self.commands
                .try_send(Publish { payload, reply })
                .map_err(|_| ExportError::Failed)?;
            wake(&self.wake);
            match result.recv_timeout(OPERATION_TIMEOUT + Duration::from_secs(1)) {
                Ok(result) => result,
                Err(_) => {
                    self.cancel();
                    Err(ExportError::Failed)
                }
            }
        }
    }

    impl Drop for Owner {
        fn drop(&mut self) {
            self.cancel();
        }
    }

    fn wake(mut socket: &UnixStream) {
        let _ = socket.write(&[1]);
    }

    struct Atoms {
        clipboard: u32,
        targets: u32,
        timestamp: u32,
        incr: u32,
        formats: std::collections::BTreeMap<&'static str, u32>,
    }

    struct Transfer {
        requestor: u32,
        property: u32,
        target: u32,
        bytes: Arc<[u8]>,
        offset: usize,
        deadline: Instant,
    }

    pub(super) struct Worker {
        connection: RustConnection<BoundedStream>,
        window: u32,
        atoms: Atoms,
        snapshot_owner: u32,
        timestamp: u32,
        expected_claims: usize,
        current: Arc<AtomicBool>,
        cancelled: CancellationToken,
        payload: Option<Payload>,
        transfers: Vec<Transfer>,
        queued_events: VecDeque<Event>,
    }

    impl Drop for Worker {
        fn drop(&mut self) {
            self.current.store(false, Ordering::Release);
            self.cancelled.cancel();
        }
    }

    impl Worker {
        pub(super) fn capture() -> Result<Self, ExportError> {
            if std::env::var_os("DISPLAY").is_none() {
                return Err(ExportError::Unavailable);
            }
            Self::capture_inner().map_err(|_| ExportError::Failed)
        }

        fn capture_inner() -> Result<Self> {
            let cancelled = CancellationToken::new();
            let connection = connect(cancelled.clone())?;
            connection.xfixes_query_version(2, 0)?.reply()?;
            let window = connection.generate_id()?;
            connection
                .create_window(
                    COPY_DEPTH_FROM_PARENT,
                    window,
                    connection.setup().roots[0].root,
                    0,
                    0,
                    1,
                    1,
                    0,
                    WindowClass::INPUT_ONLY,
                    COPY_FROM_PARENT,
                    &CreateWindowAux::new(),
                )?
                .check()?;
            let atom = |name: &[u8]| -> Result<u32> {
                Ok(connection.intern_atom(false, name)?.reply()?.atom)
            };
            let atoms = Atoms {
                clipboard: atom(b"CLIPBOARD")?,
                targets: atom(b"TARGETS")?,
                timestamp: atom(b"TIMESTAMP")?,
                incr: atom(b"INCR")?,
                formats: super::super::MIME_TYPES
                    .into_iter()
                    .chain(super::super::CONTENT_MIME_TYPES)
                    .map(|mime| atom(mime.as_bytes()).map(|id| (mime, id)))
                    .collect::<Result<_>>()?,
            };
            connection
                .xfixes_select_selection_input(
                    window,
                    atoms.clipboard,
                    SelectionEventMask::SET_SELECTION_OWNER
                        | SelectionEventMask::SELECTION_WINDOW_DESTROY
                        | SelectionEventMask::SELECTION_CLIENT_CLOSE,
                )?
                .check()?;
            let snapshot_owner = connection
                .get_selection_owner(atoms.clipboard)?
                .reply()?
                .owner;
            Ok(Self {
                connection,
                window,
                atoms,
                snapshot_owner,
                timestamp: CURRENT_TIME,
                expected_claims: 0,
                current: Arc::new(AtomicBool::new(false)),
                cancelled,
                payload: None,
                transfers: Vec::new(),
                queued_events: VecDeque::new(),
            })
        }

        pub(super) fn begin(mut self) -> Result<Owner, ExportError> {
            self.connection.stream().renew();
            self.claim(self.snapshot_owner)?;
            let (commands, receiver) = mpsc::sync_channel(1);
            let (wake_write, wake_read) = UnixStream::pair().map_err(|_| ExportError::Failed)?;
            wake_write
                .set_nonblocking(true)
                .map_err(|_| ExportError::Failed)?;
            wake_read
                .set_nonblocking(true)
                .map_err(|_| ExportError::Failed)?;
            let owner = Owner {
                commands,
                wake: wake_write,
                current: self.current.clone(),
                cancelled: self.cancelled.clone(),
            };
            self.current.store(true, Ordering::Release);
            std::thread::Builder::new()
                .name("rdp-file-clipboard".into())
                .spawn(move || {
                    let _ = self.run(receiver, wake_read);
                    self.current.store(false, Ordering::Release);
                    self.cancelled.cancel();
                })
                .map_err(|_| ExportError::Failed)?;
            Ok(owner)
        }

        fn claim(&mut self, expected_owner: u32) -> Result<(), ExportError> {
            // X11 has no conditional ownership operation. Keep the server grab to
            // this owner query + claim; no file I/O or channel operations occur here.
            self.connection
                .grab_server()
                .map_err(|_| ExportError::Failed)?;
            let result = (|| -> Result<(), ExportError> {
                let actual = self
                    .connection
                    .get_selection_owner(self.atoms.clipboard)
                    .map_err(|_| ExportError::Failed)?
                    .reply()
                    .map_err(|_| ExportError::Failed)?
                    .owner;
                if actual != expected_owner {
                    return Err(ExportError::Changed);
                }
                while let Some(event) = self
                    .connection
                    .poll_for_event()
                    .map_err(|_| ExportError::Failed)?
                {
                    if matches!(event, Event::XfixesSelectionNotify(ref event) if event.selection == self.atoms.clipboard)
                        || matches!(event, Event::SelectionClear(ref event) if event.selection == self.atoms.clipboard)
                    {
                        return Err(ExportError::Changed);
                    }
                    if self.queued_events.len() == 1024 {
                        return Err(ExportError::Failed);
                    }
                    self.queued_events.push_back(event);
                }
                self.expected_claims += 1;
                self.connection
                    .set_selection_owner(self.window, self.atoms.clipboard, CURRENT_TIME)
                    .map_err(|_| ExportError::Failed)?;
                Ok(())
            })();
            // Queue the ungrab before waiting for a reply, including on rejection.
            let ungrabbed = self
                .connection
                .ungrab_server()
                .map_err(|_| ExportError::Failed)?
                .check()
                .map_err(|_| ExportError::Failed);
            result.and(ungrabbed)
        }

        fn run(
            &mut self,
            commands: mpsc::Receiver<Publish>,
            mut wake_read: UnixStream,
        ) -> Result<()> {
            loop {
                if self.cancelled.is_cancelled() {
                    return Ok(());
                }
                self.connection.stream().renew();
                loop {
                    let event = match self.queued_events.pop_front() {
                        Some(event) => Some(event),
                        None => self.connection.poll_for_event()?,
                    };
                    let Some(event) = event else {
                        break;
                    };
                    self.handle_event(event)?;
                    if !self.current.load(Ordering::Acquire) {
                        return Ok(());
                    }
                }
                let mut published = false;
                while let Ok(command) = commands.try_recv() {
                    let result = if self.payload.is_some() {
                        Err(ExportError::Invalid)
                    } else {
                        self.claim(self.window)
                    };
                    if result.is_ok() {
                        self.payload = Some(command.payload);
                        published = true;
                    }
                    let _ = command.reply.send(result);
                    if result == Err(ExportError::Changed) {
                        return Ok(());
                    }
                }
                if published {
                    continue;
                }
                for index in (0..self.transfers.len()).rev() {
                    if self.transfers[index].deadline <= Instant::now() {
                        self.finish_transfer(index)?;
                    }
                }
                self.connection.flush()?;
                let timeout = self
                    .transfers
                    .iter()
                    .map(|transfer| transfer.deadline)
                    .min()
                    .map(|deadline| {
                        let remaining = deadline.saturating_duration_since(Instant::now());
                        Timespec {
                            tv_sec: remaining.as_secs() as _,
                            tv_nsec: remaining.subsec_nanos() as _,
                        }
                    });
                let mut descriptors = [
                    PollFd::new(&self.connection.stream().stream, PollFlags::IN),
                    PollFd::new(&wake_read, PollFlags::IN),
                ];
                match poll(&mut descriptors, timeout.as_ref()) {
                    Ok(_) | Err(rustix::io::Errno::INTR) => {}
                    Err(error) => return Err(error.into()),
                }
                let mut buffer = [0; 64];
                loop {
                    match wake_read.read(&mut buffer) {
                        Ok(0) => return Ok(()),
                        Ok(_) => {}
                        Err(error) if error.kind() == io::ErrorKind::WouldBlock => break,
                        Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
                        Err(error) => return Err(error.into()),
                    }
                }
            }
        }

        fn handle_event(&mut self, event: Event) -> Result<()> {
            match event {
                Event::XfixesSelectionNotify(event) if event.selection == self.atoms.clipboard => {
                    if event.owner == self.window && self.expected_claims > 0 {
                        self.expected_claims -= 1;
                        self.timestamp = event.selection_timestamp;
                    } else {
                        self.current.store(false, Ordering::Release);
                    }
                }
                Event::SelectionClear(event) if event.selection == self.atoms.clipboard => {
                    self.current.store(false, Ordering::Release);
                }
                Event::SelectionRequest(request) => self.request(request)?,
                Event::PropertyNotify(event) if event.state == Property::DELETE => {
                    if let Some(index) = self.transfers.iter().position(|transfer| {
                        transfer.requestor == event.window && transfer.property == event.atom
                    }) {
                        let chunk = self.chunk_size();
                        let transfer = &mut self.transfers[index];
                        let end = (transfer.offset + chunk).min(transfer.bytes.len());
                        self.connection.change_property8(
                            PropMode::REPLACE,
                            transfer.requestor,
                            transfer.property,
                            transfer.target,
                            &transfer.bytes[transfer.offset..end],
                        )?;
                        if end == transfer.offset {
                            self.finish_transfer(index)?;
                        } else {
                            transfer.offset = end;
                            transfer.deadline = Instant::now() + TRANSFER_TIMEOUT;
                        }
                    }
                }
                Event::DestroyNotify(event) => self
                    .transfers
                    .retain(|transfer| transfer.requestor != event.window),
                _ => {}
            }
            Ok(())
        }

        fn chunk_size(&self) -> usize {
            (usize::from(self.connection.setup().maximum_request_length) * 4)
                .saturating_sub(24)
                .min(64 * 1024)
                & !3
        }

        fn finish_transfer(&mut self, index: usize) -> Result<()> {
            let transfer = self.transfers.swap_remove(index);
            if !self
                .transfers
                .iter()
                .any(|other| other.requestor == transfer.requestor)
            {
                self.connection.change_window_attributes(
                    transfer.requestor,
                    &ChangeWindowAttributesAux::new().event_mask(EventMask::NO_EVENT),
                )?;
            }
            Ok(())
        }

        fn request(&mut self, request: SelectionRequestEvent) -> Result<()> {
            let property = if request.property == NONE {
                request.target
            } else {
                request.property
            };
            let valid = request.selection == self.atoms.clipboard
                && request.owner == self.window
                && (request.time == CURRENT_TIME
                    || (request.time.wrapping_sub(self.timestamp) as i32) >= 0);
            let accepted = if !valid {
                false
            } else if request.target == self.atoms.targets {
                let mut targets = vec![self.atoms.targets, self.atoms.timestamp];
                if let Some(payload) = &self.payload {
                    targets.extend(
                        payload
                            .formats
                            .iter()
                            .filter_map(|(mime, _)| self.atoms.formats.get(mime).copied()),
                    );
                }
                self.connection.change_property32(
                    PropMode::REPLACE,
                    request.requestor,
                    property,
                    AtomEnum::ATOM,
                    &targets,
                )?;
                true
            } else if request.target == self.atoms.timestamp {
                self.connection.change_property32(
                    PropMode::REPLACE,
                    request.requestor,
                    property,
                    AtomEnum::INTEGER,
                    &[self.timestamp],
                )?;
                true
            } else if let Some(bytes) = self.payload.as_ref().and_then(|payload| {
                payload
                    .formats
                    .iter()
                    .find(|(mime, _)| self.atoms.formats.get(mime) == Some(&request.target))
                    .map(|(_, bytes)| bytes.clone())
            }) {
                if bytes.len() <= self.chunk_size() {
                    self.connection.change_property8(
                        PropMode::REPLACE,
                        request.requestor,
                        property,
                        request.target,
                        &bytes,
                    )?;
                    true
                } else if self.transfers.len() < MAX_TRANSFERS
                    && !self.transfers.iter().any(|transfer| {
                        transfer.requestor == request.requestor && transfer.property == property
                    })
                {
                    self.connection.change_window_attributes(
                        request.requestor,
                        &ChangeWindowAttributesAux::new()
                            .event_mask(EventMask::PROPERTY_CHANGE | EventMask::STRUCTURE_NOTIFY),
                    )?;
                    self.connection.change_property32(
                        PropMode::REPLACE,
                        request.requestor,
                        property,
                        self.atoms.incr,
                        &[bytes.len() as u32],
                    )?;
                    self.transfers.push(Transfer {
                        requestor: request.requestor,
                        property,
                        target: request.target,
                        bytes,
                        offset: 0,
                        deadline: Instant::now() + TRANSFER_TIMEOUT,
                    });
                    true
                } else {
                    false
                }
            } else {
                false
            };
            self.connection.send_event(
                false,
                request.requestor,
                EventMask::NO_EVENT,
                SelectionNotifyEvent {
                    response_type: SELECTION_NOTIFY_EVENT,
                    sequence: 0,
                    time: request.time,
                    requestor: request.requestor,
                    selection: request.selection,
                    target: request.target,
                    property: if accepted { property } else { NONE },
                },
            )?;
            Ok(())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn encoded_files_round_trip_without_treating_names_as_uri_syntax() {
        let paths = vec![
            PathBuf::from("/tmp/中文 a#b?c%\n.txt"),
            PathBuf::from("/tmp/second.txt"),
        ];
        let (gnome, uris) = encode(&paths).unwrap();
        assert_eq!(
            super::super::parse("x-special/gnome-copied-files", &gnome).unwrap(),
            paths
        );
        assert_eq!(super::super::parse("text/uri-list", &uris).unwrap(), paths);
        assert!(uris.windows(3).any(|bytes| bytes == b"%0A"));
        assert!(gnome.starts_with(b"copy\n"));
    }

    #[test]
    fn gnome_files_have_no_empty_lines_or_trailing_separator() {
        let paths = vec![
            PathBuf::from("/tmp/first file.txt"),
            PathBuf::from("/tmp/second.txt"),
        ];
        let (gnome, uris) = encode(&paths).unwrap();
        assert_eq!(
            gnome,
            b"copy\nfile:///tmp/first%20file.txt\nfile:///tmp/second.txt"
        );
        assert_eq!(
            uris,
            b"file:///tmp/first%20file.txt\r\nfile:///tmp/second.txt\r\n"
        );
        // Nautilus splits with g_strsplit, which preserves an empty final field.
        assert!(
            gnome
                .split(|byte| *byte == b'\n')
                .all(|line| !line.is_empty())
        );

        let (single, _) = encode(&paths[..1]).unwrap();
        assert_eq!(single, b"copy\nfile:///tmp/first%20file.txt");
    }

    #[test]
    fn rejects_empty_relative_and_excessive_file_lists() {
        assert_eq!(encode(&[]), Err(ExportError::Invalid));
        assert_eq!(
            encode(&[PathBuf::from("relative")]),
            Err(ExportError::Invalid)
        );
        assert_eq!(
            encode(&vec![PathBuf::from("/tmp/a"); super::super::MAX_FILES + 1]),
            Err(ExportError::Invalid)
        );
        assert_eq!(
            encode(&[PathBuf::from(format!(
                "/{}",
                "a".repeat(super::super::MAX_LIST_BYTES)
            ))]),
            Err(ExportError::TooLarge)
        );
    }

    #[cfg(unix)]
    #[test]
    fn preserves_non_utf8_local_names() {
        use std::os::unix::ffi::OsStringExt;
        let paths = vec![PathBuf::from(std::ffi::OsString::from_vec(
            b"/tmp/\xff".to_vec(),
        ))];
        let (_, uris) = encode(&paths).unwrap();
        assert_eq!(uris, b"file:///tmp/%FF\r\n");
        assert_eq!(super::super::parse("text/uri-list", &uris).unwrap(), paths);
    }
}
