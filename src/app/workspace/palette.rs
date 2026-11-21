mod history;
use super::Workspace;
use super::connection_kind::{ConnectionKind, target};
use crate::app::forms::SettingsSection;
use crate::app::pane::{Pane, PaneEvent};
use crate::app::text_input::{InputEvent, TextInput};
use crate::app::ui;
use crate::app::window_frame::WindowFrame;
use gpui::prelude::*;
use gpui::{
    AnyElement, App, Context, Entity, FocusHandle, KeyDownEvent, MouseButton, SharedString,
    Subscription, Window, div, px, rgb, rgba,
};
use history::{HistoryAction, HistoryRecord, history_items};
use rustterm::config::{
    AppConfig, Connection, Group, QuickCommand, QuickOpenOrder, ShortcutAction, Uuid,
};
use rustterm::i18n::messages as i18n_key;
use rustterm::i18n::{Message, tr};

use std::collections::HashMap;
use std::rc::Rc;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum PaletteScope {
    Commands,
    Connections,
}

impl PaletteScope {
    fn placeholder(self) -> &'static str {
        match self {
            Self::Commands => i18n_key::WORKSPACE_SEARCH_CONNECTIONS_COMMANDS_AND_ACTIONS,
            Self::Connections => i18n_key::WORKSPACE_FILTER_BY_NAME_HOST_OR_GROUP,
        }
    }
}

fn is_reconnect_shortcut(key: &gpui::Keystroke) -> bool {
    key.key.eq_ignore_ascii_case("r")
        && key.modifiers.control
        && key.modifiers.shift
        && !key.modifiers.alt
        && !key.modifiers.platform
}

pub(super) struct Palette {
    pub open: bool,
    scope: PaletteScope,
    query: Entity<TextInput>,
    quick_open: QuickOpenResults,
    displayed_commands: Option<(String, Rc<[Item]>)>,
    name: Entity<TextInput>,
    command: Entity<TextInput>,
    editing: Option<Option<Uuid>>,
    selected: usize,
    scroll: gpui::ScrollHandle,
    focus: FocusHandle,
    _subscriptions: Vec<Subscription>,
}
struct Item {
    label: SharedString,
    detail: SharedString,
    action: Action,
}

impl Item {
    fn history_record(&self) -> Option<HistoryRecord> {
        match self.action {
            Action::History(record) => Some(record),
            _ => None,
        }
    }
}

#[derive(Default)]
struct QuickOpenResults {
    cached: Option<(String, Rc<[Item]>)>,
}

impl QuickOpenResults {
    fn items(&mut self, config: &AppConfig, query: &str) -> Rc<[Item]> {
        if let Some((previous, items)) = &self.cached
            && previous == query
        {
            return Rc::clone(items);
        }
        let items = Rc::from(connection_items(config, query, PaletteScope::Connections));
        self.cached = Some((query.to_owned(), Rc::clone(&items)));
        items
    }

    fn invalidate(&mut self) {
        self.cached = None;
    }
}

fn connection_items(config: &AppConfig, query: &str, scope: PaletteScope) -> Vec<Item> {
    let groups: HashMap<_, _> = Group::tree(&config.groups)
        .into_iter()
        .enumerate()
        .map(|(index, node)| (node.group.id, (index + 1, node.path)))
        .collect();
    let query = query.trim().to_lowercase();
    let words: Vec<_> = query.split_whitespace().collect();
    let initial_list = scope == PaletteScope::Connections && query.is_empty();
    let recent: HashMap<_, _> =
        if initial_list && config.settings.quick_open_order == QuickOpenOrder::RecentlyUsed {
            config
                .recent_connections
                .ids()
                .iter()
                .copied()
                .enumerate()
                .map(|(index, id)| (id, index))
                .collect()
        } else {
            HashMap::new()
        };
    let mut matches = Vec::new();
    for (index, profile) in config.profiles.iter().enumerate() {
        let kind = ConnectionKind::of(&profile.connection).label();
        let address = target(&profile.connection);
        let mut detail = match &profile.connection {
            Connection::Ssh(c) if !c.username.is_empty() => {
                format!("{kind} · {}@{address}", c.username)
            }
            Connection::Mysql(c) | Connection::Postgres(c) | Connection::ClickHouse(c)
                if !c.database.is_empty() =>
            {
                format!("{kind} · {address} / {}", c.database)
            }
            _ => format!("{kind} · {address}"),
        };
        let group = profile.group.and_then(|id| groups.get(&id));
        if let Some((_, path)) = group {
            detail.push_str(" · ");
            detail.push_str(path);
        }
        let name = profile.name.to_lowercase();
        let rank = if name == query {
            0
        } else if name.starts_with(&query) {
            1
        } else if name.contains(&query) {
            2
        } else {
            let fields = detail.to_lowercase();
            if !words
                .iter()
                .all(|word| name.contains(word) || fields.contains(word))
            {
                continue;
            }
            3
        };
        matches.push((
            (
                rank,
                recent.get(&profile.id).copied().unwrap_or(usize::MAX),
                if initial_list {
                    group.map_or(0, |(index, _)| *index)
                } else {
                    0
                },
            ),
            index,
            profile,
            detail,
        ));
    }
    matches.sort_by_key(|(rank, ..)| *rank);
    matches
        .into_iter()
        .take(128)
        .map(|(_, index, profile, detail)| Item {
            label: profile.name.clone().into(),
            detail: detail.into(),
            action: Action::Connect {
                index,
                id: profile.id,
            },
        })
        .collect()
}
#[derive(Clone)]
enum Action {
    Connect { index: usize, id: Uuid },
    QuickOpen,
    PasteSaved { run: Uuid, text: String },
    OpenHistory,
    History(HistoryRecord),
    Edit(Uuid),
    NewTerminal,
    Settings,
    Lock,
    Files,
    SyncFiles,
    Forwards,
    AddCommand,
}
impl Palette {
    pub(super) fn hide(&mut self) {
        self.open = false;
        self.displayed_commands = None;
        self.invalidate_connections();
    }

