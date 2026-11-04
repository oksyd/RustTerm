//! RDP 8.0 bulk decompression, MS-RDPEGFX 2.2.5 and 3.1.9.1.

use crate::{Error, Result, wire::Reader};

const HISTORY_BYTES: usize = 2_500_000;
const SEGMENT_BYTES: usize = 65_535;

pub(super) struct Decoder {
    history: Vec<u8>,
    cursor: usize,
    available: usize,
}

impl Decoder {
    pub(super) fn new() -> Self {
        Self {
            history: Vec::new(),
            cursor: 0,
            available: 0,
        }
    }

    pub(super) fn decode(&mut self, data: &[u8], limit: usize) -> Result<Vec<u8>> {
        let mut reader = Reader::new(data);
        let mut output = Vec::new();
        match reader.u8()? {
            0xe0 => self.segment(reader.remaining(), &mut output, limit)?,
            0xe1 => {
                let count = reader.u16_le()?;
                let expected = reader.u32_le()? as usize;
                if expected > limit {
                    return Err(Error::Limit("ZGFX message expansion"));
                }
                for _ in 0..count {
                    let length = reader.u32_le()? as usize;
                    self.segment(reader.take(length)?, &mut output, expected)?;
                }
                reader.finish()?;
                if output.len() != expected {
                    return Err(Error::Invalid("ZGFX multipart size"));
                }
            }
            _ => return Err(Error::Invalid("ZGFX segment descriptor")),
        }
        // Commit only complete messages. Current-message matches read directly
        // from output; earlier messages remain in the persistent ring.
        self.remember(&output);
        Ok(output)
    }

    fn segment(&self, data: &[u8], output: &mut Vec<u8>, limit: usize) -> Result<()> {
        let (&header, bytes) = data.split_first().ok_or(Error::Truncated)?;
        if header & 15 != 4 {
            return Err(Error::Invalid("ZGFX compression type"));
        }
        let ceiling = output.len().saturating_add(SEGMENT_BYTES).min(limit);
        if header & 0x20 == 0 {
            grow(output, bytes.len(), ceiling)?;
            output.extend_from_slice(bytes);
            return Ok(());
        }
        let mut bits = Bits::new(bytes)?;
        while bits.position != bits.end {
            match bits.token()? {
                Token::Literal(value) => {
                    grow(output, 1, ceiling)?;
                    output.push(value);
                }
                Token::Distance(0) => {
                    let count = bits.read(15)? as usize;
                    let raw = bits.aligned_bytes(count)?;
                    grow(output, count, ceiling)?;
                    output.extend_from_slice(raw);
                }
                Token::Distance(distance) => {
                    let length = bits.length()?;
                    if distance > HISTORY_BYTES
                        || distance > self.available.saturating_add(output.len())
                    {
                        return Err(Error::Invalid("ZGFX match distance"));
                    }
                    grow(output, length, ceiling)?;
                    self.copy_match(output, distance, length);
                }
            }
        }
        Ok(())
    }

    fn copy_match(&self, output: &mut Vec<u8>, distance: usize, mut length: usize) {
        if distance > output.len() {
            let previous = distance - output.len();
            let mut start = (self.cursor + HISTORY_BYTES - previous) % HISTORY_BYTES;
            let mut count = previous.min(length);
            length -= count;
            while count != 0 {
                let copied = count.min(HISTORY_BYTES - start);
                output.extend_from_slice(&self.history[start..start + copied]);
                count -= copied;
                start = 0;
            }
        }
        if length != 0 {
            let start = output.len() - distance;
            // Each copy doubles the available repeating prefix, including
            // overlapping matches with a distance smaller than their length.
            while length != 0 {
                let count = length.min(output.len() - start);
                output.extend_from_within(start..start + count);
                length -= count;
            }
        }
    }

    fn remember(&mut self, bytes: &[u8]) {
        if bytes.is_empty() {
            return;
        }
        if self.history.is_empty() {
            self.history.resize(HISTORY_BYTES, 0);
        }
        self.available = self
            .available
            .saturating_add(bytes.len())
            .min(HISTORY_BYTES);
        if bytes.len() >= HISTORY_BYTES {
            self.history
                .copy_from_slice(&bytes[bytes.len() - HISTORY_BYTES..]);
            self.cursor = 0;
        } else {
            let first = bytes.len().min(HISTORY_BYTES - self.cursor);
            self.history[self.cursor..self.cursor + first].copy_from_slice(&bytes[..first]);
            self.history[..bytes.len() - first].copy_from_slice(&bytes[first..]);
            self.cursor = (self.cursor + bytes.len()) % HISTORY_BYTES;
        }
    }
}

