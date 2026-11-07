//! Active connection state, Share PDUs and fast-path update reassembly.
use std::borrow::Cow;

use crate::{
    AudioEvent, AudioToken, ClientConfig, ClipboardContent, ClipboardFile, ClipboardFileRequest,
    ConnectionInfo, Error, Input, Result,
    channels::{ChannelEvent, Channels},
    compression::{BulkCompression, BulkDecoder},
    connection::{self, ActivationFinalizer},
    graphics::{Damage, FrameMarker, Graphics, PointerKind},
    input::InputState,
    wire::{self, FrameKind, Reader},
};

mod frames;
#[cfg(feature = "graphics")]
mod gfx;
mod multitransport;

use frames::Acknowledgements;
pub use frames::FrameToken;

pub(crate) type FrameCallback<'a> = dyn FnMut(GraphicsFrame<'_>) + 'a;

/// A complete server frame borrowed at its END marker, before later updates.
pub struct GraphicsFrame<'a> {
    /// Server-assigned frame identifier, which may be reused after reactivation.
    pub id: u32,
    /// Negotiated desktop color depth. Framebuffer pixels are always BGRA8.
    pub bits_per_pixel: u16,
    /// Complete desktop pixels, valid for the duration of the callback.
    pub graphics: &'a Graphics,
    /// Changes since the previous frame callback or explicit damage retrieval.
    pub damage: Option<Damage>,
    /// Receipt to retain until the frame is displayed or deliberately discarded.
    /// Absent unless frame acknowledgements were negotiated.
    pub acknowledgement: Option<FrameToken>,
}

/// Ordered output from a session operation.
///
/// Send protocol packets in order. Render the session's latest framebuffer when
/// graphics change; the caller decides how to present clipboard text or errors.
#[derive(Debug, PartialEq, Eq)]
pub enum Event {
    /// RemoteApp launch status or a remote window lifecycle change.
    RemoteApp(crate::remote_app::RemoteAppEvent),
    /// Write this complete packet to the existing authenticated transport.
    Send(Vec<u8>),
    /// Establish an authenticated additional transport without blocking TCP.
    MultitransportRequest(crate::multitransport::Request),
    /// Write and flush this synchronization response before tunnel payloads.
    MultitransportSynchronized {
        /// Complete TCP packets, in transmission order.
        packets: Vec<Vec<u8>>,
    },
    /// Send a complete dynamic-channel message through the indicated tunnel.
    ///
    /// Preserve the order of the payloads and never cancel a partial batch.
    SendTunnel {
        /// Delivery semantics of the established transport.
        protocol: crate::multitransport::TransportProtocol,
        /// DVC payloads, already fragmented to the transport budget.
        payloads: Vec<Vec<u8>>,
    },
    /// Framebuffer pixels or pointer visibility changed; schedule a redraw.
    GraphicsChanged,
    /// The server started a logical graphics frame; defer presentation.
    GraphicsFrameBegin {
        /// Server-assigned frame identifier.
        frame_id: u32,
    },
    /// The server completed a logical graphics frame.
    ///
    /// Check [`Session::graphics_frame_in_progress`] after processing all events:
    /// the same packet may already have started another frame.
    GraphicsFrameEnd {
        /// Server-assigned frame identifier.
        frame_id: u32,
        /// Receipt for presentation-driven acknowledgement, when negotiated.
        acknowledgement: Option<FrameToken>,
    },
    /// The peer advertised new clipboard formats, before invalidation and data.
    ClipboardOffer,
    /// Remote Unicode clipboard text received from an advertised format.
    ClipboardText(String),
    /// A complete remote clipboard offer containing multiple representations.
    ClipboardContent(ClipboardContent),
    /// Previously received remote clipboard text is no longer current.
    ClipboardUnavailable,
    /// The latest local text offer was accepted by the remote clipboard.
    ///
    /// It is now safe to send a paste input gesture. Check
    /// [`Session::clipboard_ready`] before acting if later events in the same
    /// operation may have revoked that offer.
    ClipboardPublished,
    /// The latest local text offer failed or remote ownership superseded it.
    /// Cancel a pending paste gesture; the connection remains usable.
    ClipboardPublishRejected,
    /// Read a bounded range from a file in the identified local offer.
    ClipboardFileRequest(ClipboardFileRequest),
    /// No peer lock or pending request refers to this local offer anymore.
    ClipboardFilesReleased {
        /// Caller-supplied identifier passed to `set_clipboard_files`.
        offer_id: u64,
    },
    /// Validated regular files in the remote clipboard. No file bytes have been requested.
    ClipboardFiles {
        /// Session-local generation of this remote offer.
        offer_id: u64,
        /// Basenames and exact sizes, in the server's descriptor order.
        files: Vec<ClipboardFile>,
    },
    /// Response to a bounded remote file request. `None` indicates failure.
    ClipboardFileContents {
        /// Correlated request, including the remote offer and byte range.
        request: ClipboardFileRequest,
        /// Exactly the requested range, or failure.
        data: Option<Vec<u8>>,
    },
    /// A remote file snapshot was superseded or explicitly released.
    ClipboardRemoteFilesCancelled {
        /// Identifier of the revoked remote offer.
        offer_id: u64,
    },
    /// PCM playback data or audio stream control for the caller's audio device.
    Audio(AudioEvent),
    /// A microphone request; capture starts only after caller authorization.
    Capture(crate::CaptureEvent),
    /// An I/O request against an explicitly announced shared drive.
    DriveRequest(crate::DriveRequest),
    /// A queued local request could not be satisfied; the connection remains open.
    RequestRejected(Error),
    /// Server activation completed with these actual desktop dimensions.
    Resized {
        /// Remote desktop width in pixels.
        width: u16,
        /// Remote desktop height in pixels.
        height: u16,
    },
    /// The server ended the MCS connection; close and discard the transport.
    Disconnected {
        /// T.125 Disconnect Provider Ultimatum reason code, from 0 through 4.
        reason: u8,
    },
}

enum State {
    Active,
    Deactivated,
    Activating(ActivationFinalizer),
    Closed,
}

struct Fragment {
    code: u8,
    bytes: Vec<u8>,
}

/// A single authenticated desktop. Feed complete framed packets in wire order.
///
/// Any protocol error is terminal: close the transport and discard the session.
/// Outbound packets must also be written in order. Rendering and clipboard effects
/// belong to the caller; this engine does not interact with the operating system.
/// Obtain a session from [`crate::HandshakeOutput::session`]; it cannot be created
/// independently of the connector's negotiated and initialized state.
pub struct Session {
    config: ClientConfig,
    info: ConnectionInfo,
    state: State,
    graphics: Graphics,
    input: InputState,
    channels: Channels,
    remote_windows: crate::remote_app::Windows,
    bulk: Option<(BulkCompression, BulkDecoder)>,
    fragment: Option<Fragment>,
    frame_acknowledgements: Acknowledgements,
    pub(crate) multitransport: crate::multitransport::State,
    pub(crate) message_channel: connection::MessageChannel,
    #[cfg(feature = "graphics")]
    gfx: Option<crate::gfx::Gfx>,
}

