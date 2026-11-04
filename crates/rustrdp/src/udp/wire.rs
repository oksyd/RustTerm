use super::{Mode, Version};
use crate::{Error, Result};

pub(super) const MTU: usize = 1232;
pub(super) const WINDOW: usize = 256;
pub(super) const SYN: u16 = 1;
pub(super) const ACK: u16 = 4;
pub(super) const DATA: u16 = 8;
pub(super) const FEC: u16 = 16;
pub(super) const CN: u16 = 32;
pub(super) const CWR: u16 = 64;
pub(super) const AOA: u16 = 256;
pub(super) const LOSSY: u16 = 512;
pub(super) const SYNEX: u16 = 4096;

pub(super) struct SynAck {
    pub initial: u32,
    pub mtu: usize,
    pub window: usize,
    pub version: Version,
}

pub(super) fn syn(initial: u32, mode: Mode, hash: Option<[u8; 32]>) -> Vec<u8> {
    let mut bytes = Vec::with_capacity(MTU);
    bytes.extend_from_slice(&u32::MAX.to_be_bytes());
    bytes.extend_from_slice(&(WINDOW as u16).to_be_bytes());
    bytes.extend_from_slice(
        &(SYN | SYNEX | if mode == Mode::Lossy { LOSSY } else { 0 }).to_be_bytes(),
    );
    bytes.extend_from_slice(&initial.to_be_bytes());
    bytes.extend_from_slice(&(MTU as u16).to_be_bytes());
    bytes.extend_from_slice(&(MTU as u16).to_be_bytes());
    bytes.extend_from_slice(&1u16.to_be_bytes());
    let modern = mode == Mode::Reliable && hash.is_some();
    bytes.extend_from_slice(&(if modern { 0x0101u16 } else { 2 }).to_be_bytes());
    if modern && let Some(hash) = hash {
        bytes.extend_from_slice(&hash);
    }
    bytes.resize(MTU, 0);
    bytes
}

pub(super) fn syn_ack(
    bytes: &[u8],
    initial: u32,
    mode: Mode,
    offered_modern: bool,
) -> Result<SynAck> {
    let mut reader = Reader::new(bytes);
    if reader.be32()? != initial {
        return Err(Error::Invalid("UDP SYN ACK sequence"));
    }
    let window = usize::from(reader.be16()?).min(WINDOW);
    let flags = reader.be16()?;
    // SYNLOSSY is set by the initiating client. The server need not echo it;
    // the paired SYN/SYN-ACK capture in MS-RDPEUDP 4.1 explicitly omits it.
    if flags & (SYN | ACK) != SYN | ACK || (flags & LOSSY != 0 && mode != Mode::Lossy) {
        return Err(Error::Invalid("UDP SYN ACK flags"));
    }
    let initial = reader.be32()?;
    let up = usize::from(reader.be16()?);
    let down = usize::from(reader.be16()?);
    if !(1132..=MTU).contains(&up) || !(1132..=MTU).contains(&down) {
        return Err(Error::Invalid("UDP negotiated MTU"));
    }
    if flags & 0x800 != 0 {
        reader.take(32)?;
    }
    let mut version = Version::V1;
    if flags & SYNEX != 0 {
        let valid = reader.be16()? & 1 != 0;
        let selected = reader.be16()?;
        if valid {
            version = match selected {
                1 => Version::V1,
                2 => Version::V2,
                0x0101 if offered_modern && mode == Mode::Reliable => Version::V3,
                _ => return Err(Error::Unsupported("UDP negotiated version")),
            };
        }
    }
    Ok(SynAck {
        initial,
        mtu: up.min(down),
        window,
        version,
    })
}

pub(super) fn final_ack(initial: u32) -> Vec<u8> {
    let mut bytes = Vec::from(initial.to_be_bytes());
    bytes.extend_from_slice(&(WINDOW as u16).to_be_bytes());
    bytes.extend_from_slice(&ACK.to_be_bytes());
    bytes.extend_from_slice(&[0, 1, 0, 0]);
    bytes
}

pub(super) struct Source {
    pub coded: u64,
    pub source: u64,
    pub bytes: Vec<u8>,
}
pub(super) struct Parity {
    pub start: u64,
    pub count: usize,
    pub index: u8,
    pub bytes: Vec<u8>,
}
pub(super) struct Packet {
    pub window: usize,
    pub acknowledgements: Vec<(u64, u64)>,
    pub cumulative: Option<u64>,
    pub reset: Option<u64>,
    pub source: Option<Source>,
    pub parity: Option<Parity>,
    pub congestion: bool,
    pub dummy: bool,
}

