use super::{Tab, Workspace};
use crate::app::ui;
use gpui::Context;
use rustterm::config::{Connection, Profile, Uuid};
use std::collections::{HashMap, HashSet};

#[cfg(test)]
mod tests;

#[derive(Debug, PartialEq, Eq)]
pub(super) struct TabGroup {
    pub profile: Option<Uuid>,
    pub tabs: Vec<usize>,
}

const GROUP_COLORS: [u32; 6] = [
    ui::ACCENT,
    0x8bb6ca,
    ui::SUCCESS,
    ui::WARNING,
    0xd197b3,
    ui::BRAND,
];

#[derive(Default)]
pub(super) struct TabGroupColors {
    // Allocation order is independent of tab order, so older groups keep their
    // colors when an overflow group can move to a newly available color.
    assignments: Vec<(Uuid, usize)>,
}

fn preferred_color(profile: Uuid) -> usize {
    profile.as_bytes().iter().fold(0, |hash, byte| {
        (hash * 31 + usize::from(*byte)) % GROUP_COLORS.len()
    })
}

fn least_used_color(profile: Uuid, usage: &[usize; GROUP_COLORS.len()]) -> usize {
    let preferred = preferred_color(profile);
    (0..GROUP_COLORS.len())
        .map(|offset| (preferred + offset) % GROUP_COLORS.len())
        .min_by_key(|index| usage[*index])
        .unwrap()
}

impl TabGroupColors {
    fn reconcile(&mut self, groups: &[TabGroup]) {
        let active: HashSet<_> = groups.iter().filter_map(|group| group.profile).collect();
        self.assignments
            .retain(|(profile, _)| active.contains(profile));
        let mut usage = [0; GROUP_COLORS.len()];
        for &(_, index) in &self.assignments {
            usage[index] += 1;
        }
        // After shrinking from more than six groups, repair duplicate colors
        // using newly freed slots without recoloring unique or older owners.
        for (profile, index) in self.assignments.iter_mut().rev() {
            if usage[*index] > 1 {
                let available = least_used_color(*profile, &usage);
                if usage[available] == 0 {
                    usage[*index] -= 1;
                    usage[available] += 1;
                    *index = available;
                }
            }
        }
        for profile in groups.iter().filter_map(|group| group.profile) {
            if self.assignments.iter().any(|(id, _)| *id == profile) {
                continue;
            }
            let index = least_used_color(profile, &usage);
            usage[index] += 1;
            self.assignments.push((profile, index));
        }
    }

    pub(super) fn color(&self, profile: Uuid) -> u32 {
        let index = self
            .assignments
            .iter()
            .find_map(|(id, index)| (*id == profile).then_some(*index))
            .unwrap_or_else(|| preferred_color(profile));
        GROUP_COLORS[index]
    }
}

/// A mixed workspace belongs to neither connection's group.
fn tab_profile(tab: &Tab, profiles: &HashMap<usize, Profile>) -> Option<Uuid> {
    let leaves = tab.layout.leaves();
    let first = profiles.get(leaves.first()?)?;
    leaves
        .iter()
        .all(|id| {
            profiles.get(id).is_some_and(|profile| {
                profile.id == first.id && matches!(profile.connection, Connection::Ssh(_))
            })
        })
        .then_some(first.id)
}

/// Preserve each group's first position and its members' relative order.
pub(super) fn group_tabs(tabs: &[Tab], profiles: &HashMap<usize, Profile>) -> Vec<TabGroup> {
    let mut groups: Vec<TabGroup> = Vec::new();
    let mut positions: HashMap<Uuid, usize> = HashMap::new();
    for tab in tabs {
        let profile = tab_profile(tab, profiles);
        if let Some(index) = profile.and_then(|profile| positions.get(&profile)).copied() {
            groups[index].tabs.push(tab.id);
        } else {
            if let Some(profile) = profile {
                positions.insert(profile, groups.len());
            }
            groups.push(TabGroup {
                profile,
                tabs: vec![tab.id],
            });
        }
    }
    for group in &mut groups {
        if group.tabs.len() < 2 {
            group.profile = None;
        }
    }
    groups
}