    pub(super) fn invalidate_connections(&mut self) {
        self.quick_open.invalidate();
    }

    fn is_composing(&self, cx: &App) -> bool {
        self.open
            && if self.editing.is_some() {
                self.name.read(cx).is_composing() || self.command.read(cx).is_composing()
            } else {
                self.query.read(cx).is_composing()
            }
    }

    pub fn new(window: &mut Window, cx: &mut Context<Workspace>) -> Self {
        let query = cx.new(|cx| {
            TextInput::new(
                i18n_key::WORKSPACE_SEARCH_CONNECTIONS_COMMANDS_AND_ACTIONS,
                false,
                cx,
            )
        });
        let name = cx.new(|cx| TextInput::new(i18n_key::WORKSPACE_COMMAND_NAME, false, cx));
        let command = cx.new(|cx| {
            let mut input = TextInput::new(i18n_key::WORKSPACE_COMMAND_TEXT, false, cx);
            input.set_literal(true, cx);
            input
        });
        let subscriptions = vec![
            cx.subscribe_in(&query, window, |this, _, event, window, cx| {
                if !this.palette.open || this.lock_screen.locked {
                    return;
                }
                match event {
                    InputEvent::Changed => {
                        this.palette.selected = 0;
                        this.palette.scroll.scroll_to_item(0);
                        cx.notify();
                    }
                    InputEvent::Submit => this.run_palette_selection(window, cx),
                    InputEvent::Rejected(message) => {
                        this.error = Some((*message).into());
                        cx.notify();
                    }
                }
            }),
            cx.subscribe_in(&command, window, |this, _, event, window, cx| {
                if matches!(event, InputEvent::Submit) {
                    this.save_quick_command(window, cx);
                }
            }),
        ];
        Self {
            open: false,
            scope: PaletteScope::Commands,
            query,
            quick_open: QuickOpenResults::default(),
            displayed_commands: None,
            name,
            command,
            editing: None,
            selected: 0,
            scroll: gpui::ScrollHandle::new(),
            focus: cx.focus_handle(),
            _subscriptions: subscriptions,
        }
    }
}
impl Workspace {
    pub(super) fn toggle_palette(
        &mut self,
        scope: PaletteScope,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.lock_screen.locked
            || self.closing
            || self.modal.is_some()
            || self.authentication.read(cx).is_active()
            || self.window_state.prompts.is_busy()
        {
            return;
        }
        let switching = self.palette.open;
        if switching {
            if self.palette.scope == scope {
                self.close_palette(window, cx);
                return;
            }
            if self.palette.editing.is_some() {
                return;
            }
        }
        self.palette.open = true;
        self.palette.displayed_commands = None;
        self.palette.invalidate_connections();
        self.palette.scope = scope;
        self.palette.editing = None;
        self.palette.query.update(cx, |input, cx| {
            input.set_placeholder(scope.placeholder(), cx);
            if !switching {
                input.clear(cx);
            }
        });
        self.palette.selected = 0;
        self.palette.scroll.scroll_to_item(0);
        self.menu = None;
        self.palette.query.read(cx).focus(window);
        cx.notify();
    }
    fn close_palette(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.palette.hide();
        self.palette.editing = None;
        self.palette.command.update(cx, |input, cx| input.clear(cx));
        self.restore_focus(window, cx);
        cx.notify();
    }
    pub(super) fn focus_palette(&self, window: &mut Window, cx: &App) {
        if self.palette.editing.is_some() {
            self.palette.name.read(cx).focus(window);
        } else {
            self.palette.query.read(cx).focus(window);
        }
    }
    pub(super) fn workspace_shortcuts(
        &mut self,
        event: &KeyDownEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if !self.lock_screen.locked && self.rdp_captures_keyboard(cx) {
            self.note_activity();
            return;
        }
        if let Some(super::Modal::Settings(form)) = &self.modal
            && !self.lock_screen.locked
            && !self.modal_saving
            && !self.window_state.prompts.is_busy()
            && form.update(cx, |form, cx| form.record_shortcut(event, window, cx))
        {
            self.note_activity();
            window.prevent_default();
            cx.stop_propagation();
            return;
        }
        self.lock_key(event, window, cx);
        if self.lock_screen.locked {
            return;
        }
        if self.authentication.read(cx).is_active() {
            if !self.authentication.read(cx).owns_focus(window, cx) {
                self.authentication.read(cx).focus(window, cx);
                cx.stop_propagation();
            }
            return;
        }
        let modifiers = event.keystroke.modifiers;
        if event.keystroke.key == ","
            && modifiers.control
            && !modifiers.shift
            && !modifiers.alt
            && !modifiers.platform
        {
            self.open_settings(SettingsSection::General, window, cx);
            window.prevent_default();
            cx.stop_propagation();
        } else if let Some(action) = self.config.settings.shortcuts.action(&event.keystroke)
            && self.modal.is_none()
            && !self.window_state.prompts.is_busy()
            && !self.input_composing(window, cx)
            && !self.palette.is_composing(cx)
        {
            use super::menus::MenuAction;
            let command = match action {
                ShortcutAction::QuickOpen => MenuAction::QuickOpen,
                ShortcutAction::Palette => MenuAction::Palette,
                ShortcutAction::Sidebar if !self.palette.open && self.menu.is_none() => {
                    MenuAction::Sidebar
                }
                ShortcutAction::Search if !self.palette.open && self.menu.is_none() => {
                    let Some(pane) = self.panes.get(&self.focused) else {
                        return;
                    };
                    // The terminal handles search after its paste, menu and IME checks.
                    if matches!(pane, Pane::Terminal(_) | Pane::Rdp(_))
                        || !pane.focus_handle(cx).contains_focused(window, cx)
                        || !self.action_enabled(MenuAction::Search, cx)
                    {
                        return;
                    }
                    MenuAction::Search
                }
                _ => return,
            };
            if !event.is_held {
                self.menu_action(command, window, cx);
            }
            window.prevent_default();
            cx.stop_propagation();
        } else if is_reconnect_shortcut(&event.keystroke)
            && !self.palette.open
            && self.modal.is_none()
            && !self.utility_overlay
            && let Some(Pane::Terminal(view)) = self.panes.get(&self.focused)
        {
            // Capture before terminal input so the application shortcut never
            // degrades into Ctrl+R in the remote shell, even while connected.
            if !event.is_held
                && window.is_window_active()
                && !self.closing
                && !self.modal_saving
                && !self.input_composing(window, cx)
                && !self.window_state.prompts.is_busy()
                && !self.window_state.prompts.is_blocked()
                && view.update(cx, |view, cx| view.reconnect_ended(window, cx))
            {
                self.menu = None;
                cx.notify();
            }
            window.prevent_default();
            cx.stop_propagation();
        } else if self.palette.open && !self.palette.focus.contains_focused(window, cx) {
            self.focus_palette(window, cx);
            window.prevent_default();
            cx.stop_propagation();
        }
    }
    fn palette_items(&mut self, cx: &App) -> Rc<[Item]> {
        let query = self.palette.query.read(cx).value();
        if self.palette.scope == PaletteScope::Connections {
            let items = self.palette.quick_open.items(&self.config, query);
            self.palette.selected = self.palette.selected.min(items.len().saturating_sub(1));
            return items;
        }
        let connections = connection_items(&self.config, query, PaletteScope::Commands);
        let query = query.trim().to_lowercase();
        let mut items = vec![
            (
                i18n_key::WORKSPACE_NEW_LOCAL_TERMINAL,
                "Ctrl+Shift+T",
                Action::NewTerminal,
            ),
            (
                i18n_key::WORKSPACE_QUICK_OPEN_CONNECTION,
                self.config
                    .settings
                    .shortcuts
                    .label(ShortcutAction::QuickOpen),
                Action::QuickOpen,
            ),
            (
                i18n_key::WORKSPACE_ADD_SAVED_COMMAND,
                "",
                Action::AddCommand,
            ),
            (i18n_key::WORKSPACE_COMMAND_HISTORY, "", Action::OpenHistory),
            (i18n_key::SFTP_FILE_MANAGER_DIALOG, "", Action::Files),
            (
                if matches!(self.panes.get(&self.focused), Some(Pane::Terminal(terminal))
                    if terminal.read(cx).followed_sftp_directory().is_some())
                {
                    i18n_key::WORKSPACE_STOP_FOLLOWING_TERMINAL_DIRECTORY
                } else {
                    i18n_key::WORKSPACE_FOLLOW_TERMINAL_DIRECTORY_IN_SFTP
                },
                "SFTP",
                Action::SyncFiles,
            ),
            (
                i18n_key::CONNECTIONS_MANAGE_PORT_FORWARDS,
                "",
                Action::Forwards,
            ),
            (i18n_key::COMMON_SETTINGS_DIALOG, "Ctrl+,", Action::Settings),
            (
                i18n_key::SETTINGS_LOCK_RUSTTERM,
                "Ctrl+Shift+L",
                Action::Lock,
            ),
        ]
        .into_iter()
        .map(|(label, detail, action)| Item {
            label: tr(label).into(),
            detail: detail.to_owned().into(),
            action,
        })
        .collect::<Vec<_>>();
        let terminal_run = match self.panes.get(&self.focused) {
            Some(Pane::Terminal(terminal)) => terminal.read(cx).history_run(),
            _ => None,
        };
        for command in &self.config.quick_commands {
            if let Some(run) = terminal_run {
                items.push(Item {
                    label: rustterm::trf!(
                        i18n_key::WORKSPACE_PASTE_SAVED_COMMAND_NAME,
                        name = command.name
                    )
                    .into(),
                    detail: tr(i18n_key::WORKSPACE_SAVED_COMMAND).into(),
                    action: Action::PasteSaved {
                        run,
                        text: command.command.clone(),
                    },
                });
            }
            items.push(Item {
                label: rustterm::trf!(
                    i18n_key::WORKSPACE_EDIT_SAVED_COMMAND_NAME,
                    name = command.name
                )
                .into(),
                detail: SharedString::default(),
                action: Action::Edit(command.id),
            });
        }
        let actions = items
            .into_iter()
            .filter(|item| self.palette_action_available(&item.action, cx))
            .filter(|item| {
                format!("{} {}", item.label, item.detail)
                    .to_lowercase()
                    .contains(&query)
            });
        let history = match self.panes.get(&self.focused) {
            Some(Pane::Terminal(view))
                if !query.is_empty()
                    && let Some(run) = terminal_run =>
            {
                let terminal = view.read(cx);
                if terminal.history_actions_allowed(cx) {
                    history_items(
                        run,
                        &terminal.command_records(),
                        &query,
                        !self.utility_overlay && terminal.can_insert_history_command(run),
                    )
                } else {
                    Vec::new()
                }
            }
            _ => Vec::new(),
        };
        actions
            .chain(history)
            .chain(connections)
            .take(128)
            .collect()
    }

