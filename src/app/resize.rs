//! Keep only the newest viewport size while one terminal reflow is outstanding.
//! Input stays in the session FIFO; this coalesces geometry before admission.
use std::time::{Duration, Instant};

const MIN_RESIZE_INTERVAL: Duration = Duration::from_millis(16);
type GridSize = (u16, u16);

pub struct ResizeScheduler {
    desired: GridSize,
    sent: GridSize,
    in_flight: bool,
    not_before: Option<Instant>,
}
impl ResizeScheduler {
    pub fn new(size: GridSize) -> Self {
        Self {
            desired: size,
            sent: size,
            in_flight: false,
            not_before: None,
        }
    }
    pub fn request(&mut self, size: GridSize) {
        self.desired = size;
    }
    pub fn next(&self, now: Instant) -> Option<GridSize> {
        (self.desired != self.sent
            && !self.in_flight
            && self.not_before.is_none_or(|deadline| now >= deadline))
        .then_some(self.desired)
    }
    pub fn sent(&mut self, size: GridSize, now: Instant) {
        self.sent = size;
        self.in_flight = true;
        self.not_before = Some(now + MIN_RESIZE_INTERVAL);
    }
    pub fn complete(&mut self) {
        self.in_flight = false;
    }
    pub fn requested_size(&self) -> GridSize {
        self.sent
    }
    pub fn is_pending(&self) -> bool {
        self.desired != self.sent || self.in_flight
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pending_resize_covers_cooldown_and_in_flight_receipts() {
        let now = Instant::now();
        let mut resize = ResizeScheduler::new((80, 24));
        assert!(!resize.is_pending());
        resize.request((100, 30));
        assert!(resize.is_pending());
        resize.sent((100, 30), now);
        assert!(resize.is_pending());
        resize.complete();
        assert!(!resize.is_pending());
        resize.request((120, 40));
        assert_eq!(resize.next(now), None);
        assert!(resize.is_pending());
        resize.request((100, 30));
        assert!(!resize.is_pending());
    }

    #[test]
    fn resize_burst_keeps_only_latest_size_and_waits_for_reflow() {
        let now = Instant::now();
        let mut resize = ResizeScheduler::new((80, 24));
        resize.request((81, 25));
        let first = resize.next(now).unwrap();
        resize.sent(first, now);
        for cols in 82..100 {
            resize.request((cols, 30));
        }
        assert_eq!(resize.next(now + Duration::from_millis(100)), None);
        // Only the actor's receipt can complete this operation; a cached grid
        // with equal dimensions does not acknowledge a new request.
        resize.complete();
        assert_eq!(resize.next(now + Duration::from_millis(15)), None);
        assert_eq!(resize.next(now + Duration::from_millis(16)), Some((99, 30)));
    }

    #[test]
    fn final_resize_survives_a_busy_queue_without_another_layout_event() {
        let now = Instant::now();
        let mut resize = ResizeScheduler::new((80, 24));
        resize.request((120, 40));
        assert_eq!(resize.next(now), Some((120, 40)));
        // A failed admission never calls sent; the existing poll retries the target.
        let retry = now + Duration::from_millis(20);
        assert_eq!(resize.next(retry), Some((120, 40)));
        resize.sent((120, 40), retry);
        resize.complete();
        assert_eq!(resize.next(retry + Duration::from_millis(20)), None);
        // Returning to the already applied size cancels an unsent intermediate size.
        resize.request((121, 40));
        resize.request((120, 40));
        assert_eq!(resize.next(retry + Duration::from_millis(40)), None);
    }
}
