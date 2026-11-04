use super::TransportProtocol;
use crate::{Error, Result, wire::Reader};

/// Network measurements reported by the server, as specified in MS-RDPBCGR.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct NetworkCharacteristics {
    /// Lowest measured round-trip latency, in milliseconds.
    pub base_rtt: Option<u32>,
    /// Current average round-trip latency, in milliseconds.
    pub average_rtt: Option<u32>,
    /// Estimated available bandwidth, in kilobits per second.
    pub bandwidth: Option<u32>,
}

struct Measurement {
    started: u64,
    sequence: u16,
    bytes: u32,
}

/// Answers continuous bandwidth measurements on one authenticated UDP tunnel.
pub struct BandwidthDetection {
    protocol: TransportProtocol,
    measurement: Option<Measurement>,
    characteristics: NetworkCharacteristics,
}

impl BandwidthDetection {
    /// Creates independent measurement state for one tunnel.
    pub fn new(protocol: TransportProtocol) -> Self {
        Self {
            protocol,
            measurement: None,
            characteristics: NetworkCharacteristics::default(),
        }
    }

    /// The most recent metrics reported by the server.
    pub fn characteristics(&self) -> NetworkCharacteristics {
        self.characteristics
    }

    /// Processes the subheaders of a tunnel data PDU, then counts its payload.
    ///
    /// `now_ms` uses a monotonic clock. Returned packets are complete EMT data
    /// PDUs containing response subheaders and must be sent on this same secured
    /// tunnel, without wrapping them as dynamic-channel data.
    ///
    /// # Errors
    /// Rejects malformed measurement requests, mismatched transport types,
    /// oversized tunnel fields, and a clock moving backwards.
    pub fn receive(
        &mut self,
        subheaders: &[u8],
        payload_length: usize,
        now_ms: u64,
    ) -> Result<Vec<Vec<u8>>> {
        if subheaders.len() > 251 || payload_length > u16::MAX as usize {
            return Err(Error::Limit("tunnel bandwidth measurement"));
        }
        let mut reader = Reader::new(subheaders);
        let mut replies = Vec::new();
        while !reader.is_empty() {
            let length = usize::from(reader.u8()?);
            if length < 2 {
                return Err(Error::Invalid("tunnel measurement subheader"));
            }
            let kind = reader.u8()?;
            let mut data = Reader::new(reader.take(length - 2)?);
            if kind != 0 {
                continue;
            }
            let sequence = data.u16_le()?;
            match data.u16_le()? {
                request @ (0x0014 | 0x0114) => {
                    data.finish()?;
                    if (request == 0x0114) != (self.protocol == TransportProtocol::Lossy) {
                        return Err(Error::Invalid("bandwidth start transport"));
                    }
                    self.measurement = Some(Measurement {
                        started: now_ms,
                        sequence,
                        bytes: 0,
                    });
                }
                request @ (0x0429 | 0x0629) => {
                    data.finish()?;
                    let lossy = self.protocol == TransportProtocol::Lossy;
                    if (request == 0x0629) != lossy {
                        return Err(Error::Invalid("bandwidth stop transport"));
                    }
                    let Some(measurement) = self.measurement.as_ref() else {
                        // A lossy tunnel may have dropped the corresponding start.
                        if lossy {
                            continue;
                        }
                        return Err(Error::Invalid("bandwidth stop without start"));
                    };
                    if lossy && sequence != measurement.sequence {
                        continue;
                    }
                    let elapsed = now_ms
                        .checked_sub(measurement.started)
                        .ok_or(Error::Invalid("bandwidth measurement clock"))?
                        .min(u64::from(u32::MAX)) as u32;
                    let mut response = vec![2, 0, 0, 18, 14, 1];
                    response.extend_from_slice(&sequence.to_le_bytes());
                    response.extend_from_slice(&0x000bu16.to_le_bytes());
                    response.extend_from_slice(&elapsed.to_le_bytes());
                    response.extend_from_slice(&measurement.bytes.to_le_bytes());
                    replies.push(response);
                    if !lossy {
                        self.measurement = None;
                    }
                }
                request @ (0x0840 | 0x0880 | 0x08c0) => {
                    if request & 0x0040 != 0 {
                        self.characteristics.base_rtt = Some(data.u32_le()?);
                    }
                    if request & 0x0080 != 0 {
                        self.characteristics.bandwidth = Some(data.u32_le()?);
                    }
                    self.characteristics.average_rtt = Some(data.u32_le()?);
                    data.finish()?;
                }
                _ => return Err(Error::Unsupported("tunnel auto-detect request")),
            }
        }
        if let Some(measurement) = self.measurement.as_mut() {
            // Only HigherLayerData is counted; EMT headers are excluded by
            // MS-RDPBCGR 3.2.5.14, including all optional subheaders.
            measurement.bytes = measurement.bytes.wrapping_add(payload_length as u32);
        }
        Ok(replies)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reliable_measurement_uses_actual_payload_count_and_clock() {
        let mut detection = BandwidthDetection::new(TransportProtocol::Reliable);
        detection.receive(&[6, 0, 4, 0, 0x14, 0], 9, 100).unwrap();
        detection.receive(&[], 2000, 125).unwrap();
        let replies = detection.receive(&[6, 0, 5, 0, 0x29, 4], 7, 150).unwrap();
        assert_eq!(
            replies,
            [vec![
                2, 0, 0, 18, 14, 1, 5, 0, 11, 0, 50, 0, 0, 0, 0xd9, 7, 0, 0
            ]]
        );
        assert!(detection.measurement.is_none());
        assert!(detection.receive(&[6, 0, 5, 0, 0x29, 4], 0, 151).is_err());
    }

    #[test]
    fn lossy_measurement_ignores_unmatched_stops_and_restarts() {
        let mut detection = BandwidthDetection::new(TransportProtocol::Lossy);
        assert!(
            detection
                .receive(&[6, 0, 2, 0, 0x29, 6], 0, 0)
                .unwrap()
                .is_empty()
        );
        detection.receive(&[6, 0, 1, 0, 0x14, 1], 50, 10).unwrap();
        assert!(
            detection
                .receive(&[6, 0, 2, 0, 0x29, 6], 0, 20)
                .unwrap()
                .is_empty()
        );
        detection.receive(&[6, 0, 2, 0, 0x14, 1], 100, 30).unwrap();
        let replies = detection.receive(&[6, 0, 2, 0, 0x29, 6], 0, 40).unwrap();
        assert_eq!(&replies[0][10..], &[10, 0, 0, 0, 100, 0, 0, 0]);
        assert!(detection.receive(&[6, 0, 2, 0, 0x29, 6], 0, 20).is_err());
    }

    #[test]
    fn network_results_validate_lengths_and_preserve_metrics() {
        let mut detection = BandwidthDetection::new(TransportProtocol::Reliable);
        let result = [18, 0, 0, 0, 0xc0, 8, 10, 0, 0, 0, 0, 4, 0, 0, 20, 0, 0, 0];
        for end in 1..result.len() {
            assert!(detection.receive(&result[..end], 0, 0).is_err());
        }
        detection.receive(&result, 0, 0).unwrap();
        assert_eq!(
            detection.characteristics(),
            NetworkCharacteristics {
                base_rtt: Some(10),
                average_rtt: Some(20),
                bandwidth: Some(1024),
            }
        );
        assert!(detection.receive(&[6, 0, 0, 0, 0x14, 1], 0, 0).is_err());
    }
}
