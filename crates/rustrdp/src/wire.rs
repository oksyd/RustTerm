//! Allocation-free frame-length probes for decrypted RDP and CredSSP streams.
//!
//! A returned length describes the next complete frame; it does not imply that
//! its body has arrived. Keep incomplete bytes, submit complete frames in wire
//! order, and retain any following bytes for the next frame.
use crate::{Error, Result};

#[derive(Clone, Copy)]
pub(crate) struct Reader<'a> {
    data: &'a [u8],
    position: usize,
}

impl<'a> Reader<'a> {
    pub fn new(data: &'a [u8]) -> Self {
        Self { data, position: 0 }
    }
    pub fn remaining(&self) -> &'a [u8] {
        &self.data[self.position..]
    }
    pub fn remaining_len(&self) -> usize {
        self.data.len() - self.position
    }
    #[cfg(feature = "licensing")]
    pub fn position(&self) -> usize {
        self.position
    }
    pub fn is_empty(&self) -> bool {
        self.position == self.data.len()
    }
    pub fn take(&mut self, length: usize) -> Result<&'a [u8]> {
        let end = self.position.checked_add(length).ok_or(Error::Truncated)?;
        let bytes = self.data.get(self.position..end).ok_or(Error::Truncated)?;
        self.position = end;
        Ok(bytes)
    }
    pub fn skip(&mut self, length: usize) -> Result<()> {
        self.take(length).map(|_| ())
    }
    fn array<const N: usize>(&mut self) -> Result<[u8; N]> {
        let mut bytes = [0; N];
        bytes.copy_from_slice(self.take(N)?);
        Ok(bytes)
    }
    pub fn u8(&mut self) -> Result<u8> {
        Ok(self.take(1)?[0])
    }
    pub fn u16_le(&mut self) -> Result<u16> {
        Ok(u16::from_le_bytes(self.array()?))
    }
    pub fn u16_be(&mut self) -> Result<u16> {
        Ok(u16::from_be_bytes(self.array()?))
    }
    pub fn u32_le(&mut self) -> Result<u32> {
        Ok(u32::from_le_bytes(self.array()?))
    }
    pub fn finish(self) -> Result<()> {
        if self.is_empty() {
            Ok(())
        } else {
            Err(Error::Invalid("trailing message bytes"))
        }
    }
}

/// Transport framing of an RDP packet after TLS decryption.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FrameKind {
    /// TPKT wrapping an X.224 connection or data packet.
    X224,
    /// An RDP fast-path output packet.
    FastPath,
}

/// Returns a declared packet length without allocating or waiting for its body.
///
/// `Ok(None)` means that the length header is incomplete. `Ok(Some((kind, size)))`
/// includes the header in `size`; wait until `data.len() >= size` before passing
/// `&data[..size]` to the connector or session. Trailing frames are not consumed.
/// `max` is the caller's inclusive maximum frame size, normally
/// [`crate::Limits::max_pdu`]. This probe does not validate the packet body.
///
/// # Errors
///
/// Returns [`Error::Invalid`] for malformed framing, [`Error::Unsupported`] for
/// legacy RDP encryption, or [`Error::Limit`] if the declared size exceeds `max`.
///
/// ```
/// use rustrdp::wire::{frame_size, FrameKind};
/// assert_eq!(frame_size(&[3, 0, 0], 4096)?, None);
/// assert_eq!(frame_size(&[3, 0, 0, 7], 4096)?, Some((FrameKind::X224, 7)));
/// # Ok::<(), rustrdp::Error>(())
/// ```
pub fn frame_size(data: &[u8], max: usize) -> Result<Option<(FrameKind, usize)>> {
    let Some(&first) = data.first() else {
        return Ok(None);
    };
    let (kind, length, minimum) = if first == 3 {
        if data.len() < 4 {
            return Ok(None);
        }
        if data[1] != 0 {
            return Err(Error::Invalid("TPKT reserved byte"));
        }
        (
            FrameKind::X224,
            usize::from(u16::from_be_bytes([data[2], data[3]])),
            7,
        )
    } else {
        if first & 3 != 0 {
            return Err(Error::Invalid("fast-path action"));
        }
        if first & 0xc0 != 0 {
            return Err(Error::Unsupported("standard RDP encryption"));
        }
        if data.len() < 2 {
            return Ok(None);
        }
        if data[1] & 0x80 == 0 {
            (FrameKind::FastPath, usize::from(data[1]), 2)
        } else {
            if data.len() < 3 {
                return Ok(None);
            }
            (
                FrameKind::FastPath,
                (usize::from(data[1] & 0x7f) << 8) | usize::from(data[2]),
                3,
            )
        }
    };
    if length < minimum {
        return Err(Error::Invalid("packet length"));
    }
    if length > max {
        return Err(Error::Limit("packet length"));
    }
    Ok(Some((kind, length)))
}

