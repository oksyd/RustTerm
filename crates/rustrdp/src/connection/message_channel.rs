//! Main-transport network detection and heartbeat PDUs (MS-RDPBCGR 2.2.14–16).

use std::time::Instant;

use crate::{Error, Result, multitransport::NetworkCharacteristics, wire::Reader};

pub(crate) enum Message<'a> {
    Multitransport(&'a [u8]),
    Response(Vec<u8>),
    Accepted,
}

struct Measurement {
    started_ms: u64,
    bytes: u32,
    continuous: bool,
}

pub(crate) struct MessageChannel {
    epoch: Instant,
    measurement: Option<Measurement>,
    characteristics: NetworkCharacteristics,
}

impl Default for MessageChannel {
    fn default() -> Self {
        Self {
            epoch: Instant::now(),
            measurement: None,
            characteristics: NetworkCharacteristics::default(),
        }
    }
}

impl MessageChannel {
    pub(crate) fn receive<'a>(&mut self, bytes: &'a [u8]) -> Result<Message<'a>> {
        self.receive_at(
            bytes,
            self.epoch.elapsed().as_millis().min(u128::from(u64::MAX)) as u64,
        )
    }

    pub(crate) fn record_data(&mut self, length: usize) {
        if let Some(measurement) = self.measurement.as_mut()
            && measurement.continuous
        {
            measurement.bytes = measurement.bytes.wrapping_add(length as u32);
        }
    }

    fn receive_at<'a>(&mut self, bytes: &'a [u8], now_ms: u64) -> Result<Message<'a>> {
        let mut reader = Reader::new(bytes);
        // SEC_RESET_SEQNO / SEC_IGNORE_SEQNO and flagsHi are ignored by the
        // protocol. TLS already authenticates and encrypts this connection.
        let flags = reader.u16_le()? & !0x8030;
        reader.skip(2)?;
        let payload = reader.remaining();
        match flags {
            0x0002 => {
                self.record_data(payload.len());
                Ok(Message::Multitransport(payload))
            }
            0x1000 => self.auto_detect(payload, now_ms),
            0x4000 => {
                if reader.u8()? != 0 {
                    return Err(Error::Invalid("heartbeat reserved field"));
                }
                let _period = reader.u8()?;
                let _warning_threshold = reader.u8()?;
                let _reconnect_threshold = reader.u8()?;
                reader.finish()?;
                // The heartbeat has no response. Warning/reconnect thresholds
                // are advisory; the caller owns connection-health policy.
                self.record_data(payload.len());
                Ok(Message::Accepted)
            }
            _ => Err(Error::Unsupported("RDP message channel PDU")),
        }
    }

    fn auto_detect(&mut self, bytes: &[u8], now_ms: u64) -> Result<Message<'static>> {
        let mut reader = Reader::new(bytes);
        let length = reader.u8()?;
        if reader.u8()? != 0 {
            return Err(Error::Invalid("auto-detect request header type"));
        }
        let sequence = reader.u16_le()?;
        let kind = reader.u16_le()?;
        let expected = match kind {
            0x0001 | 0x1001 | 0x0014 | 0x1014 | 0x0429 => 6,
            0x0002 | 0x002b => 8,
            0x0840 | 0x0880 => 14,
            0x08c0 => 18,
            _ => return Err(Error::Unsupported("TCP auto-detect request")),
        };
        if length != expected {
            return Err(Error::Invalid("auto-detect request header length"));
        }
        match kind {
            0x0001 | 0x1001 => {
                reader.finish()?;
                self.record_data(bytes.len());
                Ok(Message::Response(response(sequence, 0, &[])))
            }
            0x0014 | 0x1014 => {
                reader.finish()?;
                self.measurement = Some(Measurement {
                    started_ms: now_ms,
                    bytes: 0,
                    continuous: kind == 0x0014,
                });
                Ok(Message::Accepted)
            }
            0x0002 => {
                let length = usize::from(reader.u16_le()?);
                reader.skip(length)?;
                reader.finish()?;
                let measurement = self
                    .measurement
                    .as_mut()
                    .ok_or(Error::Invalid("bandwidth payload without start"))?;
                if measurement.continuous {
                    return Err(Error::Invalid(
                        "connect-time bandwidth payload after activation",
                    ));
                }
                measurement.bytes = measurement.bytes.wrapping_add(bytes.len() as u32);
                Ok(Message::Accepted)
            }
            0x002b | 0x0429 => {
                if kind == 0x002b {
                    let length = usize::from(reader.u16_le()?);
                    if length == 0 {
                        return Err(Error::Invalid("empty bandwidth stop payload"));
                    }
                    reader.skip(length)?;
                }
                reader.finish()?;
                let measurement = self
                    .measurement
                    .as_ref()
                    .ok_or(Error::Invalid("bandwidth stop without start"))?;
                if measurement.continuous != (kind == 0x0429) {
                    return Err(Error::Invalid("bandwidth stop measurement type"));
                }
                let elapsed = now_ms
                    .checked_sub(measurement.started_ms)
                    .ok_or(Error::Invalid("bandwidth measurement clock"))?
                    .min(u64::from(u32::MAX)) as u32;
                // Connect-time payloads include their eight-byte header. For
                // continuous detection, count received bytes after security.
                let count = measurement.bytes.wrapping_add(bytes.len() as u32);
                let mut results = elapsed.to_le_bytes().to_vec();
                results.extend_from_slice(&count.to_le_bytes());
                self.measurement = None;
                Ok(Message::Response(response(
                    sequence,
                    if kind == 0x002b { 3 } else { 11 },
                    &results,
                )))
            }
            0x0840 | 0x0880 | 0x08c0 => {
                let mut characteristics = self.characteristics;
                if kind & 0x0040 != 0 {
                    characteristics.base_rtt = Some(reader.u32_le()?);
                }
                if kind & 0x0080 != 0 {
                    characteristics.bandwidth = Some(reader.u32_le()?);
                }
                characteristics.average_rtt = Some(reader.u32_le()?);
                reader.finish()?;
                self.characteristics = characteristics;
                self.record_data(bytes.len());
                Ok(Message::Accepted)
            }
            _ => unreachable!(),
        }
    }
}

