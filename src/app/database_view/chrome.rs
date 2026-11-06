use super::*;
use gpui::{AnyElement, IntoElement, MouseButton, div, prelude::*, px, rgb};
use rustterm::i18n::messages as i18n_key;
use ui::Icon;

impl DatabaseView {
    pub(super) fn toggle_menu(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if !self.interactive() || self.input_composing(cx) {
            return;
        }
        if self.menu_open {
            self.close_menu(window, cx);
            return;
        }
        self.menu_return_focus = window.focused(cx);
        self.menu_open = true;
        self.menu_index = usize::from(!self.available());
        self.focus.focus(window);
        cx.notify();
    }

    fn close_menu(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.menu_open = false;
        self.menu_return_focus
            .take()
            .unwrap_or_else(|| self.focus.clone())
            .focus(window);
        cx.notify();
    }

    fn menu_item(&self, index: usize) -> (&'static str, Icon, bool) {
        if index == 0 {
            (
                if self.engine() == SqlEngine::Sqlite {
                    i18n_key::DATABASE_CHECK_DATABASE_ACCESS
                } else {
                    i18n_key::DATABASE_TEST_DATABASE_CONNECTION
                },
                Icon::Pulse,
                self.available(),
            )
        } else if self.is_running() {
            (
                if self.engine() == SqlEngine::Sqlite {
                    i18n_key::DATABASE_CLOSE_DATABASE
                } else {
                    i18n_key::COMMON_DISCONNECT
                },
                Icon::Disconnect,
                self.interactive(),
            )
        } else {
            (
                if self.engine() == SqlEngine::Sqlite {
                    i18n_key::DATABASE_REOPEN_DATABASE
                } else {
                    i18n_key::COMMON_RECONNECT
                },
                Icon::Refresh,
                self.interactive(),
            )
        }
    }

    fn run_menu_action(&mut self, index: usize, window: &mut Window, cx: &mut Context<Self>) {
        if !self.menu_open || !self.menu_item(index).2 || self.input_composing(cx) {
            return;
        }
        self.close_menu(window, cx);
        if index == 0 {
            self.test_connection(cx);
        } else {
            self.disconnect_or_reconnect(window, cx);
        }
    }

    pub(super) fn menu_key_down(
        &mut self,
        event: &KeyDownEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        match event.keystroke.key.as_str() {
            "escape" => self.close_menu(window, cx),
            "up" | "down" | "tab" => {
                self.menu_index = if self.available() {
                    1 - self.menu_index
                } else {
                    1
                };
            }
            "home" => self.menu_index = usize::from(!self.available()),
            "end" => self.menu_index = 1,
            "enter" | "return" | "space" => self.run_menu_action(self.menu_index, window, cx),
            _ => {}
        }
        // The menu owns the keyboard until dismissed; editor shortcuts must not
        // execute a query behind it.
        window.prevent_default();
        cx.stop_propagation();
        cx.notify();
    }

    pub(super) fn render_menu(&self, cx: &Context<Self>) -> Option<AnyElement> {
        if !self.menu_open {
            return None;
        }
        let popup = div()
            .absolute()
            .top(px(ui::PANE_HEADER_HEIGHT))
            .right(px(8.))
            .w(px(240.))
            .max_w_full()
            .py_1()
            .bg(rgb(ui::colors().raised))
            .border_1()
            .border_color(rgb(ui::colors().border))
            .rounded(px(4.))
            .shadow_md()
            .occlude()
            .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
            .on_mouse_down(MouseButton::Right, |_, _, cx| cx.stop_propagation())
            .children((0..2).map(|index| {
                let (label, glyph, enabled) = self.menu_item(index);
                div()
                    .id(("database-menu-action", index))
                    .h(px(32.))
                    .px_3()
                    .flex()
                    .items_center()
                    .gap_2()
                    .text_color(rgb(if enabled {
                        ui::colors().text
                    } else {
                        ui::colors().faint
                    }))
                    .when(index == self.menu_index, |row| {
                        row.bg(rgb(ui::colors().selected))
                    })
                    .when(enabled, |row| {
                        row.cursor_pointer()
                            .hover(|style| style.bg(rgb(ui::colors().hover)))
                            .on_mouse_move(cx.listener(move |this, _, _, cx| {
                                if this.menu_index != index {
                                    this.menu_index = index;
                                    cx.notify();
                                }
                            }))
                    })
                    .child(ui::icon(glyph))
                    .child(div().min_w_0().truncate().child(tr(label)))
                    .on_click(cx.listener(move |this, _, window, cx| {
                        this.run_menu_action(index, window, cx);
                    }))
            }));
        Some(
            div()
                .absolute()
                .inset_0()
                .occlude()
                .on_scroll_wheel(|_, _, cx| cx.stop_propagation())
                .on_mouse_down(
                    MouseButton::Left,
                    cx.listener(|this, _, window, cx| {
                        this.close_menu(window, cx);
                        cx.stop_propagation();
                    }),
                )
                .on_mouse_down(
                    MouseButton::Right,
                    cx.listener(|this, _, window, cx| {
                        this.close_menu(window, cx);
                        cx.stop_propagation();
                    }),
                )
                .child(popup)
                .into_any_element(),
        )
    }
}
