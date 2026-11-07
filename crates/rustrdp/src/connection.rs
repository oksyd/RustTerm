//! Authenticated RDP negotiation, channel connection, licensing and activation.
//!
//! Layouts and state transitions follow [MS-RDPBCGR] sections 2.2.1 and 3.2.5.3:
//! <https://learn.microsoft.com/en-us/openspecs/windows_protocols/ms-rdpbcgr/5073f4ed-1e93-45e1-b039-6e30c385867c>.
//! The caller verifies TLS and completes CredSSP (or RDSTLS for a trusted handoff)
//! before calling [`Connector::start`].
//! No password is accepted, retained or retransmitted by this protocol engine.

mod activation;
mod gcc;
#[cfg(feature = "licensing")]
pub mod licensing;
mod message_channel;
mod redirection;

pub(crate) use redirection::payload as redirection_payload;
pub use redirection::{parse_redirection_negotiation_response, redirection_request};

pub(crate) use activation::{ActivationFinalizer, reactivate};
pub(crate) use message_channel::{Message, MessageChannel};

use crate::{
    ClientConfig, ConnectionInfo, Error, Event, GraphicsFrame, Result, Session,
    session::FrameCallback,
    wire::{self, Reader},
};

/// Security mode to authenticate on the TLS transport before starting RDP.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u32)]
pub enum SecurityProtocol {
    /// TLS with CredSSP authentication.
    Hybrid = 2,
    /// TLS/CredSSP followed by an Early User Authorization Result.
    HybridEx = 8,
    /// TLS with a server-issued RDSTLS handoff token, after a trusted redirection.
    RdsTls = 4,
}

/// Server capabilities selected by the X.224 negotiation response.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct NegotiationResponse {
    /// Security protocol to authenticate before starting the connector.
    pub protocol: SecurityProtocol,
    /// Whether initial monitor layout and extended monitor attributes may be sent.
    pub extended_client_data_supported: bool,
}

/// X.224 connection request advertising only TLS with CredSSP authentication.
/// No username cookie is sent before the server has been authenticated.
pub fn negotiation_request() -> Vec<u8> {
    let mut packet = vec![3, 0, 0, 19, 14, 0xe0, 0, 0, 0, 0, 0, 1, 0, 8, 0];
    packet.extend_from_slice(&gcc::REQUESTED_PROTOCOLS.to_le_bytes());
    packet
}

/// Validate the complete TPKT/X.224 Connection Confirm and its selected protocol.
pub fn parse_negotiation_response(packet: &[u8]) -> Result<SecurityProtocol> {
    Ok(parse_negotiation_response_details(packet)?.protocol)
}

/// Validate negotiation and retain display capability flags for
/// [`Connector::from_negotiation`].
pub fn parse_negotiation_response_details(packet: &[u8]) -> Result<NegotiationResponse> {
    negotiation_response(packet, false)
}

fn negotiation_response(packet: &[u8], redirected: bool) -> Result<NegotiationResponse> {
    let mut reader = Reader::new(packet);
    if reader.u8()? != 3 || reader.u8()? != 0 || usize::from(reader.u16_be()?) != packet.len() {
        return Err(Error::Invalid("negotiation TPKT header"));
    }
    if usize::from(reader.u8()?) + 5 != packet.len() || reader.u8()? != 0xd0 {
        return Err(Error::Invalid("X.224 Connection Confirm"));
    }
    reader.skip(4)?; // destination and source references
    if reader.u8()? != 0 {
        return Err(Error::Unsupported("X.224 transport class"));
    }
    if reader.is_empty() {
        return Err(Error::Unsupported("server without NLA negotiation"));
    }
    let kind = reader.u8()?;
    let flags = reader.u8()?;
    if reader.u16_le()? != 8 {
        return Err(Error::Invalid("RDP negotiation structure length"));
    }
    let value = reader.u32_le()?;
    reader.finish()?;
    let protocol = match (kind, value) {
        (2, 2) if !redirected => SecurityProtocol::Hybrid,
        (2, 8) if !redirected => SecurityProtocol::HybridEx,
        (2, 4) if redirected => SecurityProtocol::RdsTls,
        (2, _) if redirected => return Err(Error::Unsupported("server did not select RDSTLS")),
        (2, _) => return Err(Error::Unsupported("server selected a protocol without NLA")),
        (3, _) => return Err(Error::Server(value)),
        _ => return Err(Error::Invalid("RDP negotiation response type")),
    };
    Ok(NegotiationResponse {
        protocol,
        extended_client_data_supported: flags & 1 != 0,
    })
}

/// Actions produced by one connection step.
///
/// Send all `packets` through the authenticated transport, then process `events`.
/// Once `session` is present, publish its initial graphics and transfer all
/// subsequent frames to it; the connector has completed its work.
#[must_use = "handshake packets, events and the transferred session must be handled"]
#[derive(Default)]
pub struct HandshakeOutput {
    /// Complete outbound packets in their required wire order.
    pub packets: Vec<Vec<u8>>,
    /// Non-wire events received during activation, including clipboard updates.
    pub events: Vec<Event>,
    /// The active session, retaining early graphics, channels and bulk history.
    pub session: Option<Session>,
    /// A verified new or upgraded CAL for the caller's protected license store.
    #[cfg(feature = "licensing")]
    pub issued_license: Option<licensing::IssuedLicense>,
}

enum State {
    Ready,
    ConnectResponse,
    AttachUser,
    Join { channels: Vec<u16>, next: usize },
    License,
    DemandActive,
    Finalizing(Box<Session>),
    Connected,
    Failed,
}

/// A single-use, transport-independent RDP connection state machine.
///
/// Feed complete decrypted TPKT frames; fast-path frames are also accepted during
/// final activation. An error permanently fails the connector; drop the transport
/// and construct a new one to retry.
pub struct Connector {
    config: ClientConfig,
    protocol: SecurityProtocol,
    extended_client_data_supported: bool,
    info: ConnectionInfo,
    state: State,
    multitransport: crate::multitransport::State,
    message_channel: MessageChannel,
    #[cfg(feature = "licensing")]
    licensing: Option<licensing::LicenseClient>,
}

