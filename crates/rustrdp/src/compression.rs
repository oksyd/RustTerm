//! RDP 4.0/5.0 MPPC bulk decompression (MS-RDPBCGR §3.1.8 and RFC 2118).
//!
//! One decoder belongs to one direction of a connection. Slow-path, fast-path
//! and static virtual-channel payloads share its history and must be submitted
//! in wire order, before their independent fragment reassembly.

use std::borrow::Cow;

use crate::{Error, Result};

const COMPRESSED: u8 = 0x20;
const AT_FRONT: u8 = 0x40;
const FLUSHED: u8 = 0x80;
const HISTORY_BYTES: usize = 65536;

/// Maximum supported bulk compression package advertised in Client Info.
/// RDP 5.0 support includes RDP 4.0; the server chooses one package per session.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
pub enum BulkCompression {
    /// MPPC with an 8 KiB history window.
    Rdp4 = 0,
    /// MPPC with a 64 KiB history window, also permitting RDP 4.0 negotiation.
    Rdp5 = 1,
}

impl BulkCompression {
    fn from_flags(flags: u8) -> Result<Self> {
        match flags & 15 {
            0 => Ok(Self::Rdp4),
            1 => Ok(Self::Rdp5),
            _ => Err(Error::Unsupported("bulk compression package")),
        }
    }

    fn history_size(self) -> usize {
        match self {
            Self::Rdp4 => 8192,
            Self::Rdp5 => HISTORY_BYTES,
        }
    }

    fn length_bits(self) -> usize {
        match self {
            Self::Rdp4 => 12,
            Self::Rdp5 => 15,
        }
    }
}

/// A bounded receive-side bulk compression context, independent of transport.
///
/// Uncompressed input is returned by reference and never enters the history.
/// A decode error is terminal: discard the decoder and its enclosing session.
pub struct BulkDecoder {
    history: Box<[u8]>,
    position: usize,
    algorithm: Option<BulkCompression>,
    max_output: usize,
    failed: bool,
}

impl BulkDecoder {
    /// Creates a fresh decoder with a fixed 64 KiB history allocation.
    ///
    /// `max_output` limits each decoded payload, including uncompressed input.
    /// The first compressed packet chooses RDP 4.0 or 5.0 for this context;
    /// changing the package later is an error. A zero limit accepts only empty
    /// output. Applications using [`crate::Session`] do not need a separate
    /// decoder: the session owns and drives its negotiated history.
    pub fn new(max_output: usize) -> Self {
        Self {
            // All initial history bytes are valid zeros (MS-RDPBCGR 3.1.8.1).
            history: vec![0; HISTORY_BYTES].into_boxed_slice(),
            position: 0,
            algorithm: None,
            max_output,
            failed: false,
        }
    }

