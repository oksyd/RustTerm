//! An RDP client protocol engine with caller-owned transport, authentication and UI.
//!
//! The caller negotiates NLA, verifies the TLS peer and completes CredSSP before
//! starting [`Connector`]. This crate never opens a socket, accesses a clipboard,
//! starts a thread or accepts a server certificate on the caller's behalf.
//!
//! # Driving a connection
//!
//! Use [`connection::negotiation_request`] and
//! [`connection::parse_negotiation_response`] before TLS, then authenticate the
//! peer and complete CredSSP on the same transport. Create a [`Connector`] with
//! the selected [`SecurityProtocol`], send [`Connector::start`], and feed each
//! complete decrypted frame to [`Connector::advance`]. Process every returned
//! packet and event in order. [`HandshakeOutput::session`] transfers the active
//! session, including graphics and compression history received during setup.
//!
//! A transport can use [`wire::frame_size`] to separate partial or coalesced
//! frames. The caller supplies receive/send queue bounds, timeouts, cancellation
//! and reconnection. Never retry a malformed packet on the same state machine.
//!
//! ```
//! use rustrdp::{ClientConfig, Connector, SecurityProtocol};
//!
//! let config = ClientConfig {
//!     username: "alice".into(),
//!     ..ClientConfig::default()
//! };
//! // The transport has already verified TLS and completed CredSSP.
//! let mut connector = Connector::new(config, SecurityProtocol::Hybrid)?;
//! let first_packet = connector.start()?;
//! assert!(!first_packet.is_empty());
//! // Write first_packet to that authenticated transport, then call advance().
//! # Ok::<(), rustrdp::Error>(())
//! ```
//!
//! # Features
//!
//! Default builds use only the standard library. The optional `licensing`
//! feature adds CAL exchanges backed by RustCrypto; the caller still supplies
//! stable client identity, fresh cryptographic randomness and license storage.
//! Configure the exchange with `Connector::with_licensing`; the Cargo feature
//! alone does not enable it for a connection.
//! MPPC receive decompression and lossy planar decoding require no dependencies
//! and are enabled through [`ClientConfig`], independently of Cargo features.
//!
//! # Protocol scope
//!
//! Supports bitmap desktops, keyboard/mouse input, clipboard, audio, monitor
//! layouts and drive-redirection messages. Device I/O belongs to the caller.
//! The optional `graphics` feature adds the graphics pipeline with RemoteFX,
//! ClearCodec and H.264 AVC420/AVC444. Enable it explicitly through
//! [`ClientConfig::graphics_pipeline`] and acknowledge presented frames.
//! GDI drawing orders, Gateway and RemoteApp are unsupported. Optional codecs
//! and CAL exchanges still need broader real-server interoperability validation.
#![forbid(unsafe_code)]

mod channels;
mod clipboard;
pub mod compression;
pub mod connection;
mod drive;
#[cfg(feature = "graphics")]
mod gfx;
pub mod graphics;
mod input;
mod monitor;
pub mod multitransport;
mod session;
pub mod udp;
pub mod wire;

pub use channels::audio::{AudioEvent, AudioFormat, AudioSamples, AudioToken};
pub use channels::audio_input::{CaptureEvent, CaptureRequest};
pub use clipboard::{
    ClipboardContent, ClipboardFile, ClipboardFileRequest, MAX_CLIPBOARD_FILE_CHUNK,
    MAX_CLIPBOARD_FILES,
};
pub use compression::BulkCompression;
pub use connection::{Connector, HandshakeOutput, NegotiationResponse, SecurityProtocol};
pub use drive::{DriveCompletion, DriveRequest};
pub use input::{Input, MouseButton};
pub use monitor::Monitor;
pub use session::{Event, FrameToken, GraphicsFrame, Session};

/// Result returned by protocol parsing, configuration and local operations.
pub type Result<T> = std::result::Result<T, Error>;

/// The most recently decoded bitmap encoding on the graphics pipeline.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum GraphicsCodec {
    /// Uncompressed BGRA pixels.
    Raw,
    /// RemoteFX tile transform coding.
    RemoteFx,
    /// ClearCodec screen-content coding.
    Clear,
    /// RemoteFX progressive tile refinement.
    Progressive,
    /// RDP planar bitmap compression.
    Planar,
    /// H.264 with 4:2:0 chroma subsampling.
    Avc420,
    /// RDP's two-stream H.264 4:4:4 reconstruction.
    Avc444,
    /// The second RDP H.264 4:4:4 chroma layout.
    Avc444V2,
    /// Alpha-only update.
    Alpha,
}

