//! Multitransport tunnel framing and dynamic-channel migration (MS-RDPEMT/EDYC).
//!
//! Tunnel PDUs must only be exchanged through authenticated TLS (reliable UDP)
//! or DTLS (lossy UDP). The cookie authenticates the existing RDP session; it
//! does not replace encryption or verification of the server certificate.

use crate::{Error, Result, wire::Reader};

mod autodetect;
pub use autodetect::{BandwidthDetection, NetworkCharacteristics};

pub(crate) const CLIENT_FLAGS: u32 = 0x0000_0205;
pub(crate) const SOFT_SYNC: u32 = 0x0000_0200;

/// The UDP tunnel's delivery semantics.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum TransportProtocol {
    /// Ordered, retransmitted packets protected by TLS.
    Reliable,
    /// Best-effort datagrams protected by DTLS.
    Lossy,
}

impl TransportProtocol {
    /// The tunnel identifier used in dynamic-channel soft-sync messages.
    pub fn tunnel_type(self) -> u32 {
        match self {
            Self::Reliable => 1,
            Self::Lossy => 3,
        }
    }

    fn from_tunnel_type(value: u32) -> Result<Self> {
        match value {
            1 => Ok(Self::Reliable),
            3 => Ok(Self::Lossy),
            _ => Err(Error::Unsupported("multitransport tunnel type")),
        }
    }
}

/// A server request to create an additional authenticated transport.
#[derive(Clone, PartialEq, Eq)]
pub struct Request {
    /// Identifier to echo in the tunnel create request and TCP response.
    pub id: u32,
    /// Requested transport type.
    pub protocol: TransportProtocol,
    /// Session-binding secret delivered over the authenticated main connection.
    pub cookie: [u8; 16],
}

impl std::fmt::Debug for Request {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Request")
            .field("id", &self.id)
            .field("protocol", &self.protocol)
            .finish_non_exhaustive()
    }
}

impl Request {
    /// Decodes an Initiate Multitransport Request, excluding its security header.
    ///
    /// # Errors
    /// Returns an error for a truncated request or an unknown transport type.
    pub fn decode(data: &[u8]) -> Result<Self> {
        let mut reader = Reader::new(data);
        let id = reader.u32_le()?;
        let protocol = match reader.u16_le()? {
            1 => TransportProtocol::Reliable,
            2 => TransportProtocol::Lossy,
            _ => return Err(Error::Unsupported("requested multitransport protocol")),
        };
        let reserved = reader.u16_le()?;
        let mut cookie = [0; 16];
        cookie.copy_from_slice(reader.take(16)?);
        // Windows may append optional connection metadata when this field is
        // nonzero. The authenticated MCS envelope already bounds that data.
        if reserved == 0 {
            reader.finish()?;
        }
        Ok(Self {
            id,
            protocol,
            cookie,
        })
    }

    /// Encodes the first message to send after the tunnel TLS/DTLS handshake.
    pub fn create_pdu(&self) -> Vec<u8> {
        let mut data = vec![0, 24, 0, 4];
        data.extend_from_slice(&self.id.to_le_bytes());
        data.extend_from_slice(&[0; 4]);
        data.extend_from_slice(&self.cookie);
        data
    }

    /// Encodes a TCP message-channel response including its security header.
    ///
    /// Send success only after the encrypted tunnel's create response succeeds.
    pub fn response(&self, status: u32) -> Vec<u8> {
        let mut data = vec![4, 0, 0, 0];
        data.extend_from_slice(&self.id.to_le_bytes());
        data.extend_from_slice(&status.to_le_bytes());
        data
    }
}

/// A completely framed and decrypted tunnel message.
#[derive(Debug, PartialEq, Eq)]
pub enum TunnelPdu<'a> {
    /// Server response to the tunnel create request (HRESULT).
    Created(u32),
    /// Dynamic-channel bytes with optional network-detection subheaders.
    Data {
        /// Complete, individually length-prefixed subheaders.
        subheaders: &'a [u8],
        /// Dynamic-channel PDU bytes.
        payload: &'a [u8],
    },
}

/// Reads the tunnel frame size without consuming partial input.
///
/// # Errors
/// Returns an error if the action, flags, or header length is invalid.
pub fn frame_size(data: &[u8]) -> Result<Option<usize>> {
    if data.len() < 4 {
        return Ok(None);
    }
    if data[0] >> 4 != 0 || data[0] & 15 > 2 || data[3] < 4 {
        return Err(Error::Invalid("multitransport tunnel header"));
    }
    Ok(Some(
        usize::from(data[3]) + usize::from(u16::from_le_bytes([data[1], data[2]])),
    ))
}

