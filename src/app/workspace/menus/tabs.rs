use super::{Menu, MenuAction, MenuItem};
use crate::app::pane::Pane;
use crate::app::workspace::{Tab, Workspace, tab_groups::group_tabs};
use gpui::{App, Context, Pixels, Point, Window};
use rustterm::config::{Connection, Profile, Uuid};
use rustterm::i18n::messages as i18n_key;
use std::collections::HashMap;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Target {
    Tab(usize),
    Group(Uuid),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(in crate::app::workspace) struct TabMenu {
    target: Target,
    // Keep the list stable while open; availability is checked again on activation.
    new_session: bool,
}

impl TabMenu {
    pub(super) fn items(self) -> &'static [MenuItem] {
        use MenuAction::*;
        const TAB: &[MenuItem] = &[
            MenuItem::new(i18n_key::WORKSPACE_NEW_TAB_SESSION, "", NewTabSession),
            MenuItem::new(i18n_key::WORKSPACE_CLOSE_TAB, "", CloseTab),
            MenuItem::new(i18n_key::WORKSPACE_CLOSE_OTHER_TABS, "", CloseOtherTabs),
            MenuItem::new(
                i18n_key::WORKSPACE_CLOSE_TABS_TO_RIGHT,
                "",
                CloseTabsToRight,
            ),
        ];
        const GROUP: &[MenuItem] = &[
            MenuItem::new(i18n_key::WORKSPACE_NEW_TAB_SESSION, "", NewTabSession),
            MenuItem::new(i18n_key::WORKSPACE_CLOSE_TAB_GROUP, "", CloseTabGroup),
        ];
        let items = match self.target {
            Target::Tab(_) => TAB,
            Target::Group(_) => GROUP,
        };
        &items[usize::from(!self.new_session)..]
    }
}

impl MenuAction {
    pub(super) fn is_tab_action(self) -> bool {
        matches!(
            self,
            Self::NewTabSession
                | Self::CloseTab
                | Self::CloseOtherTabs
                | Self::CloseTabsToRight
                | Self::CloseTabGroup
        )
    }
}

impl Workspace {
    pub(in crate::app::workspace) fn open_tab_menu(
        &mut self,
        id: usize,
        position: Point<Pixels>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.open_tab_context(Target::Tab(id), position, window, cx);
    }

    pub(in crate::app::workspace) fn open_tab_group_menu(
        &mut self,
        id: Uuid,
        position: Point<Pixels>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.open_tab_context(Target::Group(id), position, window, cx);
    }

    fn open_tab_context(
        &mut self,
        target: Target,
        position: Point<Pixels>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.closing
            || self.lock_screen.locked
            || self.modal.is_some()
            || self.window_state.prompts.is_busy()
            || target_tabs(target, &self.tabs, &self.pane_profiles).is_empty()
        {
            return;
        }
        if self.menu.take().is_some() {
            self.restore_popup_focus(window, cx);
        }
        self.remember_popup_focus(window, cx);
        self.release_rdp_control(cx);
        self.menu = Some(Menu::Tab(TabMenu {
            target,
            new_session: self.tab_session_profile(target, cx).is_some(),
        }));
        // Opening a background tab's menu does not switch tabs or acknowledge output.
        self.menu_anchor = position;
        self.menu_index = 0;
        self.menu_scroll.scroll_to_item(0);
        self.menu_focus.focus(window);
        cx.notify();
    }

    fn tab_session_profile(&self, target: Target, cx: &App) -> Option<Profile> {
        let tabs = target_tabs(target, &self.tabs, &self.pane_profiles);
        let candidates = tabs
            .iter()
            .flat_map(|tab| tab.layout.leaves())
            .collect::<Vec<_>>();
        // A split tab has no unambiguous connection to duplicate. A group is homogeneous.
        if matches!(target, Target::Tab(_)) && candidates.len() != 1 {
            return None;
        }
        candidates.into_iter().find_map(|id| {
            let Some(Pane::Terminal(view)) = self.panes.get(&id) else {
                return None;
            };
            if view.read(cx).is_container() {
                return None;
            }
            let profile = self.profile_for_pane(id, cx)?;
            matches!(
                profile.connection,
                Connection::Local(_) | Connection::Ssh(_)
            )
            .then_some(profile)
        })
    }

    pub(super) fn tab_action_enabled(&self, action: MenuAction, cx: &App) -> bool {
        let Some(Menu::Tab(menu)) = self.menu else {
            return false;
        };
        if !menu.items().iter().any(|item| item.action == action) {
            return false;
        }
        if action == MenuAction::NewTabSession {
            self.tab_session_profile(menu.target, cx).is_some()
        } else {
            !close_targets(menu.target, action, &self.tabs, &self.pane_profiles).is_empty()
        }
    }

    pub(super) fn tab_menu_action(
        &mut self,
        action: MenuAction,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(Menu::Tab(menu)) = self.menu.take() else {
            return;
        };
        self.restore_popup_focus(window, cx);
        if action == MenuAction::NewTabSession {
            if let Some(profile) = self.tab_session_profile(menu.target, cx) {
                self.connect_profile(profile, None, window, cx);
            }
        } else {
            // Resolve by stable ID, then freeze pane IDs before the asynchronous
            // confirmation. New tabs opened while confirming must not be closed.
            let ids = close_targets(menu.target, action, &self.tabs, &self.pane_profiles);
            if !ids.is_empty() {
                self.confirm_close(ids, window, cx);
            }
        }
        cx.notify();
    }
}

fn target_tabs<'a>(
    target: Target,
    tabs: &'a [Tab],
    profiles: &HashMap<usize, Profile>,
) -> Vec<&'a Tab> {
    match target {
        Target::Tab(id) => tabs.iter().filter(|tab| tab.id == id).collect(),
        Target::Group(id) => {
            let Some(group) = group_tabs(tabs, profiles)
                .into_iter()
                .find(|group| group.profile == Some(id))
            else {
                return Vec::new();
            };
            tabs.iter()
                .filter(|tab| group.tabs.contains(&tab.id))
                .collect()
        }
    }
}

fn close_targets(
    target: Target,
    action: MenuAction,
    tabs: &[Tab],
    profiles: &HashMap<usize, Profile>,
) -> Vec<usize> {
    use MenuAction::*;
    if let Target::Group(_) = target {
        return if action == CloseTabGroup {
            target_tabs(target, tabs, profiles)
                .iter()
                .flat_map(|tab| tab.layout.leaves())
                .collect()
        } else {
            Vec::new()
        };
    }
    let Target::Tab(id) = target else {
        unreachable!()
    };
    let Some(index) = tabs.iter().position(|tab| tab.id == id) else {
        // A stale target must not turn "close others" into "close everything".
        return Vec::new();
    };
    tabs.iter()
        .enumerate()
        .filter(|(candidate, _)| match action {
            CloseTab => *candidate == index,
            CloseOtherTabs => *candidate != index,
            CloseTabsToRight => *candidate > index,
            _ => false,
        })
        .flat_map(|(_, tab)| tab.layout.leaves())
        .collect()
}

#[cfg(test)]
mod tests;
