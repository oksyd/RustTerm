use super::*;
use crate::app::ui::{
    ButtonStyle, Icon, icon, icon_button, icon_button_enabled, text_button, text_button_enabled,
};
use gpui::{
    AnyElement, CursorStyle, IntoElement, MouseButton, MouseMoveEvent, Render, div, prelude::*, px,
    rgb, uniform_list,
};
use rustterm::i18n::messages as i18n_key;

impl Render for RedisView {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let entity = cx.entity();
        let header = self.render_header(cx);
        let show_browser = if self.compact {
            self.editor.is_none() && (self.browser_open || self.selected_key.is_none())
        } else {
            !self.browser_collapsed
        };
        let show_detail = !self.compact || !show_browser;
        let mut body = div().flex_1().min_h_0().min_w_0().flex();
        if show_browser {
            body = body.child(self.render_browser(cx));
            if !self.compact {
                body = body.child(
                    div()
                        .id("redis-browser-divider")
                        .w(px(4.))
                        .h_full()
                        .flex_shrink_0()
                        .cursor(CursorStyle::ResizeLeftRight)
                        .bg(rgb(ui::colors().border))
                        .hover(|s| s.bg(rgb(ui::colors().accent)))
                        .on_mouse_down(
                            MouseButton::Left,
                            cx.listener(|this, _, _, cx| {
                                this.resizing_browser = true;
                                cx.stop_propagation();
                            }),
                        ),
                );
            }
        }
        if show_detail {
            body = body.child(self.render_detail(cx));
        }
        let console = self.render_console(cx);
        let mut panel = div()
            .id(("redis-pane", self.index))
            .size_full()
            .min_w_0()
            .min_h_0()
            .relative()
            .overflow_hidden()
            .flex()
            .flex_col()
            .bg(rgb(ui::colors().bg))
            .text_color(rgb(ui::colors().text))
            .text_size(px(13.))
            .track_focus(&self.focus)
            .on_key_down(cx.listener(Self::key_down))
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|this, _, window, cx| {
                    this.focus.focus(window);
                    cx.stop_propagation();
                }),
            )
            .on_mouse_move(cx.listener(|this, event: &MouseMoveEvent, _, cx| {
                if this.resizing_browser
                    && event.pressed_button == Some(MouseButton::Left)
                    && let Some(bounds) = this.bounds
                {
                    let width = f32::from(event.position.x - bounds.left()).clamp(180., 360.);
                    if (this.browser_width - width).abs() > 0.5 {
                        this.browser_width = width;
                        cx.notify();
                    }
                } else if this.resizing_console
                    && event.pressed_button == Some(MouseButton::Left)
                    && let Some(bounds) = this.bounds
                {
                    let height = f32::from(bounds.bottom() - event.position.y)
                        .clamp(120., this.console_max_height());
                    if (this.console_height - height).abs() > 0.5 {
                        this.console_height = height;
                        cx.notify();
                    }
                }
            }))
            .on_mouse_up(
                MouseButton::Left,
                cx.listener(|this, _, _, _| {
                    this.resizing_browser = false;
                    this.resizing_console = false;
                }),
            )
            .on_mouse_up_out(
                MouseButton::Left,
                cx.listener(|this, _, _, _| {
                    this.resizing_browser = false;
                    this.resizing_console = false;
                }),
            )
            .child(header)
            .when(self.ops.section == operations::Section::Keys, |d| {
                d.when(!self.console_fills_body(), |d| d.child(body))
                    .when(self.console_open, |d| d.child(console))
            })
            .when(self.ops.section != operations::Section::Keys, |d| {
                d.child(self.render_operations(cx))
            });
        if let Some(error) = &self.error {
            panel = panel.child(
                ui::feedback(error.render(), true)
                    .id("redis-error")
                    .max_h(px(80.))
                    .overflow_y_scroll()
                    .map(ui::preserve_scroll_axes)
                    .map(|content| ui::scrollable(content, None)),
            );
        } else if let Some(busy) = self.busy {
            panel = panel.child(
                div()
                    .px(px(10.))
                    .py(px(4.))
                    .flex_shrink_0()
                    .text_size(px(11.))
                    .text_color(rgb(ui::colors().muted))
                    .child(tr(busy)),
            );
        } else if let Some(notice) = &self.notice {
            panel = panel.child(
                div()
                    .px(px(10.))
                    .py(px(4.))
                    .flex_shrink_0()
                    .text_size(px(11.))
                    .text_color(rgb(ui::colors().muted))
                    .child(notice.render()),
            );
        }
        if self.error.is_none()
            && !self.window_state.is_locked()
            && let Some(notice) = &self.success_notice
        {
            panel = panel.child(
                div()
                    .absolute()
                    .right(px(12.))
                    .bottom(px(12.))
                    .max_w(px((self.viewport_size.0 - 24.).max(100.)))
                    .flex()
                    .items_center()
                    .gap_2()
                    .px_3()
                    .py_2()
                    .rounded(px(4.))
                    .border_1()
                    .border_color(rgb(ui::colors().border))
                    .bg(rgb(ui::colors().raised))
                    .text_size(px(12.))
                    .text_color(rgb(ui::colors().text))
                    .child(icon(Icon::Check).text_color(rgb(ui::colors().success)))
                    .child(notice.render()),
            );
        }
        if let Some(menu) = self.render_menu(cx) {
            panel = panel.child(menu);
        }
        panel.child(
            gpui::canvas(
                move |bounds, _, cx| {
                    entity.update(cx, |this, _| this.bounds = Some(bounds));
                },
                |_, _, _, _| {},
            )
            .absolute()
            .top_0()
            .left_0()
            .size_full(),
        )
    }
}

impl RedisView {
    pub(super) fn enabled(&self) -> bool {
        self.state == ConnectionState::Connected
            && self.busy.is_none()
            && !self.native_pending
            && self.editor.is_none()
    }