fn response(sequence: u16, kind: u16, body: &[u8]) -> Vec<u8> {
    let mut data = vec![0, 0x20, 0, 0, (6 + body.len()) as u8, 1];
    data.extend_from_slice(&sequence.to_le_bytes());
    data.extend_from_slice(&kind.to_le_bytes());
    data.extend_from_slice(body);
    data
}

#[cfg(test)]
mod tests {
    use super::*;

    fn reply(message: Message<'_>) -> Vec<u8> {
        match message {
            Message::Response(data) => data,
            _ => panic!("expected auto-detect response"),
        }
    }

    #[test]
    fn windows_rtt_fixture_responds_with_matching_sequence() {
        let mut channel = MessageChannel::default();
        // Windows 11 continuous RTT request captured during a live connection.
        let request = [0, 0x10, 0, 0, 6, 0, 0, 0, 1, 0];
        assert_eq!(
            reply(channel.receive_at(&request, 123).unwrap()),
            [0, 0x20, 0, 0, 6, 1, 0, 0, 0, 0]
        );
        // Connect-time detection uses 0x1001. Uninitialized flagsHi and the
        // obsolete sequence flags must not alter the request interpretation.
        let request = [0x30, 0x10, 0x9d, 0xfe, 6, 0, 0x34, 0x12, 1, 0x10];
        assert_eq!(
            reply(channel.receive_at(&request, 124).unwrap()),
            [0, 0x20, 0, 0, 6, 1, 0x34, 0x12, 0, 0]
        );
        for end in 0..request.len() {
            assert!(channel.receive_at(&request[..end], 125).is_err());
        }
    }

    #[test]
    fn bandwidth_results_use_elapsed_time_and_the_received_byte_count() {
        let mut channel = MessageChannel::default();
        channel
            .receive_at(&[0, 16, 0, 0, 6, 0, 1, 0, 20, 16], 100)
            .unwrap();
        channel
            .receive_at(&[0, 16, 0, 0, 8, 0, 2, 0, 2, 0, 4, 0, 1, 2, 3, 4], 120)
            .unwrap();
        let stop = [0, 16, 0, 0, 8, 0, 3, 0, 43, 0, 1, 0, 5];
        assert_eq!(
            reply(channel.receive_at(&stop, 150).unwrap()),
            [0, 32, 0, 0, 14, 1, 3, 0, 3, 0, 50, 0, 0, 0, 21, 0, 0, 0]
        );
        assert!(channel.measurement.is_none());
        channel
            .receive_at(&[0, 16, 0, 0, 6, 0, 4, 0, 20, 0], 200)
            .unwrap();
        channel.record_data(1000);
        channel
            .receive_at(&[0, 16, 0, 0, 6, 0, 5, 0, 1, 0], 220)
            .unwrap();
        let stop = [0, 16, 0, 0, 6, 0, 6, 0, 41, 4];
        assert_eq!(
            reply(channel.receive_at(&stop, 250).unwrap()),
            [0, 32, 0, 0, 14, 1, 6, 0, 11, 0, 50, 0, 0, 0, 244, 3, 0, 0]
        );
        assert!(channel.receive_at(&stop, 260).is_err());
    }

    #[test]
    fn production_clock_and_heartbeat_are_validated_without_synthetic_metrics() {
        let mut channel = MessageChannel::default();
        channel.receive(&[0, 16, 0, 0, 6, 0, 1, 0, 20, 0]).unwrap();
        // Advance the private epoch without sleeping or inventing a timer result.
        channel.epoch -= std::time::Duration::from_millis(50);
        let data = reply(channel.receive(&[0, 16, 0, 0, 6, 0, 2, 0, 41, 4]).unwrap());
        assert!(u32::from_le_bytes(data[10..14].try_into().unwrap()) >= 50);
        let heartbeat = [0, 64, 0, 0, 0, 5, 3, 4];
        assert!(matches!(channel.receive(&heartbeat), Ok(Message::Accepted)));
        for end in 0..heartbeat.len() {
            assert!(channel.receive(&heartbeat[..end]).is_err());
        }
        assert!(channel.receive(&[0, 64, 0, 0, 1, 5, 3, 4]).is_err());
        let result = [
            0, 16, 0, 0, 18, 0, 0, 0, 0xc0, 8, 10, 0, 0, 0, 0, 4, 0, 0, 20, 0, 0, 0,
        ];
        channel.receive(&result).unwrap();
        assert_eq!(
            channel.characteristics,
            NetworkCharacteristics {
                base_rtt: Some(10),
                bandwidth: Some(1024),
                average_rtt: Some(20),
            }
        );
    }
}
