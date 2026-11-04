//! MS-RDPEAI microphone negotiation. The caller owns permission and the capture device.
use super::audio::format::{CaptureEncoder, WaveFormat};
use super::{Action, ChannelEvent};
use crate::{AudioFormat, Error, Result, wire::Reader};

/// A capture request has a monotonically increasing generation; stale samples are rejected.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CaptureRequest {
    /// Session-local identity for this capture format.
    pub generation: u64,
    /// Required signed 16-bit little-endian PCM format.
    pub format: AudioFormat,
    /// Exact number of frames to include in each sample packet.
    pub frames: u32,
}
/// Local capture lifecycle requested by the authenticated remote server.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CaptureEvent {
    /// Open the device only after caller permission and confirm the result.
    Open(CaptureRequest),
    /// Stop capture and release the device immediately.
    Close,
}

#[derive(Default)]
pub(super) struct AudioInput {
    version: Option<u32>,
    formats: Vec<WaveFormat>,
    request: Option<CaptureRequest>,
    index: u32,
    generation: u64,
    ready: bool,
    changing: bool,
    encoder: Option<CaptureEncoder>,
}
impl AudioInput {
    pub fn receive(&mut self, data: &[u8]) -> Result<Vec<Action>> {
        // MS-RDPEAI 3.1.5 requires malformed audio messages to be ignored.
        // Parsing commits negotiation state only after validating each message.
        match self.receive_message(data) {
            Err(Error::Truncated | Error::Invalid(_) | Error::Limit(_)) => Ok(Vec::new()),
            result => result,
        }
    }
    fn receive_message(&mut self, data: &[u8]) -> Result<Vec<Action>> {
        let mut r = Reader::new(data);
        match r.u8()? {
            1 if self.version.is_none() => {
                let version = r.u32_le()?;
                r.finish()?;
                if version == 0 {
                    return Err(Error::Invalid("audio input version"));
                }
                self.version = Some(version.min(2));
                Ok(vec![Action::Send(packet(1, version.min(2)))])
            }
            2 if self.version.is_some() && self.formats.is_empty() => {
                let count = r.u32_le()?;
                r.skip(4)?;
                if count == 0 || count > 256 {
                    return Err(Error::Limit("audio input formats"));
                }
                let mut encoded = Vec::new();
                let mut formats = Vec::new();
                for _ in 0..count {
                    let bytes = r.remaining();
                    if let Some(format) = WaveFormat::read(&mut r)?
                        && format.capture_supported()
                    {
                        formats.push(format);
                        encoded.extend_from_slice(&bytes[..bytes.len() - r.remaining_len()]);
                    }
                }
                r.finish()?;
                self.formats = formats;
                let mut response = packet(2, self.formats.len() as u32);
                response.extend_from_slice(&(9 + encoded.len() as u32).to_le_bytes());
                response.extend(encoded);
                Ok(vec![Action::Send(vec![5]), Action::Send(response)])
            }
            3 if !self.formats.is_empty() && self.request.is_none() => {
                let frames = r.u32_le()?;
                let index = r.u32_le()?;
                // The optional capture hint does not override the negotiated wire format.
                r.skip(16)?;
                let extra = usize::from(r.u16_le()?);
                r.skip(extra)?;
                r.finish()?;
                self.open(index, frames, false)
            }
            7 if self.request.is_some() && self.version == Some(2) => {
                let index = r.u32_le()?;
                r.finish()?;
                let frames = self
                    .request
                    .ok_or(Error::Invalid("audio input request"))?
                    .frames;
                self.open(index, frames, true)
            }
            // MS-RDPEAI requires ignoring out-of-sequence or unrecognized messages.
            _ => Ok(Vec::new()),
        }
    }
    fn open(&mut self, index: u32, frames: u32, changing: bool) -> Result<Vec<Action>> {
        let wire_format = *self
            .formats
            .get(index as usize)
            .ok_or(Error::Invalid("audio input format index"))?;
        let format = wire_format.pcm;
        if frames == 0
            || frames > format.sample_rate
            || u64::from(frames) * u64::from(format.channels) * 2 > 256 * 1024
        {
            return Err(Error::Limit("audio input packet frames"));
        }
        self.generation = self
            .generation
            .checked_add(1)
            .ok_or(Error::Limit("capture generation"))?;
        let request = CaptureRequest {
            generation: self.generation,
            format,
            frames,
        };
        self.encoder = Some(CaptureEncoder::new(wire_format)?);
        self.request = Some(request);
        self.index = index;
        self.ready = false;
        self.changing = changing;
        Ok(vec![Action::Event(ChannelEvent::Capture(
            CaptureEvent::Open(request),
        ))])
    }
    pub fn confirm(&mut self, generation: u64, available: bool) -> Result<Vec<Action>> {
        if self.request.is_none_or(|r| r.generation != generation) {
            return Ok(Vec::new());
        }
        self.ready = available;
        let mut actions = vec![Action::Send(packet(7, self.index))];
        if !self.changing {
            actions.push(Action::Send(packet(
                4,
                if available { 0 } else { 0x80004005 },
            )));
        }
        if !available {
            self.request = None;
            self.encoder = None;
        }
        Ok(actions)
    }
    pub fn samples(&mut self, generation: u64, samples: &[u8]) -> Result<Vec<Action>> {
        let Some(request) = self
            .request
            .filter(|r| r.generation == generation && self.ready)
        else {
            return Ok(Vec::new());
        };
        let expected = request.frames as usize * usize::from(request.format.channels) * 2;
        if samples.len() != expected {
            return Err(Error::Invalid("audio input sample count"));
        }
        if self.encoder.is_none() {
            self.encoder = Some(CaptureEncoder::new(self.formats[self.index as usize])?);
        }
        let encoded = self
            .encoder
            .as_mut()
            .ok_or(Error::Invalid("audio capture encoder"))?
            .encode(samples)?;
        if encoded.is_empty() {
            return Ok(Vec::new());
        }
        let mut packet = Vec::with_capacity(encoded.len() + 1);
        packet.push(6);
        packet.extend_from_slice(&encoded);
        Ok(vec![Action::Send(vec![5]), Action::Send(packet)])
    }