    fn palette_action_available(&self, action: &Action, cx: &App) -> bool {
        match action {
            Action::Files
            | Action::SyncFiles
            | Action::Forwards
            | Action::PasteSaved { .. }
            | Action::OpenHistory => {
                let Some(Pane::Terminal(terminal)) = self.panes.get(&self.focused) else {
                    return false;
                };
                let terminal = terminal.read(cx);
                match action {
                    Action::Files => {
                        self.files.contains_key(&self.focused) || terminal.sftp_client().is_some()
                    }
                    Action::SyncFiles => terminal.sftp_directory().is_some(),
                    Action::Forwards => terminal.forwarding().is_some(),
                    Action::PasteSaved { run, .. } => {
                        !self.utility_overlay
                            && terminal.history_actions_allowed(cx)
                            && terminal.can_insert_history_command(*run)
                    }
                    Action::OpenHistory => terminal.history_actions_allowed(cx),
                    _ => false,
                }
            }
            _ => true,
        }
    }
    fn run_palette_selection(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if !self.palette.open
            || self.palette.editing.is_some()
            || self.lock_screen.locked
            || self.palette.query.read(cx).is_composing()
        {
            return;
        }
        let items = self.palette_navigation_items(cx);
        let Some(item) = items.get(self.palette.selected) else {
            return;
        };
        self.run_palette_action(item.action.clone(), window, cx);
    }

