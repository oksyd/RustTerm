//! MS-RDPEA: negotiated RDPSND decoding with caller-owned PCM consumption.

pub(crate) mod format;

use std::collections::BTreeMap;

use crate::{Error, Result, wire::Reader};

use super::{Action, ChannelEvent};
use format::{Decoder, WaveFormat};

const CLOSE: u8 = 1;
const WAVE: u8 = 2;
const VOLUME: u8 = 3;
const WAVE_CONFIRM: u8 = 5;
const TRAINING: u8 = 6;
const FORMATS: u8 = 7;
const QUALITY: u8 = 12;
const WAVE2: u8 = 13;
const MAX_FORMATS: usize = 256;
const MAX_PENDING: usize = 64;

/// An accepted signed 16-bit little-endian, interleaved PCM format.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct AudioFormat {
    /// Samples per second in each channel.
    pub sample_rate: u32,
    /// One channel for mono, two for stereo.
    pub channels: u16,
}

/// Identifies one audio block awaiting caller consumption.
///
/// Return this token to `Session::confirm_audio` after consuming or deliberately
/// discarding the corresponding samples. Tokens from an earlier session must
/// never be submitted to a new session.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct AudioToken(u64);

/// A complete PCM block, with no operating-system playback side effects.
#[derive(Debug, PartialEq, Eq)]
pub struct AudioSamples {
    /// The negotiated PCM encoding of `data`.
    pub format: AudioFormat,
    /// Acknowledgment identity, independent of the server's wrapping block ID.
    pub token: AudioToken,
    /// Signed 16-bit little-endian PCM samples, interleaved for stereo.
    pub data: Vec<u8>,
}

/// Playback operations performed by the application, in event order.
#[derive(Debug, PartialEq, Eq)]
pub enum AudioEvent {
    /// Consume a block, then explicitly acknowledge it.
    Samples(AudioSamples),
    /// Apply the server's channel volume settings to subsequent samples.
    Volume {
        /// Left-channel setting: zero is silence and `u16::MAX` is full volume.
        left: u16,
        /// Right-channel setting: zero is silence and `u16::MAX` is full volume.
        right: u16,
    },
    /// Finish queued playback and release its device. Formats remain negotiated.
    Close,
}

struct WaveInfo {
    format: usize,
    timestamp: u16,
    block: u8,
    length: usize,
    first: [u8; 4],
}

struct Confirmation {
    timestamp: u16,
    block: u8,
}

pub(super) struct Audio {
    max_message: usize,
    formats: Vec<WaveFormat>,
    decoder: Option<(usize, Decoder)>,
    version: Option<u16>,
    wave: Option<WaveInfo>,
    next_token: u64,
    pending: BTreeMap<AudioToken, Confirmation>,
}

impl Audio {
    pub(super) fn new(max_message: usize) -> Self {
        Self {
            max_message,
            formats: Vec::new(),
            decoder: None,
            version: None,
            wave: None,
            next_token: 0,
            pending: BTreeMap::new(),
        }
    }

    pub(super) fn receive(&mut self, data: &[u8]) -> Result<Vec<Action>> {
        if data.len() > self.max_message {
            return Err(Error::Limit("audio message"));
        }
        // SNDWAV has no SNDPROLOG: its first four bytes are zero padding and
        // replace the four samples bytes carried by the preceding SNDWAVINFO.
        if let Some(wave) = self.wave.take() {
            if data.len() != wave.length || data.get(..4) != Some(&[0; 4]) {
                return Err(Error::Invalid("audio wave continuation"));
            }
            let mut pcm = data.to_vec();
            pcm[..4].copy_from_slice(&wave.first);
            return self.samples(wave.format, wave.timestamp, wave.block, pcm);
        }
        let mut reader = Reader::new(data);
        let kind = reader.u8()?;
        reader.skip(1)?;
        let size = usize::from(reader.u16_le()?);
        if kind == WAVE {
            if self.version.is_none() || size < 12 || data.len() != 16 {
                return Err(Error::Invalid("audio wave info"));
            }
            let timestamp = reader.u16_le()?;
            let format = self.format(reader.u16_le()?)?;
            let block = reader.u8()?;
            reader.skip(3)?;
            let first = reader.take(4)?.try_into().map_err(|_| Error::Truncated)?;
            let length = size - 8;
            self.validate_samples(format, length)?;
            self.wave = Some(WaveInfo {
                format,
                timestamp,
                block,
                length,
                first,
            });
            return Ok(Vec::new());
        }
        if size != reader.remaining_len() {
            return Err(Error::Invalid("audio PDU length"));
        }
        if kind == FORMATS {
            return self.negotiate(reader);
        }
        if self.version.is_none() {
            // MS-RDPEA 3.1.5: ignore out-of-sequence messages.
            return Ok(Vec::new());
        }
        match kind {
            TRAINING => {
                let fields = reader.take(4)?;
                // wPackSize is echoed, not used as an allocation size.
                Ok(vec![Action::Send(pdu(TRAINING, fields))])
            }
            WAVE2 => {
                if self.version.is_none_or(|version| version < 8) {
                    return Err(Error::Invalid("audio wave2 version"));
                }
                let timestamp = reader.u16_le()?;
                let format = self.format(reader.u16_le()?)?;
                let block = reader.u8()?;
                reader.skip(7)?; // Padding and the optional audio/video clock.
                self.validate_samples(format, reader.remaining_len())?;
                self.samples(format, timestamp, block, reader.remaining().to_vec())
            }
            VOLUME => {
                let left = reader.u16_le()?;
                let right = reader.u16_le()?;
                reader.finish()?;
                Ok(vec![Action::Event(ChannelEvent::Audio(
                    AudioEvent::Volume { left, right },
                ))])
            }
            CLOSE => {
                reader.finish()?;
                self.decoder = None;
                Ok(vec![Action::Event(ChannelEvent::Audio(AudioEvent::Close))])
            }
            // UDP, encryption and pitch are not advertised. Ignore unsolicited
            // optional messages as required by MS-RDPEA 3.1.5.
            _ => Ok(Vec::new()),
        }
    }

