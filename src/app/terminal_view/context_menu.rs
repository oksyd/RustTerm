use super::TerminalView;
use crate::app::{pane::PaneEvent, ui};
use gpui::{
    AnyElement, Context, CursorStyle, KeyDownEvent, Pixels, Point, Window, anchored, div, point,
    prelude::*, px, rgb,
};
use rustterm::i18n::messages as i18n_key;
use rustterm::{i18n::tr, session::Command};

#[derive(Clone, Copy)]
enum Action {
    Copy,
    Paste,
    SelectAll,
    Find,
    Bottom,
}

const ACTIONS: [(Action, &str, &str); 5] = [
    (
        Action::Copy,
        i18n_key::WORKSPACE_COPY_SELECTION,
        "Ctrl+Shift+C",
    ),
    (Action::Paste, i18n_key::COMMON_PASTE, "Ctrl+Shift+V"),
    (
        Action::SelectAll,
        i18n_key::COMMON_SELECT_ALL,
        "Ctrl+Shift+A",
    ),
    (
        Action::Find,
        i18n_key::WORKSPACE_SEARCH_TERMINAL_OUTPUT,
        "Ctrl+Shift+F",
    ),
    (
        Action::Bottom,
        i18n_key::TERMINAL_SCROLL_TO_BOTTOM,
        "Shift+End",
    ),
];

impl TerminalView {
    pub(super) fn open_context_menu(
        &mut self,
        position: Point<Pixels>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if !self.visible
            || !self.window_state.visibility.is_visible()
            || self.window_state.is_locked()
            || self.chrome_blocked(cx)
        {
            return;
        }
        self.menu_open = true;
        self.context_menu_position = Some(position);
        self.menu_index = self.context_menu_enabled_index(0, false);
        self.menu_scroll.scroll_to_item(0);
        cx.emit(PaneEvent::Focused(self.index));
        self.menu_focus.focus(window);
        cx.notify();
    }

    fn context_action_enabled(&self, action: Action) -> bool {
        match action {
            Action::Copy => {
                !self.show_final_alternate
                    && self.snapshot.as_ref().is_some_and(|s| s.has_selection)
            }
            Action::Paste => self.can_send_input(),
            Action::SelectAll => !self.show_final_alternate && self.snapshot.is_some(),
            Action::Find => self.snapshot.is_some(),
            Action::Bottom => {
                !self.show_final_alternate
                    && self.snapshot.as_ref().is_some_and(|s| s.display_offset > 0)
            }
        }
    }

    fn context_menu_enabled_index(&self, start: usize, backwards: bool) -> usize {
        (0..ACTIONS.len())
            .map(|step| {
                if backwards {
                    (start + ACTIONS.len() - step) % ACTIONS.len()
                } else {
                    (start + step) % ACTIONS.len()
                }
            })
            .find(|&index| self.context_action_enabled(ACTIONS[index].0))
            .unwrap_or(start)
    }

    fn context_menu_action(&mut self, action: Action, window: &mut Window, cx: &mut Context<Self>) {
        if self.window_state.is_locked()
            || self.chrome_blocked(cx)
            || !self.context_action_enabled(action)
        {
            return;
        }
        self.close_menu(window, cx);
        match action {
            Action::Copy => self.copy_selection(cx),
            Action::Paste => self.paste(window, cx),
            Action::SelectAll => self.send(Command::SelectAll, cx),
            Action::Find => self.open_search(window, cx),
            Action::Bottom => self.send(Command::ScrollToBottom, cx),
        }
    }

