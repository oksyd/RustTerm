//! Native RDP transport, independent of GPUI. All queues and frame sizes are bounded.
//! Frames replace the previous value; an idle session waits on network/input readiness.
use crate::i18n::messages as i18n_key;

#[cfg(target_os = "linux")]
mod audio;
#[cfg(not(target_os = "linux"))]
#[path = "unsupported/audio.rs"]
mod audio;
mod commands;
#[cfg(target_os = "linux")]
mod downloads;
#[cfg(not(target_os = "linux"))]
#[path = "unsupported/downloads.rs"]
mod downloads;
mod file_transfer;
#[cfg(target_os = "linux")]
mod files;
#[cfg(not(target_os = "linux"))]
#[path = "unsupported/files.rs"]
mod files;
mod frames;
mod protocol;
#[cfg(target_os = "linux")]
mod shared_drives;
#[cfg(not(target_os = "linux"))]
#[path = "unsupported/shared_drives.rs"]
mod shared_drives;
mod tls;
mod transport;

use crate::config::{RdpConfig, Secret};
use std::sync::{
    Arc, Mutex, OnceLock,
    atomic::{AtomicBool, Ordering},
};
use tokio::sync::{mpsc, oneshot, watch};
use tokio_util::sync::CancellationToken;

pub const MAX_FRAME_PIXELS: usize = 33_554_432;
// Leave room for the renderer's one-pixel sampling border in a 512px atlas slot.
pub const RDP_TILE_SIZE: u16 = 510;
pub const MAX_CLIPBOARD_BYTES: usize = 1024 * 1024;
pub const MAX_CLIPBOARD_CONTENT_BYTES: usize = 64 * 1024 * 1024;
const MAX_TEXT_BYTES: usize = 4096;
const COMMAND_CAPACITY: usize = 64;
const COMMAND_BYTE_LIMIT: usize = MAX_CLIPBOARD_CONTENT_BYTES + 2 * MAX_CLIPBOARD_BYTES;