    pub(super) fn confirm(&mut self, token: AudioToken, elapsed_ms: u32) -> Result<Vec<Action>> {
        let Some(confirmation) = self.pending.remove(&token) else {
            // A new FORMATS sequence cancels queued playback. Its in-flight
            // worker completions must not acknowledge newer server blocks.
            return Ok(Vec::new());
        };
        let mut body = Vec::with_capacity(4);
        body.extend_from_slice(
            &confirmation
                .timestamp
                .wrapping_add(elapsed_ms as u16)
                .to_le_bytes(),
        );
        body.extend_from_slice(&[confirmation.block, 0]);
        Ok(vec![Action::Send(pdu(WAVE_CONFIRM, &body))])
    }

    fn negotiate(&mut self, mut reader: Reader<'_>) -> Result<Vec<Action>> {
        reader.skip(14)?; // Server flags, volume, pitch and UDP port are unused.
        let count = usize::from(reader.u16_le()?);
        reader.skip(1)?;
        let version = reader.u16_le()?;
        reader.skip(1)?;
        if count > MAX_FORMATS {
            return Err(Error::Limit("audio format count"));
        }
        if version < 2 {
            return Err(Error::Unsupported("audio protocol version"));
        }
        let mut formats = Vec::new();
        let mut encoded = Vec::new();
        for _ in 0..count {
            let bytes = reader.remaining();
            if let Some(format) = WaveFormat::read(&mut reader)? {
                formats.push(format);
                encoded.extend_from_slice(&bytes[..bytes.len() - reader.remaining_len()]);
            }
        }
        reader.finish()?;
        let version = version.min(8);
        let mut body = Vec::with_capacity(20 + encoded.len());
        body.extend_from_slice(&3u32.to_le_bytes()); // ALIVE | VOLUME, no pitch.
        body.extend_from_slice(&u32::MAX.to_le_bytes());
        body.extend_from_slice(&0u32.to_le_bytes());
        body.extend_from_slice(&0u16.to_be_bytes()); // All data uses RDPSND, never UDP.
        body.extend_from_slice(&(formats.len() as u16).to_le_bytes());
        body.push(0);
        body.extend_from_slice(&version.to_le_bytes());
        body.push(0);
        body.extend_from_slice(&encoded);
        let mut actions = Vec::new();
        if self.version.is_some() {
            actions.push(Action::Event(ChannelEvent::Audio(AudioEvent::Close)));
        }
        self.pending.clear();
        self.wave = None;
        self.formats = formats;
        self.decoder = None;
        self.version = Some(version);
        actions.push(Action::Send(pdu(FORMATS, &body)));
        if version >= 6 {
            actions.push(Action::Send(pdu(QUALITY, &[2, 0, 0, 0])));
        }
        if self.formats.is_empty() {
            actions.push(Action::Event(ChannelEvent::RequestRejected(
                Error::Unsupported("audio format"),
            )));
        }
        Ok(actions)
    }

