use std::collections::{BTreeMap, BTreeSet, VecDeque};

use crate::{
    Error, Result,
    clipboard::{
        ClipboardContent, ClipboardFile, ClipboardFileRequest, MAX_CLIPBOARD_FILE_CHUNK,
        MAX_CLIPBOARD_FILES,
    },
    wire::Reader,
};

use super::{Action, ChannelEvent};

const MONITOR_READY: u16 = 1;
const FORMAT_LIST: u16 = 2;
const FORMAT_LIST_RESPONSE: u16 = 3;
const DATA_REQUEST: u16 = 4;
const DATA_RESPONSE: u16 = 5;
const TEMP_DIRECTORY: u16 = 6;
const CAPABILITIES: u16 = 7;
const FILE_REQUEST: u16 = 8;
const FILE_RESPONSE: u16 = 9;
const LOCK: u16 = 10;
const UNLOCK: u16 = 11;
const OK: u16 = 1;
const FAIL: u16 = 2;
const UNICODE_TEXT: u32 = 13;
const FILE_GROUP: u32 = 0xc000;
const FILE_GROUP_NAME: &str = "FileGroupDescriptorW";
const HTML: u32 = 0xc001;
const RTF: u32 = 0xc002;
const PNG: u32 = 0xc003;
const LONG_NAMES: u32 = 2;
const FILE_STREAMS: u32 = 4;
const NO_FILE_PATHS: u32 = 8;
const CAN_LOCK: u32 = 16;
const HUGE_FILES: u32 = 32;
const MAX_FORMATS: usize = 256;
const MAX_LOCKS: usize = 4;
const MAX_FILE_REQUESTS: usize = 16;
const FILE_DESCRIPTOR_SIZE: usize = 592;

pub(super) struct Clipboard {
    limit: usize,
    max_message: usize,
    files_enabled: bool,
    ready: bool,
    peer_flags: u32,
    long_names: bool,
    // An outstanding format-list ACK always belongs to offered, even after
    // ownership changes. Pending data never takes over that untagged ACK slot.
    local: LocalData,
    offered: Option<LocalData>,
    pending: Option<LocalData>,
    remote_unicode: bool,
    remote_formats: Vec<RemoteFormat>,
    remaining_formats: VecDeque<RemoteFormat>,
    remote_content: ClipboardContent,
    remote_rich: bool,
    generation: u64,
    requested: Option<DataRequest>,
    wanted: Option<RemoteFormat>,
    remote_files: Option<RemoteFiles>,
    remote_reads: BTreeMap<u32, ClipboardFileRequest>,
    next_stream_id: u32,
    next_lock_id: u32,
    files: BTreeMap<u64, Vec<ClipboardFile>>,
    locks: BTreeMap<u32, u64>,
    file_requests: BTreeMap<u64, FileRequest>,
    last_offer_id: Option<u64>,
    next_request_id: u64,
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum LocalData {
    Empty,
    Text(Vec<u8>),
    Content(Vec<Representation>),
    Files(u64),
}

impl LocalData {
    fn file_id(&self) -> Option<u64> {
        match self {
            Self::Files(id) => Some(*id),
            Self::Empty | Self::Text(_) | Self::Content(_) => None,
        }
    }
}

struct FileRequest {
    request: ClipboardFileRequest,
    stream_id: u32,
}

#[derive(Clone, Copy)]
enum RemoteFormat {
    Text,
    Files(u32),
    Content(u32, ContentFormat),
}

#[derive(Clone, Copy)]
enum ContentFormat {
    Html,
    Rtf,
    Png,
    Dib,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct Representation {
    id: u32,
    name: &'static str,
    data: Vec<u8>,
}

struct DataRequest {
    generation: u64,
    format: RemoteFormat,
}

struct RemoteFiles {
    id: u64,
    files: Option<Vec<ClipboardFile>>,
    lock: Option<u32>,
}

#[derive(Default)]
struct Formats {
    unicode: bool,
    files: Option<u32>,
    html: Option<u32>,
    rtf: Option<u32>,
    png: Option<u32>,
    dib: Option<u32>,
}

impl Formats {
    fn content(&self) -> Vec<RemoteFormat> {
        let mut formats = Vec::new();
        if self.unicode {
            formats.push(RemoteFormat::Text);
        }
        for (id, kind) in [
            (self.html, ContentFormat::Html),
            (self.rtf, ContentFormat::Rtf),
            (self.png, ContentFormat::Png),
            (
                self.png.is_none().then_some(self.dib).flatten(),
                ContentFormat::Dib,
            ),
        ] {
            if let Some(id) = id {
                formats.push(RemoteFormat::Content(id, kind));
            }
        }
        formats
    }
}

impl Clipboard {
    pub(super) fn new(limit: usize, files_enabled: bool, max_message: usize) -> Self {
        Self {
            limit,
            max_message,
            files_enabled,
            ready: false,
            peer_flags: 0,
            long_names: false,
            local: LocalData::Empty,
            offered: None,
            pending: None,
            remote_unicode: false,
            remote_formats: Vec::new(),
            remaining_formats: VecDeque::new(),
            remote_content: ClipboardContent::default(),
            remote_rich: false,
            generation: 0,
            requested: None,
            wanted: None,
            remote_files: None,
            remote_reads: BTreeMap::new(),
            next_stream_id: 0,
            next_lock_id: 0,
            files: BTreeMap::new(),
            locks: BTreeMap::new(),
            file_requests: BTreeMap::new(),
            last_offer_id: None,
            next_request_id: 0,
        }
    }

    pub(super) fn receive(&mut self, data: &[u8]) -> Result<Vec<Action>> {
        let mut reader = Reader::new(data);
        let kind = reader.u16_le()?;
        let flags = reader.u16_le()?;
        let length = reader.u32_le()? as usize;
        let body = reader.take(length)?;
        // MS-RDPECLIP section 6 permits an extra four-byte Windows trailer.
        if !reader.is_empty() && reader.remaining_len() != 4 {
            return Err(Error::Invalid("clipboard PDU length"));
        }
        let mut actions = match kind {
            CAPABILITIES => {
                require_flags(flags, 0)?;
                self.capabilities(body)?;
                Vec::new()
            }
            MONITOR_READY => {
                require_flags(flags, 0)?;
                require_empty(body)?;
                if self.ready {
                    return Err(Error::Invalid("duplicate clipboard monitor ready"));
                }
                self.ready = true;
                let features = LONG_NAMES
                    | if self.files_enabled {
                        FILE_STREAMS | NO_FILE_PATHS | CAN_LOCK | HUGE_FILES
                    } else {
                        0
                    };
                let mut caps = vec![1, 0, 0, 0, 1, 0, 12, 0, 2, 0, 0, 0];
                caps.extend_from_slice(&features.to_le_bytes());
                let data = self.pending.take().unwrap_or(LocalData::Empty);
                vec![
                    Action::Send(pdu(CAPABILITIES, 0, &caps)),
                    self.advertise(data),
                ]
            }
            FORMAT_LIST => {
                if flags & !4 != 0 {
                    return Err(Error::Invalid("clipboard format list flags"));
                }
                let formats = parse_formats(body, self.long_names, flags & 4 != 0)?;
                self.invalidate()?;
                let mut actions = Vec::new();
                self.release_remote(&mut actions);
                let rejected = self
                    .pending
                    .as_ref()
                    .is_some_and(|data| *data != LocalData::Empty)
                    || self
                        .offered
                        .as_ref()
                        .is_some_and(|data| *data != LocalData::Empty);
                self.local = LocalData::Empty;
                self.pending = None;
                if let Some(offer) = &mut self.offered {
                    *offer = LocalData::Empty;
                }
                self.remote_unicode = formats.unicode;
                self.remote_formats = formats.content();
                self.remote_rich = self
                    .remote_formats
                    .iter()
                    .any(|format| matches!(format, RemoteFormat::Content(..)));
                self.remote_content = ClipboardContent::default();
                self.remaining_formats = self.remote_formats.iter().copied().collect();
                self.wanted = self.remaining_formats.pop_front();
                actions.extend([
                    Action::Event(ChannelEvent::ClipboardOffer),
                    Action::Event(ChannelEvent::ClipboardUnavailable),
                    Action::Send(pdu(FORMAT_LIST_RESPONSE, OK, &[])),
                ]);
                if self.files_enabled
                    && self.peer_flags & FILE_STREAMS != 0
                    && let Some(format) = formats.files
                {
                    let lock = if self.peer_flags & CAN_LOCK != 0 {
                        self.next_lock_id = self
                            .next_lock_id
                            .checked_add(1)
                            .ok_or(Error::Limit("clipboard lock IDs"))?;
                        actions.push(Action::Send(pdu(LOCK, 0, &self.next_lock_id.to_le_bytes())));
                        Some(self.next_lock_id)
                    } else {
                        None
                    };
                    self.remote_files = Some(RemoteFiles {
                        id: self.generation,
                        files: None,
                        lock,
                    });
                    self.wanted = Some(RemoteFormat::Files(format));
                    self.remaining_formats.clear();
                }
                if rejected {
                    actions.push(Action::Event(ChannelEvent::ClipboardPublishRejected));
                }
                self.request_if_idle(&mut actions);
                actions
            }
            FORMAT_LIST_RESPONSE => {
                require_response(flags)?;
                require_empty(body)?;
                let offer = self
                    .offered
                    .take()
                    .ok_or(Error::Invalid("unsolicited clipboard format response"))?;
                let mut actions = Vec::new();
                let publishes_data = offer != LocalData::Empty;
                self.local = if flags == OK { offer } else { LocalData::Empty };
                if publishes_data && self.pending.is_none() {
                    actions.push(Action::Event(if flags == OK {
                        ChannelEvent::ClipboardPublished
                    } else {
                        ChannelEvent::ClipboardPublishRejected
                    }));
                }
                self.publish_pending(&mut actions);
                actions
            }
            DATA_REQUEST => {
                require_flags(flags, 0)?;
                let mut reader = Reader::new(body);
                let format = reader.u32_le()?;
                require_empty(reader.remaining())?;
                let response = match (format, self.available()) {
                    // Delayed rendering may precede FORMAT_LIST_RESPONSE.
                    (UNICODE_TEXT, LocalData::Text(text)) => pdu(DATA_RESPONSE, OK, text),
                    (id, LocalData::Content(formats)) => {
                        formats.iter().find(|format| format.id == id).map_or_else(
                            || pdu(DATA_RESPONSE, FAIL, &[]),
                            |format| pdu(DATA_RESPONSE, OK, &format.data),
                        )
                    }
                    (FILE_GROUP, LocalData::Files(id)) => match self.files.get(id) {
                        Some(files) => pdu(DATA_RESPONSE, OK, &file_descriptors(files)),
                        None => pdu(DATA_RESPONSE, FAIL, &[]),
                    },
                    _ => pdu(DATA_RESPONSE, FAIL, &[]),
                };
                vec![Action::Send(response)]
            }
            DATA_RESPONSE => {
                require_response(flags)?;
                let Some(request) = self.requested.take() else {
                    return Err(Error::Invalid("unsolicited clipboard data response"));
                };
                let mut actions = Vec::new();
                if request.generation == self.generation {
                    match request.format {
                        RemoteFormat::Text if self.remote_unicode => {
                            self.wanted = self.remaining_formats.pop_front();
                            if length > self.wire_limit() {
                                return Err(Error::Limit("clipboard text"));
                            }
                            if flags == OK {
                                let text = decode_text(body, self.limit)?;
                                if self.remote_rich {
                                    self.remote_content.text = Some(text);
                                } else {
                                    actions.push(Action::Event(ChannelEvent::ClipboardText(text)));
                                }
                            } else {
                                require_empty(body)?;
                                if !self.remote_rich {
                                    actions.push(Action::Event(ChannelEvent::ClipboardUnavailable));
                                }
                            }
                        }
                        RemoteFormat::Content(_, format) => {
                            self.wanted = self.remaining_formats.pop_front();
                            if flags == OK {
                                if body.len() > self.max_message.saturating_sub(8)
                                    || self.remote_content.len().saturating_add(body.len())
                                        > self.max_message.saturating_sub(8)
                                {
                                    return Err(Error::Limit("clipboard content"));
                                }
                                match format {
                                    ContentFormat::Html => {
                                        self.remote_content.html = Some(decode_html(body)?)
                                    }
                                    ContentFormat::Rtf => {
                                        self.remote_content.rtf =
                                            Some(body.strip_suffix(&[0]).unwrap_or(body).to_vec())
                                    }
                                    ContentFormat::Png => {
                                        self.remote_content.png = Some(body.to_vec())
                                    }
                                    ContentFormat::Dib => {
                                        self.remote_content.dib = Some(body.to_vec())
                                    }
                                }
                            } else {
                                require_empty(body)?;
                            }
                        }
                        RemoteFormat::Files(_) if self.remote_files.is_some() => {
                            self.wanted = None;
                            let files = if flags == OK {
                                parse_file_descriptors(body, self.max_message, self.peer_flags)
                            } else {
                                Err(Error::Invalid("remote clipboard file list unavailable"))
                            };
                            match files {
                                Ok(files) => {
                                    let remote = self.remote_files.as_mut().unwrap();
                                    remote.files = Some(files.clone());
                                    actions.push(Action::Event(ChannelEvent::ClipboardFiles {
                                        offer_id: remote.id,
                                        files,
                                    }));
                                }
                                Err(error) => {
                                    actions
                                        .push(Action::Event(ChannelEvent::RequestRejected(error)));
                                    self.release_remote(&mut actions);
                                }
                            }
                        }
                        _ => {}
                    }
                    if self.remote_rich && self.remote_files.is_none() && self.wanted.is_none() {
                        let content = std::mem::take(&mut self.remote_content);
                        actions.push(Action::Event(if content.is_empty() {
                            ChannelEvent::ClipboardUnavailable
                        } else {
                            ChannelEvent::ClipboardContent(content)
                        }));
                    }
                }
                self.request_if_idle(&mut actions);
                actions
            }
            FILE_REQUEST => {
                require_flags(flags, 0)?;
                self.file_request(body)?
            }
            FILE_RESPONSE => {
                require_response(flags)?;
                self.remote_file_response(body, flags)?
            }
            LOCK | UNLOCK => {
                require_flags(flags, 0)?;
                let mut reader = Reader::new(body);
                let id = reader.u32_le()?;
                require_empty(reader.remaining())?;
                if self.files_enabled && self.peer_flags & CAN_LOCK != 0 {
                    if kind == UNLOCK {
                        self.locks.remove(&id);
                    } else if !self.locks.contains_key(&id)
                        && self.locks.len() < MAX_LOCKS
                        && let Some(offer_id) = self.available().file_id()
                    {
                        self.locks.insert(id, offer_id);
                    }
                }
                Vec::new()
            }
            TEMP_DIRECTORY => {
                require_flags(flags, 0)?;
                // We advertise NO_FILE_PATHS and never access this peer path.
                if body.len() != 520 {
                    return Err(Error::Invalid("clipboard temporary directory length"));
                }
                Vec::new()
            }
            _ => return Err(Error::Unsupported("clipboard PDU")),
        };
        self.release_unused(&mut actions);
        Ok(actions)
    }

