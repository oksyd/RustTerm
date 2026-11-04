use super::*;
use gpui::{AnyElement, MouseButton, div, prelude::*, px, rgb};
use rustterm::i18n::messages as i18n_key;
use ui::{ButtonStyle, Icon};

impl DatabaseView {
    pub(in crate::app::database_view) fn render_table_workspace(
        &self,
        cx: &Context<Self>,
    ) -> AnyElement {
        let table = self.table_workspace().unwrap();
        let enabled = self.available();
        let editable = self.table_editable() && !table.editing();
        let toolbar = div()
            .flex_shrink_0()
            .flex()
            .items_center()
            .flex_wrap()
            .gap_1()
            .px_2()
            .py_1()
            .border_b_1()
            .border_color(rgb(ui::BORDER))
            .children(
                [
                    (TableAction::Data, "Data"),
                    (TableAction::Columns, i18n_key::DATABASE_STRUCTURE),
                    (TableAction::Indexes, "Indexes"),
                ]
                .into_iter()
                .enumerate()
                .map(|(index, (section, label))| {
                    super::super::render::action(
                        ("table-section", index),
                        label,
                        enabled,
                        ButtonStyle::Neutral,
                    )
                    .when(table.section == section, |button| {
                        button.bg(rgb(ui::SELECTED)).text_color(rgb(ui::ACCENT))
                    })
                    .on_click(
                        cx.listener(move |this, _, _, cx| this.select_table_section(section, cx)),
                    )
                }),
            )
            .child(self.render_transaction(cx))
            .child(
                ui::icon_button_enabled(
                    "refresh-table",
                    Icon::Refresh,
                    i18n_key::DATABASE_REFRESH_TABLE,
                    enabled && !table.has_local_edit(),
                )
                .on_click(cx.listener(|this, _, _, cx| this.refresh_active_table(cx))),
            )
            .when(self.busy.is_some(), |bar| {
                bar.child(
                    super::super::render::action(
                        "cancel-table-query",
                        i18n_key::DATABASE_CANCEL_QUERY,
                        self.interactive(),
                        ButtonStyle::Neutral,
                    )
                    .on_click(cx.listener(|this, _, _, cx| this.cancel_query(cx))),
                )
            });
        let mut body = div()
            .relative()
            .flex_1()
            .min_h_0()
            .min_w_0()
            .flex()
            .flex_col()
            .child(toolbar);
        if table.section == TableAction::Data {
            body = body.child(self.render_table_filter(cx));
            let selected = self.documents[self.active_document].selected_cell;
            let deleted = selected.is_some_and(|(row, _)| {
                matches!(table.changes.get(&row), Some(RowChange::Delete { .. }))
            });
            body = body.child(
                div()
                    .px_2()
                    .py_1()
                    .flex_shrink_0()
                    .flex()
                    .flex_wrap()
                    .items_center()
                    .gap_1()
                    .child(
                        super::super::render::action(
                            "table-add-row",
                            i18n_key::DATABASE_ADD_ROW,
                            editable,
                            ButtonStyle::Neutral,
                        )
                        .on_click(
                            cx.listener(|this, _, window, cx| this.add_table_row(window, cx)),
                        ),
                    )
                    .child(
                        super::super::render::action(
                            "table-delete-row",
                            if deleted {
                                i18n_key::DATABASE_RESTORE_ROW
                            } else {
                                i18n_key::DATABASE_DELETE_ROW
                            },
                            editable && selected.is_some(),
                            ButtonStyle::Neutral,
                        )
                        .on_click(cx.listener(|this, _, _, cx| this.delete_table_row(cx))),
                    )
                    .child(div().flex_1())
                    .when(!table.changes.is_empty(), |bar| {
                        bar.child(div().text_size(px(11.)).text_color(rgb(ui::WARNING)).child(
                            rustterm::trf!(
                                i18n_key::DATABASE_PENDING_ROW_COUNT,
                                count = table.changes.len()
                            ),
                        ))
                    })
                    .child(
                        super::super::render::action(
                            "table-discard",
                            i18n_key::COMMON_DISCARD,
                            enabled && !table.changes.is_empty(),
                            ButtonStyle::Neutral,
                        )
                        .on_click(cx.listener(|this, _, _, cx| this.discard_table_changes(cx))),
                    )
                    .child(
                        super::super::render::action(
                            "table-review",
                            i18n_key::DATABASE_REVIEW_CHANGES,
                            editable && !table.changes.is_empty(),
                            ButtonStyle::Primary,
                        )
                        .on_click(cx.listener(|this, _, _, cx| this.review_table_changes(cx))),
                    ),
            );
            let reason = if matches!(&self.config, DatabaseConfig::Sqlite(config) if config.read_only)
            {
                Some(i18n_key::COMMON_READ_ONLY_BADGE)
            } else {
                table
                    .schema
                    .as_ref()
                    .and_then(|schema| schema.read_only_reason)
            };
            if let Some(reason) = reason {
                body = body.child(
                    div()
                        .px_2()
                        .py_1()
                        .text_size(px(11.))
                        .text_color(rgb(ui::MUTED))
                        .child(tr(reason)),
                );
            }
        }
        if table.review {
            body = body.child(self.render_change_review(cx));
        } else {
            body = body.child(self.render_results(cx));
            if table.section == TableAction::Data {
                body = body.child(self.render_table_pagination(cx));
                if table.edit.as_ref().is_some_and(|edit| edit.expanded) {
                    body = body.child(
                        div()
                            .absolute()
                            .top_0()
                            .bottom_0()
                            .right_0()
                            .w(px(380.))
                            .max_w_full()
                            .bg(rgb(ui::SURFACE))
                            .border_l_1()
                            .border_color(rgb(ui::BORDER))
                            .shadow_md()
                            .occlude()
                            .flex()
                            .flex_col()
                            .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
                            .child(self.render_cell_editor(cx)),
                    );
                }
            }
        }
        body.children(self.render_filter_picker(cx))
            .into_any_element()
    }