    fn render_detail(&self, cx: &mut Context<Self>) -> AnyElement {
        let mut detail = div()
            .flex_1()
            .min_w_0()
            .min_h_0()
            .overflow_hidden()
            .flex()
            .flex_col();
        if let Some(editor) = &self.editor
            && matches!(editor.action, EditAction::Create(_))
        {
            return detail
                .child(self.render_editor(editor, cx))
                .into_any_element();
        }
        let (Some(key), Some(info)) = (&self.selected_key, &self.info) else {
            return detail
                .child(
                    div()
                        .flex_1()
                        .flex()
                        .flex_col()
                        .gap(px(10.))
                        .items_center()
                        .justify_center()
                        .p(px(20.))
                        .child(
                            icon(Icon::Redis)
                                .size(px(30.))
                                .text_color(rgb(ui::colors().faint)),
                        )
                        .child(
                            div()
                                .text_color(rgb(ui::colors().muted))
                                .child(tr(i18n_key::REDIS_SELECT_A_KEY_TO_INSPECT_ITS_VALUE)),
                        )
                        .child(
                            div()
                                .text_size(px(11.))
                                .text_color(rgb(ui::colors().faint))
                                .child(tr(
                                    i18n_key::WORKSPACE_BROWSE_KEYS_OR_OPEN_THE_REDIS_CONSOLE,
                                )),
                        ),
                )
                .into_any_element();
        };
        let enabled = self.enabled();
        let key_exists = !matches!(info.kind, KeyType::Missing);
        let ttl = match info.ttl_ms {
            -2 => tr(i18n_key::REDIS_EXPIRED_OR_MISSING).to_owned(),
            -1 => tr(i18n_key::REDIS_NO_EXPIRY).to_owned(),
            milliseconds => {
                rustterm::trf!(i18n_key::REDIS_TTL_SECONDS_S, seconds = milliseconds / 1000)
            }
        };
        let size = if matches!(info.kind, KeyType::String) {
            rustterm::trf!(i18n_key::COMMON_BYTE_COUNT, count = info.length)
        } else {
            rustterm::trf!(i18n_key::REDIS_ENTRY_COUNT, count = info.length)
        };
        detail = detail.child(
            div()
                .h(px(56.))
                .px(px(12.))
                .flex_shrink_0()
                .flex()
                .items_center()
                .gap(px(8.))
                .when(self.compact || self.browser_collapsed, |d| {
                    d.child(
                        icon_button("redis-show-keys", Icon::Key, i18n_key::REDIS_SHOW_KEYS)
                            .tab_index(0)
                            .on_click(
                                cx.listener(|this, _, window, cx| this.open_search(window, cx)),
                            ),
                    )
                })
                .child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .flex()
                        .flex_col()
                        .gap(px(3.))
                        .child(
                            div()
                                .overflow_hidden()
                                .whitespace_nowrap()
                                .text_ellipsis()
                                .font_family(ui::MONOSPACE_FONT)
                                .text_size(px(14.))
                                .child(value::label(key)),
                        )
                        .child(
                            div()
                                .flex()
                                .gap(px(8.))
                                .text_size(px(10.))
                                .text_color(rgb(ui::colors().muted))
                                .child(ttl)
                                .child("·")
                                .child(size)
                                .when_some(value::encoding(&info.data), |d, encoding| {
                                    d.child("·").child(tr(encoding))
                                }),
                        ),
                )
                .child(
                    div()
                        .px(px(7.))
                        .py(px(3.))
                        .rounded(px(3.))
                        .bg(rgb(ui::colors().selected))
                        .text_size(px(10.))
                        .text_color(rgb(ui::colors().accent))
                        .child(kind_label(&info.kind)),
                )
                .child(
                    icon_button_enabled(
                        "redis-refresh-value",
                        Icon::Refresh,
                        self.disabled_reason()
                            .unwrap_or(i18n_key::REDIS_REFRESH_VALUE),
                        enabled,
                    )
                    .on_click(cx.listener(|this, _, _, cx| this.refresh_value(cx))),
                )
                .child(
                    icon_button("redis-key-menu", Icon::More, i18n_key::REDIS_KEY_ACTIONS)
                        .tab_index(0)
                        .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
                        .on_click(cx.listener(|this, _, window, cx| {
                            this.toggle_menu(RedisMenu::Key, window, cx)
                        })),
                ),
        );
        if let Some(editor) = &self.editor {
            return detail
                .child(self.render_editor(editor, cx))
                .into_any_element();
        }
        if key_exists {
            let rows = row_count(&info.data);
            let can_edit = matches!(info.kind, KeyType::String)
                && !value::partial_string(info, &self.detail_page);
            let can_copy = matches!(info.kind, KeyType::String);
            detail = detail.child(
                div()
                    .min_h(px(36.))
                    .px(px(12.))
                    .py(px(6.))
                    .flex_shrink_0()
                    .flex()
                    .flex_wrap()
                    .items_center()
                    .gap(px(6.))
                    .bg(rgb(ui::colors().surface))
                    .border_y_1()
                    .border_color(rgb(ui::colors().border))
                    .when(matches!(info.kind, KeyType::String), |d| {
                        d.child(
                            text_button_enabled(
                                "redis-preview-json",
                                i18n_key::REDIS_JSON,
                                if self.preview_json && self.json_preview.is_some() {
                                    ButtonStyle::Primary
                                } else {
                                    ButtonStyle::Neutral
                                }, self.json_preview.is_some())
                            .when(self.json_preview.is_none(), |button| {
                                let reason = i18n_key::COMMON_JSON_PREVIEW_REQUIRES_A_COMPLETE_JSON_VALUE_WITHIN_THE_PREVIEW;
                                button.tooltip(move |_, cx| cx.new(|_| ui::Tooltip(tr(reason).into())).into())
                            })


                            .on_click(cx.listener(|this, _, _, cx| {
                                if this.json_preview.is_some() {
                                    this.preview_json = true;
                                    this.preview_scroll
                                        .scroll_to_item_strict(0, ScrollStrategy::Top);
                                    cx.notify();
                                }
                            })),
                        )
                        .child(
                            text_button(
                                "redis-preview-raw",
                                i18n_key::REDIS_RAW_VALUE,
                                if !self.preview_json || self.json_preview.is_none() {
                                    ButtonStyle::Primary
                                } else {
                                    ButtonStyle::Neutral
                                },
                            )
                            .tab_index(0)
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.preview_json = false;
                                this.preview_scroll
                                    .scroll_to_item_strict(0, ScrollStrategy::Top);
                                cx.notify();
                            })),
                        )
                    })
                    .child(div().flex_1())
                    .when(can_copy, |d| {
                        d.child(
                            text_button(
                                "redis-copy-value",
                                if value::partial_string(info, &self.detail_page) {
                                    i18n_key::REDIS_COPY_PAGE
                                } else {
                                    i18n_key::REDIS_COPY_VALUE
                                },
                                ButtonStyle::Neutral,
                            )
                            .tab_index(0)
                            .on_click(cx.listener(|this, _, _, cx| this.copy_value(cx))),
                        )
                    })
                    .when(can_edit, |d| {
                        d.child(
                            text_button_enabled("redis-edit-value", i18n_key::REDIS_EDIT_VALUE, ButtonStyle::Primary, enabled)
                            .when(!enabled, |button| {
                                let reason = self.disabled_reason().unwrap_or(i18n_key::COMMON_CONNECT_TO_REDIS_TO_USE_THIS_ACTION);
                                button.tooltip(move |_, cx| cx.new(|_| ui::Tooltip(tr(reason).into())).into())
                            })


                                .on_click(cx.listener(|this, _, window, cx| {
                                    this.edit_value(false, window, cx)
                                })),
                        )
                    })
                    .when(
                        !matches!(info.kind, KeyType::String | KeyType::Unsupported),
                        |d| {
                            d.child(
                                text_button_enabled("redis-add-entry", i18n_key::REDIS_ADD_ENTRY, ButtonStyle::Primary, enabled)
                            .when(!enabled, |button| {
                                let reason = self.disabled_reason().unwrap_or(i18n_key::COMMON_CONNECT_TO_REDIS_TO_USE_THIS_ACTION);
                                button.tooltip(move |_, cx| cx.new(|_| ui::Tooltip(tr(reason).into())).into())
                            })


                                    .on_click(cx.listener(|this, _, window, cx| {
                                        this.edit_value(true, window, cx)
                                    })),
                            )
                        },
                    ),
            );
            if matches!(info.data, KeyData::String(_)) {
                let formatted = self.preview_json && self.json_preview.is_some();
                let preview = self.current_preview();
                let lines = preview.lines.len();
                detail = detail
                    .child(
                        div()
                            .relative()
                            .flex_1()
                            .min_h_0()
                            .min_w_0()
                            .flex()
                            .flex_col()
                            .pr(px(ui::SCROLLBAR_WIDTH))
                            .pb(px(ui::SCROLLBAR_WIDTH))
                            .child(
                                uniform_list(
                                    "redis-string-preview",
                                    lines,
                                    cx.processor(|this, range: std::ops::Range<usize>, _, _| {
                                        range
                                            .map(|index| this.render_preview_line(index))
                                            .collect::<Vec<_>>()
                                    }),
                                )
                                .map(ui::preserve_scroll_axes)
                                .with_width_from_item(Some(preview.widest))
                                .with_horizontal_sizing_behavior(
                                    gpui::ListHorizontalSizingBehavior::Unconstrained,
                                )
                                .track_scroll(self.preview_scroll.clone())
                                .flex_1()
                                .min_h_0()
                                .min_w_0()
                                .py(px(12.)),
                            )
                            .child(ui::scrollbars(
                                "redis-preview-bars",
                                Some(self.preview_scroll.0.borrow().base_handle.clone()),
                                Some(self.preview_scroll.0.borrow().base_handle.clone()),
                            )),
                    )
                    .child(
                        div()
                            .h(px(if self.compact { 24. } else { 30. }))
                            .px(px(16.))
                            .flex_shrink_0()
                            .flex()
                            .items_center()
                            .justify_between()
                            .border_t_1()
                            .border_color(rgb(ui::colors().border))
                            .text_size(px(10.))
                            .text_color(rgb(ui::colors().muted))
                            .child(tr(if formatted {
                                i18n_key::REDIS_FORMATTED_PREVIEW
                            } else {
                                i18n_key::REDIS_RAW_PREVIEW
                            }))
                            .child(rustterm::trf!(
                                i18n_key::SFTP_PREVIEW_LINE_COUNT,
                                count = lines
                            )),
                    );
            } else if rows > 0 {
                let (first, second) = column_labels(&info.kind);
                detail = detail
                    .child(
                        div()
                            .h(px(28.))
                            .px(px(10.))
                            .flex_shrink_0()
                            .flex()
                            .items_center()
                            .gap(px(10.))
                            .border_b_1()
                            .border_color(rgb(ui::colors().border))
                            .text_size(px(10.))
                            .text_color(rgb(ui::colors().muted))
                            .child(div().flex_1().min_w_0().child(tr(first)))
                            .when_some(second, |d, label| {
                                d.child(div().flex_1().min_w_0().child(tr(label)))
                            }),
                    )
                    .child(
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
                                    "redis-value-rows",
                                    rows,
                                    cx.processor(|this, range: std::ops::Range<usize>, _, cx| {
                                        range
                                            .map(|index| this.render_value_row(index, cx))
                                            .collect::<Vec<_>>()
                                    }),
                                )
                                .map(ui::preserve_scroll_axes)
                                .track_scroll(self.row_scroll.clone())
                                .flex_1()
                                .min_h_0(),
                            )
                            .child(ui::scrollbars(
                                "redis-value-bars",
                                None,
                                Some(self.row_scroll.0.borrow().base_handle.clone()),
                            )),
                    );
                if let Some(row) = self.selected_row {
                    let preview = selected_preview(&info.data, row);
                    detail =
                        detail.child(
                            div()
                                .id("redis-row-preview")
                                .flex_shrink_0()
                                .flex()
                                .flex_col()
                                .border_t_1()
                                .border_color(rgb(ui::colors().border))
                                .child(
                                    div()
                                        .h(px(34.))
                                        .px(px(10.))
                                        .flex_shrink_0()
                                        .flex()
                                        .items_center()
                                        .gap(px(6.))
                                        .child(
                                            div()
                                                .flex_1()
                                                .text_size(px(10.))
                                                .text_color(rgb(ui::colors().muted))
                                                .child(tr(i18n_key::REDIS_SELECTED_ENTRY)),
                                        )
                                        .when(self.entry_copyable(), |d| {
                                            d.child(
                                                icon_button(
                                                    "redis-copy-entry",
                                                    Icon::Copy,
                                                    i18n_key::REDIS_COPY_VALUE,
                                                )
                                                .tab_index(0)
                                                .on_click(cx.listener(|this, _, _, cx| {
                                                    this.copy_value(cx)
                                                })),
                                            )
                                        })
                                        .when(self.entry_editable(), |d| {
                                            d.child(
                                                text_button_enabled(
                                                    "redis-edit-entry",
                                                    i18n_key::REDIS_EDIT_ENTRY,
                                                    ButtonStyle::Neutral,
                                                    enabled,
                                                )
                                                .when(!enabled, |button| {
                                                    let reason = self.disabled_reason().unwrap_or(
                                                        i18n_key::COMMON_CONNECT_TO_REDIS_TO_USE_THIS_ACTION,
                                                    );
                                                    button.tooltip(move |_, cx| {
                                                        cx.new(|_| ui::Tooltip(tr(reason).into()))
                                                            .into()
                                                    })
                                                })
                                                .on_click(cx.listener(|this, _, window, cx| {
                                                    this.edit_value(false, window, cx)
                                                })),
                                            )
                                        })
                                        .when(self.entry_removable(), |d| {
                                            d.child(
                                                icon_button_enabled(
                                                    "redis-delete-entry",
                                                    Icon::Trash,
                                                    self.disabled_reason()
                                                        .unwrap_or(i18n_key::REDIS_DELETE_ENTRY),
                                                    enabled,
                                                )
                                                .on_click(cx.listener(|this, _, window, cx| {
                                                    this.delete_row(window, cx)
                                                })),
                                            )
                                        }),
                                )
                                .child(
                                    div()
                                        .id("redis-selected-entry-content")
                                        .max_h(px(112.))
                                        .min_h(px(32.))
                                        .overflow_y_scroll()
                                        .map(ui::preserve_scroll_axes)
                                        .px(px(10.))
                                        .pb(px(8.))
                                        .font_family(ui::MONOSPACE_FONT)
                                        .text_size(px(11.))
                                        .child(preview)
                                        .map(|content| ui::scrollable(content, None)),
                                ),
                        );
                }
            } else {
                detail = detail.child(
                    div()
                        .flex_1()
                        .p(px(12.))
                        .text_color(rgb(ui::colors().muted))
                        .child(tr(if matches!(info.kind, KeyType::Unsupported) {
                            i18n_key::REDIS_THIS_REDIS_DATA_TYPE_IS_NOT_SUPPORTED_BY_THE_VISUAL
                        } else {
                            i18n_key::REDIS_NO_ENTRIES_IN_THIS_PAGE
                        })),
                );
            }
        } else {
            detail = detail.child(
                div()
                    .flex_1()
                    .p(px(12.))
                    .text_color(rgb(ui::colors().muted))
                    .child(tr(i18n_key::REDIS_THIS_KEY_HAS_EXPIRED_OR_WAS_DELETED)),
            );
        }
        if info.next.is_some() || !self.detail_history.is_empty() {
            detail = detail.child(
                div()
                    .h(px(34.))
                    .px(px(8.))
                    .flex_shrink_0()
                    .flex()
                    .items_center()
                    .gap(px(8.))
                    .border_t_1()
                    .border_color(rgb(ui::colors().border))
                    .child(
                        text_button_enabled(
                            "redis-prev-value",
                            i18n_key::COMMON_PREVIOUS,
                            ButtonStyle::Neutral,
                            enabled && !self.detail_history.is_empty(),
                        )
                        .when(!enabled || self.detail_history.is_empty(), |button| {
                            let reason = self
                                .disabled_reason()
                                .unwrap_or(i18n_key::COMMON_ALREADY_AT_THE_FIRST_PAGE);
                            button.tooltip(move |_, cx| {
                                cx.new(|_| ui::Tooltip(tr(reason).into())).into()
                            })
                        })
                        .focus(|style| style.bg(rgb(ui::colors().selected)).text_color(rgb(ui::colors().text)))
                        .on_click(cx.listener(|this, _, _, cx| this.page_value(false, cx))),
                    )
                    .child(
                        text_button_enabled(
                            "redis-next-value",
                            i18n_key::COMMON_NEXT,
                            ButtonStyle::Neutral,
                            enabled && info.next.is_some(),
                        )
                        .when(!enabled || info.next.is_none(), |button| {
                            let reason = self
                                .disabled_reason()
                                .unwrap_or(i18n_key::COMMON_ALREADY_AT_THE_LAST_PAGE);
                            button.tooltip(move |_, cx| {
                                cx.new(|_| ui::Tooltip(tr(reason).into())).into()
                            })
                        })
                        .focus(|style| style.bg(rgb(ui::colors().selected)).text_color(rgb(ui::colors().text)))
                        .on_click(cx.listener(|this, _, _, cx| this.page_value(true, cx))),
                    )
                    .child(
                        div()
                            .text_size(px(10.))
                            .text_color(rgb(ui::colors().muted))
                            .child(tr(
                                i18n_key::REDIS_PAGES_REFLECT_LIVE_DATA_CONCURRENT_WRITES_MAY_CHANGE_RESULTS,
                            )),
                    ),
            );
        }
        detail.into_any_element()
    }

    fn current_preview(&self) -> &value::Preview {
        if self.preview_json {
            self.json_preview.as_ref().unwrap_or(&self.raw_preview)
        } else {
            &self.raw_preview
        }
    }

    fn render_preview_line(&self, index: usize) -> AnyElement {
        let line = self.current_preview().lines[index].clone();
        let content = if self.preview_json && self.json_preview.is_some() {
            let highlights = json_highlights(&line);
            gpui::StyledText::new(line)
                .with_highlights(highlights)
                .into_any_element()
        } else {
            line.into_any_element()
        };
        div()
            .h(px(26.))
            .px(px(16.))
            .flex()
            .items_center()
            .gap(px(14.))
            .font_family(ui::MONOSPACE_FONT)
            .text_size(px(13.))
            .line_height(px(26.))
            .whitespace_nowrap()
            .child(
                div()
                    .w(px(38.))
                    .flex_shrink_0()
                    .pr(px(ui::SCROLLBAR_WIDTH))
                    .border_r_1()
                    .border_color(rgb(ui::colors().border))
                    .text_right()
                    .text_color(rgb(ui::colors().faint))
                    .child((index + 1).to_string()),
            )
            .child(content)
            .into_any_element()
    }

    fn console_fills_body(&self) -> bool {
        self.console_open && self.viewport_size.1 - ui::PANE_HEADER_HEIGHT < 400.
    }

    fn console_max_height(&self) -> f32 {
        (self.viewport_size.1 - ui::PANE_HEADER_HEIGHT - 160.).max(120.)
    }

    pub(super) fn toggle_console(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.editor.is_some() && !self.console_open {
            self.error =
                Some(i18n_key::REDIS_SAVE_OR_CANCEL_THE_CURRENT_EDIT_BEFORE_NAVIGATING.into());
        } else {
            self.console_open = self.ops.section != operations::Section::Keys || !self.console_open;
            self.ops.section = operations::Section::Keys;
            if self.console_open {
                self.command_input.read(cx).focus(window);
            } else {
                self.focus.focus(window);
            }
        }
        cx.notify();
    }

    fn render_value_row(&self, index: usize, cx: &mut Context<Self>) -> AnyElement {
        let (first, second) = match self.info.as_ref().map(|info| &info.data) {
            Some(KeyData::Hash(rows)) => {
                (value::label(&rows[index].0), value::label(&rows[index].1))
            }
            Some(KeyData::List(rows)) => (
                (self.detail_page.offset + index as u64).to_string(),
                value::label(&rows[index]),
            ),
            Some(KeyData::Set(rows)) => (value::label(&rows[index]), String::new()),
            Some(KeyData::ZSet(rows)) => {
                (value::label(&rows[index].0), value::label(&rows[index].1))
            }
            Some(KeyData::Stream(rows)) => (
                value::label(&rows[index].id),
                rustterm::trf!(
                    i18n_key::REDIS_FIELD_COUNT,
                    count = rows[index].fields.len()
                ),
            ),
            _ => (String::new(), String::new()),
        };
        div()
            .id(("redis-value-row", index))
            .w_full()
            .min_w_0()
            .font_family(ui::MONOSPACE_FONT)
            .when(
                self.selected_row == Some(index) || (index == 0 && self.selected_row.is_none()),
                |d| d.tab_index(0),
            )
            .focus(|style| {
                style
                    .bg(rgb(ui::colors().selected))
                    .text_color(rgb(ui::colors().text))
            })
            .h(px(30.))
            .px(px(10.))
            .flex()
            .items_center()
            .gap(px(10.))
            .cursor_pointer()
            .border_b_1()
            .border_color(rgb(ui::colors().surface))
            .when(self.selected_row == Some(index), |d| {
                d.bg(rgb(ui::colors().selected))
            })
            .hover(|d| d.bg(rgb(ui::colors().hover)))
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .text_size(px(11.))
                    .overflow_hidden()
                    .whitespace_nowrap()
                    .text_ellipsis()
                    .child(first),
            )
            .when(
                self.info
                    .as_ref()
                    .is_some_and(|info| column_labels(&info.kind).1.is_some()),
                |d| {
                    d.child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .text_color(rgb(ui::colors().muted))
                            .text_size(px(11.))
                            .overflow_hidden()
                            .whitespace_nowrap()
                            .text_ellipsis()
                            .child(second),
                    )
                },
            )
            .on_click(
                cx.listener(move |this, event: &gpui::ClickEvent, window, cx| {
                    if this.available(cx) {
                        this.selected_row = Some(index);
                        this.navigation = Navigation::Values;
                        this.focus.focus(window);
                        if event.click_count() == 2 && this.entry_editable() {
                            this.edit_value(false, window, cx);
                        }
                        cx.notify();
                    }
                }),
            )
            .on_key_down(cx.listener(move |this, event: &KeyDownEvent, window, cx| {
                if matches!(
                    event.keystroke.key.as_str(),
                    "up" | "down" | "home" | "end" | "enter" | "f2" | "delete"
                ) {
                    this.selected_row = Some(index);
                    this.navigation = Navigation::Values;
                    this.focus.focus(window);
                    this.key_down(event, window, cx);
                }
            }))
            .on_mouse_down(
                MouseButton::Right,
                cx.listener(move |this, event: &gpui::MouseDownEvent, window, cx| {
                    if this.available(cx) {
                        this.selected_row = Some(index);
                        this.navigation = Navigation::Values;
                        this.open_context_menu(RedisMenu::Entry, event.position, window, cx);
                    }
                    cx.stop_propagation();
                }),
            )
            .into_any_element()
    }

    fn render_editor(&self, editor: &Editor, cx: &mut Context<Self>) -> AnyElement {
        let editing = self.busy.is_none();
        let mut form = div()
            .id("redis-editor")
            .flex_1()
            .min_h_0()
            .overflow_y_scroll()
            .map(ui::preserve_scroll_axes)
            .p(px(10.))
            .flex_shrink_0()
            .flex()
            .flex_col()
            .gap(px(8.))
            .border_b_1()
            .border_color(rgb(ui::colors().border))
            .bg(rgb(ui::colors().surface));
        form = form.child(
            div()
                .flex()
                .items_center()
                .justify_between()
                .child(
                    div()
                        .text_color(rgb(ui::colors().accent))
                        .child(tr(match &editor.action {
                            EditAction::Create(_) => i18n_key::REDIS_CREATE_KEY,
                            EditAction::Rename => i18n_key::REDIS_RENAME_KEY,
                            EditAction::Ttl => i18n_key::REDIS_SET_TTL,
                            _ => i18n_key::REDIS_EDIT_REDIS_DATA,
                        })),
                )
                .child(
                    div()
                        .flex()
                        .gap(px(6.))
                        .child(
                            text_button_enabled(
                                "redis-save-edit",
                                i18n_key::COMMON_SAVE,
                                ButtonStyle::Primary,
                                editing,
                            )
                            .when(!editing, |button| {
                                let reason =
                                    i18n_key::COMMON_WAIT_FOR_THE_CURRENT_REDIS_REQUEST_TO_FINISH;
                                button.tooltip(move |_, cx| {
                                    cx.new(|_| ui::Tooltip(tr(reason).into())).into()
                                })
                            })
                            .focus(|style| {
                                style
                                    .bg(rgb(ui::colors().selected))
                                    .text_color(rgb(ui::colors().text))
                            })
                            .on_click(cx.listener(|this, _, _, cx| this.save_editor(cx))),
                        )
                        .child(
                            text_button_enabled(
                                "redis-cancel-edit",
                                i18n_key::COMMON_CANCEL,
                                ButtonStyle::Neutral,
                                editing,
                            )
                            .when(!editing, |button| {
                                let reason =
                                    i18n_key::COMMON_WAIT_FOR_THE_CURRENT_REDIS_REQUEST_TO_FINISH;
                                button.tooltip(move |_, cx| {
                                    cx.new(|_| ui::Tooltip(tr(reason).into())).into()
                                })
                            })
                            .focus(|style| {
                                style
                                    .bg(rgb(ui::colors().selected))
                                    .text_color(rgb(ui::colors().text))
                            })
                            .on_click(
                                cx.listener(|this, _, window, cx| this.cancel_editor(window, cx)),
                            ),
                        ),
                ),
        );
        if let EditAction::Create(kind) = editor.action {
            form = form.child(
                div().flex().flex_wrap().gap(px(5.)).children(
                    DataKind::ALL
                        .into_iter()
                        .enumerate()
                        .map(|(index, option)| {
                            text_button_enabled(
                                ("redis-create-kind", index),
                                option.name(),
                                if option == kind {
                                    ButtonStyle::Primary
                                } else {
                                    ButtonStyle::Neutral
                                },
                                editing,
                            )
                            .when(!editing, |button| {
                                let reason =
                                    i18n_key::COMMON_WAIT_FOR_THE_CURRENT_REDIS_REQUEST_TO_FINISH;
                                button.tooltip(move |_, cx| {
                                    cx.new(|_| ui::Tooltip(tr(reason).into())).into()
                                })
                            })
                            .focus(|style| {
                                style
                                    .bg(rgb(ui::colors().selected))
                                    .text_color(rgb(ui::colors().text))
                            })
                            .on_click(
                                cx.listener(move |this, _, _, cx| this.set_edit_kind(option, cx)),
                            )
                        }),
                ),
            );
        }
        let escaped = editor.encoding == value::EditEncoding::Escaped;
        if editor.action.text_fields().into_iter().any(|field| field) {
            form = form.child(
                div().flex().items_center().gap(px(6.)).children(
                    [
                        (value::EditEncoding::Text, i18n_key::REDIS_TEXT),
                        (value::EditEncoding::Escaped, i18n_key::REDIS_ESCAPED_TEXT),
                    ]
                    .into_iter()
                    .enumerate()
                    .map(|(index, (encoding, label))| {
                        text_button_enabled(
                            ("redis-editor-encoding", index),
                            label,
                            if editor.encoding == encoding {
                                ButtonStyle::Primary
                            } else {
                                ButtonStyle::Neutral
                            },
                            editing,
                        )
                        .when(!editing, |button| {
                            let reason =
                                i18n_key::COMMON_WAIT_FOR_THE_CURRENT_REDIS_REQUEST_TO_FINISH;
                            button.tooltip(move |_, cx| {
                                cx.new(|_| ui::Tooltip(tr(reason).into())).into()
                            })
                        })
                        .on_click(
                            cx.listener(move |this, _, _, cx| this.set_edit_encoding(encoding, cx)),
                        )
                    }),
                ),
            );
        }
        let context = match &editor.action {
            EditAction::Hash(Some(field)) => Some(rustterm::trf!(
                i18n_key::REDIS_FIELD_FIELD,
                field = value::label(field)
            )),
            EditAction::List(Some((index, _))) => {
                Some(rustterm::trf!(i18n_key::REDIS_INDEX_INDEX, index = index))
            }
            _ => None,
        };
        if let Some(context) = context {
            form = form.child(
                div()
                    .text_size(px(11.))
                    .text_color(rgb(ui::colors().muted))
                    .child(context),
            );
        }
        if matches!(editor.action, EditAction::Create(_) | EditAction::Rename) {
            form = form.child(field(
                if escaped {
                    i18n_key::REDIS_KEY_AS_JSON_STRING
                } else {
                    i18n_key::REDIS_KEY
                },
                editor.name.clone(),
            ));
        }
        let extra_label = match &editor.action {
            EditAction::Create(DataKind::Hash | DataKind::Stream)
            | EditAction::Hash(None)
            | EditAction::Stream => Some(if escaped {
                i18n_key::REDIS_FIELD_AS_JSON_STRING
            } else {
                i18n_key::REDIS_FIELD
            }),
            EditAction::Create(DataKind::ZSet) | EditAction::ZSet(_) => Some(i18n_key::REDIS_SCORE),
            _ => None,
        };
        if let Some(label) = extra_label {
            form = form.child(field(label, editor.extra.clone()));
        }
        match &editor.action {
            EditAction::Rename => {}
            EditAction::ZSet(Some(member)) => {
                form = form.child(
                    div()
                        .text_size(px(11.))
                        .text_color(rgb(ui::colors().muted))
                        .child(value::label(member)),
                );
            }
            EditAction::Ttl => {
                form = form.child(field(
                    i18n_key::REDIS_SECONDS_1_REMOVES_EXPIRY,
                    editor.value.clone(),
                ));
            }
            _ => {
                form = form.child(field(
                    if escaped {
                        i18n_key::REDIS_VALUE_AS_JSON_STRING
                    } else {
                        i18n_key::CONNECTIONS_VALUE
                    },
                    editor.value.clone(),
                ))
            }
        }
        if editor.action.text_fields().into_iter().any(|field| field) {
            let hint = if escaped {
                i18n_key::REDIS_JSON_STRINGS_PRESERVE_NEWLINES_AND_CONTROL_CHARACTERS_EXAMPLE_HELLO_NWORLD
            } else if !matches!(editor.action, EditAction::Rename) {
                i18n_key::REDIS_ENTER_INSERTS_A_NEWLINE_CTRL_ENTER_SAVES
            } else {
                i18n_key::REDIS_ENTER_THE_KEY_NAME_DIRECTLY_WITHOUT_EXTRA_QUOTES
            };
            form = form.child(
                div()
                    .text_size(px(10.))
                    .text_color(rgb(ui::colors().muted))
                    .child(tr(hint)),
            );
        }
        ui::scrollable(form, None).into_any_element()
    }

    fn render_console(&self, cx: &mut Context<Self>) -> AnyElement {
        let enabled = self.enabled();
        let fill = self.console_fills_body();
        let mut console = div()
            .when(fill, |d| d.flex_1().min_h_0())
            .when(!fill, |d| d.flex_shrink_0())
            .when(!fill && self.console_open, |d| {
                d.h(px(self
                    .console_height
                    .clamp(120., self.console_max_height())))
            })
            .flex()
            .flex_col()
            .border_t_1()
            .border_color(rgb(ui::colors().border))
            .bg(rgb(ui::colors().surface))
            .when(self.console_open && !fill, |d| {
                d.child(
                    div()
                        .id("redis-console-divider")
                        .h(px(4.))
                        .flex_shrink_0()
                        .cursor(CursorStyle::ResizeUpDown)
                        .hover(|s| s.bg(rgb(ui::colors().accent)))
                        .on_mouse_down(
                            MouseButton::Left,
                            cx.listener(|this, _, _, cx| {
                                this.resizing_console = true;
                                cx.stop_propagation();
                            }),
                        ),
                )
            })
            .child(
                div()
                    .h(px(if self.console_open { 32. } else { 28. }))
                    .flex_shrink_0()
                    .px(px(12.))
                    .flex()
                    .items_center()
                    .gap(px(8.))
                    .child(
                        icon_button(
                            "redis-toggle-console",
                            if self.console_open {
                                Icon::ChevronDown
                            } else {
                                Icon::ChevronUp
                            },
                            i18n_key::REDIS_TOGGLE_REDIS_CONSOLE,
                        )
                        .tab_index(0)
                        .focus(|style| {
                            style
                                .bg(rgb(ui::colors().selected))
                                .text_color(rgb(ui::colors().text))
                        })
                        .on_click(cx.listener(|this, _, window, cx| {
                            this.toggle_console(window, cx);
                        })),
                    )
                    .child(icon(Icon::Terminal).text_color(rgb(ui::colors().accent)))
                    .child(
                        div()
                            .text_size(px(11.))
                            .text_color(rgb(ui::colors().accent))
                            .child(tr(i18n_key::REDIS_REDIS_CLI)),
                    )
                    .child(div().flex_1())
                    .when(self.console_open && !self.compact, |d| {
                        d.child(
                            div()
                                .text_size(px(10.))
                                .text_color(rgb(ui::colors().muted))
                                .child(tr(i18n_key::REDIS_COMMAND_HISTORY)),
                        )
                    })
                    .when(self.console_open, |d| {
                        d.child(
                            text_button_enabled(
                                "redis-copy-result",
                                i18n_key::REDIS_COPY_RESULT,
                                ButtonStyle::Neutral,
                                !self.console_output.is_empty(),
                            )
                            .focus(|style| {
                                style
                                    .bg(rgb(ui::colors().selected))
                                    .text_color(rgb(ui::colors().text))
                            })
                            .on_click(cx.listener(|this, _, _, cx| {
                                if this.window_state.is_locked() {
                                    return;
                                }
                                cx.write_to_clipboard(ClipboardItem::new_string(
                                    this.console_output.clone(),
                                ));
                                this.show_success(i18n_key::COMMON_RESULT_COPIED, cx);
                            })),
                        )
                        .child(
                            text_button_enabled(
                                "redis-clear-result",
                                i18n_key::REDIS_CLEAR,
                                ButtonStyle::Neutral,
                                !self.console_output.is_empty(),
                            )
                            .focus(|style| {
                                style
                                    .bg(rgb(ui::colors().selected))
                                    .text_color(rgb(ui::colors().text))
                            })
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.console_output.clear();
                                cx.notify();
                            })),
                        )
                    }),
            );
        if self.console_open {
            let output = div()
                .id("redis-console-output")
                .flex_1()
                .min_h_0()
                .overflow_y_scroll()
                .map(ui::preserve_scroll_axes)
                .track_scroll(&self.console_scroll)
                .px(px(10.))
                .py(px(6.))
                .bg(rgb(ui::colors().input))
                .font_family(ui::MONOSPACE_FONT)
                .text_size(px(11.))
                .child(if self.console_output.is_empty() {
                    tr(i18n_key::REDIS_COMMANDS_USE_THE_SELECTED_DATABASE_RESULTS_ARE_BOUNDED_USE_PAGED).to_owned()
                } else {
                    self.console_output.clone()
                });
            console = console
                .child(ui::scrollable(output, Some(self.console_scroll.clone())))
                .child(
                    div()
                        .h(px(36.))
                        .flex_shrink_0()
                        .px(px(8.))
                        .flex()
                        .items_center()
                        .gap(px(6.))
                        .child(div().text_color(rgb(ui::colors().accent)).child(format!(
                            "{} {} ›",
                            tr(i18n_key::REDIS_DB),
                            self.database
                        )))
                        .child(div().flex_1().min_w_0().child(self.command_input.clone()))
                        .child(
                            text_button_enabled(
                                "redis-run-command",
                                i18n_key::REDIS_RUN,
                                ButtonStyle::Primary,
                                enabled,
                            )
                            .when(!enabled, |button| {
                                let reason = self.disabled_reason().unwrap_or(
                                    i18n_key::COMMON_CONNECT_TO_REDIS_TO_USE_THIS_ACTION,
                                );
                                button.tooltip(move |_, cx| {
                                    cx.new(|_| ui::Tooltip(tr(reason).into())).into()
                                })
                            })
                            .focus(|style| {
                                style
                                    .bg(rgb(ui::colors().selected))
                                    .text_color(rgb(ui::colors().text))
                            })
                            .on_click(cx.listener(|this, _, _, cx| this.execute(cx))),
                        ),
                );
        }
        console.into_any_element()
    }
}

