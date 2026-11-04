//! ZMODEM autostart detection and an isolated bridge to the system lrzsz tools.
//!
//! Protocol framing follows lrzsz 0.12.21rc's `zm.c`. The helpers implement the
//! transfer protocol; this module only recognizes CRC-checked hexadecimal
//! autostart/final headers and preserves bytes following the final handshake.

use crate::i18n::messages as i18n_key;

#[cfg(target_os = "linux")]
mod linux;
mod session;
pub(super) use session::Transfer;
pub use session::{ZmodemPhase, ZmodemStatus};

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{Context as _, Result, anyhow, bail};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

const HEX_PREFIX: &[u8] = b"**\x18B";
const HEADER_LEN: usize = 20;
const QUEUE_CAPACITY: usize = 16;
const CHUNK_BYTES: usize = 64 * 1024;
const IO_TIMEOUT: Duration = Duration::from_secs(2);
const CANCEL_BYTES: &[u8] = b"\x18\x18\x18\x18\x18\x18\x18\x18\x08\x08\x08\x08\x08\x08\x08\x08";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Direction {
    Download,
    Upload,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Request {
    pub direction: Direction,
    /// The complete validated header and all following bytes from the same read.
    pub initial: Vec<u8>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Detection {
    pub visible: Vec<u8>,
    pub request: Option<Request>,
}

#[derive(Debug, Default)]
pub struct Detector {
    pending: Vec<u8>,
}

impl Detector {
    pub fn has_pending(&self) -> bool {
        !self.pending.is_empty()
    }

    /// A full ZHEX prefix is present, but its CRC-checked header is incomplete.
    /// Give these candidates more time than an ordinary trailing '*' to arrive.
    pub fn pending_header(&self) -> bool {
        self.pending.starts_with(HEX_PREFIX)
    }

    pub fn feed(&mut self, bytes: &[u8]) -> Detection {
        self.pending.extend_from_slice(bytes);
        let mut offset = 0;
        while let Some(relative) = find_prefix(&self.pending[offset..]) {
            let start = offset + relative;
            if self.pending.len() - start < HEADER_LEN {
                let visible = self.pending.drain(..start).collect();
                return Detection {
                    visible,
                    request: None,
                };
            }
            let direction = match hex_header(&self.pending[start..]) {
                Some(0) => Some(Direction::Download),
                Some(1) => Some(Direction::Upload),
                _ => None,
            };
            if let Some(direction) = direction {
                let initial = self.pending.split_off(start);
                let visible = std::mem::take(&mut self.pending);
                return Detection {
                    visible,
                    request: Some(Request { direction, initial }),
                };
            }
            offset = start + 1;
        }
        let keep = prefix_suffix_len(&self.pending);
        let visible = self.pending.drain(..self.pending.len() - keep).collect();
        Detection {
            visible,
            request: None,
        }
    }

    pub fn flush(&mut self) -> Vec<u8> {
        std::mem::take(&mut self.pending)
    }
}

fn find_prefix(bytes: &[u8]) -> Option<usize> {
    bytes
        .windows(HEX_PREFIX.len())
        .position(|window| window == HEX_PREFIX)
}

fn prefix_suffix_len(bytes: &[u8]) -> usize {
    (1..HEX_PREFIX.len())
        .rev()
        .find(|&len| bytes.ends_with(&HEX_PREFIX[..len]))
        .unwrap_or(0)
}

fn hex_header(bytes: &[u8]) -> Option<u8> {
    if bytes.len() < HEADER_LEN
        || !bytes.starts_with(HEX_PREFIX)
        || bytes[18] & 0x7f != b'\r'
        || bytes[19] & 0x7f != b'\n'
    {
        return None;
    }
    let mut decoded = [0_u8; 7];
    for (index, pair) in bytes[4..18].as_chunks::<2>().0.iter().enumerate() {
        decoded[index] = hex_digit(pair[0])? << 4 | hex_digit(pair[1])?;
    }
    let expected = u16::from_be_bytes([decoded[5], decoded[6]]);
    (crc16(&decoded[..5]) == expected).then_some(decoded[0])
}

fn hex_digit(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        b'A'..=b'F' => Some(byte - b'A' + 10),
        _ => None,
    }
}

fn crc16(bytes: &[u8]) -> u16 {
    let mut crc = 0_u16;
    for &byte in bytes {
        crc ^= u16::from(byte) << 8;
        for _ in 0..8 {
            crc = if crc & 0x8000 == 0 {
                crc << 1
            } else {
                (crc << 1) ^ 0x1021
            };
        }
    }
    crc
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Selection {
    DownloadDirectory(PathBuf),
    UploadFiles(Vec<PathBuf>),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Progress {
    pub direction: Direction,
    /// Aggregate payload bytes reported by the helper (never wire overhead).
    pub bytes_transferred: u64,
    pub total_bytes: Option<u64>,
    pub files_completed: usize,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Outcome {
    pub success: bool,
    pub cancelled: bool,
    pub message: String,
    /// Successfully published downloads, or the uploaded local paths on success.
    pub files: Vec<PathBuf>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Event {
    Write(Vec<u8>),
    /// Ordinary terminal output after the final protocol boundary.
    Terminal(Vec<u8>),
    Progress(Progress),
    Finished(Outcome),
}

struct InputState {
    sender: Option<mpsc::Sender<Vec<u8>>>,
    finalized: bool,
}

type InputRoute = Arc<tokio::sync::Mutex<InputState>>;

#[derive(Debug)]
pub struct FeedError {
    /// The suffix that was not accepted by the helper/event queue.
    pub bytes: Vec<u8>,
    /// True only after the final protocol boundary was validated. The actor can
    /// render this suffix directly; false means it must suppress protocol data.
    pub terminal: bool,
    pub message: String,
}

impl std::fmt::Display for FeedError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.message.fmt(formatter)
    }
}

impl std::error::Error for FeedError {}

pub struct Engine {
    input: InputRoute,
    events: mpsc::Sender<Event>,
    cancel: CancellationToken,
    progress: Arc<Mutex<Progress>>,
    finished: Arc<AtomicBool>,
    finalized: Arc<AtomicBool>,
}

impl Engine {
    #[cfg(test)]
    pub(super) fn finalized_fixture(events: mpsc::Sender<Event>, handshake_complete: bool) -> Self {
        let cancel = CancellationToken::new();
        if !handshake_complete {
            cancel.cancel();
        }
        Self {
            input: Arc::new(tokio::sync::Mutex::new(InputState {
                sender: None,
                finalized: true,
            })),
            events,
            cancel,
            progress: Arc::new(Mutex::new(Progress {
                direction: Direction::Download,
                bytes_transferred: 0,
                total_bytes: None,
                files_completed: 0,
            })),
            finished: Arc::new(AtomicBool::new(handshake_complete)),
            finalized: Arc::new(AtomicBool::new(true)),
        }
    }

    pub fn start(
        request: Request,
        selection: Selection,
        events: mpsc::Sender<Event>,
        cancel: CancellationToken,
    ) -> Result<Self> {
        if !matches!(
            (&request.direction, &selection),
            (Direction::Download, Selection::DownloadDirectory(_))
                | (Direction::Upload, Selection::UploadFiles(_))
        ) {
            bail!(crate::i18n::tr(
                i18n_key::CONNECTIONS_THE_SELECTED_FILES_DO_NOT_MATCH_THE_ZMODEM_TRANSFER_DIRECTION
            ));
        }
        #[cfg(not(target_os = "linux"))]
        {
            let _ = (request, selection, events, cancel);
            bail!(crate::i18n::tr(
                i18n_key::CONNECTIONS_ZMODEM_FILE_TRANSFER_IS_CURRENTLY_SUPPORTED_ONLY_ON_LINUX
            ));
        }
        #[cfg(target_os = "linux")]
        {
            let helper = linux::find_helper(request.direction)?;
            let cancel = cancel.child_token();
            let progress = Arc::new(Mutex::new(Progress {
                direction: request.direction,
                bytes_transferred: 0,
                total_bytes: None,
                files_completed: 0,
            }));
            let (sender, receiver) = mpsc::channel(QUEUE_CAPACITY);
            let input = Arc::new(tokio::sync::Mutex::new(InputState {
                sender: Some(sender),
                finalized: false,
            }));
            let finished = Arc::new(AtomicBool::new(false));
            let finalized = Arc::new(AtomicBool::new(false));
            let worker = linux::Worker {
                input: input.clone(),
                events: events.clone(),
                cancel: cancel.clone(),
                progress: progress.clone(),
                finished: finished.clone(),
                finalized: finalized.clone(),
            };
            tokio::spawn(worker.run(helper, request, selection, receiver));
            Ok(Self {
                input,
                events,
                cancel,
                progress,
                finished,
                finalized,
            })
        }
    }

    /// Bounded and cancellable: a stalled helper cannot indefinitely block the
    /// session actor. The actor should cancel the transfer if this returns Err.
    pub async fn feed(&self, bytes: Vec<u8>) -> std::result::Result<(), FeedError> {
        for (index, chunk) in bytes.chunks(CHUNK_BYTES).enumerate() {
            let result: Result<()> = tokio::select! {
                biased;
                _ = self.cancel.cancelled() => Err(anyhow!(crate::i18n::tr(i18n_key::CONNECTIONS_ZMODEM_CANCELLED))),
                result = tokio::time::timeout(IO_TIMEOUT, async {
                    let route = self.input.lock().await;
                    if route.finalized {
                        bail!(crate::i18n::tr(i18n_key::CONNECTIONS_ZMODEM_HAS_ENDED));
                    }
                    if let Some(sender) = route.sender.as_ref() {
                        sender.send(chunk.to_vec()).await.context(crate::i18n::tr(i18n_key::CONNECTIONS_THE_ZMODEM_RECEIVE_CHANNEL_IS_CLOSED))
                    } else if self.finished.load(Ordering::Acquire) {
                        self.events.send(Event::Terminal(chunk.to_vec())).await
                            .context(crate::i18n::tr(i18n_key::CONNECTIONS_THE_ZMODEM_EVENT_CHANNEL_IS_CLOSED))
                    } else {
                        bail!(crate::i18n::tr(i18n_key::CONNECTIONS_THE_ZMODEM_RECEIVE_CHANNEL_IS_CLOSED));
                    }
                }) => result.context(crate::i18n::tr(i18n_key::CONNECTIONS_ZMODEM_INPUT_TIMED_OUT)).and_then(|result| result),
            };
            if let Err(error) = result {
                return Err(FeedError {
                    bytes: bytes[index * CHUNK_BYTES..].to_vec(),
                    terminal: self.finalized.load(Ordering::Acquire)
                        && self.finished.load(Ordering::Acquire),
                    message: format!("{error:#}"),
                });
            }
        }
        Ok(())
    }

    pub fn cancel(&self) {
        self.cancel.cancel();
    }

    pub fn status(&self) -> Progress {
        self.progress
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .clone()
    }
}

impl Drop for Engine {
    fn drop(&mut self) {
        self.cancel.cancel();
    }
}

/// Only recognizes completion after the *local* helper emits ZFIN. In
/// particular, a ZFIN-looking byte sequence in downloaded binary data does not
/// arm the OO scanner or turn file content into terminal output.
struct FinishGate {
    direction: Direction,
    pending: Vec<u8>,
    finished: bool,
}

impl FinishGate {
    fn new(direction: Direction) -> Self {
        Self {
            direction,
            pending: Vec::new(),
            finished: false,
        }
    }

    fn feed(&mut self, bytes: &[u8], armed: bool) -> (Vec<u8>, Vec<u8>) {
        if self.finished {
            return (Vec::new(), bytes.to_vec());
        }
        if !armed {
            return (bytes.to_vec(), Vec::new());
        }
        self.pending.extend_from_slice(bytes);
        match self.direction {
            Direction::Download => {
                if let Some(start) = self.pending.windows(2).position(|bytes| bytes == b"OO") {
                    let terminal = self.pending.split_off(start + 2);
                    self.finished = true;
                    (std::mem::take(&mut self.pending), terminal)
                } else {
                    let keep = usize::from(self.pending.ends_with(b"O"));
                    let helper = self.pending.drain(..self.pending.len() - keep).collect();
                    (helper, Vec::new())
                }
            }
            Direction::Upload => {
                let mut offset = 0;
                while let Some(relative) = find_prefix(&self.pending[offset..]) {
                    let start = offset + relative;
                    if self.pending.len() - start < HEADER_LEN {
                        return (self.pending.drain(..start).collect(), Vec::new());
                    }
                    if hex_header(&self.pending[start..]) == Some(8) {
                        let terminal = self.pending.split_off(start + HEADER_LEN);
                        self.finished = true;
                        return (std::mem::take(&mut self.pending), terminal);
                    }
                    offset = start + 1;
                }
                let keep = prefix_suffix_len(&self.pending);
                (
                    self.pending.drain(..self.pending.len() - keep).collect(),
                    Vec::new(),
                )
            }
        }
    }
}

#[derive(Default)]
struct FinalHeaderScanner {
    tail: Vec<u8>,
}

impl FinalHeaderScanner {
    fn feed(&mut self, bytes: &[u8]) -> bool {
        self.tail.extend_from_slice(bytes);
        let found = self
            .tail
            .windows(HEADER_LEN)
            .any(|window| hex_header(window) == Some(8));
        let discard = self.tail.len().saturating_sub(HEADER_LEN - 1);
        self.tail.drain(..discard);
        found
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn header(kind: u8, flags: u8) -> Vec<u8> {
        let data = [kind, 0, 0, 0, flags];
        let mut result = HEX_PREFIX.to_vec();
        for byte in data {
            result.extend_from_slice(format!("{byte:02x}").as_bytes());
        }
        result.extend_from_slice(format!("{:04x}\r\n", crc16(&data)).as_bytes());
        result
    }

    #[test]
    fn zmodem_detects_crc_checked_headers_at_every_chunk_boundary() {
        // Captured from Ubuntu's unmodified lrzsz 0.12.21rc rz helper.
        let handshake = b"**\x18B0100000063f694\r\x8a\x11";
        for split in 0..=handshake.len() {
            let mut detector = Detector::default();
            let first = detector.feed(&handshake[..split]);
            let second = detector.feed(&handshake[split..]);
            let request = first.request.or(second.request).expect("valid autostart");
            assert_eq!(request.direction, Direction::Upload);
            assert!(first.visible.is_empty());
            if split < HEADER_LEN {
                assert_eq!(request.initial, handshake);
            }
        }
    }

    #[test]
    fn zmodem_rejects_bad_crc_and_preserves_terminal_bytes() {
        let mut invalid = header(0, 0);
        invalid[15] = b'1';
        let mut detector = Detector::default();
        let mut visible = Vec::new();
        for byte in b"prompt **"
            .iter()
            .chain(invalid.iter())
            .chain(b"tail *".iter())
        {
            let result = detector.feed(&[*byte]);
            assert!(result.request.is_none());
            assert!(detector.pending.len() < HEADER_LEN);
            visible.extend(result.visible);
        }
        visible.extend(detector.flush());
        let expected = [b"prompt **".as_slice(), &invalid, b"tail *"].concat();
        assert_eq!(visible, expected);
    }

    #[test]
    fn zmodem_download_final_gate_does_not_leak_binary_or_drop_prompt() {
        let mut gate = FinishGate::new(Direction::Download);
        let binary = [header(8, 0), b"OObinary payload".to_vec()].concat();
        assert_eq!(gate.feed(&binary, false), (binary, Vec::new()));
        assert_eq!(gate.feed(b"O", true), (Vec::new(), Vec::new()));
        assert_eq!(
            gate.feed(b"O\r\nshell$ ", true),
            (b"OO".to_vec(), b"\r\nshell$ ".to_vec())
        );
        assert_eq!(gate.feed(b"next", true), (Vec::new(), b"next".to_vec()));
    }

    #[test]
    fn zmodem_upload_final_header_preserves_tail_at_every_boundary() {
        let final_header = header(8, 0);
        for split in 0..=final_header.len() {
            let mut gate = FinishGate::new(Direction::Upload);
            let (mut protocol, mut visible) = gate.feed(&final_header[..split], true);
            let tail = [&final_header[split..], b"shell$ "].concat();
            let (rest, terminal) = gate.feed(&tail, true);
            protocol.extend(rest);
            visible.extend(terminal);
            assert_eq!(protocol, final_header);
            assert_eq!(visible, b"shell$ ");
        }
    }
}