/// Errors contain field names and protocol status codes, never received payloads.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Error {
    /// The complete frame or structure supplied by the caller is incomplete.
    Truncated,
    /// A configuration value, protocol field or state transition is invalid.
    Invalid(&'static str),
    /// A requested or received capability is unsupported or was not negotiated.
    Unsupported(&'static str),
    /// An operation would exceed a configured or protocol resource bound.
    Limit(&'static str),
    /// A channel's outbound queue is full; the session must be discarded.
    Backpressure,
    /// A server supplied a failure status code; its namespace depends on phase.
    Server(u32),
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Truncated => f.write_str("truncated RDP message"),
            Self::Invalid(field) => write!(f, "invalid RDP {field}"),
            Self::Unsupported(feature) => write!(f, "unsupported RDP {feature}"),
            Self::Limit(resource) => write!(f, "RDP {resource} exceeds the configured limit"),
            Self::Backpressure => f.write_str("RDP outbound queue is full"),
            Self::Server(code) => write!(f, "RDP server error 0x{code:08x}"),
        }
    }
}
impl std::error::Error for Error {}

/// Allocation limits are checked before expanding untrusted data.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Limits {
    /// Largest accepted desktop width in pixels; default 3840.
    pub max_width: u16,
    /// Largest accepted desktop height in pixels; default 2160.
    pub max_height: u16,
    /// Maximum received frame and reassembled update size; default 4 MiB.
    pub max_pdu: usize,
    /// Maximum decoded BGRA framebuffer/update size; default 3840 × 2160 × 4.
    pub max_bitmap_bytes: usize,
    /// Maximum reassembled static or dynamic channel message; default 4 MiB.
    pub max_channel_message: usize,
    /// Maximum clipboard text size in UTF-8 bytes; default 1 MiB.
    pub max_clipboard_bytes: usize,
    /// Maximum pointer width and height in pixels; default 384.
    pub max_pointer_size: u16,
    /// Number of pointer cache entries, from 1 through 256; default 64.
    pub max_pointer_cache: usize,
    /// Maximum negotiated static channels, at most 31; default 8.
    pub max_static_channels: usize,
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            max_width: 3840,
            max_height: 2160,
            max_pdu: 4 * 1024 * 1024,
            max_bitmap_bytes: 3840 * 2160 * 4,
            max_channel_message: 4 * 1024 * 1024,
            max_clipboard_bytes: 1024 * 1024,
            max_pointer_size: 384,
            max_pointer_cache: 64,
            max_static_channels: 8,
        }
    }
}

impl Limits {
    /// Compute a checked BGRA byte count for a nonempty desktop within limits.
    ///
    /// Returns [`Error::Limit`] for invalid dimensions, multiplication overflow
    /// or a size exceeding [`Self::max_bitmap_bytes`]. No allocation is made.
    pub fn frame_bytes(&self, width: u16, height: u16) -> Result<usize> {
        if width == 0 || height == 0 || width > self.max_width || height > self.max_height {
            return Err(Error::Limit("desktop dimensions"));
        }
        usize::from(width)
            .checked_mul(usize::from(height))
            .and_then(|n| n.checked_mul(4))
            .filter(|n| *n <= self.max_bitmap_bytes)
            .ok_or(Error::Limit("framebuffer"))
    }
}

/// Preferred desktop color depth requested during connection negotiation.
///
/// The server may select another supported depth. Read
/// [`ConnectionInfo::color_depth`] to inspect the negotiated result.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
#[repr(u16)]
pub enum ColorDepth {
    /// RGB565 color, using 16 bits per pixel.
    Bpp16 = 16,
    /// Full RGB color, using 32 bits per pixel.
    #[default]
    Bpp32 = 32,
}

impl ColorDepth {
    /// Number of bits per pixel requested by this preference.
    pub const fn bits_per_pixel(self) -> u16 {
        self as u16
    }
}