impl Connector {
    /// Inspect an authenticated frame for a server handoff before activation.
    /// The returned sensitive payload must not be logged. If present, discard
    /// this connector and its transport before authenticating the new session.
    pub fn server_redirection<'a>(&self, packet: &'a [u8]) -> Result<Option<&'a [u8]>> {
        if matches!(
            self.state,
            State::License | State::DemandActive | State::Finalizing(_)
        ) {
            redirection::payload(packet, self.info.io_channel_id, None)
        } else {
            Ok(None)
        }
    }
    /// Validate preferences and create a connection without performing I/O.
    ///
    /// `protocol` must be the result of [`parse_negotiation_response`]. Before
    /// calling [`Self::start`], authenticate TLS/CredSSP on the same transport.
    /// Initial multi-monitor layouts require [`Self::from_negotiation`] instead,
    /// because this constructor does not receive the server capability flags.
    pub fn new(config: ClientConfig, protocol: SecurityProtocol) -> Result<Self> {
        Self::from_negotiation(
            config,
            NegotiationResponse {
                protocol,
                extended_client_data_supported: false,
            },
        )
    }

    /// Create a connector using validated security and display negotiation.
    ///
    /// Obtain `negotiation` from [`parse_negotiation_response_details`] and
    /// authenticate TLS/CredSSP before calling [`Self::start`]. Initial multi-monitor
    /// layouts require server support for extended client data. Desktop scale
    /// remains a preference in the core block and may be ignored by older servers.
    pub fn from_negotiation(
        config: ClientConfig,
        negotiation: NegotiationResponse,
    ) -> Result<Self> {
        config.validate()?;
        if config.monitors.len() > 1 && !negotiation.extended_client_data_supported {
            return Err(Error::Unsupported("initial monitor layout"));
        }
        if gcc::requested_channels(&config).len() > config.limits.max_static_channels {
            return Err(Error::Limit("static channel count"));
        }
        let info = ConnectionInfo {
            user_id: 0,
            server_id: 1002,
            io_channel_id: 0,
            share_id: 0,
            width: config.width,
            height: config.height,
            color_depth: config.color_depth.bits_per_pixel(),
            channels: Vec::new(),
            fastpath_input: false,
            unicode_input: false,
            mouse_extended: false,
            horizontal_wheel: false,
            vc_chunk_size: 1600,
            surface_commands: 0,
            nscodec: false,
            frame_acknowledgements: false,
            message_channel_id: None,
            multitransport_flags: 0,
        };
        Ok(Self {
            config,
            protocol: negotiation.protocol,
            extended_client_data_supported: negotiation.extended_client_data_supported,
            info,
            state: State::Ready,
            multitransport: crate::multitransport::State::default(),
            message_channel: MessageChannel::default(),
            #[cfg(feature = "licensing")]
            licensing: None,
        })
    }

    /// Report an early multitransport handshake result without waiting for activation.
    ///
    /// The tunnel must authenticate the primary server's certificate before a
    /// successful status is reported. This returns the TCP message-channel reply.
    pub fn complete_multitransport(&mut self, request_id: u32, status: u32) -> Result<Vec<Event>> {
        if let State::Finalizing(session) = &mut self.state {
            return session.complete_multitransport(request_id, status);
        }
        if !matches!(self.state, State::License | State::DemandActive) {
            return Err(Error::Invalid("multitransport handshake phase"));
        }
        let request = self.multitransport.complete(request_id, status)?;
        let channel = self
            .info
            .message_channel_id
            .ok_or(Error::Invalid("missing message channel"))?;
        Ok(vec![Event::Send(wire::mcs_send_data(
            self.info.user_id,
            channel,
            &request.response(status),
        )?)])
    }

    /// Enable CAL exchange using a caller-owned stable identity and fresh entropy.
    /// Persist any issued license for subsequent connections with the same identity.
    ///
    /// Call before [`Self::start`]. A cached CAL must belong to the same client
    /// identity and authenticated server context. The caller supplies fresh
    /// CSPRNG output for every new connection, including failed-connection retries.
    #[cfg(feature = "licensing")]
    pub fn with_licensing(
        mut self,
        identity: licensing::LicenseIdentity,
        entropy: licensing::LicenseEntropy,
        cached: Option<licensing::IssuedLicense>,
    ) -> Result<Self> {
        if !matches!(self.state, State::Ready) {
            return Err(Error::Invalid("licensing configured after connector start"));
        }
        self.licensing = Some(licensing::LicenseClient::new(
            &self.config,
            identity,
            entropy,
            cached,
        )?);
        Ok(self)
    }

    /// Start after authenticated TLS/CredSSP, including the HYBRID_EX early
    /// authorization result when that protocol was selected.
    pub fn start(&mut self) -> Result<Vec<u8>> {
        let state = std::mem::replace(&mut self.state, State::Failed);
        if !matches!(state, State::Ready) {
            #[cfg(feature = "licensing")]
            {
                self.licensing = None;
            }
            return Err(Error::Invalid("connector already started"));
        }
        let packet = gcc::connect_initial(
            &self.config,
            self.protocol,
            self.extended_client_data_supported,
        )
        .and_then(|packet| {
            if packet.len() > self.config.limits.max_pdu {
                Err(Error::Limit("MCS Connect Initial"))
            } else {
                Ok(packet)
            }
        });
        if packet.is_ok() {
            self.state = State::ConnectResponse;
        } else {
            #[cfg(feature = "licensing")]
            {
                self.licensing = None;
            }
        }
        packet
    }

    /// Process one complete decrypted frame and return the next protocol actions.
    ///
    /// Preserve network order and handle each output before feeding another
    /// frame. Any error is terminal. On success, take [`HandshakeOutput::session`]
    /// when present instead of constructing or replaying a separate session.
    pub fn advance(&mut self, packet: &[u8]) -> Result<HandshakeOutput> {
        self.advance_with_callback(packet, None)
    }

    /// Process a handshake packet and expose complete frames during activation.
    ///
    /// The callback has the same borrowing and damage semantics as
    /// [`Session::process_with_frames`]. Retain acknowledgement tokens with their
    /// rendered frames and submit them once [`HandshakeOutput::session`] becomes
    /// available. This preserves early frames before later updates change them.
    pub fn advance_with_frames(
        &mut self,
        packet: &[u8],
        mut on_frame: impl FnMut(GraphicsFrame<'_>),
    ) -> Result<HandshakeOutput> {
        self.advance_with_callback(packet, Some(&mut on_frame))
    }

    fn advance_with_callback(
        &mut self,
        packet: &[u8],
        on_frame: Option<&mut FrameCallback<'_>>,
    ) -> Result<HandshakeOutput> {
        let result = self.advance_packet(packet, on_frame);
        #[cfg(feature = "licensing")]
        if result.is_err() {
            self.licensing = None;
        }
        result
    }

    fn advance_packet(
        &mut self,
        packet: &[u8],
        on_frame: Option<&mut FrameCallback<'_>>,
    ) -> Result<HandshakeOutput> {
        let state = std::mem::replace(&mut self.state, State::Failed);
        if packet.len() > self.config.limits.max_pdu {
            return Err(Error::Limit("handshake packet"));
        }
        let mut output = HandshakeOutput::default();
        if !matches!(state, State::Ready | State::Connected | State::Failed)
            && let Some(reason) = wire::mcs_disconnect(packet)?
        {
            output.events.push(Event::Disconnected { reason });
            return Ok(output);
        }
        if matches!(state, State::License | State::DemandActive)
            && let Some(message_channel) = self.info.message_channel_id
        {
            let (_, channel, data) = wire::mcs_indication(packet)?;
            if channel == message_channel {
                match self.message_channel.receive(data)? {
                    Message::Multitransport(data) => {
                        let request = self
                            .multitransport
                            .request(data, self.info.multitransport_flags)?;
                        output.events.push(Event::MultitransportRequest(request));
                    }
                    Message::Response(data) => output.packets.push(wire::mcs_send_data(
                        self.info.user_id,
                        message_channel,
                        &data,
                    )?),
                    Message::Accepted => {}
                }
                self.state = state;
                return Ok(output);
            }
            let security_length = if matches!(state, State::License) {
                4
            } else {
                0
            };
            self.message_channel
                .record_data(data.len().saturating_sub(security_length));
        }
        self.state = match state {
            State::ConnectResponse => {
                let server = gcc::connect_response_for(packet, &self.config, self.protocol)?;
                self.info.io_channel_id = server.io_channel;
                self.info.channels = server.channels;
                self.info.message_channel_id = server.message_channel;
                self.info.multitransport_flags = server.multitransport_flags;
                output.packets.push(wire::x224_data(&[4, 1, 0, 1, 0])?);
                output.packets.push(wire::x224_data(&[0x28])?);
                State::AttachUser
            }
            State::AttachUser => {
                self.info.user_id = attach_user(packet)?;
                let mut channels = vec![self.info.user_id, self.info.io_channel_id];
                channels.extend(self.info.channels.iter().map(|channel| channel.id));
                channels.extend(self.info.message_channel_id);
                if channels.iter().skip(1).any(|id| *id == self.info.user_id) {
                    return Err(Error::Invalid("MCS user/channel collision"));
                }
                output
                    .packets
                    .push(join_channel(self.info.user_id, channels[0])?);
                State::Join { channels, next: 0 }
            }
            State::Join { channels, next } => {
                confirm_join(packet, self.info.user_id, channels[next])?;
                if next + 1 == channels.len() {
                    output.packets.push(client_info(&self.config, &self.info)?);
                    State::License
                } else {
                    output
                        .packets
                        .push(join_channel(self.info.user_id, channels[next + 1])?);
                    State::Join {
                        channels,
                        next: next + 1,
                    }
                }
            }
            State::License => {
                let data = io_data(packet, &self.info)?;
                #[cfg(feature = "licensing")]
                if let Some(client) = &mut self.licensing {
                    let license = client.process(data)?;
                    if let Some(reply) = license.reply {
                        output.packets.push(wire::mcs_send_data(
                            self.info.user_id,
                            self.info.io_channel_id,
                            &reply,
                        )?);
                    }
                    output.issued_license = license.issued;
                    self.state = if license.complete {
                        State::DemandActive
                    } else {
                        State::License
                    };
                    if license.complete {
                        self.licensing = None;
                    }
                    return Ok(output);
                }
                valid_client_license(data)?;
                State::DemandActive
            }
            State::DemandActive => {
                let (source, channel, data) = wire::mcs_indication(packet)?;
                if channel != self.info.io_channel_id {
                    return Err(Error::Invalid("handshake I/O channel"));
                }
                let mut pdus = Reader::new(data);
                let demand = loop {
                    let Some(pdu) = wire::next_share_pdu(&mut pdus)? else {
                        self.state = State::DemandActive;
                        return Ok(output);
                    };
                    if !activation::before_demand(pdu)? {
                        break pdu;
                    }
                };
                output.packets = activation::activate(&self.config, &mut self.info, demand, false)?;
                if source != self.info.server_id {
                    return Err(Error::Invalid("MCS server source"));
                }
                let mut session = Session::activating(self.config.clone(), self.info.clone())?;
                session.multitransport = std::mem::take(&mut self.multitransport);
                session.message_channel = std::mem::take(&mut self.message_channel);
                session.initialize_multitransport()?;
                let mut events = Vec::new();
                if session.share_pdus(pdus.remaining(), &mut events)? {
                    events.push(Event::GraphicsChanged);
                }
                for event in events {
                    match event {
                        Event::Send(packet) => output.packets.push(packet),
                        event => output.events.push(event),
                    }
                }
                if session.is_active() {
                    output.session = Some(session);
                    State::Connected
                } else {
                    State::Finalizing(Box::new(session))
                }
            }
            State::Finalizing(mut session) => {
                for event in session.process_with_callback(packet, on_frame)? {
                    match event {
                        Event::Send(packet) => output.packets.push(packet),
                        event => output.events.push(event),
                    }
                }
                if output
                    .events
                    .iter()
                    .any(|event| matches!(event, Event::Disconnected { .. }))
                {
                    State::Failed
                } else if session.is_active() {
                    output.session = Some(*session);
                    State::Connected
                } else {
                    State::Finalizing(session)
                }
            }
            State::Ready => return Err(Error::Invalid("connector not started")),
            State::Connected => return Err(Error::Invalid("connection already complete")),
            State::Failed => return Err(Error::Invalid("failed connector")),
        };
        Ok(output)
    }
}

