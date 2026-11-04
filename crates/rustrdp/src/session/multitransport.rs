use super::{Event, GraphicsFrame, Session, State};
use crate::{
    Error, Result,
    connection::Message,
    multitransport::{self, TransportProtocol, TunnelPdu},
    wire,
};

impl Session {
    pub(crate) fn initialize_multitransport(&mut self) -> Result<()> {
        for protocol in [TransportProtocol::Reliable, TransportProtocol::Lossy] {
            if self.multitransport.ready(protocol) {
                self.channels.tunnel_ready(
                    protocol,
                    self.info.multitransport_flags & multitransport::SOFT_SYNC != 0,
                )?;
            }
        }
        Ok(())
    }

    pub(super) fn receive_message_channel(&mut self, data: &[u8]) -> Result<Vec<Event>> {
        match self.message_channel.receive(data)? {
            Message::Multitransport(data) => {
                let request = self
                    .multitransport
                    .request(data, self.info.multitransport_flags)?;
                Ok(vec![Event::MultitransportRequest(request)])
            }
            Message::Response(data) => {
                let channel = self
                    .info
                    .message_channel_id
                    .ok_or(Error::Invalid("missing message channel"))?;
                Ok(vec![Event::Send(wire::mcs_send_data(
                    self.info.user_id,
                    channel,
                    &data,
                )?)])
            }
            Message::Accepted => Ok(Vec::new()),
        }
    }

    /// Reports the result of establishing and authenticating a requested tunnel.
    ///
    /// Pass zero only after a successful encrypted Tunnel Create Response.
    /// Failure HRESULTs leave all existing TCP channels available.
    pub fn complete_multitransport(&mut self, request_id: u32, status: u32) -> Result<Vec<Event>> {
        if matches!(self.state, State::Closed) {
            return Err(Error::Invalid("closed session"));
        }
        let channel = self
            .info
            .message_channel_id
            .ok_or(Error::Invalid("missing message channel"))?;
        let request = self.multitransport.complete(request_id, status)?;
        if status & 0x8000_0000 == 0 {
            self.channels.tunnel_ready(
                request.protocol,
                self.info.multitransport_flags & multitransport::SOFT_SYNC != 0,
            )?;
        }
        Ok(vec![Event::Send(wire::mcs_send_data(
            self.info.user_id,
            channel,
            &request.response(status),
        )?)])
    }

    /// Processes a decrypted tunnel PDU, exposing completed graphics frames.
    ///
    /// The host must authenticate the tunnel against the primary connection's
    /// certificate before calling this method. Errors terminate this session.
    pub fn process_tunnel_with_frames(
        &mut self,
        protocol: TransportProtocol,
        packet: &[u8],
        mut on_frame: impl FnMut(GraphicsFrame<'_>),
    ) -> Result<Vec<Event>> {
        let result = (|| {
            if !self.multitransport.ready(protocol) || matches!(self.state, State::Closed) {
                return Err(Error::Invalid("unestablished multitransport tunnel"));
            }
            let TunnelPdu::Data { payload, .. } = multitransport::decode_pdu(packet)? else {
                return Err(Error::Invalid("unexpected tunnel create response"));
            };
            if payload.is_empty() {
                return Ok(Vec::new());
            }
            if !self.channels.tunnel_active(protocol) {
                self.multitransport.buffered_bytes = self
                    .multitransport
                    .buffered_bytes
                    .checked_add(payload.len())
                    .filter(|bytes| *bytes <= self.config.limits.max_channel_message)
                    .ok_or(Error::Backpressure)?;
                if self.multitransport.buffered.len() >= 4096 {
                    return Err(Error::Backpressure);
                }
                self.multitransport
                    .buffered
                    .push_back((protocol, payload.to_vec()));
                return Ok(Vec::new());
            }
            let actions = self.channels.receive_tunnel(protocol, payload)?;
            let mut events = Vec::new();
            self.channel_events_with_frames(actions, &mut events, &mut Some(&mut on_frame))?;
            Ok(events)
        })();
        if result.is_err() {
            self.state = State::Closed;
            self.graphics.cancel_frame();
            self.frame_acknowledgements.reset(false);
        }
        result
    }

    pub(super) fn flush_multitransport(
        &mut self,
        events: &mut Vec<Event>,
        on_frame: &mut Option<&mut super::FrameCallback<'_>>,
    ) -> Result<()> {
        let count = self.multitransport.buffered.len();
        for _ in 0..count {
            let Some((protocol, bytes)) = self.multitransport.buffered.pop_front() else {
                break;
            };
            if !self.channels.tunnel_active(protocol) {
                self.multitransport.buffered.push_back((protocol, bytes));
                continue;
            }
            self.multitransport.buffered_bytes -= bytes.len();
            let actions = self.channels.receive_tunnel(protocol, &bytes)?;
            self.channel_events_with_frames(actions, events, on_frame)?;
        }
        Ok(())
    }

    /// Closes a transport which failed after, or while, being established.
    ///
    /// After channel migration, losing a tunnel is terminal: silently moving
    /// back to TCP would lose or duplicate unacknowledged channel messages.
    pub fn close_multitransport(&mut self, protocol: TransportProtocol) -> Result<Vec<Event>> {
        self.multitransport.close(protocol);
        if self.channels.tunnel_closed(protocol)? {
            self.state = State::Closed;
            return Err(Error::Invalid(
                "multitransport closed after channel migration",
            ));
        }
        Ok(Vec::new())
    }
}
