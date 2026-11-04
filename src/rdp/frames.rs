use std::{sync::Arc, time::Duration};

use rustrdp::graphics::Damage;
use tokio::time::Instant;

use super::{RDP_TILE_SIZE, RdpFrame, RdpTile};

/// Limit publication rate without delaying an update after an idle period.
pub(super) struct FrameClock {
    interval: Duration,
    next_allowed: Option<Instant>,
    deadline: Option<Instant>,
}

impl FrameClock {
    pub fn new(interval: Duration, now: Instant) -> Self {
        Self {
            interval,
            next_allowed: None,
            deadline: Some(now),
        }
    }

    pub fn deadline(&self) -> Option<Instant> {
        self.deadline
    }

    pub fn is_due(&self, now: Instant) -> bool {
        self.deadline.is_some_and(|deadline| deadline <= now)
    }

    pub fn dirty(&mut self, now: Instant) {
        self.deadline
            .get_or_insert_with(|| self.next_allowed.map_or(now, |next| now.max(next)));
    }

    pub fn published(&mut self, started: Instant) {
        // Timers can wake a little late. Keep their intended cadence instead
        // of adding that lateness to every subsequent frame. After a long
        // stall, start a new interval rather than publishing catch-up bursts.
        let next = self.deadline.take().unwrap_or(started) + self.interval;
        self.next_allowed = Some(if next > started {
            next
        } else {
            started + self.interval
        });
    }

    pub fn suspend(&mut self) {
        self.deadline = None;
    }
}

/// Complete snapshots share unchanged regions, including across skipped frames.
#[derive(Default)]
pub(super) struct FrameSnapshot {
    size: (u16, u16),
    tiles: Vec<Arc<RdpTile>>,
    audit: Option<SnapshotAudit>,
}

#[derive(Default)]
pub(super) struct SnapshotAudit {
    pub captures: u64,
    pub checked_tiles: u64,
    pub mismatched_tiles: u64,
}

impl FrameSnapshot {
    pub fn with_audit(enabled: bool) -> Self {
        Self {
            audit: enabled.then(SnapshotAudit::default),
            ..Self::default()
        }
    }

    pub fn take_audit(&mut self) -> Option<SnapshotAudit> {
        self.audit.as_mut().map(std::mem::take)
    }

    pub fn capture(&mut self, session: &mut rustrdp::Session) -> RdpFrame {
        let damage = session.take_graphics_damage();
        self.capture_graphics(session.graphics(), damage, session.info().color_depth())
    }

    pub fn capture_graphics(
        &mut self,
        graphics: &rustrdp::graphics::Graphics,
        damage: Option<Damage>,
        bits_per_pixel: u16,
    ) -> RdpFrame {
        self.update(
            graphics.width(),
            graphics.height(),
            graphics.pixels(),
            damage,
        );
        RdpFrame {
            sequence: 0,
            published_at: std::time::Instant::now(),
            acknowledgement: None,
            width: graphics.width(),
            height: graphics.height(),
            bits_per_pixel,
            tiles: self.tiles.clone(),
            hide_local_pointer: graphics.hide_local_pointer(),
        }
    }

    fn update(&mut self, width: u16, height: u16, pixels: &[u8], damage: Option<Damage>) {
        debug_assert_eq!(pixels.len(), usize::from(width) * usize::from(height) * 4);
        if self.size != (width, height) {
            self.tiles.clear();
            for y in (0..height).step_by(usize::from(RDP_TILE_SIZE)) {
                for x in (0..width).step_by(usize::from(RDP_TILE_SIZE)) {
                    let mut tile = RdpTile {
                        x,
                        y,
                        width: RDP_TILE_SIZE.min(width - x),
                        height: RDP_TILE_SIZE.min(height - y),
                        pixels: Vec::new(),
                    };
                    tile.pixels = tile.read_pixels(width, pixels);
                    self.tiles.push(Arc::new(tile));
                }
            }
            self.size = (width, height);
            return;
        }
        if let Some(audit) = &mut self.audit {
            audit.captures = audit.captures.saturating_add(1);
            for tile in &self.tiles {
                if damage.is_some_and(|damage| tile.intersects(damage)) {
                    continue;
                }
                audit.checked_tiles = audit.checked_tiles.saturating_add(1);
                if !tile.matches_pixels(width, pixels) {
                    audit.mismatched_tiles = audit.mismatched_tiles.saturating_add(1);
                }
            }
        }
        let Some(damage) = damage else { return };
        for tile in &mut self.tiles {
            // A union can cover large unchanged areas between separate updates.
            // Comparing candidate rows also avoids uploads for identical bitmaps.
            if !tile.intersects(damage) || tile.matches_pixels(width, pixels) {
                continue;
            }
            *tile = Arc::new(RdpTile {
                x: tile.x,
                y: tile.y,
                width: tile.width,
                height: tile.height,
                pixels: tile.read_pixels(width, pixels),
            });
        }
    }
}