/// Protocol preferences. Passwords and TLS policy belong to the transport caller.
#[derive(Clone, Debug)]
pub struct ClientConfig {
    /// Username already authenticated by CredSSP; up to 255 UTF-16 code units.
    pub username: String,
    /// Optional Windows domain, represented by an empty string when absent.
    pub domain: String,
    /// Nonempty client computer name, up to 15 UTF-16 code units.
    pub client_name: String,
    /// Requested initial desktop width in pixels; default 1280.
    pub width: u16,
    /// Requested initial desktop height in pixels; default 720.
    pub height: u16,
    /// Preferred session color depth; default 32 bits per pixel.
    pub color_depth: ColorDepth,
    /// Requested single-monitor desktop scaling percentage, between 100 and 500.
    /// Explicit [`Monitor::scale`] values take precedence for monitor layouts.
    /// Servers may ignore this preference; it is retained during [`Session::resize`].
    pub desktop_scale: u32,
    /// Request text, image and rich-text clipboard redirection; default true.
    pub clipboard: bool,
    /// Enable file clipboard transfers in both directions; requires `clipboard`. Default false.
    pub clipboard_files: bool,
    /// Request audio playback and its control channel; default false.
    /// PCM and G.711 are always supported; `audio-codecs` adds AAC and ADPCM.
    /// Decoded samples exposed to the caller are always 16-bit PCM.
    pub audio_output: bool,
    /// Initial multi-monitor topology; empty uses the width/height single monitor.
    pub monitors: Vec<Monitor>,
    /// Permit microphone capture requests; default false.
    pub audio_input: bool,
    /// Explicitly shared device names. The caller supplies all filesystem access.
    pub shared_drives: Vec<String>,
    /// Request the Display Control channel; default true.
    pub dynamic_resolution: bool,
    /// Enable the RDP Graphics Pipeline and its video codecs. Requires the
    /// `graphics` feature, lossy bitmaps, and presentation-driven acknowledgements.
    /// Disabled by default.
    pub graphics_pipeline: bool,
    /// Offer additional reliable and lossy transports. Disabled by default.
    pub multitransport: bool,
    /// Maximum server-to-client bulk compression package. Disabled by default.
    pub bulk_compression: Option<BulkCompression>,
    /// Allow bitmap color fidelity loss and chroma subsampling; default false.
    pub allow_lossy_bitmaps: bool,
    /// Opt into presentation-driven frame acknowledgements. Disabled by default.
    ///
    /// The value is a nonzero in-flight frame hint, at most 64. Enable this only
    /// when the caller retains frame tokens and calls [`Session::acknowledge_frame`]
    /// after consuming frames, including when displays are hidden or discarded.
    /// Legacy surface updates also require server acknowledgement and frame-marker
    /// capabilities. The graphics pipeline negotiates its acknowledgements
    /// independently; [`GraphicsFrame::acknowledgement`] identifies frames to confirm.
    pub frame_acknowledgements: Option<std::num::NonZeroU32>,
    /// Bounds applied before accepting or expanding peer-controlled data.
    pub limits: Limits,
}

impl Default for ClientConfig {
    fn default() -> Self {
        Self {
            username: String::new(),
            domain: String::new(),
            client_name: "RustRDP".into(),
            width: 1280,
            height: 720,
            color_depth: ColorDepth::default(),
            desktop_scale: 100,
            clipboard: true,
            clipboard_files: false,
            audio_output: false,
            dynamic_resolution: true,
            graphics_pipeline: false,
            multitransport: false,
            monitors: Vec::new(),
            audio_input: false,
            shared_drives: Vec::new(),
            bulk_compression: None,
            allow_lossy_bitmaps: false,
            frame_acknowledgements: None,
            limits: Limits::default(),
        }
    }
}

impl ClientConfig {
    pub(crate) fn primary_scale(&self) -> u32 {
        self.monitors
            .iter()
            .find(|monitor| monitor.primary)
            .map_or(self.desktop_scale, |monitor| monitor.scale)
    }

