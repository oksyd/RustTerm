use rustterm::config::{Connection, Profile, SshSessionPosition, Uuid};
use std::collections::HashMap;

/// Live pane instances reserve their numbers, independent of tab order and profile names.
#[derive(Default)]
pub(super) struct SessionNames {
    panes: HashMap<usize, (Uuid, u64)>,
}

impl SessionNames {
    pub fn assign(&mut self, id: usize, profile: &Profile, saved: Option<SshSessionPosition>) {
        if !matches!(profile.connection, Connection::Ssh(_)) {
            self.remove(id);
            return;
        }
        if self
            .panes
            .get(&id)
            .is_some_and(|(owner, _)| *owner == profile.id)
        {
            return; // Reconnecting keeps the instance's number.
        }
        self.remove(id);
        let available = |number| {
            number != 0
                && !self
                    .panes
                    .values()
                    .any(|&(owner, used)| owner == profile.id && used == number)
        };
        let number = if let Some(saved) =
            saved.filter(|saved| available(saved.number) && saved.last >= saved.number)
        {
            saved.number
        } else {
            // Fill the lowest free slot; a closed tab no longer reserves its number.
            let mut next = 1;
            while !available(next) {
                next += 1;
            }
            next
        };
        self.panes.insert(id, (profile.id, number));
    }

    pub fn remove(&mut self, id: usize) {
        self.panes.remove(&id);
    }

    pub fn position(&self, id: usize) -> Option<SshSessionPosition> {
        let &(owner, number) = self.panes.get(&id)?;
        Some(SshSessionPosition {
            number,
            // Retain the snapshot format used by earlier builds. This value is
            // not an allocation counter; only currently open panes occupy slots.
            last: self
                .panes
                .values()
                .filter_map(|&(profile, number)| (profile == owner).then_some(number))
                .max()
                .unwrap_or(number),
        })
    }

    pub fn suffix(&self, id: usize) -> String {
        match self.panes.get(&id) {
            Some((_, number)) if *number > 1 => format!(":{number}"),
            _ => String::new(),
        }
    }

    pub fn name(&self, id: usize, original: &str) -> String {
        format!("{original}{}", self.suffix(id))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn closing_the_second_ssh_tab_reuses_suffix_two_on_every_reopen() {
        let profile = Profile::ssh("server", Default::default());
        let mut names = SessionNames::default();
        names.assign(1, &profile, None);
        for id in 2..6 {
            names.assign(id, &profile, None);
            assert_eq!(names.name(id, &profile.name), "server:2");
            names.remove(id);
        }
        assert_eq!(names.name(1, &profile.name), "server");
    }

    #[test]
    fn closed_ssh_numbers_are_reused_without_renumbering_live_sessions() {
        let profile = Profile::ssh("生产服务器", Default::default());
        let mut names = SessionNames::default();
        for id in [10, 20, 30] {
            names.assign(id, &profile, None);
        }
        assert_eq!(names.name(10, &profile.name), "生产服务器");
        assert_eq!(names.name(20, &profile.name), "生产服务器:2");
        assert_eq!(names.name(30, &profile.name), "生产服务器:3");
        names.assign(20, &profile, None);
        assert_eq!(names.name(20, &profile.name), "生产服务器:2");
        names.remove(20);
        names.assign(40, &profile, None);
        assert_eq!(names.name(40, &profile.name), "生产服务器:2");
        assert_eq!(names.name(10, &profile.name), "生产服务器");
        assert_eq!(names.name(30, &profile.name), "生产服务器:3");
        names.remove(40);
        names.assign(50, &profile, None);
        assert_eq!(names.name(50, &profile.name), "生产服务器:2");
        names.remove(10);
        names.assign(60, &profile, None);
        assert_eq!(names.name(60, &profile.name), "生产服务器");
        assert_eq!(names.name(30, &profile.name), "生产服务器:3");
        for id in [30, 50, 60] {
            names.remove(id);
        }
        names.assign(70, &profile, None);
        assert_eq!(names.name(70, &profile.name), "生产服务器");
        assert_eq!(profile.name, "生产服务器");
    }

    #[test]
    fn restoring_reordered_panes_preserves_live_numbers_and_reuses_gaps() {
        let profile = Profile::ssh("server", Default::default());
        let mut names = SessionNames::default();
        for id in [1, 2, 3, 4] {
            names.assign(id, &profile, None);
        }
        names.remove(2);
        names.remove(4);
        let third = names.position(3).unwrap();
        let first = names.position(1).unwrap();
        let mut restored = SessionNames::default();
        restored.assign(100, &profile, Some(third));
        restored.assign(101, &profile, Some(first));
        restored.assign(102, &profile, None);
        assert_eq!(restored.name(100, &profile.name), "server:3");
        assert_eq!(restored.name(101, &profile.name), "server");
        assert_eq!(restored.name(102, &profile.name), "server:2");
        restored.assign(103, &profile, None);
        assert_eq!(restored.name(103, &profile.name), "server:4");
    }

    #[test]
    fn numbering_uses_profile_identity_and_leaves_other_protocols_unchanged() {
        let profile = Profile::ssh("server", Default::default());
        let other = Profile::ssh("server", Default::default());
        let rdp = Profile::rdp("server", Default::default());
        let mut names = SessionNames::default();
        names.assign(1, &profile, None);
        names.assign(2, &profile, None);
        names.assign(3, &other, None);
        names.assign(4, &rdp, None);
        assert_eq!(names.name(2, &profile.name), "server:2");
        assert_eq!(names.name(3, &other.name), "server");
        assert_eq!(names.position(4), None);
        let mut renamed = profile.clone();
        renamed.name = "renamed:host".into();
        names.assign(2, &renamed, None);
        assert_eq!(names.name(2, &renamed.name), "renamed:host:2");
    }

    #[test]
    fn conflicting_restored_numbers_do_not_create_identical_session_names() {
        let profile = Profile::ssh("server", Default::default());
        let saved = SshSessionPosition { number: 2, last: 3 };
        let mut names = SessionNames::default();
        names.assign(1, &profile, Some(saved));
        names.assign(2, &profile, Some(saved));
        assert_eq!(names.name(1, &profile.name), "server:2");
        assert_eq!(names.name(2, &profile.name), "server");
    }

    #[test]
    fn old_snapshot_counter_does_not_skip_available_numbers() {
        let profile = Profile::ssh("server", Default::default());
        let mut names = SessionNames::default();
        names.assign(
            1,
            &profile,
            Some(SshSessionPosition {
                number: 1,
                last: 99,
            }),
        );
        names.assign(2, &profile, None);
        assert_eq!(names.name(2, &profile.name), "server:2");
    }
}