    pub(super) fn set_text(&mut self, text: String) -> Result<Vec<Action>> {
        if text.len() > self.limit {
            return Err(Error::Limit("clipboard text"));
        }
        if text.contains('\0') {
            return Err(Error::Unsupported("clipboard text containing NUL"));
        }
        let length = text
            .encode_utf16()
            .count()
            .checked_add(1)
            .and_then(|n| n.checked_mul(2))
            .filter(|length| *length <= self.wire_limit() && *length <= u32::MAX as usize)
            .ok_or(Error::Limit("clipboard UTF-16 text"))?;
        let mut encoded = Vec::with_capacity(length);
        for unit in text.encode_utf16().chain(std::iter::once(0)) {
            encoded.extend_from_slice(&unit.to_le_bytes());
        }
        self.set_local(LocalData::Text(encoded))
    }

    pub(super) fn set_files(
        &mut self,
        offer_id: u64,
        files: Vec<ClipboardFile>,
    ) -> Result<Vec<Action>> {
        if !self.ready || !self.files_enabled || self.peer_flags & FILE_STREAMS == 0 {
            return Err(Error::Unsupported(
                "clipboard file streaming was not negotiated",
            ));
        }
        if self
            .last_offer_id
            .is_some_and(|previous| offer_id <= previous)
        {
            return Err(Error::Invalid("clipboard file offer ID must increase"));
        }
        validate_files(&files, self.max_message)?;
        if self.peer_flags & HUGE_FILES == 0
            && files.iter().any(|file| file.size > u64::from(u32::MAX))
        {
            return Err(Error::Unsupported(
                "large clipboard files were not negotiated",
            ));
        }
        self.invalidate()?;
        self.last_offer_id = Some(offer_id);
        self.files.insert(offer_id, files);
        self.pending = Some(LocalData::Files(offer_id));
        self.remote_unicode = false;
        self.wanted = None;
        let mut actions = vec![Action::Event(ChannelEvent::ClipboardUnavailable)];
        self.release_remote(&mut actions);
        self.publish_pending(&mut actions);
        self.release_unused(&mut actions);
        Ok(actions)
    }

    pub(super) fn set_content(&mut self, content: ClipboardContent) -> Result<Vec<Action>> {
        if content.len() > self.max_message.saturating_sub(8) {
            return Err(Error::Limit("clipboard content"));
        }
        if content.html.is_none()
            && content.rtf.is_none()
            && content.png.is_none()
            && content.dib.is_none()
        {
            return match content.text {
                Some(text) => self.set_text(text),
                None => self.set_local(LocalData::Empty),
            };
        }
        let mut formats = Vec::new();
        if let Some(text) = content.text {
            if text.len() > self.limit || text.contains('\0') {
                return Err(Error::Limit("clipboard text"));
            }
            formats.push(Representation {
                id: UNICODE_TEXT,
                name: "",
                data: text
                    .encode_utf16()
                    .chain(std::iter::once(0))
                    .flat_map(u16::to_le_bytes)
                    .collect(),
            });
        }
        if let Some(html) = content.html {
            formats.push(Representation {
                id: HTML,
                name: "HTML Format",
                data: encode_html(&html)?,
            });
        }
        if let Some(mut rtf) = content.rtf {
            if !rtf.ends_with(&[0]) {
                rtf.push(0);
            }
            formats.push(Representation {
                id: RTF,
                name: "Rich Text Format",
                data: rtf,
            });
        }
        if let Some(png) = content.png {
            formats.push(Representation {
                id: PNG,
                name: "PNG",
                data: png,
            });
        }
        if let Some(dib) = content.dib {
            let header = dib
                .get(..4)
                .and_then(|bytes| bytes.try_into().ok())
                .map(u32::from_le_bytes);
            let id = match header {
                Some(124) => 17,
                Some(40 | 52 | 56 | 108) => 8,
                _ => return Err(Error::Invalid("clipboard DIB header")),
            };
            formats.push(Representation {
                id,
                name: "",
                data: dib,
            });
        }
        if formats
            .iter()
            .map(|format| format.data.len())
            .sum::<usize>()
            > self.max_message.saturating_sub(8)
        {
            return Err(Error::Limit("clipboard encoded content"));
        }
        self.set_local(LocalData::Content(formats))
    }

    pub(super) fn file_contents(
        &mut self,
        request_id: u64,
        data: Option<Vec<u8>>,
    ) -> Result<Vec<Action>> {
        let Some(pending) = self.file_requests.remove(&request_id) else {
            // Cancellation invalidates tokens before asynchronous reads finish.
            return Ok(Vec::new());
        };
        let data = data.filter(|data| data.len() == pending.request.length as usize);
        let mut actions = vec![file_response(pending.stream_id, data.as_deref())];
        self.release_unused(&mut actions);
        Ok(actions)
    }

    pub(super) fn cancel_files(&mut self) -> Result<Vec<Action>> {
        let advertised = self.available().file_id().is_some();
        if self.local.file_id().is_some() {
            self.local = LocalData::Empty;
        }
        if let Some(offer) = &mut self.offered
            && offer.file_id().is_some()
        {
            *offer = LocalData::Empty;
        }
        if self
            .pending
            .as_ref()
            .is_some_and(|data| data.file_id().is_some())
        {
            self.pending = None;
        }
        if advertised && self.pending.is_none() {
            self.pending = Some(LocalData::Empty);
        }
        self.locks.clear();
        let mut actions = std::mem::take(&mut self.file_requests)
            .into_values()
            .map(|pending| file_response(pending.stream_id, None))
            .collect::<Vec<_>>();
        self.publish_pending(&mut actions);
        self.release_unused(&mut actions);
        Ok(actions)
    }

    pub(super) fn request_file(
        &mut self,
        offer_id: u64,
        index: u32,
        offset: u64,
        length: u32,
    ) -> Result<Vec<Action>> {
        let remote = self
            .remote_files
            .as_ref()
            .filter(|remote| remote.id == offer_id)
            .ok_or(Error::Invalid("remote clipboard file offer unavailable"))?;
        let file = remote
            .files
            .as_ref()
            .and_then(|files| files.get(index as usize))
            .ok_or(Error::Invalid("remote clipboard file index"))?;
        let maximum = (MAX_CLIPBOARD_FILE_CHUNK as usize).min(self.max_message.saturating_sub(12));
        if file.directory
            || length == 0
            || offset > file.size
            || u64::from(length) > file.size - offset
        {
            return Err(Error::Invalid("remote clipboard file range"));
        }
        if length as usize > maximum || self.remote_reads.len() >= MAX_FILE_REQUESTS {
            return Err(Error::Limit("remote clipboard file requests"));
        }
        let stream_id = self
            .next_stream_id
            .checked_add(1)
            .ok_or(Error::Limit("remote clipboard file stream IDs"))?;
        let id = self
            .next_request_id
            .checked_add(1)
            .ok_or(Error::Limit("clipboard file request IDs"))?;
        self.next_stream_id = stream_id;
        self.next_request_id = id;
        let request = ClipboardFileRequest {
            id,
            offer_id,
            index,
            offset,
            length,
        };
        let mut body = Vec::with_capacity(28);
        for value in [
            stream_id,
            index,
            2, // FILECONTENTS_RANGE.
            offset as u32,
            (offset >> 32) as u32,
            length,
        ] {
            body.extend_from_slice(&value.to_le_bytes());
        }
        if let Some(lock) = remote.lock {
            body.extend_from_slice(&lock.to_le_bytes());
        }
        self.remote_reads.insert(stream_id, request);
        Ok(vec![Action::Send(pdu(FILE_REQUEST, 0, &body))])
    }

    pub(super) fn cancel_remote_files(&mut self, offer_id: u64) -> Result<Vec<Action>> {
        let mut actions = Vec::new();
        if self
            .remote_files
            .as_ref()
            .is_some_and(|remote| remote.id == offer_id)
        {
            self.release_remote(&mut actions);
        }
        Ok(actions)
    }