#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum RdpError {
    #[error("{}", crate::i18n::tr(i18n_key::RDP_INVALID_RDP_CONNECTION_SETTINGS))]
    InvalidConfig,
    #[error(
        "{}",
        crate::i18n::tr(i18n_key::RDP_COULD_NOT_CONNECT_TO_THE_RDP_SERVER)
    )]
    Connection,
    #[error(
        "{}",
        crate::i18n::tr(i18n_key::RDP_RDP_TLS_VERIFICATION_OR_HANDSHAKE_FAILED)
    )]
    Tls,
    #[error(
        "{}",
        crate::i18n::tr(i18n_key::RDP_THE_RDP_CERTIFICATE_WAS_REJECTED_CREDENTIALS_WERE_NOT_SENT)
    )]
    CertificateRejected,
    #[error(
        "{}",
        crate::i18n::tr(i18n_key::RDP_RDP_REQUIRES_NETWORK_LEVEL_AUTHENTICATION_NLA)
    )]
    NlaRequired,
    #[error("{}", crate::i18n::tr(i18n_key::RDP_RDP_AUTHENTICATION_FAILED))]
    Authentication,
    #[error("{message}: {0}", message = crate::i18n::tr(i18n_key::RDP_RDP_RETURNED_AN_INVALID_OR_UNSUPPORTED_PROTOCOL_MESSAGE))]
    Protocol(&'static str),
    #[error("{message}: {0}", message = crate::i18n::tr(i18n_key::RDP_THIS_RDP_CAPABILITY_IS_NOT_SUPPORTED_YET))]
    Unsupported(&'static str),
    #[error("{message} (0x{0:08x})", message = crate::i18n::tr(i18n_key::RDP_THE_RDP_SERVER_RETURNED_AN_ERROR))]
    Server(u32),
    #[error("{message}: {0}", message = crate::i18n::tr(i18n_key::RDP_RDP_EXCEEDED_THE_FRAME_MESSAGE_OR_CLIPBOARD_SIZE_LIMIT))]
    Limit(&'static str),
    #[error("{}", crate::i18n::tr(i18n_key::RDP_THE_RDP_OPERATION_TIMED_OUT))]
    Timeout,
    #[error(
        "{}",
        crate::i18n::tr(i18n_key::RDP_THE_REMOTE_CLIPBOARD_IS_NOT_READY_TRY_PASTING_AGAIN)
    )]
    ClipboardUnavailable,
    #[error(
        "{}",
        crate::i18n::tr(
            i18n_key::RDP_SELECT_UP_TO_256_REGULAR_FILES_WITH_DISTINCT_WINDOWS_COMPATIBLE
        )
    )]
    FileSelection,
    #[error(
        "{}",
        crate::i18n::tr(i18n_key::RDP_A_SOURCE_FILE_CHANGED_DURING_TRANSFER_COPY_AND_PASTE_AGAIN)
    )]
    FileChanged,
    #[error(
        "{}",
        crate::i18n::tr(i18n_key::RDP_COULD_NOT_READ_A_FILE_FOR_THE_REMOTE_DESKTOP)
    )]
    FileUnavailable,
    #[error(
        "{}",
        crate::i18n::tr(i18n_key::RDP_COULD_NOT_RECEIVE_FILES_FROM_THE_REMOTE_DESKTOP)
    )]
    FileDownload,
    #[error(
        "{}",
        crate::i18n::tr(i18n_key::RDP_NOT_ENOUGH_LOCAL_DISK_SPACE_TO_RECEIVE_THE_REMOTE_FILES)
    )]
    FileSpace,
    #[error("{}", crate::i18n::tr(i18n_key::COMMON_FILE_TRANSFER_CANCELLED))]
    FileCancelled,
    #[error(
        "{}",
        crate::i18n::tr(i18n_key::RDP_REMOTE_AUDIO_COULD_NOT_BE_PLAYED_ON_THIS_COMPUTER)
    )]
    AudioUnavailable,
    #[error(
        "{}",
        crate::i18n::tr(i18n_key::RDP_THE_LOCAL_MICROPHONE_COULD_NOT_BE_OPENED_FOR_THIS_REMOTE)
    )]
    MicrophoneUnavailable,
    #[error("{}", crate::i18n::tr(i18n_key::RDP_RDP_IS_DISCONNECTED))]
    Disconnected,
    #[error(
        "{}",
        crate::i18n::tr(i18n_key::RDP_THE_RDP_MESSAGE_QUEUE_IS_FULL_RECONNECT_TO_CONTINUE)
    )]
    Busy,
    #[error(
        "{}",
        crate::i18n::tr(i18n_key::RDP_RDP_INPUT_IS_TEMPORARILY_BUSY_PLEASE_RETRY)
    )]
    InputBusy,
    #[error(
        "{}",
        crate::i18n::tr(i18n_key::RDP_THE_RDP_BACKGROUND_RUNTIME_COULD_NOT_START)
    )]
    Runtime,
}
pub type Result<T> = std::result::Result<T, RdpError>;

impl RdpError {
    pub fn diagnostic_code(&self) -> &'static str {
        match self {
            Self::InvalidConfig => "rdp-error-invalid-config",
            Self::Connection => "rdp-error-connection",
            Self::Tls => "rdp-error-tls",
            Self::CertificateRejected => "rdp-error-certificate-rejected",
            Self::NlaRequired => "rdp-error-nla-required",
            Self::Authentication => "rdp-error-authentication",
            Self::Protocol(_) => "rdp-error-protocol",
            Self::Unsupported(_) => "rdp-error-unsupported",
            Self::Server(_) => "rdp-error-server",
            Self::Limit(_) => "rdp-error-limit",
            Self::Timeout => "rdp-error-timeout",
            Self::ClipboardUnavailable => "rdp-error-clipboard-unavailable",
            Self::FileSelection => "rdp-files-selection",
            Self::FileChanged => "rdp-files-changed",
            Self::FileUnavailable => "rdp-files-unavailable",
            Self::FileDownload => "rdp-files-download",
            Self::FileSpace => "rdp-files-disk-space",
            Self::FileCancelled => "rdp-files-cancelled",
            Self::AudioUnavailable => "rdp-audio-unavailable",
            Self::MicrophoneUnavailable => "rdp-microphone-unavailable",
            Self::Disconnected => "rdp-error-disconnected",
            Self::Busy => "rdp-error-busy",
            Self::InputBusy => "rdp-input-busy",
            Self::Runtime => "rdp-error-runtime",
        }
    }

    pub fn diagnostic_context(&self) -> Option<&'static str> {
        match self {
            Self::Protocol(context) | Self::Unsupported(context) | Self::Limit(context) => {
                Some(context)
            }
            _ => None,
        }
    }
}

