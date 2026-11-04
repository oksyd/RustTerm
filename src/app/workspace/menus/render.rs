use super::{Menu, MenuAction};
use crate::app::ui::{Icon, icon};
use crate::app::{pane::Pane, ui, window_frame::WindowFrame, workspace::Workspace};
use gpui::{
    AnyElement, Bounds, Context, MouseButton, Pixels, Point, Size, Window, div, point, prelude::*,
    px, rgb, size,
};
use rustterm::i18n::messages as i18n_key;
use rustterm::i18n::tr;

const ROW_HEIGHT: f32 = 30.;
const HEADER_HEIGHT: f32 = 32.;
const MENU_PADDING: f32 = 5.; // Four pixels of padding and the one-pixel border.
const MENU_GAP: f32 = 4.;

impl Workspace {
    pub(in crate::app::workspace) fn render_menu(
        &self,
        window: &Window,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        let menu = self.menu?;
        let parent = if menu == Menu::NewConnection {
            Menu::File
        } else {
            menu
        };
        let content = WindowFrame::new(window).content_bounds(window.viewport_size());
        let limits = Bounds::new(
            content.origin + point(px(MENU_GAP), px(ui::TITLE_HEIGHT + MENU_GAP)),
            size(
                (content.size.width - px(2. * MENU_GAP)).max(px(1.)),
                (content.size.height - px(ui::TITLE_HEIGHT + 2. * MENU_GAP)).max(px(1.)),
            ),
        );
        // Measure the expanded categories before displaying the application popup.
        let anchor = if let Some(index) = menu.category_index() {
            self.application_menu_bounds[index]?.bottom_left()
        } else {
            self.menu_anchor
        };
        let parent_bounds = fit_popup(anchor, self.menu_size(parent, limits.size), limits);
        let mut backdrop = div()
            .absolute()
            .top(px(ui::TITLE_HEIGHT))
            .left_0()
            .right_0()
            .bottom_0()
            .occlude();
        for button in [MouseButton::Left, MouseButton::Right, MouseButton::Middle] {
            backdrop = backdrop
                .on_mouse_down(
                    button,
                    cx.listener(|this, _, window, cx| {
                        this.menu = None;
                        this.restore_popup_focus(window, cx);
                        window.prevent_default();
                        cx.stop_propagation();
                        cx.notify();
                    }),
                )
                .on_mouse_up(button, |_, _, cx| cx.stop_propagation());
        }
        backdrop = backdrop.child(self.render_menu_popup(parent, parent_bounds, content, cx));
        if menu == Menu::NewConnection {
            let index = Menu::File
                .items()
                .iter()
                .position(|item| matches!(item.action, MenuAction::NewConnection))
                .expect("File contains the connection submenu");
            let row_y = parent_bounds.top()
                + px(MENU_PADDING + index as f32 * ROW_HEIGHT)
                + self.menu_scroll.offset().y;
            let child_bounds = place_submenu(
                parent_bounds,
                row_y,
                self.menu_size(menu, limits.size),
                limits,
            );
            backdrop = backdrop.child(self.render_menu_popup(menu, child_bounds, content, cx));
        }
        Some(backdrop.into_any_element())
    }

    fn menu_size(&self, menu: Menu, available: Size<Pixels>) -> Size<Pixels> {
        let width = match menu {
            Menu::File => 320.,
            Menu::Create | Menu::NewConnection | Menu::Profile(_) => 300.,
            _ => 380.,
        };
        let header = if menu.profile(&self.config.profiles).is_some() {
            HEADER_HEIGHT
        } else {
            0.
        };
        size(
            px(width).min(available.width),
            px(menu.items().len() as f32 * ROW_HEIGHT + 2. * MENU_PADDING + header)
                .min(available.height),
        )
    }

