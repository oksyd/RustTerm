use rustterm::i18n::messages as i18n_key;
mod render;
mod tabs;

use super::sidebar::LibraryItem;
use super::{Modal, Workspace};
use crate::app::forms::{Fields, ProfileKind, SettingsSection};
use crate::app::layout::Axis;
use crate::app::pane::{Pane, PaneEvent};
use crate::app::ui;
use gpui::{App, Bounds, Context, KeyDownEvent, Pixels, Window, px};
use rustterm::config::{Connection, Profile, Uuid};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Menu {
    File,
    Edit,
    View,
    Session,
    Help,
    NewConnection,
    ConnectionType,
    Tabs,
    Create,
    Profile(Uuid),
    Group(Uuid),
    Tab(tabs::TabMenu),
}
pub(super) const APPLICATION_MENUS: [(Menu, &str); 5] = [
    (Menu::File, i18n_key::COMMON_FILE),
    (Menu::Edit, i18n_key::COMMON_EDIT),
    (Menu::View, i18n_key::WORKSPACE_VIEW),
    (Menu::Session, i18n_key::WORKSPACE_CONNECTIONS),
    (Menu::Help, i18n_key::COMMON_HELP),
];
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum MenuAction {
    FilterType(Option<super::connection_kind::ConnectionKind>),
    ActivateTab(usize),
    Palette,
    Forwards,
    NewTerminal,
    NewConnection,
    NewLocal,
    NewSsh,
    NewTelnet,
    NewRedis,
    NewElasticsearch,
    NewMysql,
    NewClickHouse,
    NewPostgres,
    NewSqlite,
    NewRdp,
    NewGroup,
    NewSubgroup,
    EditGroup,
    DeleteGroup,
    GroupToRoot,
    Settings,
    Updates,
    Lock,
    Quit,
    Copy,
    Paste,
    Search,
    Files,
    Filter,
    SplitHorizontal,
    SplitVertical,
    NextPane,
    NextTab,
    Sidebar,
    Zoom,
    LineNumbers,
    LineTimestamps,
    Highlight,
    Disconnect,
    Close,
    Trust,
    Help,
    EditProfile,
    OpenProfile,
    OpenProfileRight,
    OpenProfileBelow,
    DeleteProfile,
    NewTabSession,
    CloseTab,
    CloseOtherTabs,
    CloseTabsToRight,
    CloseTabGroup,
}
#[derive(Clone)]
struct MenuItem {
    label: std::borrow::Cow<'static, str>,
    shortcut: &'static str,
    action: MenuAction,
}
impl MenuItem {
    const fn new(label: &'static str, shortcut: &'static str, action: MenuAction) -> Self {
        Self {
            label: std::borrow::Cow::Borrowed(label),
            shortcut,
            action,
        }
    }
}
impl Menu {
    pub(super) fn category(self) -> Option<Self> {
        match self {
            Self::File | Self::NewConnection => Some(Self::File),
            Self::Edit | Self::View | Self::Session | Self::Help => Some(self),
            Self::Create
            | Self::Profile(_)
            | Self::Group(_)
            | Self::Tab(_)
            | Self::ConnectionType
            | Self::Tabs => None,
        }
    }

    pub(super) fn category_index(self) -> Option<usize> {
        let category = self.category()?;
        APPLICATION_MENUS
            .iter()
            .position(|(menu, _)| *menu == category)
    }

