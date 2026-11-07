use super::*;
use gpui::{AnyElement, IntoElement, MouseButton, div, prelude::*, px};
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
        self.object_menu = None;
        self.menu_scroll.scroll_to_item(0);
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
            "up" | "down" | "tab" | "home" | "end" => {
                if let Some(index) =
                    ui::popup::next_index(event, self.menu_index, &[self.available(), true])
                {
                    self.menu_index = index;
                }
            }
            "enter" | "return" | "space" => self.run_menu_action(self.menu_index, window, cx),
            _ => {}
        }
        // The menu owns the keyboard until dismissed; editor shortcuts must not
        // execute a query behind it.
        self.menu_scroll.scroll_to_item(self.menu_index);
        window.prevent_default();
        cx.stop_propagation();
        cx.notify();
    }

    pub(super) fn render_menu(&self, cx: &Context<Self>) -> Option<AnyElement> {
        if !self.menu_open {
            return None;
        }
        let bounds = ui::popup::fit(
            gpui::point(px(self.viewport_size.0 - 248.), px(ui::PANE_HEADER_HEIGHT)),
            gpui::size(
                px(240.),
                px(2. * ui::metrics::ROW + 2. * ui::popup::PADDING),
            ),
            gpui::Bounds::new(
                gpui::point(px(4.), px(4.)),
                gpui::size(
                    px((self.viewport_size.0 - 8.).max(0.)),
                    px((self.viewport_size.1 - 8.).max(0.)),
                ),
            ),
        );
        let popup = ui::popup::surface("database-actions-popup")
            .absolute()
            .left(bounds.origin.x)
            .top(bounds.origin.y)
            .w(bounds.size.width)
            .max_h(bounds.size.height)
            .overflow_y_scroll()
            .map(ui::preserve_scroll_axes)
            .track_scroll(&self.menu_scroll)
            .children((0..2).map(|index| {
                let (label, glyph, enabled) = self.menu_item(index);
                ui::popup::row(
                    ("database-menu-action", index),
                    enabled,
                    index == self.menu_index,
                )
                .when(enabled, |row| {
                    row.on_mouse_move(cx.listener(move |this, _, _, cx| {
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
                .child(ui::scrollable(popup, Some(self.menu_scroll.clone())))
                .into_any_element(),
        )
    }
}
