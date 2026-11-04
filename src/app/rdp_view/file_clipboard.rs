//! Reads native clipboard representations and explicitly offered local file URIs.

use rustterm::i18n::messages as i18n_key;
mod content;
pub(super) mod export;

#[cfg(target_os = "linux")]
pub(super) use content::from_mime;
pub(super) use content::to_mime;

pub(super) enum SelectionData {
    Files(Vec<PathBuf>),
    Content(rustrdp::ClipboardContent),
}

pub(super) const CONTENT_MIME_TYPES: [&str; 7] = [
    "text/html",
    "text/rtf",
    "application/rtf",
    "image/png",
    "text/plain;charset=utf-8",
    "UTF8_STRING",
    "text/plain",
];
const MAX_CONTENT_BYTES: usize = rustterm::rdp::MAX_CLIPBOARD_CONTENT_BYTES;

use std::{
    collections::HashSet,
    path::PathBuf,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
};

pub(super) const MIME_TYPES: [&str; 2] = ["x-special/gnome-copied-files", "text/uri-list"];
const MAX_LIST_BYTES: usize = 4 * 1024 * 1024;
const MAX_FILES: usize = rustrdp::MAX_CLIPBOARD_FILES;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum ReadError {
    Unavailable,
    Failed,
    TimedOut,
    Cancelled,
    Changed,
    TooLarge,
    Invalid,
    Nonlocal,
    Cut,
}

impl ReadError {
    pub(super) fn message(self) -> &'static str {
        match self {
            Self::Unavailable => i18n_key::RDP_FILE_CLIPBOARD_IS_UNAVAILABLE_USE_SEND_FILES_INSTEAD,
            Self::Failed => i18n_key::RDP_COULD_NOT_READ_FILES_FROM_THE_LOCAL_CLIPBOARD,
            Self::TimedOut => i18n_key::RDP_READING_THE_LOCAL_FILE_CLIPBOARD_TIMED_OUT,
            Self::Cancelled => i18n_key::RDP_FILE_PASTE_WAS_CANCELLED,
            Self::Changed => {
                i18n_key::RDP_THE_CLIPBOARD_CHANGED_WHILE_READING_FILES_COPY_AND_PASTE_AGAIN
            }
            Self::TooLarge => i18n_key::RDP_THE_CLIPBOARD_FILE_LIST_IS_TOO_LARGE_SELECT_FEWER_FILES,
            Self::Invalid => i18n_key::RDP_THE_CLIPBOARD_CONTAINS_AN_INVALID_FILE_LIST,
            Self::Nonlocal => i18n_key::RDP_ONLY_LOCAL_FILES_CAN_BE_PASTED_INTO_THE_REMOTE_DESKTOP,
            Self::Cut => i18n_key::RDP_USE_COPY_INSTEAD_OF_CUT_TO_SEND_FILES_TO_THE,
        }
    }
}

/// Call on a background executor. No selection data is cached between paste gestures.
/// Xwayland can provide the same X11 clipboard bridge in a Wayland session.
pub(super) fn read(
    cancel: Arc<AtomicBool>,
    files: bool,
) -> Result<Option<SelectionData>, ReadError> {
    if cancel.load(Ordering::Relaxed) {
        return Err(ReadError::Cancelled);
    }
    #[cfg(target_os = "linux")]
    {
        x11::read(cancel, files)
    }
    #[cfg(not(target_os = "linux"))]
    {
        let _ = files;
        Err(ReadError::Unavailable)
    }
}

pub(super) fn parse(mime: &str, bytes: &[u8]) -> Result<Vec<PathBuf>, ReadError> {
    if bytes.len() > MAX_LIST_BYTES {
        return Err(ReadError::TooLarge);
    }
    let text = std::str::from_utf8(bytes).map_err(|_| ReadError::Invalid)?;
    let mut lines = text.lines();
    match mime {
        "x-special/gnome-copied-files" => match lines.next() {
            Some("copy") => {}
            Some("cut") => return Err(ReadError::Cut),
            _ => return Err(ReadError::Invalid),
        },
        "text/uri-list" => {}
        _ => return Err(ReadError::Invalid),
    }
    let mut paths = Vec::new();
    let mut seen = HashSet::new();
    for line in lines {
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let path = local_uri(line)?;
        if seen.insert(path.clone()) {
            if paths.len() == MAX_FILES {
                return Err(ReadError::TooLarge);
            }
            paths.push(path);
        }
    }
    if paths.is_empty() {
        return Err(ReadError::Invalid);
    }
    Ok(paths)
}

