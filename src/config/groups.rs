use super::{AppConfig, Group, Result, Uuid, i18n_key, invalid};
use std::collections::{HashMap, HashSet};

pub struct GroupNode<'a> {
    pub group: &'a Group,
    pub path: String,
    pub depth: usize,
}

impl Group {
    /// Stable, parent-first order without recursive traversal. Configuration
    /// validation rejects missing parents and cycles before the tree is used.
    pub fn tree(groups: &[Self]) -> Vec<GroupNode<'_>> {
        let mut children: HashMap<Option<Uuid>, Vec<&Self>> = HashMap::new();
        for group in groups {
            children.entry(group.parent).or_default().push(group);
        }
        let mut pending: Vec<_> = children
            .get(&None)
            .into_iter()
            .flatten()
            .rev()
            .map(|group| (*group, String::new(), 0))
            .collect();
        let mut nodes = Vec::with_capacity(groups.len());
        let mut visited = HashSet::new();
        while let Some((group, prefix, depth)) = pending.pop() {
            if !visited.insert(group.id) {
                continue;
            }
            let path = if prefix.is_empty() {
                group.name.clone()
            } else {
                format!("{prefix} / {}", group.name)
            };
            for child in children.get(&Some(group.id)).into_iter().flatten().rev() {
                pending.push((*child, path.clone(), depth + 1));
            }
            nodes.push(GroupNode { group, path, depth });
        }
        nodes
    }
}

impl AppConfig {
    pub(super) fn validate_group_tree(&self) -> Result<()> {
        let parents: HashMap<_, _> = self.groups.iter().map(|g| (g.id, g.parent)).collect();
        for group in &self.groups {
            let mut ancestors = HashSet::new();
            let mut current = Some(group.id);
            while let Some(id) = current {
                if !ancestors.insert(id) {
                    return Err(invalid(i18n_key::CONNECTIONS_GROUP_CYCLE));
                }
                current = *parents
                    .get(&id)
                    .ok_or_else(|| invalid(i18n_key::CONNECTIONS_GROUP_PARENT_MISSING))?;
            }
        }
        Ok(())
    }

    /// Reparent a whole subtree, optionally placing it before a sibling.
    /// Check every reference before mutating so rejected drops leave it intact.
    pub fn move_group(
        &mut self,
        id: Uuid,
        parent: Option<Uuid>,
        before: Option<Uuid>,
    ) -> Result<()> {
        let from = self
            .groups
            .iter()
            .position(|g| g.id == id)
            .ok_or_else(|| invalid(i18n_key::CONNECTIONS_GROUP_PARENT_MISSING))?;
        let mut current = parent;
        let mut visited = HashSet::from([id]);
        while let Some(ancestor) = current {
            if !visited.insert(ancestor) {
                return Err(invalid(i18n_key::CONNECTIONS_GROUP_CYCLE));
            }
            current = self
                .groups
                .iter()
                .find(|g| g.id == ancestor)
                .ok_or_else(|| invalid(i18n_key::CONNECTIONS_GROUP_PARENT_MISSING))?
                .parent;
        }
        if let Some(before) = before {
            if before == id {
                return Ok(());
            }
            if !self
                .groups
                .iter()
                .any(|g| g.id == before && g.parent == parent)
            {
                return Err(invalid(i18n_key::CONNECTIONS_GROUP_PARENT_MISSING));
            }
        }
        let mut group = self.groups.remove(from);
        group.parent = parent;
        let to = before
            .and_then(|id| self.groups.iter().position(|g| g.id == id))
            .unwrap_or(self.groups.len());
        self.groups.insert(to, group);
        Ok(())
    }