    fn render_table_filter(&self, cx: &Context<Self>) -> AnyElement {
        let table = self.table_workspace().unwrap();
        let enabled = self.available() && table.schema.is_some() && !table.has_local_edit();
        let column = table
            .schema
            .as_ref()
            .and_then(|schema| schema.columns.get(table.filter_column))
            .map(|column| column.name.clone())
            .unwrap_or_else(|| tr(i18n_key::DATABASE_COLUMN).into());
        div()
            .px_2()
            .py_1()
            .flex_shrink_0()
            .flex()
            .flex_wrap()
            .items_center()
            .gap_1()
            .child(
                div()
                    .id("filter-column")
                    .w(px(142.))
                    .h(px(30.))
                    .px_2()
                    .flex()
                    .items_center()
                    .justify_between()
                    .rounded_sm()
                    .bg(rgb(ui::HOVER))
                    .cursor_pointer()
                    .tab_index(0)
                    .child(div().min_w_0().truncate().child(column))
                    .child(ui::icon(Icon::ChevronDown))
                    .on_click(cx.listener(move |this, _, _, cx| {
                        if enabled {
                            let table =
                                this.documents[this.active_document].table.as_mut().unwrap();
                            table.picker = if table.picker == Some(Picker::Column) {
                                None
                            } else {
                                Some(Picker::Column)
                            };
                            cx.notify();
                        }
                    })),
            )
            .child(
                div()
                    .id("filter-operator")
                    .w(px(106.))
                    .h(px(30.))
                    .px_2()
                    .flex()
                    .items_center()
                    .justify_between()
                    .rounded_sm()
                    .bg(rgb(ui::HOVER))
                    .cursor_pointer()
                    .tab_index(0)
                    .child(tr(table.filter_operator.label()))
                    .child(ui::icon(Icon::ChevronDown))
                    .on_click(cx.listener(move |this, _, _, cx| {
                        if enabled {
                            let table =
                                this.documents[this.active_document].table.as_mut().unwrap();
                            table.picker = if table.picker == Some(Picker::Operator) {
                                None
                            } else {
                                Some(Picker::Operator)
                            };
                            cx.notify();
                        }
                    })),
            )
            .when(table.filter_operator.needs_value(), |row| {
                row.child(
                    div()
                        .flex_1()
                        .min_w(px(100.))
                        .child(table.filter_input.clone()),
                )
            })
            .child(
                super::super::render::action(
                    "apply-server-filter",
                    i18n_key::DATABASE_APPLY_FILTER,
                    enabled,
                    ButtonStyle::Neutral,
                )
                .on_click(cx.listener(|this, _, _, cx| this.apply_table_filter(cx))),
            )
            .child(
                ui::icon_button_enabled(
                    "clear-server-filter",
                    Icon::Close,
                    i18n_key::WORKSPACE_CLEAR_FILTER,
                    enabled && table.request.filter.is_some(),
                )
                .on_click(cx.listener(|this, _, _, cx| this.clear_table_filter(cx))),
            )
            .into_any_element()
    }

