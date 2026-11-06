use crate::app::{
    terminal_view::TerminalView,
    ui::{self, ButtonStyle, Icon},
};
use gpui::{
    AnyElement, App, Bounds, Context, Entity, EventEmitter, FocusHandle, Focusable, KeyDownEvent,
    MouseButton, MouseDownEvent, Pixels, Point, Render, ScrollHandle, Subscription, Window, canvas,
    div, point, prelude::*, px, rgb,
};
use rustterm::i18n::messages as i18n_key;
use rustterm::{config::Uuid, i18n::tr, terminal::ShellCommand};
use std::sync::Arc;

mod menu;

#[derive(Clone, Copy)]
struct CommandMenu {
    run: Uuid,
    command: u64,
    position: Point<Pixels>,
    index: usize,
}

#[derive(Clone, Copy, Debug)]
pub enum CommandHistoryEvent {
    Close,
    Reveal { run: Uuid, command: u64 },
    Insert { run: Uuid, command: u64 },
}

pub struct CommandHistoryView {
    terminal: Entity<TerminalView>,
    run: Option<Uuid>,
    records: Arc<[ShellCommand]>,
    selected: Option<u64>,
    selection_expired: bool,
    integration_enabled: bool,
    integration_available: bool,
    insertion_available: bool,
    focus: FocusHandle,
    scroll: ScrollHandle,
    bounds: Option<Bounds<Pixels>>,
    menu: Option<CommandMenu>,
    menu_focus: FocusHandle,
    menu_scroll: ScrollHandle,
    _menu_blur: Subscription,
    _observe: Subscription,
}

impl EventEmitter<CommandHistoryEvent> for CommandHistoryView {}
impl Focusable for CommandHistoryView {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus.clone()
    }
}

impl CommandHistoryView {
    pub fn new(
        terminal: Entity<TerminalView>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let _observe = cx.observe(&terminal, |this, _, cx| {
            this.sync(cx);
        });
        let run = terminal.read(cx).history_run();
        let records = terminal.read(cx).command_records();
        let selected = records.last().map(|command| command.id);
        let integration_enabled = terminal.read(cx).history_integration_enabled();
        let integration_available = terminal.read(cx).can_enable_shell_integration(cx);
        let insertion_available =
            run.is_some_and(|run| terminal.read(cx).can_insert_history_command(run));
        let menu_focus = cx.focus_handle();
        let _menu_blur = cx.on_blur(&menu_focus, window, |this: &mut Self, _, cx| {
            if this.menu.take().is_some() {
                cx.notify();
            }
        });
        Self {
            terminal,
            run,
            records,
            selected,
            selection_expired: false,
            integration_enabled,
            integration_available,
            insertion_available,
            focus: cx.focus_handle(),
            scroll: ScrollHandle::new(),
            bounds: None,
            menu: None,
            menu_focus,
            menu_scroll: ScrollHandle::new(),
            _menu_blur,
            _observe,
        }
    }

    pub fn focus(&self, window: &mut Window, _: &App) {
        self.focus.focus(window);
    }
    pub fn focus_handle(&self) -> &FocusHandle {
        &self.focus
    }
    pub fn owns_focus(&self, window: &Window, cx: &App) -> bool {
        self.focus.contains_focused(window, cx) || self.menu_focus.is_focused(window)
    }

    fn sync(&mut self, cx: &mut Context<Self>) {
        let terminal = self.terminal.read(cx);
        let run = terminal.history_run();
        let records = terminal.command_records();
        let integration_enabled = terminal.history_integration_enabled();
        let integration_available = terminal.can_enable_shell_integration(cx);
        let insertion_available = run.is_some_and(|run| terminal.can_insert_history_command(run));
        if self.menu.is_some_and(|menu| {
            Some(menu.run) != run
                || !records.iter().any(|record| record.id == menu.command)
                || !terminal.history_actions_allowed(cx)
        }) {
            self.menu = None;
            cx.notify();
        }
        let actions_changed = self.integration_enabled != integration_enabled
            || self.integration_available != integration_available
            || self.insertion_available != insertion_available;
        self.integration_enabled = integration_enabled;
        self.integration_available = integration_available;
        self.insertion_available = insertion_available;
        if self.run == run && Arc::ptr_eq(&records, &self.records) {
            if actions_changed {
                cx.notify();
            }
            return;
        }
        if self.run != run {
            self.run = run;
            self.selected = records.last().map(|command| command.id);
            self.selection_expired = false;
            self.scroll.scroll_to_item(0);
        } else if self
            .selected
            .is_some_and(|id| !records.iter().any(|command| command.id == id))
        {
            self.selected = None;
            self.selection_expired = true;
        }
        if self.records.is_empty() && !self.selection_expired {
            self.selected = records.last().map(|command| command.id);
        }
        self.records = records;
        cx.notify();
    }

    fn select(&mut self, run: Uuid, id: u64, cx: &mut Context<Self>) {
        self.sync(cx);
        if self.run == Some(run) && self.records.iter().any(|command| command.id == id) {
            self.selected = Some(id);
            self.selection_expired = false;
            cx.notify();
        }
    }