fn local_uri(uri: &str) -> Result<PathBuf, ReadError> {
    let (scheme, tail) = uri.split_once(':').ok_or(ReadError::Invalid)?;
    if !scheme.eq_ignore_ascii_case("file") {
        return Err(ReadError::Nonlocal);
    }
    let path = if let Some(authority) = tail.strip_prefix("//") {
        let slash = authority.find('/').ok_or(ReadError::Invalid)?;
        let (host, path) = authority.split_at(slash);
        if !host.is_empty() && !host.eq_ignore_ascii_case("localhost") {
            return Err(ReadError::Nonlocal);
        }
        path
    } else {
        tail
    };
    if !path.starts_with('/') || path.contains(['?', '#', '\0']) {
        return Err(ReadError::Invalid);
    }
    let mut decoded = Vec::with_capacity(path.len());
    let mut bytes = path.as_bytes().iter().copied();
    while let Some(byte) = bytes.next() {
        let byte = if byte == b'%' {
            let high = bytes.next().and_then(hex).ok_or(ReadError::Invalid)?;
            let low = bytes.next().and_then(hex).ok_or(ReadError::Invalid)?;
            high * 16 + low
        } else {
            byte
        };
        if byte == 0 {
            return Err(ReadError::Invalid);
        }
        decoded.push(byte);
    }
    #[cfg(unix)]
    {
        use std::os::unix::ffi::OsStringExt;
        Ok(std::ffi::OsString::from_vec(decoded).into())
    }
    #[cfg(not(unix))]
    {
        String::from_utf8(decoded)
            .map(PathBuf::from)
            .map_err(|_| ReadError::Invalid)
    }
}

fn hex(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        b'A'..=b'F' => Some(byte - b'A' + 10),
        _ => None,
    }
}

#[cfg(target_os = "linux")]
mod x11 {
    use super::*;
    use anyhow::{Context as _, Result, bail};
    use rustix::{
        event::{PollFd, PollFlags, Timespec, poll},
        net::{AddressFamily, SocketAddrUnix, SocketFlags, SocketType, socket_with},
    };
    use std::{
        io,
        os::{fd::AsFd, unix::net::UnixStream},
        time::{Duration, Instant},
    };
    use x11rb::{
        COPY_DEPTH_FROM_PARENT, COPY_FROM_PARENT, CURRENT_TIME, NONE,
        connection::Connection,
        protocol::{
            Event,
            xfixes::{ConnectionExt as _, SelectionEventMask},
            xproto::{
                AtomEnum, ConnectionExt as _, CreateWindowAux, EventMask, GetPropertyReply,
                Property, WindowClass,
            },
        },
        reexports::x11rb_protocol::{
            parse_display::{ConnectAddress, parse_display},
            xauth::get_auth,
        },
        rust_connection::{DefaultStream, PollMode, RustConnection, Stream},
        utils::RawFdContainer,
    };

    const TIMEOUT: Duration = Duration::from_secs(3);

    struct Deadline {
        end: Instant,
        cancelled: Arc<AtomicBool>,
    }

    impl Deadline {
        fn check(&self) -> io::Result<()> {
            if self.cancelled.load(Ordering::Relaxed) {
                return Err(io::Error::new(
                    io::ErrorKind::ConnectionAborted,
                    "clipboard cancelled",
                ));
            }
            if Instant::now() >= self.end {
                return Err(io::Error::new(
                    io::ErrorKind::TimedOut,
                    "clipboard timed out",
                ));
            }
            Ok(())
        }

        fn wait(&self, fd: &impl AsFd, events: PollFlags) -> io::Result<()> {
            loop {
                self.check()?;
                let wait = self
                    .end
                    .saturating_duration_since(Instant::now())
                    .min(Duration::from_millis(50));
                let timeout = Timespec {
                    tv_sec: 0,
                    tv_nsec: wait.as_nanos() as _,
                };
                let mut descriptors = [PollFd::new(fd, events)];
                match poll(&mut descriptors, Some(&timeout)) {
                    Ok(0) | Err(rustix::io::Errno::INTR) => {}
                    Ok(_) => return Ok(()),
                    Err(error) => return Err(error.into()),
                }
            }
        }

