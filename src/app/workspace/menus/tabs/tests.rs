use super::*;
use crate::app::layout::{Axis, PaneNode};

fn tab(id: usize, pane: usize) -> Tab {
    Tab {
        id,
        title: "same name".into(),
        layout: PaneNode::Leaf(pane),
        focused: pane,
    }
}

#[test]
fn background_target_and_its_splits_are_independent_of_active_tab_and_reordering() {
    let mut tabs = vec![tab(10, 1), tab(20, 2), tab(30, 3)];
    assert!(tabs[1].layout.split(2, 4, 100, Axis::Horizontal, 0.5));
    let profiles = HashMap::new();
    let target = Target::Tab(20);
    assert_eq!(
        close_targets(target, MenuAction::CloseTab, &tabs, &profiles),
        [2, 4]
    );
    assert_eq!(
        close_targets(target, MenuAction::CloseOtherTabs, &tabs, &profiles),
        [1, 3]
    );
    assert_eq!(
        close_targets(target, MenuAction::CloseTabsToRight, &tabs, &profiles),
        [3]
    );
    tabs.swap(0, 1);
    assert_eq!(
        close_targets(target, MenuAction::CloseTab, &tabs, &profiles),
        [2, 4]
    );
    assert_eq!(
        close_targets(target, MenuAction::CloseTabsToRight, &tabs, &profiles),
        [1, 3]
    );
}

#[test]
fn stale_or_empty_targets_cannot_close_unrelated_tabs() {
    let tabs = [tab(10, 1)];
    let profiles = HashMap::new();
    for action in [
        MenuAction::CloseTab,
        MenuAction::CloseOtherTabs,
        MenuAction::CloseTabsToRight,
    ] {
        assert!(close_targets(Target::Tab(999), action, &tabs, &profiles).is_empty());
    }
    for action in [MenuAction::CloseOtherTabs, MenuAction::CloseTabsToRight] {
        assert!(close_targets(Target::Tab(10), action, &tabs, &profiles).is_empty());
    }
}

#[test]
fn group_close_includes_split_panes_but_not_same_named_connections_or_mixed_tabs() {
    let ssh = Profile::ssh("same name", Default::default());
    let other = Profile::ssh("same name", Default::default());
    let mut tabs = vec![tab(10, 1), tab(20, 2), tab(30, 3), tab(40, 6)];
    assert!(tabs[1].layout.split(2, 4, 100, Axis::Horizontal, 0.5));
    assert!(tabs[3].layout.split(6, 7, 101, Axis::Vertical, 0.5));
    let profiles = HashMap::from([
        (1, ssh.clone()),
        (2, ssh.clone()),
        (3, other.clone()),
        (4, ssh.clone()),
        (6, ssh.clone()),
        (7, other),
    ]);
    let target = Target::Group(ssh.id);
    let selected = close_targets(target, MenuAction::CloseTabGroup, &tabs, &profiles);
    assert_eq!(selected, [1, 2, 4]);
    tabs.remove(1);
    assert!(close_targets(target, MenuAction::CloseTabGroup, &tabs, &profiles).is_empty());
    assert!(close_targets(Target::Tab(10), MenuAction::CloseTabGroup, &tabs, &profiles).is_empty());
}

#[test]
fn unsupported_sessions_omit_new_session_and_tab_close_has_no_pane_shortcut() {
    for target in [Target::Tab(10), Target::Group(Uuid::new_v4())] {
        let menu = TabMenu {
            target,
            new_session: false,
        };
        assert!(
            !menu
                .items()
                .iter()
                .any(|item| item.action == MenuAction::NewTabSession)
        );
        assert!(menu.items().iter().all(|item| item.shortcut.is_empty()));
        let menu = TabMenu {
            new_session: true,
            ..menu
        };
        assert_eq!(menu.items()[0].action, MenuAction::NewTabSession);
        assert!(menu.items().len() <= 4);
    }
}
