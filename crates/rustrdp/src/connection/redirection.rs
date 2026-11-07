use super::{NegotiationResponse, negotiation_response};
use crate::{
    Error, Result,
    wire::{self, Reader},
};

/// Negotiate RDSTLS using a routing token from an authenticated server handoff.
/// Never use an account password here: the routing token precedes TLS.
pub fn redirection_request(routing_token: &[u8]) -> Result<Vec<u8>> {
    let token = routing_token.strip_suffix(b"\r\n").unwrap_or(routing_token);
    if token.is_empty() || token.len() > 239 || token.iter().any(|b| b.is_ascii_control()) {
        return Err(Error::Invalid("redirection routing token"));
    }
    let length = 21 + token.len();
    let mut packet = vec![3, 0];
    packet.extend_from_slice(&(length as u16).to_be_bytes());
    packet.extend_from_slice(&[(length - 5) as u8, 0xe0, 0, 0, 0, 0, 0]);
    packet.extend_from_slice(token);
    packet.extend_from_slice(b"\r\n");
    packet.extend_from_slice(&[1, 0, 8, 0, 4, 0, 0, 0]);
    Ok(packet)
}

/// Require RDSTLS after [`redirection_request`]. Ordinary connections continue
/// to use NLA-only negotiation; a redirect never enables Standard RDP Security.
pub fn parse_redirection_negotiation_response(packet: &[u8]) -> Result<NegotiationResponse> {
    negotiation_response(packet, true)
}

pub(crate) fn payload(
    packet: &[u8],
    io_channel: u16,
    server: Option<u16>,
) -> Result<Option<&[u8]>> {
    if packet.first() != Some(&3) || packet.get(7) != Some(&0x68) {
        return Ok(None);
    }
    let (source, channel, data) = wire::mcs_indication(packet)?;
    if channel != io_channel {
        return Ok(None);
    }
    let mut pdus = Reader::new(data);
    while !pdus.is_empty() {
        let mut header = pdus;
        let length = usize::from(header.u16_le()?);
        if length == 0x8000 {
            return Ok(None);
        }
        if length < 6 {
            return Err(Error::Invalid("Share Control length"));
        }
        let kind = header.u16_le()?;
        // Licensing has a security header instead of a Share Control header.
        if kind & 0xfff0 != 0x10 && kind != 0x0a {
            return Ok(None);
        }
        let pdu_source = header.u16_le()?;
        let pdu = pdus.take(length)?;
        if matches!(kind, 0x0a | 0x1a) {
            if server.is_some_and(|id| id != source || (pdu_source != 0 && pdu_source != id)) {
                return Err(Error::Invalid("server redirection source"));
            }
            pdus.finish()?;
            return Ok(Some(&pdu[6..]));
        }
    }
    Ok(None)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::connection::{
        SecurityProtocol, indication_frame, parse_negotiation_response_details,
    };

    #[test]
    fn routing_negotiation_is_bounded_and_cannot_downgrade() {
        let request = redirection_request(b"12345").unwrap();
        assert_eq!(
            usize::from(u16::from_be_bytes([request[2], request[3]])),
            request.len()
        );
        assert_eq!(usize::from(request[4]) + 5, request.len());
        assert_eq!(&request[11..18], b"12345\r\n");
        assert_eq!(&request[request.len() - 4..], &[4, 0, 0, 0]);
        assert_eq!(request, redirection_request(b"12345\r\n").unwrap());
        for token in [b"".as_slice(), b"a\r\nb", b"a\0b", &[b'x'; 240]] {
            assert!(redirection_request(token).is_err());
        }
        let mut response = [3, 0, 0, 19, 14, 0xd0, 0, 0, 0, 0, 0, 2, 1, 8, 0, 4, 0, 0, 0];
        assert_eq!(
            parse_redirection_negotiation_response(&response)
                .unwrap()
                .protocol,
            SecurityProtocol::RdsTls
        );
        assert!(parse_negotiation_response_details(&response).is_err());
        for mode in [0, 1, 2, 8] {
            response[15] = mode;
            assert!(parse_redirection_negotiation_response(&response).is_err());
        }
    }

    #[test]
    fn handoffs_are_scoped_to_the_server_and_io_channel() {
        for version in [0u16, 0x10] {
            let mut pdu = wire::share_control(1002, 10, &[0; 14]).unwrap();
            pdu[2..4].copy_from_slice(&(version | 10).to_le_bytes());
            let packet = indication_frame(1002, 1003, &pdu).unwrap();
            assert_eq!(
                payload(&packet, 1003, Some(1002)).unwrap(),
                Some([0; 14].as_slice())
            );
            assert!(payload(&packet, 1003, Some(1004)).is_err());
            assert!(payload(&packet, 1004, Some(1002)).unwrap().is_none());
            for length in 8..packet.len() {
                assert!(payload(&packet[..length], 1003, None).is_err());
            }
        }
        let license = indication_frame(1002, 1003, &[0x80, 0, 0, 0, 0xff, 3, 16, 0]).unwrap();
        assert!(payload(&license, 1003, None).unwrap().is_none());
    }
}
