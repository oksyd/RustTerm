use super::*;
use crate::app::layout::{Axis, PaneNode};

fn fixture(profiles: &[Profile]) -> (Vec<Tab>, HashMap<usize, Profile>) {
    let tabs = profiles
        .iter()
        .enumerate()
        .map(|(i, profile)| Tab {
            id: i + 1,
            title: profile.name.clone(),
            layout: PaneNode::Leaf(i + 1),
            focused: i + 1,
        })
        .collect();
    let profiles = profiles
        .iter()
        .enumerate()
        .map(|(i, profile)| (i + 1, profile.clone()))
        .collect();
    (tabs, profiles)
}

fn ids(tabs: &[Tab]) -> Vec<usize> {
    tabs.iter().map(|tab| tab.id).collect()
}

#[test]
fn groups_same_ssh_profile_at_first_position_and_leaves_other_protocols_separate() {
    let a = Profile::ssh("server", Default::default());
    let b = Profile::ssh("server", Default::default());
    let local = Profile::local("local", Default::default());
    let rdp = Profile::rdp("server", Default::default());
    let (mut tabs, profiles) = fixture(&[
        a.clone(),
        local,
        a.clone(),
        b.clone(),
        b.clone(),
        rdp.clone(),
        rdp,
    ]);
    let groups = group_tabs(&tabs, &profiles);
    assert_eq!(
        groups,
        vec![
            TabGroup {
                profile: Some(a.id),
                tabs: vec![1, 3]
            },
            TabGroup {
                profile: None,
                tabs: vec![2]
            },
            TabGroup {
                profile: Some(b.id),
                tabs: vec![4, 5]
            },
            TabGroup {
                profile: None,
                tabs: vec![6]
            },
            TabGroup {
                profile: None,
                tabs: vec![7]
            },
        ]
    );
    apply_order(&mut tabs, &groups);
    assert_eq!(ids(&tabs), vec![1, 3, 2, 4, 5, 6, 7]);
    assert_eq!(group_tabs(&tabs, &profiles), groups);
}

#[test]
fn mixed_splits_stay_independent_and_homogeneous_splits_join_their_profile() {
    let a = Profile::ssh("A", Default::default());
    let b = Profile::ssh("B", Default::default());
    let (mut tabs, mut profiles) = fixture(&[a.clone(), a.clone(), a.clone()]);
    profiles.insert(90, b);
    profiles.insert(91, a.clone());
    assert!(tabs[1].layout.split(2, 90, 100, Axis::Horizontal, 0.5));
    assert!(tabs[2].layout.split(3, 91, 101, Axis::Vertical, 0.5));
    let groups = group_tabs(&tabs, &profiles);
    assert_eq!(
        groups[0],
        TabGroup {
            profile: Some(a.id),
            tabs: vec![1, 3]
        }
    );
    assert_eq!(
        groups[1],
        TabGroup {
            profile: None,
            tabs: vec![2]
        }
    );
    profiles.remove(&91);
    assert!(
        group_tabs(&tabs, &profiles)
            .iter()
            .all(|group| group.profile.is_none())
    );
}

#[test]
fn groups_appear_on_second_tab_and_disappear_when_only_one_remains() {
    let a = Profile::ssh("A", Default::default());
    let (mut tabs, mut profiles) = fixture(&[
        a.clone(),
        Profile::local("local", Default::default()),
        a.clone(),
    ]);
    let groups = group_tabs(&tabs, &profiles);
    apply_order(&mut tabs, &groups);
    tabs.retain(|tab| tab.id != 3);
    let groups = group_tabs(&tabs, &profiles);
    assert_eq!(groups[0].profile, None);
    assert_eq!(groups[0].tabs, [1]);
    profiles.insert(4, a.clone());
    tabs.push(Tab {
        id: 4,
        title: a.name.clone(),
        layout: PaneNode::Leaf(4),
        focused: 4,
    });
    let groups = group_tabs(&tabs, &profiles);
    apply_order(&mut tabs, &groups);
    assert_eq!(ids(&tabs), vec![1, 4, 2]);
    assert_eq!(groups[0].profile, Some(a.id));
}