fn field(label: &'static str, input: Entity<TextInput>) -> AnyElement {
    div()
        .flex()
        .flex_col()
        .gap(px(4.))
        .child(
            div()
                .text_size(px(10.))
                .text_color(rgb(ui::colors().muted))
                .child(tr(label)),
        )
        .child(input)
        .into_any_element()
}

pub(super) fn kind_label(kind: &KeyType) -> &'static str {
    match kind {
        KeyType::Missing => tr(i18n_key::REDIS_MISSING),
        KeyType::String => "String",
        KeyType::Hash => "Hash",
        KeyType::List => "List",
        KeyType::Set => "Set",
        KeyType::ZSet => "ZSet",
        KeyType::Stream => "Stream",
        KeyType::Unsupported => tr(i18n_key::REDIS_UNSUPPORTED),
    }
}

pub(super) fn row_count(data: &KeyData) -> usize {
    match data {
        KeyData::Hash(rows) | KeyData::ZSet(rows) => rows.len(),
        KeyData::List(rows) | KeyData::Set(rows) => rows.len(),
        KeyData::Stream(rows) => rows.len(),
        _ => 0,
    }
}

fn selected_preview(data: &KeyData, row: usize) -> String {
    match data {
        KeyData::Hash(rows) => rows.get(row).map(|(_, value)| value::preview(value)),
        KeyData::List(rows) | KeyData::Set(rows) => {
            rows.get(row).map(|value| value::preview(value))
        }
        KeyData::ZSet(rows) => rows
            .get(row)
            .map(|(member, score)| format!("{}\n{}", value::preview(member), value::label(score))),
        KeyData::Stream(rows) => rows.get(row).map(|entry| {
            entry
                .fields
                .iter()
                .take(100)
                .map(|(field, value)| format!("{}: {}", value::label(field), value::label(value)))
                .collect::<Vec<_>>()
                .join("\n")
        }),
        _ => None,
    }
    .unwrap_or_default()
}