    fn items(self) -> &'static [MenuItem] {
        use MenuAction::*;
        match self {
            Self::ConnectionType | Self::Tabs => &[],
            Self::Tab(menu) => menu.items(),
            Self::File => {
                const {
                    &[
                        MenuItem::new(
                            i18n_key::WORKSPACE_NEW_LOCAL_TERMINAL,
                            "Ctrl+Shift+T",
                            NewTerminal,
                        ),
                        MenuItem::new(i18n_key::COMMON_NEW_CONNECTION, "", NewConnection),
                        MenuItem::new(i18n_key::WORKSPACE_NEW_GROUP, "", NewGroup),
                        MenuItem::new(i18n_key::COMMON_SETTINGS_DIALOG, "Ctrl+,", Settings),
                        MenuItem::new(i18n_key::WORKSPACE_COMMAND_PALETTE, "Ctrl+Shift+P", Palette),
                        MenuItem::new(i18n_key::SETTINGS_LOCK_RUSTTERM, "Ctrl+Shift+L", Lock),
                        MenuItem::new(i18n_key::WORKSPACE_QUIT, "", Quit),
                    ]
                }
            }
            Self::Edit => {
                const {
                    &[
                        MenuItem::new(i18n_key::WORKSPACE_COPY_SELECTION, "Ctrl+Shift+C", Copy),
                        MenuItem::new(i18n_key::COMMON_PASTE, "Ctrl+Shift+V", Paste),
                        MenuItem::new(i18n_key::REDIS_SEARCH_CURRENT_PANE, "Ctrl+Shift+F", Search),
                        MenuItem::new(i18n_key::WORKSPACE_FILTER_CONNECTION_PROFILES, "", Filter),
                    ]
                }
            }
            Self::View => {
                const {
                    &[
                        MenuItem::new(
                            i18n_key::WORKSPACE_SHOW_CONNECTION_SIDEBAR,
                            "Ctrl+Shift+B",
                            Sidebar,
                        ),
                        MenuItem::new(
                            i18n_key::WORKSPACE_ZOOM_CURRENT_PANE,
                            "Ctrl+Shift+Enter",
                            Zoom,
                        ),
                        MenuItem::new(
                            i18n_key::WORKSPACE_DUPLICATE_CONNECTION_TO_THE_RIGHT,
                            "Ctrl+Shift+D",
                            SplitHorizontal,
                        ),
                        MenuItem::new(
                            i18n_key::WORKSPACE_DUPLICATE_CONNECTION_BELOW,
                            "Ctrl+Shift+E",
                            SplitVertical,
                        ),
                        MenuItem::new(i18n_key::WORKSPACE_NEXT_PANE, "Ctrl+Shift+J", NextPane),
                        MenuItem::new(i18n_key::WORKSPACE_NEXT_TAB, "Ctrl+Tab", NextTab),
                        MenuItem::new(i18n_key::WORKSPACE_SHOW_LINE_NUMBERS, "", LineNumbers),
                        MenuItem::new(
                            i18n_key::SETTINGS_SHOW_OUTPUT_TIMESTAMPS,
                            "",
                            LineTimestamps,
                        ),
                        MenuItem::new(i18n_key::WORKSPACE_TERMINAL_HIGHLIGHTING, "", Highlight),
                    ]
                }
            }
            Self::Session => {
                const {
                    &[
                        MenuItem::new(i18n_key::SFTP_FILE_MANAGER_DIALOG, "Ctrl+Shift+S", Files),
                        MenuItem::new(i18n_key::CONNECTIONS_MANAGE_PORT_FORWARDS, "", Forwards),
                        MenuItem::new(
                            i18n_key::WORKSPACE_DISCONNECT_RECONNECT_CURRENT_PANE,
                            "",
                            Disconnect,
                        ),
                        MenuItem::new(
                            i18n_key::WORKSPACE_CLOSE_CURRENT_PANE,
                            "Ctrl+Shift+W",
                            Close,
                        ),
                        MenuItem::new(i18n_key::WORKSPACE_MANAGE_TRUSTED_HOSTS, "", Trust),
                        MenuItem::new(i18n_key::WORKSPACE_NEW_GROUP, "", NewGroup),
                    ]
                }
            }
            Self::Help => {
                const {
                    &[
                        MenuItem::new(i18n_key::UPDATES_TITLE, "", Updates),
                        MenuItem::new(i18n_key::WORKSPACE_SHORTCUTS_AND_ABOUT_RUSTTERM, "", Help),
                    ]
                }
            }
            Self::NewConnection => {
                let items = Self::Create.items();
                &items[..items.len() - 1]
            }
            Self::Create => {
                const {
                    &[
                        MenuItem::new(i18n_key::WORKSPACE_ADD_SSH_PROFILE, "", NewSsh),
                        MenuItem::new(i18n_key::CONNECTIONS_ADD_TELNET_CONNECTION, "", NewTelnet),
                        MenuItem::new(i18n_key::REDIS_ADD_REDIS_CONNECTION, "", NewRedis),
                        MenuItem::new(i18n_key::ES_ADD_CONNECTION, "", NewElasticsearch),
                        MenuItem::new(i18n_key::DATABASE_ADD_MYSQL_CONNECTION, "", NewMysql),
                        MenuItem::new(
                            i18n_key::DATABASE_ADD_CLICKHOUSE_CONNECTION,
                            "",
                            NewClickHouse,
                        ),
                        MenuItem::new(
                            i18n_key::DATABASE_ADD_POSTGRESQL_CONNECTION,
                            "",
                            NewPostgres,
                        ),
                        MenuItem::new(i18n_key::DATABASE_ADD_SQLITE_DATABASE, "", NewSqlite),
                        MenuItem::new(i18n_key::RDP_ADD_RDP_CONNECTION, "", NewRdp),
                        MenuItem::new(i18n_key::WORKSPACE_ADD_LOCAL_PROFILE, "", NewLocal),
                        MenuItem::new(i18n_key::WORKSPACE_NEW_GROUP, "", NewGroup),
                    ]
                }
            }
            Self::Group(_) => {
                const {
                    &[
                        MenuItem::new(i18n_key::WORKSPACE_NEW_SUBGROUP, "", NewSubgroup),
                        MenuItem::new(i18n_key::WORKSPACE_EDIT_GROUP, "F2", EditGroup),
                        MenuItem::new(i18n_key::WORKSPACE_MOVE_GROUP_TO_ROOT, "", GroupToRoot),
                        MenuItem::new(i18n_key::WORKSPACE_DELETE_GROUP, "Delete", DeleteGroup),
                    ]
                }
            }
            Self::Profile(_) => {
                const {
                    &[
                        MenuItem::new(
                            i18n_key::WORKSPACE_EDIT_CONNECTION_PROFILE,
                            "F2",
                            EditProfile,
                        ),
                        MenuItem::new(i18n_key::WORKSPACE_OPEN_CONNECTION, "Enter", OpenProfile),
                        MenuItem::new(i18n_key::WORKSPACE_OPEN_TO_THE_RIGHT, "", OpenProfileRight),
                        MenuItem::new(i18n_key::WORKSPACE_OPEN_BELOW, "", OpenProfileBelow),
                        MenuItem::new(
                            i18n_key::WORKSPACE_DELETE_CONNECTION_PROFILE,
                            "Delete",
                            DeleteProfile,
                        ),
                    ]
                }
            }
        }
    }

    fn profile(self, profiles: &[Profile]) -> Option<&Profile> {
        let Self::Profile(id) = self else { return None };
        profiles.iter().find(|profile| profile.id == id)
    }
}