    pub(super) fn local_ready(&self) -> bool {
        self.ready
            && self.local != LocalData::Empty
            && self.offered.is_none()
            && self.pending.is_none()
    }

    pub(super) fn request(&mut self) -> Vec<Action> {
        if self.remote_formats.is_empty() || self.remote_files.is_some() {
            return vec![Action::Event(ChannelEvent::ClipboardUnavailable)];
        }
        if self
            .requested
            .as_ref()
            .is_some_and(|request| request.generation == self.generation)
        {
            return Vec::new();
        }
        self.remote_content = ClipboardContent::default();
        self.remaining_formats = self.remote_formats.iter().copied().collect();
        self.wanted = self.remaining_formats.pop_front();
        let mut actions = Vec::new();
        self.request_if_idle(&mut actions);
        actions
    }

    fn available(&self) -> &LocalData {
        self.offered.as_ref().unwrap_or(&self.local)
    }

    fn set_local(&mut self, data: LocalData) -> Result<Vec<Action>> {
        let latest = self
            .pending
            .as_ref()
            .or(self.offered.as_ref())
            .unwrap_or(&self.local);
        if latest == &data {
            return Ok(if self.local_ready() {
                vec![Action::Event(ChannelEvent::ClipboardPublished)]
            } else {
                Vec::new()
            });
        }
        self.invalidate()?;
        self.pending = Some(data);
        self.remote_unicode = false;
        self.wanted = None;
        let mut actions = vec![Action::Event(ChannelEvent::ClipboardUnavailable)];
        self.release_remote(&mut actions);
        self.publish_pending(&mut actions);
        self.release_unused(&mut actions);
        Ok(actions)
    }

    fn file_request(&mut self, data: &[u8]) -> Result<Vec<Action>> {
        if data.len() != 24 && data.len() != 28 {
            return Err(Error::Invalid("clipboard file request length"));
        }
        let mut reader = Reader::new(data);
        let stream_id = reader.u32_le()?;
        let index = reader.u32_le()?;
        let flags = reader.u32_le()?;
        let low = reader.u32_le()?;
        let high = reader.u32_le()?;
        let offset = u64::from(low) | (u64::from(high) << 32);
        let length = reader.u32_le()?;
        let offer_id = if reader.is_empty() {
            self.available().file_id()
        } else {
            let lock = reader.u32_le()?;
            self.locks.get(&lock).copied()
        };
        let fail = || vec![file_response(stream_id, None)];
        if !self.files_enabled || self.peer_flags & FILE_STREAMS == 0 {
            return Ok(fail());
        }
        if self
            .file_requests
            .values()
            .any(|pending| pending.stream_id == stream_id)
        {
            return Err(Error::Invalid("duplicate clipboard file stream ID"));
        }
        let Some(offer_id) = offer_id else {
            return Ok(fail());
        };
        let Some(file) = self
            .files
            .get(&offer_id)
            .and_then(|files| files.get(index as usize))
        else {
            return Ok(fail());
        };
        if file.directory {
            return Ok(fail());
        }
        match flags {
            1 if offset == 0 && length == 8 => Ok(vec![file_response(
                stream_id,
                Some(&file.size.to_le_bytes()),
            )]),
            2 if offset <= file.size => {
                let length = u64::from(length).min(file.size - offset) as u32;
                let maximum =
                    (MAX_CLIPBOARD_FILE_CHUNK as usize).min(self.max_message.saturating_sub(12));
                if length as usize > maximum || self.file_requests.len() == MAX_FILE_REQUESTS {
                    return Ok(fail());
                }
                if length == 0 {
                    return Ok(vec![file_response(stream_id, Some(&[]))]);
                }
                let id = self
                    .next_request_id
                    .checked_add(1)
                    .ok_or(Error::Limit("clipboard file request IDs"))?;
                self.next_request_id = id;
                let request = ClipboardFileRequest {
                    id,
                    offer_id,
                    index,
                    offset,
                    length,
                };
                self.file_requests
                    .insert(id, FileRequest { request, stream_id });
                Ok(vec![Action::Event(ChannelEvent::ClipboardFileRequest(
                    request,
                ))])
            }
            _ => Ok(fail()),
        }
    }

    fn release_unused(&mut self, actions: &mut Vec<Action>) {
        let mut retained = BTreeSet::new();
        retained.extend(self.local.file_id());
        retained.extend(self.offered.as_ref().and_then(LocalData::file_id));
        retained.extend(self.pending.as_ref().and_then(LocalData::file_id));
        retained.extend(self.locks.values().copied());
        retained.extend(
            self.file_requests
                .values()
                .map(|pending| pending.request.offer_id),
        );
        self.files.retain(|id, _| {
            if retained.contains(id) {
                true
            } else {
                actions.push(Action::Event(ChannelEvent::ClipboardFilesReleased {
                    offer_id: *id,
                }));
                false
            }
        });
    }

    fn remote_file_response(&mut self, body: &[u8], flags: u16) -> Result<Vec<Action>> {
        let mut reader = Reader::new(body);
        let stream_id = reader.u32_le()?;
        let Some(request) = self.remote_reads.remove(&stream_id) else {
            // Stream IDs are never reused. Responses for cancelled or already
            // completed reads cannot acquire a newer clipboard generation.
            return Ok(if stream_id != 0 && stream_id <= self.next_stream_id {
                Vec::new()
            } else {
                vec![Action::Event(ChannelEvent::RequestRejected(
                    Error::Invalid("unsolicited clipboard file response"),
                ))]
            });
        };
        let bytes = reader.remaining();
        let data = (flags == OK && bytes.len() == request.length as usize).then(|| bytes.to_vec());
        Ok(vec![Action::Event(ChannelEvent::ClipboardFileContents {
            request,
            data,
        })])
    }

    fn release_remote(&mut self, actions: &mut Vec<Action>) {
        let Some(remote) = self.remote_files.take() else {
            return;
        };
        self.remote_reads.clear();
        if matches!(self.wanted, Some(RemoteFormat::Files(_))) {
            self.wanted = None;
        }
        if let Some(lock) = remote.lock {
            actions.push(Action::Send(pdu(UNLOCK, 0, &lock.to_le_bytes())));
        }
        actions.push(Action::Event(ChannelEvent::ClipboardRemoteFilesCancelled {
            offer_id: remote.id,
        }));
    }

    fn invalidate(&mut self) -> Result<()> {
        self.generation = self
            .generation
            .checked_add(1)
            .ok_or(Error::Limit("clipboard generations"))?;
        self.remote_formats.clear();
        self.remaining_formats.clear();
        self.remote_content = ClipboardContent::default();
        self.remote_rich = false;
        Ok(())
    }

    fn wire_limit(&self) -> usize {
        self.limit.saturating_mul(2).saturating_add(2)
    }

    fn request_if_idle(&mut self, actions: &mut Vec<Action>) {
        // DATA_RESPONSE has no format ID. Text and file descriptors must share
        // this slot even after ownership changes, until its old reply arrives.
        if self.requested.is_none()
            && let Some(format) = self.wanted
        {
            self.requested = Some(DataRequest {
                generation: self.generation,
                format,
            });
            let format_id = match format {
                RemoteFormat::Text => UNICODE_TEXT,
                RemoteFormat::Files(id) => id,
                RemoteFormat::Content(id, _) => id,
            };
            actions.push(Action::Send(pdu(DATA_REQUEST, 0, &format_id.to_le_bytes())));
        }
    }

    fn publish_pending(&mut self, actions: &mut Vec<Action>) {
        if self.ready
            && self.offered.is_none()
            && let Some(data) = self.pending.take()
        {
            if self.local == data && data != LocalData::Empty {
                actions.push(Action::Event(ChannelEvent::ClipboardPublished));
            } else {
                actions.push(self.advertise(data));
            }
        }
    }

    fn advertise(&mut self, data: LocalData) -> Action {
        let mut formats = Vec::new();
        let mut flags = 0;
        match &data {
            LocalData::Text(_) => {
                formats.extend_from_slice(&UNICODE_TEXT.to_le_bytes());
                formats.resize(if self.long_names { 6 } else { 36 }, 0);
            }
            LocalData::Files(_) => {
                formats.extend_from_slice(&FILE_GROUP.to_le_bytes());
                if self.long_names {
                    for unit in FILE_GROUP_NAME.encode_utf16().chain(std::iter::once(0)) {
                        formats.extend_from_slice(&unit.to_le_bytes());
                    }
                } else {
                    flags = 4; // Short names use the protocol's ASCII flag.
                    formats.extend_from_slice(FILE_GROUP_NAME.as_bytes());
                    formats.resize(36, 0);
                }
            }
            LocalData::Content(content) => {
                for format in content {
                    append_format(&mut formats, format.id, format.name, self.long_names);
                }
                if !self.long_names {
                    flags = 4;
                }
            }
            LocalData::Empty => {}
        }
        self.offered = Some(data);
        Action::Send(pdu(FORMAT_LIST, flags, &formats))
    }