    fn render_filter_picker(&self, cx: &Context<Self>) -> Option<AnyElement> {
        let table = self.table_workspace()?;
        let picker = table.picker?;
        let choices: Vec<String> = match picker {
            Picker::Column => table
                .schema
                .as_ref()?
                .columns
                .iter()
                .map(|column| column.name.clone())
                .collect(),
            Picker::Operator => FilterOperator::ALL
                .iter()
                .map(|operator| operator.label().into())
                .collect(),
        };
        Some(
            div()
                .id("table-filter-picker")
                .absolute()
                .top(px(68.))
                .left(px(if picker == Picker::Column { 8. } else { 154. }))
                .w(px(240.))
                .max_h(px(240.))
                .overflow_y_scroll()
                .map(ui::preserve_scroll_axes)
                .bg(rgb(ui::RAISED))
                .border_1()
                .border_color(rgb(ui::BORDER))
                .shadow_md()
                .occlude()
                .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
                .on_mouse_down_out(cx.listener(|this, _, _, cx| {
                    if let Some(table) = this.documents[this.active_document].table.as_mut() {
                        table.picker = None;
                        cx.notify();
                    }
                }))
                .children(choices.into_iter().enumerate().map(|(index, label)| {
                    div()
                        .id(("table-filter-choice", index))
                        .px_2()
                        .py_1()
                        .cursor_pointer()
                        .tab_index(0)
                        .hover(|style| style.bg(rgb(ui::HOVER)))
                        .child(label)
                        .on_click(cx.listener(move |this, _, _, cx| {
                            let table =
                                this.documents[this.active_document].table.as_mut().unwrap();
                            match picker {
                                Picker::Column => table.filter_column = index,
                                Picker::Operator => {
                                    table.filter_operator = FilterOperator::ALL[index]
                                }
                            }
                            table.picker = None;
                            cx.notify();
                        }))
                }))
                .map(|content| ui::scrollable(content, None))
                .into_any_element(),
        )
    }

    fn render_table_pagination(&self, cx: &Context<Self>) -> AnyElement {
        let table = self.table_workspace().unwrap();
        let enabled = self.available() && !table.has_local_edit();
        let offset = table.page.as_ref().map_or(0, |page| page.offset);
        let count = table.page.as_ref().map_or(0, |page| page.result.rows.len());
        div()
            .px_2()
            .py_1()
            .flex_shrink_0()
            .flex()
            .items_center()
            .gap_2()
            .border_t_1()
            .border_color(rgb(ui::BORDER))
            .child(
                ui::icon_button_enabled(
                    "previous-table-page",
                    Icon::ChevronLeft,
                    i18n_key::DATABASE_PREVIOUS_PAGE,
                    enabled && offset > 0,
                )
                .on_click(cx.listener(|this, _, _, cx| this.table_page_step(false, cx))),
            )
            .child(div().text_size(px(11.)).child(rustterm::trf!(
                i18n_key::DATABASE_ROWS_FIRST_LAST,
                first = if count == 0 { 0 } else { offset + 1 },
                last = offset + count as u64
            )))
            .child(
                ui::icon_button_enabled(
                    "next-table-page",
                    Icon::ChevronRight,
                    i18n_key::DATABASE_NEXT_PAGE,
                    enabled && table.page.as_ref().is_some_and(|page| page.has_next),
                )
                .on_click(cx.listener(|this, _, _, cx| this.table_page_step(true, cx))),
            )
            .child(div().flex_1())
            .child(
                div()
                    .text_size(px(11.))
                    .text_color(rgb(ui::MUTED))
                    .child(tr(i18n_key::DATABASE_ROWS_PER_PAGE)),
            )
            .child(
                div()
                    .w(px(64.))
                    .child(self.documents[self.active_document].row_limit.clone()),
            )
            .child(
                super::super::render::action(
                    "apply-page-size",
                    "Apply",
                    enabled,
                    ButtonStyle::Neutral,
                )
                .on_click(cx.listener(|this, _, _, cx| {
                    if let Some(table) = this.table_workspace() {
                        let mut request = table.request.clone();
                        request.offset = 0;
                        this.load_table_page(request, cx);
                    }
                })),
            )
            .into_any_element()
    }

    pub(in crate::app::database_view) fn inline_cell_input(
        &self,
        row: usize,
        column: usize,
    ) -> Option<Entity<TextInput>> {
        let edit = self.table_workspace()?.edit.as_ref()?;
        (!edit.expanded && (edit.row, edit.column) == (row, column)).then(|| edit.input.clone())
    }

