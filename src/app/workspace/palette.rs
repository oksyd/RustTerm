use super::{Modal, Workspace};
use crate::app::forms::SettingsSection;
use crate::app::pane::{Pane, PaneEvent};
use crate::app::text_input::{InputEvent, TextInput};
use crate::app::ui;
use crate::app::window_frame::WindowFrame;
use gpui::prelude::*;
use gpui::{
    AnyElement, App, Context, Entity, FocusHandle, KeyDownEvent, MouseButton, Subscription, Window,
    div, px, rgb, rgba,
};
use rustterm::config::{Connection, Uuid};
use rustterm::i18n::messages as i18n_key;
use rustterm::i18n::{Message, tr};

use rustterm::config::QuickCommand;

pub(super) struct Palette {
    pub open: bool,
    query: Entity<TextInput>,
    name: Entity<TextInput>,
    command: Entity<TextInput>,
    editing: Option<Option<Uuid>>,
    selected: usize,
    scroll: gpui::ScrollHandle,
    focus: FocusHandle,
    _subscriptions: Vec<Subscription>,
}
#[derive(Clone)]
struct Item {
    label: String,
    detail: String,
    action: Action,
}
#[derive(Clone)]
enum Action {
    Connect(Uuid),
    PasteSaved { run: Uuid, text: String },
    InsertCommand { run: Uuid, command: u64 },
    CopyOutput { run: Uuid, command: u64 },
    SaveCommand(String),
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
            query,
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
    pub(super) fn toggle_palette(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.lock_screen.locked
            || self.closing
            || self.modal.is_some()
            || self.authentication.read(cx).is_active()
            || self.window_state.prompts.is_busy()
        {
            return;
        }
        if self.palette.open {
            self.close_palette(window, cx);
            return;
        }
        self.palette.open = true;
        self.palette.editing = None;
        self.palette.query.update(cx, |input, cx| input.clear(cx));
        self.palette.selected = 0;
        self.menu = None;
        self.palette.query.read(cx).focus(window);
        cx.notify();
    }
    fn close_palette(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.palette.open = false;
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
        } else if event.keystroke.key == "p"
            && modifiers.control
            && modifiers.shift
            && !modifiers.alt
            && !modifiers.platform
        {
            self.toggle_palette(window, cx);
            window.prevent_default();
            cx.stop_propagation();
        } else if self.palette.open && !self.palette.focus.contains_focused(window, cx) {
            self.focus_palette(window, cx);
            window.prevent_default();
            cx.stop_propagation();
        }
    }
    fn palette_items(&self, cx: &App) -> Vec<Item> {
        let mut items = vec![
            (
                i18n_key::WORKSPACE_NEW_LOCAL_TERMINAL,
                "Ctrl+Shift+T",
                Action::NewTerminal,
            ),
            (
                i18n_key::WORKSPACE_ADD_SAVED_COMMAND,
                "",
                Action::AddCommand,
            ),
            (i18n_key::SFTP_FILE_MANAGER_DIALOG, "", Action::Files),
            (
                if self.files_follow.contains(&self.focused) {
                    i18n_key::WORKSPACE_STOP_FOLLOWING_TERMINAL_DIRECTORY
                } else {
                    i18n_key::WORKSPACE_FOLLOW_TERMINAL_DIRECTORY_IN_SFTP
                },
                "",
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
            detail: detail.into(),
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
                    ),
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
                ),
                detail: String::new(),
                action: Action::Edit(command.id),
            });
        }
        for profile in &self.config.profiles {
            let target = match &profile.connection {
                Connection::Ssh(s) => format!("SSH {}@{}:{}", s.username, s.host, s.port),
                Connection::Telnet(s) => format!("Telnet {}:{}", s.host, s.port),
                Connection::Elasticsearch(config) => format!("Elasticsearch {}", config.endpoint),
                Connection::Redis(s) => format!("Redis {}:{}", s.host, s.port),
                Connection::Mysql(s) => format!("MySQL {}:{} {}", s.host, s.port, s.database),
                Connection::ClickHouse(s) => {
                    format!("ClickHouse {}:{} {}", s.host, s.port, s.database)
                }
                Connection::Postgres(s) => {
                    format!("PostgreSQL {}:{} {}", s.host, s.port, s.database)
                }
                Connection::Sqlite(s) => format!("SQLite {}", s.path.display()),
                Connection::Rdp(s) => format!("RDP {}:{}", s.host, s.port),
                Connection::Local(s) => s.shell.clone(),
            };
            items.push(Item {
                label: profile.name.clone(),
                detail: target,
                action: Action::Connect(profile.id),
            });
        }
        if let Some(Pane::Terminal(terminal)) = self.panes.get(&self.focused)
            && let Some(run) = terminal_run
        {
            for command in terminal.read(cx).command_records().iter().rev().take(32) {
                let detail = command.exit_status.map_or_else(
                    || tr(i18n_key::WORKSPACE_RUNNING).to_owned(),
                    |status| format!("{} {status}", tr(i18n_key::WORKSPACE_EXIT_STATUS)),
                );
                items.push(Item {
                    label: rustterm::trf!(
                        i18n_key::WORKSPACE_INSERT_COMMAND_COMMAND,
                        command = command.command
                    ),
                    detail: detail.clone(),
                    action: Action::InsertCommand {
                        run,
                        command: command.id,
                    },
                });
                if command.output.is_some() {
                    items.push(Item {
                        label: rustterm::trf!(
                            i18n_key::WORKSPACE_COPY_OUTPUT_COMMAND,
                            command = command.command
                        ),
                        detail,
                        action: Action::CopyOutput {
                            run,
                            command: command.id,
                        },
                    });
                }
                items.push(Item {
                    label: rustterm::trf!(
                        i18n_key::WORKSPACE_SAVE_COMMAND_COMMAND,
                        command = command.command
                    ),
                    detail: String::new(),
                    action: Action::SaveCommand(command.command.clone()),
                });
            }
        }
        let query = self.palette.query.read(cx).value().to_lowercase();
        items
            .into_iter()
            .filter(|item| self.palette_action_available(&item.action, cx))
            .filter(|item| {
                format!("{} {}", item.label, item.detail)
                    .to_lowercase()
                    .contains(&query)
            })
            .take(128)
            .collect()
    }

    fn palette_action_available(&self, action: &Action, cx: &App) -> bool {
        match action {
            Action::Files
            | Action::SyncFiles
            | Action::Forwards
            | Action::PasteSaved { .. }
            | Action::InsertCommand { .. }
            | Action::CopyOutput { .. } => {
                let Some(Pane::Terminal(terminal)) = self.panes.get(&self.focused) else {
                    return false;
                };
                let terminal = terminal.read(cx);
                match action {
                    Action::Files => {
                        self.files.contains_key(&self.focused) || terminal.sftp_client().is_some()
                    }
                    Action::SyncFiles => {
                        self.files_follow.contains(&self.focused)
                            || (terminal.sftp_client().is_some()
                                && terminal.current_directory().is_some())
                    }
                    Action::Forwards => terminal.forwarding().is_some(),
                    Action::PasteSaved { run, .. } => {
                        !self.utility_overlay
                            && terminal.history_actions_allowed(cx)
                            && terminal.can_insert_history_command(*run)
                    }
                    Action::InsertCommand { run, command } => {
                        !self.utility_overlay
                            && terminal.history_actions_allowed(cx)
                            && terminal.can_insert_history_command(*run)
                            && terminal.command_records().iter().any(|record| {
                                record.id == *command
                                    && !record.command.is_empty()
                                    && !record.command.chars().any(char::is_control)
                            })
                    }
                    Action::CopyOutput { run, command } => {
                        terminal.history_actions_allowed(cx)
                            && terminal.history_run() == Some(*run)
                            && terminal
                                .command_records()
                                .iter()
                                .any(|record| record.id == *command && record.output.is_some())
                    }
                    _ => unreachable!(),
                }
            }
            _ => true,
        }
    }
    fn run_palette_selection(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.lock_screen.locked || self.palette.query.read(cx).is_composing() {
            return;
        }
        let Some(item) = self.palette_items(cx).get(self.palette.selected).cloned() else {
            return;
        };
        self.run_palette_action(item.action, window, cx);
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
            Action::AddCommand => {
                self.edit_quick_command(None, String::new(), window, cx);
                return;
            }
            Action::Edit(id) => {
                self.edit_quick_command(Some(id), String::new(), window, cx);
                return;
            }
            Action::SaveCommand(text) => {
                self.edit_quick_command(None, text, window, cx);
                return;
            }
            _ => self.close_palette(window, cx),
        }
        match action {
            Action::Connect(id) => {
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
                            terminal.send_text(&text, cx);
                        }
                    });
                }
            }
            Action::InsertCommand { run, command } => {
                if let Some(Pane::Terminal(terminal)) = self.panes.get(&self.focused) {
                    terminal.update(cx, |terminal, cx| {
                        terminal.insert_history_command(run, command, cx);
                    });
                }
            }
            Action::CopyOutput { run, command } => {
                if let Some(Pane::Terminal(terminal)) = self.panes.get(&self.focused) {
                    terminal.update(cx, |terminal, cx| {
                        if terminal.history_run() == Some(run) {
                            terminal.copy_command_output(command, cx);
                        }
                    });
                }
            }
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
        let controller = match self.panes.get(&self.focused) {
            Some(Pane::Terminal(terminal)) => terminal.read(cx).forwarding(),
            _ => None,
        };
        let Some(controller) = controller else {
            self.error = Some(i18n_key::WORKSPACE_PORT_FORWARDING_REQUIRES_AN_SSH_SESSION.into());
            cx.notify();
            return;
        };
        self.modal = Some(Modal::Tunnels(cx.new(|cx| {
            crate::app::tunnel_panel::TunnelPanel::new(controller, self.window_state.clone(), cx)
        })));
        self.modal_focus.focus(window);
        cx.notify();
    }
    pub(super) fn follow_terminal_directory(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.files_follow.remove(&self.focused) {
            if let Some(files) = self.files.get(&self.focused) {
                files.update(cx, |files, _| files.cancel_pending_navigation());
            }
            cx.notify();
            return;
        }
        let directory = match self.panes.get(&self.focused) {
            Some(Pane::Terminal(terminal)) => terminal.read(cx).current_directory(),
            _ => None,
        };
        let Some(directory) = directory else {
            self.error = Some(
                i18n_key::WORKSPACE_ENABLE_SHELL_INTEGRATION_TO_FOLLOW_THE_TERMINAL_DIRECTORY
                    .into(),
            );
            cx.notify();
            return;
        };
        self.open_files(window, cx);
        if let Some(files) = self.active_files() {
            files.update(cx, |files, cx| files.navigate_to(directory, cx));
            self.files_follow.insert(self.focused);
        }
    }
    pub(super) fn render_palette(&self, window: &mut Window, cx: &mut Context<Self>) -> AnyElement {
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
            card = card.child(self.palette.query.clone()).child(
                div()
                    .id("palette-results")
                    .max_h(px(420.))
                    .overflow_y_scroll()
                    .map(ui::preserve_scroll_axes)
                    .track_scroll(&self.palette.scroll)
                    .children(items.into_iter().enumerate().map(|(index, item)| {
                        div()
                            .id(("palette-item", index))
                            .flex()
                            .items_center()
                            .gap_3()
                            .px_3()
                            .py_2()
                            .cursor_pointer()
                            .when(index == self.palette.selected, |row| {
                                row.bg(rgb(ui::colors().selected))
                            })
                            .hover(|row| row.bg(rgb(ui::colors().hover)))
                            .child(div().flex_1().min_w_0().truncate().child(item.label))
                            .child(
                                div()
                                    .text_size(px(11.))
                                    .text_color(rgb(ui::colors().muted))
                                    .child(item.detail),
                            )
                            .on_click(cx.listener(move |this, _, window, cx| {
                                this.run_palette_action(item.action.clone(), window, cx)
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
                let composing = if this.palette.editing.is_some() {
                    this.palette.name.read(cx).is_composing()
                        || this.palette.command.read(cx).is_composing()
                } else {
                    this.palette.query.read(cx).is_composing()
                };
                if composing {
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
                    let count = this.palette_items(cx).len().max(1);
                    this.palette.selected = if event.keystroke.key == "up" {
                        (this.palette.selected + count - 1) % count
                    } else {
                        (this.palette.selected + 1) % count
                    };
                    this.palette.scroll.scroll_to_item(this.palette.selected);
                    cx.notify();
                    window.prevent_default();
                    cx.stop_propagation();
                }
            }))
            .child(card)
            .into_any_element()
    }
}
