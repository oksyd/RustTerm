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

const AAC_RATES: [u32; 13] = [
    96000, 88200, 64000, 48000, 44100, 32000, 24000, 22050, 16000, 12000, 11025, 8000, 7350,
];

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
            // MS-RDPEA / MS-RDPEAI's WAVE_FORMAT_AAC (0xa106) carries raw AAC
            // access units. An ADTS header is not part of this wire format.
            transport: Transport::Raw,
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

    pub fn encode(&mut self, samples: &[u8]) -> Result<Vec<Vec<u8>>> {
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
        let mut output_bytes = 0;
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
            if encoded.output_size > (256 * 1024usize).saturating_sub(output_bytes) {
                return Err(Error::Limit("AAC capture output"));
            }
            // Raw AAC has no frame-length header. Each encoder access unit
            // must remain a separate RDPEAI DATA packet.
            if encoded.output_size != 0 {
                output.push(self.packet[..encoded.output_size].to_vec());
                output_bytes += encoded.output_size;
            }
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
    aac: Option<AacTransport>,
}

enum AacTransport {
    Raw(fdk_aac::dec::Decoder),
    Adts,
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
            aac: None,
        })
    }

    pub fn decode(&mut self, data: &[u8], limit: usize) -> Result<Vec<u8>> {
        let mut output = Vec::new();
        if self.format.encoding == Encoding::Aac {
            if data.is_empty() {
                return Err(Error::Invalid("empty AAC packet"));
            }
            if self.aac.is_none() {
                // Raw AAC elements cannot start with the ADTS sync byte. Keep
                // framing fixed for this stream, including fragmented headers.
                self.aac = Some(if data[0] == 0xff {
                    AacTransport::Adts
                } else {
                    let rate = AAC_RATES
                        .iter()
                        .position(|&rate| rate == self.format.pcm.sample_rate)
                        .ok_or(Error::Unsupported("AAC sample rate"))?
                        as u8;
                    let config = [
                        (2 << 3) | (rate >> 1),
                        (rate << 7) | ((self.format.pcm.channels as u8) << 3),
                    ];
                    let mut decoder = fdk_aac::dec::Decoder::new(fdk_aac::dec::Transport::Raw);
                    decoder
                        .config_raw(&config)
                        .map_err(|_| Error::Unsupported("AAC raw configuration"))?;
                    AacTransport::Raw(decoder)
                });
            }
            if let Some(AacTransport::Raw(decoder)) = &mut self.aac {
                return decode_raw(decoder, self.format, &mut self.scratch, data, limit);
            }
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

fn decode_raw(
    decoder: &mut fdk_aac::dec::Decoder,
    format: WaveFormat,
    scratch: &mut Vec<i16>,
    data: &[u8],
    limit: usize,
) -> Result<Vec<u8>> {
    if data.len() > limit {
        return Err(Error::Limit("AAC audio payload"));
    }
    scratch.resize(1024 * usize::from(format.pcm.channels), 0);
    let consumed = decoder
        .fill(data)
        .map_err(|_| Error::Invalid("AAC raw payload"))?;
    if consumed != data.len() {
        return Err(Error::Limit("buffered AAC data"));
    }
    // Unlike ADTS, raw AAC is packet-delimited. Do not merge truncated access
    // units with the next RDP packet or silently discard concatenated frames.
    decoder
        .decode_frame(scratch)
        .map_err(|_| Error::Invalid("AAC raw payload"))?;
    let info = decoder.stream_info();
    if info.sampleRate != format.pcm.sample_rate as i32
        || info.numChannels != i32::from(format.pcm.channels)
        || info.frameSize != 1024
    {
        return Err(Error::Invalid("decoded AAC format"));
    }
    if scratch.len() > limit / 2 {
        return Err(Error::Limit("decoded audio block"));
    }
    Ok(scratch
        .iter()
        .flat_map(|sample| sample.to_le_bytes())
        .collect())
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
        if bytes.len() < 7 {
            return Err(Error::Truncated);
        }
        if bytes[0] != 0xff || bytes[1] & 0xf6 != 0xf0 {
            return Err(Error::Invalid("AAC ADTS header"));
        }
        let profile = (bytes[2] >> 6) + 1;
        let rate_index = usize::from((bytes[2] >> 2) & 15);
        let channels = u16::from(((bytes[2] & 1) << 2) | (bytes[3] >> 6));
        if profile != 2
            || AAC_RATES.get(rate_index) != Some(&format.pcm.sample_rate)
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
    fn microsoft_aac_raw_access_units_decode_without_adts_headers() {
        let frame = [0x00, 0xc8, 0x00, 0x07];
        let mut decoder = Decoder::new(format(Encoding::Aac, 1)).unwrap();
        for _ in 0..3 {
            assert_eq!(decoder.decode(&frame, 4096).unwrap(), vec![0; 2048]);
        }
        assert!(decoder.decode(&frame, 2047).is_err());
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
    fn captured_pcm_is_encoded_as_decodable_raw_aac_across_small_capture_packets() {
        let format = format(Encoding::Aac, 1);
        let mut encoder = Encoder::new(format).unwrap();
        let mut decoder = Decoder::new(format).unwrap();
        let mut pcm_bytes = 0;
        // Less than a quarter second, no wall-clock waits or audio device.
        for _ in 0..32 {
            for packet in encoder.encode(&[0; 320 * 2]).unwrap() {
                assert_ne!(packet[0], 0xff, "RDP AAC capture must not add ADTS headers");
                pcm_bytes += decoder.decode(&packet, 65536).unwrap().len();
            }
        }
        assert!(pcm_bytes >= 1024 * 2);
        assert!(decoder.pending.is_empty());
    }

    #[test]
    fn raw_aac_preserves_stereo_access_unit_boundaries_for_large_capture_buffers() {
        for channels in [1, 2] {
            for sample_rate in [16000, 44100, 48000] {
                let mut format = format(Encoding::Aac, 1);
                format.pcm.channels = channels;
                format.pcm.sample_rate = sample_rate;
                format.byte_rate = 8000 * u32::from(channels);
                let mut encoder = Encoder::new(format).unwrap();
                let mut decoder = Decoder::new(format).unwrap();
                let tone = (0..4096)
                    .flat_map(|frame| {
                        let sample = ((frame as f32 * 440.0 * std::f32::consts::TAU
                            / sample_rate as f32)
                            .sin()
                            * 8000.0) as i16;
                        (0..channels).flat_map(move |_| sample.to_le_bytes())
                    })
                    .collect::<Vec<_>>();
                let packets = encoder.encode(&tone).unwrap();
                assert!(packets.len() >= 2);
                let mut audible = false;
                for packet in packets {
                    let pcm = decoder.decode(&packet, 65536).unwrap();
                    assert_eq!(pcm.len(), 1024 * usize::from(channels) * 2);
                    audible |= pcm
                        .as_chunks::<2>()
                        .0
                        .iter()
                        .any(|bytes| i16::from_le_bytes(*bytes).unsigned_abs() > 1000);
                }
                assert!(audible, "AAC must preserve the captured signal");
            }
        }
    }
}