    fn palette_navigation_items(&mut self, cx: &App) -> Rc<[Item]> {
        if let Some((query, displayed)) = &self.palette.displayed_commands
            && query == self.palette.query.read(cx).value()
        {
            return Rc::clone(displayed);
        }
        self.palette_items(cx)
    }
    fn edit_quick_command(
        &mut self,
        id: Option<Uuid>,
        text: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let existing = id.and_then(|id| {
            self.config
                .quick_commands
                .iter()
                .find(|command| command.id == id)
        });
        let name = existing
            .map_or("", |command| command.name.as_str())
            .to_owned();
        let text = existing.map_or(text, |command| command.command.clone());
        self.palette.editing = Some(id);
        self.palette
            .name
            .update(cx, |input, cx| input.set_value(name, cx));
        self.palette
            .command
            .update(cx, |input, cx| input.set_value(text, cx));
        self.palette.name.read(cx).focus(window);
        cx.notify();
    }
    fn save_quick_command(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if !self.palette.open
            || self.lock_screen.locked
            || self.palette.command.read(cx).is_composing()
            || self.palette.name.read(cx).is_composing()
        {
            return;
        }
        let Some(editing) = self.palette.editing else {
            return;
        };
        let name = self.palette.name.read(cx).value().trim().to_owned();
        let command = self.palette.command.read(cx).value().to_owned();
        if name.is_empty() || name.len() > 256 || command.is_empty() {
            self.error = Some(i18n_key::WORKSPACE_ENTER_A_COMMAND_NAME_AND_COMMAND_TEXT.into());
            cx.notify();
            return;
        }
        let mut candidate = self.config.clone();
        if let Some(id) = editing {
            if let Some(existing) = candidate
                .quick_commands
                .iter_mut()
                .find(|command| command.id == id)
            {
                existing.name = name;
                existing.command = command;
            }
        } else {
            if candidate.quick_commands.len() >= 256 {
                self.error = Some(i18n_key::WORKSPACE_TOO_MANY_SAVED_COMMANDS.into());
                cx.notify();
                return;
            }
            candidate.quick_commands.push(QuickCommand {
                id: Uuid::new_v4(),
                name,
                command,
            });
        }
        if let Err(error) = candidate.validate() {
            self.error = Some(Message::from_error(error));
            cx.notify();
            return;
        }
        self.config.quick_commands = candidate.quick_commands;
        self.save(cx);
        self.close_palette(window, cx);
    }
    fn run_palette_action(&mut self, action: Action, window: &mut Window, cx: &mut Context<Self>) {
        if self.lock_screen.locked || !self.palette_action_available(&action, cx) {
            return;
        }
        match action {
            Action::QuickOpen => {
                self.palette.query.update(cx, |input, cx| input.clear(cx));
                self.toggle_palette(PaletteScope::Connections, window, cx);
                return;
            }
            Action::AddCommand => {
                self.edit_quick_command(None, String::new(), window, cx);
                return;
            }
            Action::Edit(id) => {
                self.edit_quick_command(Some(id), String::new(), window, cx);
                return;
            }
            Action::History(record) => {
                self.run_history_action(record, HistoryAction::Insert, window, cx);
                return;
            }
            _ => self.close_palette(window, cx),
        }
        match action {
            Action::Connect { id, .. } => {
                if let Some(profile) = self
                    .config
                    .profiles
                    .iter()
                    .find(|profile| profile.id == id)
                    .cloned()
                {
                    self.open_or_focus_profile(profile, window, cx);
                }
            }
            Action::PasteSaved { run, text } => {
                if let Some(Pane::Terminal(terminal)) = self.panes.get(&self.focused) {
                    terminal.update(cx, |terminal, cx| {
                        if terminal.can_insert_history_command(run) {
                            terminal.send_text(&text, window, cx);
                        }
                    });
                }
            }
            Action::OpenHistory => self.open_commands(window, cx),
            Action::NewTerminal => self.pane_event(&PaneEvent::NewTab, window, cx),
            Action::Files => self.open_files(window, cx),
            Action::SyncFiles => self.follow_terminal_directory(window, cx),
            Action::Forwards => self.open_tunnels(window, cx),
            Action::Lock => self.lock_workspace(window, cx),
            Action::Settings => self.open_settings(SettingsSection::General, window, cx),
            _ => {}
        }
        cx.notify();
    }
    pub(super) fn open_tunnels(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.lock_screen.locked || self.modal.is_some() {
            return;
        }
        if self
            .pane_profiles
            .get(&self.focused)
            .is_some_and(|profile| {
                matches!(profile.connection, rustterm::config::Connection::Ssh(_))
            })
        {
            self.open_ports(window, cx);
        } else {
            self.open_all_ports(window, cx);
        }
    }
    pub(super) fn follow_terminal_directory(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(Pane::Terminal(terminal)) = self.panes.get(&self.focused) else {
            return;
        };
        let view = terminal.read(cx);
        if view.sftp_directory().is_none() {
            self.error = Some(
                if view.production_protected() {
                    i18n_key::CONNECTIONS_PRODUCTION_BLOCKED
                } else {
                    i18n_key::WORKSPACE_ENABLE_SHELL_INTEGRATION_TO_FOLLOW_THE_TERMINAL_DIRECTORY
                }
                .into(),
            );
            cx.notify();
            return;
        }
        let enable = view.followed_sftp_directory().is_none();
        terminal.update(cx, |terminal, _| terminal.set_sftp_follow(enable));
        if enable {
            self.open_files(window, cx);
        }
        cx.notify();
    }
    pub(super) fn render_palette(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        if !self.palette.focus.contains_focused(window, cx) {
            self.focus_palette(window, cx);
        }
        let mut card = div()
            .id("command-palette-card")
            .w(px(640.))
            .max_w_full()
            .max_h_full()
            .p_3()
            .flex()
            .flex_col()
            .gap_2()
            .bg(rgb(ui::colors().surface))
            .border_1()
            .border_color(rgb(ui::colors().border))
            .rounded_lg();
        if let Some(editing) = self.palette.editing {
            card = card
                .child(tr(i18n_key::WORKSPACE_SAVED_COMMAND))
                .child(self.palette.name.clone())
                .child(div().h(px(150.)).child(self.palette.command.clone()))
                .child(
                    div()
                        .flex()
                        .gap_2()
                        .justify_end()
                        .when_some(editing, |row, id| {
                            row.child(
                                ui::text_button(
                                    "delete-saved-command",
                                    i18n_key::COMMON_DELETE,
                                    ui::ButtonStyle::Danger,
                                )
                                .tab_index(0)
                                .on_click(cx.listener(
                                    move |this, _, window, cx| {
                                        this.config
                                            .quick_commands
                                            .retain(|command| command.id != id);
                                        this.save(cx);
                                        this.close_palette(window, cx);
                                    },
                                )),
                            )
                        })
                        .child(
                            ui::text_button(
                                "cancel-command-edit",
                                i18n_key::COMMON_CANCEL,
                                ui::ButtonStyle::Neutral,
                            )
                            .tab_index(0)
                            .on_click(
                                cx.listener(|this, _, window, cx| this.close_palette(window, cx)),
                            ),
                        )
                        .child(
                            ui::text_button(
                                "save-command",
                                i18n_key::COMMON_SAVE,
                                ui::ButtonStyle::Primary,
                            )
                            .tab_index(0)
                            .on_click(cx.listener(
                                |this, _, window, cx| this.save_quick_command(window, cx),
                            )),
                        ),
                );
        } else {
            let items = self.palette_items(cx);
            if self.palette.scope == PaletteScope::Commands {
                let query = self.palette.query.read(cx).value();
                if let Some((previous_query, displayed)) = &self.palette.displayed_commands
                    && previous_query == query
                    && let Some(previous) = displayed
                        .get(self.palette.selected)
                        .and_then(Item::history_record)
                    && let Some(index) = items.iter().position(|item| {
                        item.history_record()
                            .is_some_and(|record| previous.same_execution(record))
                    })
                    && index != self.palette.selected
                {
                    self.palette.selected = index;
                    self.palette.scroll.scroll_to_item(index);
                }
                self.palette.selected = self.palette.selected.min(items.len().saturating_sub(1));
                self.palette.displayed_commands = Some((query.to_owned(), Rc::clone(&items)));
            }
            let selected_hint = items.get(self.palette.selected).and_then(|item| {
                if let (PaletteScope::Connections, Action::Connect { index, id }) =
                    (self.palette.scope, &item.action)
                {
                    let profile = self
                        .config
                        .profiles
                        .get(*index)
                        .filter(|profile| profile.id == *id)?;
                    self.profile_open_action(profile).hint(&profile.connection)
                } else {
                    item.history_record()
                        .map(|record| tr(record.hint()).to_owned())
                }
            });
            let empty_connections =
                self.palette.scope == PaletteScope::Connections && items.is_empty();
            let query = div()
                .flex()
                .items_center()
                .gap_2()
                .when(self.palette.scope == PaletteScope::Connections, |row| {
                    row.child(ui::icon(ui::Icon::Connections))
                        .child(tr(i18n_key::WORKSPACE_CONNECTIONS))
                })
                .child(div().flex_1().min_w_0().child(self.palette.query.clone()));
            card = card.child(query).child(
                div()
                    .id("palette-results")
                    .max_h(px(420.))
                    .overflow_y_scroll()
                    .map(ui::preserve_scroll_axes)
                    .track_scroll(&self.palette.scroll)
                    .when(empty_connections, |list| list.child(
                        div().p_3().text_color(rgb(ui::colors().muted)).child(tr(
                            if self.config.profiles.is_empty() {
                                i18n_key::WORKSPACE_NO_SAVED_CONNECTIONS_YET_CHOOSE_NEW_CONNECTION_TO_ADD_ONE
                            } else {
                                i18n_key::WORKSPACE_NO_MATCHING_CONNECTION_PROFILES
                            }
                        ))
                    ))
                    .children(items.iter().enumerate().map(|(index, item)| {
                        let rows = Rc::clone(&items);
                        let selected = index == self.palette.selected;
                        let history = item.history_record();
                        let description = div()
                            .flex()
                            .items_center()
                            .gap_3()
                            .child(div().flex_1().min_w_0().truncate().child(item.label.clone()))
                            .child(
                                div()
                                    .max_w(gpui::relative(0.65))
                                    .truncate()
                                    .text_size(px(11.))
                                    .text_color(rgb(ui::colors().muted))
                                    .child(item.detail.clone()),
                            )
                            .when_some(history, |row, record| {
                                row.child(self.history_buttons(record, selected, cx)
                                    .when(!selected, |buttons| buttons.opacity(0.).group_hover("palette-history-row", |s| s.opacity(1.))))
                            });
                        div()
                            .id(("palette-item", index))
                            .when(history.is_some(), |row| row.group("palette-history-row"))
                            .px_3()
                            .py_2()
                            .cursor_pointer()
                            .when(index == self.palette.selected, |row| {
                                row.bg(rgb(ui::colors().selected))
                            })
                            .hover(|row| row.bg(rgb(ui::colors().hover)))
                            .child(description)
                            .when(index == self.palette.selected, |row| {
                                row.children(selected_hint.as_ref().map(|hint| {
                                    div()
                                        .mt_1()
                                        .text_size(px(11.))
                                        .text_color(rgb(ui::colors().muted))
                                        .child(hint.clone())
                                }))
                            })
                            .on_click(cx.listener(move |this, _, window, cx| {
                                if let Some(item) = rows.get(index) {
                                    this.palette.selected = index;
                                    this.run_palette_action(item.action.clone(), window, cx);
                                    cx.notify();
                                }
                            }))
                    }))
                    .map(|content| ui::scrollable(content, Some(self.palette.scroll.clone()))),
            );
        }
        if let Some(error) = &self.error {
            card = card.child(
                div()
                    .text_color(rgb(ui::colors().danger))
                    .child(error.render()),
            );
        }
        div()
            .track_focus(&self.palette.focus)
            .absolute()
            .inset_0()
            .occlude()
            .flex()
            .justify_center()
            .items_start()
            .p_6()
            .bg(rgba(0x101114bb))
            .map(|backdrop| WindowFrame::new(window).rounded(backdrop))
            .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
            .on_key_down(cx.listener(|this, event: &KeyDownEvent, window, cx| {
                if this.palette.is_composing(cx) {
                    return;
                }
                if event.keystroke.key == "escape" {
                    this.close_palette(window, cx);
                    window.prevent_default();
                    cx.stop_propagation();
                    return;
                }
                if event.keystroke.key == "tab" {
                    if event.keystroke.modifiers.shift {
                        window.focus_prev();
                    } else {
                        window.focus_next();
                    }
                    if !this.palette.focus.contains_focused(window, cx) {
                        this.focus_palette(window, cx);
                    }
                    window.prevent_default();
                    cx.stop_propagation();
                    return;
                }
                if this.palette.editing.is_none()
                    && matches!(event.keystroke.key.as_str(), "up" | "down")
                {
                    let count = this.palette_navigation_items(cx).len().max(1);
                    this.palette.selected = if event.keystroke.key == "up" {
                        (this.palette.selected + count - 1) % count
                    } else {
                        (this.palette.selected + 1) % count
                    };
                    this.palette.scroll.scroll_to_item(this.palette.selected);
                    this.palette.query.read(cx).focus(window);
                    cx.notify();
                    window.prevent_default();
                    cx.stop_propagation();
                }
            }))
            .child(card)
            .into_any_element()
    }
}