    fn selected_record(&self, run: Uuid, id: u64, cx: &App) -> Option<&ShellCommand> {
        (self.run == Some(run)
            && self.selected == Some(id)
            && self.terminal.read(cx).history_run() == Some(run)
            && self.terminal.read(cx).history_actions_allowed(cx))
        .then(|| self.records.iter().find(|command| command.id == id))
        .flatten()
    }

    fn key_down(&mut self, event: &KeyDownEvent, window: &mut Window, cx: &mut Context<Self>) {
        if self.menu.is_some() {
            self.menu_key(event, window, cx);
            return;
        }
        if event.keystroke.key == "escape" {
            cx.emit(CommandHistoryEvent::Close);
            cx.stop_propagation();
            return;
        }
        if event.keystroke.modifiers.control
            || event.keystroke.modifiers.alt
            || event.keystroke.modifiers.platform
        {
            return;
        }
        let (Some(run), false) = (self.run, self.records.is_empty()) else {
            return;
        };
        let current = self
            .selected
            .and_then(|id| self.records.iter().position(|record| record.id == id))
            .unwrap_or(self.records.len() - 1);
        if (event.keystroke.modifiers.shift && event.keystroke.key == "f10")
            || (!event.keystroke.modifiers.shift && event.keystroke.key == "menu")
        {
            let position = self
                .scroll
                .bounds_for_item(self.records.len() - 1 - current)
                .map(|bounds| bounds.origin + self.scroll.offset() + point(px(12.), px(12.)))
                .unwrap_or_else(|| self.scroll.bounds().origin);
            self.open_menu(run, self.records[current].id, position, window, cx);
            window.prevent_default();
            cx.stop_propagation();
            return;
        }
        let next = match event.keystroke.key.as_str() {
            "up" => (current + 1).min(self.records.len() - 1),
            "down" => current.saturating_sub(1),
            "home" => self.records.len() - 1,
            "end" => 0,
            _ => return,
        };
        self.select(run, self.records[next].id, cx);
        self.scroll.scroll_to_item(self.records.len() - 1 - next);
        window.prevent_default();
        cx.stop_propagation();
    }

    fn row(&self, run: Uuid, command: &ShellCommand, cx: &Context<Self>) -> AnyElement {
        let id = command.id;
        let selected = self.selected == Some(id);
        let status = match command.exit_status {
            Some(0) => (ui::colors().success, "0".into()),
            Some(code) => (ui::colors().danger, code.to_string()),
            None => (ui::colors().muted, "—".into()),
        };
        div()
            .id(("history-command", id))
            .px_3()
            .py_2()
            .flex()
            .flex_col()
            .gap_1()
            .border_l_2()
            .border_color(rgb(if selected {
                ui::colors().accent
            } else {
                ui::colors().surface
            }))
            .bg(rgb(if selected {
                ui::colors().selected
            } else {
                ui::colors().surface
            }))
            .cursor_pointer()
            .hover(|style| style.bg(rgb(ui::colors().hover)))
            .on_click(cx.listener(move |this, _, _, cx| this.select(run, id, cx)))
            .on_mouse_down(
                MouseButton::Right,
                cx.listener(move |this, event: &MouseDownEvent, window, cx| {
                    this.open_menu(run, id, event.position, window, cx);
                    window.prevent_default();
                    cx.stop_propagation();
                }),
            )
            .child(
                div()
                    .font_family(ui::MONOSPACE_FONT)
                    .text_size(px(12.))
                    .truncate()
                    .child(command.command.replace(['\n', '\r'], " ↵ ")),
            )
            .child(
                div()
                    .flex()
                    .gap_2()
                    .items_center()
                    .text_size(px(10.))
                    .text_color(rgb(ui::colors().muted))
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .truncate()
                            .child(command.cwd.clone().unwrap_or_default()),
                    )
                    .child(format!("{:.1}s", command.duration.as_secs_f64()))
                    .child(div().text_color(rgb(status.0)).child(format!(
                        "{} {}",
                        tr(i18n_key::TERMINAL_EXIT),
                        status.1
                    ))),
            )
            .into_any_element()
    }

    fn detail(&self, command: &ShellCommand) -> AnyElement {
        div()
            .flex_shrink_0()
            .p_3()
            .border_t_1()
            .border_color(rgb(ui::colors().border))
            .flex()
            .flex_col()
            .gap_2()
            .child(
                div()
                    .text_size(px(11.))
                    .text_color(rgb(ui::colors().muted))
                    .child(tr(i18n_key::TERMINAL_SELECTED_COMMAND_OUTPUT)),
            )
            .child(
                div()
                    .id("command-output-preview")
                    .max_h(px(160.))
                    .overflow_y_scroll()
                    .map(ui::preserve_scroll_axes)
                    .font_family(ui::MONOSPACE_FONT)
                    .text_size(px(11.))
                    .child(command.output.as_ref().map_or_else(
                        || tr(if command.output_cache_disabled {
                            i18n_key::TERMINAL_COMMAND_OUTPUT_CACHE_WAS_DISABLED
                        } else {
                            i18n_key::TERMINAL_COMMAND_OUTPUT_IS_NO_LONGER_RETAINED_OR_EXCEEDS_64_KIB
                        }).to_owned(),
                        |output| {
                            if output.is_empty() {
                                tr(i18n_key::TERMINAL_THIS_COMMAND_PRODUCED_NO_OUTPUT).to_owned()
                            } else {
                                output.to_string()
                            }
                        },
                    ))
                    .map(|content| ui::scrollable(content, None)),
            )
            .child(
                div()
                    .text_size(px(10.))
                    .text_color(rgb(ui::colors().muted))
                    .child(tr(
                        i18n_key::TERMINAL_COMMAND_CONTEXT_MENU_HINT,
                    )),
            )
            .into_any_element()
    }
}