    fn render_menu_popup(
        &self,
        menu: Menu,
        bounds: Bounds<Pixels>,
        content: Bounds<Pixels>,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let child = menu == Menu::NewConnection;
        let active = self.menu == Some(menu);
        let scroll = if child {
            &self.submenu_scroll
        } else {
            &self.menu_scroll
        };
        let mut popup = div()
            .id(if child {
                "connection-submenu"
            } else {
                "workspace-menu"
            })
            .absolute()
            .left(bounds.left() - content.left())
            .top(bounds.top() - content.top() - px(ui::TITLE_HEIGHT))
            .w(bounds.size.width)
            .h(bounds.size.height)
            .flex()
            .flex_col()
            .overflow_hidden()
            .py(px(MENU_PADDING - 1.))
            .bg(rgb(ui::RAISED))
            .border_1()
            .border_color(rgb(ui::BORDER))
            .rounded(px(3.))
            .shadow_md()
            .occlude()
            .when(active, |d| d.track_focus(&self.menu_focus));
        for button in [MouseButton::Left, MouseButton::Right, MouseButton::Middle] {
            popup = popup
                .on_mouse_down(button, |_, window, cx| {
                    window.prevent_default();
                    cx.stop_propagation();
                })
                .on_mouse_up(button, |_, _, cx| cx.stop_propagation());
        }
        if let Some(profile) = menu.profile(&self.config.profiles) {
            popup = popup.child(
                div()
                    .px_3()
                    .h(px(HEADER_HEIGHT))
                    .flex()
                    .items_center()
                    .flex_shrink_0()
                    .min_w_0()
                    .text_size(px(11.))
                    .text_color(rgb(ui::MUTED))
                    .border_b_1()
                    .border_color(rgb(ui::BORDER))
                    .child(div().truncate().child(profile.name.clone())),
            );
        }
        let mut items = div()
            .id(if child {
                "connection-submenu-items"
            } else {
                "workspace-menu-items"
            })
            .flex_1()
            .min_h_0()
            .overflow_y_scroll()
            .map(ui::preserve_scroll_axes)
            .track_scroll(scroll);
        for (index, item) in menu.items().iter().enumerate() {
            let action = item.action;
            let submenu = matches!(action, MenuAction::NewConnection);
            let enabled = self.action_enabled(action, cx);
            let selected = if active {
                index == self.menu_index
            } else {
                submenu
            };
            let label = match (action, self.panes.get(&self.focused)) {
                (MenuAction::Copy, Some(Pane::Terminal(_))) => {
                    i18n_key::WORKSPACE_COPY_TERMINAL_SELECTION
                }
                (MenuAction::Paste, Some(Pane::Terminal(_))) => {
                    i18n_key::WORKSPACE_PASTE_INTO_TERMINAL
                }
                (MenuAction::Copy, Some(Pane::Database(_))) => {
                    i18n_key::WORKSPACE_COPY_SQL_SELECTION_OR_RESULTS
                }
                (MenuAction::Paste, Some(Pane::Database(_))) => {
                    i18n_key::WORKSPACE_PASTE_INTO_SQL_EDITOR
                }
                _ => item.label,
            };
            let checked = match action {
                MenuAction::LineNumbers => self.config.settings.line_numbers,
                MenuAction::LineTimestamps => self.config.settings.line_timestamps,
                MenuAction::Highlight => self.config.settings.highlight,
                MenuAction::Sidebar => self.sidebar_visible,
                MenuAction::Zoom => self.zoomed.is_some(),
                _ => false,
            };
            items = items.child(
                div()
                    .id((if child { "submenu-item" } else { "menu-item" }, index))
                    .h(px(ROW_HEIGHT))
                    .px_3()
                    .flex()
                    .items_center()
                    .gap_2()
                    .text_color(rgb(if !enabled {
                        ui::FAINT
                    } else if matches!(action, MenuAction::DeleteProfile) {
                        ui::DANGER
                    } else {
                        ui::TEXT
                    }))
                    .when(matches!(action, MenuAction::DeleteProfile), |d| {
                        d.border_t_1().border_color(rgb(ui::BORDER))
                    })
                    .when(selected, |d| d.bg(rgb(ui::SELECTED)))
                    .when(enabled, |d| {
                        d.cursor_pointer()
                            .hover(|style| style.bg(rgb(ui::SELECTED)))
                            .on_mouse_move(cx.listener(move |this, _, window, cx| {
                                if this.menu == Some(menu) {
                                    if submenu {
                                        this.menu_action(action, window, cx);
                                    } else if this.menu_index != index {
                                        this.menu_index = index;
                                        cx.notify();
                                    }
                                }
                            }))
                    })
                    .child(
                        div()
                            .w(px(14.))
                            .flex_shrink_0()
                            .when(checked, |d| d.child(icon(Icon::Check))),
                    )
                    .child(div().flex_1().min_w_0().truncate().child(tr(label)))
                    .when(submenu, |d| d.child(icon(Icon::ChevronRight).size(px(12.))))
                    .when(!submenu && !item.shortcut.is_empty(), |d| {
                        d.child(
                            div()
                                .text_size(px(10.))
                                .flex_shrink_0()
                                .text_color(rgb(ui::MUTED))
                                .child(item.shortcut),
                        )
                    })
                    .on_click(cx.listener(move |this, _, window, cx| {
                        if this.menu == Some(menu)
                            || (menu == Menu::File && this.menu == Some(Menu::NewConnection))
                        {
                            this.menu_action(action, window, cx);
                        }
                        cx.stop_propagation();
                    })),
            );
        }
        popup
            .child(ui::scrollable(items, Some(scroll.clone())))
            .into_any_element()
    }
}

fn fit_popup(anchor: Point<Pixels>, size: Size<Pixels>, limits: Bounds<Pixels>) -> Bounds<Pixels> {
    Bounds::new(
        point(
            anchor.x.clamp(
                limits.left(),
                (limits.right() - size.width).max(limits.left()),
            ),
            anchor.y.clamp(
                limits.top(),
                (limits.bottom() - size.height).max(limits.top()),
            ),
        ),
        size,
    )
}

fn place_submenu(
    parent: Bounds<Pixels>,
    row_y: Pixels,
    size: Size<Pixels>,
    limits: Bounds<Pixels>,
) -> Bounds<Pixels> {
    let right = parent.right() + px(MENU_GAP);
    let x = if right + size.width <= limits.right() {
        right
    } else {
        parent.left() - px(MENU_GAP) - size.width
    };
    fit_popup(point(x, row_y - px(MENU_PADDING)), size, limits)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn popups_stay_inside_content_and_submenus_flip_left() {
        let limits = Bounds::new(point(px(15.), px(47.)), size(px(670.), px(380.)));
        let parent = fit_popup(point(px(15.), px(47.)), size(px(320.), px(220.)), limits);
        assert_eq!(parent.origin, limits.origin);
        let child = place_submenu(parent, px(82.), size(px(300.), px(250.)), limits);
        assert_eq!(child.left(), parent.right() + px(MENU_GAP));
        assert_eq!(child.top(), px(77.));
        assert!(child.right() <= limits.right());
        let parent = fit_popup(point(px(680.), px(420.)), parent.size, limits);
        let child = place_submenu(parent, parent.bottom(), child.size, limits);
        assert_eq!(child.right(), parent.left() - px(MENU_GAP));
        assert_eq!(child.bottom(), limits.bottom());
        assert!(child.left() >= limits.left());
    }
}