    pub fn reset(&mut self, generation: u64) {
        if self
            .request
            .is_some_and(|request| request.generation == generation)
        {
            self.encoder = None;
        }
    }

    pub fn close(&mut self) {
        self.version = None;
        self.formats.clear();
        self.request = None;
        self.encoder = None;
        self.ready = false;
    }
}
fn packet(kind: u8, value: u32) -> Vec<u8> {
    let mut p = vec![kind];
    p.extend_from_slice(&value.to_le_bytes());
    p
}

#[cfg(test)]
mod tests {
    use super::*;
    fn negotiated() -> AudioInput {
        let mut input = AudioInput::default();
        input.receive(&packet(1, 2)).unwrap();
        let mut formats = packet(2, 1);
        formats.extend_from_slice(&0u32.to_le_bytes());
        formats.extend_from_slice(&1u16.to_le_bytes());
        formats.extend_from_slice(&1u16.to_le_bytes());
        formats.extend_from_slice(&16000u32.to_le_bytes());
        formats.extend_from_slice(&32000u32.to_le_bytes());
        formats.extend_from_slice(&2u16.to_le_bytes());
        formats.extend_from_slice(&16u16.to_le_bytes());
        formats.extend_from_slice(&0u16.to_le_bytes());
        let actions = input.receive(&formats).unwrap();
        assert!(matches!(&actions[0], Action::Send(bytes) if bytes == &[5]));
        input
    }
    #[test]
    fn capture_requires_explicit_confirmation_and_exact_sample_blocks() {
        let mut input = negotiated();
        let actions = input.open(0, 320, false).unwrap();
        let Action::Event(ChannelEvent::Capture(CaptureEvent::Open(request))) = actions[0] else {
            panic!("capture request")
        };
        assert!(
            input
                .samples(request.generation, &[0; 640])
                .unwrap()
                .is_empty()
        );
        let confirmed = input.confirm(request.generation, true).unwrap();
        assert!(matches!(&confirmed[0], Action::Send(bytes) if bytes == &packet(7,0)));
        assert!(matches!(&confirmed[1], Action::Send(bytes) if bytes == &packet(4,0)));
        assert!(input.samples(request.generation, &[0; 639]).is_err());
        assert_eq!(
            input.samples(request.generation, &[0; 640]).unwrap().len(),
            2
        );
        input.close();
        assert!(
            input
                .samples(request.generation, &[0; 640])
                .unwrap()
                .is_empty()
        );
    }
    #[test]
    fn microphone_packet_counts_and_format_indices_are_bounded() {
        let mut input = negotiated();
        assert!(input.open(1, 320, false).is_err());
        assert!(input.open(0, u32::MAX, false).is_err());
        assert!(input.open(0, 0, false).is_err());
    }

