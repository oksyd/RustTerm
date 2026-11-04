use super::{Encoding, WaveFormat};
use crate::{Error, Result};
use symphonia_core::{
    audio::{
        GenericAudioBufferRef,
        layouts::{CHANNEL_LAYOUT_MONO, CHANNEL_LAYOUT_STEREO},
    },
    codecs::audio::{
        AudioCodecParameters, AudioDecoder,
        well_known::{CODEC_ID_AAC, CODEC_ID_ADPCM_IMA_WAV, CODEC_ID_ADPCM_MS},
    },
    packet::PacketRef,
    units::{Duration, Timestamp},
};

pub(crate) struct Encoder {
    codec: fdk_aac::enc::Encoder,
    pcm: Vec<i16>,
    packet: Vec<u8>,
}

impl Encoder {
    pub fn new(format: WaveFormat) -> Result<Self> {
        use fdk_aac::enc::{AudioObjectType, BitRate, ChannelMode, EncoderParams, Transport};
        let codec = fdk_aac::enc::Encoder::new(EncoderParams {
            bit_rate: BitRate::Cbr(
                format
                    .byte_rate
                    .checked_mul(8)
                    .ok_or(Error::Limit("AAC capture bitrate"))?,
            ),
            sample_rate: format.pcm.sample_rate,
            channels: if format.pcm.channels == 1 {
                ChannelMode::Mono
            } else {
                ChannelMode::Stereo
            },
            audio_object_type: AudioObjectType::Mpeg4LowComplexity,
            transport: Transport::Adts,
        })
        .map_err(|_| Error::Unsupported("AAC capture format"))?;
        let info = codec
            .info()
            .map_err(|_| Error::Invalid("AAC encoder information"))?;
        if info.maxOutBufBytes == 0 || info.maxOutBufBytes > 65536 {
            return Err(Error::Limit("AAC encoder output"));
        }
        Ok(Self {
            codec,
            pcm: Vec::new(),
            packet: vec![0; info.maxOutBufBytes as usize],
        })
    }

    pub fn encode(&mut self, samples: &[u8]) -> Result<Vec<u8>> {
        if samples.len() > 256 * 1024 || !samples.len().is_multiple_of(2) {
            return Err(Error::Limit("AAC capture samples"));
        }
        self.pcm.clear();
        self.pcm.extend(
            samples
                .as_chunks::<2>()
                .0
                .iter()
                .map(|bytes| i16::from_le_bytes(*bytes)),
        );
        let mut remaining = self.pcm.as_slice();
        let mut output = Vec::new();
        while !remaining.is_empty() {
            let encoded = self
                .codec
                .encode(remaining, &mut self.packet)
                .map_err(|_| Error::Invalid("AAC capture encoding"))?;
            if encoded.input_consumed == 0
                || encoded.input_consumed > remaining.len()
                || encoded.output_size > self.packet.len()
            {
                return Err(Error::Invalid("AAC encoder progress"));
            }
            if encoded.output_size > (256 * 1024usize).saturating_sub(output.len()) {
                return Err(Error::Limit("AAC capture output"));
            }
            output.extend_from_slice(&self.packet[..encoded.output_size]);
            remaining = &remaining[encoded.input_consumed..];
        }
        Ok(output)
    }
}

pub(in crate::channels::audio) struct Decoder {
    format: WaveFormat,
    codec: Box<dyn AudioDecoder>,
    pending: Vec<u8>,
    scratch: Vec<i16>,
}

impl Decoder {
    pub fn new(format: WaveFormat) -> Result<Self> {
        let frames = match format.encoding {
            Encoding::MsAdpcm { frames } | Encoding::ImaAdpcm { frames } => u64::from(frames),
            _ => 1024,
        };
        let params = AudioCodecParameters {
            codec: match format.encoding {
                Encoding::MsAdpcm { .. } => CODEC_ID_ADPCM_MS,
                Encoding::ImaAdpcm { .. } => CODEC_ID_ADPCM_IMA_WAV,
                Encoding::Aac => CODEC_ID_AAC,
                _ => return Err(Error::Invalid("compressed audio encoding")),
            },
            sample_rate: Some(format.pcm.sample_rate),
            channels: Some(if format.pcm.channels == 1 {
                CHANNEL_LAYOUT_MONO
            } else {
                CHANNEL_LAYOUT_STEREO
            }),
            max_frames_per_packet: Some(frames),
            frames_per_block: Some(frames),
            ..AudioCodecParameters::default()
        };
        let codec: Box<dyn AudioDecoder> = if format.encoding == Encoding::Aac {
            Box::new(
                symphonia_codec_aac::AacDecoder::try_new(&params, &Default::default())
                    .map_err(|_| Error::Unsupported("AAC audio format"))?,
            )
        } else {
            Box::new(
                symphonia_codec_adpcm::AdpcmDecoder::try_new(&params, &Default::default())
                    .map_err(|_| Error::Unsupported("ADPCM audio format"))?,
            )
        };
        Ok(Self {
            format,
            codec,
            pending: Vec::new(),
            scratch: Vec::new(),
        })
    }