    fn format(&self, index: u16) -> Result<usize> {
        let index = usize::from(index);
        if index < self.formats.len() {
            Ok(index)
        } else {
            Err(Error::Invalid("audio format index"))
        }
    }

    fn validate_samples(&self, index: usize, length: usize) -> Result<()> {
        self.formats[index].validate_packet(length, self.max_message)
    }

    fn samples(
        &mut self,
        index: usize,
        timestamp: u16,
        block: u8,
        data: Vec<u8>,
    ) -> Result<Vec<Action>> {
        self.validate_samples(index, data.len())?;
        if self.pending.len() >= MAX_PENDING {
            return Err(Error::Backpressure);
        }
        let format = self.formats[index];
        if self
            .decoder
            .as_ref()
            .is_none_or(|(previous, _)| *previous != index)
        {
            self.decoder = Some((index, Decoder::new(format)?));
        }
        let (_, decoder) = self
            .decoder
            .as_mut()
            .ok_or(Error::Invalid("audio decoder"))?;
        let data = decoder.decode(data, self.max_message)?;
        self.next_token = self
            .next_token
            .checked_add(1)
            .ok_or(Error::Limit("audio sequence"))?;
        let token = AudioToken(self.next_token);
        self.pending
            .insert(token, Confirmation { timestamp, block });
        // A transport packet may end part-way through an AAC frame. The bytes
        // are now owned by the bounded decoder buffer and can be acknowledged.
        if data.is_empty() {
            return self.confirm(token, 0);
        }
        Ok(vec![Action::Event(ChannelEvent::Audio(
            AudioEvent::Samples(AudioSamples {
                format: format.pcm,
                token,
                data,
            }),
        ))])
    }
}

fn pdu(kind: u8, body: &[u8]) -> Vec<u8> {
    let mut data = Vec::with_capacity(body.len() + 4);
    data.extend_from_slice(&[kind, 0]);
    data.extend_from_slice(&(body.len() as u16).to_le_bytes());
    data.extend_from_slice(body);
    data
}

#[cfg(test)]
mod tests {
    use super::*;

    fn format(tag: u16, channels: u16, rate: u32) -> Vec<u8> {
        let mut bytes = Vec::new();
        bytes.extend_from_slice(&tag.to_le_bytes());
        bytes.extend_from_slice(&channels.to_le_bytes());
        bytes.extend_from_slice(&rate.to_le_bytes());
        bytes.extend_from_slice(&(rate * u32::from(channels) * 2).to_le_bytes());
        bytes.extend_from_slice(&(channels * 2).to_le_bytes());
        bytes.extend_from_slice(&16u16.to_le_bytes());
        bytes.extend_from_slice(&0u16.to_le_bytes());
        bytes
    }

    fn formats(entries: &[Vec<u8>]) -> Vec<u8> {
        let mut body = vec![0; 14];
        body.extend_from_slice(&(entries.len() as u16).to_le_bytes());
        body.extend_from_slice(&[0, 8, 0, 0]);
        for format in entries {
            body.extend_from_slice(format);
        }
        pdu(FORMATS, &body)
    }

    fn ready() -> Audio {
        let mut audio = Audio::new(65539);
        audio.receive(&formats(&[format(1, 2, 48000)])).unwrap();
        audio
    }

    fn take_samples(actions: Vec<Action>) -> AudioSamples {
        match actions.into_iter().next().unwrap() {
            Action::Event(ChannelEvent::Audio(AudioEvent::Samples(samples))) => samples,
            _ => panic!("expected samples"),
        }
    }

    fn wave2(index: u16, timestamp: u16, block: u8, samples: &[u8]) -> Vec<u8> {
        let mut body = Vec::new();
        body.extend_from_slice(&timestamp.to_le_bytes());
        body.extend_from_slice(&index.to_le_bytes());
        body.extend_from_slice(&[block, 1, 2, 3]);
        body.extend_from_slice(&12345u32.to_le_bytes());
        body.extend_from_slice(samples);
        pdu(WAVE2, &body)
    }

    #[test]
    fn negotiation_filters_formats_and_indexes_the_client_list() {
        let mut audio = Audio::new(65539);
        let actions = audio
            .receive(&formats(&[
                format(7, 2, 48000),
                format(1, 1, 16000),
                format(1, 6, 48000),
            ]))
            .unwrap();
        let Action::Send(response) = &actions[0] else {
            panic!("expected formats")
        };
        assert_eq!(response[0], FORMATS);
        assert_eq!(&response[18..20], &1u16.to_le_bytes());
        assert_eq!(&response[24..], format(1, 1, 16000));
        let samples = take_samples(audio.receive(&wave2(0, 5, 7, &[1, 2])).unwrap());
        assert_eq!(
            samples.format,
            AudioFormat {
                channels: 1,
                sample_rate: 16000
            }
        );
        assert!(audio.receive(&wave2(1, 5, 8, &[1, 2])).is_err());
    }