    /// Decompresses the payload using its compression-header flags byte.
    /// Pass `(channel_flags >> 16) as u8` for static virtual channel chunks.
    /// A successful result contains only this packet's uncompressed bytes.
    ///
    /// Bits 0–3 identify the package; `0x20` means compressed, `0x40` rewinds
    /// the history cursor, and `0x80` clears history. The input contains only the
    /// compressed payload, with transport/compression headers removed.
    /// Uncompressed bytes are borrowed; compressed results own their buffer.
    ///
    /// # Errors
    ///
    /// Malformed tokens, unsupported packages, package changes and output or
    /// history limits return an error and permanently invalidate this decoder.
    /// Do not reuse it after an error or share it between connections.
    ///
    /// ```
    /// use rustrdp::compression::BulkDecoder;
    /// let mut decoder = BulkDecoder::new(4096);
    /// // ASCII bytes below 0x80 are literal tokens in both MPPC packages.
    /// assert_eq!(decoder.decode(0x21, b"hello")?.as_ref(), b"hello");
    /// # Ok::<(), rustrdp::Error>(())
    /// ```
    pub fn decode<'a>(&mut self, flags: u8, payload: &'a [u8]) -> Result<Cow<'a, [u8]>> {
        if self.failed {
            return Err(Error::Invalid("failed bulk decompressor"));
        }
        let result = self.decode_packet(flags, payload);
        if result.is_err() {
            self.failed = true;
        }
        result
    }

    fn decode_packet<'a>(&mut self, flags: u8, payload: &'a [u8]) -> Result<Cow<'a, [u8]>> {
        if flags & 0x10 != 0 {
            return Err(Error::Invalid("bulk compression flags"));
        }
        if flags & AT_FRONT != 0 && flags & COMPRESSED == 0 {
            return Err(Error::Invalid("uncompressed PACKET_AT_FRONT"));
        }
        if flags & FLUSHED != 0 {
            self.history.fill(0);
            self.position = 0;
        }
        if flags & COMPRESSED == 0 {
            if payload.len() > self.max_output {
                return Err(Error::Limit("bulk output"));
            }
            return Ok(Cow::Borrowed(payload));
        }
        let algorithm = BulkCompression::from_flags(flags)?;
        if self.algorithm.is_some_and(|selected| selected != algorithm) {
            return Err(Error::Invalid("changed bulk compression package"));
        }
        self.algorithm = Some(algorithm);
        let size = algorithm.history_size();
        if flags & AT_FRONT != 0 {
            self.position = 0;
        }
        let start = self.position;
        let mut bits = Bits::new(payload)?;
        // Every symbol needs at least eight bits. The final partial byte is
        // padding, never a zero literal and never a new match instruction.
        while bits.remaining() >= 8 {
            if bits.read(1)? == 0 {
                let literal = bits.read(7)? as u8;
                self.append(literal, start, size)?;
            } else if bits.read(1)? == 0 {
                let literal = 0x80 | bits.read(7)? as u8;
                self.append(literal, start, size)?;
            } else {
                let offset = copy_offset(&mut bits, algorithm)?;
                let length = match_length(&mut bits, algorithm.length_bits())?;
                if offset >= size {
                    return Err(Error::Invalid("bulk copy offset"));
                }
                self.reserve_output(length, start, size)?;
                let source = self.position.wrapping_sub(offset) & (size - 1);
                if source + length > size {
                    return Err(Error::Invalid("bulk copy source"));
                }
                // Source and destination can overlap. Read each byte after the
                // previous write so repeated patterns expand correctly.
                for index in 0..length {
                    self.history[self.position] = self.history[source + index];
                    self.position += 1;
                }
            }
        }
        Ok(Cow::Owned(self.history[start..self.position].to_vec()))
    }

    fn append(&mut self, byte: u8, start: usize, size: usize) -> Result<()> {
        self.reserve_output(1, start, size)?;
        self.history[self.position] = byte;
        self.position += 1;
        Ok(())
    }

    fn reserve_output(&self, length: usize, start: usize, size: usize) -> Result<()> {
        if length > size.saturating_sub(self.position) {
            return Err(Error::Limit("bulk history window"));
        }
        if length > self.max_output.saturating_sub(self.position - start) {
            return Err(Error::Limit("bulk output"));
        }
        Ok(())
    }
}

// The common leading `11` has already been consumed by the literal decoder.
fn copy_offset(bits: &mut Bits<'_>, algorithm: BulkCompression) -> Result<usize> {
    match algorithm {
        BulkCompression::Rdp4 => {
            if bits.read(1)? == 0 {
                Ok(320 + bits.read(13)?)
            } else if bits.read(1)? == 0 {
                Ok(64 + bits.read(8)?)
            } else {
                bits.read(6)
            }
        }
        BulkCompression::Rdp5 => {
            if bits.read(1)? == 0 {
                Ok(2368 + bits.read(16)?)
            } else if bits.read(1)? == 0 {
                Ok(320 + bits.read(11)?)
            } else if bits.read(1)? == 0 {
                Ok(64 + bits.read(8)?)
            } else {
                bits.read(6)
            }
        }
    }
}

fn match_length(bits: &mut Bits<'_>, maximum_bits: usize) -> Result<usize> {
    if bits.read(1)? == 0 {
        return Ok(3);
    }
    let mut width = 2;
    while bits.read(1)? != 0 {
        width += 1;
        if width > maximum_bits {
            return Err(Error::Invalid("bulk match length prefix"));
        }
    }
    Ok((1 << width) | bits.read(width)?)
}