        fn error(&self) -> ReadError {
            if self.cancelled.load(Ordering::Relaxed) {
                ReadError::Cancelled
            } else if Instant::now() >= self.end {
                ReadError::TimedOut
            } else {
                ReadError::Failed
            }
        }
    }

    struct TimedStream {
        stream: DefaultStream,
        deadline: Arc<Deadline>,
    }

    impl Stream for TimedStream {
        fn poll(&self, mode: PollMode) -> io::Result<()> {
            let mut flags = PollFlags::empty();
            if mode.readable() {
                flags |= PollFlags::IN;
            }
            if mode.writable() {
                flags |= PollFlags::OUT;
            }
            self.deadline.wait(&self.stream, flags)
        }
        fn read(&self, bytes: &mut [u8], fds: &mut Vec<RawFdContainer>) -> io::Result<usize> {
            self.deadline.check()?;
            self.stream.read(bytes, fds)
        }
        fn write(&self, bytes: &[u8], fds: &mut Vec<RawFdContainer>) -> io::Result<usize> {
            self.deadline.check()?;
            self.stream.write(bytes, fds)
        }
    }

    fn connect(deadline: Arc<Deadline>) -> Result<RustConnection<TimedStream>> {
        let display = parse_display(None)?;
        for address in display.connect_instruction() {
            // File-manager clipboard integration is local to this desktop.
            let ConnectAddress::Socket(path) = address else {
                continue;
            };
            for abstract_name in [true, false] {
                deadline.check()?;
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
                        deadline.wait(&fd, PollFlags::OUT)?;
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
                    && (!metadata.is_file() || metadata.len() > MAX_CONTENT_BYTES as u64)
                {
                    bail!("invalid X11 authority file");
                }
                let (auth_name, auth_data) = match get_auth(family, &address, display.display) {
                    Ok(auth) => auth.unwrap_or_default(),
                    Err(error) if error.kind() == io::ErrorKind::NotFound => Default::default(),
                    Err(error) => return Err(error.into()),
                };
                return Ok(RustConnection::connect_to_stream_with_auth_info(
                    TimedStream { stream, deadline },
                    display.screen.into(),
                    auth_name,
                    auth_data,
                )?);
            }
        }
        bail!("local X11 display is unavailable")
    }

    struct Selection {
        connection: RustConnection<TimedStream>,
        window: u32,
        clipboard: u32,
        property: u32,
        incr: u32,
    }

    impl Selection {
        fn new(connection: RustConnection<TimedStream>) -> Result<Self> {
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
                    &CreateWindowAux::new().event_mask(EventMask::PROPERTY_CHANGE),
                )?
                .check()?;
            let clipboard = connection.intern_atom(false, b"CLIPBOARD")?.reply()?.atom;
            let property = connection
                .intern_atom(false, b"RUSTTERM_FILE_CLIPBOARD")?
                .reply()?
                .atom;
            let incr = connection.intern_atom(false, b"INCR")?.reply()?.atom;
            connection
                .xfixes_select_selection_input(
                    window,
                    clipboard,
                    SelectionEventMask::SET_SELECTION_OWNER
                        | SelectionEventMask::SELECTION_WINDOW_DESTROY
                        | SelectionEventMask::SELECTION_CLIENT_CLOSE,
                )?
                .check()?;
            Ok(Self {
                connection,
                window,
                clipboard,
                property,
                incr,
            })
        }

        fn next(&self) -> Result<Event> {
            loop {
                self.connection.stream().deadline.check()?;
                if let Some(event) = self.connection.poll_for_event()? {
                    if matches!(&event, Event::XfixesSelectionNotify(event) if event.selection == self.clipboard)
                    {
                        return Err(ReadError::Changed.into());
                    }
                    return Ok(event);
                }
                self.connection.stream().poll(PollMode::Readable)?;
            }
        }