fn apply_order(tabs: &mut [Tab], groups: &[TabGroup]) {
    let positions: HashMap<_, _> = groups
        .iter()
        .flat_map(|group| group.tabs.iter().copied())
        .enumerate()
        .map(|(index, id)| (id, index))
        .collect();
    tabs.sort_by_key(|tab| positions[&tab.id]);
}

/// Return the direct child index of a tab, or its header while collapsed.
fn visible_index(groups: &[TabGroup], collapsed: &HashSet<Uuid>, tab: usize) -> Option<usize> {
    let mut index = 0;
    for group in groups {
        let folded = group
            .profile
            .is_some_and(|profile| collapsed.contains(&profile));
        if let Some(member) = group.tabs.iter().position(|id| *id == tab) {
            return Some(
                index
                    + if folded {
                        0
                    } else {
                        usize::from(group.profile.is_some()) + member
                    },
            );
        }
        index += usize::from(group.profile.is_some()) + if folded { 0 } else { group.tabs.len() };
    }
    None
}

fn move_tab(
    tabs: &mut [Tab],
    profiles: &HashMap<usize, Profile>,
    from: usize,
    to: usize,
    whole_group: bool,
) {
    let mut groups = group_tabs(tabs, profiles);
    let Some(source) = groups.iter().position(|group| group.tabs.contains(&from)) else {
        return;
    };
    let Some(target) = groups.iter().position(|group| group.tabs.contains(&to)) else {
        return;
    };
    if source == target {
        if whole_group {
            return;
        }
        let members = &mut groups[source].tabs;
        let from = members.iter().position(|id| *id == from).unwrap();
        let to = members.iter().position(|id| *id == to).unwrap();
        let tab = members.remove(from);
        members.insert(to, tab);
    } else {
        // Automatic membership cannot be broken by dropping into another profile.
        let group = groups.remove(source);
        groups.insert(target, group);
    }
    apply_order(tabs, &groups);
}

impl Workspace {
    pub(super) fn tab_groups(&self) -> Vec<TabGroup> {
        group_tabs(&self.tabs, &self.pane_profiles)
    }

    pub(super) fn regroup_tabs(&mut self) {
        let active = self.tabs.get(self.active).map(|tab| tab.id);
        let groups = self.tab_groups();
        apply_order(&mut self.tabs, &groups);
        if let Some(index) = active.and_then(|id| self.tabs.iter().position(|tab| tab.id == id)) {
            self.active = index;
        }
        self.collapsed_tab_groups
            .retain(|profile| groups.iter().any(|group| group.profile == Some(*profile)));
        self.tab_group_colors.reconcile(&groups);
    }

    pub(super) fn expand_active_tab_group(&mut self) {
        if let Some(profile) = self
            .tabs
            .get(self.active)
            .and_then(|tab| tab_profile(tab, &self.pane_profiles))
        {
            self.collapsed_tab_groups.remove(&profile);
        }
    }

    pub(super) fn scroll_active_tab_into_view(&self) {
        if let Some(index) = self
            .tabs
            .get(self.active)
            .and_then(|tab| visible_index(&self.tab_groups(), &self.collapsed_tab_groups, tab.id))
        {
            self.tab_scroll.scroll_to_item(index);
        }
    }

    pub(super) fn toggle_tab_group(&mut self, profile: Uuid, cx: &mut Context<Self>) {
        if !self.collapsed_tab_groups.remove(&profile) {
            self.collapsed_tab_groups.insert(profile);
        }
        let groups = self.tab_groups();
        if let Some(group) = groups.iter().find(|group| group.profile == Some(profile)) {
            // The first member follows its header when expanded.
            if let Some(index) = visible_index(&groups, &self.collapsed_tab_groups, group.tabs[0]) {
                self.tab_scroll.scroll_to_item(
                    index - usize::from(!self.collapsed_tab_groups.contains(&profile)),
                );
            }
        }
        cx.notify();
    }

    pub(super) fn reorder_tab(
        &mut self,
        from: usize,
        to: usize,
        whole_group: bool,
        cx: &mut Context<Self>,
    ) {
        let active = self.tabs.get(self.active).map(|tab| tab.id);
        move_tab(&mut self.tabs, &self.pane_profiles, from, to, whole_group);
        if let Some(index) = active.and_then(|id| self.tabs.iter().position(|tab| tab.id == id)) {
            self.active = index;
        }
        self.scroll_active_tab_into_view();
        cx.notify();
    }
}