/// Decodes one decrypted server tunnel PDU.
///
/// # Errors
/// Rejects truncation, trailing data, malformed subheaders and unexpected actions.
pub fn decode_pdu(data: &[u8]) -> Result<TunnelPdu<'_>> {
    let length = frame_size(data)?.ok_or(Error::Truncated)?;
    if data.len() < length {
        return Err(Error::Truncated);
    }
    if data.len() != length {
        return Err(Error::Invalid("trailing multitransport data"));
    }
    let header = usize::from(data[3]);
    let payload = &data[header..];
    match data[0] {
        1 => {
            if header != 4 || payload.len() != 4 {
                return Err(Error::Invalid("multitransport create response length"));
            }
            Ok(TunnelPdu::Created(u32::from_le_bytes(
                payload.try_into().map_err(|_| Error::Truncated)?,
            )))
        }
        2 => {
            let subheaders = &data[4..header];
            let mut reader = Reader::new(subheaders);
            while !reader.is_empty() {
                let size = usize::from(reader.u8()?);
                if size < 2 {
                    return Err(Error::Invalid("multitransport subheader length"));
                }
                reader.skip(size - 1)?;
            }
            Ok(TunnelPdu::Data {
                subheaders,
                payload,
            })
        }
        _ => Err(Error::Invalid("unexpected multitransport tunnel action")),
    }
}

/// Encodes a tunnel data PDU after successful tunnel creation.
///
/// # Errors
/// Returns an error if the payload exceeds the protocol's 16-bit length field.
pub fn data_pdu(payload: &[u8]) -> Result<Vec<u8>> {
    let size = u16::try_from(payload.len()).map_err(|_| Error::Limit("multitransport payload"))?;
    let mut data = Vec::with_capacity(payload.len() + 4);
    data.push(2);
    data.extend_from_slice(&size.to_le_bytes());
    data.push(4);
    data.extend_from_slice(payload);
    Ok(data)
}

/// The server's assignment of dynamic channels to established UDP tunnels.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SoftSync {
    /// Explicit assignments; an empty list means the server omitted channel lists.
    pub channels: Vec<(TransportProtocol, Vec<u32>)>,
}

impl SoftSync {
    /// Decodes a complete DYNVC_SOFT_SYNC_REQUEST PDU.
    ///
    /// # Errors
    /// Rejects duplicate tunnels/channels, invalid lengths or unflushed TCP data.
    pub fn decode(data: &[u8]) -> Result<Self> {
        let mut reader = Reader::new(data);
        if reader.u8()? != 0x80 || reader.u8()? != 0 {
            return Err(Error::Invalid("soft-sync header"));
        }
        let length =
            usize::try_from(reader.u32_le()?).map_err(|_| Error::Limit("soft-sync length"))?;
        if length != data.len() - 2 {
            return Err(Error::Invalid("soft-sync length"));
        }
        let flags = reader.u16_le()?;
        if flags & 1 == 0 || flags & !3 != 0 {
            return Err(Error::Invalid("soft-sync flags"));
        }
        let count = usize::from(reader.u16_le()?);
        if count > 2 {
            return Err(Error::Limit("soft-sync tunnel count"));
        }
        let mut channels = Vec::with_capacity(count);
        if flags & 2 != 0 {
            let mut seen = std::collections::BTreeSet::new();
            for _ in 0..count {
                let tunnel = TransportProtocol::from_tunnel_type(reader.u32_le()?)?;
                if channels.iter().any(|(prior, _)| *prior == tunnel) {
                    return Err(Error::Invalid("duplicate soft-sync tunnel"));
                }
                let channel_count = usize::from(reader.u16_le()?);
                if channel_count > 256 {
                    return Err(Error::Limit("soft-sync channel count"));
                }
                let mut ids = Vec::with_capacity(channel_count);
                for _ in 0..channel_count {
                    let id = reader.u32_le()?;
                    if !seen.insert(id) {
                        return Err(Error::Invalid("duplicate soft-sync channel"));
                    }
                    ids.push(id);
                }
                channels.push((tunnel, ids));
            }
        }
        reader.finish()?;
        Ok(Self { channels })
    }