        fn property(&self) -> Result<GetPropertyReply> {
            let reply = self
                .connection
                .get_property(
                    true,
                    self.window,
                    self.property,
                    AtomEnum::ANY,
                    0,
                    (MAX_CONTENT_BYTES / 4) as u32,
                )?
                .reply()?;
            if reply.bytes_after != 0 || reply.value.len() > MAX_CONTENT_BYTES {
                return Err(ReadError::TooLarge.into());
            }
            Ok(reply)
        }

        fn request(
            &self,
            target: u32,
            expected_type: u32,
            format: u8,
        ) -> Result<Option<GetPropertyReply>> {
            self.connection
                .delete_property(self.window, self.property)?
                .check()?;
            self.connection.convert_selection(
                self.window,
                self.clipboard,
                target,
                self.property,
                CURRENT_TIME,
            )?;
            self.connection.flush()?;
            loop {
                if let Event::SelectionNotify(event) = self.next()?
                    && event.requestor == self.window
                    && event.selection == self.clipboard
                    && event.target == target
                {
                    if event.property == NONE {
                        return Ok(None);
                    }
                    if event.property != self.property {
                        bail!("unexpected clipboard property");
                    }
                    break;
                }
            }
            let mut reply = self.property()?;
            if reply.type_ != self.incr {
                return Ok(Some(reply));
            }
            if reply.format != 32 || reply.value.len() != 4 {
                return Err(ReadError::Invalid.into());
            }
            if reply
                .value32()
                .and_then(|mut lengths| lengths.next())
                .context("missing INCR length")? as usize
                > MAX_CONTENT_BYTES
            {
                return Err(ReadError::TooLarge.into());
            }
            let mut value = Vec::new();
            // Deleting the INCR property acknowledges readiness for the first chunk.
            self.connection.flush()?;
            loop {
                if !matches!(self.next()?, Event::PropertyNotify(event)
                    if event.window == self.window && event.atom == self.property && event.state == Property::NEW_VALUE)
                {
                    continue;
                }
                reply = self.property()?;
                if reply.type_ != expected_type || reply.format != format {
                    return Err(ReadError::Invalid.into());
                }
                if reply.value.is_empty() {
                    reply.value = value;
                    reply.value_len = (reply.value.len() / (format as usize / 8)) as u32;
                    return Ok(Some(reply));
                }
                if reply.value.len() > MAX_CONTENT_BYTES - value.len() {
                    return Err(ReadError::TooLarge.into());
                }
                value.extend_from_slice(&reply.value);
                self.connection.flush()?;
            }
        }

        fn read(&self, files: bool) -> Result<Option<SelectionData>> {
            let owner = self
                .connection
                .get_selection_owner(self.clipboard)?
                .reply()?
                .owner;
            if owner == NONE {
                return Ok(None);
            }
            let targets = self
                .connection
                .intern_atom(false, b"TARGETS")?
                .reply()?
                .atom;
            let Some(reply) = self.request(targets, AtomEnum::ATOM.into(), 32)? else {
                return Ok(None);
            };
            if reply.type_ != u32::from(AtomEnum::ATOM) || reply.format != 32 {
                return Err(ReadError::Invalid.into());
            }
            if reply.value_len > 256 {
                return Err(ReadError::TooLarge.into());
            }
            let targets = reply
                .value32()
                .context("invalid target list")?
                .collect::<HashSet<_>>();
            for mime in MIME_TYPES.into_iter().filter(|_| files) {
                let target = self
                    .connection
                    .intern_atom(false, mime.as_bytes())?
                    .reply()?
                    .atom;
                if !targets.contains(&target) {
                    continue;
                }
                let reply = self.request(target, target, 8)?.ok_or(ReadError::Changed)?;
                if reply.type_ != target || reply.format != 8 {
                    return Err(ReadError::Invalid.into());
                }
                self.verify_owner(owner)?;
                return parse(mime, &reply.value)
                    .map(SelectionData::Files)
                    .map(Some)
                    .map_err(Into::into);
            }
            let mut representations = Vec::new();
            let mut total = 0usize;
            for mime in CONTENT_MIME_TYPES {
                let target = self
                    .connection
                    .intern_atom(false, mime.as_bytes())?
                    .reply()?
                    .atom;
                if !targets.contains(&target) {
                    continue;
                }
                let reply = self.request(target, target, 8)?.ok_or(ReadError::Changed)?;
                if reply.type_ != target || reply.format != 8 {
                    return Err(ReadError::Invalid.into());
                }
                total = total.saturating_add(reply.value.len());
                if total > MAX_CONTENT_BYTES {
                    return Err(ReadError::TooLarge.into());
                }
                representations.push((mime, reply.value));
            }
            self.verify_owner(owner)?;
            let content = from_mime(representations)?;
            Ok((!content.is_empty()).then_some(SelectionData::Content(content)))
        }