    pub(super) fn context_menu_key(
        &mut self,
        event: &KeyDownEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        self.menu_index = self.menu_index.min(ACTIONS.len() - 1);
        let key = event.keystroke.key.as_str();
        match key {
            "escape" => self.close_menu(window, cx),
            "up" | "down" | "tab" => {
                let backwards = key == "up" || (key == "tab" && event.keystroke.modifiers.shift);
                let start = if backwards {
                    (self.menu_index + ACTIONS.len() - 1) % ACTIONS.len()
                } else {
                    (self.menu_index + 1) % ACTIONS.len()
                };
                self.menu_index = self.context_menu_enabled_index(start, backwards);
            }
            "home" => self.menu_index = self.context_menu_enabled_index(0, false),
            "end" => self.menu_index = self.context_menu_enabled_index(ACTIONS.len() - 1, true),
            "enter" | "return" | "space" => {
                self.context_menu_action(ACTIONS[self.menu_index].0, window, cx)
            }
            _ => {}
        }
        self.menu_scroll.scroll_to_item(self.menu_index);
        cx.stop_propagation();
        window.prevent_default();
        cx.notify();
        true
    }

    pub(super) fn render_context_menu(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        if !self.menu_open {
            return None;
        }
        let position = self.context_menu_position?;
        let bounds = self.bounds?;
        // The pane can be smaller than the window. Constrain both size and anchor
        // before letting GPUI fit the popup to the window's visible bounds.
        let width = (f32::from(bounds.size.width) - 8.).clamp(1., 296.);
        let height = (f32::from(bounds.size.height) - 8.).clamp(1., 150.);
        let position = point(
            position.x.clamp(
                bounds.left(),
                (bounds.right() - px(width)).max(bounds.left()),
            ),
            position.y.clamp(
                bounds.top(),
                (bounds.bottom() - px(height)).max(bounds.top()),
            ),
        );
        let mut popup = div()
            .id(("terminal-context-menu", self.index))
            .w(px(width))
            .max_h(px(height))
            .flex()
            .flex_col()
            .py(px(4.))
            .bg(rgb(ui::RAISED))
            .border_1()
            .border_color(rgb(ui::BORDER))
            .rounded(px(3.))
            .shadow_md()
            .occlude()
            .overflow_y_scroll()
            .map(ui::preserve_scroll_axes)
            .track_scroll(&self.menu_scroll)
            .track_focus(&self.menu_focus)
            .on_any_mouse_down(|_, _, cx| cx.stop_propagation());
        for (index, (action, label, shortcut)) in ACTIONS.into_iter().enumerate() {
            let enabled = self.context_action_enabled(action);
            popup = popup.child(
                div()
                    .id(("terminal-context-action", index))
                    .h(px(28.))
                    .flex_shrink_0()
                    .px(px(8.))
                    .flex()
                    .items_center()
                    .gap(px(8.))
                    .text_size(px(11.))
                    .text_color(rgb(if enabled { ui::TEXT } else { ui::FAINT }))
                    .when(enabled && index == self.menu_index, |d| {
                        d.bg(rgb(ui::SELECTED))
                    })
                    .when(enabled, |d| {
                        d.cursor_pointer()
                            .hover(|s| s.bg(rgb(ui::SELECTED)))
                            .on_mouse_move(cx.listener(move |this, _, _, cx| {
                                if this.menu_index != index {
                                    this.menu_index = index;
                                    cx.notify();
                                }
                            }))
                    })
                    .child(div().flex_1().min_w_0().truncate().child(tr(label)))
                    .when(width >= 260., |d| {
                        d.child(
                            div()
                                .flex_shrink_0()
                                .text_size(px(9.))
                                .text_color(rgb(ui::MUTED))
                                .child(shortcut),
                        )
                    })
                    .on_click(cx.listener(move |this, _, window, cx| {
                        cx.stop_propagation();
                        this.context_menu_action(action, window, cx);
                    })),
            );
        }
        Some(
            div()
                .absolute()
                .inset_0()
                .cursor(CursorStyle::Arrow)
                .occlude()
                .on_any_mouse_down(cx.listener(|this, _, window, cx| {
                    this.close_menu(window, cx);
                    window.prevent_default();
                    cx.stop_propagation();
                }))
                .on_mouse_move(|_, _, cx| cx.stop_propagation())
                .on_scroll_wheel(|_, _, cx| cx.stop_propagation())
                .child(
                    anchored()
                        .position(position)
                        .child(ui::scrollable(popup, Some(self.menu_scroll.clone()))),
                )
                .into_any_element(),
        )
    }
}