#[cfg(test)]
mod shortcut_tests {
    use super::*;
    use gpui::{Keystroke, Modifiers};

    #[test]
    fn quick_open_reuses_ranked_results_until_the_search_or_connections_change() {
        use rustterm::config::{LocalConfig, Profile};
        let mut config = AppConfig {
            profiles: ["alpha", "beta"]
                .map(|name| Profile::local(name, LocalConfig::default()))
                .into(),
            ..Default::default()
        };
        let mut results = QuickOpenResults::default();
        let visible = results.items(&config, "");
        assert_eq!(visible.len(), 2);
        assert!(Rc::ptr_eq(&visible, &results.items(&config, "")));

        let filtered = results.items(&config, "beta");
        assert_eq!(filtered.len(), 1);
        assert!(
            matches!(filtered[0].action, Action::Connect { index: 1, id } if id == config.profiles[1].id)
        );
        assert!(results.items(&config, "missing").is_empty());

        config.profiles[0].name = "missing".into();
        results.invalidate();
        let updated = results.items(&config, "missing");
        assert_eq!(updated.len(), 1);
        assert_eq!(updated[0].label, "missing");
    }

    #[test]
    fn quick_open_ranks_recent_connections_only_for_an_empty_picker() {
        use rustterm::config::{LocalConfig, Profile, QuickOpenOrder};
        let group = Group::new("Team");
        let mut grouped = Profile::local("api-backup", LocalConfig::default());
        grouped.group = Some(group.id);
        let exact = Profile::local("api", LocalConfig::default());
        let other = Profile::local("worker", LocalConfig::default());
        let mut config = AppConfig {
            groups: vec![group],
            profiles: vec![grouped.clone(), exact.clone(), other.clone()],
            ..Default::default()
        };
        config.recent_connections.remember(grouped.id);
        config.recent_connections.remember(other.id);
        let order = |config: &AppConfig, query| {
            connection_items(config, query, PaletteScope::Connections)
                .into_iter()
                .map(|item| item.label)
                .collect::<Vec<_>>()
        };
        assert_eq!(order(&config, ""), ["worker", "api-backup", "api"]);
        assert_eq!(order(&config, "api"), ["api", "api-backup"]);
        config.settings.quick_open_order = QuickOpenOrder::ConnectionList;
        assert_eq!(order(&config, ""), ["api", "worker", "api-backup"]);
    }

