use super::*;
use gpui::{
    AnyElement, CursorStyle, Div, IntoElement, MouseButton, MouseMoveEvent, Render, Stateful, div,
    prelude::*, px, rgb, uniform_list,
};
use rustterm::i18n::messages as i18n_key;
use ui::{ButtonStyle, Icon};

impl Render for DatabaseView {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let compact = self.viewport_size.0 < BROWSER_MIN_WIDTH;
        let browser = self.browser_open;
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
                        .bg(rgb(ui::BORDER))
                        .hover(|style| style.bg(rgb(ui::ACCENT)))
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
            .bg(rgb(ui::BG))
            .text_color(rgb(ui::TEXT))
            .text_size(px(12.))
            .track_focus(&self.focus)
            .capture_key_down(cx.listener(Self::capture_edit_key))
            .on_key_down(cx.listener(Self::key_down))
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|this, _, window, cx| {
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
                self.documents[self.active_document]
                    .error
                    .clone()
                    .or_else(|| self.error.clone()),
                |panel, error| {
                    panel.child(
                        div()
                            .id("database-error")
                            .max_h(px(64.))
                            .overflow_y_scroll()
                            .map(ui::preserve_scroll_axes)
                            .px_3()
                            .py_2()
                            .flex_shrink_0()
                            .border_t_1()
                            .border_color(rgb(ui::BORDER))
                            .text_color(rgb(ui::DANGER))
                            .child(error.render())
                            .map(|content| ui::scrollable(content, None)),
                    )
                },
            )
            .when(
                self.error.is_none() && self.documents[self.active_document].error.is_none(),
                |panel| {
                    if let Some(busy) = self.busy {
                        panel.child(notice(tr(busy).to_owned(), ui::MUTED))
                    } else if let Some(message) = &self.notice {
                        panel.child(notice(message.render(), ui::MUTED))
                    } else {
                        panel
                    }
                },
            )
            .when(
                self.error.is_none()
                    && self.documents[self.active_document].error.is_none()
                    && self.busy.is_none(),
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
                                .bg(rgb(ui::RAISED))
                                .border_1()
                                .border_color(rgb(ui::BORDER))
                                .text_size(px(11.))
                                .text_color(rgb(ui::SUCCESS))
                                .shadow_sm()
                                .occlude()
                                .child(message.render()),
                        )
                    })
                },
            )
            .children(self.render_menu(cx))
    }
}