impl Render for CommandHistoryView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        self.sync(cx);
        if self.menu.is_none() && self.menu_focus.is_focused(window) {
            self.focus.focus(window);
        }
        let integration_hint = if !self.terminal.read(cx).is_running() {
            i18n_key::COMMON_CONNECT_THE_TERMINAL_BEFORE_ENABLING_SHELL_INTEGRATION
        } else {
            i18n_key::COMMON_RETURN_TO_THE_LIVE_TERMINAL_AND_LEAVE_FULL_SCREEN_APPLICATIONS
        };
        let integration = ui::text_button_enabled(
            "command-shell-integration",
            if self.integration_enabled {
                i18n_key::TERMINAL_SHELL_INTEGRATION_DIALOG
            } else {
                i18n_key::TERMINAL_ENABLE_SHELL_INTEGRATION
            },
            ButtonStyle::Neutral,
            self.integration_available,
        )
        .text_size(px(11.))
        .when(!self.integration_available, |button| {
            button.tooltip(move |_, cx| cx.new(|_| ui::Tooltip(tr(integration_hint).into())).into())
        })
        .when(self.integration_available, |button| {
            button.on_click(cx.listener(|this, _, window, cx| {
                this.terminal.update(cx, |terminal, cx| {
                    terminal.enable_shell_integration(window, cx);
                });
            }))
        });
        let mut list = div()
            .id("command-history-list")
            .flex_1()
            .min_h_0()
            .overflow_y_scroll()
            .map(ui::preserve_scroll_axes)
            .track_scroll(&self.scroll);
        if self.records.is_empty() {
            list = list.child(div().p_4().flex().flex_col().gap_3().text_color(rgb(ui::colors().muted))
                .child(ui::icon(Icon::Terminal).size(px(24.)))
                .child(tr(i18n_key::TERMINAL_NO_COMMANDS_RECORDED_YET))
                .child(div().text_size(px(12.)).child(tr(if self.integration_enabled {
                    i18n_key::TERMINAL_COMPLETED_COMMANDS_WILL_APPEAR_HERE_SELECT_ONE_TO_INSPECT_ITS
                } else {
                    i18n_key::TERMINAL_SHELL_INTEGRATION_RECORDS_COMPLETED_COMMANDS_AND_THEIR_OUTPUT
                }))));
        } else if let Some(run) = self.run {
            list = list.children(
                self.records
                    .iter()
                    .rev()
                    .map(|command| self.row(run, command, cx)),
            );
        }
        let detail = self.selected.and_then(|id| {
            self.records
                .iter()
                .find(|command| command.id == id)
                .map(|command| self.detail(command))
        });
        let owner = cx.entity();
        div()
            .id("command-history")
            .relative()
            .size_full()
            .min_w_0()
            .min_h_0()
            .flex()
            .flex_col()
            .bg(rgb(ui::colors().surface))
            .text_color(rgb(ui::colors().text))
            .text_size(px(12.))
            .track_focus(&self.focus)
            .tab_index(0)
            .on_key_down(cx.listener(Self::key_down))
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|this, _, window, cx| {
                    if !window.default_prevented() {
                        this.focus.focus(window);
                    }
                    cx.stop_propagation();
                }),
            )
            .child(
                div()
                    .px_3()
                    .py_2()
                    .flex_shrink_0()
                    .flex()
                    .justify_end()
                    .child(integration),
            )
            .child(ui::scrollable(list, Some(self.scroll.clone())))
            .children(detail)
            .child(canvas(move |bounds, _, cx| {
                owner.update(cx, |this, cx| {
                    if this.bounds != Some(bounds) {
                        this.bounds = Some(bounds);
                        if this.menu.is_some() {
                            cx.notify();
                        }
                    }
                });
            }, |_, _, _, _| {}).absolute().inset_0())
            .children(self.render_menu(cx))
            .when(self.selection_expired, |d| {
                d.child(div().p_3().text_color(rgb(ui::colors().muted)).child(tr(
                    i18n_key::TERMINAL_THE_SELECTED_COMMAND_IS_NO_LONGER_RETAINED_SELECT_ANOTHER_COMMAND,
                )))
            })
    }
}