    fn capabilities(&mut self, data: &[u8]) -> Result<()> {
        if self.ready {
            return Err(Error::Invalid("clipboard capabilities after monitor ready"));
        }
        let mut reader = Reader::new(data);
        let count = reader.u16_le()?;
        reader.skip(2)?;
        if count > 64 {
            return Err(Error::Limit("clipboard capability count"));
        }
        let mut general_seen = false;
        self.long_names = false;
        self.peer_flags = 0;
        for _ in 0..count {
            let kind = reader.u16_le()?;
            let length = reader.u16_le()? as usize;
            if length < 4 {
                return Err(Error::Invalid("clipboard capability length"));
            }
            let mut capability = Reader::new(reader.take(length - 4)?);
            if kind == 1 {
                if general_seen || length != 12 {
                    return Err(Error::Invalid("clipboard general capability"));
                }
                general_seen = true;
                capability.u32_le()?;
                self.peer_flags = capability.u32_le()?;
                self.long_names = self.peer_flags & LONG_NAMES != 0;
            }
        }
        require_empty(reader.remaining())
    }
}

fn validate_files(files: &[ClipboardFile], max_message: usize) -> Result<()> {
    if files.is_empty()
        || files.len() > MAX_CLIPBOARD_FILES
        || 12 + files.len() * FILE_DESCRIPTOR_SIZE > max_message
    {
        return Err(Error::Limit("clipboard file list"));
    }
    let mut names = BTreeMap::new();
    for file in files {
        if file.name.encode_utf16().count() > 259 || (file.directory && file.size != 0) {
            return Err(Error::Invalid("clipboard file path or directory size"));
        }
        for name in file.name.split('\\') {
            if name.is_empty()
                || matches!(name, "." | "..")
                || name.ends_with([' ', '.'])
                || name.chars().any(|ch| ch < ' ' || "<>:\"/|?*".contains(ch))
            {
                return Err(Error::Invalid("clipboard file path"));
            }
            let stem = name
                .split('.')
                .next()
                .unwrap_or_default()
                .trim_end_matches(' ')
                .to_ascii_uppercase();
            if matches!(stem.as_str(), "CON" | "PRN" | "AUX" | "NUL")
                || ((stem.starts_with("COM") || stem.starts_with("LPT"))
                    && matches!(
                        stem.get(3..),
                        Some("1" | "2" | "3" | "4" | "5" | "6" | "7" | "8" | "9" | "¹" | "²" | "³")
                    ))
            {
                return Err(Error::Invalid("clipboard reserved filename"));
            }
        }
        if names.insert(file.name.to_lowercase(), file).is_some() {
            return Err(Error::Invalid("clipboard duplicate filename"));
        }
    }
    let mut ancestors = BTreeMap::new();
    for file in files {
        for (offset, _) in file.name.match_indices('\\') {
            let parent = &file.name[..offset];
            let key = parent.to_lowercase();
            if names
                .get(&key)
                .is_some_and(|entry| !entry.directory || entry.name != parent)
                || ancestors
                    .insert(key, parent)
                    .is_some_and(|previous| previous != parent)
            {
                return Err(Error::Invalid("clipboard conflicting parent path"));
            }
        }
    }
    Ok(())
}

fn file_descriptors(files: &[ClipboardFile]) -> Vec<u8> {
    let mut data = vec![0; 4 + FILE_DESCRIPTOR_SIZE * files.len()];
    data[..4].copy_from_slice(&(files.len() as u32).to_le_bytes());
    for (index, file) in files.iter().enumerate() {
        let descriptor = &mut data[4 + index * FILE_DESCRIPTOR_SIZE..][..FILE_DESCRIPTOR_SIZE];
        // FD_UNICODE | FD_PROGRESSUI | FD_FILESIZE | FD_ATTRIBUTES.
        descriptor[..4].copy_from_slice(&0x8000_4044u32.to_le_bytes());
        descriptor[36..40]
            .copy_from_slice(&(if file.directory { 0x10u32 } else { 0x80 }).to_le_bytes());
        descriptor[64..68].copy_from_slice(&((file.size >> 32) as u32).to_le_bytes());
        descriptor[68..72].copy_from_slice(&(file.size as u32).to_le_bytes());
        for (index, unit) in file.name.encode_utf16().enumerate() {
            descriptor[72 + index * 2..74 + index * 2].copy_from_slice(&unit.to_le_bytes());
        }
    }
    data
}

fn parse_file_descriptors(
    data: &[u8],
    max_message: usize,
    peer_flags: u32,
) -> Result<Vec<ClipboardFile>> {
    let mut reader = Reader::new(data);
    let count = reader.u32_le()? as usize;
    if count == 0
        || count > MAX_CLIPBOARD_FILES
        || data.len().saturating_add(8) > max_message
        || reader.remaining_len() != count * FILE_DESCRIPTOR_SIZE
    {
        return Err(Error::Limit("remote clipboard file list"));
    }
    let mut files = Vec::with_capacity(count);
    let mut total_size = 0u64;
    for _ in 0..count {
        let mut descriptor = Reader::new(reader.take(FILE_DESCRIPTOR_SIZE)?);
        let flags = descriptor.u32_le()?;
        descriptor.skip(32)?; // CLSID, SIZEL and POINTL.
        let attributes = descriptor.u32_le()?;
        descriptor.skip(24)?; // Creation, access and write FILETIMEs.
        let high = descriptor.u32_le()?;
        let low = descriptor.u32_le()?;
        let size = (u64::from(high) << 32) | u64::from(low);
        let directory = attributes & 0x10 != 0;
        // Retain list indices, including directories; only regular files have streams.
        if flags & 4 == 0 || (!directory && flags & 0x40 == 0)
            || flags & 0x8000 != 0 // FD_LINKUI.
            || attributes & (0x40 | 0x400) != 0
        {
            return Err(Error::Unsupported(
                "clipboard device, link or missing file size",
            ));
        }
        let size = if directory { 0 } else { size };
        if peer_flags & HUGE_FILES == 0 && size > u64::from(u32::MAX) {
            return Err(Error::Unsupported(
                "large clipboard files were not negotiated",
            ));
        }
        total_size = total_size
            .checked_add(size)
            .ok_or(Error::Limit("remote clipboard total file size"))?;
        let mut units = Vec::with_capacity(260);
        while !descriptor.is_empty() {
            let unit = descriptor.u16_le()?;
            if unit == 0 {
                break;
            }
            units.push(unit);
        }
        if units.len() == 260 {
            return Err(Error::Invalid("clipboard filename terminator"));
        }
        let name =
            String::from_utf16(&units).map_err(|_| Error::Invalid("clipboard filename UTF-16"))?;
        files.push(ClipboardFile {
            name,
            size,
            directory,
        });
    }
    validate_files(&files, max_message)?;
    Ok(files)
}

fn file_response(stream_id: u32, data: Option<&[u8]>) -> Action {
    let mut body = stream_id.to_le_bytes().to_vec();
    if let Some(data) = data {
        body.extend_from_slice(data);
    }
    Action::Send(pdu(
        FILE_RESPONSE,
        if data.is_some() { OK } else { FAIL },
        &body,
    ))
}

fn append_format(output: &mut Vec<u8>, id: u32, name: &str, long: bool) {
    output.extend_from_slice(&id.to_le_bytes());
    if long {
        output.extend(
            name.encode_utf16()
                .chain(std::iter::once(0))
                .flat_map(u16::to_le_bytes),
        );
    } else {
        let start = output.len();
        output.extend_from_slice(name.as_bytes());
        output.resize(start + 32, 0);
    }
}

fn parse_formats(data: &[u8], long_names: bool, ascii_names: bool) -> Result<Formats> {
    let mut reader = Reader::new(data);
    let mut formats = Formats::default();
    let mut count = 0;
    while !reader.is_empty() {
        count += 1;
        if count > MAX_FORMATS {
            return Err(Error::Limit("clipboard formats"));
        }
        let id = reader.u32_le()?;
        formats.unicode |= id == UNICODE_TEXT;
        let name = if long_names {
            let mut units = Vec::new();
            loop {
                let unit = reader.u16_le()?;
                if unit == 0 {
                    break;
                }
                units.push(unit);
                if units.len() > 255 {
                    return Err(Error::Limit("clipboard format name"));
                }
            }
            String::from_utf16(&units).map_err(|_| Error::Invalid("clipboard format name"))?
        } else {
            let bytes = reader.take(32)?;
            if ascii_names {
                String::from_utf8_lossy(bytes.split(|byte| *byte == 0).next().unwrap_or_default())
                    .into_owned()
            } else {
                let units: Vec<_> = bytes
                    .as_chunks::<2>()
                    .0
                    .iter()
                    .map(|bytes| u16::from_le_bytes(*bytes))
                    .take_while(|unit| *unit != 0)
                    .collect();
                String::from_utf16(&units).map_err(|_| Error::Invalid("clipboard format name"))?
            }
        };
        let slot = match name.as_str() {
            FILE_GROUP_NAME => Some(&mut formats.files),
            "HTML Format" => Some(&mut formats.html),
            "Rich Text Format" => Some(&mut formats.rtf),
            "PNG" => Some(&mut formats.png),
            _ => None,
        };
        if let Some(slot) = slot {
            if slot.is_some_and(|previous| previous != id) {
                return Err(Error::Invalid("ambiguous clipboard format"));
            }
            *slot = Some(id);
        }
        if id == 17 || (id == 8 && formats.dib.is_none()) {
            formats.dib = Some(id);
        }
    }
    Ok(formats)
}

fn encode_html(fragment: &str) -> Result<Vec<u8>> {
    if fragment.contains('\0') {
        return Err(Error::Invalid("clipboard HTML terminator"));
    }
    const PREFIX: &str = "<html><body><!--StartFragment-->";
    const SUFFIX: &str = "<!--EndFragment--></body></html>";
    let header = |start, end, fragment_start, fragment_end| {
        format!(
            "Version:1.0\r\nStartHTML:{start:010}\r\nEndHTML:{end:010}\r\nStartFragment:{fragment_start:010}\r\nEndFragment:{fragment_end:010}\r\n"
        )
    };
    let start = header(0, 0, 0, 0).len();
    let fragment_start = start + PREFIX.len();
    let fragment_end = fragment_start + fragment.len();
    let end = fragment_end + SUFFIX.len();
    let mut output = header(start, end, fragment_start, fragment_end).into_bytes();
    output.extend_from_slice(PREFIX.as_bytes());
    output.extend_from_slice(fragment.as_bytes());
    output.extend_from_slice(SUFFIX.as_bytes());
    output.push(0);
    Ok(output)
}

fn decode_html(data: &[u8]) -> Result<String> {
    let mut cursor = 0usize;
    let mut start = None;
    let mut end = None;
    while cursor < data.len().min(4096) {
        let Some(length) = data[cursor..]
            .iter()
            .position(|byte| matches!(byte, b'\r' | b'\n'))
        else {
            break;
        };
        let line = &data[cursor..cursor + length];
        let Some(colon) = line.iter().position(|byte| *byte == b':') else {
            break;
        };
        let key = &line[..colon];
        if !matches!(
            key,
            b"Version"
                | b"StartHTML"
                | b"EndHTML"
                | b"StartFragment"
                | b"EndFragment"
                | b"StartSelection"
                | b"EndSelection"
                | b"SourceURL"
        ) {
            break;
        }
        let target = match key {
            b"StartFragment" => Some(&mut start),
            b"EndFragment" => Some(&mut end),
            _ => None,
        };
        if let Some(target) = target {
            if target.is_some() {
                return Err(Error::Invalid("duplicate clipboard HTML offset"));
            }
            *target = Some(
                std::str::from_utf8(&line[colon + 1..])
                    .ok()
                    .and_then(|value| value.trim().parse::<usize>().ok())
                    .ok_or(Error::Invalid("clipboard HTML offset"))?,
            );
        }
        cursor += length;
        if data.get(cursor) == Some(&b'\r') {
            cursor += 1;
        }
        if data.get(cursor) == Some(&b'\n') {
            cursor += 1;
        }
        if let Some(start) = start
            && cursor >= start
        {
            break;
        }
    }
    let start = start.ok_or(Error::Invalid("clipboard HTML fragment start"))?;
    let end = end.ok_or(Error::Invalid("clipboard HTML fragment end"))?;
    if start < cursor || end < start {
        return Err(Error::Invalid("clipboard HTML fragment"));
    }
    let fragment = data
        .get(start..end)
        .ok_or(Error::Invalid("clipboard HTML fragment bounds"))?;
    let fragment =
        std::str::from_utf8(fragment).map_err(|_| Error::Invalid("clipboard HTML UTF-8"))?;
    if fragment.contains('\0') {
        return Err(Error::Invalid("clipboard HTML terminator"));
    }
    Ok(fragment.to_owned())
}

fn decode_text(data: &[u8], limit: usize) -> Result<String> {
    if data.len() < 2 || !data.len().is_multiple_of(2) || data[data.len() - 2..] != [0, 0] {
        return Err(Error::Invalid("clipboard Unicode terminator"));
    }
    let units = data[..data.len() - 2]
        .as_chunks::<2>()
        .0
        .iter()
        .map(|bytes| u16::from_le_bytes([bytes[0], bytes[1]]));
    let mut text = String::with_capacity(data.len().min(limit));
    for unit in char::decode_utf16(units) {
        let character = unit.map_err(|_| Error::Invalid("clipboard UTF-16"))?;
        if character == '\0' {
            return Err(Error::Invalid("embedded clipboard Unicode terminator"));
        }
        if text.len().saturating_add(character.len_utf8()) > limit {
            return Err(Error::Limit("clipboard UTF-8 text"));
        }
        text.push(character);
    }
    Ok(text)
}

fn pdu(kind: u16, flags: u16, data: &[u8]) -> Vec<u8> {
    let mut output = Vec::with_capacity(8 + data.len());
    output.extend_from_slice(&kind.to_le_bytes());
    output.extend_from_slice(&flags.to_le_bytes());
    output.extend_from_slice(&(data.len() as u32).to_le_bytes());
    output.extend_from_slice(data);
    output
}

fn require_flags(flags: u16, expected: u16) -> Result<()> {
    if flags == expected {
        Ok(())
    } else {
        Err(Error::Invalid("clipboard flags"))
    }
}

fn require_response(flags: u16) -> Result<()> {
    if flags == OK || flags == FAIL {
        Ok(())
    } else {
        Err(Error::Invalid("clipboard response flags"))
    }
}

fn require_empty(data: &[u8]) -> Result<()> {
    if data.is_empty() {
        Ok(())
    } else {
        Err(Error::Invalid("clipboard trailing data"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rich_selection_is_received_as_one_coherent_offer() {
        let mut sender = file_clipboard(LONG_NAMES | FILE_STREAMS);
        let mut receiver = file_clipboard(LONG_NAMES | FILE_STREAMS);
        let content = ClipboardContent {
            text: Some("Hello 🌍".into()),
            html: Some("<b>Hello 🌍</b>".into()),
            rtf: Some(br"{\rtf1 Hello}".to_vec()),
            png: Some(b"opaque PNG data supplied by the caller".to_vec()),
            dib: None,
        };
        let offer = sender
            .set_content(content.clone())
            .unwrap()
            .into_iter()
            .find_map(|action| match action {
                Action::Send(bytes) if bytes[..2] == FORMAT_LIST.to_le_bytes() => Some(bytes),
                _ => None,
            })
            .unwrap();
        let mut actions = receiver.receive(&offer).unwrap();
        let mut received = Vec::new();
        loop {
            let mut next = None;
            for action in actions {
                match action {
                    Action::Send(request) if request[..2] == DATA_REQUEST.to_le_bytes() => {
                        next = Some(request)
                    }
                    Action::Event(ChannelEvent::ClipboardContent(content)) => {
                        received.push(content)
                    }
                    Action::Event(ChannelEvent::ClipboardText(_)) => {
                        panic!("partial text must not replace rich formats")
                    }
                    _ => {}
                }
            }
            let Some(request) = next else {
                break;
            };
            let response = sender
                .receive(&request)
                .unwrap()
                .into_iter()
                .find_map(|action| {
                    if let Action::Send(bytes) = action {
                        Some(bytes)
                    } else {
                        None
                    }
                })
                .unwrap();
            actions = receiver.receive(&response).unwrap();
        }
        assert_eq!(received, vec![content]);
    }

    #[test]
    fn html_offsets_are_utf8_bytes_and_plain_fragments_need_no_context() {
        let fragment = "<strong>caf\u{e9} 🌍</strong>";
        let encoded = encode_html(fragment).unwrap();
        assert_eq!(decode_html(&encoded).unwrap(), fragment);
        let make_header = |start, end| {
            format!(
                "Version:1.0\r\nStartHTML:-1\r\nEndHTML:-1\r\nStartFragment:{start:010}\r\nEndFragment:{end:010}\r\n"
            )
        };
        let start = make_header(0, 0).len();
        let mut plain = make_header(start, start + 5).into_bytes();
        plain.extend_from_slice(b"hello");
        assert_eq!(decode_html(&plain).unwrap(), "hello");
        assert!(decode_html(b"Version:1.0\nStartFragment:9999\nEndFragment:99999\n").is_err());
    }

    #[test]
    fn directories_keep_list_indices_and_reject_content_reads_or_path_conflicts() {
        let entries = vec![
            ClipboardFile {
                name: "folder".into(),
                size: 0,
                directory: true,
            },
            ClipboardFile {
                name: "folder\\data.txt".into(),
                size: 3,
                directory: false,
            },
            ClipboardFile {
                name: "folder\\empty".into(),
                size: 0,
                directory: true,
            },
        ];
        let data = file_descriptors(&entries);
        assert_eq!(
            parse_file_descriptors(&data, 4096, HUGE_FILES).unwrap(),
            entries
        );
        let mut clipboard = file_clipboard(FILE_STREAMS | HUGE_FILES);
        clipboard.set_files(1, entries).unwrap();
        let response = clipboard
            .receive(&file_request(1, 0, 1, 0, 8, None))
            .unwrap();
        assert!(matches!(&response[0], Action::Send(bytes) if bytes[2..4] == FAIL.to_le_bytes()));
        let request = clipboard
            .receive(&file_request(2, 1, 2, 0, 3, None))
            .unwrap();
        assert!(
            matches!(&request[0], Action::Event(ChannelEvent::ClipboardFileRequest(request)) if request.index == 1)
        );
        for parent in [
            ClipboardFile {
                name: "folder".into(),
                size: 0,
                directory: false,
            },
            ClipboardFile {
                name: "FOLDER".into(),
                size: 0,
                directory: true,
            },
        ] {
            assert!(
                validate_files(
                    &[
                        parent,
                        ClipboardFile {
                            name: "folder\\child".into(),
                            size: 0,
                            directory: false
                        }
                    ],
                    4096
                )
                .is_err()
            );
        }
    }

    fn format_list() -> Vec<u8> {
        let mut format = vec![0; 36];
        format[..4].copy_from_slice(&UNICODE_TEXT.to_le_bytes());
        pdu(FORMAT_LIST, 0, &format)
    }

    fn sent(actions: &[Action], kind: u16) -> usize {
        actions
            .iter()
            .filter(
                |action| matches!(action, Action::Send(bytes) if bytes[..2] == kind.to_le_bytes()),
            )
            .count()
    }

    fn acknowledged(actions: &[Action]) -> bool {
        actions
            .iter()
            .any(|action| matches!(action, Action::Event(ChannelEvent::ClipboardPublished)))
    }

    fn rejected(actions: &[Action]) -> bool {
        actions.iter().any(|action| {
            matches!(
                action,
                Action::Event(ChannelEvent::ClipboardPublishRejected)
            )
        })
    }

    fn ready_clipboard() -> Clipboard {
        let mut clipboard = Clipboard::new(1024, false, 4 * 1024 * 1024);
        clipboard.receive(&pdu(MONITOR_READY, 0, &[])).unwrap();
        clipboard
            .receive(&pdu(FORMAT_LIST_RESPONSE, OK, &[]))
            .unwrap();
        clipboard
    }

    fn response(clipboard: &mut Clipboard) -> Vec<u8> {
        let actions = clipboard
            .receive(&pdu(DATA_REQUEST, 0, &UNICODE_TEXT.to_le_bytes()))
            .unwrap();
        let [Action::Send(bytes)] = actions.as_slice() else {
            panic!("expected one clipboard data response");
        };
        bytes.clone()
    }

    fn encoded(text: &str) -> Vec<u8> {
        text.encode_utf16()
            .chain(std::iter::once(0))
            .flat_map(u16::to_le_bytes)
            .collect()
    }

    #[test]
    fn remote_format_list_emits_offer_before_request_but_local_text_does_not() {
        let mut clipboard = ready_clipboard();
        let remote = clipboard.receive(&format_list()).unwrap();
        assert!(matches!(
            remote.as_slice(),
            [
                Action::Event(ChannelEvent::ClipboardOffer),
                Action::Event(ChannelEvent::ClipboardUnavailable),
                Action::Send(ack),
                Action::Send(request),
            ] if *ack == pdu(FORMAT_LIST_RESPONSE, OK, &[])
                && *request == pdu(DATA_REQUEST, 0, &UNICODE_TEXT.to_le_bytes())
        ));
        let local = clipboard.set_text("local text".into()).unwrap();
        assert!(matches!(
            local.as_slice(),
            [
                Action::Event(ChannelEvent::ClipboardUnavailable),
                Action::Send(_)
            ]
        ));
    }

    #[test]
    fn data_request_before_first_format_ack_receives_advertised_text() {
        let mut clipboard = Clipboard::new(1024, false, 4 * 1024 * 1024);
        clipboard
            .set_text("first \u{20ac}\u{1f642}".into())
            .unwrap();
        assert_eq!(response(&mut clipboard), pdu(DATA_RESPONSE, FAIL, &[]));
        clipboard.receive(&pdu(MONITOR_READY, 0, &[])).unwrap();

        assert_eq!(
            response(&mut clipboard),
            pdu(DATA_RESPONSE, OK, &encoded("first \u{20ac}\u{1f642}"))
        );
        assert!(!clipboard.local_ready());
        assert!(acknowledged(
            &clipboard
                .receive(&pdu(FORMAT_LIST_RESPONSE, OK, &[]))
                .unwrap()
        ));
        assert!(clipboard.local_ready());
    }

    #[test]
    fn data_request_before_update_ack_receives_offered_not_queued_text() {
        let mut clipboard = ready_clipboard();
        clipboard.set_text("old".into()).unwrap();
        clipboard
            .receive(&pdu(FORMAT_LIST_RESPONSE, OK, &[]))
            .unwrap();
        clipboard.set_text("offered".into()).unwrap();
        assert_eq!(
            sent(&clipboard.set_text("queued".into()).unwrap(), FORMAT_LIST),
            0
        );
        assert_eq!(
            response(&mut clipboard),
            pdu(DATA_RESPONSE, OK, &encoded("offered"))
        );
        assert!(!clipboard.local_ready());

        let actions = clipboard
            .receive(&pdu(FORMAT_LIST_RESPONSE, OK, &[]))
            .unwrap();
        assert_eq!(sent(&actions, FORMAT_LIST), 1);
        assert!(!acknowledged(&actions));
        assert_eq!(
            response(&mut clipboard),
            pdu(DATA_RESPONSE, OK, &encoded("queued"))
        );
        assert!(!clipboard.local_ready());
    }

    #[test]
    fn data_request_after_rejection_or_remote_takeover_has_no_local_text() {
        let mut clipboard = ready_clipboard();
        clipboard.set_text("old".into()).unwrap();
        clipboard
            .receive(&pdu(FORMAT_LIST_RESPONSE, OK, &[]))
            .unwrap();
        clipboard.set_text("rejected".into()).unwrap();
        clipboard
            .receive(&pdu(FORMAT_LIST_RESPONSE, FAIL, &[]))
            .unwrap();
        assert_eq!(response(&mut clipboard), pdu(DATA_RESPONSE, FAIL, &[]));

        clipboard.set_text("revoked".into()).unwrap();
        clipboard.set_text("never advertised".into()).unwrap();
        clipboard.receive(&format_list()).unwrap();
        assert_eq!(response(&mut clipboard), pdu(DATA_RESPONSE, FAIL, &[]));
        clipboard
            .receive(&pdu(FORMAT_LIST_RESPONSE, OK, &[]))
            .unwrap();
        assert_eq!(response(&mut clipboard), pdu(DATA_RESPONSE, FAIL, &[]));
        assert!(!clipboard.local_ready());
    }

    #[test]
    fn initialization_ack_never_publishes_a_later_local_offer() {
        let mut clipboard = Clipboard::new(1024, false, 4 * 1024 * 1024);
        clipboard.receive(&pdu(MONITOR_READY, 0, &[])).unwrap();
        let actions = clipboard.set_text("text \u{20ac}\u{1f642}".into()).unwrap();
        assert_eq!(sent(&actions, FORMAT_LIST), 0);
        assert!(!clipboard.local_ready());

        let actions = clipboard
            .receive(&pdu(FORMAT_LIST_RESPONSE, OK, &[]))
            .unwrap();
        assert_eq!(sent(&actions, FORMAT_LIST), 1);
        assert!(!acknowledged(&actions));
        assert!(!clipboard.local_ready());
        assert_eq!(
            response(&mut clipboard),
            pdu(DATA_RESPONSE, OK, &encoded("text \u{20ac}\u{1f642}"))
        );

        let actions = clipboard
            .receive(&pdu(FORMAT_LIST_RESPONSE, OK, &[]))
            .unwrap();
        assert!(acknowledged(&actions));
        assert!(clipboard.local_ready());
        assert_eq!(
            response(&mut clipboard),
            pdu(DATA_RESPONSE, OK, &encoded("text \u{20ac}\u{1f642}"))
        );
        let actions = clipboard.set_text("text \u{20ac}\u{1f642}".into()).unwrap();
        assert!(acknowledged(&actions));
        assert_eq!(sent(&actions, FORMAT_LIST), 0);
    }

    #[test]
    fn local_offers_coalesce_and_data_requests_use_the_advertised_snapshot() {
        let mut clipboard = ready_clipboard();
        assert_eq!(
            sent(&clipboard.set_text("first".into()).unwrap(), FORMAT_LIST),
            1
        );
        assert_eq!(
            sent(&clipboard.set_text("skipped".into()).unwrap(), FORMAT_LIST),
            0
        );
        assert_eq!(
            sent(&clipboard.set_text("latest".into()).unwrap(), FORMAT_LIST),
            0
        );
        assert!(clipboard.set_text("latest".into()).unwrap().is_empty());

        let actions = clipboard
            .receive(&pdu(FORMAT_LIST_RESPONSE, OK, &[]))
            .unwrap();
        assert_eq!(sent(&actions, FORMAT_LIST), 1);
        assert!(!acknowledged(&actions));
        assert!(!clipboard.local_ready());
        assert_eq!(
            response(&mut clipboard),
            pdu(DATA_RESPONSE, OK, &encoded("latest"))
        );

        let actions = clipboard
            .receive(&pdu(FORMAT_LIST_RESPONSE, OK, &[]))
            .unwrap();
        assert!(acknowledged(&actions));
        assert!(clipboard.local_ready());
        assert_eq!(
            response(&mut clipboard),
            pdu(DATA_RESPONSE, OK, &encoded("latest"))
        );
    }

    #[test]
    fn remote_takeover_revokes_queued_text_without_reusing_the_old_ack_slot() {
        let mut clipboard = ready_clipboard();
        clipboard.set_text("old".into()).unwrap();
        clipboard.set_text("discarded".into()).unwrap();
        let actions = clipboard.receive(&format_list()).unwrap();
        assert!(rejected(&actions));
        assert!(!clipboard.local_ready());
        assert_eq!(response(&mut clipboard), pdu(DATA_RESPONSE, FAIL, &[]));

        assert_eq!(
            sent(&clipboard.set_text("new".into()).unwrap(), FORMAT_LIST),
            0
        );
        let actions = clipboard
            .receive(&pdu(FORMAT_LIST_RESPONSE, OK, &[]))
            .unwrap();
        assert!(!acknowledged(&actions));
        assert_eq!(sent(&actions, FORMAT_LIST), 1);
        assert_eq!(
            response(&mut clipboard),
            pdu(DATA_RESPONSE, OK, &encoded("new"))
        );

        let actions = clipboard
            .receive(&pdu(FORMAT_LIST_RESPONSE, OK, &[]))
            .unwrap();
        assert!(acknowledged(&actions));
        assert_eq!(
            response(&mut clipboard),
            pdu(DATA_RESPONSE, OK, &encoded("new"))
        );
    }

    #[test]
    fn failed_publication_is_reported_without_rejecting_a_newer_queued_offer() {
        let mut clipboard = ready_clipboard();
        clipboard.set_text("rejected".into()).unwrap();
        let actions = clipboard
            .receive(&pdu(FORMAT_LIST_RESPONSE, FAIL, &[]))
            .unwrap();
        assert!(rejected(&actions));
        assert!(!clipboard.local_ready());
        assert_eq!(response(&mut clipboard), pdu(DATA_RESPONSE, FAIL, &[]));

        clipboard.set_text("older".into()).unwrap();
        clipboard.set_text("latest".into()).unwrap();
        let actions = clipboard
            .receive(&pdu(FORMAT_LIST_RESPONSE, FAIL, &[]))
            .unwrap();
        assert!(!rejected(&actions));
        assert!(!acknowledged(&actions));
        assert_eq!(sent(&actions, FORMAT_LIST), 1);
        assert!(acknowledged(
            &clipboard
                .receive(&pdu(FORMAT_LIST_RESPONSE, OK, &[]))
                .unwrap()
        ));
        assert!(clipboard.local_ready());
    }

    #[test]
    fn late_response_cannot_replace_the_new_clipboard() {
        let mut clipboard = Clipboard::new(1024, false, 4 * 1024 * 1024);
        assert_eq!(
            sent(&clipboard.receive(&format_list()).unwrap(), DATA_REQUEST),
            1
        );
        assert_eq!(
            sent(&clipboard.receive(&format_list()).unwrap(), DATA_REQUEST),
            0
        );
        let late = clipboard
            .receive(&pdu(DATA_RESPONSE, OK, &[b'o', 0, 0, 0]))
            .unwrap();
        assert_eq!(sent(&late, DATA_REQUEST), 1);
        assert!(
            !late
                .iter()
                .any(|action| matches!(action, Action::Event(ChannelEvent::ClipboardText(_))))
        );
        let fresh = clipboard
            .receive(&pdu(DATA_RESPONSE, OK, &[0xac, 0x20, 0xac, 0x20, 0, 0]))
            .unwrap();
        assert!(
            matches!(&fresh[0], Action::Event(ChannelEvent::ClipboardText(text)) if text == "\u{20ac}\u{20ac}")
        );
    }

    #[test]
    fn remote_ownership_revokes_previously_advertised_local_text() {
        let mut clipboard = Clipboard::new(1024, false, 4 * 1024 * 1024);
        clipboard.set_text("private".into()).unwrap();
        clipboard.receive(&pdu(MONITOR_READY, 0, &[])).unwrap();
        clipboard
            .receive(&pdu(FORMAT_LIST_RESPONSE, OK, &[]))
            .unwrap();
        clipboard.receive(&format_list()).unwrap();
        let response = clipboard
            .receive(&pdu(DATA_REQUEST, 0, &UNICODE_TEXT.to_le_bytes()))
            .unwrap();
        assert!(
            matches!(&response[0], Action::Send(bytes) if *bytes == pdu(DATA_RESPONSE, FAIL, &[]))
        );
    }

    #[test]
    fn unicode_limits_include_both_encodings() {
        let mut clipboard = Clipboard::new(8, false, 4 * 1024 * 1024);
        assert!(clipboard.set_text("123456789".into()).is_err());
        assert!(clipboard.set_text("12345678".into()).is_ok());
        let accepted = clipboard.pending.clone();
        assert!(matches!(
            clipboard.set_text("a\0b".into()),
            Err(Error::Unsupported(_))
        ));
        assert_eq!(clipboard.pending, accepted);
        assert!(decode_text(&[0, 0xd8, 0, 0], 8).is_err());
        assert!(decode_text(&[0xac, 0x20, 0xac, 0x20, 0, 0], 4).is_err());
        assert_eq!(decode_text(&[0, 0], 8).unwrap(), "");
    }

    #[test]
    fn accepts_only_the_documented_windows_trailer() {
        let mut clipboard = Clipboard::new(1024, false, 4 * 1024 * 1024);
        let mut data = pdu(MONITOR_READY, 0, &[]);
        data.extend_from_slice(&[0; 4]);
        assert!(clipboard.receive(&data).is_ok());
        data.push(0);
        assert!(clipboard.receive(&data).is_err());
    }
    fn file_clipboard(features: u32) -> Clipboard {
        let mut clipboard = Clipboard::new(1024, true, 4 * 1024 * 1024);
        let mut capabilities = vec![1, 0, 0, 0, 1, 0, 12, 0, 2, 0, 0, 0];
        capabilities.extend_from_slice(&features.to_le_bytes());
        clipboard
            .receive(&pdu(CAPABILITIES, 0, &capabilities))
            .unwrap();
        clipboard.receive(&pdu(MONITOR_READY, 0, &[])).unwrap();
        clipboard
            .receive(&pdu(FORMAT_LIST_RESPONSE, OK, &[]))
            .unwrap();
        clipboard
    }

    fn files(size: u64) -> Vec<ClipboardFile> {
        vec![ClipboardFile {
            directory: false,
            name: "report \u{20ac}.txt".into(),
            size,
        }]
    }

    fn file_request(
        stream: u32,
        index: u32,
        operation: u32,
        offset: u64,
        length: u32,
        lock: Option<u32>,
    ) -> Vec<u8> {
        let mut body = Vec::new();
        for field in [
            stream,
            index,
            operation,
            offset as u32,
            (offset >> 32) as u32,
            length,
        ] {
            body.extend_from_slice(&field.to_le_bytes());
        }
        if let Some(lock) = lock {
            body.extend_from_slice(&lock.to_le_bytes());
        }
        pdu(FILE_REQUEST, 0, &body)
    }

    fn requested_file(actions: &[Action]) -> ClipboardFileRequest {
        actions
            .iter()
            .find_map(|action| match action {
                Action::Event(ChannelEvent::ClipboardFileRequest(request)) => Some(*request),
                _ => None,
            })
            .expect("file read request")
    }

    fn released(actions: &[Action], id: u64) -> bool {
        actions.iter().any(|action| {
            matches!(action,
            Action::Event(ChannelEvent::ClipboardFilesReleased { offer_id }) if *offer_id == id)
        })
    }

    fn file_reply(actions: &[Action], stream: u32, data: Option<&[u8]>) -> bool {
        let Action::Send(expected) = file_response(stream, data) else {
            unreachable!()
        };
        actions
            .iter()
            .any(|action| matches!(action, Action::Send(bytes) if *bytes == expected))
    }

    #[test]
    fn file_capabilities_are_opt_in_and_require_peer_support() {
        let mut disabled = Clipboard::new(1024, false, 4096);
        let actions = disabled.receive(&pdu(MONITOR_READY, 0, &[])).unwrap();
        let Action::Send(caps) = &actions[0] else {
            panic!("capabilities")
        };
        assert_eq!(&caps[20..24], &LONG_NAMES.to_le_bytes());
        assert!(disabled.set_files(1, files(3)).is_err());
        let mut legacy = file_clipboard(0);
        assert!(legacy.set_files(1, files(3)).is_err());
        let mut ordinary = file_clipboard(FILE_STREAMS | LONG_NAMES);
        assert!(
            ordinary
                .set_files(1, files(u64::from(u32::MAX) + 1))
                .is_err()
        );
        assert!(ordinary.set_files(1, files(3)).is_ok());
        let mut huge = file_clipboard(FILE_STREAMS | LONG_NAMES | HUGE_FILES);
        assert!(huge.set_files(1, files(u64::from(u32::MAX) + 1)).is_ok());
    }

    #[test]
    fn file_list_is_unicode_and_available_before_format_ack() {
        let mut clipboard = file_clipboard(FILE_STREAMS | LONG_NAMES | HUGE_FILES);
        let size = (7u64 << 32) + 19;
        let actions = clipboard.set_files(10, files(size)).unwrap();
        let Action::Send(format) = &actions[1] else {
            panic!("file format")
        };
        assert_eq!(&format[8..12], &FILE_GROUP.to_le_bytes());
        assert_eq!(decode_text(&format[12..], 64).unwrap(), FILE_GROUP_NAME);
        let data = clipboard
            .receive(&pdu(DATA_REQUEST, 0, &FILE_GROUP.to_le_bytes()))
            .unwrap();
        let [Action::Send(bytes)] = data.as_slice() else {
            panic!("descriptor response")
        };
        assert_eq!(bytes.len(), 8 + 4 + 592);
        assert_eq!(&bytes[8..12], &1u32.to_le_bytes());
        let descriptor = &bytes[12..];
        assert_eq!(&descriptor[..4], &0x8000_4044u32.to_le_bytes());
        assert_eq!(&descriptor[36..40], &0x80u32.to_le_bytes());
        assert_eq!(&descriptor[64..68], &7u32.to_le_bytes());
        assert_eq!(&descriptor[68..72], &19u32.to_le_bytes());
        let name = files(0).remove(0).name;
        assert_eq!(&descriptor[72..72 + encoded(&name).len()], encoded(&name));
        assert!(!clipboard.local_ready());
        assert!(acknowledged(
            &clipboard
                .receive(&pdu(FORMAT_LIST_RESPONSE, OK, &[]))
                .unwrap()
        ));
        assert!(clipboard.local_ready());
    }

    #[test]
    fn file_size_range_and_eof_use_the_advertised_snapshot() {
        let mut clipboard = file_clipboard(FILE_STREAMS | LONG_NAMES | HUGE_FILES);
        let size = (3u64 << 32) + 7;
        clipboard.set_files(1, files(size)).unwrap();
        assert!(file_reply(
            &clipboard
                .receive(&file_request(7, 0, 1, 0, 8, None))
                .unwrap(),
            7,
            Some(&size.to_le_bytes())
        ));
        let actions = clipboard
            .receive(&file_request(8, 0, 2, size - 3, 64, None))
            .unwrap();
        let request = requested_file(&actions);
        assert_eq!(
            (
                request.offer_id,
                request.index,
                request.offset,
                request.length
            ),
            (1, 0, size - 3, 3)
        );
        assert!(file_reply(
            &clipboard
                .file_contents(request.id, Some(vec![1, 2, 3]))
                .unwrap(),
            8,
            Some(&[1, 2, 3])
        ));
        assert!(file_reply(
            &clipboard
                .receive(&file_request(9, 0, 2, size, 64, None))
                .unwrap(),
            9,
            Some(&[])
        ));
        assert!(file_reply(
            &clipboard
                .receive(&file_request(10, 0, 2, size + 1, 1, None))
                .unwrap(),
            10,
            None
        ));
        assert!(file_reply(
            &clipboard
                .receive(&file_request(11, 1, 1, 0, 8, None))
                .unwrap(),
            11,
            None
        ));
        let request = requested_file(
            &clipboard
                .receive(&file_request(12, 0, 2, 0, 3, None))
                .unwrap(),
        );
        assert!(file_reply(
            &clipboard
                .file_contents(request.id, Some(vec![1, 2]))
                .unwrap(),
            12,
            None
        ));
    }

    #[test]
    fn locked_offer_outlives_new_offers_and_unlock_waits_for_inflight_reads() {
        let mut clipboard = file_clipboard(FILE_STREAMS | LONG_NAMES | CAN_LOCK);
        clipboard.set_files(1, files(10)).unwrap();
        clipboard
            .receive(&pdu(FORMAT_LIST_RESPONSE, OK, &[]))
            .unwrap();
        clipboard
            .receive(&pdu(LOCK, 0, &77u32.to_le_bytes()))
            .unwrap();
        clipboard.set_files(2, files(20)).unwrap();
        let ack = clipboard
            .receive(&pdu(FORMAT_LIST_RESPONSE, OK, &[]))
            .unwrap();
        assert!(!released(&ack, 1));
        let request = requested_file(
            &clipboard
                .receive(&file_request(8, 0, 2, 0, 4, Some(77)))
                .unwrap(),
        );
        assert_eq!(request.offer_id, 1);
        let unlock = clipboard
            .receive(&pdu(UNLOCK, 0, &77u32.to_le_bytes()))
            .unwrap();
        assert!(!released(&unlock, 1));
        let reply = clipboard
            .file_contents(request.id, Some(vec![1; 4]))
            .unwrap();
        assert!(file_reply(&reply, 8, Some(&[1; 4])));
        assert!(released(&reply, 1));
        assert!(file_reply(
            &clipboard
                .receive(&file_request(9, 0, 2, 0, 4, Some(77)))
                .unwrap(),
            9,
            None
        ));
        assert_eq!(
            requested_file(
                &clipboard
                    .receive(&file_request(10, 0, 2, 0, 4, None))
                    .unwrap()
            )
            .offer_id,
            2
        );
    }

    #[test]
    fn superseded_queued_file_offer_releases_only_its_sources() {
        let mut clipboard = file_clipboard(FILE_STREAMS | LONG_NAMES);
        clipboard.set_files(1, files(10)).unwrap();
        clipboard.set_files(2, files(20)).unwrap();
        let next = clipboard.set_files(3, files(30)).unwrap();
        assert!(released(&next, 2));
        assert!(!released(&next, 1));
        assert!(!released(&next, 3));
        let old_ack = clipboard
            .receive(&pdu(FORMAT_LIST_RESPONSE, OK, &[]))
            .unwrap();
        assert!(!released(&old_ack, 1));
        assert!(!acknowledged(&old_ack));
        let fresh_ack = clipboard
            .receive(&pdu(FORMAT_LIST_RESPONSE, OK, &[]))
            .unwrap();
        assert!(released(&fresh_ack, 1));
        assert!(acknowledged(&fresh_ack));
        clipboard.set_text("new text".into()).unwrap();
        assert!(released(
            &clipboard
                .receive(&pdu(FORMAT_LIST_RESPONSE, OK, &[]))
                .unwrap(),
            3
        ));
        assert!(clipboard.set_files(2, files(20)).is_err());
    }

    #[test]
    fn cancellation_fails_pending_reads_and_does_not_resurrect_old_text() {
        let mut clipboard = file_clipboard(FILE_STREAMS | LONG_NAMES | CAN_LOCK);
        clipboard.set_text("old text".into()).unwrap();
        clipboard
            .receive(&pdu(FORMAT_LIST_RESPONSE, OK, &[]))
            .unwrap();
        clipboard.set_files(1, files(10)).unwrap();
        clipboard
            .receive(&pdu(LOCK, 0, &1u32.to_le_bytes()))
            .unwrap();
        let request = requested_file(
            &clipboard
                .receive(&file_request(8, 0, 2, 0, 4, Some(1)))
                .unwrap(),
        );
        let cancelled = clipboard.cancel_files().unwrap();
        assert!(file_reply(&cancelled, 8, None));
        assert!(released(&cancelled, 1));
        assert!(
            clipboard
                .file_contents(request.id, Some(vec![1; 4]))
                .unwrap()
                .is_empty()
        );
        clipboard
            .receive(&pdu(FORMAT_LIST_RESPONSE, OK, &[]))
            .unwrap();
        clipboard
            .receive(&pdu(FORMAT_LIST_RESPONSE, OK, &[]))
            .unwrap();
        assert!(!clipboard.local_ready());
        assert_eq!(response(&mut clipboard), pdu(DATA_RESPONSE, FAIL, &[]));
    }

    #[test]
    fn file_reads_and_metadata_are_bounded_and_fail_without_tearing_down_text() {
        let mut clipboard = file_clipboard(FILE_STREAMS | LONG_NAMES);
        clipboard.set_files(1, files(16 * 1024 * 1024)).unwrap();
        assert!(file_reply(
            &clipboard
                .receive(&file_request(
                    50,
                    0,
                    2,
                    0,
                    MAX_CLIPBOARD_FILE_CHUNK + 1,
                    None
                ))
                .unwrap(),
            50,
            None
        ));
        let mut first = None;
        for stream in 0..MAX_FILE_REQUESTS as u32 {
            let request = requested_file(
                &clipboard
                    .receive(&file_request(stream, 0, 2, 0, 1, None))
                    .unwrap(),
            );
            first.get_or_insert(request);
        }
        assert!(file_reply(
            &clipboard
                .receive(&file_request(50, 0, 2, 0, 1, None))
                .unwrap(),
            50,
            None
        ));
        clipboard.file_contents(first.unwrap().id, None).unwrap();
        assert_eq!(
            requested_file(
                &clipboard
                    .receive(&file_request(50, 0, 2, 0, 1, None))
                    .unwrap()
            )
            .length,
            1
        );
        assert!(
            validate_files(
                &[ClipboardFile {
                    directory: false,
                    name: "small".into(),
                    size: 0
                }],
                592
            )
            .is_err()
        );
        assert!(
            validate_files(
                &vec![
                    ClipboardFile {
                        directory: false,
                        name: "file".into(),
                        size: 0
                    };
                    MAX_CLIPBOARD_FILES + 1
                ],
                usize::MAX
            )
            .is_err()
        );
        clipboard.cancel_files().unwrap();
        clipboard
            .receive(&pdu(FORMAT_LIST_RESPONSE, OK, &[]))
            .unwrap();
        clipboard
            .receive(&pdu(FORMAT_LIST_RESPONSE, OK, &[]))
            .unwrap();
        clipboard.set_text("still usable".into()).unwrap();
        assert!(acknowledged(
            &clipboard
                .receive(&pdu(FORMAT_LIST_RESPONSE, OK, &[]))
                .unwrap()
        ));
    }

    #[test]
    fn file_paths_cannot_escape_or_alias_windows_devices_or_each_other() {
        for name in [
            "",
            ".",
            "..",
            "../secret",
            "C:\\secret",
            "a/b",
            "a\0b",
            "a\nb",
            "CON",
            "con.txt",
            "COM1.log",
            "LPT¹",
            "name.",
            "name ",
        ] {
            assert!(
                validate_files(
                    &[ClipboardFile {
                        directory: false,
                        name: name.into(),
                        size: 1
                    }],
                    4096
                )
                .is_err(),
                "{name:?}"
            );
        }
        assert!(
            validate_files(
                &[ClipboardFile {
                    directory: false,
                    name: "x".repeat(260),
                    size: 0
                }],
                4096
            )
            .is_err()
        );
        assert!(
            validate_files(
                &[
                    ClipboardFile {
                        directory: false,
                        name: "A.txt".into(),
                        size: 0
                    },
                    ClipboardFile {
                        directory: false,
                        name: "a.txt".into(),
                        size: 0
                    }
                ],
                4096
            )
            .is_err()
        );
        assert!(validate_files(&files(0), 4096).is_ok());
    }

    #[test]
    fn file_format_uses_complete_ascii_name_when_peer_lacks_long_names() {
        let mut clipboard = file_clipboard(FILE_STREAMS);
        let actions = clipboard.set_files(1, files(4)).unwrap();
        let Action::Send(bytes) = &actions[1] else {
            panic!("format list")
        };
        assert_eq!(&bytes[..4], &[2, 0, 4, 0]);
        assert_eq!(bytes.len(), 44);
        assert_eq!(
            &bytes[12..12 + FILE_GROUP_NAME.len()],
            FILE_GROUP_NAME.as_bytes()
        );
    }

    fn remote_formats(format_id: u32, long_names: bool, text: bool) -> Vec<u8> {
        let mut body = format_id.to_le_bytes().to_vec();
        if long_names {
            body.extend_from_slice(&encoded(FILE_GROUP_NAME));
        } else {
            body.extend_from_slice(FILE_GROUP_NAME.as_bytes());
            body.resize(36, 0);
        }
        if text {
            body.extend_from_slice(&UNICODE_TEXT.to_le_bytes());
            body.resize(body.len() + if long_names { 2 } else { 32 }, 0);
        }
        pdu(FORMAT_LIST, if long_names { 0 } else { 4 }, &body)
    }

    fn remote_offer(clipboard: &mut Clipboard, files: Vec<ClipboardFile>) -> u64 {
        clipboard
            .receive(&remote_formats(0xd321, clipboard.long_names, true))
            .unwrap();
        let actions = clipboard
            .receive(&pdu(DATA_RESPONSE, OK, &file_descriptors(&files)))
            .unwrap();
        actions
            .iter()
            .find_map(|action| match action {
                Action::Event(ChannelEvent::ClipboardFiles {
                    offer_id,
                    files: received,
                }) => {
                    assert_eq!(received, &files);
                    Some(*offer_id)
                }
                _ => None,
            })
            .expect("remote file offer")
    }

    fn remote_response(clipboard: &mut Clipboard, stream: u32, data: Option<&[u8]>) -> Vec<Action> {
        let Action::Send(bytes) = file_response(stream, data) else {
            unreachable!()
        };
        clipboard.receive(&bytes).unwrap()
    }

    #[test]
    fn remote_named_files_are_preferred_and_use_the_remote_format_id() {
        for long in [false, true] {
            let mut clipboard = file_clipboard(FILE_STREAMS | if long { LONG_NAMES } else { 0 });
            let actions = clipboard
                .receive(&remote_formats(0xda12, long, true))
                .unwrap();
            assert!(actions.iter().any(|action| matches!(action,
                Action::Send(bytes) if *bytes == pdu(DATA_REQUEST, 0, &0xda12u32.to_le_bytes())
            )));
            assert_eq!(sent(&actions, LOCK), 0);
            let actions = clipboard
                .receive(&pdu(DATA_RESPONSE, OK, &file_descriptors(&files(7))))
                .unwrap();
            assert!(actions.iter().any(|action| matches!(action,
                Action::Event(ChannelEvent::ClipboardFiles { files: received, .. }) if *received == files(7)
            )));
            assert_eq!(sent(&actions, FILE_REQUEST), 0);
            assert_eq!(sent(&clipboard.request(), DATA_REQUEST), 0);
        }
        let mut disabled = ready_clipboard();
        let actions = disabled
            .receive(&remote_formats(0xda12, false, true))
            .unwrap();
        assert!(actions.iter().any(|action| matches!(action,
            Action::Send(bytes) if *bytes == pdu(DATA_REQUEST, 0, &UNICODE_TEXT.to_le_bytes())
        )));

        let mut clipboard = file_clipboard(FILE_STREAMS);
        clipboard.limit = 0;
        clipboard.max_message = 12 + MAX_CLIPBOARD_FILES * FILE_DESCRIPTOR_SIZE;
        remote_offer(
            &mut clipboard,
            (0..MAX_CLIPBOARD_FILES)
                .map(|index| ClipboardFile {
                    directory: false,
                    name: format!("file-{index}"),
                    size: 0,
                })
                .collect(),
        );
    }

    #[test]
    fn metadata_and_text_responses_keep_their_original_untagged_request_slot() {
        let mut clipboard = file_clipboard(FILE_STREAMS);
        clipboard.receive(&format_list()).unwrap();
        let actions = clipboard
            .receive(&remote_formats(0xd123, false, true))
            .unwrap();
        assert_eq!(sent(&actions, DATA_REQUEST), 0);
        let actions = clipboard
            .receive(&pdu(DATA_RESPONSE, OK, &encoded("old text")))
            .unwrap();
        assert!(actions.iter().any(|action| matches!(action,
            Action::Send(bytes) if *bytes == pdu(DATA_REQUEST, 0, &0xd123u32.to_le_bytes())
        )));
        assert!(
            !actions
                .iter()
                .any(|action| matches!(action, Action::Event(ChannelEvent::ClipboardText(_))))
        );

        clipboard.receive(&format_list()).unwrap();
        // This obsolete file metadata is larger than the configured text limit.
        // It must be discarded by generation before selecting a decoder or limit.
        clipboard.limit = 1;
        let actions = clipboard
            .receive(&pdu(DATA_RESPONSE, OK, &file_descriptors(&files(5))))
            .unwrap();
        assert_eq!(sent(&actions, DATA_REQUEST), 1);
        assert!(
            !actions
                .iter()
                .any(|action| matches!(action, Action::Event(ChannelEvent::ClipboardFiles { .. })))
        );
        let actions = clipboard
            .receive(&pdu(DATA_RESPONSE, OK, &encoded("x")))
            .unwrap();
        assert!(
            matches!(&actions[0], Action::Event(ChannelEvent::ClipboardText(text)) if text == "x")
        );
    }

    #[test]
    fn remote_ranges_keep_offsets_indexes_locks_and_out_of_order_correlations() {
        let mut clipboard = file_clipboard(FILE_STREAMS | LONG_NAMES | CAN_LOCK | HUGE_FILES);
        let offset = (3u64 << 32) + 17;
        let offer_id = remote_offer(&mut clipboard, files(offset + 8));
        let actions = clipboard.request_file(offer_id, 0, offset, 3).unwrap();
        assert!(matches!(&actions[0], Action::Send(bytes)
            if *bytes == file_request(1, 0, 2, offset, 3, Some(1))));
        clipboard.request_file(offer_id, 0, offset + 3, 5).unwrap();
        let actions = remote_response(&mut clipboard, 2, Some(&[4; 5]));
        assert!(
            matches!(&actions[0], Action::Event(ChannelEvent::ClipboardFileContents { request, data })
            if request.offer_id == offer_id && request.offset == offset + 3 && request.length == 5
                && data.as_deref() == Some(&[4; 5][..]))
        );
        let actions = remote_response(&mut clipboard, 1, Some(&[2; 3]));
        assert!(
            matches!(&actions[0], Action::Event(ChannelEvent::ClipboardFileContents { request, data })
            if request.offer_id == offer_id && request.offset == offset && request.index == 0
                && data.as_deref() == Some(&[2; 3][..]))
        );
        let actions = clipboard.cancel_remote_files(offer_id).unwrap();
        assert_eq!(sent(&actions, UNLOCK), 1);
        assert!(clipboard.cancel_remote_files(offer_id).unwrap().is_empty());
    }

    #[test]
    fn cancelling_or_replacing_remote_files_drops_late_responses_without_reusing_ids() {
        let mut clipboard = file_clipboard(FILE_STREAMS | CAN_LOCK);
        let old = remote_offer(&mut clipboard, files(4));
        clipboard.request_file(old, 0, 0, 4).unwrap();
        let actions = clipboard
            .receive(&remote_formats(0xd123, false, false))
            .unwrap();
        assert_eq!(sent(&actions, UNLOCK), 1);
        assert_eq!(sent(&actions, LOCK), 1);
        assert!(actions.iter().any(|action| matches!(action,
            Action::Event(ChannelEvent::ClipboardRemoteFilesCancelled { offer_id }) if *offer_id == old)));
        assert!(remote_response(&mut clipboard, 1, Some(&[9; 4])).is_empty());
        clipboard.cancel_remote_files(clipboard.generation).unwrap();
        // Cancellation retains the old metadata response slot until consumed.
        clipboard.receive(&format_list()).unwrap();
        let actions = clipboard
            .receive(&pdu(DATA_RESPONSE, OK, &file_descriptors(&files(4))))
            .unwrap();
        assert_eq!(sent(&actions, DATA_REQUEST), 1);
        clipboard
            .receive(&pdu(DATA_RESPONSE, OK, &encoded("text")))
            .unwrap();

        let fresh = remote_offer(&mut clipboard, files(4));
        assert_ne!(fresh, old);
        clipboard.request_file(fresh, 0, 0, 4).unwrap();
        assert!(clipboard.remote_reads.contains_key(&2));
        assert!(remote_response(&mut clipboard, 1, Some(&[9; 4])).is_empty());
        let actions = clipboard.set_text("local".into()).unwrap();
        assert_eq!(sent(&actions, UNLOCK), 1);
        assert!(remote_response(&mut clipboard, 2, Some(&[9; 4])).is_empty());
    }

    #[test]
    fn remote_invalid_descriptors_fail_without_breaking_text_clipboard() {
        let valid = file_descriptors(&files(4));
        let mut invalid = Vec::new();
        for attributes in [0x40u32, 0x400] {
            let mut data = valid.clone();
            data[40..44].copy_from_slice(&attributes.to_le_bytes());
            invalid.push(data);
        }
        let mut no_size = valid.clone();
        no_size[4..8].copy_from_slice(&4u32.to_le_bytes());
        invalid.push(no_size);
        invalid.push(valid[..valid.len() - 1].to_vec());
        invalid.push(file_descriptors(&[ClipboardFile {
            directory: false,
            name: "../escape".into(),
            size: 4,
        }]));
        invalid.push(file_descriptors(&[
            ClipboardFile {
                directory: false,
                name: "a".into(),
                size: 4,
            },
            ClipboardFile {
                directory: false,
                name: "A".into(),
                size: 4,
            },
        ]));
        let overflow = file_descriptors(&[
            ClipboardFile {
                directory: false,
                name: "large".into(),
                size: u64::MAX,
            },
            ClipboardFile {
                directory: false,
                name: "extra".into(),
                size: 1,
            },
        ]);
        assert!(matches!(
            parse_file_descriptors(&overflow, 4096, HUGE_FILES),
            Err(Error::Limit("remote clipboard total file size"))
        ));
        invalid.push(overflow);
        for data in invalid {
            let mut clipboard = file_clipboard(FILE_STREAMS | CAN_LOCK | HUGE_FILES);
            clipboard
                .receive(&remote_formats(0xd123, false, false))
                .unwrap();
            let actions = clipboard.receive(&pdu(DATA_RESPONSE, OK, &data)).unwrap();
            assert_eq!(sent(&actions, UNLOCK), 1);
            assert!(
                actions.iter().any(|action| matches!(
                    action,
                    Action::Event(ChannelEvent::RequestRejected(_))
                ))
            );
            assert!(clipboard.remote_files.is_none());
            clipboard.receive(&format_list()).unwrap();
            let actions = clipboard
                .receive(&pdu(DATA_RESPONSE, OK, &encoded("still works")))
                .unwrap();
            assert!(
                matches!(&actions[0], Action::Event(ChannelEvent::ClipboardText(text)) if text == "still works")
            );
        }
    }

    #[test]
    fn remote_file_requests_are_bounded_and_failures_leave_the_offer_usable() {
        let mut clipboard = file_clipboard(FILE_STREAMS);
        let offer_id = remote_offer(
            &mut clipboard,
            files(2 * u64::from(MAX_CLIPBOARD_FILE_CHUNK)),
        );
        for (index, offset, length) in [
            (1, 0, 1),
            (0, 0, 0),
            (0, u64::MAX, 1),
            (0, 0, MAX_CLIPBOARD_FILE_CHUNK + 1),
        ] {
            assert!(
                clipboard
                    .request_file(offer_id, index, offset, length)
                    .is_err()
            );
        }
        assert!(clipboard.request_file(offer_id + 1, 0, 0, 1).is_err());
        for _ in 0..MAX_FILE_REQUESTS {
            clipboard.request_file(offer_id, 0, 0, 4).unwrap();
        }
        assert!(clipboard.request_file(offer_id, 0, 0, 4).is_err());
        for (stream, data) in [(1, None), (2, Some(&[1, 2][..])), (3, Some(&[1; 5][..]))] {
            let actions = remote_response(&mut clipboard, stream, data);
            assert!(matches!(
                &actions[0],
                Action::Event(ChannelEvent::ClipboardFileContents { data: None, .. })
            ));
        }
        clipboard.request_file(offer_id, 0, 0, 4).unwrap();
        assert!(remote_response(&mut clipboard, 1, Some(&[9; 4])).is_empty());
        assert!(!clipboard.remote_reads.is_empty());
        clipboard.cancel_remote_files(offer_id).unwrap();
        assert!(clipboard.remote_reads.is_empty());
    }
}
