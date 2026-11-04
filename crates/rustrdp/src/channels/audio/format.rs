use crate::{AudioFormat, Error, Result, wire::Reader};

#[cfg(feature = "audio-codecs")]
mod compressed;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Encoding {
    Pcm,
    ALaw,
    MuLaw,
    #[cfg(feature = "audio-codecs")]
    MsAdpcm {
        frames: u16,
    },
    #[cfg(feature = "audio-codecs")]
    ImaAdpcm {
        frames: u16,
    },
    #[cfg(feature = "audio-codecs")]
    Aac,
}

#[derive(Clone, Copy, Debug)]
pub(crate) struct WaveFormat {
    pub pcm: AudioFormat,
    pub encoding: Encoding,
    pub block_align: u16,
    #[cfg(feature = "audio-codecs")]
    pub byte_rate: u32,
}

impl WaveFormat {
    pub fn read(reader: &mut Reader<'_>) -> Result<Option<Self>> {
        let tag = reader.u16_le()?;
        let channels = reader.u16_le()?;
        let sample_rate = reader.u32_le()?;
        let byte_rate = reader.u32_le()?;
        let block_align = reader.u16_le()?;
        let bits = reader.u16_le()?;
        let extra_len = usize::from(reader.u16_le()?);
        let extra = reader.take(extra_len)?;
        if !matches!(channels, 1 | 2)
            || !matches!(
                sample_rate,
                8000 | 11025
                    | 12000
                    | 16000
                    | 22050
                    | 24000
                    | 32000
                    | 44100
                    | 48000
                    | 64000
                    | 88200
                    | 96000
            )
            || block_align == 0
            || byte_rate == 0
        {
            return Ok(None);
        }
        let encoding = match tag {
            1 if bits == 16
                && extra.is_empty()
                && block_align == channels * 2
                && byte_rate == sample_rate * u32::from(block_align) =>
            {
                Encoding::Pcm
            }
            6 | 7
                if bits == 8
                    && extra.is_empty()
                    && block_align == channels
                    && byte_rate == sample_rate * u32::from(channels) =>
            {
                if tag == 6 {
                    Encoding::ALaw
                } else {
                    Encoding::MuLaw
                }
            }
            #[cfg(feature = "audio-codecs")]
            2 if bits == 4 && extra.len() == 32 && block_align >= channels * 7 => {
                let frames = u16::from_le_bytes([extra[0], extra[1]]);
                let expected = (u32::from(block_align) - u32::from(channels) * 7) * 2
                    / u32::from(channels)
                    + 2;
                // Symphonia implements the standard Microsoft predictor table.
                const COEFFICIENTS: [(i16, i16); 7] = [
                    (256, 0),
                    (512, -256),
                    (0, 0),
                    (192, 64),
                    (240, 0),
                    (460, -208),
                    (392, -232),
                ];
                if u32::from(frames) != expected
                    || extra[2..4] != 7u16.to_le_bytes()
                    || !extra[4..].as_chunks::<4>().0.iter().zip(COEFFICIENTS).all(
                        |(bytes, (a, b))| {
                            bytes[..2] == a.to_le_bytes() && bytes[2..] == b.to_le_bytes()
                        },
                    )
                {
                    return Ok(None);
                }
                Encoding::MsAdpcm { frames }
            }
            #[cfg(feature = "audio-codecs")]
            0x11 if bits == 4
                && extra.len() == 2
                && block_align >= channels * 4
                && (block_align - channels * 4).is_multiple_of(channels * 4) =>
            {
                let frames = u16::from_le_bytes([extra[0], extra[1]]);
                let expected = (u32::from(block_align) - u32::from(channels) * 4) * 2
                    / u32::from(channels)
                    + 1;
                if u32::from(frames) != expected {
                    return Ok(None);
                }
                Encoding::ImaAdpcm { frames }
            }
            #[cfg(feature = "audio-codecs")]
            0xa106 if matches!(bits, 0 | 16) && extra.is_empty() => Encoding::Aac,
            _ => return Ok(None),
        };
        Ok(Some(Self {
            pcm: AudioFormat {
                sample_rate,
                channels,
            },
            encoding,
            block_align,
            #[cfg(feature = "audio-codecs")]
            byte_rate,
        }))
    }

    pub fn capture_supported(self) -> bool {
        match self.encoding {
            Encoding::Pcm | Encoding::ALaw | Encoding::MuLaw => true,
            #[cfg(feature = "audio-codecs")]
            Encoding::Aac => {
                (1000..=40000 * u32::from(self.pcm.channels)).contains(&self.byte_rate)
            }
            #[cfg(feature = "audio-codecs")]
            _ => false,
        }
    }

    pub fn validate_packet(self, length: usize, limit: usize) -> Result<()> {
        if length > limit {
            return Err(Error::Limit("audio sample block"));
        }
        #[cfg(feature = "audio-codecs")]
        if self.encoding == Encoding::Aac {
            return if length == 0 {
                Err(Error::Invalid("empty AAC packet"))
            } else {
                Ok(())
            };
        }
        if length == 0 || !length.is_multiple_of(usize::from(self.block_align)) {
            return Err(Error::Invalid("audio sample alignment"));
        }
        Ok(())
    }

    pub fn encode_capture(self, samples: &[u8]) -> Result<Vec<u8>> {
        match self.encoding {
            Encoding::Pcm => Ok(samples.to_vec()),
            Encoding::ALaw | Encoding::MuLaw => Ok(samples
                .as_chunks::<2>()
                .0
                .iter()
                .map(|bytes| {
                    let sample = i16::from_le_bytes(*bytes);
                    encode_g711(sample, self.encoding == Encoding::ALaw)
                })
                .collect()),
            #[cfg(feature = "audio-codecs")]
            _ => Err(Error::Unsupported("audio capture encoding")),
        }
    }
}