#[cfg(test)]
fn indication_frame(source: u16, channel: u16, payload: &[u8]) -> Result<Vec<u8>> {
    let mut data = vec![0x68];
    data.extend_from_slice(
        &source
            .checked_sub(1001)
            .ok_or(Error::Invalid("MCS source"))?
            .to_be_bytes(),
    );
    data.extend_from_slice(&channel.to_be_bytes());
    data.push(0x70);
    wire::put_per_length(&mut data, payload.len())?;
    data.extend_from_slice(payload);
    wire::x224_data(&data)
}

fn io_data<'a>(packet: &'a [u8], info: &ConnectionInfo) -> Result<&'a [u8]> {
    let (_, channel, data) = wire::mcs_indication(packet)?;
    if channel != info.io_channel_id {
        return Err(Error::Invalid("handshake I/O channel"));
    }
    Ok(data)
}

fn attach_user(packet: &[u8]) -> Result<u16> {
    let mut reader = Reader::new(wire::x224_payload(packet)?);
    let kind = reader.u8()?;
    if kind & !2 != 0x2c {
        return Err(Error::Invalid("MCS Attach User Confirm"));
    }
    let result = reader.u8()?;
    if result != 0 {
        return Err(Error::Server(u32::from(result)));
    }
    if kind & 2 == 0 {
        return Err(Error::Invalid("missing MCS user identifier"));
    }
    let user = reader
        .u16_be()?
        .checked_add(1001)
        .ok_or(Error::Invalid("MCS user identifier"))?;
    reader.finish()?;
    Ok(user)
}

fn join_channel(user: u16, channel: u16) -> Result<Vec<u8>> {
    let mut request = vec![0x38];
    request.extend_from_slice(&(user - 1001).to_be_bytes());
    request.extend_from_slice(&channel.to_be_bytes());
    wire::x224_data(&request)
}

fn confirm_join(packet: &[u8], user: u16, channel: u16) -> Result<()> {
    let mut reader = Reader::new(wire::x224_payload(packet)?);
    let kind = reader.u8()?;
    if kind & !2 != 0x3c {
        return Err(Error::Invalid("MCS Channel Join Confirm"));
    }
    let result = reader.u8()?;
    if result != 0 {
        return Err(Error::Server(u32::from(result)));
    }
    if kind & 2 == 0
        || reader.u16_be()? != user - 1001
        || reader.u16_be()? != channel
        || reader.u16_be()? != channel
    {
        return Err(Error::Invalid("MCS channel join confirmation"));
    }
    reader.finish()
}

fn utf16z(value: &str) -> Vec<u8> {
    value
        .encode_utf16()
        .chain(std::iter::once(0))
        .flat_map(u16::to_le_bytes)
        .collect()
}