impl Session {
    /// Inspect a frame from the authenticated primary transport for a handoff.
    /// A returned payload contains credentials: do not log it, and discard this
    /// session and its transport before following the redirection.
    pub fn server_redirection<'a>(&self, packet: &'a [u8]) -> Result<Option<&'a [u8]>> {
        connection::redirection_payload(packet, self.info.io_channel_id, Some(self.info.server_id))
    }
    pub(crate) fn new(config: ClientConfig, info: ConnectionInfo) -> Result<Self> {
        config.validate()?;
        if info.user_id < 1001
            || info.io_channel_id < 1001
            || info.server_id < 1001
            || info.user_id == info.io_channel_id
            || info.channels.iter().any(|channel| {
                channel.id < 1001 || channel.id == info.io_channel_id || channel.id == info.user_id
            })
        {
            return Err(Error::Invalid("session channel IDs"));
        }
        Ok(Self {
            frame_acknowledgements: Acknowledgements::new(info.frame_acknowledgements),
            multitransport: crate::multitransport::State::default(),
            message_channel: connection::MessageChannel::default(),
            graphics: Graphics::new(info.width, info.height, config.limits)?,
            channels: Channels::new(&info.channels, &config, info.vc_chunk_size)?,
            remote_windows: crate::remote_app::Windows::default(),
            bulk: config
                .bulk_compression
                .map(|maximum| (maximum, BulkDecoder::new(config.limits.max_pdu))),
            config,
            info,
            state: State::Active,
            input: InputState::default(),
            fragment: None,
            #[cfg(feature = "graphics")]
            gfx: None,
        })
    }

    pub(crate) fn activating(config: ClientConfig, info: ConnectionInfo) -> Result<Self> {
        let finalizer = ActivationFinalizer::new(&info);
        let mut session = Self::new(config, info)?;
        session.state = State::Activating(finalizer);
        Ok(session)
    }

    /// Current server-negotiated desktop and input capabilities.
    pub fn info(&self) -> &ConnectionInfo {
        &self.info
    }
    /// Snapshot of the currently known RemoteApp windows, synchronized with graphics.
    pub fn remote_windows(&self) -> Vec<crate::remote_app::RemoteWindow> {
        self.remote_windows.snapshot()
    }
    /// Latest BGRA framebuffer, including any software-rendered remote pointer.
    pub fn graphics(&self) -> &Graphics {
        &self.graphics
    }
    /// Last bitmap codec successfully decoded on the graphics pipeline.
    ///
    /// `None` means that no pipeline bitmap has been decoded, including sessions
    /// that use only legacy bitmap updates. Capability advertisement is not usage.
    pub fn graphics_codec(&self) -> Option<crate::GraphicsCodec> {
        #[cfg(feature = "graphics")]
        {
            self.gfx.as_ref().and_then(crate::gfx::Gfx::codec)
        }
        #[cfg(not(feature = "graphics"))]
        {
            None
        }
    }
    /// Return and clear counts of H.264 decode calls and calls with no picture.
    pub fn take_avc_decode_counts(&mut self) -> (u64, u64) {
        #[cfg(feature = "graphics")]
        {
            self.gfx
                .as_mut()
                .map_or((0, 0), crate::gfx::Gfx::take_avc_audit)
        }
        #[cfg(not(feature = "graphics"))]
        {
            (0, 0)
        }
    }
    /// Returns and clears accumulated framebuffer damage.
    ///
    /// The first call and any desktop resize report the entire desktop. Later
    /// calls return a bounding rectangle for changes since the previous call,
    /// or `None` when no pixels need repainting. Pointer visibility can change
    /// without pixel damage; handle [`Event::GraphicsChanged`] and consult
    /// [`Graphics::hide_local_pointer`] independently of this rectangle.
    pub fn take_graphics_damage(&mut self) -> Option<Damage> {
        self.graphics.take_damage()
    }
    /// Whether a server frame has started but has not yet ended.
    ///
    /// While true, desktop pixels may contain only part of a logical update.
    /// Delay presentation, including cursor-only redraws, until this is false.
    /// Legacy bitmap streams without frame markers always return false.
    pub fn graphics_frame_in_progress(&self) -> bool {
        self.graphics.frame_in_progress()
    }
    /// Whether input is currently accepted.
    ///
    /// False during server reactivation and after disconnection or a protocol
    /// error. Keep processing frames during reactivation until [`Event::Resized`].
    pub fn is_active(&self) -> bool {
        matches!(self.state, State::Active)
    }

    /// Process one complete decrypted TPKT or fast-path frame in network order.
    ///
    /// Every error closes this session permanently. [`Event::Disconnected`]
    /// also ends it; neither condition may be retried on the same instance.
    pub fn process(&mut self, packet: &[u8]) -> Result<Vec<Event>> {
        self.process_with_callback(packet, None)
    }

    /// Processes a packet and exposes each completed server frame to `on_frame`.
    ///
    /// The callback runs at the frame's END marker, before any subsequent frame
    /// in the same packet can change the desktop. The view includes the frame ID,
    /// pixels, damage accumulated since the last callback or
    /// [`Self::take_graphics_damage`] call, and an optional acknowledgement token.
    /// Each callback clears that damage.
    /// Copy or retain the needed pixels inside the callback; the view is borrowed.
    ///
    /// Streams without frame markers do not invoke this callback. Continue to
    /// handle [`Event::GraphicsChanged`] for unframed updates. Errors have the
    /// same terminal behavior as [`Self::process`].
    pub fn process_with_frames(
        &mut self,
        packet: &[u8],
        mut on_frame: impl FnMut(GraphicsFrame<'_>),
    ) -> Result<Vec<Event>> {
        self.process_with_callback(packet, Some(&mut on_frame))
    }

    pub(crate) fn process_with_callback(
        &mut self,
        packet: &[u8],
        mut on_frame: Option<&mut FrameCallback<'_>>,
    ) -> Result<Vec<Event>> {
        let result = self.process_packet(packet, &mut on_frame);
        if result.is_err() {
            self.state = State::Closed;
            self.fragment = None;
            self.graphics.cancel_frame();
            self.frame_acknowledgements.reset(false);
        }
        result
    }

    fn process_packet(
        &mut self,
        packet: &[u8],
        on_frame: &mut Option<&mut FrameCallback<'_>>,
    ) -> Result<Vec<Event>> {
        if matches!(self.state, State::Closed) {
            return Err(Error::Invalid("closed session"));
        }
        let (kind, length) =
            wire::frame_size(packet, self.config.limits.max_pdu)?.ok_or(Error::Truncated)?;
        if length != packet.len() {
            return Err(Error::Invalid("frame length"));
        }
        let mut events = Vec::new();
        let mut changed = false;
        match kind {
            FrameKind::FastPath => {
                if matches!(self.state, State::Deactivated) {
                    return Err(Error::Invalid("graphics before activation"));
                }
                self.message_channel.record_data(packet.len());
                changed =
                    self.fast_path(wire::fast_path_payload(packet)?, &mut events, on_frame)?;
            }
            FrameKind::X224 => {
                if let Some(reason) = wire::mcs_disconnect(packet)? {
                    self.state = State::Closed;
                    self.fragment = None;
                    self.graphics.cancel_frame();
                    self.frame_acknowledgements.reset(false);
                    return Ok(vec![Event::Disconnected { reason }]);
                }
                let (initiator, channel, data) = wire::mcs_indication(packet)?;
                if initiator != self.info.server_id {
                    return Err(Error::Invalid("MCS server source"));
                }
                if Some(channel) == self.info.message_channel_id {
                    events.extend(self.receive_message_channel(data)?);
                    return Ok(events);
                }
                self.message_channel.record_data(data.len());
                if channel == self.info.io_channel_id {
                    changed |= self.share_pdus(data, &mut events)?;
                } else {
                    let mut header = Reader::new(data);
                    let length = header.u32_le()? as usize;
                    let flags = header.u32_le()?;
                    let payload = self.bulk_payload((flags >> 16) as u8, header.remaining())?;
                    let actions = self.channels.receive_parts(
                        channel,
                        length,
                        flags & !0x00ff_0000,
                        &payload,
                    )?;
                    self.channel_events_with_frames(actions, &mut events, on_frame)?;
                }
            }
        }
        if changed {
            events.push(Event::GraphicsChanged);
        }
        Ok(events)
    }

    pub(crate) fn share_pdus(&mut self, data: &[u8], events: &mut Vec<Event>) -> Result<bool> {
        let mut pdus = Reader::new(data);
        let mut changed = false;
        while let Some(pdu) = wire::next_share_pdu(&mut pdus)? {
            changed |= self.share_control(pdu, events)?;
        }
        Ok(changed)
    }

    /// Acknowledge consumption of a completed frame and all earlier received frames.
    ///
    /// Call this after presentation, or after deliberately discarding frames that
    /// will never be displayed. Retain the token with its pixels when coalescing
    /// updates; completing the newest token also acknowledges earlier skipped
    /// frames. This operation emits protocol packets but never submits them.
    ///
    /// Tokens from another session, an older activation, or an already confirmed
    /// frame are harmless no-ops. Inactive sessions also return no packets, so
    /// late renderer completions cannot acknowledge reused server frame IDs.
    /// Legacy server ID `0xffffffff` is echoed unchanged; that protocol permits
    /// servers to interpret it as acknowledgement of all in-flight frames,
    /// including newer frames. This engine never synthesizes that special ID.
    pub fn acknowledge_frame(&mut self, token: &FrameToken) -> Result<Vec<Event>> {
        if !self.is_active() {
            return Ok(Vec::new());
        }
        let info = &self.info;
        #[cfg(feature = "graphics")]
        let channels = &mut self.channels;
        self.frame_acknowledgements
            .acknowledge_with(token, |id, kind| match kind {
                frames::Acknowledgement::Legacy => Ok(vec![Event::Send(wire::share_data(
                    info.user_id,
                    info.io_channel_id,
                    info.share_id,
                    0x38,
                    &id.to_le_bytes(),
                )?)]),
                #[cfg(feature = "graphics")]
                frames::Acknowledgement::Graphics { total } => channels
                    .send_graphics(crate::gfx::Gfx::acknowledge(id, total))?
                    .into_iter()
                    .map(|action| match action {
                        ChannelEvent::Send { channel_id, data } => {
                            wire::mcs_send_data(info.user_id, channel_id, &data).map(Event::Send)
                        }
                        ChannelEvent::SendTunnel { protocol, payloads } => {
                            Ok(Event::SendTunnel { protocol, payloads })
                        }
                        _ => Err(Error::Invalid("graphics acknowledgement action")),
                    })
                    .collect(),
            })
    }

    /// Encode a local input operation and update the software pointer if needed.
    ///
    /// Input during server reactivation is ignored. Unsupported input capabilities
    /// and invalid local input return an error without closing an active session.
    /// Submit [`Input::ReleaseAll`] when the local window loses keyboard focus.
    pub fn input(&mut self, input: Input) -> Result<Vec<Event>> {
        if matches!(self.state, State::Closed) {
            return Err(Error::Invalid("closed session"));
        }
        // A Deactivate/Demand Active pair temporarily stops desktop input.
        if !self.is_active() {
            return Ok(Vec::new());
        }
        let moves_pointer = matches!(
            input,
            Input::Pointer { .. } | Input::MouseButton { .. } | Input::Wheel { .. }
        );
        let packets = self.input.encode(input, &self.info)?;
        let mut events: Vec<_> = packets.into_iter().map(Event::Send).collect();
        if moves_pointer {
            let (x, y) = self.input.position();
            if self.graphics.move_pointer(x, y)? {
                events.push(Event::GraphicsChanged);
            }
        }
        Ok(events)
    }

    /// Encode a small remote pointer motion and restore its current position.
    ///
    /// This is desktop activity, not a network heartbeat. Call only when the user
    /// enables idle prevention. Held keys/buttons and server reactivation suppress
    /// it. Both moves share one packet, use the latest server/client position, and
    /// do not change the local framebuffer. This cannot unlock a desktop or
    /// override mandatory server policies.
    pub fn keep_awake(&mut self) -> Result<Vec<Event>> {
        if matches!(self.state, State::Closed) {
            return Err(Error::Invalid("closed session"));
        }
        if !self.is_active() {
            return Ok(Vec::new());
        }
        Ok(self
            .input
            .keep_awake(self.graphics.pointer_position(), &self.info)?
            .into_iter()
            .map(Event::Send)
            .collect())
    }

    /// Advertise local text without reading the operating system clipboard.
    ///
    /// Text must fit [`crate::Limits::max_clipboard_bytes`] and contain no NUL.
    /// Unsupported clipboard access or invalid text preserves the connection;
    /// [`Error::Backpressure`] closes it to avoid dropping a queued operation.
    /// Publication completes with [`Event::ClipboardPublished`] or
    /// [`Event::ClipboardPublishRejected`]. Repeated pending updates retain only
    /// the latest text; identical accepted text completes without a wire roundtrip.
    pub fn set_clipboard(&mut self, text: String) -> Result<Vec<Event>> {
        if matches!(self.state, State::Closed) {
            return Err(Error::Invalid("closed session"));
        }
        let actions = self.channels.set_clipboard(text);
        if matches!(&actions, Err(Error::Backpressure)) {
            self.state = State::Closed;
        }
        let actions = actions?;
        let mut events = Vec::new();
        self.channel_events(actions, &mut events)?;
        Ok(events)
    }

    /// Whether the latest local text is accepted and the desktop is active.
    ///
    /// A new local offer or remote clipboard ownership makes this false. Use it
    /// with [`Event::ClipboardPublished`] before sending [`Input::Paste`]. Cancel
    /// a deferred paste on local focus loss or [`Event::ClipboardPublishRejected`].
    pub fn clipboard_ready(&self) -> bool {
        self.is_active() && self.channels.clipboard_ready()
    }

    /// Publish caller-supplied clipboard representations without accessing the OS.
    pub fn set_clipboard_content(&mut self, content: ClipboardContent) -> Result<Vec<Event>> {
        self.channel_operation(|channels| channels.set_clipboard_content(content))
    }

    /// Publish regular-file metadata. Keep the underlying files alive until
    /// [`Event::ClipboardFilesReleased`]; read bytes only for explicit requests.
    pub fn set_clipboard_files(
        &mut self,
        offer_id: u64,
        files: Vec<ClipboardFile>,
    ) -> Result<Vec<Event>> {
        self.channel_operation(|channels| channels.set_clipboard_files(offer_id, files))
    }

    /// Answer a file range request, or send failure with `None`.
    pub fn clipboard_file_contents(
        &mut self,
        request_id: u64,
        data: Option<Vec<u8>>,
    ) -> Result<Vec<Event>> {
        self.channel_operation(|channels| channels.clipboard_file_contents(request_id, data))
    }

    /// Revoke local files, cancel pending reads and release clipboard snapshots.
    pub fn cancel_clipboard_files(&mut self) -> Result<Vec<Event>> {
        self.channel_operation(Channels::cancel_clipboard_files)
    }

    /// Request a bounded range of a remote file. Results arrive through
    /// [`Event::ClipboardFileContents`]. Keep requests within the advertised size.
    pub fn request_clipboard_file(
        &mut self,
        offer_id: u64,
        index: u32,
        offset: u64,
        length: u32,
    ) -> Result<Vec<Event>> {
        self.channel_operation(|channels| {
            channels.request_clipboard_file(offer_id, index, offset, length)
        })
    }

    /// Cancel remote reads and release the server's snapshot lock. Also call this
    /// after all requested file bytes have been consumed successfully.
    pub fn cancel_remote_files(&mut self, offer_id: u64) -> Result<Vec<Event>> {
        self.channel_operation(|channels| channels.cancel_remote_files(offer_id))
    }

    /// Confirm a consumed audio block. `elapsed_ms` is time since its audio event.
    pub fn confirm_audio(&mut self, token: AudioToken, elapsed_ms: u32) -> Result<Vec<Event>> {
        self.channel_operation(|channels| channels.confirm_audio(token, elapsed_ms))
    }

    fn channel_operation(
        &mut self,
        operation: impl FnOnce(&mut Channels) -> Result<Vec<ChannelEvent>>,
    ) -> Result<Vec<Event>> {
        if matches!(self.state, State::Closed) {
            return Err(Error::Invalid("closed session"));
        }
        let actions = operation(&mut self.channels);
        if matches!(actions, Err(Error::Backpressure)) {
            self.state = State::Closed;
        }
        let mut events = Vec::new();
        self.channel_events(actions?, &mut events)?;
        Ok(events)
    }

    /// Request the peer's current Unicode text format, when one is available.
    ///
    /// The response arrives through [`Event::ClipboardText`] or
    /// [`Event::ClipboardUnavailable`]. [`Error::Backpressure`] is terminal.
    pub fn request_clipboard(&mut self) -> Result<Vec<Event>> {
        if matches!(self.state, State::Closed) {
            return Err(Error::Invalid("closed session"));
        }
        let actions = self.channels.request_clipboard();
        if matches!(&actions, Err(Error::Backpressure)) {
            self.state = State::Closed;
        }
        let actions = actions?;
        let mut events = Vec::new();
        self.channel_events(actions, &mut events)?;
        Ok(events)
    }

    /// Request a single-monitor desktop through the Display Control channel.
    ///
    /// The dimensions must fit local limits and protocol bounds; width must be
    /// even. Before channel setup finishes, only the latest request is retained.
    /// A request does not change the local framebuffer until the server
    /// reactivates it and reports [`Event::Resized`]. Unsupported/invalid requests
    /// preserve the connection, while [`Error::Backpressure`] closes it.
    pub fn resize(&mut self, width: u16, height: u16) -> Result<Vec<Event>> {
        if matches!(self.state, State::Closed) {
            return Err(Error::Invalid("closed session"));
        }
        let actions = self.channels.resize(width, height);
        if matches!(&actions, Err(Error::Backpressure)) {
            self.state = State::Closed;
        }
        let actions = actions?;
        let mut events = Vec::new();
        self.channel_events(actions, &mut events)?;
        Ok(events)
    }

    /// Request a validated multi-monitor topology, retaining it until server capabilities arrive.
    pub fn monitor_layout(&mut self, monitors: Vec<crate::Monitor>) -> Result<Vec<Event>> {
        self.channel_operation(|channels| channels.monitor_layout(monitors))
    }
    /// Report whether the requested local microphone stream could be opened.
    pub fn confirm_capture(&mut self, generation: u64, available: bool) -> Result<Vec<Event>> {
        self.channel_operation(|channels| channels.confirm_capture(generation, available))
    }
    /// Close audio input after device failure or permission revocation.
    pub fn close_capture(&mut self) -> Result<Vec<Event>> {
        self.channel_operation(|channels| channels.close_capture())
    }
    /// Submit exactly one negotiated block of signed 16-bit PCM samples.
    pub fn capture_samples(&mut self, generation: u64, samples: &[u8]) -> Result<Vec<Event>> {
        self.channel_operation(|channels| channels.capture_samples(generation, samples))
    }
    /// Discard codec state after capture is interrupted, before sending fresh samples.
    pub fn reset_capture(&mut self, generation: u64) -> Result<Vec<Event>> {
        self.channel_operation(|channels| channels.reset_capture(generation))
    }
    /// Complete one previously issued shared-drive request.
    pub fn complete_drive(&mut self, completion: crate::DriveCompletion) -> Result<Vec<Event>> {
        self.channel_operation(|channels| channels.complete_drive(completion))
    }

    fn channel_events(
        &mut self,
        actions: Vec<ChannelEvent>,
        events: &mut Vec<Event>,
    ) -> Result<()> {
        self.channel_events_with_frames(actions, events, &mut None)
    }

    fn channel_events_with_frames(
        &mut self,
        actions: Vec<ChannelEvent>,
        events: &mut Vec<Event>,
        on_frame: &mut Option<&mut FrameCallback<'_>>,
    ) -> Result<()> {
        for action in actions {
            match action {
                ChannelEvent::Send { channel_id, data } => events.push(Event::Send(
                    wire::mcs_send_data(self.info.user_id, channel_id, &data)?,
                )),
                ChannelEvent::SendTunnel { protocol, payloads } => {
                    events.push(Event::SendTunnel { protocol, payloads })
                }
                ChannelEvent::TunnelSynchronized {
                    channel_id,
                    fragments,
                } => {
                    let packets = fragments
                        .iter()
                        .map(|data| wire::mcs_send_data(self.info.user_id, channel_id, data))
                        .collect::<Result<Vec<_>>>()?;
                    events.push(Event::MultitransportSynchronized { packets });
                    self.flush_multitransport(events, on_frame)?;
                }
                ChannelEvent::ClipboardOffer => events.push(Event::ClipboardOffer),
                ChannelEvent::ClipboardText(text) => events.push(Event::ClipboardText(text)),
                ChannelEvent::ClipboardContent(content) => {
                    events.push(Event::ClipboardContent(content))
                }
                ChannelEvent::ClipboardUnavailable => events.push(Event::ClipboardUnavailable),
                ChannelEvent::ClipboardPublished => events.push(Event::ClipboardPublished),
                ChannelEvent::ClipboardPublishRejected => {
                    events.push(Event::ClipboardPublishRejected)
                }
                ChannelEvent::ClipboardFileRequest(request) => {
                    events.push(Event::ClipboardFileRequest(request))
                }
                ChannelEvent::ClipboardFilesReleased { offer_id } => {
                    events.push(Event::ClipboardFilesReleased { offer_id })
                }
                ChannelEvent::ClipboardFiles { offer_id, files } => {
                    events.push(Event::ClipboardFiles { offer_id, files })
                }
                ChannelEvent::ClipboardFileContents { request, data } => {
                    events.push(Event::ClipboardFileContents { request, data })
                }
                ChannelEvent::ClipboardRemoteFilesCancelled { offer_id } => {
                    events.push(Event::ClipboardRemoteFilesCancelled { offer_id })
                }
                ChannelEvent::Audio(event) => events.push(Event::Audio(event)),
                ChannelEvent::Capture(event) => events.push(Event::Capture(event)),
                ChannelEvent::DriveRequest(request) => events.push(Event::DriveRequest(request)),
                ChannelEvent::RequestRejected(error) => events.push(Event::RequestRejected(error)),
                ChannelEvent::RemoteApp(event) => events.push(Event::RemoteApp(event)),
                ChannelEvent::DisplayReady => {} // Channels sends any pending latest size itself.
                #[cfg(feature = "graphics")]
                ChannelEvent::GraphicsOpened => {
                    let gfx =
                        crate::gfx::Gfx::new(self.config.limits, self.config.allow_lossy_bitmaps);
                    let actions = self.channels.send_graphics(gfx.capabilities())?;
                    self.gfx = Some(gfx);
                    self.channel_events(actions, events)?;
                }
                #[cfg(feature = "graphics")]
                ChannelEvent::GraphicsData(data) => {
                    self.graphics_pipeline(&data, events, on_frame)?
                }
                #[cfg(feature = "graphics")]
                ChannelEvent::GraphicsClosed => {
                    self.gfx = None;
                    self.graphics.cancel_frame();
                    self.frame_acknowledgements
                        .reset(self.info.frame_acknowledgements);
                }
            }
        }
        Ok(())
    }

    fn share_control(&mut self, packet: &[u8], events: &mut Vec<Event>) -> Result<bool> {
        let mut reader = Reader::new(packet);
        if usize::from(reader.u16_le()?) != packet.len() {
            return Err(Error::Invalid("Share Control length"));
        }
        let kind = reader.u16_le()?;
        if kind & 0xfff0 != 0x10 {
            return Err(Error::Unsupported("Share Control version"));
        }
        let source = reader.u16_le()?;
        // Some server notification PDUs explicitly use pduSource=0.
        if source != 0 && source != self.info.server_id && kind & 15 != 1 {
            return Err(Error::Invalid("Share Control source"));
        }
        match kind & 15 {
            1 => {
                if !matches!(self.state, State::Deactivated) {
                    return Err(Error::Invalid("unexpected Demand Active"));
                }
                let packets = connection::reactivate(&self.config, &mut self.info, packet)?;
                self.channels.set_chunk_size(self.info.vc_chunk_size)?;
                self.graphics.resize(self.info.width, self.info.height)?;
                self.frame_acknowledgements
                    .reset(self.info.frame_acknowledgements);
                self.state = State::Activating(ActivationFinalizer::new(&self.info));
                events.extend(packets.into_iter().map(Event::Send));
                Ok(false)
            }
            6 => {
                let _share = reader.u32_le()?;
                let length = usize::from(reader.u16_le()?);
                reader.skip(length)?;
                reader.finish()?;
                events.extend(
                    self.input
                        .encode(Input::ReleaseAll, &self.info)?
                        .into_iter()
                        .map(Event::Send),
                );
                #[cfg(feature = "graphics")]
                if let Some(gfx) = &mut self.gfx {
                    gfx.deactivate();
                }
                self.state = State::Deactivated;
                self.graphics.cancel_frame();
                self.frame_acknowledgements.reset(false);
                self.fragment = None;
                Ok(false)
            }
            7 => self.share_data(reader.remaining(), events),
            10 => Err(Error::Unsupported("server redirection")),
            _ => Err(Error::Unsupported("Share Control PDU type")),
        }
    }

    /// Send a local window-manager operation to a known remote application.
    pub fn remote_app(
        &mut self,
        command: crate::remote_app::RemoteAppCommand,
    ) -> Result<Vec<Event>> {
        if !matches!(self.state, State::Active) {
            return Err(Error::Invalid("inactive session"));
        }
        use crate::remote_app::RemoteAppCommand;
        let id = match &command {
            RemoteAppCommand::Activate { window_id, .. }
            | RemoteAppCommand::System { window_id, .. }
            | RemoteAppCommand::Move { window_id, .. } => *window_id,
        };
        if !self.remote_windows.contains(id) {
            return Err(Error::Invalid("unknown RemoteApp window"));
        }
        let actions = self.channels.remote_app(command)?;
        let mut events = Vec::new();
        self.channel_events(actions, &mut events)?;
        Ok(events)
    }

    fn window_orders(&mut self, data: &[u8], slow: bool, events: &mut Vec<Event>) -> Result<bool> {
        if self.config.remote_app.is_none() {
            return Err(Error::Unsupported("drawing orders"));
        }
        events.extend(
            self.remote_windows
                .receive(data, slow)?
                .into_iter()
                .map(Event::RemoteApp),
        );
        Ok(false)
    }

    fn share_data(&mut self, data: &[u8], events: &mut Vec<Event>) -> Result<bool> {
        let mut reader = Reader::new(data);
        let share_id = reader.u32_le()?;
        reader.skip(2)?; // pad and stream ID
        // This field is not consistently populated by senders (MS-RDPBCGR
        // 4.1.18); framing and bounded decompression define the payload size.
        let _uncompressed_length = reader.u16_le()?;
        let kind = reader.u8()?;
        let compression = reader.u8()?;
        let compressed_length = usize::from(reader.u16_le()?);
        if compression & 0x20 != 0 && compressed_length != data.len() + 6 {
            return Err(Error::Invalid("compressed Share Data length"));
        }
        let payload = self.bulk_payload(compression, reader.remaining())?;
        if payload.len().saturating_add(18) > self.config.limits.max_pdu {
            return Err(Error::Limit("Share Data payload"));
        }
        if let State::Activating(finalizer) = &mut self.state
            && matches!(kind, 20 | 31 | 38 | 40 | 47 | 54 | 55)
        {
            if finalizer.advance_data(share_id, kind, &payload)? {
                self.state = State::Active;
                events.push(Event::Resized {
                    width: self.info.width,
                    height: self.info.height,
                });
                return Ok(true);
            }
            return Ok(false);
        }
        let payload = payload.as_ref();
        let mut reader = Reader::new(payload);
        if matches!(kind, 2 | 27) && matches!(self.state, State::Deactivated) {
            return Err(Error::Invalid("graphics before activation"));
        }
        match kind {
            2 => match reader.u16_le()? {
                0 => self.window_orders(reader.remaining(), true, events),
                1 => self.graphics.bitmap_update(payload),
                2 => self.graphics.palette_update(payload),
                3 => {
                    reader.skip(2)?;
                    reader.finish()?;
                    Ok(false)
                }
                _ => Err(Error::Invalid("graphics update type")),
            },
            27 => {
                let kind = reader.u16_le()?;
                reader.skip(2)?;
                let kind = match kind {
                    1 => {
                        let kind = match reader.u32_le()? {
                            0 => PointerKind::Hidden,
                            0x7f00 => PointerKind::Default,
                            _ => return Err(Error::Invalid("system pointer type")),
                        };
                        reader.finish()?;
                        return self.graphics.pointer_update(kind, &[]);
                    }
                    3 => PointerKind::Position,
                    6 => PointerKind::Color,
                    7 => PointerKind::Cached,
                    8 => PointerKind::New,
                    // MS-RDPBCGR 3.2.5.9.2 requires ignoring unknown slow-path
                    // pointer updates. Large pointers use fast-path code 12.
                    _ => return Ok(false),
                };
                self.graphics.pointer_update(kind, reader.remaining())
            }
            47 => {
                let code = reader.u32_le()?;
                reader.finish()?;
                if code == 0 {
                    Ok(false)
                } else {
                    Err(Error::Server(code))
                }
            }
            // These notifications carry no desktop pixels. Hardware LEDs, local
            // IME state, beeps and auto-reconnect cookies are deliberately unused.
            20 | 34 | 40 => {
                reader.skip(8)?;
                reader.finish()?;
                Ok(false)
            }
            31 | 41 | 50 | 54 => {
                reader.skip(4)?;
                reader.finish()?;
                Ok(false)
            }
            45 => {
                reader.skip(10)?;
                reader.finish()?;
                Ok(false)
            }
            38 => {
                if reader.u32_le()? > 3 {
                    return Err(Error::Invalid("logon information type"));
                }
                // No reconnect cookie or identity from this payload is retained.
                Ok(false)
            }
            55 => {
                let count = reader.u32_le()? as usize;
                if count == 0 || count > 16 {
                    return Err(Error::Limit("monitor count"));
                }
                reader.skip(count * 20)?;
                reader.finish()?;
                Ok(false)
            }
            _ => Err(Error::Unsupported("Share Data PDU type")),
        }
    }

    fn bulk_payload<'a>(&mut self, flags: u8, payload: &'a [u8]) -> Result<Cow<'a, [u8]>> {
        if flags == 0 {
            return Ok(Cow::Borrowed(payload));
        }
        let (maximum, decoder) = self
            .bulk
            .as_mut()
            .ok_or(Error::Unsupported("unnegotiated bulk compression"))?;
        if flags & 15 > *maximum as u8 {
            return Err(Error::Unsupported("unnegotiated bulk compression package"));
        }
        decoder.decode(flags, payload)
    }

    fn fast_path(
        &mut self,
        data: &[u8],
        events: &mut Vec<Event>,
        on_frame: &mut Option<&mut FrameCallback<'_>>,
    ) -> Result<bool> {
        let mut reader = Reader::new(data);
        let mut changed = false;
        let fragment_limit = self
            .config
            .limits
            .max_pdu
            .min(self.config.limits.max_bitmap_bytes);
        while !reader.is_empty() {
            let header = reader.u8()?;
            let code = header & 15;
            let fragmentation = (header >> 4) & 3;
            let compression = match header >> 6 {
                0 => 0,
                2 => reader.u8()?,
                _ => return Err(Error::Invalid("fast-path compression field")),
            };
            let size = usize::from(reader.u16_le()?);
            let payload = self.bulk_payload(compression, reader.take(size)?)?;
            match fragmentation {
                0 => {
                    if self.fragment.is_some() {
                        return Err(Error::Invalid("interrupted fast-path fragments"));
                    }
                    changed |= self.graphics_update(code, &payload, events, on_frame)?;
                }
                2 => {
                    if self.fragment.is_some() {
                        return Err(Error::Invalid("overlapping fast-path fragments"));
                    }
                    if payload.len() > fragment_limit {
                        return Err(Error::Limit("graphics fragments"));
                    }
                    self.fragment = Some(Fragment {
                        code,
                        bytes: payload.into_owned(),
                    });
                }
                1 | 3 => {
                    let fragment = self
                        .fragment
                        .as_mut()
                        .ok_or(Error::Invalid("missing first fast-path fragment"))?;
                    if fragment.code != code {
                        return Err(Error::Invalid("fast-path fragment type"));
                    }
                    if fragment.bytes.len().saturating_add(payload.len()) > fragment_limit {
                        return Err(Error::Limit("graphics fragments"));
                    }
                    fragment.bytes.extend_from_slice(&payload);
                    if fragmentation == 1 {
                        let fragment = self
                            .fragment
                            .take()
                            .ok_or(Error::Invalid("graphics fragment state"))?;
                        changed |= self.graphics_update(code, &fragment.bytes, events, on_frame)?;
                    }
                }
                _ => unreachable!(),
            }
        }
        Ok(changed)
    }

    fn graphics_update(
        &mut self,
        code: u8,
        data: &[u8],
        events: &mut Vec<Event>,
        on_frame: &mut Option<&mut FrameCallback<'_>>,
    ) -> Result<bool> {
        let pointer = match code {
            0 => return self.window_orders(data, false, events),
            1 => return self.graphics.bitmap_update(data),
            2 => return self.graphics.palette_update(data),
            3 => {
                Reader::new(data).finish()?;
                return Ok(false);
            }
            4 => {
                let acknowledgements = &mut self.frame_acknowledgements;
                let bits_per_pixel = self.info.color_depth;
                let mut on_marker = |marker, graphics: &mut Graphics| {
                    match marker {
                        FrameMarker::Begin(frame_id) => {
                            events.push(Event::GraphicsFrameBegin { frame_id });
                        }
                        FrameMarker::End(frame_id) => {
                            let acknowledgement = acknowledgements.complete(frame_id)?;
                            if let Some(callback) = on_frame.as_deref_mut() {
                                let damage = graphics.take_damage();
                                callback(GraphicsFrame {
                                    id: frame_id,
                                    bits_per_pixel,
                                    graphics,
                                    damage,
                                    acknowledgement: acknowledgement.clone(),
                                });
                            }
                            events.push(Event::GraphicsFrameEnd {
                                frame_id,
                                acknowledgement,
                            });
                        }
                    }
                    Ok(())
                };
                return self.graphics.surface_update(
                    data,
                    self.info.surface_commands,
                    self.info.nscodec,
                    &mut Some(&mut on_marker),
                );
            }
            5 => PointerKind::Hidden,
            6 => PointerKind::Default,
            8 => PointerKind::Position,
            9 => PointerKind::Color,
            10 => PointerKind::Cached,
            11 => PointerKind::New,
            12 => PointerKind::Large,
            _ => return Err(Error::Invalid("fast-path update code")),
        };
        self.graphics.pointer_update(pointer, data)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    pub(super) fn session() -> Session {
        let config = ClientConfig {
            width: 2,
            height: 2,
            ..ClientConfig::default()
        };
        let info = ConnectionInfo {
            user_id: 1001,
            server_id: 1002,
            io_channel_id: 1003,
            share_id: 1,
            width: 2,
            height: 2,
            color_depth: 32,
            channels: vec![],
            fastpath_input: true,
            unicode_input: true,
            mouse_extended: true,
            horizontal_wheel: true,
            vc_chunk_size: 1600,
            surface_commands: 0,
            nscodec: false,
            frame_acknowledgements: false,
            message_channel_id: None,
            multitransport_flags: 0,
        };
        Session::new(config, info).unwrap()
    }

    #[test]
    fn remote_app_session_routes_static_channel_window_orders_and_local_commands() {
        use crate::remote_app::{RemoteApp, RemoteAppCommand, RemoteAppEvent, SystemCommand};
        let base = session();
        let mut config = base.config;
        config.dynamic_resolution = false;
        config.remote_app = Some(RemoteApp {
            program: "||calc".into(),
            ..Default::default()
        });
        let mut info = base.info;
        info.channels = vec![crate::Channel {
            id: 1004,
            name: "rail".into(),
        }];
        let mut session = Session::new(config, info).unwrap();
        let mut data = vec![8, 0, 0, 0, 3, 0, 0, 0];
        data.extend_from_slice(&[5, 0, 8, 0, 1, 0, 0, 0]);
        let mut handshake = wire::mcs_send_data(1002, 1004, &data).unwrap();
        handshake[7] = 0x68;
        let responses = session.process(&handshake).unwrap();
        assert_eq!(
            responses
                .iter()
                .filter(|e| matches!(e, Event::Send(_)))
                .count(),
            9
        );
        let order = crate::remote_app::test_window_order();
        let events = session.process(&fast(&[(0, &order)])).unwrap();
        assert!(
            events
                .iter()
                .any(|e| matches!(e, Event::RemoteApp(RemoteAppEvent::Window(w)) if w.id == 42))
        );
        assert_eq!(session.remote_windows()[0].title, "A");
        for command in [
            RemoteAppCommand::Activate {
                window_id: 42,
                enabled: true,
            },
            RemoteAppCommand::System {
                window_id: 42,
                command: SystemCommand::Close,
            },
        ] {
            assert!(matches!(
                &session.remote_app(command).unwrap()[..],
                [Event::Send(_)]
            ));
            assert!(session.is_active());
        }
        assert!(
            session
                .remote_app(RemoteAppCommand::Activate {
                    window_id: 43,
                    enabled: true
                })
                .is_err()
        );
        let deletion = [1, 0, 0x2e, 11, 0, 0, 0, 0, 0x21, 42, 0, 0, 0];
        assert!(
            session
                .process(&fast(&[(0, &deletion)]))
                .unwrap()
                .iter()
                .any(|e| matches!(e, Event::RemoteApp(RemoteAppEvent::Deleted(42))))
        );
        assert!(session.remote_windows().is_empty());
        assert!(session.is_active());
        assert!(
            super::tests::session()
                .process(&fast(&[(0, &order)]))
                .is_err()
        );
    }

    fn fast(updates: &[(u8, &[u8])]) -> Vec<u8> {
        let mut packet = vec![0, 0];
        for (header, data) in updates {
            packet.push(*header);
            packet.extend_from_slice(&(data.len() as u16).to_le_bytes());
            packet.extend_from_slice(data);
        }
        packet[1] = packet.len() as u8;
        packet
    }

    fn surface_marker(action: u16, id: u32) -> Vec<u8> {
        let mut data = 4u16.to_le_bytes().to_vec();
        data.extend_from_slice(&action.to_le_bytes());
        data.extend_from_slice(&id.to_le_bytes());
        data
    }

    fn surface_bits(codec: u8, pixels: &[u8]) -> Vec<u8> {
        let mut data = Vec::new();
        for value in [1u16, 0, 0, 1, 1] {
            data.extend_from_slice(&value.to_le_bytes());
        }
        data.extend_from_slice(&[32, 0, 0, codec]);
        data.extend_from_slice(&1u16.to_le_bytes());
        data.extend_from_slice(&1u16.to_le_bytes());
        data.extend_from_slice(&(pixels.len() as u32).to_le_bytes());
        data.extend_from_slice(pixels);
        data
    }

    fn acknowledgement_session() -> Session {
        let mut session = session();
        session.config.frame_acknowledgements = std::num::NonZeroU32::new(3);
        session.info.surface_commands = 0x52;
        session.info.frame_acknowledgements = true;
        session.frame_acknowledgements.reset(true);
        session
    }

    fn completed_frame(session: &mut Session, id: u32) -> FrameToken {
        let mut surface = surface_marker(0, id);
        surface.extend(surface_marker(1, id));
        session
            .process(&fast(&[(4, &surface)]))
            .unwrap()
            .into_iter()
            .find_map(|event| match event {
                Event::GraphicsFrameEnd {
                    acknowledgement, ..
                } => acknowledgement,
                _ => None,
            })
            .unwrap()
    }

    #[test]
    fn frame_receipts_match_callbacks_and_events_without_eager_acknowledgements() {
        let mut session = acknowledgement_session();
        let mut surface = surface_marker(0, 7);
        surface.extend(surface_bits(0, &[1, 2, 3, 0]));
        surface.extend(surface_marker(1, 7));
        surface.extend(surface_marker(0, 8));
        let mut receipt = None;
        let events = session
            .process_with_frames(&fast(&[(4, &surface)]), |frame| {
                assert_eq!(frame.id, 7);
                assert_eq!(&frame.graphics.pixels()[..4], &[1, 2, 3, 255]);
                receipt = frame.acknowledgement;
            })
            .unwrap();
        let receipt = receipt.unwrap();
        assert!(events.contains(&Event::GraphicsFrameEnd {
            frame_id: 7,
            acknowledgement: Some(receipt.clone()),
        }));
        assert!(!events.iter().any(|event| matches!(event, Event::Send(_))));
        assert!(session.graphics_frame_in_progress());
        assert_eq!(session.acknowledge_frame(&receipt).unwrap().len(), 1);
        assert!(session.acknowledge_frame(&receipt).unwrap().is_empty());
    }

    #[test]
    fn late_frame_receipts_are_no_ops_after_deactivation_disconnect_and_error() {
        for terminal in [
            share_packet(&wire::share_control(1002, 6, &[1, 0, 0, 0, 0, 0]).unwrap()),
            vec![3, 0, 0, 9, 2, 0xf0, 0x80, 0x21, 0x80],
            fast(&[(4, &surface_marker(1, 99))]),
        ] {
            let mut session = acknowledgement_session();
            let receipt = completed_frame(&mut session, 7);
            let _ = session.process(&terminal);
            assert!(!session.is_active());
            assert!(session.acknowledge_frame(&receipt).unwrap().is_empty());
            // A reused ID after reactivation belongs to a different receipt epoch.
            session.state = State::Active;
            assert!(session.acknowledge_frame(&receipt).unwrap().is_empty());
            session.frame_acknowledgements.reset(true);
            let fresh = completed_frame(&mut session, 7);
            assert_ne!(receipt, fresh);
            assert!(session.acknowledge_frame(&receipt).unwrap().is_empty());
            assert_eq!(session.acknowledge_frame(&fresh).unwrap().len(), 1);
        }
    }

    #[test]
    fn activation_keeps_receipts_pending_until_the_session_is_active() {
        let mut session = acknowledgement_session();
        session.state = State::Activating(ActivationFinalizer::new(&session.info));
        let receipt = completed_frame(&mut session, 7);
        assert!(session.acknowledge_frame(&receipt).unwrap().is_empty());
        for (kind, body) in [
            (31, vec![1, 0, 0, 0]),
            (20, vec![4, 0, 0, 0, 0, 0, 0, 0]),
            (20, vec![2, 0, 0, 0, 0, 0, 0, 0]),
            (40, vec![0; 8]),
        ] {
            session.process(&slow(kind, &body)).unwrap();
        }
        assert!(session.is_active());
        assert_eq!(session.acknowledge_frame(&receipt).unwrap().len(), 1);
    }

    #[test]
    fn surface_frames_deliver_complete_pixels_before_the_next_begin() {
        let mut session = session();
        session.info.surface_commands = 0x52;
        session.take_graphics_damage();
        let mut surface = surface_marker(0, 1);
        surface.extend(surface_bits(0, &[1, 2, 3, 0]));
        surface.extend(surface_marker(1, 1));
        surface.extend(surface_marker(0, 2));
        surface.extend(surface_bits(0, &[4, 5, 6, 0]));
        let mut observed = Vec::new();
        let events = session
            .process_with_frames(&fast(&[(4, &surface)]), |frame| {
                observed.push((
                    frame.id,
                    frame.graphics.pixels()[..4].to_vec(),
                    frame.damage,
                ));
            })
            .unwrap();
        assert_eq!(
            observed,
            [(
                1,
                vec![1, 2, 3, 255],
                Some(Damage {
                    x: 0,
                    y: 0,
                    width: 1,
                    height: 1
                })
            )]
        );
        assert_eq!(
            events,
            [
                Event::GraphicsFrameBegin { frame_id: 1 },
                Event::GraphicsFrameEnd {
                    frame_id: 1,
                    acknowledgement: None
                },
                Event::GraphicsFrameBegin { frame_id: 2 },
                Event::GraphicsChanged
            ]
        );
        assert!(session.graphics_frame_in_progress());
        assert_eq!(&session.graphics().pixels()[..4], &[4, 5, 6, 255]);
        let end = fast(&[(4, &surface_marker(1, 2))]);
        session
            .process_with_frames(&end, |frame| {
                observed.push((
                    frame.id,
                    frame.graphics.pixels()[..4].to_vec(),
                    frame.damage,
                ));
            })
            .unwrap();
        assert_eq!(observed[1].0, 2);
        assert_eq!(observed[1].1, [4, 5, 6, 255]);
        assert!(observed[1].2.is_some());
        assert!(!session.graphics_frame_in_progress());
        assert_eq!(session.take_graphics_damage(), None);
    }

    #[test]
    fn fragmented_nscodec_surface_preserves_frame_boundaries() {
        let mut session = session();
        session.info.surface_commands = 0x52;
        session.info.nscodec = true;
        let mut encoded = Vec::new();
        for count in [1u32, 1, 1, 0] {
            encoded.extend_from_slice(&count.to_le_bytes());
        }
        encoded.extend_from_slice(&[1, 0, 0, 0, 50, 0, 0]);
        let mut surface = surface_marker(0, 9);
        surface.extend(surface_bits(1, &encoded));
        surface.extend(surface_marker(1, 9));
        assert!(
            session
                .process(&fast(&[(0x24, &surface[..13])]))
                .unwrap()
                .is_empty()
        );
        assert_eq!(&session.graphics().pixels()[..4], &[0, 0, 0, 255]);
        let mut completed = Vec::new();
        let events = session
            .process_with_frames(&fast(&[(0x14, &surface[13..])]), |frame| {
                completed.push((frame.id, frame.graphics.pixels()[..4].to_vec()));
            })
            .unwrap();
        assert_eq!(completed, [(9, vec![50, 50, 50, 255])]);
        assert_eq!(
            events,
            [
                Event::GraphicsFrameBegin { frame_id: 9 },
                Event::GraphicsFrameEnd {
                    frame_id: 9,
                    acknowledgement: None
                },
                Event::GraphicsChanged
            ]
        );
        assert!(!session.graphics_frame_in_progress());
    }

    #[test]
    fn surface_commands_require_negotiation_and_resize_cancels_open_frames() {
        let mut rejected = session();
        assert!(
            rejected
                .process(&fast(&[(4, &surface_marker(0, 1))]))
                .is_err()
        );
        assert!(!rejected.is_active());
        let mut session = session();
        session.info.surface_commands = 0x52;
        session
            .process(&fast(&[(4, &surface_marker(0, 1))]))
            .unwrap();
        assert!(session.graphics_frame_in_progress());
        session.graphics.resize(3, 2).unwrap();
        assert!(!session.graphics_frame_in_progress());
    }
    #[test]
    fn pointer_visibility_follows_wire_order_in_one_frame() {
        let mut session = session();
        assert_eq!(
            session.take_graphics_damage(),
            Some(Damage {
                x: 0,
                y: 0,
                width: 2,
                height: 2
            })
        );
        let events = session.process(&fast(&[(6, &[]), (5, &[])])).unwrap();
        assert_eq!(events, vec![Event::GraphicsChanged]);
        assert!(session.graphics().hide_local_pointer());
        assert_eq!(session.take_graphics_damage(), None);
        session.process(&fast(&[(8, &[1, 0, 1, 0])])).unwrap();
        assert!(session.graphics().hide_local_pointer());
        session.process(&fast(&[(6, &[])])).unwrap();
        assert!(!session.graphics().hide_local_pointer());
    }
    #[test]
    fn fragmented_updates_are_sequenced_and_bounded() {
        let mut session = session();
        session.config.limits.max_bitmap_bytes = 4;
        session.process(&fast(&[(0x28, &[1, 0])])).unwrap();
        session.process(&fast(&[(0x18, &[1, 0])])).unwrap();
        session.process(&fast(&[(0x28, &[1, 0, 0, 0])])).unwrap();
        assert_eq!(
            session.process(&fast(&[(0x38, &[0])])).unwrap_err(),
            Error::Limit("graphics fragments")
        );
        assert!(!session.is_active());
        assert!(session.process(&fast(&[(6, &[])])).is_err());
    }
    #[test]
    fn incomplete_or_interleaved_fragments_fail_closed() {
        let mut first = session();
        assert!(first.process(&fast(&[(0x18, &[1, 0, 1, 0])])).is_err());
        let mut second = session();
        second.process(&fast(&[(0x28, &[1, 0])])).unwrap();
        assert!(second.process(&fast(&[(0x1a, &[1, 0])])).is_err());
    }
    #[test]
    fn official_mcs_disconnect_fixture() {
        let mut session = session();
        assert_eq!(
            session
                .process(&[3, 0, 0, 9, 2, 0xf0, 0x80, 0x21, 0x80])
                .unwrap(),
            vec![Event::Disconnected { reason: 3 }]
        );
        assert!(!session.is_active());
    }

    #[test]
    fn closed_session_cannot_emit_commands() {
        let mut session = session();
        session
            .process(&[3, 0, 0, 9, 2, 0xf0, 0x80, 0x21, 0x80])
            .unwrap();
        for result in [
            session.input(Input::ReleaseAll),
            session.set_clipboard("private".into()),
            session.request_clipboard(),
            session.resize(1280, 720),
        ] {
            assert_eq!(result.unwrap_err(), Error::Invalid("closed session"));
        }
    }

    #[test]
    fn suspended_clipboard_updates_coalesce_and_peer_replies_remain_bounded() {
        let mut config = ClientConfig {
            width: 320,
            height: 200,
            ..ClientConfig::default()
        };
        config.limits.max_channel_message = 1600;
        config.limits.max_clipboard_bytes = 128;
        let mut info = session().info().clone();
        info.width = config.width;
        info.height = config.height;
        info.channels.push(crate::Channel {
            id: 1004,
            name: "cliprdr".into(),
        });
        let mut session = Session::new(config, info).unwrap();
        for message in [
            &[8, 0, 0, 0, 3, 0, 0, 0, 1, 0, 0, 0, 0, 0, 0, 0][..], // Monitor ready.
            &[0, 0, 0, 0, 0x20, 0, 0, 0],                          // Suspend all static channels.
        ] {
            let mut packet = wire::mcs_send_data(1002, 1004, message).unwrap();
            packet[7] = 0x68;
            session.process(&packet).unwrap();
        }
        // Local updates wait for the untagged initial ACK and only retain the
        // latest value, even while static channel transmission is suspended.
        for index in 0..32 {
            assert_eq!(
                session.set_clipboard(index.to_string()).unwrap(),
                [Event::ClipboardUnavailable]
            );
        }
        assert!(session.is_active());
        assert!(!session.clipboard_ready());

        // Peer format changes still require a response. A flood cannot grow the
        // suspended wire queue beyond its configured 1600-byte bound.
        let message = [8, 0, 0, 0, 3, 0, 0, 0, 2, 0, 0, 0, 0, 0, 0, 0];
        let mut packet = wire::mcs_send_data(1002, 1004, &message).unwrap();
        packet[7] = 0x68;
        for _ in 0..100 {
            assert!(session.process(&packet).is_ok());
        }
        assert_eq!(session.process(&packet).unwrap_err(), Error::Backpressure);
        assert!(!session.is_active());
        assert_eq!(
            session.request_clipboard().unwrap_err(),
            Error::Invalid("closed session")
        );
    }

    fn slow(kind: u8, payload: &[u8]) -> Vec<u8> {
        let mut packet = wire::share_data(1002, 1003, 1, kind, payload).unwrap();
        packet[7] = 0x68; // MCS Send Data Indication, rather than Request.
        packet
    }

    #[test]
    fn bitmap_payload_format_is_shared_by_both_transports() {
        let mut payload = Vec::new();
        for field in [1_u16, 1, 0, 0, 0, 0, 1, 1, 24, 0, 4] {
            payload.extend_from_slice(&field.to_le_bytes());
        }
        payload.extend_from_slice(&[1, 2, 3, 0]);
        for packet in [fast(&[(1, &payload)]), slow(2, &payload)] {
            let mut session = session();
            assert_eq!(session.process(&packet).unwrap(), [Event::GraphicsChanged]);
            assert_eq!(&session.graphics().pixels()[..4], &[1, 2, 3, 255]);
        }
    }

    #[test]
    fn keyboard_input_does_not_undo_server_pointer_position() {
        let mut session = session();
        let mut pointer = Vec::new();
        for field in [0_u16, 0, 0, 1, 1, 2, 4] {
            pointer.extend_from_slice(&field.to_le_bytes());
        }
        pointer.extend_from_slice(&[255, 255, 255, 0, 0, 0]);
        session
            .process(&fast(&[(9, &pointer), (8, &[1, 0, 1, 0])]))
            .unwrap();
        let pixels = session.graphics().pixels().to_vec();
        let events = session.input(Input::Unicode("a".into())).unwrap();
        assert!(!events.contains(&Event::GraphicsChanged));
        assert_eq!(session.graphics().pixels(), pixels);
        assert_eq!(&session.graphics().pixels()[12..], &[255; 4]);
    }

    #[test]
    fn keep_awake_uses_server_pointer_position_without_redrawing_the_frame() {
        let mut session = session();
        session.process(&fast(&[(8, &[1, 0, 1, 0])])).unwrap();
        assert_eq!(session.input.position(), (0, 0));
        let pixels = session.graphics().pixels().to_vec();
        let events = session.keep_awake().unwrap();
        let [Event::Send(packet)] = events.as_slice() else {
            panic!("expected a single input packet and no graphics changes");
        };
        assert_eq!(packet, &[8, 16, 0x20, 0, 8, 0, 0, 1, 0, 0x20, 0, 8, 1, 0, 1, 0]);
        assert_eq!(session.graphics().pointer_position(), (1, 1));
        assert_eq!(session.graphics().pixels(), pixels);
        session.state = State::Closed;
        assert!(session.keep_awake().is_err());
    }

    #[test]
    fn synchronization_ime_and_unknown_slow_pointer_match_spec() {
        let mut session = session();
        assert!(
            session
                .process(&slow(2, &[3, 0, 0x80, 0x80]))
                .unwrap()
                .is_empty()
        );
        assert!(session.process(&fast(&[(3, &[])])).unwrap().is_empty());
        assert!(session.process(&slow(45, &[0; 10])).unwrap().is_empty());
        assert!(
            session
                .process(&slow(27, &[9, 0, 0, 0, 1]))
                .unwrap()
                .is_empty()
        );
        assert!(!session.graphics().hide_local_pointer());
    }

    #[test]
    fn fragmented_updates_obey_advertised_multifragment_limit() {
        let mut session = session();
        session.config.limits.max_pdu = 32;
        session.process(&fast(&[(0x29, &[0; 20])])).unwrap();
        assert_eq!(
            session.process(&fast(&[(0x39, &[0; 20])])).unwrap_err(),
            Error::Limit("graphics fragments")
        );
    }

    fn share_packet(pdu: &[u8]) -> Vec<u8> {
        let mut packet = wire::mcs_send_data(1002, 1003, pdu).unwrap();
        packet[7] = 0x68;
        packet
    }

    fn bulk_session(compression: Option<BulkCompression>) -> Session {
        let config = ClientConfig {
            width: 2,
            height: 2,
            bulk_compression: compression,
            ..ClientConfig::default()
        };
        let mut info = session().info;
        info.channels.push(crate::Channel {
            id: 1004,
            name: "cliprdr".into(),
        });
        Session::new(config, info).unwrap()
    }

    fn compressed_fast(header: u8, compression: u8, payload: &[u8]) -> Vec<u8> {
        let mut packet = vec![0, (payload.len() + 6) as u8, header | 0x80, compression];
        packet.extend_from_slice(&(payload.len() as u16).to_le_bytes());
        packet.extend_from_slice(payload);
        packet
    }

    fn compressed_slow(kind: u8, compression: u8, payload: &[u8]) -> Vec<u8> {
        let mut body = vec![1, 0, 0, 0, 0, 1, 0, 0, kind, compression];
        body.extend_from_slice(&((payload.len() + 18) as u16).to_le_bytes());
        body.extend_from_slice(payload);
        share_packet(&wire::share_control(1002, 7, &body).unwrap())
    }

    fn compressed_channel(length: u32, compression: u8, payload: &[u8]) -> Vec<u8> {
        let mut body = length.to_le_bytes().to_vec();
        body.extend_from_slice(&(3 | (u32::from(compression) << 16)).to_le_bytes());
        body.extend_from_slice(payload);
        let mut packet = wire::mcs_send_data(1002, 1004, &body).unwrap();
        packet[7] = 0x68;
        packet
    }

    #[test]
    fn bulk_history_is_shared_by_slow_fast_and_static_channels() {
        let mut session = bulk_session(Some(BulkCompression::Rdp5));
        // Literal Save Session Info type 0, then a literal pointer position.
        session
            .process(&compressed_slow(38, 0x61, &[0; 4]))
            .unwrap();
        session
            .process(&compressed_fast(8, 0x21, &[1, 0, 0, 0]))
            .unwrap();
        // Monitor Ready = copy(offset=4,len=4) + copy(offset=12,len=4).
        // Its header comes from the fast-path packet and its length from slow-path.
        let events = session
            .process(&compressed_channel(8, 0x21, &[0xf8, 0x91, 0xf3, 0x20]))
            .unwrap();
        assert!(events.iter().any(|event| matches!(event, Event::Send(_))));
        // A slow-path Error Info references the virtual-channel bytes just decoded.
        assert_eq!(
            session
                .process(&compressed_slow(47, 0x21, &[0xf9, 0x10]))
                .unwrap_err(),
            Error::Server(1)
        );
    }

    #[test]
    fn initial_activation_keeps_bulk_history_until_active() {
        let initial = bulk_session(Some(BulkCompression::Rdp5));
        let mut session = Session::activating(initial.config, initial.info).unwrap();
        session
            .process(&compressed_fast(8, 0x61, &[1, 0, 0, 0]))
            .unwrap();
        assert!(!session.is_active());
        assert!(session.input(Input::ReleaseAll).unwrap().is_empty());
        assert!(session.keep_awake().unwrap().is_empty());
        // Synchronize references the early graphics data, before activation finishes.
        session
            .process(&compressed_slow(31, 0x21, &[0xf8, 0x90]))
            .unwrap();
        for action in [4, 2] {
            session
                .process(&compressed_slow(20, 0x21, &[action, 0, 0, 0, 0, 0, 0, 0]))
                .unwrap();
        }
        let events = session
            .process(&compressed_slow(40, 0x21, &[0; 8]))
            .unwrap();
        assert!(events.contains(&Event::Resized {
            width: 2,
            height: 2,
        }));
        assert!(session.is_active());
        // copy(offset=32,len=4) still resolves to the first activation packet.
        session
            .process(&compressed_fast(8, 0x21, &[0xfc, 0x10]))
            .unwrap();
    }

    #[test]
    fn compressed_fast_fragments_are_decoded_before_reassembly() {
        let mut session = bulk_session(Some(BulkCompression::Rdp5));
        session.process(&compressed_fast(0x28, 0x61, &[1])).unwrap();
        // The final fragment flushes history and copies three initial zero bytes.
        session
            .process(&compressed_fast(0x18, 0xa1, &[0xf8, 0]))
            .unwrap();
        assert!(session.fragment.is_none());
        assert!(session.is_active());
    }

    #[test]
    fn compression_requires_negotiation_on_every_transport() {
        for packet in [
            compressed_fast(8, 0x20, &[1, 0, 0, 0]),
            compressed_slow(31, 0x20, &[1, 0, 0, 0]),
            compressed_channel(8, 0x20, &[1, 0, 0, 0, 0, 0, 0, 0]),
        ] {
            let mut session = bulk_session(None);
            assert_eq!(
                session.process(&packet).unwrap_err(),
                Error::Unsupported("unnegotiated bulk compression")
            );
            assert!(!session.is_active());
        }
        let mut rdp4 = bulk_session(Some(BulkCompression::Rdp4));
        assert_eq!(
            rdp4.process(&compressed_fast(8, 0x21, &[1, 0, 0, 0]))
                .unwrap_err(),
            Error::Unsupported("unnegotiated bulk compression package")
        );
        // RDP 5.0 advertises support for the earlier package as well.
        bulk_session(Some(BulkCompression::Rdp5))
            .process(&compressed_fast(8, 0x20, &[1, 0, 0, 0]))
            .unwrap();
    }

    #[test]
    fn compressed_share_data_length_matches_its_enclosing_pdu() {
        let packet = compressed_slow(31, 0x21, &[1, 0, 0, 0]);
        let mut pdu = wire::mcs_indication(&packet).unwrap().2.to_vec();
        pdu[16..18].copy_from_slice(&18u16.to_le_bytes());
        let mut session = bulk_session(Some(BulkCompression::Rdp5));
        assert_eq!(
            session.process(&share_packet(&pdu)).unwrap_err(),
            Error::Invalid("compressed Share Data length")
        );
    }

    #[test]
    fn reactivation_accepts_early_graphics_but_keeps_input_disabled() {
        let mut session = session();
        let deactivate = wire::share_control(1002, 6, &[1, 0, 0, 0, 0, 0]).unwrap();
        session.process(&share_packet(&deactivate)).unwrap();
        assert!(!session.is_active());

        let mut demand = crate::connection::tests::demand_active();
        let bitmap = demand
            .windows(4)
            .position(|bytes| bytes == [2, 0, 28, 0])
            .unwrap();
        demand[bitmap + 12..bitmap + 16].copy_from_slice(&[2, 0, 2, 0]);
        session.process(&share_packet(&demand)).unwrap();
        assert!(
            session
                .input(Input::Unicode("a".into()))
                .unwrap()
                .is_empty()
        );

        let mut payload = Vec::new();
        for field in [1_u16, 1, 0, 0, 0, 0, 1, 1, 24, 0, 4] {
            payload.extend_from_slice(&field.to_le_bytes());
        }
        payload.extend_from_slice(&[1, 2, 3, 0]);
        assert_eq!(
            session.process(&fast(&[(1, &payload)])).unwrap(),
            [Event::GraphicsChanged]
        );
        payload[22..].copy_from_slice(&[9, 8, 7, 0]);
        assert_eq!(
            session.process(&slow(2, &payload)).unwrap(),
            [Event::GraphicsChanged]
        );
        assert_eq!(&session.graphics().pixels()[..4], &[9, 8, 7, 255]);
        assert!(!session.is_active());

        for (kind, body) in [
            (31, vec![1, 0, 0, 0]),
            (20, vec![4, 0, 0, 0, 0, 0, 0, 0]),
            (20, vec![2, 0, 0, 0, 0, 0, 0, 0]),
        ] {
            assert!(session.process(&slow(kind, &body)).unwrap().is_empty());
        }
        let events = session.process(&slow(40, &[0; 8])).unwrap();
        assert!(events.contains(&Event::Resized {
            width: 2,
            height: 2
        }));
        assert!(session.is_active());
        assert!(
            !session
                .input(Input::Unicode("a".into()))
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn mcs_sender_is_checked_before_dispatch() {
        let mut session = session();
        let mut packet = slow(31, &[1, 0, 0, 0]);
        packet[8..10].copy_from_slice(&[0, 0]);
        assert_eq!(
            session.process(&packet).unwrap_err(),
            Error::Invalid("MCS server source")
        );
        assert!(!session.is_active());
    }
}
