use super::*;
use gpui::{
    AnyElement, CursorStyle, Div, IntoElement, MouseButton, MouseMoveEvent, Render, Stateful, div,
    prelude::*, px, rgb, uniform_list,
};
use rustterm::i18n::messages as i18n_key;
use ui::{ButtonStyle, Icon};

impl Render for DatabaseView {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let entity = cx.entity().downgrade();
        let document_error = if self.page != WorkspacePage::Document {
            None
        } else {
            self.documents[self.active_document].error.clone()
        };
        let compact = self.viewport_size.0 < BROWSER_MIN_WIDTH;
        let browser = self.browser_open && self.page != WorkspacePage::Sessions;
        let mut body = div().flex_1().min_w_0().min_h_0().flex();
        if browser {
            body = body.child(self.render_browser(compact, cx));
            if !compact {
                body = body.child(
                    div()
                        .id("database-browser-divider")
                        .w(px(5.))
                        .h_full()
                        .flex_shrink_0()
                        .cursor(CursorStyle::ResizeLeftRight)
                        .bg(rgb(ui::colors().border))
                        .hover(|style| style.bg(rgb(ui::colors().accent)))
                        .tooltip(|_, cx| {
                            cx.new(|_| {
                                ui::Tooltip(
                                    tr(i18n_key::COMMON_DRAG_TO_RESIZE_DATABASE_BROWSER).into(),
                                )
                            })
                            .into()
                        })
                        .on_mouse_down(
                            MouseButton::Left,
                            cx.listener(|this, event: &gpui::MouseDownEvent, window, cx| {
                                if this.interactive() {
                                    this.resize_drag = Some(ResizeDrag {
                                        target: sizing::ResizeTarget::Browser,
                                        start_x: f32::from(event.position.x),
                                        width: this.browser_width(),
                                    });
                                }
                                window.prevent_default();
                                cx.stop_propagation();
                            }),
                        )
                        .on_click(|_, _, cx| cx.stop_propagation()),
                );
            }
        }
        if !compact || !browser {
            body = body.child(
                div()
                    .flex_1()
                    .min_w_0()
                    .min_h_0()
                    .flex()
                    .flex_col()
                    .child(self.render_documents(cx))
                    .child(self.render_editor(cx)),
            );
        }
        div()
            .id(("database-pane", self.index))
            .relative()
            .size_full()
            .min_w_0()
            .min_h_0()
            .overflow_hidden()
            .flex()
            .flex_col()
            .bg(rgb(ui::colors().bg))
            .text_color(rgb(ui::colors().text))
            .text_size(px(12.))
            .track_focus(&self.focus)
            .capture_key_down(cx.listener(Self::capture_edit_key))
            .on_key_down(cx.listener(Self::key_down))
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|this, _, window, cx| {
                    this.object_menu = None;
                    this.focus.focus(window);
                    cx.stop_propagation();
                }),
            )
            .on_mouse_move(
                cx.listener(|this, event: &MouseMoveEvent, _, cx| this.drag_resize(event, cx)),
            )
            .on_mouse_up(
                MouseButton::Left,
                cx.listener(|this, _, _, _| this.resize_drag = None),
            )
            .on_mouse_up_out(
                MouseButton::Left,
                cx.listener(|this, _, _, _| this.resize_drag = None),
            )
            .child(self.render_header(cx))
            .child(body)
            .when_some(
                document_error.clone().or_else(|| self.error.clone()),
                |panel, error| {
                    panel.child(
                        ui::feedback(error.render(), true)
                            .id("database-error")
                            .max_h(px(64.))
                            .overflow_y_scroll()
                            .map(ui::preserve_scroll_axes)
                            .flex_shrink_0()
                            .border_t_1()
                            .border_color(rgb(ui::colors().border))
                            .map(|content| ui::scrollable(content, None)),
                    )
                },
            )
            .when(self.error.is_none() && document_error.is_none(), |panel| {
                if let Some(busy) = self.busy {
                    panel.child(notice(tr(busy).to_owned(), ui::colors().muted))
                } else if let Some(message) = &self.notice {
                    panel.child(notice(message.render(), ui::colors().muted))
                } else {
                    panel
                }
            })
            .when(
                self.error.is_none() && document_error.is_none() && self.busy.is_none(),
                |panel| {
                    panel.when_some(self.success_notice.as_ref(), |panel, message| {
                        panel.child(
                            div()
                                .absolute()
                                .bottom(px(8.))
                                .right(px(8.))
                                .max_w(px((self.viewport_size.0 - 16.).min(420.)))
                                .px_3()
                                .py_2()
                                .rounded(px(4.))
                                .bg(rgb(ui::colors().raised))
                                .border_1()
                                .border_color(rgb(ui::colors().border))
                                .text_size(px(11.))
                                .text_color(rgb(ui::colors().success))
                                .shadow_sm()
                                .occlude()
                                .child(message.render()),
                        )
                    })
                },
            )
            .children(self.render_menu(cx))
            .children(self.render_object_menu(cx))
            .child(
                gpui::canvas(
                    move |bounds, _, cx| {
                        let _ = entity.update(cx, |this, _| this.pane_origin = bounds.origin);
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

impl DatabaseView {
    fn render_header(&self, cx: &Context<Self>) -> Div {
        let address = match &self.config {
            DatabaseConfig::Mysql(config)
            | DatabaseConfig::Postgres(config)
            | DatabaseConfig::ClickHouse(config) => {
                let host = if config.host.contains(':') && !config.host.starts_with('[') {
                    format!("[{}]", config.host)
                } else {
                    config.host.clone()
                };
                let scheme = if self.engine() == SqlEngine::Mysql {
                    "mysql"
                } else if self.engine() == SqlEngine::ClickHouse {
                    if config.tls { "https" } else { "http" }
                } else {
                    "postgresql"
                };
                format!(
                    "{scheme}://{}@{host}:{}/{}",
                    config.username, config.port, config.database
                )
            }
            DatabaseConfig::Sqlite(config) => config.path.display().to_string(),
        };
        ui::panel_header()
            .min_w_0()
            .child(
                ui::icon(Icon::for_sql_engine(self.engine())).text_color(rgb(self.engine_color())),
            )
            .child(
                div()
                    .flex_shrink_0()
                    .px_2()
                    .text_size(px(11.))
                    .text_color(rgb(self.engine_color()))
                    .child(self.engine().label()),
            )
            .child(
                div()
                    .id("database-identity")
                    .flex_1()
                    .min_w_0()
                    .px_1()
                    .truncate()
                    .child(address.clone())
                    .tooltip({
                        let address = address.clone();
                        move |_, cx| cx.new(|_| ui::Tooltip(address.clone())).into()
                    }),
            )
            .child(
                ui::icon_button("database-copy-address", Icon::Copy, i18n_key::COMMON_COPY)
                    .on_click(cx.listener(move |_, _, _, cx| {
                        cx.write_to_clipboard(ClipboardItem::new_string(address.clone()))
                    })),
            )
            .when_some(
                match &self.config {
                    DatabaseConfig::Sqlite(config) => Some(if config.read_only {
                        i18n_key::COMMON_READ_ONLY_BADGE
                    } else {
                        i18n_key::DATABASE_READ_WRITE
                    }),
                    _ => None,
                },
                |header, mode| {
                    header.child(
                        div()
                            .flex_shrink_0()
                            .px_2()
                            .text_size(px(11.))
                            .text_color(rgb(ui::colors().muted))
                            .child(tr(mode)),
                    )
                },
            )
            .child(
                ui::text_button(
                    "database-browser-toggle",
                    i18n_key::COMMON_BROWSE,
                    ButtonStyle::Neutral,
                )
                .tab_index(0)
                .flex_shrink_0()
                .whitespace_nowrap()
                .when(
                    self.browser_open && self.page != WorkspacePage::Sessions,
                    |button| {
                        button
                            .bg(rgb(ui::colors().selected))
                            .text_color(rgb(ui::colors().accent))
                    },
                )
                .tooltip(|_, cx| {
                    cx.new(|_| {
                        ui::Tooltip(tr(i18n_key::DATABASE_TOGGLE_DATABASE_BROWSER).to_owned())
                    })
                    .into()
                })
                .on_click(cx.listener(|this, _, window, cx| {
                    if this.interactive() {
                        if this.page == WorkspacePage::Sessions {
                            this.pause_monitor();
                            this.page = WorkspacePage::Overview;
                            this.browser_open = true;
                        } else {
                            this.browser_open = !this.browser_open;
                        }
                        if this.browser_open {
                            this.table_filter.read(cx).focus(window);
                        } else if this.page == WorkspacePage::Document
                            && !this.results_only
                            && this.documents[this.active_document].editor_open
                        {
                            this.documents[this.active_document]
                                .sql_input
                                .read(cx)
                                .focus(window);
                        } else {
                            this.focus.focus(window);
                        }
                        cx.notify();
                    }
                })),
            )
            .child(
                ui::icon_button(
                    "database-menu",
                    Icon::More,
                    i18n_key::WORKSPACE_DATABASE_ACTIONS,
                )
                .tab_index(0)
                .when(self.menu_open, |button| {
                    button.bg(rgb(ui::colors().selected))
                })
                .on_mouse_down(MouseButton::Left, |_, window, cx| {
                    window.prevent_default();
                    cx.stop_propagation();
                })
                .on_click(cx.listener(|this, _, window, cx| this.toggle_menu(window, cx))),
            )
            .when(self.pane_close_visible, |header| {
                header.child(
                    ui::icon_button(
                        "database-close",
                        Icon::Close,
                        i18n_key::TERMINAL_CLOSE_PANE_CTRL_SHIFT_W,
                    )
                    .tab_index(0)
                    .on_click(cx.listener(|this, _, _, cx| cx.emit(PaneEvent::Close(this.index)))),
                )
            })
    }

    pub(super) fn render_results(&self, cx: &Context<Self>) -> AnyElement {
        let Some(result) = self.current_result() else {
            return div()
                .flex_1()
                .min_h_0()
                .flex()
                .items_center()
                .justify_center()
                .p_3()
                .text_color(rgb(ui::colors().muted))
                .child(
                    div()
                        .flex()
                        .flex_col()
                        .items_center()
                        .gap_2()
                        .child(
                            ui::icon(Icon::Table)
                                .size(px(28.))
                                .text_color(rgb(ui::colors().faint)),
                        )
                        .child(tr(i18n_key::DATABASE_QUERY_READY))
                        .child(
                            div()
                                .text_size(px(11.))
                                .child(tr(i18n_key::DATABASE_QUERY_READY_HINT)),
                        ),
                )
                .into_any_element();
        };
        let output = self.documents[self.active_document]
            .output
            .as_ref()
            .unwrap();
        let revision = self.documents[self.active_document].result_revision;
        let mut results = div()
            .flex_1()
            .min_h_0()
            .min_w_0()
            .flex()
            .flex_col()
            .border_t_1()
            .border_color(rgb(ui::colors().border));
        if output.results.len() > 1 {
            results = results.child(
                div()
                    .id("database-result-tabs")
                    .h(px(30.))
                    .flex()
                    .flex_shrink_0()
                    .overflow_x_scroll()
                    .map(ui::preserve_scroll_axes)
                    .children(output.results.iter().enumerate().map(|(index, _)| {
                        div()
                            .id(("database-result-tab", index))
                            .h_full()
                            .px_3()
                            .flex()
                            .items_center()
                            .flex_shrink_0()
                            .cursor_pointer()
                            .tab_index(0)
                            .when(
                                index == self.documents[self.active_document].result_index,
                                |d| {
                                    d.bg(rgb(ui::colors().selected))
                                        .text_color(rgb(ui::colors().accent))
                                },
                            )
                            .hover(|s| s.bg(rgb(ui::colors().hover)))
                            .child(rustterm::trf!(
                                i18n_key::DATABASE_RESULT_NUMBER,
                                number = index + 1
                            ))
                            .on_click(
                                cx.listener(move |this, _, _, cx| this.select_result(index, cx)),
                            )
                    }))
                    .map(|d| ui::scrollable(d, None)),
            );
        }
        let document = &self.documents[self.active_document];
        let filtered = !document.result_filter.read(cx).value().is_empty();
        results = results
            .child(
                div()
                    .h(px(38.))
                    .px_2()
                    .flex_shrink_0()
                    .flex()
                    .items_center()
                    .gap_2()
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .truncate()
                            .text_size(px(11.))
                            .text_color(rgb(ui::colors().muted))
                            .child(if result.columns.is_empty() {
                                rustterm::trf!(
                                    i18n_key::DATABASE_AFFECTED_STATUS,
                                    rows = result.affected_rows,
                                    elapsed = output.elapsed.as_millis()
                                )
                            } else {
                                rustterm::trf!(
                                    i18n_key::DATABASE_QUERY_STATUS,
                                    rows = document.visible_rows.len(),
                                    elapsed = output.elapsed.as_millis()
                                )
                            })
                            .when(result.truncated, |d| {
                                d.child(format!(" · {}", tr(i18n_key::DATABASE_RESULT_TRUNCATED)))
                            }),
                    )
                    .child(
                        ui::icon_button(
                            "database-find-result",
                            Icon::Search,
                            i18n_key::DATABASE_FIND_IN_LOADED_ROWS,
                        )
                        .when(filtered, |b| b.text_color(rgb(ui::colors().accent)))
                        .on_click(cx.listener(|this, _, window, cx| {
                            this.result_search_open = !this.result_search_open;
                            if this.result_search_open {
                                this.documents[this.active_document]
                                    .result_filter
                                    .read(cx)
                                    .focus(window);
                            }
                            cx.notify();
                        })),
                    )
                    .child(
                        ui::icon_button(
                            "database-copy",
                            Icon::Copy,
                            i18n_key::DATABASE_COPY_DISPLAYED_RESULT_AS_TSV,
                        )
                        .on_click(cx.listener(|this, _, _, cx| this.copy_result(cx))),
                    )
                    .child(
                        action(
                            "database-export",
                            i18n_key::DATABASE_EXPORT_CSV,
                            self.interactive(),
                            ButtonStyle::Neutral,
                        )
                        .on_click(cx.listener(|this, _, _, cx| this.export_csv(cx))),
                    ),
            )
            .when(self.result_search_open || filtered, |d| {
                d.child(
                    div()
                        .px_2()
                        .pb_1()
                        .flex_shrink_0()
                        .child(document.result_filter.clone()),
                )
            });
        if result.columns.is_empty() {
            return results
                .child(
                    div()
                        .flex_1()
                        .p_3()
                        .text_color(rgb(ui::colors().muted))
                        .child(tr(i18n_key::DATABASE_NO_ROWS_RETURNED)),
                )
                .into_any_element();
        }
        let width = self.result_width();
        let header = div()
            .w(px(width))
            .h(px(28.))
            .flex_shrink_0()
            .flex()
            .bg(rgb(ui::colors().raised))
            .children(result.columns.iter().enumerate().map(|(column, name)| {
                let sort = self
                    .table_workspace()
                    .filter(|table| table.section == TableAction::Data)
                    .and_then(|table| {
                        table
                            .request
                            .order
                            .map(|order| (order.column, order.descending))
                    })
                    .or(self.documents[self.active_document].sort);
                let suffix = match sort {
                    Some((selected, false)) if selected == column => " ↑",
                    Some((selected, true)) if selected == column => " ↓",
                    _ => "",
                };
                div()
                    .id(("database-column", column))
                    .relative()
                    .w(px(self.column_width(column)))
                    .h_full()
                    .flex_shrink_0()
                    .px_2()
                    .flex()
                    .items_center()
                    .border_r_1()
                    .border_color(rgb(ui::colors().border))
                    .cursor_pointer()
                    .tab_index(0)
                    .hover(|style| style.bg(rgb(ui::colors().hover)))
                    .tooltip({
                        let name = name.clone();
                        move |_, cx| cx.new(|_| ui::Tooltip(name.clone())).into()
                    })
                    .child(div().truncate().child(format!("{name}{suffix}")))
                    .on_click(cx.listener(move |this, _, _, cx| {
                        if this.documents[this.active_document].result_revision == revision {
                            this.sort_rows(column, cx);
                        }
                    }))
                    .child(
                        div()
                            .id(("database-column-resize", column))
                            .absolute()
                            .right_0()
                            .top_0()
                            .w(px(6.))
                            .h_full()
                            .cursor(CursorStyle::ResizeLeftRight)
                            .hover(|style| style.bg(rgb(ui::colors().accent)))
                            .tooltip(|_, cx| {
                                cx.new(|_| {
                                    ui::Tooltip(
                                        tr(i18n_key::COMMON_DRAG_TO_RESIZE_DOUBLE_CLICK_TO_FIT_COLUMN).into(),
                                    )
                                })
                                .into()
                            })
                            .on_mouse_down(
                                MouseButton::Left,
                                cx.listener(
                                    move |this, event: &gpui::MouseDownEvent, window, cx| {
                                        if this.interactive()
                                            && this.documents[this.active_document].result_revision
                                                == revision
                                        {
                                            if event.click_count == 2 {
                                                this.resize_drag = None;
                                                this.fit_column(column, window, cx);
                                            } else {
                                                this.resize_drag = Some(ResizeDrag {
                                                    target: sizing::ResizeTarget::Column(column),
                                                    start_x: f32::from(event.position.x),
                                                    width: this.column_width(column),
                                                });
                                            }
                                        }
                                        window.prevent_default();
                                        cx.stop_propagation();
                                    },
                                ),
                            )
                            .on_click(|_, _, cx| cx.stop_propagation()),
                    )
            }));
        let table = div()
            .w(px(width))
            .h_full()
            .min_h_0()
            .flex()
            .flex_col()
            .child(header)
            .child(
                uniform_list(
                    "database-result-rows",
                    self.documents[self.active_document].visible_rows.len(),
                    cx.processor(|this, range: std::ops::Range<usize>, _, cx| {
                        range
                            .map(|row| this.render_result_row(row, cx))
                            .collect::<Vec<_>>()
                    }),
                )
                .map(ui::preserve_scroll_axes)
                .track_scroll(self.documents[self.active_document].result_scroll.clone())
                .w(px(width))
                .flex_1()
                .min_h_0(),
            );
        results = results.child(
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
                    div()
                        .id("database-result-horizontal")
                        .track_scroll(&self.documents[self.active_document].horizontal_scroll)
                        .flex_1()
                        .min_h_0()
                        .min_w_0()
                        .overflow_x_scroll()
                        .map(ui::preserve_scroll_axes)
                        .child(table),
                )
                .child(ui::scrollbars(
                    "database-result-bars",
                    Some(
                        self.documents[self.active_document]
                            .horizontal_scroll
                            .clone(),
                    ),
                    Some(
                        self.documents[self.active_document]
                            .result_scroll
                            .0
                            .borrow()
                            .base_handle
                            .clone(),
                    ),
                )),
        );
        if let Some((row, column)) = self.documents[self.active_document].selected_cell
            && let Some(cell) = result.rows.get(row).and_then(|row| row.get(column))
        {
            results = results.child(
                div()
                    .max_h(px(76.))
                    .flex_shrink_0()
                    .flex()
                    .items_start()
                    .gap_2()
                    .p_2()
                    .border_t_1()
                    .border_color(rgb(ui::colors().border))
                    .child(
                        div()
                            .id("database-selected-value")
                            .flex_1()
                            .min_w_0()
                            .max_h(px(60.))
                            .overflow_y_scroll()
                            .map(ui::preserve_scroll_axes)
                            .font_family(ui::MONOSPACE_FONT)
                            .whitespace_normal()
                            .child(self.cell_preview(row, column, cell.as_deref()))
                            .map(|content| ui::scrollable(content, None)),
                    )
                    .children(self.render_inline_modes(cx))
                    .when(
                        self.can_edit_inline_cell(row, column) && self.inline_edit_cell().is_none(),
                        |bar| {
                            bar.child(
                                ui::icon_button_enabled(
                                    "database-edit-cell",
                                    Icon::Edit,
                                    i18n_key::DATABASE_EDIT_VALUE_F2,
                                    self.available(),
                                )
                                .on_mouse_down(MouseButton::Left, |_, window, cx| {
                                    window.prevent_default();
                                    cx.stop_propagation();
                                })
                                .on_click(
                                    cx.listener(|this, _, window, cx| this.edit_cell(window, cx)),
                                ),
                            )
                        },
                    )
                    .child(
                        ui::icon_button_enabled(
                            "database-open-value",
                            Icon::Maximize,
                            if self.can_edit_cell(row, column) {
                                i18n_key::DATABASE_EXPANDED_EDITOR_SHIFT_ENTER
                            } else {
                                i18n_key::COMMON_VIEW_FULL_VALUE
                            },
                            self.interactive(),
                        )
                        .on_mouse_down(MouseButton::Left, |_, window, cx| {
                            window.prevent_default();
                            cx.stop_propagation();
                        })
                        .on_click(cx.listener(
                            move |this, _, window, cx| {
                                if this.can_edit_cell(row, column) {
                                    this.expand_cell_editor(window, cx);
                                } else {
                                    this.open_value(window, cx);
                                }
                            },
                        )),
                    )
                    .child(
                        ui::icon_button_enabled(
                            "database-copy-cell",
                            Icon::Copy,
                            i18n_key::DATABASE_COPY_CELL,
                            self.interactive(),
                        )
                        .on_mouse_down(MouseButton::Left, |_, window, cx| {
                            window.prevent_default();
                            cx.stop_propagation();
                        })
                        .on_click(cx.listener(|this, _, _, cx| this.copy_cell(cx))),
                    ),
            );
        }
        results.into_any_element()
    }

    fn render_result_row(&self, visible_index: usize, cx: &Context<Self>) -> AnyElement {
        let result = self.current_result().unwrap();
        let row_index = self.documents[self.active_document].visible_rows[visible_index];
        let row = &result.rows[row_index];
        let revision = self.documents[self.active_document].result_revision;
        div()
            .w(px(self.result_width()))
            .h(px(30.))
            .flex()
            .bg(rgb(if visible_index.is_multiple_of(2) {
                ui::colors().bg
            } else {
                ui::colors().surface
            }))
            .children(row.iter().enumerate().map(|(column, cell)| {
                if let Some(input) = self.inline_cell_input(row_index, column) {
                    return div()
                        .id((
                            "database-cell-editor",
                            row_index * result.columns.len() + column,
                        ))
                        .w(px(self.column_width(column)))
                        .h_full()
                        .flex_shrink_0()
                        .child(input)
                        .on_click(|_, _, cx| cx.stop_propagation())
                        .into_any_element();
                }
                div()
                    .id(("database-cell", row_index * result.columns.len() + column))
                    .w(px(self.column_width(column)))
                    .h_full()
                    .flex_shrink_0()
                    .px_2()
                    .flex()
                    .items_center()
                    .border_r_1()
                    .border_b_1()
                    .border_color(rgb(ui::colors().border))
                    .cursor_pointer()
                    .when(
                        self.documents[self.active_document].selected_cell
                            == Some((row_index, column)),
                        |item| item.bg(rgb(ui::colors().selected)),
                    )
                    .hover(|style| style.bg(rgb(ui::colors().hover)))
                    .text_color(rgb(self.cell_color(row_index, column, cell.is_none())))
                    .child(div().truncate().child(self.cell_preview(
                        row_index,
                        column,
                        cell.as_deref(),
                    )))
                    .on_mouse_down(MouseButton::Left, |_, window, cx| {
                        window.prevent_default();
                        cx.stop_propagation();
                    })
                    .on_click(
                        cx.listener(move |this, event: &gpui::ClickEvent, window, cx| {
                            if this.interactive()
                                && this.documents[this.active_document].result_revision == revision
                            {
                                if !this.select_grid_cell(row_index, column, window, cx) {
                                    return;
                                }
                                if event.click_count() == 2 {
                                    if this.can_edit_cell(row_index, column) {
                                        this.edit_cell(window, cx);
                                    } else {
                                        this.open_value(window, cx);
                                    }
                                }
                                cx.notify();
                            }
                        }),
                    )
                    .into_any_element()
            }))
            .into_any_element()
    }
}

pub(super) fn action(
    id: impl Into<gpui::ElementId>,
    label: &'static str,
    enabled: bool,
    style: ButtonStyle,
) -> Stateful<Div> {
    ui::text_button_enabled(id, label, style, enabled)
        .flex_shrink_0()
        .whitespace_nowrap()
        .focus(|style| style.border_color(rgb(ui::colors().accent)))
        .on_mouse_down(MouseButton::Left, |_, window, cx| {
            window.prevent_default();
            cx.stop_propagation();
        })
}
fn notice(message: String, color: u32) -> Div {
    div()
        .px_3()
        .py_1()
        .flex_shrink_0()
        .border_t_1()
        .border_color(rgb(ui::colors().border))
        .text_size(px(11.))
        .text_color(rgb(color))
        .child(message)
}
pub(super) fn preview(cell: Option<&str>, limit: usize) -> String {
    let Some(value) = cell else {
        return "NULL".into();
    };
    if value.is_empty() {
        return "\"\"".into();
    }
    let mut chars = value.chars();
    let mut result = String::new();
    for ch in chars.by_ref().take(limit) {
        if ch.is_control() {
            result.extend(ch.escape_debug());
        } else {
            result.push(ch);
        }
    }
    if chars.next().is_some() {
        result.push('…');
    }
    result
}