impl From<rustrdp::Error> for RdpError {
    fn from(error: rustrdp::Error) -> Self {
        match error {
            rustrdp::Error::Limit(context) => Self::Limit(context),
            rustrdp::Error::Backpressure => Self::Busy,
            rustrdp::Error::Unsupported(feature) => Self::Unsupported(feature),
            rustrdp::Error::Server(code) => Self::Server(code),
            rustrdp::Error::Invalid(context) => Self::Protocol(context),
            rustrdp::Error::Truncated => Self::Protocol("truncated protocol message"),
        }
    }
}

/// A complete desktop snapshot. Unchanged tiles are shared with earlier frames,
/// so skipping a published frame never loses an intervening update.
pub struct RdpFrame {
    /// Assigned when the snapshot is published, independently of server frame IDs.
    pub sequence: u64,
    pub published_at: std::time::Instant,
    pub acknowledgement: Option<rustrdp::FrameToken>,
    pub width: u16,
    pub height: u16,
    /// Negotiated desktop color depth; tile pixels always use packed BGRA8.
    pub bits_per_pixel: u16,
    pub tiles: Vec<Arc<RdpTile>>,
    /// Remote software pointer pixels (or a server hide request) replace the local cursor.
    pub hide_local_pointer: bool,
}

/// An immutable desktop region containing packed, top-down BGRA8 pixels.
pub struct RdpTile {
    pub x: u16,
    pub y: u16,
    pub width: u16,
    pub height: u16,
    pub pixels: Vec<u8>,
}