    /// Encodes the tunnels the client will use after flushing its TCP response.
    ///
    /// The caller must ensure every listed tunnel has completed creation.
    pub fn response(tunnels: &[TransportProtocol]) -> Result<Vec<u8>> {
        if tunnels.len() > 2 || (tunnels.len() == 2 && tunnels[0] == tunnels[1]) {
            return Err(Error::Invalid("soft-sync response tunnels"));
        }
        let mut data = vec![0x90, 0];
        data.extend_from_slice(&(tunnels.len() as u32).to_le_bytes());
        for tunnel in tunnels {
            data.extend_from_slice(&tunnel.tunnel_type().to_le_bytes());
        }
        Ok(data)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn documented_create_response_and_data_roundtrip() {
        assert_eq!(
            decode_pdu(&[1, 4, 0, 4, 0, 0, 0, 0]).unwrap(),
            TunnelPdu::Created(0)
        );
        let packet = data_pdu(b"channel").unwrap();
        assert_eq!(
            decode_pdu(&packet).unwrap(),
            TunnelPdu::Data {
                subheaders: &[],
                payload: b"channel"
            }
        );
        for end in 0..packet.len() {
            assert!(decode_pdu(&packet[..end]).is_err());
        }
    }

    #[test]
    fn framing_is_bounded_and_rejects_invalid_subheaders() {
        assert_eq!(frame_size(&[2, 255, 255, 255]).unwrap(), Some(65790));
        assert!(decode_pdu(&[2, 0, 0, 5, 0]).is_err());
        assert!(decode_pdu(&[2, 0, 0, 6, 3, 0]).is_err());
        assert!(data_pdu(&vec![0; 65536]).is_err());
    }

    #[test]
    fn request_keeps_session_cookie_out_of_debug() {
        let request = Request {
            id: 9,
            protocol: TransportProtocol::Reliable,
            cookie: [42; 16],
        };
        let mut raw = vec![9, 0, 0, 0, 1, 0, 0, 0];
        raw.extend_from_slice(&[42; 16]);
        assert_eq!(Request::decode(&raw).unwrap(), request);
        assert!(!format!("{request:?}").contains("42"));
        assert_eq!(&request.create_pdu()[12..], &[42; 16]);
        assert_eq!(request.response(0), [4, 0, 0, 0, 9, 0, 0, 0, 0, 0, 0, 0]);
    }

    #[test]
    fn soft_sync_preserves_assignments_and_rejects_duplicates() {
        let mut data = vec![
            0x80, 0, 18, 0, 0, 0, 3, 0, 1, 0, 1, 0, 0, 0, 1, 0, 7, 0, 0, 0,
        ];
        assert_eq!(
            SoftSync::decode(&data).unwrap().channels,
            [(TransportProtocol::Reliable, vec![7])]
        );
        data[14] = 2;
        data[2] = 22;
        data.extend_from_slice(&7u32.to_le_bytes());
        assert!(SoftSync::decode(&data).is_err());
        assert_eq!(
            SoftSync::response(&[TransportProtocol::Reliable]).unwrap(),
            [0x90, 0, 1, 0, 0, 0, 1, 0, 0, 0]
        );
    }
}

#[derive(Default)]
pub(crate) struct State {
    pending: std::collections::BTreeMap<u32, Request>,
    ready: std::collections::BTreeSet<TransportProtocol>,
    pub(crate) buffered: std::collections::VecDeque<(TransportProtocol, Vec<u8>)>,
    pub(crate) buffered_bytes: usize,
}

impl State {
    pub(crate) fn request(&mut self, data: &[u8], flags: u32) -> Result<Request> {
        let request = Request::decode(data)?;
        let capability = match request.protocol {
            TransportProtocol::Reliable => 1,
            TransportProtocol::Lossy => 4,
        };
        if flags & capability == 0 {
            return Err(Error::Invalid("unnegotiated multitransport request"));
        }
        if self.pending.len() >= 2
            || self.pending.contains_key(&request.id)
            || self
                .pending
                .values()
                .any(|prior| prior.protocol == request.protocol)
            || self.ready.contains(&request.protocol)
        {
            return Err(Error::Invalid("duplicate multitransport request"));
        }
        self.pending.insert(request.id, request.clone());
        Ok(request)
    }

    pub(crate) fn complete(&mut self, id: u32, status: u32) -> Result<Request> {
        let request = self
            .pending
            .remove(&id)
            .ok_or(Error::Invalid("unknown multitransport request"))?;
        if status & 0x8000_0000 == 0 {
            self.ready.insert(request.protocol);
        }
        Ok(request)
    }
    pub(crate) fn ready(&self, protocol: TransportProtocol) -> bool {
        self.ready.contains(&protocol)
    }
    pub(crate) fn close(&mut self, protocol: TransportProtocol) {
        self.ready.remove(&protocol);
        self.pending
            .retain(|_, request| request.protocol != protocol);
        self.buffered.retain(|(transport, bytes)| {
            if *transport == protocol {
                self.buffered_bytes -= bytes.len();
                false
            } else {
                true
            }
        });
    }
}