        fn verify_owner(&self, owner: u32) -> Result<()> {
            // A round trip queues all preceding notifications, including a new
            // selection published by the same application window.
            if self
                .connection
                .get_selection_owner(self.clipboard)?
                .reply()?
                .owner
                != owner
            {
                return Err(ReadError::Changed.into());
            }
            while let Some(event) = self.connection.poll_for_event()? {
                if matches!(event, Event::XfixesSelectionNotify(event) if event.selection == self.clipboard)
                {
                    return Err(ReadError::Changed.into());
                }
            }
            Ok(())
        }
    }

    pub(super) fn read(
        cancelled: Arc<AtomicBool>,
        files: bool,
    ) -> Result<Option<SelectionData>, ReadError> {
        if std::env::var_os("DISPLAY").is_none() {
            return Err(ReadError::Unavailable);
        }
        let deadline = Arc::new(Deadline {
            end: Instant::now() + TIMEOUT,
            cancelled,
        });
        let result = connect(deadline.clone())
            .and_then(Selection::new)
            .and_then(|selection| selection.read(files));
        result.map_err(|error| {
            error
                .downcast_ref::<ReadError>()
                .copied()
                .unwrap_or_else(|| deadline.error())
        })
    }
}

impl std::fmt::Display for ReadError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(rustterm::i18n::tr(self.message()))
    }
}
impl std::error::Error for ReadError {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_escaped_local_files_and_preserves_order() {
        assert_eq!(parse("x-special/gnome-copied-files", b"copy\nfile:///tmp/a%20b.txt\nfile://localhost/tmp/%E4%B8%AD%E6%96%87.txt\nfile:///tmp/a%20b.txt\n"),
            Ok(vec![PathBuf::from("/tmp/a b.txt"), PathBuf::from("/tmp/中文.txt")]));
        assert_eq!(
            parse("text/uri-list", b"# comment\r\nfile:///tmp/a%23b%3Fc\r\n"),
            Ok(vec![PathBuf::from("/tmp/a#b?c")])
        );
    }

    #[test]
    fn rejects_cut_remote_and_malformed_uris() {
        assert_eq!(
            parse(MIME_TYPES[0], b"cut\nfile:///tmp/a"),
            Err(ReadError::Cut)
        );
        for value in [
            "https://example.org/a",
            "file://server/share/a",
            "file://user@localhost/a",
        ] {
            assert_eq!(
                parse(MIME_TYPES[1], value.as_bytes()),
                Err(ReadError::Nonlocal)
            );
        }
        for value in [
            "/tmp/a",
            "file:relative",
            "file:///tmp/a?b",
            "file:///tmp/a#b",
            "file:///tmp/%",
            "file:///tmp/%ZZ",
            "file:///tmp/%00",
        ] {
            assert_eq!(
                parse(MIME_TYPES[1], value.as_bytes()),
                Err(ReadError::Invalid),
                "{value}"
            );
        }
        assert_eq!(
            parse("text/plain", b"file:///tmp/a"),
            Err(ReadError::Invalid)
        );
    }

    #[test]
    fn bounds_file_count_and_payload_size() {
        let list = (0..=MAX_FILES)
            .map(|index| format!("file:///tmp/{index}\n"))
            .collect::<String>();
        assert_eq!(
            parse(MIME_TYPES[1], list.as_bytes()),
            Err(ReadError::TooLarge)
        );
        assert_eq!(
            parse(MIME_TYPES[1], &vec![b'a'; MAX_CONTENT_BYTES + 1]),
            Err(ReadError::TooLarge)
        );
        assert_eq!(
            parse(MIME_TYPES[1], b"# nothing\n"),
            Err(ReadError::Invalid)
        );
    }
}