    #[test]
    fn quick_open_finds_saved_connections_by_name_address_protocol_and_nested_group() {
        use rustterm::config::{AppConfig, Group, LocalConfig, Profile, SshConfig};

        let org = Group::new("Example");
        let mut group = Group::new("生产");
        group.parent = Some(org.id);
        let mut api = Profile::ssh(
            "api",
            SshConfig {
                host: "web.example.test".into(),
                username: "deploy".into(),
                ..Default::default()
            },
        );
        api.group = Some(group.id);
        let other = Profile::local("api-backup", LocalConfig::default());
        let config = AppConfig {
            profiles: vec![other, api.clone()],
            groups: vec![org, group],
            ..Default::default()
        };
        let matches = connection_items(&config, "API", PaletteScope::Connections);
        assert_eq!(matches.len(), 2);
        assert!(matches!(matches[0].action, Action::Connect { id, .. } if id == api.id));
        let matches = connection_items(&config, "生产 ssh WEB", PaletteScope::Connections);
        assert_eq!(matches.len(), 1);
        assert!(matches[0].detail.contains("Example / 生产"));
        assert!(matches[0].detail.contains("deploy@web.example.test"));
        assert!(connection_items(&config, "missing host", PaletteScope::Connections).is_empty());
    }

    #[test]
    fn reconnect_does_not_capture_shell_history_search_or_altgr() {
        let mut key = Keystroke {
            modifiers: Modifiers {
                control: true,
                ..Default::default()
            },
            key: "r".into(),
            key_char: Some("r".into()),
        };
        assert!(!is_reconnect_shortcut(&key), "Ctrl+R belongs to the shell");
        key.modifiers.shift = true;
        assert!(is_reconnect_shortcut(&key));
        key.key = "R".into();
        key.key_char = Some("R".into());
        assert!(is_reconnect_shortcut(&key));
        key.modifiers.alt = true;
        key.key_char = Some("®".into());
        assert!(!is_reconnect_shortcut(&key), "AltGr text must remain input");
        key.modifiers.alt = false;
        key.modifiers.platform = true;
        assert!(!is_reconnect_shortcut(&key));
        key.modifiers.platform = false;
        key.modifiers.control = false;
        assert!(!is_reconnect_shortcut(&key), "typing R must remain input");
    }
}