fn client_info(config: &ClientConfig, info: &ConnectionInfo) -> Result<Vec<u8>> {
    let domain = utf16z(&config.domain);
    let username = utf16z(&config.username);
    let mut body = vec![0x40, 0, 0, 0]; // SEC_INFO_PKT, Basic Security Header
    body.extend_from_slice(&0u32.to_le_bytes()); // language follows the server account
    // Mouse, Unicode, maximized shell and Windows keys;
    // Disable unused video redirection and request encrypted traffic.
    // INFO_AUTOLOGON is intentionally absent: CredSSP already delegated credentials.
    let mut flags: u32 = 0x0000_0001
        | 0x0000_0002
        | 0x0000_0010
        | 0x0000_0020
        | 0x0000_0100
        | 0x0000_4000
        | 0x0002_0000
        | 0x0040_0000;
    if !config.audio_output {
        flags |= 0x0008_0000; // INFO_NOAUDIOPLAYBACK
    }
    if config.remote_app.is_some() {
        flags |= 0x0000_8000;
    } // INFO_RAIL
    if config.audio_input {
        flags |= 0x0020_0000; // INFO_AUDIOCAPTURE (MS-RDPEAI)
    }
    if let Some(compression) = config.bulk_compression {
        flags |= 0x80 | ((compression as u32) << 9);
    }
    body.extend_from_slice(&flags.to_le_bytes());
    body.extend_from_slice(&((domain.len() - 2) as u16).to_le_bytes());
    body.extend_from_slice(&((username.len() - 2) as u16).to_le_bytes());
    body.extend_from_slice(&[0; 6]); // password, alternate shell, working directory lengths
    body.extend_from_slice(&domain);
    body.extend_from_slice(&username);
    body.extend_from_slice(&[0; 6]); // three mandatory empty Unicode strings

    // The protocol engine does not know the caller's local socket/path. The
    // unspecified IPv4 address conveys that without leaking local metadata.
    let address = utf16z("0.0.0.0");
    body.extend_from_slice(&2u16.to_le_bytes()); // AF_INET
    body.extend_from_slice(&(address.len() as u16).to_le_bytes());
    body.extend_from_slice(&address);
    body.extend_from_slice(&[2, 0, 0, 0]); // empty clientDir
    // MS-RDPBCGR 2.2.1.11.1.1.1: performanceFlags requires the preceding
    // 172-byte time zone and four-byte session ID. Use an unnamed UTC time zone
    // instead of collecting the caller's local time zone or directory metadata.
    body.extend_from_slice(&[0; 172]);
    body.extend_from_slice(&0u32.to_le_bytes()); // clientSessionId (ignored)
    // Font smoothing is independent of bitmap compression and color quality.
    // Omitting this opt-in can leave Windows text without ClearType rendering.
    body.extend_from_slice(&0x0000_0080u32.to_le_bytes()); // PERF_ENABLE_FONT_SMOOTHING
    wire::mcs_send_data(info.user_id, info.io_channel_id, &body)
}