pub(crate) fn x224_data(body: &[u8]) -> Result<Vec<u8>> {
    let length = body
        .len()
        .checked_add(7)
        .and_then(|n| u16::try_from(n).ok())
        .ok_or(Error::Limit("TPKT length"))?;
    let mut packet = Vec::with_capacity(usize::from(length));
    packet.extend_from_slice(&[3, 0]);
    packet.extend_from_slice(&length.to_be_bytes());
    packet.extend_from_slice(&[2, 0xf0, 0x80]);
    packet.extend_from_slice(body);
    Ok(packet)
}

pub(crate) fn x224_payload(packet: &[u8]) -> Result<&[u8]> {
    let (kind, length) = frame_size(packet, 65535)?.ok_or(Error::Truncated)?;
    if kind != FrameKind::X224 || length != packet.len() {
        return Err(Error::Invalid("TPKT frame"));
    }
    if packet.get(4..7) != Some(&[2, 0xf0, 0x80]) {
        return Err(Error::Invalid("X.224 data header"));
    }
    Ok(&packet[7..])
}

pub(crate) fn fast_path_payload(packet: &[u8]) -> Result<&[u8]> {
    let (kind, length) = frame_size(packet, 32767)?.ok_or(Error::Truncated)?;
    if kind != FrameKind::FastPath || length != packet.len() {
        return Err(Error::Invalid("fast-path frame"));
    }
    Ok(&packet[if packet[1] & 0x80 != 0 { 3 } else { 2 }..])
}

pub(crate) fn per_length(reader: &mut Reader<'_>) -> Result<usize> {
    let first = reader.u8()?;
    if first & 0x80 == 0 {
        Ok(usize::from(first))
    } else {
        Ok((usize::from(first & 0x7f) << 8) | usize::from(reader.u8()?))
    }
}

pub(crate) fn put_per_length(out: &mut Vec<u8>, length: usize) -> Result<()> {
    if length < 128 {
        out.push(length as u8);
    } else if length <= 32767 {
        out.extend_from_slice(&((length as u16) | 0x8000).to_be_bytes());
    } else {
        return Err(Error::Limit("MCS user data"));
    }
    Ok(())
}

pub(crate) fn mcs_send_data(user_id: u16, channel: u16, payload: &[u8]) -> Result<Vec<u8>> {
    let user = user_id
        .checked_sub(1001)
        .ok_or(Error::Invalid("MCS user ID"))?;
    if payload.len() > 32767 {
        return Err(Error::Limit("MCS user data"));
    }
    let mut out = Vec::with_capacity(payload.len() + 8);
    out.push(0x64);
    out.extend_from_slice(&user.to_be_bytes());
    out.extend_from_slice(&channel.to_be_bytes());
    out.push(0x70);
    put_per_length(&mut out, payload.len())?;
    out.extend_from_slice(payload);
    x224_data(&out)
}

pub(crate) fn mcs_disconnect(packet: &[u8]) -> Result<Option<u8>> {
    if packet.first() != Some(&3) {
        return Ok(None);
    }
    let data = x224_payload(packet)?;
    if !data.first().is_some_and(|byte| byte >> 2 == 8) {
        return Ok(None);
    }
    if data.len() != 2 || data[1] & 0x7f != 0 {
        return Err(Error::Invalid("MCS disconnect"));
    }
    let reason = ((data[0] & 3) << 1) | (data[1] >> 7);
    if reason > 4 {
        return Err(Error::Invalid("MCS disconnect reason"));
    }
    Ok(Some(reason))
}

pub(crate) fn mcs_indication(packet: &[u8]) -> Result<(u16, u16, &[u8])> {
    let mut reader = Reader::new(x224_payload(packet)?);
    if reader.u8()? != 0x68 {
        return Err(Error::Invalid("MCS SendDataIndication"));
    }
    let user = reader
        .u16_be()?
        .checked_add(1001)
        .ok_or(Error::Invalid("MCS user ID"))?;
    let channel = reader.u16_be()?;
    if reader.u8()? & 0x30 != 0x30 {
        return Err(Error::Unsupported("MCS segmentation"));
    }
    let length = per_length(&mut reader)?;
    let data = reader.take(length)?;
    reader.finish()?;
    Ok((user, channel, data))
}