#[test]
fn dragging_reorders_members_and_moves_whole_groups_without_splitting_them() {
    let a = Profile::ssh("A", Default::default());
    let b = Profile::ssh("B", Default::default());
    let (mut tabs, profiles) = fixture(&[
        a.clone(),
        a,
        Profile::local("local", Default::default()),
        b.clone(),
        b,
    ]);
    move_tab(&mut tabs, &profiles, 1, 2, false);
    assert_eq!(ids(&tabs), vec![2, 1, 3, 4, 5]);
    move_tab(&mut tabs, &profiles, 2, 4, true);
    assert_eq!(ids(&tabs), vec![3, 4, 5, 2, 1]);
    move_tab(&mut tabs, &profiles, 3, 5, false);
    assert_eq!(ids(&tabs), vec![4, 5, 3, 2, 1]);
    move_tab(&mut tabs, &profiles, 2, 1, true);
    assert_eq!(ids(&tabs), vec![4, 5, 3, 2, 1]);
    move_tab(&mut tabs, &profiles, 1, 4, false);
    assert_eq!(ids(&tabs), vec![2, 1, 4, 5, 3]);
    assert!(
        tabs.iter()
            .all(|tab| tab.layout.leaves() == vec![tab.id] && tab.focused == tab.id)
    );
    move_tab(&mut tabs, &profiles, 99, 4, false);
    move_tab(&mut tabs, &profiles, 1, 99, false);
    assert_eq!(ids(&tabs), vec![2, 1, 4, 5, 3]);
}

fn colliding_color_groups(count: usize) -> Vec<TabGroup> {
    (0..count)
        .map(|i| TabGroup {
            // These IDs all collide under the original hash-only allocation.
            profile: Some(Uuid::from_u128(1 + i as u128 * 6)),
            tabs: vec![i * 2 + 1, i * 2 + 2],
        })
        .collect()
}

#[test]
fn six_groups_have_distinct_colors_even_when_their_ids_collide() {
    let groups = colliding_color_groups(6);
    let mut colors = TabGroupColors::default();
    let mut previous = HashMap::new();
    for count in 1..=groups.len() {
        colors.reconcile(&groups[..count]);
        let current: HashMap<_, _> = groups[..count]
            .iter()
            .map(|group| {
                let profile = group.profile.unwrap();
                (profile, colors.color(profile))
            })
            .collect();
        assert_eq!(current.values().collect::<HashSet<_>>().len(), count);
        for (profile, color) in previous {
            assert_eq!(current[&profile], color);
        }
        previous = current;
    }
}

#[test]
fn group_colors_survive_reordering_and_reuse_only_released_colors() {
    let mut groups = colliding_color_groups(6);
    let mut colors = TabGroupColors::default();
    colors.reconcile(&groups);
    let original: HashMap<_, _> = groups
        .iter()
        .map(|group| {
            let profile = group.profile.unwrap();
            (profile, colors.color(profile))
        })
        .collect();
    groups.reverse();
    colors.reconcile(&groups);
    for group in &groups {
        let profile = group.profile.unwrap();
        assert_eq!(colors.color(profile), original[&profile]);
    }
    let removed = groups.remove(2).profile.unwrap();
    colors.reconcile(&groups);
    let new_group = colliding_color_groups(7).pop().unwrap();
    let new_profile = new_group.profile.unwrap();
    groups.insert(0, new_group);
    colors.reconcile(&groups);
    assert_eq!(colors.color(new_profile), original[&removed]);
    for group in &groups[1..] {
        let profile = group.profile.unwrap();
        assert_eq!(colors.color(profile), original[&profile]);
    }
    // A group disappearing into a singleton must release its color as well.
    for group in &mut groups {
        group.profile = None;
    }
    colors.reconcile(&groups);
    assert!(colors.assignments.is_empty());
}

#[test]
fn overflow_shares_colors_evenly_and_becomes_unique_again_when_groups_close() {
    let mut groups = colliding_color_groups(8);
    let mut colors = TabGroupColors::default();
    colors.reconcile(&groups);
    let mut usage = HashMap::new();
    for group in &groups {
        *usage
            .entry(colors.color(group.profile.unwrap()))
            .or_insert(0) += 1;
    }
    assert_eq!(usage.len(), 6);
    assert!(usage.values().all(|count| (1..=2).contains(count)));
    let older_owners: Vec<_> = groups[..2]
        .iter()
        .map(|group| {
            let profile = group.profile.unwrap();
            (profile, colors.color(profile))
        })
        .collect();
    // Remove two unique colors, leaving two collisions among six groups.
    groups.drain(2..4);
    groups.reverse();
    colors.reconcile(&groups);
    let remaining: HashSet<_> = groups
        .iter()
        .map(|group| colors.color(group.profile.unwrap()))
        .collect();
    assert_eq!(remaining.len(), 6);
    for (profile, color) in older_owners {
        assert_eq!(colors.color(profile), color);
    }
}