fn valid_client_license(packet: &[u8]) -> Result<()> {
    let mut reader = Reader::new(packet);
    let security = reader.u16_le()?;
    reader.skip(2)?;
    if security & 0x0080 == 0 || security & 0x0008 != 0 {
        return Err(Error::Invalid("license security header"));
    }
    let kind = reader.u8()?;
    let version = reader.u8()? & 0x0f;
    let length = usize::from(reader.u16_le()?);
    if length != packet.len() - 4 || !matches!(version, 2 | 3) {
        return Err(Error::Invalid("license preamble"));
    }
    if kind != 0xff {
        return Err(Error::Unsupported("RDS license issuance (MS-RDPELE)"));
    }
    let code = reader.u32_le()?;
    if code != 7 {
        return Err(Error::Server(code));
    }
    if reader.u32_le()? != 2 || reader.u16_le()? != 4 || reader.u16_le()? != 0 {
        return Err(Error::Invalid("valid-client license message"));
    }
    reader.finish()
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    fn hex(value: &str) -> Vec<u8> {
        value
            .split_whitespace()
            .map(|byte| u8::from_str_radix(byte, 16).unwrap())
            .collect()
    }

    // MS-RDPBCGR 4.1.4's BER/GCC structure, with TLS security (zero legacy
    // encryption), the HYBRID|HYBRID_EX echo and two allocated static channels.
    fn connect_response() -> Vec<u8> {
        hex("03 00 00 68 02 f0 80 7f 66 5e 0a 01 00 02 01 00
             30 1a 02 01 22 02 01 03 02 01 00 02 01 01 02 01 00
             02 01 01 02 03 00 ff f8 02 01 02 04 3a
             00 05 00 14 7c 00 01 2a 14 76 0a 01 01 00 01 c0 00 4d 63 44 6e 24
             01 0c 0c 00 04 00 08 00 0a 00 00 00
             03 0c 0c 00 eb 03 02 00 ec 03 ed 03
             02 0c 0c 00 00 00 00 00 00 00 00 00")
    }

    pub(crate) fn demand_active() -> Vec<u8> {
        let mut caps = hex("01 00 18 00 01 00 03 00 00 02 00 00 00 00 01 04
                            00 00 00 00 00 00 01 01
                            02 00 1c 00 20 00 01 00 01 00 01 00 00 05 d0 02
                            00 00 01 00 01 00 00 08 01 00 00 00
                            0d 00 58 00 35 01");
        caps.resize(caps.len() + 82, 0);
        let mut body = 1u32.to_le_bytes().to_vec();
        body.extend_from_slice(&4u16.to_le_bytes());
        body.extend_from_slice(&((caps.len() + 4) as u16).to_le_bytes());
        body.extend_from_slice(b"RDP\0");
        body.extend_from_slice(&[3, 0, 0, 0]);
        body.extend_from_slice(&caps);
        body.extend_from_slice(&0u32.to_le_bytes());
        wire::share_control(1002, 1, &body).unwrap()
    }

    fn server_share(kind: u8, payload: &[u8], share_id: u32) -> Vec<u8> {
        let mut request = wire::share_data(1002, 1003, share_id, kind, payload).unwrap();
        request[7] = 0x68;
        wire::mcs_indication(&request).unwrap().2.to_vec()
    }

    fn licensed_connector() -> Connector {
        let mut connector =
            Connector::new(ClientConfig::default(), SecurityProtocol::Hybrid).unwrap();
        let initial = connector.start().unwrap();
        assert_eq!(&initial[7..9], &[0x7f, 0x65]);
        let output = connector.advance(&connect_response()).unwrap();
        assert_eq!(
            output.packets,
            vec![
                wire::x224_data(&[4, 1, 0, 1, 0]).unwrap(),
                wire::x224_data(&[0x28]).unwrap()
            ]
        );
        let attached = connector
            .advance(&wire::x224_data(&[0x2e, 0, 0, 0]).unwrap())
            .unwrap();
        assert_eq!(
            attached.packets,
            [wire::x224_data(&[0x38, 0, 0, 3, 0xe9]).unwrap()]
        );
        assert!(attached.session.is_none());
        for channel in [1001u16, 1003, 1004, 1005] {
            let mut confirm = vec![0x3e, 0, 0, 0];
            confirm.extend_from_slice(&channel.to_be_bytes());
            confirm.extend_from_slice(&channel.to_be_bytes());
            let result = connector
                .advance(&wire::x224_data(&confirm).unwrap())
                .unwrap();
            assert_eq!(result.packets.len(), 1);
        }
        let license = hex("80 00 00 00 ff 03 10 00 07 00 00 00 02 00 00 00 04 00 00 00");
        let licensed = connector
            .advance(&indication_frame(1002, 1003, &license).unwrap())
            .unwrap();
        assert!(licensed.packets.is_empty());
        assert!(licensed.session.is_none());
        connector
    }

    fn activated_connector() -> Connector {
        let mut connector = licensed_connector();
        let activated = connector
            .advance(&indication_frame(1002, 1003, &demand_active()).unwrap())
            .unwrap();
        assert_eq!(activated.packets.len(), 5);
        assert!(connector.info.mouse_extended && connector.info.horizontal_wheel);
        connector
    }

    #[test]
    fn negotiation_never_downgrades_from_nla() {
        let confirm = hex("03 00 00 13 0e d0 00 00 00 00 00 02 00 08 00 02 00 00 00");
        assert_eq!(
            parse_negotiation_response(&confirm),
            Ok(SecurityProtocol::Hybrid)
        );
        let mut tls_only = confirm.clone();
        tls_only[15] = 1;
        assert!(matches!(
            parse_negotiation_response(&tls_only),
            Err(Error::Unsupported(_))
        ));
        for end in 0..confirm.len() {
            assert!(parse_negotiation_response(&confirm[..end]).is_err());
        }
        assert!(
            !parse_negotiation_response_details(&confirm)
                .unwrap()
                .extended_client_data_supported
        );
        let mut extended = confirm;
        extended[12] = 1;
        assert_eq!(
            parse_negotiation_response_details(&extended).unwrap(),
            NegotiationResponse {
                protocol: SecurityProtocol::Hybrid,
                extended_client_data_supported: true,
            }
        );
        assert_eq!(negotiation_request().len(), 19);
        assert_eq!(&negotiation_request()[15..], &[10, 0, 0, 0]);
    }

    #[test]
    fn startup_deactivation_can_be_followed_by_activation_or_disconnect() {
        for disconnect in [false, true] {
            let mut connector = licensed_connector();
            let deactivate = wire::share_control(1002, 6, &[1, 0, 0, 0, 1, 0, 0]).unwrap();
            let output = connector
                .advance(&indication_frame(1002, 1003, &deactivate).unwrap())
                .unwrap();
            assert!(
                output.packets.is_empty() && output.events.is_empty() && output.session.is_none()
            );
            assert!(matches!(connector.state, State::DemandActive));
            if disconnect {
                let output = connector
                    .advance(&wire::x224_data(&[0x21, 0x80]).unwrap())
                    .unwrap();
                assert_eq!(output.events, [Event::Disconnected { reason: 3 }]);
                assert!(matches!(connector.state, State::Failed));
            } else {
                let output = connector
                    .advance(&indication_frame(1002, 1003, &demand_active()).unwrap())
                    .unwrap();
                assert!(!output.packets.is_empty());
                assert!(matches!(connector.state, State::Finalizing(_)));
            }
        }
        let mut connector = activated_connector();
        connector.state = State::DemandActive;
        let malformed = wire::share_control(1002, 6, &[1, 0, 0, 0, 2, 0, 0]).unwrap();
        assert!(
            connector
                .advance(&indication_frame(1002, 1003, &malformed).unwrap())
                .is_err()
        );
    }

    #[test]
    fn startup_handles_coalesced_notifications_and_demand_without_losing_following_pdus() {
        let mut connector = licensed_connector();
        // Ignorable T.128 flow data must not be treated as Demand Active.
        let flow = connector
            .advance(&indication_frame(1002, 1003, &[0, 0x80, 0, 0]).unwrap())
            .unwrap();
        assert!(flow.packets.is_empty() && flow.events.is_empty() && flow.session.is_none());
        assert!(matches!(connector.state, State::DemandActive));
        let mut body = server_share(54, &0x401u32.to_le_bytes(), 0);
        body.extend(wire::share_control(1002, 6, &[1, 0, 0, 0, 0, 0]).unwrap());
        body.extend(demand_active());
        body.extend(server_share(47, &[0; 4], 0));
        let output = connector
            .advance(&indication_frame(1002, 1003, &body).unwrap())
            .unwrap();
        assert_eq!(output.packets.len(), 5);
        assert!(output.session.is_none());
        let mut finish = server_share(31, &[1, 0, 0, 0], 1);
        finish.extend(server_share(20, &[4, 0, 0, 0, 0, 0, 0, 0], 1));
        finish.extend(server_share(20, &[2, 0, 0, 0, 0, 0, 0, 0], 1));
        finish.extend(server_share(40, &[0; 8], 1));
        assert!(
            connector
                .advance(&indication_frame(1002, 1003, &finish).unwrap())
                .unwrap()
                .session
                .unwrap()
                .is_active()
        );
    }

    #[test]
    fn startup_preserves_server_error_codes_before_and_after_demand() {
        for after_demand in [false, true] {
            let mut connector = licensed_connector();
            let mut body = if after_demand {
                demand_active()
            } else {
                vec![]
            };
            body.extend(server_share(47, &0x1193u32.to_le_bytes(), 0));
            assert_eq!(
                connector
                    .advance(&indication_frame(1002, 1003, &body).unwrap())
                    .err(),
                Some(Error::Server(0x1193))
            );
            assert!(matches!(connector.state, State::Failed));
        }
        for tail in [&[5, 0, 0x17, 0, 0][..], &[18, 0, 0x17, 0, 0, 0][..]] {
            let mut connector = licensed_connector();
            let mut body = demand_active();
            body.extend_from_slice(tail);
            assert!(
                connector
                    .advance(&indication_frame(1002, 1003, &body).unwrap())
                    .is_err()
            );
            assert!(matches!(connector.state, State::Failed));
        }
    }

    #[test]
    fn microphone_support_is_advertised_independently_of_playback() {
        let connector = licensed_connector();
        for audio_input in [false, true] {
            for audio_output in [false, true] {
                let config = ClientConfig {
                    audio_input,
                    audio_output,
                    ..ClientConfig::default()
                };
                let mut packet = client_info(&config, &connector.info).unwrap();
                packet[7] = 0x68;
                let data = wire::mcs_indication(&packet).unwrap().2;
                let flags = u32::from_le_bytes(data[8..12].try_into().unwrap());
                assert_eq!(flags & 0x0020_0000 != 0, audio_input);
                assert_eq!(flags & 0x0008_0000 == 0, audio_output);
            }
        }
    }

    #[test]
    fn rdstls_core_echo_must_match_the_redirected_request() {
        let mut packet = connect_response();
        let echo = packet
            .windows(8)
            .position(|p| p == [4, 0, 8, 0, 10, 0, 0, 0])
            .unwrap()
            + 4;
        assert!(
            gcc::connect_response_for(&packet, &ClientConfig::default(), SecurityProtocol::RdsTls)
                .is_err()
        );
        packet[echo] = 4;
        assert!(
            gcc::connect_response_for(&packet, &ClientConfig::default(), SecurityProtocol::RdsTls)
                .is_ok()
        );
        assert!(gcc::connect_response(&packet, &ClientConfig::default()).is_err());
    }

    #[test]
    fn message_channel_rtt_survives_handshake_activation_and_resize() {
        let mut connector = activated_connector();
        connector.info.message_channel_id = Some(1006);
        let request = indication_frame(1002, 1006, &[0, 16, 0, 0, 6, 0, 0, 0, 1, 0]).unwrap();
        let response = wire::mcs_send_data(1001, 1006, &[0, 32, 0, 0, 6, 1, 0, 0, 0, 0]).unwrap();
        for state in [State::License, State::DemandActive] {
            connector.state = state;
            let output = connector.advance(&request).unwrap();
            assert_eq!(output.packets, std::slice::from_ref(&response));
            assert!(output.events.is_empty());
            assert!(output.session.is_none());
        }
        let activation = connector
            .advance(&indication_frame(1002, 1003, &demand_active()).unwrap())
            .unwrap();
        assert!(!activation.packets.is_empty());
        assert!(activation.session.is_none());
        assert_eq!(
            connector.advance(&request).unwrap().packets,
            std::slice::from_ref(&response)
        );
        let mut finalization = server_share(31, &[1, 0, 0, 0], 1);
        finalization.extend(server_share(20, &[4, 0, 0, 0, 0, 0, 0, 0], 1));
        finalization.extend(server_share(20, &[2, 0, 0, 0, 0, 0, 0, 0], 1));
        finalization.extend(server_share(40, &[0; 8], 1));
        let mut session = connector
            .advance(&indication_frame(1002, 1003, &finalization).unwrap())
            .unwrap()
            .session
            .unwrap();
        assert_eq!(
            session.process(&request).unwrap(),
            [Event::Send(response.clone())]
        );
        let deactivate = wire::share_control(1002, 6, &[1, 0, 0, 0, 0, 0]).unwrap();
        session
            .process(&indication_frame(1002, 1003, &deactivate).unwrap())
            .unwrap();
        assert_eq!(
            session.process(&request).unwrap(),
            [Event::Send(response.clone())]
        );
        session
            .process(&indication_frame(1002, 1003, &demand_active()).unwrap())
            .unwrap();
        assert_eq!(session.process(&request).unwrap(), [Event::Send(response)]);
    }

    #[test]
    fn initial_multi_monitor_layout_requires_server_support() {
        let config = ClientConfig {
            width: 2560,
            height: 720,
            monitors: vec![
                crate::Monitor::primary(1280, 720),
                crate::Monitor {
                    left: 1280,
                    primary: false,
                    ..crate::Monitor::primary(1280, 720)
                },
            ],
            ..ClientConfig::default()
        };
        assert!(matches!(
            Connector::new(config.clone(), SecurityProtocol::Hybrid),
            Err(Error::Unsupported("initial monitor layout"))
        ));
        assert!(
            Connector::from_negotiation(
                config,
                NegotiationResponse {
                    protocol: SecurityProtocol::Hybrid,
                    extended_client_data_supported: true,
                }
            )
            .is_ok()
        );
        // Core scaling is independent of extended monitor data support.
        let mut single = ClientConfig {
            desktop_scale: 150,
            ..ClientConfig::default()
        };
        assert!(Connector::new(single.clone(), SecurityProtocol::Hybrid).is_ok());
        single.desktop_scale = 99;
        assert!(matches!(
            Connector::new(single, SecurityProtocol::Hybrid),
            Err(Error::Invalid("desktop scale"))
        ));
    }

    #[test]
    fn negotiated_color_depth_tracks_server_reactivation() {
        let mut connector = activated_connector();
        assert_eq!(connector.info.color_depth(), 32);
        let mut demand = demand_active();
        let bitmap = demand
            .windows(4)
            .position(|bytes| bytes == [2, 0, 28, 0])
            .unwrap();
        demand[bitmap + 4..bitmap + 6].copy_from_slice(&16u16.to_le_bytes());
        activation::reactivate(&connector.config, &mut connector.info, &demand).unwrap();
        assert_eq!(connector.info.color_depth(), 16);
        demand[bitmap + 4..bitmap + 6].copy_from_slice(&12u16.to_le_bytes());
        assert!(activation::reactivate(&connector.config, &mut connector.info, &demand).is_err());
        assert_eq!(connector.info.color_depth(), 16);
    }

    #[test]
    fn failed_or_repeated_start_permanently_closes_the_connector() {
        let mut connector =
            Connector::new(ClientConfig::default(), SecurityProtocol::Hybrid).unwrap();
        connector.start().unwrap();
        assert!(connector.start().is_err());
        assert!(connector.advance(&connect_response()).is_err());

        let config = ClientConfig {
            limits: crate::Limits {
                max_pdu: 7,
                ..crate::Limits::default()
            },
            ..ClientConfig::default()
        };
        let mut connector = Connector::new(config, SecurityProtocol::Hybrid).unwrap();
        assert_eq!(connector.start(), Err(Error::Limit("MCS Connect Initial")));
        assert!(connector.start().is_err());
    }

    #[cfg(feature = "licensing")]
    #[test]
    fn outer_handshake_errors_discard_unused_licensing_entropy() {
        for started in [false, true] {
            let mut connector = Connector::new(ClientConfig::default(), SecurityProtocol::Hybrid)
                .unwrap()
                .with_licensing(
                    licensing::LicenseIdentity {
                        platform_id: 0x0401_0000,
                        hardware_id: [1; 16],
                    },
                    licensing::LicenseEntropy::new([2; 32], [3; 48]),
                    None,
                )
                .unwrap();
            connector.start().unwrap();
            if started {
                assert!(connector.start().is_err());
            } else {
                assert!(connector.advance(&[]).is_err());
            }
            assert!(connector.licensing.is_none());
            assert!(connector.advance(&connect_response()).is_err());
        }
    }

    #[test]
    fn gcc_rejects_security_echo_changes_and_truncation() {
        let config = ClientConfig::default();
        let valid = connect_response();
        let server = gcc::connect_response(&valid, &config).unwrap();
        let io = server.io_channel;
        let channels = server.channels;
        assert_eq!(io, 1003);
        assert_eq!(
            channels
                .iter()
                .map(|channel| (channel.id, channel.name.as_str()))
                .collect::<Vec<_>>(),
            vec![(1004, "cliprdr"), (1005, "drdynvc")]
        );
        let mut changed = valid.clone();
        let echo = changed
            .windows(4)
            .position(|bytes| bytes == [10, 0, 0, 0])
            .unwrap();
        changed[echo] = 1;
        assert!(gcc::connect_response(&changed, &config).is_err());
        let mut advisory_length = valid.clone();
        advisory_length[45] = 0;
        assert!(gcc::connect_response(&advisory_length, &config).is_ok());
        for end in 0..valid.len() {
            assert!(gcc::connect_response(&valid[..end], &config).is_err());
        }
    }

    #[test]
    fn remote_app_uses_rail_flag_and_channel_without_an_alternate_shell() {
        let config = ClientConfig {
            remote_app: Some(crate::remote_app::RemoteApp {
                program: "||app".into(),
                ..Default::default()
            }),
            dynamic_resolution: false,
            ..Default::default()
        };
        assert!(gcc::requested_channels(&config).contains(&"rail"));
        let mut connector = Connector::new(config.clone(), SecurityProtocol::Hybrid).unwrap();
        connector.info.user_id = 1001;
        connector.info.io_channel_id = 1003;
        let packet = client_info(&config, &connector.info).unwrap();
        let mut mcs = Reader::new(wire::x224_payload(&packet).unwrap());
        mcs.skip(6).unwrap();
        let length = wire::per_length(&mut mcs).unwrap();
        let mut info = Reader::new(mcs.take(length).unwrap());
        info.skip(8).unwrap(); // security header and codepage
        let flags = info.u32_le().unwrap();
        assert_ne!(flags & 0x8000, 0);
        assert_eq!(flags & 0x0200_0000, 0); // INFO_HIDEF_RAIL_SUPPORTED
        info.skip(6).unwrap(); // domain, username, password lengths
        assert_eq!(info.u16_le().unwrap(), 0); // alternate shell
        assert_eq!(info.u16_le().unwrap(), 0); // working directory belongs in Execute
    }

    #[test]
    fn client_info_contains_no_password_or_autologon() {
        for audio_output in [false, true] {
            let config = ClientConfig {
                username: "user\u{100}".into(),
                domain: "example".into(),
                audio_output,
                ..ClientConfig::default()
            };
            let mut connector = Connector::new(config.clone(), SecurityProtocol::Hybrid).unwrap();
            connector.info.user_id = 1001;
            connector.info.io_channel_id = 1003;
            let mut packet = client_info(&config, &connector.info).unwrap();
            packet[7] = 0x68;
            let mut reader = Reader::new(wire::mcs_indication(&packet).unwrap().2);
            assert_eq!(reader.u32_le().unwrap(), 0x40);
            reader.skip(4).unwrap();
            let flags = reader.u32_le().unwrap();
            assert_eq!(flags & 0x88, 0);
            assert_eq!(flags & 0x0008_0000 != 0, !audio_output);
            let domain_length = reader.u16_le().unwrap();
            let username_length = reader.u16_le().unwrap();
            assert_eq!(reader.u16_le().unwrap(), 0);
            reader
                .skip(4 + usize::from(domain_length + username_length) + 4)
                .unwrap();
            assert_eq!(reader.take(6).unwrap(), &[0; 6]);
        }
    }

    #[test]
    fn client_info_requests_font_smoothing_for_every_graphics_quality() {
        for color_depth in [crate::ColorDepth::Bpp16, crate::ColorDepth::Bpp32] {
            for allow_lossy_bitmaps in [false, true] {
                let config = ClientConfig {
                    username: "operator\u{100}".into(),
                    domain: "example".into(),
                    color_depth,
                    allow_lossy_bitmaps,
                    bulk_compression: allow_lossy_bitmaps.then_some(crate::BulkCompression::Rdp5),
                    ..ClientConfig::default()
                };
                let mut connector =
                    Connector::new(config.clone(), SecurityProtocol::Hybrid).unwrap();
                connector.info.user_id = 1001;
                connector.info.io_channel_id = 1003;
                let mut packet = client_info(&config, &connector.info).unwrap();
                packet[7] = 0x68;
                let mut reader = Reader::new(wire::mcs_indication(&packet).unwrap().2);
                reader.skip(12).unwrap(); // security header, code page, info flags
                let mut strings_length = 0;
                for _ in 0..5 {
                    strings_length += usize::from(reader.u16_le().unwrap()) + 2;
                }
                reader.skip(strings_length).unwrap();
                assert_eq!(reader.u16_le().unwrap(), 2); // AF_INET
                let address_length = usize::from(reader.u16_le().unwrap());
                assert_eq!(reader.take(address_length).unwrap(), utf16z("0.0.0.0"));
                let directory_length = usize::from(reader.u16_le().unwrap());
                assert_eq!(reader.take(directory_length).unwrap(), &[0; 2]);
                assert_eq!(reader.take(172).unwrap(), &[0; 172]);
                assert_eq!(reader.u32_le().unwrap(), 0); // clientSessionId
                assert_eq!(reader.u32_le().unwrap(), 0x80); // no shell-disable flags
                reader.finish().unwrap();
            }
        }
    }

    #[cfg(feature = "licensing")]
    #[test]
    fn connector_wraps_optional_license_exchange_and_clears_it_on_acceptance() {
        let mut connector = Connector::new(ClientConfig::default(), SecurityProtocol::Hybrid)
            .unwrap()
            .with_licensing(
                licensing::LicenseIdentity {
                    platform_id: 0x0401_0000,
                    hardware_id: [1; 16],
                },
                licensing::LicenseEntropy::new([2; 32], [3; 48]),
                None,
            )
            .unwrap();
        connector.info.user_id = 1001;
        connector.info.io_channel_id = 1003;
        connector.state = State::License;
        let request = [
            b"\x80\0\0\0".as_slice(),
            include_bytes!("connection/licensing/fixtures/ms-rdpele-server-request.bin"),
        ]
        .concat();
        let output = connector
            .advance(&indication_frame(1002, 1003, &request).unwrap())
            .unwrap();
        assert!(matches!(connector.state, State::License));
        assert!(output.session.is_none());
        assert_eq!(output.packets.len(), 1);
        let mut reply = output.packets[0].clone();
        assert_eq!(reply[7], 0x64); // MCS SendDataRequest, not a server indication.
        reply[7] = 0x68;
        let (user, channel, data) = wire::mcs_indication(&reply).unwrap();
        assert_eq!((user, channel), (1001, 1003));
        assert_eq!(&data[..5], &[0x80, 0, 0, 0, 0x13]);

        let accepted = hex("80 00 00 00 ff 03 10 00 07 00 00 00 02 00 00 00 04 00 00 00");
        let output = connector
            .advance(&indication_frame(1002, 1003, &accepted).unwrap())
            .unwrap();
        assert!(output.packets.is_empty());
        assert!(matches!(connector.state, State::DemandActive));
        assert!(connector.licensing.is_none());
        let activating = connector
            .advance(&indication_frame(1002, 1003, &demand_active()).unwrap())
            .unwrap();
        assert_eq!(activating.packets.len(), 5);
        assert!(activating.session.is_none());
        assert!(matches!(connector.state, State::Finalizing(_)));
    }

    #[test]
    fn optional_compression_and_color_capabilities_match_the_selected_policy() {
        let mut connector = activated_connector();
        for enabled in [false, true] {
            let config = ClientConfig {
                bulk_compression: enabled.then_some(crate::BulkCompression::Rdp5),
                allow_lossy_bitmaps: enabled,
                ..ClientConfig::default()
            };
            let mut client = client_info(&config, &connector.info).unwrap();
            client[7] = 0x68;
            let data = wire::mcs_indication(&client).unwrap().2;
            let flags = u32::from_le_bytes(data[8..12].try_into().unwrap());
            assert_eq!(flags & 0x1e80, if enabled { 0x0280 } else { 0 });

            let mut confirm =
                activation::activate(&config, &mut connector.info, &demand_active(), false)
                    .unwrap()
                    .remove(0);
            confirm[7] = 0x68;
            let mut share = Reader::new(wire::mcs_indication(&confirm).unwrap().2);
            share.skip(12).unwrap();
            let descriptor = usize::from(share.u16_le().unwrap());
            let length = usize::from(share.u16_le().unwrap());
            share.skip(descriptor).unwrap();
            let mut caps = Reader::new(share.take(length).unwrap());
            let count = caps.u16_le().unwrap();
            caps.skip(2).unwrap();
            let mut found = 0;
            for _ in 0..count {
                let kind = caps.u16_le().unwrap();
                let length = usize::from(caps.u16_le().unwrap());
                let body = caps.take(length - 4).unwrap();
                match kind {
                    2 => {
                        assert_eq!(body[19], if enabled { 0x0e } else { 8 });
                        found += 1;
                    }
                    20 => {
                        assert_eq!(&body[..4], &u32::from(enabled).to_le_bytes());
                        found += 1;
                    }
                    _ => {}
                }
            }
            assert_eq!(found, 2);
            caps.finish().unwrap();
        }
    }

    #[test]
    fn activation_frame_callbacks_preserve_complete_pixels_and_receipts() {
        let mut connector = activated_connector();
        connector.config.frame_acknowledgements = std::num::NonZeroU32::new(3);
        connector.info.width = 2;
        connector.info.height = 2;
        connector.info.surface_commands = 0x52;
        connector.info.frame_acknowledgements = true;
        connector.state = State::Finalizing(Box::new(
            Session::activating(connector.config.clone(), connector.info.clone()).unwrap(),
        ));
        // One complete frame, followed by the first pixels of the next frame.
        let surface = hex("04 00 00 00 07 00 00 00
             01 00 00 00 00 00 01 00 01 00 20 00 00 00 01 00 01 00 04 00 00 00 01 02 03 00
             04 00 01 00 07 00 00 00
             04 00 00 00 08 00 00 00
             01 00 00 00 00 00 01 00 01 00 20 00 00 00 01 00 01 00 04 00 00 00 04 05 06 00");
        let mut packet = vec![0, (surface.len() + 5) as u8, 4];
        packet.extend_from_slice(&(surface.len() as u16).to_le_bytes());
        packet.extend_from_slice(&surface);
        let mut receipt = None;
        let early = connector
            .advance_with_frames(&packet, |frame| {
                assert_eq!(frame.id, 7);
                assert_eq!(frame.bits_per_pixel, 32);
                assert_eq!(&frame.graphics.pixels()[..4], &[1, 2, 3, 255]);
                assert!(frame.damage.is_some());
                receipt = frame.acknowledgement;
            })
            .unwrap();
        let receipt = receipt.unwrap();
        assert!(early.session.is_none());
        assert!(early.events.contains(&Event::GraphicsFrameEnd {
            frame_id: 7,
            acknowledgement: Some(receipt.clone()),
        }));
        assert!(early.packets.is_empty());

        let mut body = server_share(31, &[1, 0, 0, 0], 1);
        body.extend(server_share(20, &[4, 0, 0, 0, 0, 0, 0, 0], 1));
        body.extend(server_share(20, &[2, 0, 0, 0, 0, 0, 0, 0], 1));
        body.extend(server_share(40, &[0; 8], 1));
        let output = connector
            .advance_with_frames(&indication_frame(1002, 1003, &body).unwrap(), |_| {
                panic!("finalization contains no complete frame")
            })
            .unwrap();
        let mut session = output.session.unwrap();
        assert!(session.graphics_frame_in_progress());
        assert_eq!(&session.graphics().pixels()[..4], &[4, 5, 6, 255]);
        assert_eq!(session.acknowledge_frame(&receipt).unwrap().len(), 1);
        let end = hex("00 0d 04 08 00 04 00 01 00 08 00 00 00");
        let mut next = None;
        session
            .process_with_frames(&end, |frame| {
                assert_eq!(frame.id, 8);
                assert_eq!(&frame.graphics.pixels()[..4], &[4, 5, 6, 255]);
                next = frame.acknowledgement;
            })
            .unwrap();
        assert_eq!(session.acknowledge_frame(&next.unwrap()).unwrap().len(), 1);
    }

    #[test]
    fn activation_keeps_graphics_coalesced_after_font_map() {
        let mut connector = activated_connector();
        let initial_graphics = vec![0, 5, 5, 0, 0];
        let early = connector.advance(&initial_graphics).unwrap();
        assert!(early.session.is_none());
        assert_eq!(early.events, [Event::GraphicsChanged]);
        let mut body = server_share(31, &[1, 0, 0, 0], 1);
        body.extend(server_share(20, &[4, 0, 0, 0, 0, 0, 0, 0], 1));
        body.extend(server_share(20, &[2, 0, 0, 0, 0, 0, 0, 0], 1));
        body.extend(server_share(40, &[0; 8], 1));
        let update = server_share(2, &[3, 0, 0, 0], 1);
        body.extend_from_slice(&update);
        let result = connector
            .advance(&indication_frame(1002, 1003, &body).unwrap())
            .unwrap();
        let session = result.session.unwrap();
        assert!(session.is_active());
        assert!(session.graphics().hide_local_pointer());
        assert!(result.events.contains(&Event::GraphicsChanged));
    }

    #[test]
    fn activation_reports_server_errors_without_advancing_state() {
        let connector = activated_connector();
        let mut finalizer = ActivationFinalizer::new(&connector.info);
        assert!(!finalizer.advance(&server_share(47, &[0; 4], 0)).unwrap());
        assert!(
            !finalizer
                .advance(&server_share(54, &[1, 4, 0, 0], 0))
                .unwrap()
        );
        assert_eq!(
            finalizer.advance(&server_share(47, &[7, 0, 0, 0], 0)),
            Err(Error::Server(7))
        );
        assert!(finalizer.advance(&server_share(40, &[0; 8], 1)).is_err());
        assert!(
            !finalizer
                .advance(&server_share(31, &[1, 0, 0, 0], 1))
                .unwrap()
        );
    }

    #[test]
    fn invalid_demand_does_not_commit_new_dimensions() {
        let mut connector = activated_connector();
        let mut demand = demand_active();
        let bitmap = demand
            .windows(4)
            .position(|bytes| bytes == [2, 0, 28, 0])
            .unwrap();
        demand[bitmap + 12..bitmap + 14].copy_from_slice(&u16::MAX.to_le_bytes());
        let before = connector.info.width;
        assert!(reactivate(&connector.config, &mut connector.info, &demand).is_err());
        assert_eq!(connector.info.width, before);
    }

    #[test]
    fn license_issuance_is_explicitly_unsupported() {
        assert_eq!(
            valid_client_license(&[0x80, 0, 0, 0, 1, 3, 4, 0]),
            Err(Error::Unsupported("RDS license issuance (MS-RDPELE)"))
        );
    }
}