    #[test]
    fn legacy_wave_restores_the_first_four_bytes_and_waits_for_consumption() {
        let mut audio = ready();
        let info = [WAVE, 55, 16, 0, 0xfa, 0xff, 0, 0, 9, 4, 5, 6, 1, 2, 3, 4];
        assert!(audio.receive(&info).unwrap().is_empty());
        let samples = take_samples(audio.receive(&[0, 0, 0, 0, 5, 6, 7, 8]).unwrap());
        assert_eq!(samples.data, [1, 2, 3, 4, 5, 6, 7, 8]);
        let actions = audio.confirm(samples.token, 10).unwrap();
        let Action::Send(ack) = &actions[0] else {
            panic!("expected ack")
        };
        assert_eq!(ack, &[WAVE_CONFIRM, 0, 4, 0, 4, 0, 9, 0]);
        assert!(audio.confirm(samples.token, 10).unwrap().is_empty());
    }

    #[test]
    fn compressed_playback_uses_the_negotiated_wire_format_and_pcm_output() {
        let mut audio = Audio::new(65536);
        let mut mulaw = format(7, 1, 16000);
        mulaw[8..12].copy_from_slice(&16000u32.to_le_bytes());
        mulaw[12..14].copy_from_slice(&1u16.to_le_bytes());
        mulaw[14..16].copy_from_slice(&8u16.to_le_bytes());
        let actions = audio.receive(&formats(&[mulaw.clone()])).unwrap();
        let Action::Send(response) = &actions[0] else {
            panic!("formats response")
        };
        assert_eq!(&response[24..], mulaw);
        let samples = take_samples(audio.receive(&wave2(0, 10, 2, &[0x80, 0x00])).unwrap());
        assert_eq!(
            samples.data,
            [32124i16, -32124]
                .into_iter()
                .flat_map(i16::to_le_bytes)
                .collect::<Vec<_>>()
        );
        assert_eq!(
            samples.format,
            AudioFormat {
                sample_rate: 16000,
                channels: 1
            }
        );
        assert_eq!(audio.confirm(samples.token, 5).unwrap().len(), 1);
    }

    #[test]
    fn training_echoes_fields_without_returning_training_payload() {
        let mut audio = ready();
        let actions = audio
            .receive(&pdu(TRAINING, &[10, 20, 12, 0, 1, 2, 3, 4]))
            .unwrap();
        let Action::Send(response) = &actions[0] else {
            panic!("expected confirm")
        };
        assert_eq!(response, &[TRAINING, 0, 4, 0, 10, 20, 12, 0]);
    }

    #[test]
    fn renegotiation_does_not_acknowledge_a_new_block_with_an_old_token() {
        let mut audio = ready();
        let old = take_samples(audio.receive(&wave2(0, 5, 1, &[0; 4])).unwrap());
        audio.receive(&formats(&[format(1, 2, 48000)])).unwrap();
        let new = take_samples(audio.receive(&wave2(0, 5, 1, &[0; 4])).unwrap());
        assert_ne!(old.token, new.token);
        assert!(audio.confirm(old.token, 2).unwrap().is_empty());
        assert_eq!(audio.confirm(new.token, 2).unwrap().len(), 1);
    }

    #[test]
    fn malformed_audio_lengths_and_unaligned_samples_are_rejected() {
        assert!(ready().receive(&wave2(0, 0, 0, &[1, 2])).is_err());
        assert!(ready().receive(&[WAVE2, 0, 0, 0, 1]).is_err());
        let mut audio = ready();
        audio
            .receive(&[WAVE, 0, 16, 0, 0, 0, 0, 0, 0, 0, 0, 0, 1, 2, 3, 4])
            .unwrap();
        assert!(audio.receive(&[0, 0, 0, 0]).is_err());
        let mut truncated = formats(&[format(1, 2, 48000)]);
        truncated.pop();
        assert!(ready().receive(&truncated).is_err());
    }

    #[test]
    fn outstanding_acknowledgments_are_bounded() {
        let mut audio = ready();
        for block in 0..MAX_PENDING {
            audio.receive(&wave2(0, 0, block as u8, &[0; 4])).unwrap();
        }
        assert!(matches!(
            audio.receive(&wave2(0, 0, 99, &[0; 4])),
            Err(Error::Backpressure)
        ));
    }
}