/// Read one Share Control PDU from an MCS user-data field. T.128 flow data
/// consumes the remaining field and is ignored (MS-RDPBCGR 2.2.8.1.1.1.1).
pub(crate) fn next_share_pdu<'a>(pdus: &mut Reader<'a>) -> Result<Option<&'a [u8]>> {
    if pdus.is_empty() {
        return Ok(None);
    }
    let mut header = *pdus;
    let length = usize::from(header.u16_le()?);
    if length == 0x8000 {
        pdus.skip(pdus.remaining_len())?;
        return Ok(None);
    }
    if length < 6 {
        return Err(Error::Invalid("Share Control length"));
    }
    pdus.take(length).map(Some)
}

pub(crate) fn share_control(source: u16, kind: u16, body: &[u8]) -> Result<Vec<u8>> {
    let length = body
        .len()
        .checked_add(6)
        .and_then(|n| u16::try_from(n).ok())
        .ok_or(Error::Limit("Share Control PDU"))?;
    let mut out = Vec::with_capacity(usize::from(length));
    out.extend_from_slice(&length.to_le_bytes());
    out.extend_from_slice(&(kind | 0x10).to_le_bytes());
    out.extend_from_slice(&source.to_le_bytes());
    out.extend_from_slice(body);
    Ok(out)
}

pub(crate) fn share_data(
    user: u16,
    io: u16,
    share_id: u32,
    kind: u8,
    payload: &[u8],
) -> Result<Vec<u8>> {
    let length = payload
        .len()
        .checked_add(4)
        .and_then(|n| u16::try_from(n).ok())
        .ok_or(Error::Limit("Share Data PDU"))?;
    let mut body = Vec::with_capacity(payload.len().saturating_add(12));
    body.extend_from_slice(&share_id.to_le_bytes());
    body.extend_from_slice(&[0, 1]);
    body.extend_from_slice(&length.to_le_bytes());
    body.extend_from_slice(&[kind, 0, 0, 0]);
    body.extend_from_slice(payload);
    mcs_send_data(user, io, &share_control(user, 7, &body)?)
}

/// Definite-length ASN.1 SEQUENCE framing for CredSSP. No BER object is decoded here.
///
/// Use only while authenticating with CredSSP, before the RDP frame stream begins.
/// `Ok(None)` means the length header is incomplete. A returned size includes the
/// ASN.1 header and can exceed `data.len()` until the rest of the body arrives.
/// `max` is the caller's inclusive maximum CredSSP message size.
///
/// # Errors
///
/// Returns [`Error::Invalid`] for a wrong tag, indefinite length or a length
/// exceeding four octets, and [`Error::Limit`] for overflow or a size above `max`.
pub fn credssp_size(data: &[u8], max: usize) -> Result<Option<usize>> {
    if data.is_empty() {
        return Ok(None);
    }
    if data[0] != 0x30 {
        return Err(Error::Invalid("CredSSP ASN.1 tag"));
    }
    if data.len() < 2 {
        return Ok(None);
    }
    let (header, length) = if data[1] & 0x80 == 0 {
        (2, usize::from(data[1]))
    } else {
        let count = usize::from(data[1] & 0x7f);
        if count == 0 || count > 4 {
            return Err(Error::Invalid("CredSSP ASN.1 length"));
        }
        if data.len() < 2 + count {
            return Ok(None);
        }
        let mut length = 0_usize;
        for byte in &data[2..2 + count] {
            length = length * 256 + usize::from(*byte);
        }
        (2 + count, length)
    };
    let total = length
        .checked_add(header)
        .ok_or(Error::Limit("CredSSP message"))?;
    if total > max {
        return Err(Error::Limit("CredSSP message"));
    }
    Ok(Some(total))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn partial_and_oversized_framing_is_checked_before_body() {
        assert_eq!(frame_size(&[3, 0, 0], 32), Ok(None));
        assert_eq!(
            frame_size(&[3, 0, 0, 40], 32),
            Err(Error::Limit("packet length"))
        );
        assert_eq!(frame_size(&[0, 0x81], 32767), Ok(None));
        assert_eq!(
            frame_size(&[0, 0x81, 0], 32767),
            Ok(Some((FrameKind::FastPath, 256)))
        );
        assert_eq!(
            credssp_size(&[0x30, 0x84, 0xff, 0xff, 0xff, 0xff], 65536),
            Err(Error::Limit("CredSSP message"))
        );
        assert_eq!(
            credssp_size(&[0x30, 0x80], 65536),
            Err(Error::Invalid("CredSSP ASN.1 length"))
        );
    }
    #[test]
    fn bounded_reader_does_not_advance_on_truncation() {
        let mut reader = Reader::new(&[1, 2, 3]);
        assert_eq!(reader.u32_le(), Err(Error::Truncated));
        assert_eq!(reader.remaining(), &[1, 2, 3]);
        assert_eq!(reader.take(usize::MAX), Err(Error::Truncated));
        assert_eq!(reader.u16_le(), Ok(513));
    }
}