impl Workspace {
    fn menu_items(&self, menu: Menu) -> Vec<MenuItem> {
        match menu {
            Menu::ConnectionType => std::iter::once(MenuItem::new(
                i18n_key::WORKSPACE_ALL_TYPES,
                "",
                MenuAction::FilterType(None),
            ))
            .chain(
                super::connection_kind::ConnectionKind::ALL
                    .into_iter()
                    .map(|kind| {
                        MenuItem::new(kind.label(), "", MenuAction::FilterType(Some(kind)))
                    }),
            )
            .collect(),
            Menu::Tabs => self
                .tabs
                .iter()
                .map(|tab| MenuItem {
                    label: self.workspace_tab_title(tab).into(),
                    shortcut: "",
                    action: MenuAction::ActivateTab(tab.id),
                })
                .collect(),
            _ => menu.items().to_vec(),
        }
    }

    pub(super) fn edit_group(
        &mut self,
        editing: Option<Uuid>,
        parent: Option<Uuid>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.closing
            || self.lock_screen.locked
            || self.modal.is_some()
            || self.window_state.prompts.is_busy()
        {
            return;
        }
        let group = editing.and_then(|id| self.config.groups.iter().find(|g| g.id == id));
        let parent = group.map_or(parent, |g| g.parent);
        let name = group.map(|g| g.name.clone()).unwrap_or_default();
        self.remember_popup_focus(window, cx);
        let fields = Fields::new(vec![(i18n_key::WORKSPACE_GROUP_NAME, name)], cx);
        fields.focus(window, cx);
        self.modal = Some(Modal::Group {
            editing,
            parent,
            fields,
        });
        if let Some(parent) = parent {
            self.expand_library_group(parent);
        }
        cx.notify();
    }