    #[test]
    fn microphone_negotiates_companding_and_encodes_only_confirmed_pcm() {
        let mut input = AudioInput::default();
        input.receive(&packet(1, 2)).unwrap();
        let mut formats = packet(2, 1);
        formats.extend_from_slice(&27u32.to_le_bytes());
        formats.extend_from_slice(&7u16.to_le_bytes());
        formats.extend_from_slice(&1u16.to_le_bytes());
        formats.extend_from_slice(&8000u32.to_le_bytes());
        formats.extend_from_slice(&8000u32.to_le_bytes());
        formats.extend_from_slice(&1u16.to_le_bytes());
        formats.extend_from_slice(&8u16.to_le_bytes());
        formats.extend_from_slice(&0u16.to_le_bytes());
        input.receive(&formats).unwrap();
        let actions = input.open(0, 2, false).unwrap();
        let Action::Event(ChannelEvent::Capture(CaptureEvent::Open(request))) = actions[0] else {
            panic!("capture")
        };
        assert_eq!(
            request.format,
            AudioFormat {
                sample_rate: 8000,
                channels: 1
            }
        );
        let samples = [32124i16, -32124]
            .into_iter()
            .flat_map(i16::to_le_bytes)
            .collect::<Vec<_>>();
        assert!(
            input
                .samples(request.generation, &samples)
                .unwrap()
                .is_empty()
        );
        input.confirm(request.generation, true).unwrap();
        let actions = input.samples(request.generation, &samples).unwrap();
        assert!(matches!(&actions[1], Action::Send(data) if data == &[6, 0x80, 0x00]));
        assert!(
            input
                .samples(request.generation + 1, &samples)
                .unwrap()
                .is_empty()
        );
    }

    #[cfg(feature = "audio-codecs")]
    #[test]
    fn capture_discontinuity_discards_aac_history_before_resuming() {
        fn aac() -> (AudioInput, u64) {
            let mut input = AudioInput::default();
            input.receive(&packet(1, 2)).unwrap();
            let mut formats = packet(2, 1);
            formats.extend_from_slice(&27u32.to_le_bytes());
            formats.extend_from_slice(&0xa106u16.to_le_bytes());
            formats.extend_from_slice(&1u16.to_le_bytes());
            formats.extend_from_slice(&44100u32.to_le_bytes());
            formats.extend_from_slice(&8000u32.to_le_bytes());
            formats.extend_from_slice(&1u16.to_le_bytes());
            formats.extend_from_slice(&16u16.to_le_bytes());
            formats.extend_from_slice(&0u16.to_le_bytes());
            input.receive(&formats).unwrap();
            let actions = input.open(0, 320, false).unwrap();
            let Action::Event(ChannelEvent::Capture(CaptureEvent::Open(request))) = actions[0]
            else {
                panic!("capture")
            };
            input.confirm(request.generation, true).unwrap();
            (input, request.generation)
        }
        let (mut input, generation) = aac();
        for _ in 0..6 {
            input.samples(generation, &[0x34; 640]).unwrap();
        }
        input.reset(generation);
        let (mut fresh, fresh_generation) = aac();
        let packets = |actions: Vec<Action>| {
            actions
                .into_iter()
                .map(|action| match action {
                    Action::Send(packet) => packet,
                    _ => panic!("capture data must only emit network packets"),
                })
                .collect::<Vec<_>>()
        };
        for _ in 0..12 {
            assert_eq!(
                packets(input.samples(generation, &[0; 640]).unwrap()),
                packets(fresh.samples(fresh_generation, &[0; 640]).unwrap())
            );
        }
    }
}