    /// Validate names, initial desktop allocation and related resource bounds.
    ///
    /// Names cannot contain control characters. This performs no I/O and does
    /// not validate server identity, credentials or transport policy.
    /// [`Connector::new`] always performs this validation as well.
    pub fn validate(&self) -> Result<()> {
        self.limits.frame_bytes(self.width, self.height)?;
        if self.graphics_pipeline {
            if !cfg!(feature = "graphics") {
                return Err(Error::Unsupported("graphics feature is disabled"));
            }
            if !self.allow_lossy_bitmaps || self.frame_acknowledgements.is_none() {
                return Err(Error::Invalid(
                    "graphics pipeline requires lossy bitmaps and frame acknowledgements",
                ));
            }
        }
        if self
            .frame_acknowledgements
            .is_some_and(|window| window.get() > 64)
        {
            return Err(Error::Invalid("frame acknowledgement window"));
        }
        if !(100..=500).contains(&self.desktop_scale) {
            return Err(Error::Invalid("desktop scale"));
        }
        if !self.monitors.is_empty()
            && monitor::validate(&self.monitors, self.limits)? != (self.width, self.height)
        {
            return Err(Error::Invalid("initial monitor desktop dimensions"));
        }
        if self.shared_drives.len() > 16
            || self.shared_drives.iter().enumerate().any(|(i, name)| {
                name.is_empty()
                    || name.len() > 7
                    || !name.bytes().all(|c| c.is_ascii_alphanumeric() || c == b'_')
                    || self.shared_drives[..i]
                        .iter()
                        .any(|previous| previous.eq_ignore_ascii_case(name))
            })
        {
            return Err(Error::Invalid("shared drive names"));
        }
        for (value, limit, name) in [
            (&self.username, 255, "username"),
            (&self.domain, 255, "domain"),
            (&self.client_name, 15, "client name"),
        ] {
            if value.encode_utf16().count() > limit || value.chars().any(char::is_control) {
                return Err(Error::Invalid(name));
            }
        }
        if self.client_name.is_empty()
            || self.limits.max_pdu < 7
            || self.limits.max_pointer_cache == 0
            || self.limits.max_pointer_cache > 256
            || self.limits.max_static_channels > 31
            || self.limits.max_channel_message < 1600
            || self.limits.max_clipboard_bytes > self.limits.max_channel_message / 2 - 8
        {
            return Err(Error::Invalid("client limits"));
        }
        Ok(())
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Channel {
    pub id: u16,
    pub name: String,
}

/// Negotiated desktop properties, available through [`Session::info`].
///
/// The session owns this state and updates it during server reactivation.
/// Protocol identifiers and channel bookkeeping are intentionally internal.
#[derive(Clone, Debug)]
pub struct ConnectionInfo {
    pub(crate) user_id: u16,
    pub(crate) server_id: u16,
    pub(crate) io_channel_id: u16,
    pub(crate) share_id: u32,
    pub(crate) width: u16,
    pub(crate) height: u16,
    pub(crate) color_depth: u16,
    pub(crate) channels: Vec<Channel>,
    pub(crate) fastpath_input: bool,
    pub(crate) unicode_input: bool,
    pub(crate) mouse_extended: bool,
    pub(crate) horizontal_wheel: bool,
    pub(crate) vc_chunk_size: usize,
    pub(crate) surface_commands: u32,
    pub(crate) nscodec: bool,
    pub(crate) frame_acknowledgements: bool,
    pub(crate) message_channel_id: Option<u16>,
    pub(crate) multitransport_flags: u32,
}

impl ConnectionInfo {
    /// Current remote desktop width in pixels.
    pub fn width(&self) -> u16 {
        self.width
    }

    /// Current remote desktop height in pixels.
    pub fn height(&self) -> u16 {
        self.height
    }

    /// Server-negotiated desktop depth in bits per pixel.
    ///
    /// This may differ from [`ClientConfig::color_depth`] and is updated during
    /// reactivation. The exposed framebuffer is always packed BGRA8.
    pub fn color_depth(&self) -> u16 {
        self.color_depth
    }

    /// Whether the peer accepts fast-path input instead of slow-path input.
    pub fn fastpath_input(&self) -> bool {
        self.fastpath_input
    }

    /// Whether [`Input::Unicode`] text can be submitted.
    pub fn unicode_input(&self) -> bool {
        self.unicode_input
    }

    /// Whether the peer accepts the back and forward mouse buttons.
    pub fn mouse_extended(&self) -> bool {
        self.mouse_extended
    }

    /// Whether the peer accepts horizontal wheel input.
    pub fn horizontal_wheel(&self) -> bool {
        self.horizontal_wheel
    }

    /// Whether surface bitmap updates were negotiated, in addition to legacy updates.
    pub fn surface_updates(&self) -> bool {
        self.surface_commands & 0x42 != 0
    }

    /// Whether NSCodec was negotiated. The server can still use other bitmap formats.
    pub fn nscodec(&self) -> bool {
        self.nscodec
    }

    /// Whether legacy surface-update frame acknowledgements were negotiated.
    ///
    /// The graphics pipeline negotiates acknowledgements independently. Check
    /// [`GraphicsFrame::acknowledgement`] for each frame's presentation receipt.
    pub fn frame_acknowledgements(&self) -> bool {
        self.frame_acknowledgements
    }
}
