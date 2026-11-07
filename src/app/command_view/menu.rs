use super::{CommandHistoryEvent, CommandHistoryView, CommandMenu};
use crate::app::ui::{self, Icon};
use gpui::{
    AnyElement, App, ClipboardItem, Context, KeyDownEvent, Pixels, Point, Window, anchored, div,
    prelude::*, px,
};
use rustterm::{config::Uuid, i18n::messages as i18n_key, i18n::tr};

#[derive(Clone, Copy)]
enum Action {
    Reveal,
    Insert,
    CopyCommand,
    CopyOutput,
}

const ACTIONS: [(Action, &str, Icon); 4] = [
    (
        Action::Reveal,
        i18n_key::TERMINAL_LOCATE_COMMAND,
        Icon::Search,
    ),
    (
        Action::Insert,
        i18n_key::TERMINAL_INSERT_INTO_TERMINAL,
        Icon::Enter,
    ),
    (
        Action::CopyCommand,
        i18n_key::TERMINAL_COPY_COMMAND,
        Icon::Copy,
    ),
    (
        Action::CopyOutput,
        i18n_key::TERMINAL_COPY_COMMAND_OUTPUT,
        Icon::Copy,
    ),
];

impl CommandHistoryView {
    pub(super) fn open_menu(
        &mut self,
        run: Uuid,
        command: u64,
        position: Point<Pixels>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.select(run, command, cx);
        if self.selected_record(run, command, cx).is_none() {
            return;
        }
        self.menu = Some(CommandMenu {
            run,
            command,
            position,
            index: 0,
        });
        self.menu_scroll.scroll_to_item(0);
        self.menu_focus.focus(window);
        cx.notify();
    }

    fn close_menu(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.menu = None;
        self.focus.focus(window);
        cx.notify();
    }