    pub(in crate::app::database_view) fn render_inline_modes(
        &self,
        cx: &Context<Self>,
    ) -> Option<AnyElement> {
        let table = self.table_workspace()?;
        let edit = table.edit.as_ref().filter(|edit| !edit.expanded)?;
        let column = &table.schema.as_ref()?.columns[edit.column];
        Some(
            div()
                .flex()
                .gap_1()
                .children(
                    [
                        (CellMode::Value, i18n_key::CONNECTIONS_VALUE, true),
                        (CellMode::Null, "NULL", column.nullable),
                        (
                            CellMode::Default,
                            "DEFAULT",
                            matches!(table.changes.get(&edit.row), Some(RowChange::Insert { .. })),
                        ),
                    ]
                    .into_iter()
                    .filter(|(_, _, allowed)| *allowed)
                    .enumerate()
                    .map(|(index, (mode, label, _))| {
                        super::super::render::action(
                            ("inline-cell-mode", index),
                            label,
                            self.available(),
                            ButtonStyle::Neutral,
                        )
                        .when(edit.mode == mode, |button| {
                            button.bg(rgb(ui::SELECTED)).text_color(rgb(ui::ACCENT))
                        })
                        .on_click(cx.listener(
                            move |this, _, window, cx| this.set_cell_mode(mode, window, cx),
                        ))
                    }),
                )
                .into_any_element(),
        )
    }

    fn render_cell_editor(&self, cx: &Context<Self>) -> AnyElement {
        let table = self.table_workspace().unwrap();
        let edit = table.edit.as_ref().unwrap();
        let column = &table.schema.as_ref().unwrap().columns[edit.column];
        div()
            .flex_1()
            .min_h_0()
            .flex()
            .flex_col()
            .gap_2()
            .p_2()
            .border_t_1()
            .border_color(rgb(ui::ACCENT))
            .child(
                div()
                    .flex()
                    .items_center()
                    .flex_wrap()
                    .flex_shrink_0()
                    .gap_2()
                    .child(
                        div()
                            .font_weight(gpui::FontWeight::MEDIUM)
                            .child(column.name.clone()),
                    )
                    .child(
                        div()
                            .text_color(rgb(ui::MUTED))
                            .text_size(px(11.))
                            .child(column.data_type.clone()),
                    )
                    .child(div().flex_1())
                    .children(
                        [
                            (CellMode::Value, i18n_key::CONNECTIONS_VALUE),
                            (CellMode::Null, "NULL"),
                            (CellMode::Default, "DEFAULT"),
                        ]
                        .into_iter()
                        .filter(|(mode, _)| {
                            *mode != CellMode::Default
                                || matches!(
                                    table.changes.get(&edit.row),
                                    Some(RowChange::Insert { .. })
                                )
                        })
                        .enumerate()
                        .map(|(index, (mode, label))| {
                            super::super::render::action(
                                ("cell-mode", index),
                                label,
                                self.available() && (mode != CellMode::Null || column.nullable),
                                ButtonStyle::Neutral,
                            )
                            .when(edit.mode == mode, |button| {
                                button.bg(rgb(ui::SELECTED)).text_color(rgb(ui::ACCENT))
                            })
                            .on_mouse_down(MouseButton::Left, |_, window, cx| {
                                window.prevent_default();
                                cx.stop_propagation();
                            })
                            .on_click(cx.listener(
                                move |this, _, window, cx| {
                                    this.set_cell_mode(mode, window, cx);
                                },
                            ))
                        }),
                    ),
            )
            .when(edit.mode == CellMode::Value, |body| {
                body.child(
                    div()
                        .id("cell-editor-scroll")
                        .flex_1()
                        .min_h_0()
                        .overflow_y_scroll()
                        .map(ui::preserve_scroll_axes)
                        .child(edit.input.clone())
                        .map(|content| ui::scrollable(content, None)),
                )
            })
            .when(edit.mode != CellMode::Value, |body| {
                body.child(div().flex_1().text_color(rgb(ui::MUTED)).child(
                    if edit.mode == CellMode::Null {
                        "NULL"
                    } else {
                        "DEFAULT"
                    },
                ))
            })
            .child(
                div()
                    .flex()
                    .justify_end()
                    .gap_2()
                    .child(
                        super::super::render::action(
                            "cancel-cell-edit",
                            i18n_key::COMMON_CANCEL,
                            self.interactive(),
                            ButtonStyle::Neutral,
                        )
                        .on_click(cx.listener(|this, _, window, cx| {
                            this.cancel_cell_edit(window, cx);
                        })),
                    )
                    .child(
                        super::super::render::action(
                            "stage-cell-edit",
                            i18n_key::DATABASE_STAGE_CHANGE,
                            self.available(),
                            ButtonStyle::Primary,
                        )
                        .on_mouse_down(MouseButton::Left, |_, window, cx| {
                            window.prevent_default();
                            cx.stop_propagation();
                        })
                        .on_click(cx.listener(|this, _, window, cx| {
                            this.stage_cell(window, cx);
                        })),
                    ),
            )
            .into_any_element()
    }

