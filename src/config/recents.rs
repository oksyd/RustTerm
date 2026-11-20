use super::{Uuid, storage};
use std::path::Path;

#[derive(Clone, Debug, Default)]
pub struct RecentConnections(Vec<Uuid>);

impl RecentConnections {
    const LIMIT: usize = 128;
    const FILE: &str = "recent-connections.json";

    pub fn ids(&self) -> &[Uuid] {
        &self.0
    }

    pub fn remember(&mut self, id: Uuid) -> bool {
        if self.0.first() == Some(&id) {
            return false;
        }
        self.forget(id);
        self.0.insert(0, id);
        self.0.truncate(Self::LIMIT);
        true
    }

    pub fn forget(&mut self, id: Uuid) {
        self.0.retain(|entry| *entry != id);
    }

    pub(super) fn load(state: &Path) -> Self {
        let mut recent = Self::default();
        if let Ok(Some(source)) = storage::read_optional(&state.join(Self::FILE))
            && let Ok(ids) = serde_json::from_str::<Vec<Uuid>>(&source)
        {
            for id in ids.into_iter().take(Self::LIMIT).rev() {
                recent.remember(id);
            }
        }
        recent
    }

    // Optional history has no bearing on whether profiles and credentials committed.
    pub(super) fn save(&self, state: &Path) {
        let path = state.join(Self::FILE);
        if let Ok(existing) = storage::read_optional(&path)
            && (existing.is_some() || !self.0.is_empty())
            && let Ok(contents) = serde_json::to_string(&self.0)
            && existing.as_deref() != Some(&contents)
        {
            let _ = storage::write_atomic(&path, contents.as_bytes());
        }
    }
}