pub(crate) enum CaptureEncoder {
    Simple(WaveFormat),
    #[cfg(feature = "audio-codecs")]
    Aac(compressed::Encoder),
}

impl CaptureEncoder {
    pub fn new(format: WaveFormat) -> Result<Self> {
        #[cfg(feature = "audio-codecs")]
        if format.encoding == Encoding::Aac {
            return Ok(Self::Aac(compressed::Encoder::new(format)?));
        }
        if !format.capture_supported() {
            return Err(Error::Unsupported("audio capture encoding"));
        }
        Ok(Self::Simple(format))
    }

    pub fn encode(&mut self, samples: &[u8]) -> Result<Vec<Vec<u8>>> {
        match self {
            Self::Simple(format) => Ok(vec![format.encode_capture(samples)?]),
            #[cfg(feature = "audio-codecs")]
            Self::Aac(encoder) => encoder.encode(samples),
        }
    }
}

pub(super) enum Decoder {
    Pcm,
    Companded {
        alaw: bool,
    },
    #[cfg(feature = "audio-codecs")]
    Compressed(Box<compressed::Decoder>),
}

impl Decoder {
    pub fn new(format: WaveFormat) -> Result<Self> {
        Ok(match format.encoding {
            Encoding::Pcm => Self::Pcm,
            Encoding::ALaw => Self::Companded { alaw: true },
            Encoding::MuLaw => Self::Companded { alaw: false },
            #[cfg(feature = "audio-codecs")]
            _ => Self::Compressed(Box::new(compressed::Decoder::new(format)?)),
        })
    }

    pub fn decode(&mut self, data: Vec<u8>, limit: usize) -> Result<Vec<u8>> {
        match self {
            Self::Pcm => Ok(data),
            Self::Companded { alaw } => {
                if data.len() > limit / 2 {
                    return Err(Error::Limit("decoded audio block"));
                }
                Ok(data
                    .into_iter()
                    .flat_map(|byte| decode_g711(byte, *alaw).to_le_bytes())
                    .collect())
            }
            #[cfg(feature = "audio-codecs")]
            Self::Compressed(decoder) => decoder.decode(&data, limit),
        }
    }
}

// ITU-T G.711 logarithmic PCM. Encoding always starts from signed PCM16;
// the wire representation uses eight bits per sample in both directions.
fn decode_g711(byte: u8, alaw: bool) -> i16 {
    if alaw {
        let value = byte ^ 0x55;
        let exponent = (value >> 4) & 7;
        let mut magnitude = i32::from(value & 15) << 4;
        magnitude += if exponent == 0 { 8 } else { 0x108 };
        if exponent > 1 {
            magnitude <<= exponent - 1;
        }
        (if value & 0x80 != 0 {
            magnitude
        } else {
            -magnitude
        }) as i16
    } else {
        let value = !byte;
        let magnitude = ((i32::from(value & 15) << 3) + 0x84) << ((value >> 4) & 7);
        (if value & 0x80 != 0 {
            0x84 - magnitude
        } else {
            magnitude - 0x84
        }) as i16
    }
}

fn encode_g711(sample: i16, alaw: bool) -> u8 {
    let sample = i32::from(sample);
    if alaw {
        let sign = if sample >= 0 { 0x80 } else { 0 };
        let magnitude = if sample >= 0 { sample } else { -sample - 1 };
        let exponent = (24 - (magnitude as u32 | 0xff).leading_zeros()).min(7);
        let shift = if exponent == 0 { 4 } else { exponent + 3 };
        (sign | (exponent as u8) << 4 | ((magnitude >> shift) as u8 & 15)) ^ 0x55
    } else {
        let sign = if sample < 0 { 0x80 } else { 0 };
        let magnitude = sample.abs().min(32635) + 0x84;
        let exponent = (24 - (magnitude as u32 | 0xff).leading_zeros()).min(7);
        !(sign | (exponent as u8) << 4 | ((magnitude >> (exponent + 3)) as u8 & 15))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn g711_known_silence_endpoints_and_codeword_roundtrips() {
        assert_eq!(decode_g711(0xd5, true), 8);
        assert_eq!(decode_g711(0x55, true), -8);
        assert_eq!(decode_g711(0xff, false), 0);
        assert_eq!(decode_g711(0x7f, false), 0);
        assert_eq!(decode_g711(0x80, false), 32124);
        assert_eq!(decode_g711(0x00, false), -32124);
        assert_eq!(decode_g711(0xaa, true), 32256);
        assert_eq!(decode_g711(0x2a, true), -32256);
        for byte in 0..=u8::MAX {
            assert_eq!(encode_g711(decode_g711(byte, true), true), byte);
            let canonical = if byte == 0x7f { 0xff } else { byte };
            assert_eq!(encode_g711(decode_g711(byte, false), false), canonical);
        }
        assert_eq!(encode_g711(i16::MIN, false), 0);
        assert_eq!(encode_g711(i16::MAX, false), 0x80);
    }

    #[test]
    fn companded_decode_is_bounded_and_preserves_channel_order() {
        let mut decoder = Decoder::Companded { alaw: false };
        assert_eq!(
            decoder.decode(vec![0x80, 0x00, 0xff, 0x7f], 8).unwrap(),
            [32124i16, -32124, 0, 0]
                .into_iter()
                .flat_map(i16::to_le_bytes)
                .collect::<Vec<_>>()
        );
        assert!(decoder.decode(vec![0xff; 5], 8).is_err());
    }
}