    fn render_change_review(&self, cx: &Context<Self>) -> AnyElement {
        let table = self.table_workspace().unwrap();
        let schema = table.schema.as_ref().unwrap();
        let mut rows = Vec::new();
        for (row, change) in &table.changes {
            let label = match change {
                RowChange::Insert { .. } => i18n_key::DATABASE_INSERT_ROW,
                RowChange::Update { .. } => i18n_key::DATABASE_UPDATE_ROW,
                RowChange::Delete { .. } => i18n_key::DATABASE_DELETE_ROW,
            };
            let mut item = div()
                .py_2()
                .border_b_1()
                .border_color(rgb(ui::BORDER))
                .child(div().font_weight(gpui::FontWeight::MEDIUM).child(format!(
                    "{} · {}",
                    tr(label),
                    row + 1
                )));
            match change {
                RowChange::Insert { values } | RowChange::Update { values, .. } => {
                    for (column, value) in values {
                        let before = match change {
                            RowChange::Update { original, .. } => super::super::render::preview(
                                original[*column].display().as_deref(),
                                200,
                            ),
                            _ => "DEFAULT".into(),
                        };
                        item = item.child(
                            div()
                                .py_1()
                                .font_family(ui::MONOSPACE_FONT)
                                .text_size(px(12.))
                                .whitespace_normal()
                                .child(format!(
                                    "{}: {} → {}",
                                    schema.columns[*column].name,
                                    before,
                                    super::super::render::preview(value.display().as_deref(), 200)
                                )),
                        );
                    }
                }
                RowChange::Delete { original } => {
                    for column in schema.primary_key() {
                        item = item.child(div().text_size(px(12.)).child(format!(
                            "{} = {}",
                            schema.columns[column].name,
                            super::super::render::preview(
                                original[column].display().as_deref(),
                                200
                            )
                        )));
                    }
                }
            }
            rows.push(item);
        }
        div()
            .flex_1()
            .min_h_0()
            .flex()
            .flex_col()
            .child(
                div()
                    .id("table-change-review")
                    .flex_1()
                    .min_h_0()
                    .overflow_y_scroll()
                    .map(ui::preserve_scroll_axes)
                    .p_3()
                    .children(rows)
                    .child(
                        div()
                            .mt_3()
                            .text_color(rgb(ui::MUTED))
                            .text_size(px(11.))
                            .child(tr(i18n_key::DATABASE_SQL_PREVIEW)),
                    )
                    .children(table.preview.iter().map(|sql| {
                        div()
                            .py_2()
                            .font_family(ui::MONOSPACE_FONT)
                            .text_size(px(12.))
                            .whitespace_normal()
                            .child(sql.clone())
                    }))
                    .map(|content| ui::scrollable(content, None)),
            )
            .child(
                div()
                    .p_2()
                    .flex_shrink_0()
                    .flex()
                    .justify_end()
                    .gap_2()
                    .border_t_1()
                    .border_color(rgb(ui::BORDER))
                    .child(
                        super::super::render::action(
                            "back-to-table",
                            i18n_key::DATABASE_BACK_TO_DATA,
                            self.available(),
                            ButtonStyle::Neutral,
                        )
                        .on_click(cx.listener(|this, _, _, cx| {
                            this.documents[this.active_document]
                                .table
                                .as_mut()
                                .unwrap()
                                .review = false;
                            cx.notify();
                        })),
                    )
                    .child(
                        super::super::render::action(
                            "submit-table-changes",
                            i18n_key::DATABASE_SUBMIT_CHANGES,
                            self.table_editable(),
                            ButtonStyle::Primary,
                        )
                        .on_click(cx.listener(|this, _, _, cx| this.submit_table_changes(cx))),
                    ),
            )
            .into_any_element()
    }
}