    /// Delete only the container, promoting its children and connections.
    pub fn remove_group(&mut self, id: Uuid) {
        let Some(index) = self.groups.iter().position(|g| g.id == id) else {
            return;
        };
        let parent = self.groups.remove(index).parent;
        for group in &mut self.groups {
            if group.parent == Some(id) {
                group.parent = parent;
            }
        }
        for profile in &mut self.profiles {
            if profile.group == Some(id) {
                profile.group = parent;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{LocalConfig, Profile};

    fn child(name: &str, parent: Uuid) -> Group {
        let mut group = Group::new(name);
        group.parent = Some(parent);
        group
    }

    #[test]
    fn old_flat_groups_and_nested_paths_round_trip() {
        let company = Group::new("Company A");
        let legacy = format!("id = '{}'\nname = 'Company A'", company.id);
        assert_eq!(toml::from_str::<Group>(&legacy).unwrap().parent, None);
        let other = Group::new("Company B");
        let first = child("Project", company.id);
        let second = child("Project", other.id);
        let config = AppConfig {
            groups: vec![first.clone(), other, second, company],
            ..AppConfig::default()
        };
        config.validate().unwrap();
        let restored: AppConfig = toml::from_str(&toml::to_string(&config).unwrap()).unwrap();
        restored.validate().unwrap();
        let nodes = Group::tree(&restored.groups);
        assert_eq!(
            nodes
                .iter()
                .map(|n| (n.path.as_str(), n.depth))
                .collect::<Vec<_>>(),
            vec![
                ("Company B", 0),
                ("Company B / Project", 1),
                ("Company A", 0),
                ("Company A / Project", 1),
            ]
        );
        assert_eq!(nodes[3].group.id, first.id);
    }

    #[test]
    fn invalid_parent_and_cycles_are_rejected_without_mutating_moves() {
        let company = Group::new("Company");
        let project = child("Project", company.id);
        let environment = child("Environment", project.id);
        let mut config = AppConfig {
            groups: vec![company.clone(), project.clone(), environment.clone()],
            ..AppConfig::default()
        };
        let original = toml::to_string(&config).unwrap();
        for parent in [company.id, project.id, environment.id, Uuid::new_v4()] {
            assert!(config.move_group(company.id, Some(parent), None).is_err());
            assert_eq!(toml::to_string(&config).unwrap(), original);
            config.groups[0].parent = Some(parent);
            assert!(config.validate().is_err());
            config.groups[0].parent = None;
        }
        assert!(
            config
                .move_group(project.id, None, Some(Uuid::new_v4()))
                .is_err()
        );
        assert_eq!(toml::to_string(&config).unwrap(), original);
    }

    #[test]
    fn moving_and_removing_containers_preserves_their_contents() {
        let company = Group::new("Company");
        let project = child("Project", company.id);
        let other = child("Other", company.id);
        let environment = child("Environment", project.id);
        let mut profile = Profile::local("Shell", LocalConfig::default());
        profile.group = Some(project.id);
        let mut config = AppConfig {
            groups: vec![
                company.clone(),
                other.clone(),
                project.clone(),
                environment.clone(),
            ],
            profiles: vec![profile.clone()],
            ..AppConfig::default()
        };
        config
            .move_group(project.id, Some(company.id), Some(other.id))
            .unwrap();
        assert_eq!(
            Group::tree(&config.groups)
                .iter()
                .map(|n| n.group.id)
                .collect::<Vec<_>>(),
            vec![company.id, project.id, environment.id, other.id]
        );
        config.move_group(project.id, None, None).unwrap();
        config.validate().unwrap();
        assert_eq!(
            Group::tree(&config.groups).last().unwrap().path,
            "Project / Environment"
        );
        assert_eq!(config.profiles[0].group, Some(project.id));
        config
            .move_group(project.id, Some(company.id), None)
            .unwrap();
        config.remove_group(project.id);
        config.validate().unwrap();
        assert_eq!(config.profiles[0].group, Some(company.id));
        assert_eq!(
            config
                .groups
                .iter()
                .find(|g| g.id == environment.id)
                .unwrap()
                .parent,
            Some(company.id)
        );
        config.remove_group(company.id);
        config.validate().unwrap();
        assert!(config.groups.iter().all(|g| g.parent.is_none()));
        assert_eq!(config.profiles.len(), 1);
        assert_eq!(config.profiles[0].id, profile.id);
        assert_eq!(config.profiles[0].group, None);
    }
}