    pub(super) fn open_group_menu(
        &mut self,
        id: Uuid,
        position: gpui::Point<Pixels>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.closing
            || self.lock_screen.locked
            || self.modal.is_some()
            || self.window_state.prompts.is_busy()
            || !self.config.groups.iter().any(|g| g.id == id)
        {
            return;
        }
        if self.menu.take().is_some() {
            self.restore_popup_focus(window, cx);
        }
        self.release_rdp_control(cx);
        self.library_selection = Some(LibraryItem::Group(Some(id)));
        self.focus_library(window, cx);
        self.remember_popup_focus(window, cx);
        self.menu = Some(Menu::Group(id));
        self.menu_anchor = position;
        self.menu_index = 0;
        self.menu_scroll.scroll_to_item(0);
        self.menu_focus.focus(window);
        cx.notify();
    }

    pub(super) fn application_menu(&self) -> Option<Menu> {
        self.menu.and_then(Menu::category)
    }

    pub(super) fn record_application_menu_bounds(
        &mut self,
        menu: Menu,
        bounds: Bounds<Pixels>,
        cx: &mut Context<Self>,
    ) {
        let Some(index) = menu.category_index() else {
            return;
        };
        if self.application_menu_bounds[index] != Some(bounds) {
            self.application_menu_bounds[index] = Some(bounds);
            if self.application_menu() == menu.category() {
                cx.notify();
            }
        }
    }

    pub(super) fn switch_application_menu(
        &mut self,
        menu: Menu,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.application_menu().is_some()
            && menu.category().is_some()
            && self.application_menu() != menu.category()
        {
            self.toggle_menu(menu, window, cx);
        }
    }

    pub(super) fn return_to_parent_menu(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.menu != Some(Menu::NewConnection) {
            return;
        }
        self.menu = Some(Menu::File);
        self.menu_index = Menu::File
            .items()
            .iter()
            .position(|item| item.action == MenuAction::NewConnection)
            .expect("File contains the new connection submenu");
        self.menu_scroll.scroll_to_item(self.menu_index);
        self.menu_focus.focus(window);
        cx.notify();
    }

