use super::WindowState;
use rustterm::rdp::{RdpFrame, RdpPerformance};
use std::time::{Duration, Instant};

#[derive(Default)]
struct Timing {
    total: Duration,
    maximum: Duration,
}

impl Timing {
    fn add(&mut self, duration: Duration) {
        self.total += duration;
        self.maximum = self.maximum.max(duration);
    }
}

struct FrameStamp {
    sequence: u64,
    published_at: Instant,
}

#[derive(Default)]
pub(super) struct Performance {
    started: Option<Instant>,
    frame: Option<FrameStamp>,
    last_painted: Option<u64>,
    prepare_calls: u64,
    prepared_frames: u64,
    painted_frames: u64,
    skipped_frames: u64,
    paint_calls: u64,
    prepared_tiles: u64,
    prepared_bytes: u64,
    uploaded_tiles: u64,
    uploaded_bytes: u64,
    paint_errors: u64,
    prepare: Timing,
    paint: Timing,
    age: Timing,
}

impl Performance {
    pub fn prepared(&mut self, frame: &RdpFrame, tiles: u64, bytes: u64, elapsed: Duration) {
        self.started.get_or_insert_with(Instant::now);
        self.prepare_calls += 1;
        self.prepared_tiles += tiles;
        self.prepared_bytes += bytes;
        self.prepare.add(elapsed);
        if self
            .frame
            .as_ref()
            .is_some_and(|previous| previous.sequence == frame.sequence)
        {
            return;
        }
        self.frame = Some(FrameStamp {
            sequence: frame.sequence,
            published_at: frame.published_at,
        });
        self.prepared_frames += 1;
    }

    pub fn painted(&mut self, elapsed: Duration, uploads: u64, bytes: u64, errors: u64) {
        let now = Instant::now();
        self.started.get_or_insert(now);
        self.paint_calls += 1;
        self.uploaded_tiles += uploads;
        self.uploaded_bytes += bytes;
        self.paint_errors += errors;
        self.paint.add(elapsed);
        let Some(frame) = &self.frame else {
            return;
        };
        if self.last_painted == Some(frame.sequence) {
            return;
        }
        if let Some(previous) = self.last_painted {
            self.skipped_frames += frame.sequence.saturating_sub(previous).saturating_sub(1);
        }
        self.last_painted = Some(frame.sequence);
        self.painted_frames += 1;
        self.age
            .add(now.saturating_duration_since(frame.published_at));
    }

    pub fn flush(&mut self, window: &WindowState, pane: usize) {
        let Some(started) = self.started else {
            return;
        };
        let elapsed = started.elapsed();
        let pane = pane as u64;
        window.record_values(
            "rdp-ui-frames",
            [
                pane,
                micros(elapsed),
                self.prepare_calls,
                self.prepared_frames,
                self.paint_calls,
                self.painted_frames,
                self.skipped_frames,
            ],
        );
        window.record_values(
            "rdp-ui-tiles",
            [
                pane,
                self.prepared_tiles,
                self.prepared_bytes,
                self.uploaded_tiles,
                self.uploaded_bytes,
                self.paint_errors,
            ],
        );
        // Sums and maxima, in microseconds; frame age is publication to CPU paint.
        // GPUI submits and waits for GPU work after our canvas paint has returned.
        window.record_values(
            "rdp-ui-timing-us",
            [
                pane,
                micros(self.prepare.total),
                micros(self.prepare.maximum),
                micros(self.paint.total),
                micros(self.paint.maximum),
                micros(self.age.total),
                micros(self.age.maximum),
            ],
        );
        let frame = self.frame.take();
        let last_painted = self.last_painted;
        *self = Self {
            frame,
            last_painted,
            ..Self::default()
        };
    }
}

pub(super) fn record_backend(window: &WindowState, pane: usize, sample: &RdpPerformance) {
    let pane = pane as u64;
    // Wire codec identifiers; 0xffff denotes no GFX bitmap received yet.
    let codec = match sample.graphics_codec {
        None => 0xffff,
        Some(rustrdp::GraphicsCodec::Raw) => 0,
        Some(rustrdp::GraphicsCodec::RemoteFx) => 3,
        Some(rustrdp::GraphicsCodec::Clear) => 8,
        Some(rustrdp::GraphicsCodec::Progressive) => 9,
        Some(rustrdp::GraphicsCodec::Planar) => 10,
        Some(rustrdp::GraphicsCodec::Avc420) => 11,
        Some(rustrdp::GraphicsCodec::Alpha) => 12,
        Some(rustrdp::GraphicsCodec::Avc444) => 14,
        Some(rustrdp::GraphicsCodec::Avc444V2) => 15,
    };
    window.record_values(
        "rdp-backend-pipeline",
        [
            pane,
            codec,
            sample.udp_packets,
            sample.udp_connections as u64,
        ],
    );
    // Network updates exclude local software-pointer prediction. Published
    // frames and UI paint counts include it, so they are not server FPS.
    window.record_values(
        "rdp-backend-network",
        [
            pane,
            micros(sample.interval),
            sample.packets,
            sample.fastpath_packets,
            sample.wire_bytes,
            sample.server_graphics_updates,
            sample.completed_frames,
            u64::from(sample.surface_updates),
            u64::from(sample.nscodec),
            u64::from(sample.frame_acknowledgements),
            sample.acknowledged_frames,
        ],
    );
    window.record_values(
        "rdp-backend-processing-us",
        [
            pane,
            micros(sample.process_time),
            micros(sample.max_process_time),
            sample.frames,
            micros(sample.snapshot_time),
            micros(sample.max_snapshot_time),
        ],
    );
    if let Some((captures, checked, mismatched)) = sample.snapshot_audit {
        window.record_values(
            "rdp-snapshot-damage-audit",
            [pane, captures, checked, mismatched],
        );
    }
    if let Some((decoded, empty)) = sample.avc_decode_audit {
        window.record_values("rdp-avc-decode-audit", [pane, decoded, empty]);
    }
}

fn micros(duration: Duration) -> u64 {
    duration.as_micros().min(u128::from(u64::MAX)) as u64
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn repainting_a_frame_does_not_inflate_fps_or_count_the_initial_gap() {
        let mut performance = Performance::default();
        let mut frame = RdpFrame {
            remote_windows: Vec::new(),
            sequence: 7,
            published_at: Instant::now(),
            acknowledgement: None,
            width: 1,
            height: 1,
            bits_per_pixel: 32,
            tiles: Vec::new(),
            hide_local_pointer: false,
        };
        performance.prepared(&frame, 1, 36, Duration::from_micros(2));
        performance.painted(Duration::from_micros(3), 1, 36, 0);
        performance.prepared(&frame, 0, 0, Duration::from_micros(1));
        performance.painted(Duration::from_micros(1), 0, 0, 0);
        frame.sequence = 8;
        performance.prepared(&frame, 1, 36, Duration::from_micros(2));
        frame.sequence = 10;
        performance.prepared(&frame, 1, 36, Duration::from_micros(2));
        performance.painted(Duration::from_micros(4), 1, 36, 0);

        assert_eq!(performance.prepare_calls, 4);
        assert_eq!(performance.prepared_frames, 3);
        assert_eq!(performance.painted_frames, 2);
        assert_eq!(performance.paint_calls, 3);
        assert_eq!(performance.skipped_frames, 2);
        assert_eq!(performance.prepared_bytes, 108);
        assert_eq!(performance.uploaded_bytes, 72);
        assert_eq!(performance.paint.total, Duration::from_micros(8));
        assert_eq!(performance.paint.maximum, Duration::from_micros(4));
    }
}