impl RdpTile {
    fn intersects(&self, damage: Damage) -> bool {
        u32::from(self.x) < u32::from(damage.x) + u32::from(damage.width)
            && u32::from(damage.x) < u32::from(self.x) + u32::from(self.width)
            && u32::from(self.y) < u32::from(damage.y) + u32::from(damage.height)
            && u32::from(damage.y) < u32::from(self.y) + u32::from(self.height)
    }

    fn matches_pixels(&self, desktop_width: u16, pixels: &[u8]) -> bool {
        let row_bytes = usize::from(self.width) * 4;
        self.rows(desktop_width, pixels)
            .zip(self.pixels.chunks_exact(row_bytes))
            .all(|(source, previous)| source == previous)
    }

    fn read_pixels(&self, desktop_width: u16, pixels: &[u8]) -> Vec<u8> {
        let mut copied = Vec::with_capacity(usize::from(self.width) * usize::from(self.height) * 4);
        for row in self.rows(desktop_width, pixels) {
            copied.extend_from_slice(row);
        }
        copied
    }

    fn rows<'a>(&self, desktop_width: u16, pixels: &'a [u8]) -> impl Iterator<Item = &'a [u8]> {
        let stride = usize::from(desktop_width) * 4;
        let row_bytes = usize::from(self.width) * 4;
        let start = usize::from(self.y) * stride + usize::from(self.x) * 4;
        (0..usize::from(self.height)).map(move |row| {
            let offset = start + row * stride;
            &pixels[offset..offset + row_bytes]
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const INTERVAL: Duration = Duration::from_nanos(16_666_667);

    #[test]
    fn steady_updates_do_not_wait_an_extra_frame() {
        let start = Instant::now();
        let mut clock = FrameClock::new(INTERVAL, start);
        assert!(clock.is_due(start));
        for frame in 0..120 {
            let now = start + INTERVAL * frame;
            clock.dirty(now);
            assert_eq!(clock.deadline(), Some(now));
            assert!(clock.is_due(now));
            clock.published(now);
            assert_eq!(clock.deadline(), None);
        }
    }

    #[test]
    fn burst_updates_coalesce_without_postponing_the_deadline() {
        let now = Instant::now();
        let mut clock = FrameClock::new(INTERVAL, now);
        clock.published(now);
        clock.dirty(now + Duration::from_millis(1));
        clock.dirty(now + Duration::from_millis(12));
        assert_eq!(clock.deadline(), Some(now + INTERVAL));
        assert!(!clock.is_due(now + Duration::from_millis(12)));
        assert!(clock.is_due(now + INTERVAL));
    }

    #[test]
    fn timer_rounding_does_not_accumulate_into_a_lower_refresh_rate() {
        let start = Instant::now();
        let mut clock = FrameClock::new(INTERVAL, start);
        clock.published(start);
        for frame in 1..=120 {
            let expected = start + INTERVAL * frame;
            clock.dirty(expected - Duration::from_millis(10));
            assert_eq!(clock.deadline(), Some(expected));
            clock.published(expected + Duration::from_millis(1));
        }
    }

    #[test]
    fn late_processing_drops_missed_ticks_without_a_publication_burst() {
        let start = Instant::now();
        let mut clock = FrameClock::new(INTERVAL, start);
        let resumed = start + INTERVAL * 10;
        clock.published(resumed);
        clock.dirty(resumed);
        assert_eq!(clock.deadline(), Some(resumed + INTERVAL));
        assert!(!clock.is_due(resumed));
    }

    #[test]
    fn idle_or_slow_frames_publish_immediately_without_catch_up() {
        let now = Instant::now();
        let mut clock = FrameClock::new(INTERVAL, now);
        clock.published(now);
        let later = now + Duration::from_secs(1);
        clock.dirty(later);
        assert_eq!(clock.deadline(), Some(later));
        clock.published(later);
        assert!(!clock.is_due(later + INTERVAL * 10));
        clock.dirty(later + Duration::from_millis(40));
        assert!(clock.is_due(later + Duration::from_millis(40)));
    }

    #[test]
    fn suspension_discards_pending_publication() {
        let now = Instant::now();
        let mut clock = FrameClock::new(INTERVAL, now);
        clock.suspend();
        assert!(!clock.is_due(now));
        clock.dirty(now + INTERVAL);
        assert!(clock.is_due(now + INTERVAL));
    }

    #[test]
    fn tiles_copy_packed_rows_and_clip_desktop_edges() {
        let width = RDP_TILE_SIZE + 1;
        let pixels: Vec<u8> = (0..usize::from(width) * 2 * 4)
            .map(|value| (value % 251) as u8)
            .collect();
        let mut snapshot = FrameSnapshot::default();
        snapshot.update(width, 2, &pixels, None);
        assert_eq!(snapshot.tiles.len(), 2);
        let first = &snapshot.tiles[0];
        let edge = &snapshot.tiles[1];
        assert_eq!(
            (first.x, first.y, first.width, first.height),
            (0, 0, RDP_TILE_SIZE, 2)
        );
        assert_eq!(
            (edge.x, edge.y, edge.width, edge.height),
            (RDP_TILE_SIZE, 0, 1, 2)
        );
        let stride = usize::from(width) * 4;
        assert_eq!(
            first.pixels,
            [&pixels[..stride - 4], &pixels[stride..stride * 2 - 4]].concat()
        );
        assert_eq!(
            edge.pixels,
            [&pixels[stride - 4..stride], &pixels[stride * 2 - 4..]].concat()
        );
    }

    #[test]
    fn snapshots_share_unchanged_tiles_and_survive_skipped_frames() {
        let width = RDP_TILE_SIZE * 3;
        let mut pixels = vec![0; usize::from(width) * 4];
        let mut snapshot = FrameSnapshot::default();
        snapshot.update(width, 1, &pixels, None);
        let initial = snapshot.tiles.clone();
        pixels[0] = 1;
        snapshot.update(
            width,
            1,
            &pixels,
            Some(Damage {
                x: 0,
                y: 0,
                width: 1,
                height: 1,
            }),
        );
        let skipped = snapshot.tiles.clone();
        assert!(!Arc::ptr_eq(&initial[0], &skipped[0]));
        assert!(Arc::ptr_eq(&initial[1], &skipped[1]));
        assert!(Arc::ptr_eq(&initial[2], &skipped[2]));
        pixels[usize::from(RDP_TILE_SIZE * 2) * 4] = 2;
        snapshot.update(
            width,
            1,
            &pixels,
            Some(Damage {
                x: RDP_TILE_SIZE * 2,
                y: 0,
                width: 1,
                height: 1,
            }),
        );
        assert!(Arc::ptr_eq(&skipped[0], &snapshot.tiles[0]));
        assert!(Arc::ptr_eq(&initial[1], &snapshot.tiles[1]));
        assert_eq!(snapshot.tiles[0].pixels[0], 1);
        assert_eq!(snapshot.tiles[2].pixels[0], 2);
        assert_eq!(initial[0].pixels[0], 0);
        assert_eq!(skipped[2].pixels[0], 0);
    }

    #[test]
    fn damage_audit_detects_only_mismatches_outside_damage_without_changing_snapshot() {
        let width = RDP_TILE_SIZE * 3;
        let mut pixels = vec![0; usize::from(width) * 4];
        let mut snapshot = FrameSnapshot::with_audit(true);
        snapshot.update(width, 1, &pixels, None);
        pixels[0] = 1;
        pixels[usize::from(RDP_TILE_SIZE * 2) * 4] = 2;
        snapshot.update(
            width,
            1,
            &pixels,
            Some(Damage {
                x: 0,
                y: 0,
                width: 1,
                height: 1,
            }),
        );
        let audit = snapshot.take_audit().unwrap();
        assert_eq!(
            (audit.captures, audit.checked_tiles, audit.mismatched_tiles),
            (1, 2, 1)
        );
        assert_eq!(snapshot.tiles[0].pixels[0], 1);
        assert_eq!(snapshot.tiles[2].pixels[0], 0);
        snapshot.update(width, 1, &pixels, None);
        let audit = snapshot.take_audit().unwrap();
        assert_eq!(
            (audit.captures, audit.checked_tiles, audit.mismatched_tiles),
            (1, 3, 1)
        );
        let audit = snapshot.take_audit().unwrap();
        assert_eq!(
            (audit.captures, audit.checked_tiles, audit.mismatched_tiles),
            (0, 0, 0)
        );
        assert!(FrameSnapshot::with_audit(false).take_audit().is_none());
    }

    #[test]
    fn unchanged_pixels_inside_damage_keep_their_tiles() {
        let width = RDP_TILE_SIZE * 3;
        let mut pixels = vec![0; usize::from(width) * 4];
        let mut snapshot = FrameSnapshot::default();
        snapshot.update(width, 1, &pixels, None);
        let initial = snapshot.tiles.clone();
        pixels[0] = 1;
        pixels[usize::from(width) * 4 - 1] = 2;
        let full = Some(Damage {
            x: 0,
            y: 0,
            width,
            height: 1,
        });
        snapshot.update(width, 1, &pixels, full);
        assert!(Arc::ptr_eq(&initial[1], &snapshot.tiles[1]));
        let changed = snapshot.tiles.clone();
        snapshot.update(width, 1, &pixels, full);
        snapshot.update(width, 1, &pixels, None);
        for (before, after) in changed.iter().zip(&snapshot.tiles) {
            assert!(Arc::ptr_eq(before, after));
        }
    }

    #[test]
    fn resizing_rebuilds_tiles_even_with_the_same_pixel_count() {
        let mut snapshot = FrameSnapshot::default();
        snapshot.update(2, 1, &[1; 8], None);
        let old = snapshot.tiles[0].clone();
        snapshot.update(1, 2, &[2; 8], None);
        assert_eq!(snapshot.tiles.len(), 1);
        assert_eq!((snapshot.tiles[0].width, snapshot.tiles[0].height), (1, 2));
        assert!(!Arc::ptr_eq(&old, &snapshot.tiles[0]));
        assert_eq!(snapshot.tiles[0].pixels, [2; 8]);
    }

    #[test]
    fn bottom_right_damage_changes_only_the_clipped_corner_tile() {
        let side = RDP_TILE_SIZE + 1;
        let mut pixels = vec![0; usize::from(side) * usize::from(side) * 4];
        let mut snapshot = FrameSnapshot::default();
        snapshot.update(side, side, &pixels, None);
        let initial = snapshot.tiles.clone();
        let corner = pixels.len() - 4;
        pixels[corner..].copy_from_slice(&[1, 2, 3, 255]);
        snapshot.update(
            side,
            side,
            &pixels,
            Some(Damage {
                x: side - 1,
                y: side - 1,
                width: 1,
                height: 1,
            }),
        );
        assert_eq!(snapshot.tiles.len(), 4);
        for (before, after) in initial.iter().zip(&snapshot.tiles).take(3) {
            assert!(Arc::ptr_eq(before, after));
        }
        assert_eq!(snapshot.tiles[3].pixels, [1, 2, 3, 255]);
        assert_eq!(initial[3].pixels, [0; 4]);
    }

    #[test]
    fn full_damage_replaces_pixels_after_a_same_size_reset() {
        let mut snapshot = FrameSnapshot::default();
        snapshot.update(2, 1, &[1; 8], None);
        let old = snapshot.tiles[0].clone();
        snapshot.update(
            2,
            1,
            &[0; 8],
            Some(Damage {
                x: 0,
                y: 0,
                width: 2,
                height: 1,
            }),
        );
        assert!(!Arc::ptr_eq(&old, &snapshot.tiles[0]));
        assert_eq!(snapshot.tiles[0].pixels, [0; 8]);
    }
}