    pub fn decode(&mut self, data: &[u8], limit: usize) -> Result<Vec<u8>> {
        let mut output = Vec::new();
        if self.format.encoding == Encoding::Aac {
            if data.len() > limit.saturating_sub(self.pending.len()) {
                return Err(Error::Limit("buffered AAC data"));
            }
            self.pending.extend_from_slice(data);
            let mut consumed = 0;
            while self.pending.len() - consumed >= 7 {
                let bytes = &self.pending[consumed..];
                let header = AdtsHeader::parse(bytes, self.format)?;
                if bytes.len() < header.length {
                    break;
                }
                let packet = PacketRef::new(
                    0,
                    Timestamp::ZERO,
                    Duration::new(1024),
                    &bytes[header.header..header.length],
                );
                let decoded = self
                    .codec
                    .decode_ref(&packet)
                    .map_err(|_| Error::Invalid("AAC audio payload"))?;
                append(decoded, self.format, &mut self.scratch, &mut output, limit)?;
                consumed += header.length;
            }
            self.pending.drain(..consumed);
        } else {
            let frames = match self.format.encoding {
                Encoding::MsAdpcm { frames } | Encoding::ImaAdpcm { frames } => u64::from(frames),
                _ => return Err(Error::Invalid("ADPCM audio format")),
            };
            let decoded_bytes = (data.len() / usize::from(self.format.block_align)) as u64
                * frames
                * u64::from(self.format.pcm.channels)
                * 2;
            if decoded_bytes > limit as u64 {
                return Err(Error::Limit("decoded audio block"));
            }
            for block in data.chunks_exact(usize::from(self.format.block_align)) {
                let packet = PacketRef::new(0, Timestamp::ZERO, Duration::new(frames), block);
                let decoded = self
                    .codec
                    .decode_ref(&packet)
                    .map_err(|_| Error::Invalid("ADPCM audio payload"))?;
                append(decoded, self.format, &mut self.scratch, &mut output, limit)?;
            }
        }
        Ok(output)
    }
}

fn append(
    decoded: GenericAudioBufferRef<'_>,
    format: WaveFormat,
    scratch: &mut Vec<i16>,
    output: &mut Vec<u8>,
    limit: usize,
) -> Result<()> {
    if decoded.spec().rate() != format.pcm.sample_rate
        || decoded.spec().channels().count() != usize::from(format.pcm.channels)
    {
        return Err(Error::Invalid("decoded audio format"));
    }
    if decoded.samples_interleaved() > limit.saturating_sub(output.len()) / 2 {
        return Err(Error::Limit("decoded audio block"));
    }
    decoded.copy_to_vec_interleaved(scratch);
    output.extend(scratch.iter().flat_map(|sample| sample.to_le_bytes()));
    Ok(())
}

struct AdtsHeader {
    header: usize,
    length: usize,
}