impl DatabaseView {
    fn render_header(&self, cx: &Context<Self>) -> Div {
        let address = match &self.config {
            DatabaseConfig::Mysql(config) | DatabaseConfig::Postgres(config) => {
                let host = if config.host.contains(':') && !config.host.starts_with('[') {
                    format!("[{}]", config.host)
                } else {
                    config.host.clone()
                };
                let scheme = if self.engine() == SqlEngine::Mysql {
                    "mysql"
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
        div()
            .h(px(ui::PANE_HEADER_HEIGHT))
            .min_w_0()
            .flex_shrink_0()
            .px_2()
            .flex()
            .items_center()
            .gap_1()
            .bg(rgb(ui::SURFACE))
            .border_b_1()
            .border_color(rgb(ui::BORDER))
            .child(ui::connection_address(
                "database-address",
                Icon::for_sql_engine(self.engine()),
                self.engine().label(),
                address,
                format!("{}\n{}", self.title, self.status_details()),
            ))
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
                            .text_color(rgb(ui::MUTED))
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
                .when(self.browser_open, |button| {
                    button.bg(rgb(ui::SELECTED)).text_color(rgb(ui::ACCENT))
                })
                .tooltip(|_, cx| {
                    cx.new(|_| {
                        ui::Tooltip(tr(i18n_key::DATABASE_TOGGLE_DATABASE_BROWSER).to_owned())
                    })
                    .into()
                })
                .on_click(cx.listener(|this, _, window, cx| {
                    if this.interactive() {
                        this.browser_open = !this.browser_open;
                        if this.browser_open {
                            this.table_filter.read(cx).focus(window);
                        } else if this.documents[this.active_document].editor_open {
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
                .when(self.menu_open, |button| button.bg(rgb(ui::SELECTED)))
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

    fn render_browser(&self, compact: bool, cx: &Context<Self>) -> AnyElement {
        let mut browser = div()
            .min_h_0()
            .min_w_0()
            .flex()
            .flex_col()
            .border_r_1()
            .border_color(rgb(ui::BORDER))
            .when(compact, |body| body.flex_1())
            .when(!compact, |body| {
                body.w(px(self.browser_width())).flex_shrink_0()
            })
            .child(
                div()
                    .p_2()
                    .flex()
                    .flex_col()
                    .gap_2()
                    .when(self.engine() != SqlEngine::Sqlite, |header| {
                        header.child(
                            div()
                                .flex()
                                .items_center()
                                .gap_1()
                                .child(div().flex_1().min_w_0().child(self.database_input.clone()))
                                .child(
                                    ui::icon_button(
                                        "database-list-toggle",
                                        Icon::ChevronDown,
                                        i18n_key::DATABASE_CHOOSE_DATABASE,
                                    )
                                    .tab_index(0)
                                    .on_click(cx.listener(
                                        |this, _, _, cx| {
                                            if this.interactive() {
                                                this.database_open = !this.database_open;
                                                cx.notify();
                                            }
                                        },
                                    )),
                                )
                                .child(
                                    action(
                                        "database-switch",
                                        i18n_key::COMMON_GO,
                                        self.available(),
                                        ButtonStyle::Neutral,
                                    )
                                    .on_click(
                                        cx.listener(|this, _, _, cx| this.change_database(cx)),
                                    ),
                                ),
                        )
                    })
                    .child(self.table_filter.clone())
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .justify_between()
                            .child(tr(i18n_key::DATABASE_TABLES_AND_VIEWS))
                            .child(
                                ui::icon_button_enabled(
                                    "database-refresh",
                                    Icon::Refresh,
                                    if self.available() {
                                        i18n_key::DATABASE_REFRESH_DATABASE_OBJECTS
                                    } else {
                                        self.unavailable_reason()
                                    },
                                    self.available(),
                                )
                                .on_click(cx.listener(|this, _, _, cx| this.refresh_browser(cx))),
                            ),
                    ),
            );
        if self.engine() != SqlEngine::Sqlite && self.database_open {
            browser = browser.child(
                div()
                    .id("database-choices")
                    .relative()
                    .h(px((self.databases.len() as f32 * 28.).min(150.)))
                    .pr(px(ui::SCROLLBAR_WIDTH))
                    .flex_shrink_0()
                    .border_b_1()
                    .border_color(rgb(ui::BORDER))
                    .child(
                        uniform_list(
                            "database-choice-list",
                            self.databases.len(),
                            cx.processor(|this, range: std::ops::Range<usize>, _, cx| {
                                range
                                    .map(|index| this.render_database_choice(index, cx))
                                    .collect::<Vec<_>>()
                            }),
                        )
                        .map(ui::preserve_scroll_axes)
                        .track_scroll(self.database_scroll.clone())
                        .h_full()
                        .w_full(),
                    )
                    .child(ui::scrollbars(
                        "database-choice-bars",
                        None,
                        Some(self.database_scroll.0.borrow().base_handle.clone()),
                    )),
            );
        }
        if self.visible_tables.is_empty() {
            browser = browser.child(div().flex_1().p_3().text_color(rgb(ui::MUTED)).child(tr(
                if self.busy.is_some() {
                    i18n_key::DATABASE_LOADING_DATABASE_OBJECTS
                } else if self.state == State::Connected {
                    i18n_key::DATABASE_NO_MATCHING_TABLES_OR_VIEWS
                } else if self.engine() == SqlEngine::Sqlite {
                    i18n_key::DATABASE_OPEN_THE_DATABASE_TO_BROWSE_ITS_TABLES_AND_VIEWS
                } else {
                    i18n_key::DATABASE_CONNECT_TO_BROWSE_DATABASE_OBJECTS
                },
            )));
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
                            "database-tables",
                            self.visible_tables.len(),
                            cx.processor(|this, range: std::ops::Range<usize>, _, cx| {
                                range
                                    .map(|index| this.render_table(index, cx))
                                    .collect::<Vec<_>>()
                            }),
                        )
                        .map(ui::preserve_scroll_axes)
                        .track_scroll(self.table_scroll.clone())
                        .w_full()
                        .min_w_0()
                        .flex_1()
                        .min_h_0(),
                    )
                    .child(ui::scrollbars(
                        "database-table-bars",
                        None,
                        Some(self.table_scroll.0.borrow().base_handle.clone()),
                    )),
            );
        }
        browser
            .child(
                div()
                    .p_2()
                    .flex()
                    .flex_wrap()
                    .gap_1()
                    .border_t_1()
                    .border_color(rgb(ui::BORDER))
                    .children(
                        [
                            (TableAction::Data, "Data"),
                            (TableAction::Columns, "Columns"),
                            (TableAction::Indexes, "Indexes"),
                        ]
                        .into_iter()
                        .enumerate()
                        .map(|(index, (kind, label))| {
                            action(
                                ("database-table-action", index),
                                label,
                                self.available() && self.selected_table.is_some(),
                                ButtonStyle::Neutral,
                            )
                            .on_click(
                                cx.listener(move |this, _, _, cx| this.table_action(kind, cx)),
                            )
                        }),
                    ),
            )
            .into_any_element()
    }

    fn render_database_choice(&self, index: usize, cx: &Context<Self>) -> AnyElement {
        let name = self.databases[index].clone();
        div()
            .id(("database-choice", index))
            .h(px(28.))
            .px_3()
            .flex()
            .items_center()
            .cursor_pointer()
            .tab_index(0)
            .hover(|style| style.bg(rgb(ui::HOVER)))
            .when(name == self.database(), |item| item.bg(rgb(ui::SELECTED)))
            .child(div().truncate().child(name.clone()))
            .on_click(cx.listener(move |this, _, _, cx| {
                if this.available() {
                    this.database_input
                        .update(cx, |input, cx| input.set_value(name.clone(), cx));
                    this.change_database(cx);
                }
            }))
            .into_any_element()
    }

    fn render_table(&self, visible_index: usize, cx: &Context<Self>) -> AnyElement {
        let table = &self.tables[self.visible_tables[visible_index]];
        let selected = self.selected_table.as_ref() == Some(table);
        let is_view = table.kind.to_ascii_lowercase().contains("view");
        let table = table.clone();
        let name = if table.schema.is_empty() {
            table.name.clone()
        } else {
            format!("{}.{}", table.schema, table.name)
        };
        div()
            .id(("database-table", visible_index))
            .w_full()
            .min_w_0()
            .h(px(36.))
            .px_2()
            .flex()
            .items_center()
            .gap_2()
            .cursor_pointer()
            .tab_index(0)
            .when(selected, |row| row.bg(rgb(ui::SELECTED)))
            .hover(|style| style.bg(rgb(ui::HOVER)))
            .focus(|style| style.bg(rgb(ui::SELECTED)))
            .child(ui::icon(if is_view { Icon::View } else { Icon::Table }))
            .child(div().min_w_0().flex_1().truncate().child(name))
            .child(
                div()
                    .text_size(px(9.))
                    .text_color(rgb(ui::MUTED))
                    .child(if is_view {
                        tr(i18n_key::WORKSPACE_VIEW)
                    } else {
                        tr(i18n_key::DATABASE_TABLE)
                    }),
            )
            .on_click(cx.listener(move |this, event: &gpui::ClickEvent, _, cx| {
                if this.interactive() {
                    this.selected_table = Some(table.clone());
                    if event.click_count() == 2 {
                        this.table_action(TableAction::Data, cx);
                    }
                    cx.notify();
                }
            }))
            .into_any_element()
    }

    fn render_editor(&self, cx: &Context<Self>) -> AnyElement {
        let document = &self.documents[self.active_document];
        if document.value_view.is_some() {
            return self.render_value(cx);
        }
        if document.table.is_some() {
            return self.render_table_workspace(cx);
        }
        div()
            .flex_1()
            .min_w_0()
            .min_h_0()
            .flex()
            .flex_col()
            .child(
                div()
                    .px_2()
                    .py_1()
                    .flex_shrink_0()
                    .flex()
                    .flex_wrap()
                    .items_center()
                    .gap_1()
                    .child(
                        action(
                            "database-run",
                            i18n_key::DATABASE_RUN_STATEMENT,
                            self.available(),
                            ButtonStyle::Primary,
                        )
                        .tooltip(|_, cx| {
                            cx.new(|_| {
                                ui::Tooltip(
                                    tr(i18n_key::DATABASE_EXECUTE_SELECTION_OR_STATEMENT_AT_CURSOR_CTRL_ENTER)
                                        .into(),
                                )
                            })
                            .into()
                        })
                        .on_click(cx.listener(|this, _, _, cx| this.run_sql(cx))),
                    )
                    .child(
                        action(
                            "database-run-script",
                            i18n_key::DATABASE_RUN_SCRIPT,
                            self.available(),
                            ButtonStyle::Neutral,
                        )
                        .on_click(cx.listener(|this, _, _, cx| this.run_script(cx))),
                    )
                    .when(self.busy.is_some(), |bar| {
                        bar.child(
                            action(
                                "database-cancel",
                                i18n_key::DATABASE_CANCEL_QUERY,
                                self.interactive(),
                                ButtonStyle::Neutral,
                            )
                            .on_click(cx.listener(|this, _, _, cx| this.cancel_query(cx))),
                        )
                    })
                    .child(
                        ui::icon_button_enabled(
                            "database-history-previous",
                            Icon::ChevronUp,
                            i18n_key::DATABASE_PREVIOUS_QUERY,
                            self.interactive() && !document.history.is_empty(),
                        )
                        .on_click(cx.listener(|this, _, _, cx| this.recall(true, cx))),
                    )
                    .child(
                        ui::icon_button_enabled(
                            "database-history-next",
                            Icon::ChevronDown,
                            i18n_key::DATABASE_NEXT_QUERY,
                            self.interactive() && document.history_index.is_some(),
                        )
                        .on_click(cx.listener(|this, _, _, cx| this.recall(false, cx))),
                    )
                    .child(self.render_transaction(cx))
                    .child(
                        div()
                            .text_size(px(11.))
                            .text_color(rgb(ui::MUTED))
                            .child(tr(i18n_key::DATABASE_ROW_LIMIT)),
                    )
                    .child(div().w(px(64.)).child(document.row_limit.clone())),
            )
            .child(document.sql_input.clone())
            .child(self.render_results(cx))
            .into_any_element()
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
                .text_color(rgb(ui::MUTED))
                .child(tr(
                    i18n_key::DATABASE_RUN_SQL_OR_SELECT_A_TABLE_TO_INSPECT_DATA,
                ))
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
            .border_color(rgb(ui::BORDER));
        if output.results.len() > 1 {
            results = results.child(
                div()
                    .id("database-results-tabs")
                    .flex()
                    .flex_shrink_0()
                    .overflow_x_scroll()
                    .map(ui::preserve_scroll_axes)
                    .gap_1()
                    .p_1()
                    .children(output.results.iter().enumerate().map(|(index, _)| {
                        div()
                            .id(("database-result-tab", index))
                            .px_2()
                            .py_1()
                            .flex_shrink_0()
                            .cursor_pointer()
                            .tab_index(0)
                            .when(
                                index == self.documents[self.active_document].result_index,
                                |item| item.bg(rgb(ui::SELECTED)),
                            )
                            .hover(|style| style.bg(rgb(ui::HOVER)))
                            .child(rustterm::trf!(
                                i18n_key::DATABASE_RESULT_NUMBER,
                                number = index + 1
                            ))
                            .on_click(
                                cx.listener(move |this, _, _, cx| this.select_result(index, cx)),
                            )
                    }))
                    .map(|content| ui::scrollable(content, None)),
            );
        }
        results = results.child(
            div()
                .px_2()
                .py_1()
                .flex_shrink_0()
                .flex()
                .flex_wrap()
                .items_center()
                .gap_2()
                .child(
                    div()
                        .flex_1()
                        .min_w(px(120.))
                        .child(self.documents[self.active_document].result_filter.clone()),
                )
                .child(
                    ui::icon_button(
                        "database-copy",
                        Icon::Copy,
                        i18n_key::DATABASE_COPY_DISPLAYED_RESULT_AS_TSV,
                    )
                    .tab_index(0)
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
        );
        results = results.child(
            div()
                .px_2()
                .pb_1()
                .flex_shrink_0()
                .text_size(px(10.))
                .text_color(rgb(ui::MUTED))
                .child(rustterm::trf!(
                    i18n_key::DATABASE_SHOWN_TOTAL_LOADED_ROWS_AFFECTED_AFFECTED_ELAPSED_MS,
                    shown = self.documents[self.active_document].visible_rows.len(),
                    total = result.rows.len(),
                    affected = result.affected_rows,
                    elapsed = output.elapsed.as_millis()
                ))
                .when(result.truncated, |line| {
                    line.child(format!(" · {}", tr(i18n_key::DATABASE_RESULT_TRUNCATED)))
                }),
        );
        if result.columns.is_empty() {
            return results
                .child(
                    div()
                        .flex_1()
                        .p_3()
                        .text_color(rgb(ui::MUTED))
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
            .bg(rgb(ui::RAISED))
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
                    .border_color(rgb(ui::BORDER))
                    .cursor_pointer()
                    .tab_index(0)
                    .hover(|style| style.bg(rgb(ui::HOVER)))
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
                            .hover(|style| style.bg(rgb(ui::ACCENT)))
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
                    .border_color(rgb(ui::BORDER))
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
                ui::BG
            } else {
                ui::SURFACE
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
                    .border_color(rgb(ui::BORDER))
                    .cursor_pointer()
                    .when(
                        self.documents[self.active_document].selected_cell
                            == Some((row_index, column)),
                        |item| item.bg(rgb(ui::SELECTED)),
                    )
                    .hover(|style| style.bg(rgb(ui::HOVER)))
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
        .focus(|style| style.border_color(rgb(ui::ACCENT)))
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
        .border_color(rgb(ui::BORDER))
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