pub(super) fn legacy(bytes: &[u8], sent: u64, received: u64, coded: u64) -> Result<Packet> {
    if bytes.len() > MTU + 2052 {
        return Err(Error::Limit("UDP datagram"));
    }
    let mut reader = Reader::new(bytes);
    let ack = expand(u64::from(reader.be32()?), sent, 32);
    let window = usize::from(reader.be16()?).min(WINDOW);
    let flags = reader.be16()?;
    if flags & (SYN | SYNEX | 0x800) != 0 {
        return Err(Error::Invalid("unexpected UDP handshake header"));
    }
    let mut acknowledgements = Vec::new();
    if flags & ACK != 0 {
        let length = usize::from(reader.be16()?);
        if length > 2048 {
            return Err(Error::Limit("UDP ACK vector"));
        }
        let entries = reader.take(length)?;
        let mut cursor = ack;
        // The vector starts at its oldest sequence; snSourceAck is its end.
        for &entry in entries.iter().rev() {
            let count = u64::from(entry & 63) + 1;
            if entry >> 6 == 0 {
                acknowledgements.push((cursor.saturating_sub(count - 1), cursor));
            }
            cursor = cursor.saturating_sub(count);
        }
        reader.take((length + 2).next_multiple_of(4) - length - 2)?;
    }
    let reset = if flags & AOA != 0 {
        Some(expand(u64::from(reader.be32()?), received, 32))
    } else {
        None
    };
    let mut source = None;
    let mut parity = None;
    if flags & DATA != 0 {
        let code = expand(u64::from(reader.be32()?), coded, 32);
        let sequence = expand(u64::from(reader.be32()?), received, 32);
        if flags & FEC != 0 {
            let count = usize::from(reader.u8()?) + 1;
            let index = reader.u8()?;
            reader.take(2)?;
            if (0..count).any(|offset| (sequence + offset as u64) as u8 == index)
                || reader.rest().len() < 2
                || reader.rest().len() > MTU + 2
            {
                return Err(Error::Invalid("UDP FEC payload"));
            }
            parity = Some(Parity {
                start: sequence,
                count,
                index,
                bytes: reader.rest().to_vec(),
            });
        } else {
            if reader.rest().len() > MTU {
                return Err(Error::Limit("UDP source payload"));
            }
            source = Some(Source {
                coded: code,
                source: sequence,
                bytes: reader.rest().to_vec(),
            });
        }
    } else if !reader.rest().is_empty() {
        return Err(Error::Invalid("trailing UDP acknowledgement"));
    }
    Ok(Packet {
        window,
        acknowledgements,
        cumulative: None,
        reset,
        source,
        parity,
        congestion: flags & CN != 0,
        dummy: false,
    })
}

pub(super) fn modern(bytes: &[u8], sent: u64, received: u64, coded: u64) -> Result<Packet> {
    if bytes.len() < 8 {
        return Err(Error::Truncated);
    }
    if bytes.len() > MTU {
        return Err(Error::Limit("UDP2 datagram"));
    }
    let prefix = bytes[7];
    let kind = (prefix >> 1) & 15;
    if !matches!(kind, 0 | 8) {
        return Err(Error::Invalid("UDP2 packet type"));
    }
    let mut data = bytes[1..].to_vec();
    data[6] = bytes[0];
    let short = usize::from(prefix >> 5);
    if (1..7).contains(&short) {
        data.truncate(short);
    }
    let mut reader = Reader::new(&data);
    let header = reader.le16()?;
    let flags = header & 0x0fff;
    // Windows uses 0x200 (NOACK) on DATA packets without an ACK payload.
    // It adds no bytes; the ACK/ACKVEC flags still determine their layouts.
    if flags == 0 || flags & !0x035d != 0 || flags & 9 == 9 {
        return Err(Error::Invalid("UDP2 packet flags"));
    }
    let window = (1usize << (header >> 12)).min(WINDOW);
    let mut acknowledgements = Vec::new();
    let cumulative = if flags & 1 != 0 {
        let sequence = expand(u64::from(reader.le16()?), sent, 16);
        reader.take(4)?;
        let delayed = reader.u8()? & 15;
        reader.take(usize::from(delayed))?;
        Some(sequence)
    } else {
        None
    };
    if flags & 0x40 != 0 {
        reader.u8()?;
    }
    if flags & 0x100 != 0 {
        reader.take(3)?;
    }
    let reset = if flags & 0x10 != 0 {
        Some(expand(u64::from(reader.le16()?), coded, 16))
    } else {
        None
    };
    let code = if flags & 4 != 0 {
        Some(expand(u64::from(reader.le16()?), coded, 16))
    } else {
        None
    };
    if flags & 8 != 0 {
        let mut sequence = expand(u64::from(reader.le16()?), sent, 16);
        let length = reader.u8()?;
        if length & 0x80 != 0 {
            reader.take(4)?;
        }
        for &entry in reader.take(usize::from(length & 127))? {
            if entry & 0x80 == 0 {
                for bit in 0..7 {
                    if entry & (1 << bit) != 0 {
                        acknowledgements.push((sequence, sequence));
                    }
                    sequence += 1;
                }
            } else {
                let count = u64::from(entry & 63);
                if count == 0 {
                    return Err(Error::Invalid("UDP2 empty ACK run"));
                }
                if entry & 0x40 != 0 {
                    acknowledgements.push((sequence, sequence + count - 1));
                }
                sequence += count;
            }
        }
    }
    let source = if let Some(code) = code {
        let sequence = expand(u64::from(reader.le16()?), received, 16);
        Some(Source {
            coded: code,
            source: sequence,
            bytes: reader.rest().to_vec(),
        })
    } else {
        if !reader.rest().is_empty() {
            return Err(Error::Invalid("trailing UDP2 control bytes"));
        }
        None
    };
    Ok(Packet {
        window,
        acknowledgements,
        cumulative,
        reset,
        source,
        parity: None,
        congestion: false,
        dummy: kind == 8,
    })
}