struct Bits<'a> {
    bytes: &'a [u8],
    position: usize,
    length: usize,
}

impl<'a> Bits<'a> {
    fn new(bytes: &'a [u8]) -> Result<Self> {
        Ok(Self {
            bytes,
            position: 0,
            length: bytes
                .len()
                .checked_mul(8)
                .ok_or(Error::Limit("bulk input"))?,
        })
    }

    fn remaining(&self) -> usize {
        self.length - self.position
    }

    fn read(&mut self, mut count: usize) -> Result<usize> {
        if count > self.remaining() {
            return Err(Error::Truncated);
        }
        let mut value = 0;
        while count > 0 {
            let available = 8 - self.position % 8;
            let take = available.min(count);
            let mask = (1_usize << take) - 1;
            value = (value << take)
                | (usize::from(self.bytes[self.position / 8]) >> (available - take) & mask);
            self.position += take;
            count -= take;
        }
        Ok(value)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // Fixed wire vectors derived from the example tokens in MS-RDPBCGR 3.1.8.2.3.
    const RDP4_EXAMPLE: &[u8] = &[
        0x66, 0x6f, 0x72, 0x2e, 0x77, 0x68, 0x6f, 0x6d, 0x2e, 0x74, 0x68, 0x65, 0x2e, 0x62, 0x65,
        0x6c, 0x6c, 0x2e, 0x74, 0x6f, 0x6c, 0x6c, 0x73, 0x2c, 0xf4, 0x37, 0x2e, 0xfa, 0x23, 0xd3,
        0x32, 0x90, 0x80,
    ];
    const RDP5_EXAMPLE: &[u8] = &[
        0x66, 0x6f, 0x72, 0x2e, 0x77, 0x68, 0x6f, 0x6d, 0x2e, 0x74, 0x68, 0x65, 0x2e, 0x62, 0x65,
        0x6c, 0x6c, 0x2e, 0x74, 0x6f, 0x6c, 0x6c, 0x73, 0x2c, 0xfa, 0x1b, 0x97, 0x7e, 0x88, 0xfa,
        0x66, 0x52, 0x10,
    ];

    fn wire(bits: &str) -> Vec<u8> {
        let mut bytes = vec![0; bits.len().div_ceil(8)];
        for (index, bit) in bits.bytes().enumerate() {
            assert!(matches!(bit, b'0' | b'1'));
            bytes[index / 8] |= (bit - b'0') << (7 - index % 8);
        }
        bytes
    }

    #[test]
    fn microsoft_example_decodes_with_both_packages() {
        let expected = b"for.whom.the.bell.tolls,.the.bell.tolls.for.thee!";
        for (flags, bytes) in [(0x60, RDP4_EXAMPLE), (0x61, RDP5_EXAMPLE)] {
            assert_eq!(
                BulkDecoder::new(1024)
                    .decode(flags, bytes)
                    .unwrap()
                    .as_ref(),
                expected
            );
        }
    }

    #[test]
    fn literal_high_bit_and_replicating_matches() {
        assert_eq!(
            BulkDecoder::new(32)
                .decode(0x60, &[0xb3, 0xab, 0])
                .unwrap()
                .as_ref(),
            &[0xe7, 0x56]
        );
        // X c d <offset=2, length=4> Y Z, with overlapping source and destination.
        let mut data = b"Xcd".to_vec();
        data.extend(wire("111100001010000101100101011010"));
        assert_eq!(
            BulkDecoder::new(32).decode(0x60, &data).unwrap().as_ref(),
            b"XcdcdcdYZ"
        );
    }

    #[test]
    fn uncompressed_payload_is_borrowed_without_altering_history() {
        let mut decoder = BulkDecoder::new(32);
        assert_eq!(decoder.decode(0x61, b"ABC").unwrap().as_ref(), b"ABC");
        let plain = b"raw";
        assert!(
            matches!(decoder.decode(0, plain).unwrap(), Cow::Borrowed(bytes) if std::ptr::eq(bytes.as_ptr(), plain.as_ptr()))
        );
        // RDP5 copy offset 3, length 3. Plain "raw" must not enter the history.
        assert_eq!(
            decoder.decode(0x21, &[0xf8, 0x60]).unwrap().as_ref(),
            b"ABC"
        );
    }

    #[test]
    fn at_front_preserves_history_but_flush_replaces_it_with_zeros() {
        let mut decoder = BulkDecoder::new(32);
        decoder.decode(0x61, b"ABC").unwrap();
        // Offset zero reads the still-valid previous contents at the new front.
        assert_eq!(decoder.decode(0x61, &[0xf8, 0]).unwrap().as_ref(), b"ABC");
        assert_eq!(
            decoder.decode(0x80, b"unencoded").unwrap().as_ref(),
            b"unencoded"
        );
        assert_eq!(decoder.decode(0x21, &[0xf8, 0]).unwrap().as_ref(), &[0; 3]);
        // Starting at zero can refer backwards to the valid zero-filled window tail.
        assert_eq!(
            decoder.decode(0xe1, &[0xf8, 0x60]).unwrap().as_ref(),
            &[0; 3]
        );
    }

    #[test]
    fn all_copy_offset_classes_and_large_match_lengths() {
        // Start with valid zero-filled history; each copy range fits the window.
        for (flags, bits) in [
            (0x60, "11110000110"),          // offset 3, length 3
            (0x60, "1110000000000"),        // offset 64, length 3
            (0x60, "11000000000000000"),    // offset 320, length 3
            (0x61, "111110000110"),         // offset 3, length 3
            (0x61, "11110000000000"),       // offset 64, length 3
            (0x61, "1110000000000000"),     // offset 320, length 3
            (0x61, "11000000000000000000"), // offset 2368, length 3
        ] {
            assert_eq!(
                BulkDecoder::new(1024)
                    .decode(flags, &wire(bits))
                    .unwrap()
                    .as_ref(),
                &[0; 3]
            );
        }
        // A single zero-offset match exercises the maximum RDP5 L-o-M encoding.
        let bits = "11111000000".to_owned() + "111111111111110" + "111111111111111";
        let decoded = BulkDecoder::new(HISTORY_BYTES)
            .decode(0x61, &wire(&bits))
            .unwrap()
            .into_owned();
        assert_eq!(decoded.len(), 65535);
        assert!(decoded.iter().all(|byte| *byte == 0));
    }

    #[test]
    fn output_and_history_limits_are_checked_before_writes() {
        let mut decoder = BulkDecoder::new(2);
        assert_eq!(
            decoder.decode(0x61, &[0xf8, 0]).unwrap_err(),
            Error::Limit("bulk output")
        );
        assert!(decoder.decode(0, b"a").is_err());
        let mut decoder = BulkDecoder::new(HISTORY_BYTES);
        decoder.decode(0x60, &vec![b'a'; 8191]).unwrap();
        assert_eq!(
            decoder.decode(0x20, b"bc").unwrap_err(),
            Error::Limit("bulk history window")
        );
        // Largest bit-encoded RDP4 offset exceeds the 8KiB history window.
        assert!(
            BulkDecoder::new(32)
                .decode(0x60, &wire("11011111111111110"))
                .is_err()
        );
        // A backwards reference may start in the tail but cannot read past it.
        assert!(BulkDecoder::new(32).decode(0x61, &[0xf8, 0x20]).is_err());
    }

    #[test]
    fn malformed_symbols_and_package_changes_fail_closed() {
        for data in [&[0x80][..], &[0xff], &[0xff, 0xff], &[0xc0]] {
            assert!(BulkDecoder::new(1024).decode(0x60, data).is_err());
        }
        for flags in [0x10, 0x40, 0x22, 0x23] {
            assert!(BulkDecoder::new(1024).decode(flags, b"a").is_err());
        }
        let mut decoder = BulkDecoder::new(1024);
        decoder.decode(0x61, b"abc").unwrap();
        assert!(decoder.decode(0xe0, b"abc").is_err());
        assert!(decoder.decode(0x61, b"abc").is_err());
    }
}