impl AdtsHeader {
    fn parse(bytes: &[u8], format: WaveFormat) -> Result<Self> {
        const RATES: [u32; 13] = [
            96000, 88200, 64000, 48000, 44100, 32000, 24000, 22050, 16000, 12000, 11025, 8000, 7350,
        ];
        if bytes[0] != 0xff || bytes[1] & 0xf6 != 0xf0 {
            return Err(Error::Invalid("AAC ADTS header"));
        }
        let profile = (bytes[2] >> 6) + 1;
        let rate_index = usize::from((bytes[2] >> 2) & 15);
        let channels = u16::from(((bytes[2] & 1) << 2) | (bytes[3] >> 6));
        if profile != 2
            || RATES.get(rate_index) != Some(&format.pcm.sample_rate)
            || channels != format.pcm.channels
            || bytes[6] & 3 != 0
        {
            return Err(Error::Unsupported("AAC ADTS configuration"));
        }
        let header = if bytes[1] & 1 == 0 { 9 } else { 7 };
        let length = (usize::from(bytes[3] & 3) << 11)
            | (usize::from(bytes[4]) << 3)
            | usize::from(bytes[5] >> 5);
        if length <= header {
            return Err(Error::Invalid("AAC ADTS frame length"));
        }
        Ok(Self { header, length })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::AudioFormat;

    fn format(encoding: Encoding, align: u16) -> WaveFormat {
        WaveFormat {
            pcm: AudioFormat {
                sample_rate: 44100,
                channels: 1,
            },
            encoding,
            block_align: align,
            byte_rate: 8000,
        }
    }

    #[test]
    fn adpcm_blocks_decode_predictors_and_enforce_output_bounds() {
        let mut ima = Decoder::new(format(Encoding::ImaAdpcm { frames: 9 }, 8)).unwrap();
        let mut block = vec![0xe8, 0x03, 0, 0, 0, 0, 0, 0];
        assert_eq!(
            ima.decode(&block, 18).unwrap(),
            [1000i16; 9]
                .into_iter()
                .flat_map(i16::to_le_bytes)
                .collect::<Vec<_>>()
        );
        assert!(ima.decode(&block, 16).is_err());
        block[2] = 89;
        assert!(ima.decode(&block, 18).is_err());
        let mut ms = Decoder::new(format(Encoding::MsAdpcm { frames: 4 }, 8)).unwrap();
        assert_eq!(
            ms.decode(&[0, 16, 0, 0xe8, 3, 0xe8, 3, 0], 8).unwrap(),
            [1000i16; 4]
                .into_iter()
                .flat_map(i16::to_le_bytes)
                .collect::<Vec<_>>()
        );
    }

    #[test]
    fn aac_headers_bind_stream_configuration_and_reject_invalid_lengths() {
        let format = format(Encoding::Aac, 1);
        let header = [0xff, 0xf1, 0x50, 0x40, 0x01, 0x3f, 0xfc];
        assert_eq!(AdtsHeader::parse(&header, format).unwrap().length, 9);
        let mut invalid = header;
        invalid[2] = 0x10;
        assert!(AdtsHeader::parse(&invalid, format).is_err());
        invalid = header;
        invalid[4] = 0;
        assert!(AdtsHeader::parse(&invalid, format).is_err());
        let mut decoder = Decoder::new(format).unwrap();
        assert!(decoder.decode(&header[..3], 64).unwrap().is_empty());
        assert!(decoder.decode(&header[3..], 64).unwrap().is_empty());
        assert_eq!(decoder.pending, header);
        assert!(decoder.decode(&[0; 64], 64).is_err());
    }

    #[test]
    fn aac_lc_silence_survives_fragmentation_and_concatenated_frames() {
        // AAC-LC single-channel element, zero scale-factor bands, then ID_END.
        // ADTS describes one 1024-sample mono frame at 44.1 kHz.
        let frame = [
            0xff, 0xf1, 0x50, 0x40, 0x01, 0x7f, 0xfc, 0x00, 0xc8, 0x00, 0x07,
        ];
        let mut decoder = Decoder::new(format(Encoding::Aac, 1)).unwrap();
        assert!(decoder.decode(&frame[..5], 4096).unwrap().is_empty());
        assert_eq!(decoder.decode(&frame[5..], 4096).unwrap(), vec![0; 2048]);
        let pair = frame.repeat(2);
        assert_eq!(decoder.decode(&pair, 4096).unwrap(), vec![0; 4096]);
        assert!(decoder.pending.is_empty());
        assert!(decoder.decode(&frame, 2047).is_err());
    }

    #[test]
    fn captured_pcm_is_encoded_as_decodable_adts_across_small_capture_packets() {
        let format = format(Encoding::Aac, 1);
        let mut encoder = Encoder::new(format).unwrap();
        let mut decoder = Decoder::new(format).unwrap();
        let mut pcm_bytes = 0;
        // Less than a quarter second, no wall-clock waits or audio device.
        for _ in 0..32 {
            let packet = encoder.encode(&[0; 320 * 2]).unwrap();
            if !packet.is_empty() {
                pcm_bytes += decoder.decode(&packet, 65536).unwrap().len();
            }
        }
        assert!(pcm_bytes >= 1024 * 2);
        assert!(decoder.pending.is_empty());
    }
}
