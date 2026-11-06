use rustterm::config::Uuid;

/// Only output received while a surface stays hidden is unread. Sampling at
/// visibility transitions acknowledges foreground output even before the next poll.
pub(super) struct OutputActivity {
    output: Option<(Uuid, u64)>,
    visible: bool,
}

impl OutputActivity {
    pub(super) fn new(output: Option<(Uuid, u64)>, visible: bool) -> Self {
        Self { output, visible }
    }

    pub(super) fn observe(&mut self, output: Option<(Uuid, u64)>, visible: bool) -> bool {
        let unread = !visible
            && !self.visible
            && self.output.zip(output).is_some_and(|(previous, current)| {
                previous.0 == current.0 && previous.1 != current.1
            });
        self.output = output;
        self.visible = visible;
        unread
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn switching_away_acknowledges_output_received_since_the_last_visible_poll() {
        let session = Uuid::new_v4();
        let mut activity = OutputActivity::new(Some((session, 1)), true);
        // Output arrived in the foreground, then the user switched tabs before polling.
        assert!(!activity.observe(Some((session, 2)), false));
        assert!(!activity.observe(Some((session, 2)), false));
        // Actual subsequent output in the background still raises the indicator.
        assert!(activity.observe(Some((session, 3)), false));
        assert!(!activity.observe(Some((session, 3)), false));
    }

    #[test]
    fn returning_to_a_tab_reads_output_even_when_switching_away_again_immediately() {
        let session = Uuid::new_v4();
        let mut activity = OutputActivity::new(Some((session, 0)), false);
        assert!(activity.observe(Some((session, 1)), false));
        assert!(!activity.observe(Some((session, 2)), true));
        assert!(!activity.observe(Some((session, 2)), false));
        assert!(!activity.observe(Some((session, 2)), false));
        assert!(activity.observe(Some((session, 3)), false));
    }

    #[test]
    fn reconnect_and_archive_eviction_do_not_compare_unrelated_output_counters() {
        let old = Uuid::new_v4();
        let new = Uuid::new_v4();
        let mut activity = OutputActivity::new(Some((old, 25)), false);
        assert!(!activity.observe(Some((new, 0)), false));
        assert!(!activity.observe(Some((new, 0)), false));
        assert!(activity.observe(Some((new, 1)), false));
        assert!(!activity.observe(None, false));
        assert!(!activity.observe(Some((old, 25)), false));
    }
}