fn grow(output: &mut Vec<u8>, additional: usize, limit: usize) -> Result<()> {
    let required = output
        .len()
        .checked_add(additional)
        .ok_or(Error::Limit("ZGFX expansion"))?;
    if required > limit {
        return Err(Error::Limit("ZGFX expansion"));
    }
    if required > output.capacity() {
        let capacity = required.max(output.capacity().saturating_mul(2)).min(limit);
        output.reserve_exact(capacity - output.len());
    }
    Ok(())
}

enum Token {
    Literal(u8),
    Distance(usize),
}

struct Bits<'a> {
    data: &'a [u8],
    position: usize,
    end: usize,
}

impl<'a> Bits<'a> {
    fn new(data: &'a [u8]) -> Result<Self> {
        let (&trailer, data) = data.split_last().ok_or(Error::Truncated)?;
        let end = data
            .len()
            .checked_mul(8)
            .and_then(|length| length.checked_sub(usize::from(trailer & 7)))
            .ok_or(Error::Truncated)?;
        Ok(Self {
            data,
            position: 0,
            end,
        })
    }

    fn read(&mut self, mut count: usize) -> Result<u32> {
        if count > 24 || count > self.end - self.position {
            return Err(Error::Truncated);
        }
        let mut value = 0u32;
        while count != 0 {
            let available = 8 - self.position % 8;
            let take = available.min(count);
            let mask = (1u32 << take) - 1;
            value = (value << take)
                | ((u32::from(self.data[self.position / 8]) >> (available - take)) & mask);
            count -= take;
            self.position += take;
        }
        Ok(value)
    }

    fn aligned_bytes(&mut self, count: usize) -> Result<&'a [u8]> {
        let start = self.position.div_ceil(8);
        let end = start.checked_add(count).ok_or(Error::Truncated)?;
        let bit_end = end.checked_mul(8).ok_or(Error::Truncated)?;
        if bit_end > self.end {
            return Err(Error::Truncated);
        }
        self.position = bit_end;
        self.data.get(start..end).ok_or(Error::Truncated)
    }

    fn token(&mut self) -> Result<Token> {
        if self.read(1)? == 0 {
            let value = self.read(8)? as u8;
            if matches!(value, 0..=12 | 0x38..=0x40 | 0x66 | 0x80 | 0xff) {
                return Err(Error::Invalid("ZGFX reserved literal"));
            }
            return Ok(Token::Literal(value));
        }
        // Prefixes and base distances are the wire constants in
        // MS-RDPEGFX 3.1.9.1.2.4, independent of machine byte order.
        let mut prefix = 16 | self.read(4)?;
        for length in 5..=8 {
            let distance = match (length, prefix) {
                (5, 17) => Some((0, 5)),
                (5, 18) => Some((32, 7)),
                (5, 19) => Some((160, 9)),
                (5, 20) => Some((672, 10)),
                (5, 21) => Some((1696, 12)),
                (6, 44) => Some((5792, 14)),
                (6, 45) => Some((22176, 15)),
                (7, 92) => Some((54944, 18)),
                (7, 93) => Some((317088, 20)),
                (8, 188) => Some((1365664, 20)),
                (8, 189) => Some((2414240, 21)),
                _ => None,
            };
            if let Some((base, count)) = distance {
                return Ok(Token::Distance(base + self.read(count)? as usize));
            }
            let literal = match (length, prefix) {
                (5, 24..=25) => Some((prefix - 24) as u8),
                (6, 52..=53) => Some((prefix - 50) as u8),
                (6, 54) => Some(255),
                (7, 110..=125) => Some(
                    [
                        4, 5, 6, 7, 8, 9, 10, 11, 0x3a, 0x3b, 0x3c, 0x3d, 0x3e, 0x3f, 0x40, 0x80,
                    ][(prefix - 110) as usize],
                ),
                (8, 252..=255) => Some([12, 0x38, 0x39, 0x66][(prefix - 252) as usize]),
                _ => None,
            };
            if let Some(value) = literal {
                return Ok(Token::Literal(value));
            }
            if length != 8 {
                prefix = (prefix << 1) | self.read(1)?;
            }
        }
        Err(Error::Invalid("ZGFX reserved token"))
    }

    fn length(&mut self) -> Result<usize> {
        let mut ones = 0;
        while self.read(1)? != 0 {
            ones += 1;
            if ones >= 15 {
                return Err(Error::Invalid("ZGFX match length"));
            }
        }
        if ones == 0 {
            Ok(3)
        } else {
            Ok((1usize << (ones + 1)) + self.read(ones + 1)? as usize)
        }
    }
}

#[cfg(test)]
mod tests;