/// Aggregated transport and CPU timings. Counts include only the given interval;
/// published frames can include local pointer movement without a server update.
#[derive(Debug, Default)]
pub struct RdpPerformance {
    pub interval: std::time::Duration,
    pub packets: u64,
    pub fastpath_packets: u64,
    pub wire_bytes: u64,
    pub server_graphics_updates: u64,
    pub completed_frames: u64,
    pub surface_updates: bool,
    pub nscodec: bool,
    pub graphics_codec: Option<rustrdp::GraphicsCodec>,
    pub udp_packets: u64,
    pub udp_connections: usize,
    pub frame_acknowledgements: bool,
    pub acknowledged_frames: u64,
    pub process_time: std::time::Duration,
    pub max_process_time: std::time::Duration,
    pub frames: u64,
    pub snapshot_time: std::time::Duration,
    pub max_snapshot_time: std::time::Duration,
    /// Optional counts of snapshot tiles checked outside the reported damage.
    pub snapshot_audit: Option<(u64, u64, u64)>,
    /// H.264 decode calls and calls that produced no picture.
    pub avc_decode_audit: Option<(u64, u64)>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CertificateDecision {
    Once,
    Save,
    Reject,
}

pub enum RdpEvent {
    Connecting,
    Connected,
    Disconnected,
    Failed(RdpError),
    Performance(RdpPerformance),
    /// A local request was rejected; the desktop connection remains usable.
    RequestRejected(RdpError),
    FileTransfer {
        offer_id: u64,
        files: usize,
        bytes_sent: u64,
        available: bool,
    },
    RemoteFilesAvailable {
        revision: u64,
        offer_id: u64,
        files: usize,
        total_bytes: u64,
    },
    RemoteFilesProgress {
        offer_id: u64,
        bytes_received: u64,
        total_bytes: u64,
    },
    RemoteFilesReady {
        offer_id: u64,
        directory: Arc<tempfile::TempDir>,
        paths: Vec<std::path::PathBuf>,
    },
    RemoteFilesFailed {
        offer_id: u64,
        error: RdpError,
    },
    RemoteFilesCancelled {
        offer_id: u64,
    },
    CertificateChallenge {
        host: String,
        port: u16,
        fingerprint: String,
        previous_fingerprint: Option<String>,
        reply: oneshot::Sender<CertificateDecision>,
    },
    /// Save only this exact fingerprint in the originating connection profile.
    CertificateAccepted {
        fingerprint: String,
        save: bool,
    },
    Clipboard {
        revision: u64,
        event: RdpClipboardEvent,
    },
}

#[derive(Debug)]
pub enum RdpClipboardEvent {
    Offer,
    Text(String),
    Content(rustrdp::ClipboardContent),
    Unavailable,
    Published,
    PasteSent,
    PasteCancelled,
    Rejected(RdpError),
}

#[derive(Debug)]
pub enum RdpClipboardAction {
    SendText(String),
    SendContent(rustrdp::ClipboardContent),
    PasteContent(rustrdp::ClipboardContent),
    /// Publish local text and paste after the server confirms its availability.
    PasteText(String),
    SendFiles(Vec<std::path::PathBuf>),
    PasteFiles(Vec<std::path::PathBuf>),
    CancelFiles,
    RequestText,
    Cancel,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RdpMouseButton {
    Left,
    Right,
    Middle,
    Back,
    Forward,
}

pub enum RdpCommand {
    ReceiveFiles {
        offer_id: u64,
    },
    CancelReceiveFiles {
        offer_id: u64,
    },
    SetAudioMuted(bool),
    SetMicrophoneEnabled(bool),
    SetLocked(bool),
    PasteRemote,
    /// Set-1 scan code; extended keys carry the 0xE000 or 0xE100 prefix.
    Key {
        scancode: u16,
        pressed: bool,
    },
    Unicode(String),
    Pointer {
        x: u16,
        y: u16,
    },
    MouseButton {
        button: RdpMouseButton,
        pressed: bool,
        x: u16,
        y: u16,
    },
    /// Windows wheel units (120 per notch), split into signed 9-bit protocol events.
    Wheel {
        horizontal: bool,
        delta: i16,
        x: u16,
        y: u16,
    },
    Resize {
        width: u16,
        height: u16,
    },
    Clipboard {
        revision: u64,
        action: RdpClipboardAction,
    },
    /// Release held keys/buttons on blur so they cannot remain down remotely.
    ReleaseAll,
}

impl RdpCommand {
    fn invalidates_paste(&self) -> bool {
        matches!(
            self,
            Self::ReleaseAll
                | Self::PasteRemote
                | Self::Key { pressed: true, .. }
                | Self::MouseButton { pressed: true, .. }
                | Self::Unicode(_)
                | Self::Wheel { .. }
                | Self::Resize { .. }
                | Self::Clipboard { .. }
        )
    }
}

struct QueuedCommand {
    command: RdpCommand,
    paste: Option<CancellationToken>,
}

struct Control {
    stop: CancellationToken,
    pending_paste: Mutex<Option<CancellationToken>>,
    audio_muted: Arc<AtomicBool>,
    microphone_enabled: Arc<AtomicBool>,
    locked: Arc<AtomicBool>,
    capture_transmission: Arc<Mutex<CancellationToken>>,
}
impl Drop for Control {
    fn drop(&mut self) {
        self.stop.cancel();
    }
}

#[derive(Clone)]
pub struct RdpSender {
    commands: commands::Sender,
    presented: watch::Sender<Option<rustrdp::FrameToken>>,
    control: Arc<Control>,
}
impl RdpSender {
    /// Reports a submitted desktop snapshot without competing with user input.
    /// Later receipts include earlier frames; only the latest feedback is needed.
    pub fn frame_presented(&self, token: rustrdp::FrameToken) {
        if !self.control.stop.is_cancelled() {
            self.presented.send_if_modified(|current| {
                if current.as_ref().is_none_or(|previous| token > *previous) {
                    *current = Some(token);
                    true
                } else {
                    false
                }
            });
        }
    }

