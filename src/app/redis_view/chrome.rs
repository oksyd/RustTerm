use super::*;
use crate::app::ui::{
    ButtonStyle, Icon, icon, icon_button, icon_button_enabled, text_button_enabled,
};
use gpui::{AnyElement, MouseButton, Pixels, Point, div, prelude::*, px, rgb, uniform_list};
use rustterm::i18n::messages as i18n_key;

#[derive(Clone, Copy)]
enum MenuAction {
    Section(operations::Section),
    Browser,
    Connection,
    Refresh,
    CopyKey,
    Rename,
    Ttl,
    DeleteKey,
    CopyEntry,
    EditEntry,
    DeleteEntry,
}

impl RedisView {
    pub(super) fn render_browser(&self, cx: &mut Context<Self>) -> AnyElement {
        let enabled = self.enabled();
        let mut browser = div()
            .w(px(self.browser_width()))
            .when(self.compact, |d| d.flex_1())
            .min_w_0()
            .flex_shrink_0()
            .min_h_0()
            .flex()
            .flex_col()
            .bg(rgb(ui::colors().surface))
            .child(
                div()
                    .h(px(40.))
                    .px(px(12.))
                    .flex_shrink_0()
                    .flex()
                    .items_center()
                    .gap(px(6.))
                    .child(div().child(tr(i18n_key::REDIS_KEYS)))
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .overflow_hidden()
                            .whitespace_nowrap()
                            .text_ellipsis()
                            .text_size(px(11.))
                            .text_color(rgb(ui::colors().muted))
                            .child(rustterm::trf!(
                                i18n_key::REDIS_BATCH_COUNT,
                                count = self.keys.len()
                            )),
                    )
                    .child(
                        icon_button_enabled(
                            "redis-refresh-keys",
                            Icon::Refresh,
                            self.disabled_reason()
                                .unwrap_or(i18n_key::REDIS_REFRESH_KEYS),
                            enabled,
                        )
                        .on_click(cx.listener(|this, _, _, cx| this.refresh_keys(cx))),
                    )
                    .child(
                        icon_button_enabled(
                            "redis-create",
                            Icon::Plus,
                            self.disabled_reason().unwrap_or(i18n_key::REDIS_CREATE_KEY),
                            enabled,
                        )
                        .on_click(cx.listener(|this, _, window, cx| this.create_key(window, cx))),
                    )
                    .when(self.compact && self.selected_key.is_some(), |d| {
                        d.child(
                            icon_button(
                                "redis-hide-keys",
                                Icon::ChevronRight,
                                i18n_key::REDIS_SHOW_VALUE,
                            )
                            .tab_index(0)
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.browser_open = false;
                                cx.notify();
                            })),
                        )
                    }),
            )
            .child(
                div()
                    .h(px(36.))
                    .px(px(6.))
                    .flex_shrink_0()
                    .flex()
                    .items_center()
                    .gap(px(4.))
                    .child(div().flex_1().min_w_0().child(self.filter_input.clone()))
                    .child(
                        icon_button_enabled(
                            "redis-scan",
                            Icon::Search,
                            self.disabled_reason()
                                .unwrap_or(i18n_key::REDIS_SCAN_KEYS_ENTER),
                            enabled,
                        )
                        .focus(|style| {
                            style
                                .bg(rgb(ui::colors().selected))
                                .text_color(rgb(ui::colors().text))
                        })
                        .on_click(cx.listener(|this, _, _, cx| this.search(cx))),
                    ),
            );
        if self.keys.is_empty() {
            browser = browser.child(
                ui::empty_state(
                    Icon::Database,
                    tr(if self.busy.is_some() {
                        i18n_key::REDIS_SCANNING_KEYS
                    } else if self.scanned && self.scan_next != 0 {
                        i18n_key::REDIS_NO_MATCHES_IN_THIS_BATCH_CONTINUE_SCANNING
                    } else if self.scanned {
                        i18n_key::REDIS_NO_KEYS_IN_THIS_BATCH
                    } else {
                        i18n_key::REDIS_CONNECT_TO_BROWSE_KEYS
                    }),
                )
                .flex_1(),
            );
        } else {
            browser = browser.child(
                div()
                    .relative()
                    .flex_1()
                    .min_h_0()
                    .min_w_0()
                    .flex()
                    .flex_col()
                    .pr(px(ui::SCROLLBAR_WIDTH))
                    .child(
                        uniform_list(
                            "redis-keys",
                            self.keys.len(),
                            cx.processor(|this, range: std::ops::Range<usize>, _, cx| {
                                range
                                    .map(|index| this.render_key(index, cx))
                                    .collect::<Vec<_>>()
                            }),
                        )
                        .map(ui::preserve_scroll_axes)
                        .track_scroll(self.key_scroll.clone())
                        .w_full()
                        .min_w_0()
                        .flex_1()
                        .min_h_0(),
                    )
                    .child(ui::scrollbars(
                        "redis-key-bars",
                        None,
                        Some(self.key_scroll.0.borrow().base_handle.clone()),
                    )),
            );
        }
        browser
            .child(
                div()
                    .h(px(34.))
                    .px(px(6.))
                    .flex_shrink_0()
                    .flex()
                    .items_center()
                    .justify_between()
                    .border_t_1()
                    .border_color(rgb(ui::colors().border))
                    .child(
                        text_button_enabled(
                            "redis-prev-keys",
                            i18n_key::COMMON_PREVIOUS,
                            ButtonStyle::Neutral,
                            enabled && !self.scan_history.is_empty(),
                        )
                        .when(!enabled || self.scan_history.is_empty(), |button| {
                            let reason = self
                                .disabled_reason()
                                .unwrap_or(i18n_key::COMMON_ALREADY_AT_THE_FIRST_BATCH);
                            button.tooltip(move |_, cx| {
                                cx.new(|_| ui::Tooltip(tr(reason).into())).into()
                            })
                        })
                        .focus(|style| {
                            style
                                .bg(rgb(ui::colors().selected))
                                .text_color(rgb(ui::colors().text))
                        })
                        .on_click(cx.listener(|this, _, _, cx| this.page_keys(false, cx))),
                    )
                    .child(
                        text_button_enabled(
                            "redis-next-keys",
                            i18n_key::REDIS_CONTINUE_SCAN,
                            ButtonStyle::Neutral,
                            enabled && self.scan_next != 0,
                        )
                        .when(!enabled || self.scan_next == 0, |button| {
                            let reason = self.disabled_reason().unwrap_or(if self.scanned {
                                i18n_key::COMMON_THE_KEY_SCAN_IS_COMPLETE
                            } else {
                                i18n_key::COMMON_SCAN_KEYS_FIRST
                            });
                            button.tooltip(move |_, cx| {
                                cx.new(|_| ui::Tooltip(tr(reason).into())).into()
                            })
                        })
                        .focus(|style| {
                            style
                                .bg(rgb(ui::colors().selected))
                                .text_color(rgb(ui::colors().text))
                        })
                        .on_click(cx.listener(|this, _, _, cx| this.page_keys(true, cx))),
                    ),
            )
            .into_any_element()
    }

    fn render_key(&self, index: usize, cx: &mut Context<Self>) -> AnyElement {
        let key = &self.keys[index];
        let selected = self.selected_key.as_ref() == Some(key);
        div()
            .id(("redis-key", index))
            .when(
                selected || (index == 0 && self.selected_key.is_none()),
                |d| d.tab_index(0),
            )
            .focus(|style| {
                style
                    .bg(rgb(ui::colors().selected))
                    .text_color(rgb(ui::colors().text))
            })
            .h(px(34.))
            .w_full()
            .min_w_0()
            .px(px(14.))
            .flex()
            .items_center()
            .gap(px(6.))
            .cursor_pointer()
            .border_l_2()
            .border_color(rgb(if selected {
                ui::colors().accent
            } else {
                ui::colors().surface
            }))
            .when(selected, |d| d.bg(rgb(ui::colors().selected)))
            .hover(|d| d.bg(rgb(ui::colors().hover)))
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .font_family(ui::MONOSPACE_FONT)
                    .text_size(px(13.))
                    .overflow_hidden()
                    .whitespace_nowrap()
                    .text_ellipsis()
                    .child(if key.is_empty() {
                        "\"\"".into()
                    } else {
                        value::label(key)
                    }),
            )
            .when(selected, |d| {
                d.when_some(self.info.as_ref(), |d, info| {
                    d.child(
                        div()
                            .flex_shrink_0()
                            .text_right()
                            .text_size(px(9.))
                            .text_color(rgb(ui::colors().accent))
                            .child(super::render::kind_label(&info.kind).to_ascii_uppercase()),
                    )
                })
            })
            .on_click(cx.listener(move |this, _, window, cx| this.select_key(index, window, cx)))
            .on_key_down(cx.listener(move |this, event: &KeyDownEvent, window, cx| {
                if matches!(
                    event.keystroke.key.as_str(),
                    "up" | "down" | "home" | "end" | "f2" | "delete"
                ) {
                    this.navigation = Navigation::Keys;
                    this.focus.focus(window);
                    this.key_down(event, window, cx);
                }
            }))
            .on_mouse_down(
                MouseButton::Right,
                cx.listener(move |this, event: &gpui::MouseDownEvent, window, cx| {
                    if this.available(cx) {
                        this.navigation = Navigation::Keys;
                        this.focus.focus(window);
                        if this.selected_key.as_ref() != this.keys.get(index) {
                            this.select_key(index, window, cx);
                        }
                        this.open_context_menu(RedisMenu::Key, event.position, window, cx);
                    }
                    cx.stop_propagation();
                }),
            )
            .into_any_element()
    }

    pub(super) fn browser_width(&self) -> f32 {
        let available = self.viewport_size.0;
        self.browser_width.min((available - 340.).max(180.))
    }

    pub(super) fn render_header(&self, cx: &mut Context<Self>) -> AnyElement {
        let section_label = self.ops.section.label();
        let host = if self.config.host.contains(':') && !self.config.host.starts_with('[') {
            format!("[{}]", self.config.host)
        } else {
            self.config.host.clone()
        };
        let address = format!(
            "{}://{host}:{}",
            if self.config.tls { "rediss" } else { "redis" },
            self.config.port
        );
        let mut detail = format!(
            "{}\n{}",
            self.title,
            tr(if self.config.tls {
                i18n_key::REDIS_TLS_CONNECTION
            } else if self.ssh_route.is_some() {
                i18n_key::REDIS_SSH_TUNNEL
            } else {
                i18n_key::REDIS_DIRECT_CONNECTION
            })
        );
        if let Some(host) = &self.ssh_route {
            detail.push('\n');
            detail.push_str(&rustterm::trf!(i18n_key::REDIS_VIA_SSH_HOST, host = host));
        }
        ui::panel_header()
            .min_w_0()
            .child(ui::connection_address(
                "redis-address",
                Icon::Redis,
                "Redis",
                address,
                detail,
            ))
            .child(
                div()
                    .flex()
                    .flex_shrink_0()
                    .items_center()
                    .gap(px(4.))
                    .text_size(px(11.))
                    .text_color(rgb(ui::colors().muted))
                    .child(tr(i18n_key::REDIS_DB))
                    .child(div().w(px(38.)).child(self.database_input.clone())),
            )
            .child(
                text_button_enabled(
                    "redis-db-go",
                    i18n_key::COMMON_GO,
                    ButtonStyle::Neutral,
                    self.enabled(),
                )
                .when(!self.enabled(), |button| {
                    let reason = self
                        .disabled_reason()
                        .unwrap_or(i18n_key::COMMON_CONNECT_TO_REDIS_TO_USE_THIS_ACTION);
                    button.tooltip(move |_, cx| cx.new(|_| ui::Tooltip(tr(reason).into())).into())
                })
                .flex_shrink_0()
                .whitespace_nowrap()
                .on_click(cx.listener(|this, _, _, cx| this.change_database(cx))),
            )
            .child(
                div()
                    .id("redis-console-shortcut")
                    .tab_index(0)
                    .h(px(28.))
                    .px(px(8.))
                    .flex_shrink_0()
                    .flex()
                    .items_center()
                    .gap(px(6.))
                    .rounded(px(3.))
                    .text_size(px(12.))
                    .cursor_pointer()
                    .hover(|d| d.bg(rgb(ui::colors().hover)))
                    .when(
                        self.console_open && self.ops.section == operations::Section::Keys,
                        |d| {
                            d.bg(rgb(ui::colors().selected))
                                .text_color(rgb(ui::colors().accent))
                        },
                    )
                    .tooltip(|_, cx| {
                        cx.new(|_| ui::Tooltip(tr(i18n_key::REDIS_TOGGLE_REDIS_CONSOLE).to_owned()))
                            .into()
                    })
                    .child(icon(Icon::Terminal))
                    .when(!self.compact, |d| {
                        d.child(tr(i18n_key::REDIS_REDIS_CONSOLE))
                    })
                    .on_click(cx.listener(|this, _, window, cx| this.toggle_console(window, cx))),
            )
            .child(
                div()
                    .id("redis-section-menu")
                    .tab_index(0)
                    .h(px(28.))
                    .max_w(px(if self.compact { 72. } else { 150. }))
                    .flex_shrink_0()
                    .px(px(8.))
                    .flex()
                    .items_center()
                    .gap(px(6.))
                    .rounded(px(3.))
                    .text_size(px(12.))
                    .cursor_pointer()
                    .hover(|d| d.bg(rgb(ui::colors().hover)))
                    .when(self.menu == Some(RedisMenu::Section), |d| {
                        d.bg(rgb(ui::colors().selected))
                    })
                    .tooltip(move |_, cx| {
                        cx.new(|_| {
                            ui::Tooltip(format!(
                                "{} · {}",
                                tr(i18n_key::REDIS_REDIS_VIEWS),
                                tr(section_label)
                            ))
                        })
                        .into()
                    })
                    .child(
                        div()
                            .min_w_0()
                            .truncate()
                            .child(tr(self.ops.section.label())),
                    )
                    .child(icon(Icon::ChevronDown))
                    .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
                    .on_click(cx.listener(|this, _, window, cx| {
                        this.toggle_menu(RedisMenu::Section, window, cx)
                    })),
            )
            .child(
                icon_button(
                    "redis-pane-menu",
                    Icon::More,
                    i18n_key::REDIS_REDIS_PANE_ACTIONS,
                )
                .tab_index(0)
                .when(self.menu == Some(RedisMenu::Pane), |d| {
                    d.bg(rgb(ui::colors().selected))
                })
                .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
                .on_click(
                    cx.listener(|this, _, window, cx| {
                        this.toggle_menu(RedisMenu::Pane, window, cx)
                    }),
                ),
            )
            .when(self.pane_close_visible, |header| {
                header.child(
                    icon_button(
                        "redis-close",
                        Icon::Close,
                        i18n_key::TERMINAL_CLOSE_PANE_CTRL_SHIFT_W,
                    )
                    .tab_index(0)
                    .on_click(cx.listener(|this, _, _, cx| cx.emit(PaneEvent::Close(this.index)))),
                )
            })
            .into_any_element()
    }

    pub(super) fn toggle_menu(
        &mut self,
        menu: RedisMenu,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.menu == Some(menu) {
            self.close_menu(window, cx);
            return;
        }
        if self.menu.is_none() {
            self.menu_return_focus = window.focused(cx);
        }
        self.menu = Some(menu);
        self.menu_position = None;
        self.menu_index = self
            .menu_actions(menu)
            .into_iter()
            .position(|action| self.menu_item(action).2)
            .unwrap_or(0);
        self.menu_scroll.scroll_to_item(self.menu_index);
        self.focus.focus(window);
        cx.notify();
    }

    pub(super) fn render_menu(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let menu = self.menu?;
        let actions = self.menu_actions(menu);
        let top = if matches!(menu, RedisMenu::Pane | RedisMenu::Section) {
            ui::PANE_HEADER_HEIGHT - 2.
        } else {
            ui::PANE_HEADER_HEIGHT + if self.compact { 40. } else { 48. } - 2.
        };
        let bounds = ui::popup::fit(
            self.menu_position
                .unwrap_or(gpui::point(px(self.viewport_size.0 - 218.), px(top))),
            gpui::size(
                px(210.),
                px(actions.len() as f32 * ui::metrics::ROW + 2. * ui::popup::PADDING),
            ),
            gpui::Bounds::new(
                gpui::point(px(4.), px(4.)),
                gpui::size(
                    px((self.viewport_size.0 - 8.).max(0.)),
                    px((self.viewport_size.1 - 8.).max(0.)),
                ),
            ),
        );
        let popup = ui::popup::surface("redis-actions-popup")
            .absolute()
            .left(bounds.origin.x)
            .top(bounds.origin.y)
            .w(bounds.size.width)
            .max_h(bounds.size.height)
            .overflow_y_scroll()
            .map(ui::preserve_scroll_axes)
            .track_scroll(&self.menu_scroll)
            .children(actions.into_iter().enumerate().map(|(index, action)| {
                let (label, glyph, enabled, danger) = self.menu_item(action);
                ui::popup::row(
                    ("redis-menu-action", index),
                    enabled,
                    index == self.menu_index,
                )
                .when(danger && enabled, |row| {
                    row.text_color(rgb(ui::colors().danger))
                })
                .on_mouse_move(cx.listener(move |this, _, _, cx| {
                    if enabled && this.menu_index != index {
                        this.menu_index = index;
                        cx.notify();
                    }
                }))
                .when(!enabled, |button| {
                    let reason = self.disabled_reason().unwrap_or(match action {
                        MenuAction::CopyEntry | MenuAction::EditEntry | MenuAction::DeleteEntry => {
                            i18n_key::COMMON_SELECT_AN_AVAILABLE_ENTRY_FIRST
                        }
                        _ => i18n_key::COMMON_SELECT_AN_EXISTING_KEY_FIRST,
                    });
                    button.tooltip(move |_, cx| cx.new(|_| ui::Tooltip(tr(reason).into())).into())
                })
                .child(icon(glyph).text_color(rgb(if danger && enabled {
                    ui::colors().danger
                } else {
                    ui::colors().muted
                })))
                .child(tr(label))
                .on_click(cx.listener(move |this, _, window, cx| {
                    this.run_menu_action(index, window, cx);
                }))
            }));
        Some(
            div()
                .absolute()
                .top_0()
                .left_0()
                .right_0()
                .bottom_0()
                .occlude()
                .on_scroll_wheel(|_, _, cx| cx.stop_propagation())
                .on_mouse_down(
                    MouseButton::Left,
                    cx.listener(|this, _, window, cx| {
                        this.close_menu(window, cx);
                        cx.stop_propagation();
                    }),
                )
                .child(ui::scrollable(popup, Some(self.menu_scroll.clone())))
                .into_any_element(),
        )
    }

    pub(super) fn open_context_menu(
        &mut self,
        menu: RedisMenu,
        position: Point<Pixels>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.menu = None;
        self.toggle_menu(menu, window, cx);
        self.menu_position = self.bounds.map(|bounds| position - bounds.origin);
    }

    pub(super) fn entry_editable(&self) -> bool {
        self.selected_row.is_some()
            && self.info.as_ref().is_some_and(|info| {
                matches!(info.kind, KeyType::Hash | KeyType::List | KeyType::ZSet)
            })
    }

    pub(super) fn entry_removable(&self) -> bool {
        self.selected_row.is_some()
            && self.info.as_ref().is_some_and(|info| {
                matches!(
                    info.kind,
                    KeyType::Hash | KeyType::Set | KeyType::ZSet | KeyType::Stream
                )
            })
    }

    pub(super) fn entry_copyable(&self) -> bool {
        self.selected_row.is_some()
            && self.info.as_ref().is_some_and(|info| {
                matches!(
                    info.kind,
                    KeyType::Hash | KeyType::List | KeyType::Set | KeyType::ZSet
                )
            })
    }

    fn menu_actions(&self, menu: RedisMenu) -> Vec<MenuAction> {
        match menu {
            RedisMenu::Section => vec![
                MenuAction::Section(operations::Section::Keys),
                MenuAction::Section(operations::Section::Server),
                MenuAction::Section(operations::Section::SlowLog),
                MenuAction::Section(operations::Section::PubSub),
            ],
            RedisMenu::Pane => vec![MenuAction::Browser, MenuAction::Connection],
            RedisMenu::Key => {
                let mut actions = vec![MenuAction::CopyKey];
                // Right-click keeps actions local to the chosen key. The header
                // overflow already sits beside the visible refresh button.
                if self.menu_position.is_some() {
                    actions.push(MenuAction::Refresh);
                }
                actions.extend([MenuAction::Rename, MenuAction::Ttl, MenuAction::DeleteKey]);
                actions
            }
            RedisMenu::Entry => vec![
                MenuAction::CopyEntry,
                MenuAction::EditEntry,
                MenuAction::DeleteEntry,
            ],
        }
    }

    fn menu_item(&self, action: MenuAction) -> (&'static str, Icon, bool, bool) {
        let exists = self
            .info
            .as_ref()
            .is_some_and(|info| !matches!(info.kind, KeyType::Missing));
        let enabled = self.enabled();
        match action {
            MenuAction::Section(section) => (
                section.label(),
                if self.ops.section == section {
                    Icon::Check
                } else {
                    Icon::Database
                },
                self.can_select_section(),
                false,
            ),
            MenuAction::Browser => (
                i18n_key::REDIS_TOGGLE_KEY_BROWSER,
                Icon::Key,
                self.editor.is_none(),
                false,
            ),
            MenuAction::Connection => (
                if self.is_running() {
                    i18n_key::COMMON_DISCONNECT
                } else {
                    i18n_key::COMMON_RECONNECT
                },
                Icon::Disconnect,
                self.editor.is_none(),
                false,
            ),
            MenuAction::CopyKey => (
                i18n_key::REDIS_COPY_KEY_NAME,
                Icon::Copy,
                enabled && self.selected_key.is_some(),
                false,
            ),
            MenuAction::Refresh => (i18n_key::REDIS_REFRESH_VALUE, Icon::Refresh, enabled, false),
            MenuAction::Rename => (
                i18n_key::REDIS_RENAME_KEY,
                Icon::Edit,
                enabled && exists,
                false,
            ),
            MenuAction::Ttl => (
                i18n_key::REDIS_SET_TTL,
                Icon::Edit,
                enabled && exists,
                false,
            ),
            MenuAction::DeleteKey => (
                i18n_key::REDIS_DELETE_KEY,
                Icon::Trash,
                enabled && exists,
                true,
            ),
            MenuAction::CopyEntry => (
                i18n_key::REDIS_COPY_VALUE,
                Icon::Copy,
                enabled && self.entry_copyable(),
                false,
            ),
            MenuAction::EditEntry => (
                i18n_key::REDIS_EDIT_ENTRY,
                Icon::Edit,
                enabled && self.entry_editable(),
                false,
            ),
            MenuAction::DeleteEntry => (
                i18n_key::REDIS_DELETE_ENTRY,
                Icon::Trash,
                enabled && self.entry_removable(),
                true,
            ),
        }
    }

    fn run_menu_action(&mut self, index: usize, window: &mut Window, cx: &mut Context<Self>) {
        if self.window_state.is_locked() {
            return;
        }
        let Some(menu) = self.menu else {
            return;
        };
        let Some(action) = self.menu_actions(menu).get(index).copied() else {
            return;
        };
        if !self.menu_item(action).2 {
            return;
        }
        self.close_menu(window, cx);
        match action {
            MenuAction::Section(section) => self.select_section(section, window, cx),
            MenuAction::Browser => {
                self.console_open = false;
                let returning_to_keys = self.ops.section != operations::Section::Keys;
                self.ops.section = operations::Section::Keys;
                if self.compact {
                    self.browser_open = returning_to_keys || !self.browser_open;
                } else {
                    self.browser_collapsed = !returning_to_keys && !self.browser_collapsed;
                }
            }
            MenuAction::Connection => self.disconnect_or_reconnect(cx),
            MenuAction::Refresh => self.refresh_value(cx),
            MenuAction::Rename => self.rename_key(window, cx),
            MenuAction::Ttl => self.edit_ttl(window, cx),
            MenuAction::EditEntry => self.edit_value(false, window, cx),
            MenuAction::CopyEntry => self.copy_value(cx),
            MenuAction::DeleteEntry => self.delete_row(window, cx),
            MenuAction::DeleteKey => self.delete_key(window, cx),
            MenuAction::CopyKey => {
                if let Some(key) = self
                    .selected_key
                    .as_ref()
                    .and_then(|key| std::str::from_utf8(key).ok())
                {
                    cx.write_to_clipboard(ClipboardItem::new_string(key.to_owned()));
                    self.show_success(i18n_key::REDIS_KEY_NAME_COPIED, cx);
                } else {
                    self.error =
                        Some(i18n_key::REDIS_BINARY_VALUES_CANNOT_BE_COPIED_AS_TEXT.into());
                }
            }
        }
        cx.notify();
    }

    fn close_menu(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.menu = None;
        self.menu_position = None;
        self.menu_return_focus
            .take()
            .unwrap_or_else(|| self.focus.clone())
            .focus(window);
        cx.notify();
    }

    pub(super) fn menu_key_down(
        &mut self,
        event: &KeyDownEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(menu) = self.menu else {
            return;
        };
        let enabled: Vec<_> = self
            .menu_actions(menu)
            .into_iter()
            .map(|action| self.menu_item(action).2)
            .collect();
        match event.keystroke.key.as_str() {
            "escape" => self.close_menu(window, cx),
            "up" | "down" | "tab" | "home" | "end" => {
                if let Some(index) = ui::popup::next_index(event, self.menu_index, &enabled) {
                    self.menu_index = index;
                }
            }
            "enter" | "return" | "space" => self.run_menu_action(self.menu_index, window, cx),
            _ => return,
        }
        self.menu_scroll.scroll_to_item(self.menu_index);
        cx.notify();
        cx.stop_propagation();
        window.prevent_default();
    }
}