pub(super) fn wrap_modern(data: &[u8]) -> Vec<u8> {
    let mut bytes = Vec::with_capacity((data.len() + 1).max(8));
    bytes.push((data.len().min(7) as u8) << 5);
    bytes.extend_from_slice(data);
    bytes.resize(bytes.len().max(8), 0);
    bytes.swap(0, 7);
    bytes
}

pub(super) fn expand(low: u64, reference: u64, bits: u32) -> u64 {
    let modulus = 1u64 << bits;
    let candidate = (reference & !(modulus - 1)) | low;
    if candidate.saturating_add(modulus / 2) < reference {
        candidate + modulus
    } else if candidate > reference.saturating_add(modulus / 2) && candidate >= modulus {
        candidate - modulus
    } else {
        candidate
    }
}

struct Reader<'a> {
    bytes: &'a [u8],
}
impl<'a> Reader<'a> {
    fn new(bytes: &'a [u8]) -> Self {
        Self { bytes }
    }
    fn take(&mut self, count: usize) -> Result<&'a [u8]> {
        let bytes = self.bytes.get(..count).ok_or(Error::Truncated)?;
        self.bytes = &self.bytes[count..];
        Ok(bytes)
    }
    fn u8(&mut self) -> Result<u8> {
        Ok(self.take(1)?[0])
    }
    fn be16(&mut self) -> Result<u16> {
        Ok(u16::from_be_bytes(
            self.take(2)?.try_into().map_err(|_| Error::Truncated)?,
        ))
    }
    fn be32(&mut self) -> Result<u32> {
        Ok(u32::from_be_bytes(
            self.take(4)?.try_into().map_err(|_| Error::Truncated)?,
        ))
    }
    fn le16(&mut self) -> Result<u16> {
        Ok(u16::from_le_bytes(
            self.take(2)?.try_into().map_err(|_| Error::Truncated)?,
        ))
    }
    fn rest(&self) -> &'a [u8] {
        self.bytes
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn microsoft_legacy_syn_ack_and_source_capture() {
        // MS-RDPEUDP 4.1.2 and 4.2.1, excluding the displayed TLS payload tail.
        let syn = [
            0, 0, 0, 0x42, 4, 0, 0, 5, 0, 0, 0, 0x42, 4, 0xd0, 4, 0xd0, 0, 0, 0,
        ];
        let parsed = syn_ack(&syn, 0x42, Mode::Reliable, true).unwrap();
        assert_eq!(parsed.initial, 0x42);
        assert_eq!(parsed.version, Version::V1);
        // Its preceding SYN (4.1.1) requests lossy transport, but this server
        // response contains only SYN|ACK. An echo requirement breaks that pair.
        assert_eq!(
            syn_ack(&syn, 0x42, Mode::Lossy, false).unwrap().version,
            Version::V1
        );
        let source = [
            0xd6, 0xcf, 0x0a, 0xb8, 4, 0, 0, 0x0c, 0, 1, 4, 0, 0xec, 0x47, 0x1a, 0xe4, 0xec, 0x47,
            0x1a, 0xe4, 0x17, 3, 3, 0, 0x40, 0xbb,
        ];
        let packet = legacy(&source, 0xd6cf0ab8, 0xec471ae4, 0xec471ae4).unwrap();
        let data = packet.source.unwrap();
        assert_eq!(data.source, 0xec471ae4);
        assert_eq!(data.coded, 0xec471ae4);
        assert_eq!(data.bytes, [0x17, 3, 3, 0, 0x40, 0xbb]);
        assert_eq!(packet.acknowledgements, [(0xd6cf0ab4, 0xd6cf0ab8)]);
    }

    #[test]
    fn microsoft_udp2_example_checks_flags_and_payload_offsets() {
        // MS-RDPEUDP2 4.4.6. Its published header 0xc018 disagrees with its
        // listed ACK + DATA + AOA + OVERHEAD payloads; 2.2.1.1 specifies 0xc055.
        let mut bytes = [
            0x8d, 0x18, 0xc0, 0x57, 0x13, 0x0c, 0x16, 0, 4, 0x22, 0x29, 0x84, 0x40, 0x27, 0x54,
            0x33, 0x54, 0x79, 0x56, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10,
        ];
        assert!(modern(&bytes, 0x1357, 0x5679, 0x5433).is_err());
        bytes[1] = 0x55;
        let packet = modern(&bytes, 0x1357, 0x5679, 0x5433).unwrap();
        assert_eq!(packet.cumulative, Some(0x1357));
        assert_eq!(packet.reset, Some(0x5427));
        let source = packet.source.unwrap();
        assert_eq!(source.coded, 0x5433);
        assert_eq!(source.source, 0x5679);
        assert_eq!(source.bytes, [1, 2, 3, 4, 5, 6, 7, 8, 9, 10]);
        for length in 0..19 {
            assert!(modern(&bytes[..length], 0x1357, 0x5679, 0x5433).is_err());
        }
    }

    #[test]
    fn legacy_ack_vector_orders_runs_from_oldest_to_newest() {
        // MS-RDPEUDP 2.2.2.7, as decoded by FreeRDP v1_process_ack_vector:
        // highest 105, total five entries: 101 received, 102..103 missing,
        // 104..105 received. Unequal endpoint runs expose reversed decoding.
        let bytes = [0, 0, 0, 105, 1, 0, 0, 4, 0, 3, 0, 0xc1, 1, 0, 0, 0];
        let packet = legacy(&bytes, 105, 1, 1).unwrap();
        assert_eq!(packet.acknowledgements, [(104, 105), (101, 101)]);
    }

    #[test]
    fn microsoft_udp2_ack_bitmap_starts_at_bit_zero() {
        // MS-RDPEUDP2 3.1.5.7 example 1: base 1000, byte 0x64 acknowledges
        // exactly 1002, 1005 and 1006. Fixed wire bytes include the prefix swap.
        let bytes = [0, 8, 0x80, 0xe8, 3, 1, 0x64, 0xc0];
        let packet = modern(&bytes, 1006, 1, 100).unwrap();
        assert_eq!(
            packet.acknowledgements,
            [(1002, 1002), (1005, 1005), (1006, 1006)]
        );
    }

    #[test]
    fn syn_v2_v3_cookie_and_network_order() {
        let old = syn(0x10203040, Mode::Reliable, None);
        assert_eq!(
            &old[..20],
            &[
                255, 255, 255, 255, 1, 0, 0x10, 1, 0x10, 0x20, 0x30, 0x40, 4, 0xd0, 4, 0xd0, 0, 1,
                0, 2
            ]
        );
        assert_eq!(old.len(), 1232);
        let modern = syn(0x10203040, Mode::Reliable, Some([0x5a; 32]));
        assert_eq!(&modern[16..20], &[0, 1, 1, 1]);
        assert_eq!(&modern[20..52], &[0x5a; 32]);
        assert!(modern[52..].iter().all(|byte| *byte == 0));
    }

    #[test]
    fn serial_expansion_crosses_16_and_32_bit_wrap() {
        assert_eq!(expand(1, 65535, 16), 65537);
        assert_eq!(expand(65535, 65537, 16), 65535);
        assert_eq!(expand(2, u32::MAX as u64, 32), (1u64 << 32) + 2);
        assert_eq!(
            expand(u32::MAX as u64, (1u64 << 32) + 2, 32),
            u32::MAX as u64
        );
    }
}