    pub fn try_send(&self, command: RdpCommand) -> Result<()> {
        match &command {
            RdpCommand::Unicode(text) if text.len() > MAX_TEXT_BYTES => {
                return Err(RdpError::Limit("unicode input text"));
            }
            RdpCommand::Clipboard {
                action: RdpClipboardAction::SendText(text) | RdpClipboardAction::PasteText(text),
                ..
            } if text.len() > MAX_CLIPBOARD_BYTES => {
                return Err(RdpError::Limit("clipboard text"));
            }
            RdpCommand::Clipboard {
                action:
                    RdpClipboardAction::SendContent(content) | RdpClipboardAction::PasteContent(content),
                ..
            } if content.len() > MAX_CLIPBOARD_CONTENT_BYTES => {
                return Err(RdpError::Limit("clipboard content"));
            }
            RdpCommand::Clipboard {
                action: RdpClipboardAction::SendFiles(paths) | RdpClipboardAction::PasteFiles(paths),
                ..
            } if paths.is_empty()
                || paths.len() > rustrdp::MAX_CLIPBOARD_FILES
                || paths
                    .iter()
                    .map(|path| path.as_os_str().len())
                    .sum::<usize>()
                    > MAX_CLIPBOARD_BYTES =>
            {
                return Err(RdpError::FileSelection);
            }
            RdpCommand::Resize { width, height } => check_dimensions(*width, *height)?,
            RdpCommand::Key { scancode, .. } if !matches!(*scancode >> 8, 0 | 0xe0 | 0xe1) => {
                return Err(RdpError::InvalidConfig);
            }
            _ => {}
        }
        // The audio worker observes this immediately, including during connection
        // activation before the protocol loop starts consuming queued commands.
        if let RdpCommand::SetAudioMuted(muted) = &command {
            self.control.audio_muted.store(*muted, Ordering::Release);
        }
        if let RdpCommand::SetMicrophoneEnabled(enabled) = &command {
            self.control
                .microphone_enabled
                .store(*enabled, Ordering::Release);
        }
        if let RdpCommand::SetLocked(locked) = &command {
            self.control.locked.store(*locked, Ordering::Release);
        }
        if matches!(
            command,
            RdpCommand::SetMicrophoneEnabled(_) | RdpCommand::SetLocked(_)
        ) {
            let mut transmission = self
                .control
                .capture_transmission
                .lock()
                .unwrap_or_else(|error| error.into_inner());
            if self.control.locked.load(Ordering::Acquire)
                || !self.control.microphone_enabled.load(Ordering::Acquire)
            {
                transmission.cancel();
            } else if transmission.is_cancelled() {
                *transmission = self.control.stop.child_token();
            }
        }
        // Invalidate before enqueueing: a server ACK may win the worker's
        // select while focus loss or newer input is already in its command queue.
        let mut pending = self
            .control
            .pending_paste
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        if command.invalidates_paste()
            && let Some(previous) = pending.take()
        {
            previous.cancel();
        }
        let paste = matches!(
            command,
            RdpCommand::PasteRemote
                | RdpCommand::Clipboard {
                    action: RdpClipboardAction::PasteText(_)
                        | RdpClipboardAction::PasteContent(_)
                        | RdpClipboardAction::PasteFiles(_),
                    ..
                }
        )
        .then(|| {
            let token = self.control.stop.child_token();
            *pending = Some(token.clone());
            token
        });
        self.commands
            .try_send(QueuedCommand { command, paste }, pending.as_ref())
            .map_err(|error| {
                // The mailbox cancels paste before publishing its release barrier.
                pending.take();
                match error {
                    commands::SendError::Full => RdpError::InputBusy,
                    commands::SendError::Closed => RdpError::Disconnected,
                }
            })
    }
    pub fn close(&self) {
        self.control.stop.cancel();
    }
}

pub struct RdpHandle {
    pub events: mpsc::Receiver<RdpEvent>,
    pub frames: watch::Receiver<Option<Arc<RdpFrame>>>,
    sender: RdpSender,
}
impl RdpHandle {
    pub fn sender(&self) -> RdpSender {
        self.sender.clone()
    }
    pub fn try_send(&self, command: RdpCommand) -> Result<()> {
        self.sender.try_send(command)
    }
    pub fn close(&self) {
        self.sender.close();
    }
}

pub struct RdpClient;
impl RdpClient {
    pub fn connect(mut config: RdpConfig, password: Secret) -> Result<RdpHandle> {
        config.validate().map_err(|_| RdpError::InvalidConfig)?;
        config.host = crate::config::known_hosts::normalize_host(&config.host)
            .map_err(|_| RdpError::InvalidConfig)?;
        check_dimensions(config.width, config.height)?;
        static RUNTIME: OnceLock<std::result::Result<tokio::runtime::Runtime, std::io::Error>> =
            OnceLock::new();
        let runtime = RUNTIME
            .get_or_init(|| {
                tokio::runtime::Builder::new_multi_thread()
                    .worker_threads(1)
                    .thread_name("rustterm-rdp")
                    .enable_all()
                    .build()
            })
            .as_ref()
            .map_err(|_| RdpError::Runtime)?;
        let (commands_tx, commands) = commands::channel(COMMAND_CAPACITY, COMMAND_BYTE_LIMIT);
        let (events_tx, events) = mpsc::channel(16);
        let (frames_tx, frames) = watch::channel(None);
        let (presented_tx, presented) = watch::channel(None);
        let stop = CancellationToken::new();
        let audio_muted = Arc::new(AtomicBool::new(false));
        let microphone_enabled = Arc::new(AtomicBool::new(config.audio_input));
        let locked = Arc::new(AtomicBool::new(false));
        let capture_transmission = Arc::new(Mutex::new(stop.child_token()));
        let sender = RdpSender {
            commands: commands_tx,
            presented: presented_tx,
            control: Arc::new(Control {
                stop: stop.clone(),
                pending_paste: Mutex::new(None),
                audio_muted: audio_muted.clone(),
                microphone_enabled: microphone_enabled.clone(),
                locked: locked.clone(),
                capture_transmission: capture_transmission.clone(),
            }),
        };
        let redirection = protocol::RedirectionState {
            audio_muted,
            microphone_enabled,
            locked,
            capture_transmission,
        };
        runtime.spawn(async move {
            let _ = events_tx.send(RdpEvent::Connecting).await;
            let result = tokio::select! {
                biased;
                _ = stop.cancelled() => Ok(()),
                result = protocol::run(config, password, commands, &events_tx, frames_tx, presented, redirection) => result,
            };
            // Cancellation drops the socket even during TLS/NLA, writes, or a trust prompt.
            let event = match result {
                Ok(()) => RdpEvent::Disconnected,
                Err(error) => RdpEvent::Failed(error),
            };
            let _ = events_tx.send(event).await;
        });
        Ok(RdpHandle {
            events,
            frames,
            sender,
        })
    }
}

/// Verifies TLS and NLA without joining channels or activating a remote desktop.
/// Certificate decisions use the same trust gate as an interactive connection.
pub(crate) async fn probe(
    mut config: RdpConfig,
    password: Secret,
    events: &mpsc::Sender<RdpEvent>,
) -> Result<()> {
    config.validate().map_err(|_| RdpError::InvalidConfig)?;
    config.host = crate::config::known_hosts::normalize_host(&config.host)
        .map_err(|_| RdpError::InvalidConfig)?;
    protocol::probe(config, password, events).await
}

fn check_dimensions(width: u16, height: u16) -> Result<()> {
    if width < 320
        || height < 200
        || width > 8192
        || height > 8192
        || usize::from(width) * usize::from(height) > MAX_FRAME_PIXELS
    {
        Err(RdpError::Limit("desktop dimensions"))
    } else {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn protocol_errors_preserve_static_context_for_display_and_diagnostics() {
        let error = RdpError::from(rustrdp::Error::Invalid("graphics surface format"));
        assert_eq!(error, RdpError::Protocol("graphics surface format"));
        assert_eq!(error.diagnostic_code(), "rdp-error-protocol");
        assert_eq!(error.diagnostic_context(), Some("graphics surface format"));
        assert!(error.to_string().ends_with(": graphics surface format"));
        assert_eq!(
            RdpError::from(rustrdp::Error::Truncated),
            RdpError::Protocol("truncated protocol message")
        );
        assert_eq!(RdpError::Authentication.diagnostic_context(), None);
        assert_eq!(
            RdpError::Unsupported("message channel PDU").diagnostic_context(),
            Some("message channel PDU")
        );
        let error = RdpError::from(rustrdp::Error::Limit("graphics surfaces"));
        assert_eq!(error, RdpError::Limit("graphics surfaces"));
        assert_eq!(error.diagnostic_code(), "rdp-error-limit");
        assert_eq!(error.diagnostic_context(), Some("graphics surfaces"));
        assert!(error.to_string().ends_with(": graphics surfaces"));
    }

    fn sender() -> (RdpSender, commands::Receiver) {
        let (commands, receive) = commands::channel(4, COMMAND_BYTE_LIMIT);
        (
            RdpSender {
                commands,
                presented: watch::channel(None).0,
                control: Arc::new(Control {
                    stop: CancellationToken::new(),
                    pending_paste: Mutex::new(None),
                    audio_muted: Arc::new(AtomicBool::new(false)),
                    microphone_enabled: Arc::new(AtomicBool::new(false)),
                    locked: Arc::new(AtomicBool::new(false)),
                    capture_transmission: Arc::new(Mutex::new(CancellationToken::new())),
                }),
            },
            receive,
        )
    }

    #[test]
    fn muting_audio_takes_effect_before_the_protocol_dequeues_commands() {
        let (sender, mut commands) = sender();
        sender.try_send(RdpCommand::SetAudioMuted(true)).unwrap();
        assert!(sender.control.audio_muted.load(Ordering::Acquire));
        sender.try_send(RdpCommand::SetAudioMuted(false)).unwrap();
        assert!(!sender.control.audio_muted.load(Ordering::Acquire));
        let _old_mute = commands.try_recv().unwrap();
        assert!(!sender.control.audio_muted.load(Ordering::Acquire));
    }

    #[test]
    fn microphone_lock_cancels_queued_audio_without_reusing_tokens_on_unlock() {
        let (sender, mut commands) = sender();
        sender
            .try_send(RdpCommand::SetMicrophoneEnabled(true))
            .unwrap();
        let initial = sender.control.capture_transmission.lock().unwrap().clone();
        sender.try_send(RdpCommand::SetLocked(true)).unwrap();
        assert!(initial.is_cancelled());
        sender.try_send(RdpCommand::SetLocked(false)).unwrap();
        let resumed = sender.control.capture_transmission.lock().unwrap().clone();
        assert!(!resumed.is_cancelled());
        assert!(initial.is_cancelled());
        sender
            .try_send(RdpCommand::SetMicrophoneEnabled(false))
            .unwrap();
        assert!(resumed.is_cancelled());
        assert!(!sender.control.stop.is_cancelled());
        while commands.try_recv().is_ok() {}
    }

    #[test]
    fn newer_input_cancels_paste_before_either_command_is_dequeued() {
        for command in [
            RdpCommand::ReleaseAll,
            RdpCommand::Key {
                scancode: 0x0f,
                pressed: true,
            },
            RdpCommand::MouseButton {
                button: RdpMouseButton::Left,
                pressed: true,
                x: 1,
                y: 1,
            },
            RdpCommand::Unicode("new input".into()),
            RdpCommand::Wheel {
                horizontal: false,
                delta: 120,
                x: 1,
                y: 1,
            },
            RdpCommand::Resize {
                width: 800,
                height: 600,
            },
            RdpCommand::Clipboard {
                revision: 2,
                action: RdpClipboardAction::SendText("new copy".into()),
            },
            RdpCommand::Clipboard {
                revision: 2,
                action: RdpClipboardAction::RequestText,
            },
        ] {
            let (sender, mut commands) = sender();
            sender
                .try_send(RdpCommand::Clipboard {
                    revision: 1,
                    action: RdpClipboardAction::PasteText("old copy".into()),
                })
                .unwrap();
            sender.try_send(command).unwrap();
            // Cancellation cannot depend on which ready branch select! chooses.
            let pending = commands.try_recv().unwrap().paste.unwrap();
            assert!(pending.is_cancelled());
            assert!(!commands.is_empty());
            assert!(!sender.control.stop.is_cancelled());
        }
    }

    #[test]
    fn releasing_keys_and_moving_pointer_preserve_a_paste_waiting_for_ack() {
        let (sender, mut commands) = sender();
        sender
            .try_send(RdpCommand::Clipboard {
                revision: 1,
                action: RdpClipboardAction::PasteText("copy".into()),
            })
            .unwrap();
        let pending = commands.try_recv().unwrap().paste.unwrap();
        for command in [
            RdpCommand::Key {
                scancode: 0x1d,
                pressed: false,
            },
            RdpCommand::Pointer { x: 1, y: 1 },
            RdpCommand::MouseButton {
                button: RdpMouseButton::Left,
                pressed: false,
                x: 1,
                y: 1,
            },
        ] {
            sender.try_send(command).unwrap();
            assert!(!pending.is_cancelled());
        }
        assert_eq!(
            sender.try_send(RdpCommand::Resize {
                width: 1,
                height: 1
            }),
            Err(RdpError::Limit("desktop dimensions"))
        );
        assert!(!pending.is_cancelled());
        sender.try_send(RdpCommand::ReleaseAll).unwrap();
        assert!(pending.is_cancelled());
        assert_eq!(commands.len(), 4);
    }

    #[test]
    fn old_revision_barrier_cancels_paste_before_clipboard_read_finishes() {
        let (sender, mut commands) = sender();
        sender
            .try_send(RdpCommand::Clipboard {
                revision: 7,
                action: RdpClipboardAction::PasteText("previous".into()),
            })
            .unwrap();
        // The UI has started revision 8, but its synchronous clipboard read
        // has not finished. Only the previous revision reaches the worker.
        sender
            .try_send(RdpCommand::Clipboard {
                revision: 7,
                action: RdpClipboardAction::Cancel,
            })
            .unwrap();
        let previous = commands.try_recv().unwrap().paste.unwrap();
        assert!(previous.is_cancelled());
        let barrier = commands.try_recv().unwrap();
        assert!(matches!(
            barrier.command,
            RdpCommand::Clipboard {
                revision: 7,
                action: RdpClipboardAction::Cancel,
            }
        ));
        assert!(barrier.paste.is_none());
        sender
            .try_send(RdpCommand::Clipboard {
                revision: 8,
                action: RdpClipboardAction::PasteText("new text".into()),
            })
            .unwrap();
        assert!(!commands.try_recv().unwrap().paste.unwrap().is_cancelled());
    }

    #[test]
    fn newest_paste_owns_its_intent_until_connection_closes() {
        let (sender, mut commands) = sender();
        sender
            .try_send(RdpCommand::Clipboard {
                revision: 1,
                action: RdpClipboardAction::PasteText("first".into()),
            })
            .unwrap();
        let previous = commands.try_recv().unwrap().paste.unwrap();
        sender
            .try_send(RdpCommand::Clipboard {
                revision: 2,
                action: RdpClipboardAction::PasteText("second".into()),
            })
            .unwrap();
        let latest = commands.try_recv().unwrap().paste.unwrap();
        assert!(previous.is_cancelled());
        assert!(!latest.is_cancelled());
        drop(sender.clone());
        assert!(!latest.is_cancelled());
        drop(sender);
        assert!(latest.is_cancelled());
    }

    #[test]
    fn file_and_remote_paste_are_cancelled_before_delivery_on_blur() {
        for command in [
            RdpCommand::Clipboard {
                revision: 1,
                action: RdpClipboardAction::PasteFiles(vec!["/tmp/example.txt".into()]),
            },
            RdpCommand::PasteRemote,
        ] {
            let (sender, mut commands) = sender();
            sender.try_send(command).unwrap();
            let pending = commands.try_recv().unwrap().paste.unwrap();
            assert!(!pending.is_cancelled());
            sender.try_send(RdpCommand::ReleaseAll).unwrap();
            assert!(pending.is_cancelled());
            assert!(!sender.control.stop.is_cancelled());
        }
    }

    #[test]
    fn input_overflow_cancels_paste_and_releases_without_disconnect() {
        let (sender, mut commands) = sender();
        sender.try_send(RdpCommand::PasteRemote).unwrap();
        let pending = commands.try_recv().unwrap().paste.unwrap();
        for scancode in 1..=4 {
            sender
                .try_send(RdpCommand::Key {
                    scancode,
                    pressed: false,
                })
                .unwrap();
        }
        assert!(!pending.is_cancelled());
        assert_eq!(
            sender.try_send(RdpCommand::Pointer { x: 1, y: 1 }),
            Err(RdpError::InputBusy)
        );
        assert!(pending.is_cancelled());
        assert!(!sender.control.stop.is_cancelled());
        sender.try_send(RdpCommand::ReleaseAll).unwrap();
        for scancode in 1..=4 {
            assert!(matches!(
                commands.try_recv().unwrap().command,
                RdpCommand::Key { scancode: actual, pressed: false } if actual == scancode
            ));
        }
        assert!(matches!(
            commands.try_recv().unwrap().command,
            RdpCommand::ReleaseAll
        ));
        sender
            .try_send(RdpCommand::Key {
                scancode: 1,
                pressed: true,
            })
            .unwrap();
        assert!(!sender.control.stop.is_cancelled());
    }
}
