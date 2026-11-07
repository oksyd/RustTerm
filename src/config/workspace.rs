use crate::i18n::messages as i18n_key;

use super::{LocalConfig, Result, invalid};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct SavedWorkspace {
    pub tabs: Vec<SavedTab>,
    pub active: usize,
    pub focused: usize,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub collapsed_ssh_tab_groups: Vec<Uuid>,
}
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SavedTab {
    pub title: String,
    pub layout: SavedLayout,
}
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum SavedLayout {
    Pane {
        index: usize,
    },
    Split {
        vertical: bool,
        ratio: f32,
        first: Box<SavedLayout>,
        second: Box<SavedLayout>,
    },
}
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct PanePosition {
    pub profile: Uuid,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ssh_session: Option<SshSessionPosition>,
    pub local: Option<LocalConfig>,
    pub directory: Option<String>,
    pub database: Option<u32>,
    pub sql_database: Option<String>,
    pub key_pattern: Option<String>,
    pub selected_key: Option<Vec<u8>>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SshSessionPosition {
    pub number: u64,
    pub last: u64,
}
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct WorkspaceSnapshot {
    pub layout: SavedWorkspace,
    pub panes: Vec<PanePosition>,
}

impl WorkspaceSnapshot {
    pub fn is_empty(&self) -> bool {
        self == &Self::default()
    }

    pub fn validate(&self) -> Result<()> {
        fn node(
            layout: &SavedLayout,
            depth: usize,
            seen: &mut std::collections::HashSet<usize>,
            count: usize,
        ) -> bool {
            if depth > 32 {
                return false;
            }
            match layout {
                SavedLayout::Pane { index } => *index < count && seen.insert(*index),
                SavedLayout::Split {
                    ratio,
                    first,
                    second,
                    ..
                } => {
                    ratio.is_finite()
                        && *ratio > 0.
                        && *ratio < 1.
                        && node(first, depth + 1, seen, count)
                        && node(second, depth + 1, seen, count)
                }
            }
        }
        let mut seen = std::collections::HashSet::new();
        if self.panes.len() > 64
            || self.layout.tabs.len() > 64
            || self.layout.collapsed_ssh_tab_groups.len() > 64
            || {
                let mut groups = std::collections::HashSet::new();
                self.layout.collapsed_ssh_tab_groups.iter().any(|profile| {
                    !groups.insert(profile)
                        || !self.panes.iter().any(|pane| pane.profile == *profile)
                })
            }
            || !self.layout.tabs.iter().all(|tab| {
                tab.title.len() <= 1024 && node(&tab.layout, 0, &mut seen, self.panes.len())
            })
        {
            return Err(invalid(i18n_key::WORKSPACE_INVALID_SAVED_WORKSPACE_LAYOUT));
        }
        for pane in &self.panes {
            if pane
                .ssh_session
                .is_some_and(|session| session.number == 0 || session.last < session.number)
                || pane
                    .directory
                    .as_ref()
                    .is_some_and(|path| path.len() > 4096 || path.contains('\0'))
                || pane
                    .key_pattern
                    .as_ref()
                    .is_some_and(|pattern| pattern.len() > 4096)
                || pane
                    .selected_key
                    .as_ref()
                    .is_some_and(|key| key.len() > 4096)
                || pane.sql_database.as_ref().is_some_and(|database| {
                    database.len() > 256 || database.chars().any(char::is_control)
                })
            {
                return Err(invalid(
                    i18n_key::WORKSPACE_INVALID_SAVED_WORKSPACE_POSITION,
                ));
            }
            if let Some(local) = &pane.local {
                local.validate()?;
            }
        }
        Ok(())
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct QuickCommand {
    pub id: Uuid,
    pub name: String,
    pub command: String,
}