    pub(super) fn open_profile_menu(
        &mut self,
        id: Uuid,
        position: gpui::Point<Pixels>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.closing
            || self.lock_screen.locked
            || self.modal.is_some()
            || self.window_state.prompts.is_busy()
            || !self.config.profiles.iter().any(|profile| profile.id == id)
        {
            return;
        }
        if self.menu.take().is_some() {
            self.restore_popup_focus(window, cx);
        }
        self.library_selection = Some(LibraryItem::Profile(id));
        self.release_rdp_control(cx);
        self.focus_library(window, cx);
        self.remember_popup_focus(window, cx);
        self.menu = Some(Menu::Profile(id));
        self.menu_anchor = position;
        self.menu_index = 0;
        self.menu_scroll.scroll_to_item(0);
        self.menu_focus.focus(window);
        cx.notify();
    }
    pub(super) fn toggle_menu(&mut self, menu: Menu, window: &mut Window, cx: &mut Context<Self>) {
        if self.closing
            || self.lock_screen.locked
            || self.modal.is_some()
            || self.window_state.prompts.is_busy()
        {
            return;
        }
        if self.menu == Some(menu)
            || (menu.category().is_some() && self.application_menu() == menu.category())
        {
            self.menu = None;
            self.restore_popup_focus(window, cx);
        } else {
            if self.menu.is_none() {
                self.remember_popup_focus(window, cx);
            }
            self.release_rdp_control(cx);
            if menu.category().is_some() {
                if self.application_menu().is_none() {
                    self.application_menu_bounds = [None; APPLICATION_MENUS.len()];
                }
            } else {
                let title_bottom = crate::app::window_frame::WindowFrame::new(window)
                    .content_bounds(window.viewport_size())
                    .top()
                    + px(ui::TITLE_HEIGHT);
                self.menu_anchor = gpui::point(
                    window.mouse_position().x,
                    if matches!(menu, Menu::Create) || window.mouse_position().y > title_bottom {
                        window.mouse_position().y + px(16.)
                    } else {
                        title_bottom
                    },
                );
            }
            self.menu = Some(menu);
            self.menu_index = self
                .menu_items(menu)
                .iter()
                .position(|item| self.action_enabled(item.action, cx))
                .unwrap_or(0);
            self.menu_scroll.scroll_to_item(0);
            self.menu_focus.focus(window);
        }
        cx.notify();
    }
    pub(super) fn menu_key(
        &mut self,
        event: &KeyDownEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        let Some(menu) = self.menu else { return false };
        if !self.menu_focus.contains_focused(window, cx) {
            return false;
        }
        let modifiers = event.keystroke.modifiers;
        if modifiers.control || modifiers.alt || modifiers.platform {
            return false;
        }
        let items = self.menu_items(menu);
        self.menu_index = self.menu_index.min(items.len().saturating_sub(1));
        let enabled: Vec<_> = items
            .iter()
            .map(|item| self.action_enabled(item.action, cx))
            .collect();
        match event.keystroke.key.as_str() {
            "escape" | "left" if menu == Menu::NewConnection => {
                self.return_to_parent_menu(window, cx);
            }
            "escape" => {
                self.menu = None;
                self.restore_popup_focus(window, cx);
            }
            "up" | "down" | "tab" | "home" | "end" => {
                if let Some(index) = ui::popup::next_index(event, self.menu_index, &enabled) {
                    self.menu_index = index;
                }
            }
            "right"
                if items.get(self.menu_index).map(|item| item.action)
                    == Some(MenuAction::NewConnection) =>
            {
                self.menu_action(MenuAction::NewConnection, window, cx);
            }
            "left" | "right" => {
                if let Some(index) = menu.category_index() {
                    let count = APPLICATION_MENUS.len();
                    let next = if event.keystroke.key == "left" {
                        (index + count - 1) % count
                    } else {
                        (index + 1) % count
                    };
                    self.switch_application_menu(APPLICATION_MENUS[next].0, window, cx);
                }
            }
            "f2" if matches!(menu, Menu::Profile(_)) => {
                self.menu_action(MenuAction::EditProfile, window, cx)
            }
            "f2" if matches!(menu, Menu::Group(_)) => {
                self.menu_action(MenuAction::EditGroup, window, cx)
            }
            "delete" if matches!(menu, Menu::Group(_)) => {
                self.menu_action(MenuAction::DeleteGroup, window, cx)
            }
            "delete" if matches!(menu, Menu::Profile(_)) => {
                self.menu_action(MenuAction::DeleteProfile, window, cx)
            }
            "enter" | "return" | "space" => {
                if let Some(item) = items.get(self.menu_index) {
                    self.menu_action(item.action, window, cx);
                }
            }
            _ => {}
        }
        if let Some(menu) = self.menu {
            if menu == Menu::NewConnection {
                self.submenu_scroll.scroll_to_item(self.menu_index);
            } else {
                self.menu_scroll.scroll_to_item(self.menu_index);
            }
        }
        cx.stop_propagation();
        window.prevent_default();
        cx.notify();
        true
    }
    pub(super) fn action_enabled(&self, action: MenuAction, cx: &App) -> bool {
        if self.lock_screen.locked {
            return false;
        }
        use MenuAction::*;
        if action.is_tab_action() {
            return self.tab_action_enabled(action, cx);
        }
        if matches!(action, NewSubgroup | EditGroup | DeleteGroup | GroupToRoot) {
            return self
                .menu
                .and_then(|menu| match menu {
                    Menu::Group(id) => Some(id),
                    _ => None,
                })
                .and_then(|id| self.config.groups.iter().find(|g| g.id == id))
                .is_some_and(|g| action != GroupToRoot || g.parent.is_some());
        }
        if matches!(action, NewConnection) {
            return self.application_menu() == Some(Menu::File);
        }
        if matches!(
            action,
            EditProfile | OpenProfile | OpenProfileRight | OpenProfileBelow | DeleteProfile
        ) {
            return self
                .menu
                .and_then(|menu| menu.profile(&self.config.profiles))
                .is_some()
                && (!matches!(action, OpenProfileRight | OpenProfileBelow)
                    || (!self.utility_overlay && self.panes.contains_key(&self.focused)));
        }
        // Utility overlays cover pane content; workspace navigation stays available.
        if self.utility_overlay
            && matches!(
                action,
                Copy | Paste
                    | Search
                    | SplitHorizontal
                    | SplitVertical
                    | Zoom
                    | LineNumbers
                    | LineTimestamps
                    | Highlight
                    | Disconnect
                    | Close
            )
        {
            return false;
        }
        if matches!(action, Files) {
            return self.files.contains_key(&self.focused)
                || self.panes.get(&self.focused).is_some_and(
                    |p| matches!(p, Pane::Terminal(view) if view.read(cx).sftp_client().is_some()),
                );
        }
        if matches!(action, Forwards) {
            return true;
        }
        if matches!(action, Copy | Paste) {
            return match self.panes.get(&self.focused) {
                Some(Pane::Terminal(view)) => {
                    let view = view.read(cx);
                    view.history_actions_allowed(cx)
                        && (matches!(action, Copy)
                            || view
                                .history_run()
                                .is_some_and(|run| view.can_insert_history_command(run)))
                }
                Some(Pane::Database(_) | Pane::Elasticsearch(_)) => true,
                _ => false,
            };
        }
        if matches!(action, LineNumbers | LineTimestamps | Highlight) {
            return matches!(self.panes.get(&self.focused), Some(Pane::Terminal(_)));
        }
        if matches!(action, Search) && matches!(self.panes.get(&self.focused), Some(Pane::Rdp(_))) {
            return false;
        }
        if matches!(action, Zoom) {
            return self.zoomed.is_some()
                || self
                    .tabs
                    .get(self.active)
                    .is_some_and(|tab| tab.layout.leaves().len() > 1);
        }
        !matches!(
            action,
            Copy | Paste
                | Search
                | SplitHorizontal
                | SplitVertical
                | NextPane
                | Disconnect
                | Close
                | Zoom
        ) || self.panes.contains_key(&self.focused)
    }
    pub(super) fn menu_action(
        &mut self,
        action: MenuAction,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.closing
            || self.lock_screen.locked
            || self.modal.is_some()
            || self.window_state.prompts.is_busy()
            || !self.action_enabled(action, cx)
        {
            cx.notify();
            return;
        }
        if matches!(action, MenuAction::NewConnection) {
            if self.menu != Some(Menu::NewConnection) {
                self.menu = Some(Menu::NewConnection);
                self.menu_index = 0;
                self.submenu_scroll.scroll_to_item(0);
                self.menu_focus.focus(window);
                cx.notify();
            }
            return;
        }
        if action.is_tab_action() {
            self.tab_menu_action(action, window, cx);
            return;
        }
        let menu = self.menu.take();
        let group = menu.and_then(|menu| match menu {
            Menu::Group(id) => Some(id),
            _ => None,
        });
        let profile = menu
            .and_then(|menu| menu.profile(&self.config.profiles))
            .cloned();
        self.restore_popup_focus(window, cx);
        use MenuAction::*;
        if matches!(
            action,
            Copy | Paste
                | Search
                | Disconnect
                | SplitHorizontal
                | SplitVertical
                | NextPane
                | NextTab
                | Close
                | Zoom
        ) {
            self.restore_focus(window, cx);
        }
        match action {
            FilterType(kind) => {
                self.connection_type = kind;
                self.library_selection = None;
                self.sidebar_scroll.scroll_to_item(0);
                self.session_filter.read(cx).focus_handle().focus(window);
                cx.notify();
            }
            ActivateTab(id) => {
                if let Some(index) = self.tabs.iter().position(|tab| tab.id == id) {
                    self.set_active(index, window, cx);
                }
            }
            NewTabSession | CloseTab | CloseOtherTabs | CloseTabsToRight | CloseTabGroup => {
                unreachable!("tab actions are dispatched with their explicit context")
            }
            NewConnection => unreachable!("submenu navigation is handled before command dispatch"),
            Palette => self.toggle_palette(window, cx),
            Forwards => self.open_tunnels(window, cx),
            EditProfile | OpenProfile | OpenProfileRight | OpenProfileBelow | DeleteProfile => {
                let Some(profile) = profile else { return };
                match action {
                    EditProfile => {
                        let kind = match &profile.connection {
                            Connection::Local(_) => ProfileKind::Local,
                            Connection::Ssh(_) => ProfileKind::Ssh,
                            Connection::Telnet(_) => ProfileKind::Telnet,
                            Connection::Redis(_) => ProfileKind::Redis,
                            Connection::Elasticsearch(_) => ProfileKind::Elasticsearch,
                            Connection::Mysql(_) => ProfileKind::Mysql,
                            Connection::ClickHouse(_) => ProfileKind::ClickHouse,
                            Connection::Postgres(_) => ProfileKind::Postgres,
                            Connection::Sqlite(_) => ProfileKind::Sqlite,
                            Connection::Rdp(_) => ProfileKind::Rdp,
                        };
                        self.edit_profile(Some(profile), kind, window, cx);
                    }
                    OpenProfile => self.open_or_focus_profile(profile, window, cx),
                    OpenProfileRight => {
                        self.connect_profile(profile, Some(Axis::Horizontal), window, cx)
                    }
                    OpenProfileBelow => {
                        self.connect_profile(profile, Some(Axis::Vertical), window, cx)
                    }
                    DeleteProfile => self.delete_profile(profile.id, window, cx),
                    _ => unreachable!(),
                }
            }
            NewTerminal => self.pane_event(&PaneEvent::NewTab, window, cx),
            NewLocal => self.edit_profile(None, ProfileKind::Local, window, cx),
            NewSsh => self.edit_profile(None, ProfileKind::Ssh, window, cx),
            NewTelnet => self.edit_profile(None, ProfileKind::Telnet, window, cx),
            NewElasticsearch => self.edit_profile(None, ProfileKind::Elasticsearch, window, cx),
            NewRedis => self.edit_profile(None, ProfileKind::Redis, window, cx),
            NewMysql => self.edit_profile(None, ProfileKind::Mysql, window, cx),
            NewClickHouse => self.edit_profile(None, ProfileKind::ClickHouse, window, cx),
            NewPostgres => self.edit_profile(None, ProfileKind::Postgres, window, cx),
            NewSqlite => self.edit_profile(None, ProfileKind::Sqlite, window, cx),
            NewRdp => self.edit_profile(None, ProfileKind::Rdp, window, cx),
            NewGroup => self.edit_group(None, None, window, cx),
            NewSubgroup => self.edit_group(None, group, window, cx),
            EditGroup => self.edit_group(group, None, window, cx),
            DeleteGroup => {
                if let Some(id) = group {
                    self.delete_group(id, window, cx);
                }
            }
            GroupToRoot => {
                if let Some(id) = group
                    && self.config.move_group(id, None, None).is_ok()
                {
                    self.save(cx);
                }
            }
            Settings => self.open_settings(SettingsSection::General, window, cx),
            Updates => self.open_settings(SettingsSection::Updates, window, cx),
            Lock => self.lock_workspace(window, cx),
            Quit => {
                self.request_exit(window, cx);
            }
            Files => self.open_files(window, cx),
            Copy | Paste | Search | Disconnect => match self.panes.get(&self.focused) {
                Some(Pane::Terminal(pane)) => pane.update(cx, |p, cx| match action {
                    Copy => p.copy_selection(cx),
                    Paste => p.paste(window, cx),
                    Search => p.open_search(window, cx),
                    _ => p.disconnect_or_reconnect(cx),
                }),
                Some(Pane::Elasticsearch(pane)) => pane.update(cx, |p, cx| match action {
                    Copy => p.copy_json(cx),
                    Paste => p.paste_query(window, cx),
                    Search => p.focus_filter(window, cx),
                    _ => p.disconnect_or_reconnect(cx),
                }),
                Some(Pane::Redis(pane)) => pane.update(cx, |p, cx| match action {
                    Search => p.open_search(window, cx),
                    Disconnect => p.disconnect_or_reconnect(cx),
                    _ => {}
                }),
                Some(Pane::Database(pane)) => pane.update(cx, |p, cx| match action {
                    Copy => p.copy_selection(window, cx),
                    Paste => p.paste_sql(window, cx),
                    Search => p.open_search(window, cx),
                    _ => p.disconnect_or_reconnect(window, cx),
                }),
                Some(Pane::Rdp(pane)) => {
                    if matches!(action, Disconnect) {
                        pane.update(cx, |p, cx| p.disconnect_or_reconnect(cx));
                    }
                }
                None => {}
            },
            Filter => {
                if !self.sidebar_visible {
                    self.toggle_sidebar(window, cx);
                }
                self.session_filter.read(cx).focus(window);
            }
            Sidebar => self.toggle_sidebar(window, cx),
            Zoom => self.toggle_zoom(window, cx),
            SplitHorizontal => self.split(Axis::Horizontal, window, cx),
            SplitVertical => self.split(Axis::Vertical, window, cx),
            NextPane => self.pane_event(&PaneEvent::NextPane, window, cx),
            NextTab => self.pane_event(&PaneEvent::NextTab, window, cx),
            LineNumbers | LineTimestamps | Highlight => {
                if matches!(action, LineNumbers) {
                    self.config.settings.line_numbers = !self.config.settings.line_numbers;
                } else if matches!(action, LineTimestamps) {
                    self.config.settings.line_timestamps = !self.config.settings.line_timestamps;
                } else {
                    self.config.settings.highlight = !self.config.settings.highlight;
                }
                for pane in self.panes.values() {
                    pane.apply_settings(&self.config.settings, cx);
                }
                self.save(cx);
            }
            Close => self.confirm_close(vec![self.focused], window, cx),
            Trust => self.open_trust(window, cx),
            Help => {
                self.remember_popup_focus(window, cx);
                self.modal = Some(Modal::Help);
            }
        }
        cx.notify();
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    use rustterm::config::LocalConfig;

    #[test]
    fn profile_menu_keeps_its_target_across_reordering_and_never_substitutes_another_profile() {
        let target = Profile::local("target", LocalConfig::default());
        let other = Profile::local("other", LocalConfig::default());
        let menu = Menu::Profile(target.id);
        let mut profiles = vec![target.clone(), other];
        profiles.reverse();
        assert_eq!(menu.profile(&profiles).unwrap().id, target.id);

        profiles
            .iter_mut()
            .find(|profile| profile.id == target.id)
            .unwrap()
            .name = "updated".into();
        assert_eq!(menu.profile(&profiles).unwrap().name, "updated");

        profiles.retain(|profile| profile.id != target.id);
        assert!(menu.profile(&profiles).is_none());
        assert!(Menu::File.profile(&profiles).is_none());
    }
}