    fn action_disabled(&self, menu: CommandMenu, action: Action, cx: &App) -> Option<&'static str> {
        let Some(command) = self.selected_record(menu.run, menu.command, cx) else {
            return Some(i18n_key::TERMINAL_THE_SELECTED_COMMAND_IS_NO_LONGER_RETAINED_SELECT_ANOTHER_COMMAND);
        };
        let terminal = self.terminal.read(cx);
        match action {
            Action::Insert if command.command.chars().any(char::is_control) => {
                Some(i18n_key::COMMON_COMMANDS_WITH_CONTROL_CHARACTERS_CANNOT_BE_INSERTED)
            }
            Action::Insert if !terminal.is_running() => {
                Some(i18n_key::COMMON_CONNECT_THE_TERMINAL_BEFORE_INSERTING_COMMANDS)
            }
            Action::Insert if !terminal.can_insert_history_command(menu.run) => {
                Some(i18n_key::TERMINAL_INSERT_REQUIRES_A_SINGLE_LINE_COMMAND_AND_AN_IDLE_SHELL)
            }
            Action::CopyOutput if command.output.is_none() => {
                Some(if command.output_cache_disabled {
                    i18n_key::TERMINAL_COMMAND_OUTPUT_CACHE_WAS_DISABLED
                } else {
                    i18n_key::TERMINAL_COMMAND_OUTPUT_IS_NO_LONGER_RETAINED_OR_EXCEEDS_64_KIB
                })
            }
            _ => None,
        }
    }

    fn run_menu_action(
        &mut self,
        target: CommandMenu,
        action: Action,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        // A reconnect or history eviction may have happened since the menu was drawn.
        self.sync(cx);
        if !self
            .menu
            .is_some_and(|menu| menu.run == target.run && menu.command == target.command)
            || self.action_disabled(target, action, cx).is_some()
        {
            return;
        }
        let Some(command) = self.selected_record(target.run, target.command, cx) else {
            return;
        };
        match action {
            Action::CopyCommand => {
                cx.write_to_clipboard(ClipboardItem::new_string(command.command.clone()));
            }
            Action::CopyOutput => {
                if let Some(output) = &command.output {
                    cx.write_to_clipboard(ClipboardItem::new_string(output.to_string()));
                }
            }
            Action::Reveal | Action::Insert => {}
        }
        self.close_menu(window, cx);
        match action {
            Action::Reveal => cx.emit(CommandHistoryEvent::Reveal {
                run: target.run,
                command: target.command,
            }),
            Action::Insert => cx.emit(CommandHistoryEvent::Insert {
                run: target.run,
                command: target.command,
            }),
            Action::CopyCommand | Action::CopyOutput => {}
        }
    }

    pub(super) fn menu_key(
        &mut self,
        event: &KeyDownEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(menu) = self.menu else {
            return;
        };
        let key = event.keystroke.key.as_str();
        match key {
            "escape" => self.close_menu(window, cx),
            "enter" | "return" | "space" => {
                self.run_menu_action(menu, ACTIONS[menu.index].0, window, cx)
            }
            "up" | "down" | "tab" | "home" | "end" => {
                let enabled: Vec<_> = ACTIONS
                    .iter()
                    .map(|(action, _, _)| self.action_disabled(menu, *action, cx).is_none())
                    .collect();
                let next = ui::popup::next_index(event, menu.index, &enabled);
                if let Some(index) = next {
                    self.menu.as_mut().unwrap().index = index;
                    self.menu_scroll.scroll_to_item(index);
                    cx.notify();
                }
            }
            _ => {}
        }
        window.prevent_default();
        cx.stop_propagation();
    }

    pub(super) fn render_menu(&self, cx: &Context<Self>) -> Option<AnyElement> {
        let menu = self.menu?;
        let bounds = self.bounds?;
        let width = (bounds.size.width - px(8.)).clamp(px(1.), px(280.));
        let height = (bounds.size.height - px(8.)).clamp(px(1.), px(130.));
        let position = ui::popup::fit(menu.position, gpui::size(width, height), bounds).origin;
        let popup = ui::popup::surface("command-context-menu")
            .w(width).max_h(height)
            .overflow_y_scroll()
            .map(ui::preserve_scroll_axes)
            .track_scroll(&self.menu_scroll)
            .track_focus(&self.menu_focus)
            .on_key_down(cx.listener(Self::menu_key))
            .on_any_mouse_down(|_, window, cx| {
                window.prevent_default();
                cx.stop_propagation();
            })
            .on_mouse_down_out(cx.listener(|this, _, window, cx| {
                this.close_menu(window, cx);
            }))
            .children(
                ACTIONS
                    .into_iter()
                    .enumerate()
                    .map(|(index, (action, label, icon))| {
                        let reason = self.action_disabled(menu, action, cx);
                        ui::popup::row(("command-context-action", index), reason.is_none(),menu.index == index)
                    .when(reason.is_none(), |d| {
                        d
                            .on_mouse_move(cx.listener(move |this, _, _, cx| {
                                if let Some(menu) = &mut this.menu && menu.index != index {
                                    menu.index = index;
                                    cx.notify();
                                }
                            }))
                    })
                    .when_some(reason.or(matches!(action, Action::Insert).then_some(
                        i18n_key::TERMINAL_INSERTS_A_DRAFT_AT_THE_SHELL_PROMPT_PRESS_ENTER_TO,
                    )), |d, hint| {
                        d.tooltip(move |_, cx| cx.new(|_| ui::Tooltip(tr(hint).into())).into())
                    })
                    .child(ui::icon(icon))
                    .child(div().flex_1().min_w_0().truncate().child(tr(label)))
                    .on_click(cx.listener(move |this, _, window, cx| {
                        this.run_menu_action(menu, action, window, cx);
                        cx.stop_propagation();
                    }))
                    }),
            );
        Some(
            anchored()
                .position(position)
                .child(ui::scrollable(popup, Some(self.menu_scroll.clone())))
                .into_any_element(),
        )
    }
}