/// The formatted text is already validated JSON. Highlight byte ranges without
/// parsing or rewriting its tokens, so the displayed value stays exact.
fn json_highlights(text: &str) -> Vec<(std::ops::Range<usize>, gpui::HighlightStyle)> {
    let bytes = text.as_bytes();
    let mut ranges = Vec::new();
    let mut index = 0;
    while index < bytes.len() {
        let start = index;
        let color = if bytes[index] == b'"' {
            index += 1;
            while index < bytes.len() {
                match bytes[index] {
                    b'\\' => index += 2,
                    b'"' => {
                        index += 1;
                        break;
                    }
                    _ => index += 1,
                }
            }
            let key = bytes[index..]
                .iter()
                .find(|byte| !byte.is_ascii_whitespace())
                == Some(&b':');
            if key { 0xa9bfdc } else { ui::colors().success }
        } else if matches!(bytes[index], b'-' | b'0'..=b'9' | b't' | b'f' | b'n') {
            index += 1;
            while index < bytes.len()
                && !bytes[index].is_ascii_whitespace()
                && !matches!(bytes[index], b',' | b']' | b'}')
            {
                index += 1;
            }
            ui::colors().accent
        } else {
            index += 1;
            continue;
        };
        ranges.push((
            start..index,
            gpui::HighlightStyle {
                color: Some(rgb(color).into()),
                ..Default::default()
            },
        ));
    }
    ranges
}

fn column_labels(kind: &KeyType) -> (&'static str, Option<&'static str>) {
    match kind {
        KeyType::Hash => (i18n_key::REDIS_FIELD, Some(i18n_key::CONNECTIONS_VALUE)),
        KeyType::List => (i18n_key::REDIS_INDEX, Some(i18n_key::CONNECTIONS_VALUE)),
        KeyType::Set => (i18n_key::REDIS_MEMBER, None),
        KeyType::ZSet => (i18n_key::REDIS_MEMBER, Some(i18n_key::REDIS_SCORE)),
        KeyType::Stream => (i18n_key::REDIS_ENTRY_ID, Some(i18n_key::REDIS_FIELDS)),
        _ => (i18n_key::CONNECTIONS_VALUE, None),
    }
}
