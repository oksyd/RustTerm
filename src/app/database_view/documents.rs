use super::*;
use gpui::{AnyElement, MouseButton, div, prelude::*, px, rgb};
use rustterm::i18n::messages as i18n_key;
use table_workspace::TableWorkspace;
mod drafts;

pub(super) struct Document {
    pub id: u64,
    pub title: Message,
    pub table: Option<TableWorkspace>,
    pub error: Option<Message>,
    pub sql_input: Entity<TextInput>,
    pub row_limit: Entity<TextInput>,
    pub result_filter: Entity<TextInput>,
    pub output: Option<Arc<QueryOutput>>,
    pub result_index: usize,
    pub result_revision: usize,
    pub visible_rows: Vec<usize>,
    pub sort: Option<(usize, bool)>,
    pub selected_cell: Option<(usize, usize)>,
    pub column_widths: Vec<Vec<f32>>,
    pub value_view: Option<ValueView>,
    pub result_scroll: gpui::UniformListScrollHandle,
    pub horizontal_scroll: gpui::ScrollHandle,
    pub history: Vec<String>,
    pub history_index: Option<usize>,
    pub history_draft: String,
    pub editor_dirty: bool,
    pub editor_revision: usize,
    pub editor_open: bool,
    _subscriptions: Vec<Subscription>,
}

#[derive(PartialEq, Eq)]
struct DiscardVersion {
    revision: usize,
    sql: String,
    table: Option<table_workspace::snapshot::DiscardState>,
}

impl Document {
    pub fn new(
        id: u64,
        title: impl Into<Message>,
        engine: SqlEngine,
        cx: &mut Context<DatabaseView>,
    ) -> Self {
        let sql_input = cx.new(|cx| {
            let mut input = TextInput::new(
                i18n_key::DATABASE_WRITE_SQL_CTRL_ENTER_RUNS_THE_CURRENT_STATEMENT,
                false,
                cx,
            );
            input.set_sql_dialect(engine, cx);
            input
        });
        let row_limit = cx.new(|cx| {
            let mut input = TextInput::new(i18n_key::DATABASE_RESULT_ROW_LIMIT, false, cx);
            input.set_value(DEFAULT_ROW_LIMIT.to_string(), cx);
            input
        });
        let result_filter =
            cx.new(|cx| TextInput::new(i18n_key::DATABASE_FIND_IN_LOADED_ROWS, false, cx));
        let subscriptions = Self::bind_inputs(id, &sql_input, &result_filter, cx);
        Self {
            id,
            title: title.into(),
            table: None,
            error: None,
            sql_input,
            row_limit,
            result_filter,
            output: None,
            result_index: 0,
            result_revision: 0,
            visible_rows: Vec::new(),
            sort: None,
            selected_cell: None,
            column_widths: Vec::new(),
            value_view: None,
            result_scroll: gpui::UniformListScrollHandle::default(),
            horizontal_scroll: gpui::ScrollHandle::new(),
            history: Vec::new(),
            history_index: None,
            history_draft: String::new(),
            editor_dirty: false,
            editor_revision: 0,
            editor_open: true,
            _subscriptions: subscriptions,
        }
    }

    fn bind_inputs(
        id: u64,
        sql_input: &Entity<TextInput>,
        result_filter: &Entity<TextInput>,
        cx: &mut Context<DatabaseView>,
    ) -> Vec<Subscription> {
        vec![
            cx.subscribe(sql_input, move |this, _, event, cx| {
                if this.closed {
                    return;
                }
                let Some(index) = this.document_index(id) else {
                    return;
                };
                match event {
                    InputEvent::Changed => {
                        this.documents[index].editor_dirty = true;
                        this.documents[index].editor_revision += 1;
                        cx.notify();
                    }
                    InputEvent::Submit if index == this.active_document => this.run_sql(cx),
                    InputEvent::Rejected(message) => {
                        this.documents[index].error = Some((*message).into());
                        cx.notify();
                    }
                    _ => {}
                }
            }),
            cx.subscribe(result_filter, move |this, _, event, cx| {
                if !this.closed
                    && matches!(event, InputEvent::Changed)
                    && let Some(index) = this.document_index(id)
                {
                    this.documents[index].filter_rows(cx);
                    cx.notify();
                }
            }),
        ]
    }

    pub fn dirty(&self) -> bool {
        self.editor_dirty
            || self
                .table
                .as_ref()
                .is_some_and(TableWorkspace::has_local_edit)
    }

    fn discard_version(&self, cx: &App) -> DiscardVersion {
        DiscardVersion {
            revision: self.editor_revision,
            sql: self.sql_input.read(cx).value().to_owned(),
            table: self.table.as_ref().map(|table| table.discard_state(cx)),
        }
    }

    fn focus(&self, window: &mut Window, cx: &App) -> bool {
        if self.value_view.is_some() {
            return false;
        }
        if let Some(table) = &self.table {
            table.focus_edit(window, cx)
        } else if self.editor_open {
            self.sql_input.read(cx).focus(window);
            true
        } else {
            false
        }
    }

    pub fn current_result(&self) -> Option<&ResultSet> {
        self.output.as_ref()?.results.get(self.result_index)
    }

    pub fn install_output(&mut self, output: QueryOutput, cx: &App) {
        self.column_widths = output
            .results
            .iter()
            .map(|result| vec![sizing::DEFAULT_COLUMN_WIDTH; result.columns.len()])
            .collect();
        self.output = Some(Arc::new(output));
        self.result_index = 0;
        self.result_revision += 1;
        self.sort = None;
        self.error = None;
        self.selected_cell = None;
        self.value_view = None;
        self.horizontal_scroll.set_offset(gpui::Point::default());
        self.filter_rows(cx);
    }

    pub fn filter_rows(&mut self, cx: &App) {
        let needle = self.result_filter.read(cx).value().to_lowercase();
        let mut visible_rows = self
            .current_result()
            .map(|result| results::visible_rows(result, &needle, self.sort))
            .unwrap_or_default();
        let valid_cell = |&(row, column): &(usize, usize)| {
            self.current_result()
                .and_then(|result| result.rows.get(row))
                .is_some_and(|row| column < row.len())
        };
        let selected = self.selected_cell.filter(valid_cell);
        let editing = self
            .table
            .as_ref()
            .and_then(TableWorkspace::editing_cell)
            .filter(valid_cell);
        self.selected_cell =
            retain_filter_selection(&mut visible_rows, &self.visible_rows, selected, editing);
        self.visible_rows = visible_rows;
        self.value_view = None;
        let visible = self
            .selected_cell
            .and_then(|(row, _)| self.visible_rows.iter().position(|index| *index == row))
            .unwrap_or(0);
        self.result_scroll
            .scroll_to_item(visible, gpui::ScrollStrategy::Top);
    }
}

fn retain_filter_selection(
    visible_rows: &mut Vec<usize>,
    previous_rows: &[usize],
    selected: Option<(usize, usize)>,
    editing: Option<(usize, usize)>,
) -> Option<(usize, usize)> {
    if let Some((row, column)) = editing {
        if !visible_rows.contains(&row) {
            let position = previous_rows
                .iter()
                .position(|index| *index == row)
                .unwrap_or(visible_rows.len())
                .min(visible_rows.len());
            visible_rows.insert(position, row);
        }
        Some((row, column))
    } else {
        selected.filter(|(row, _)| visible_rows.contains(row))
    }
}

impl DatabaseView {
    pub(super) fn document_index(&self, id: u64) -> Option<usize> {
        self.documents.iter().position(|document| document.id == id)
    }

    pub(super) fn new_query(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if !self.interactive() {
            return;
        }
        let id = self.next_document_id;
        self.next_document_id += 1;
        let document = Document::new(
            id,
            rustterm::msg!(i18n_key::DATABASE_QUERY_NUMBER, number = id),
            self.engine(),
            cx,
        );
        self.documents.push(document);
        self.select_document(self.documents.len() - 1, window, cx);
        self.update_completions(cx);
    }

    pub(super) fn select_document(
        &mut self,
        index: usize,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if !self.interactive() {
            return;
        }
        let Some(document) = self.documents.get(index) else {
            return;
        };
        self.active_document = index;
        self.selected_table = document.table.as_ref().map(|state| state.info.clone());
        self.resize_drag = None;
        self.success_notice = None;
        self.success_task = None;
        self.notice = None;
        if self.viewport_size.0 < BROWSER_MIN_WIDTH {
            self.browser_open = false;
        }
        if !document.focus(window, cx) {
            self.focus.focus(window);
        }
        let unloaded = document
            .table
            .as_ref()
            .filter(|_| document.output.is_none())
            .map(|table| table.section);
        if let Some(section) = unloaded
            && self.available()
        {
            self.select_table_section(section, cx);
        }
        cx.notify();
    }

    pub(super) fn close_document(&mut self, id: u64, window: &mut Window, cx: &mut Context<Self>) {
        if !self.interactive() || self.running_document == Some(id) || self.input_composing(cx) {
            return;
        }
        let Some(index) = self.document_index(id) else {
            return;
        };
        if self.documents[index].dirty() {
            let Some(permit) = self.window_state.prompts.acquire() else {
                return;
            };
            let version = self.documents[index].discard_version(cx);
            let generation = self.generation;
            let focus = window.focused(cx);
            self.native_pending = true;
            let answer = window.prompt(
                gpui::PromptLevel::Warning,
                tr(i18n_key::DATABASE_DISCARD_THIS_PAGE_S_CHANGES),
                None,
                &[tr(i18n_key::COMMON_CANCEL), tr(i18n_key::COMMON_DISCARD)],
                cx,
            );
            cx.spawn_in(window, async move |this, cx| {
                let discard = answer.await == Ok(1);
                let _ = this.update_in(cx, |this, window, cx| {
                    let valid = permit.is_valid();
                    drop(permit);
                    this.native_pending = false;
                    if this.closed || this.generation != generation || !valid || !this.interactive()
                    {
                        cx.notify();
                        return;
                    }
                    if discard
                        && this.running_document != Some(id)
                        && let Some(index) = this.document_index(id)
                        && this.documents[index].discard_version(cx) == version
                    {
                        let background = index != this.active_document;
                        this.remove_document(index, window, cx);
                        if background && let Some(focus) = focus {
                            focus.focus(window);
                        }
                    } else if let Some(focus) = focus {
                        focus.focus(window);
                    }
                    cx.notify();
                });
            })
            .detach();
            cx.notify();
        } else {
            self.remove_document(index, window, cx);
        }
    }

    fn remove_document(&mut self, index: usize, window: &mut Window, cx: &mut Context<Self>) {
        let Some(document) = self.documents.get(index) else {
            return;
        };
        if !self.interactive() || self.running_document == Some(document.id) {
            return;
        }
        let closing_active = index == self.active_document;
        self.documents.remove(index);
        if self.documents.is_empty() {
            let id = self.next_document_id;
            self.next_document_id += 1;
            self.documents.push(Document::new(
                id,
                rustterm::msg!(i18n_key::DATABASE_QUERY_NUMBER, number = id),
                self.engine(),
                cx,
            ));
            self.active_document = 0;
        } else if index < self.active_document {
            self.active_document -= 1;
        } else {
            self.active_document = self.active_document.min(self.documents.len() - 1);
        }
        self.update_completions(cx);
        if closing_active {
            self.select_document(self.active_document, window, cx);
        } else {
            cx.notify();
        }
    }

    pub(super) fn update_completions(&mut self, cx: &mut Context<Self>) {
        let engine = self.engine();
        let mut names: Vec<_> = self
            .tables
            .iter()
            .map(|table| {
                let name = quote_identifier(engine, &table.name);
                if table.schema.is_empty() {
                    name
                } else {
                    format!("{}.{}", quote_identifier(engine, &table.schema), name)
                }
            })
            .collect();
        for document in &self.documents {
            if let Some(schema) = document
                .table
                .as_ref()
                .and_then(|table| table.schema.as_ref())
            {
                names.extend(
                    schema
                        .columns
                        .iter()
                        .map(|column| quote_identifier(engine, &column.name)),
                );
            }
        }
        for document in &self.documents {
            document
                .sql_input
                .update(cx, |input, cx| input.set_sql_completions(names.clone(), cx));
        }
    }

    pub(super) fn render_documents(&self, cx: &Context<Self>) -> AnyElement {
        div()
            .id("database-documents")
            .h(px(34.))
            .flex_shrink_0()
            .flex()
            .items_center()
            .bg(rgb(ui::CHROME))
            .border_b_1()
            .border_color(rgb(ui::BORDER))
            .child(
                div()
                    .id("database-document-scroll")
                    .flex_1()
                    .min_w_0()
                    .h_full()
                    .overflow_x_scroll()
                    .map(ui::preserve_scroll_axes)
                    .flex()
                    .children(self.documents.iter().enumerate().map(|(index, document)| {
                        let id = document.id;
                        div()
                            .id(("database-document", id))
                            .tab_index(0)
                            .h_full()
                            .max_w(px(240.))
                            .flex_shrink_0()
                            .flex()
                            .items_center()
                            .gap_2()
                            .px_2()
                            .border_r_1()
                            .border_color(rgb(ui::BORDER))
                            .cursor_pointer()
                            .when(index == self.active_document, |d| {
                                d.bg(rgb(ui::BG)).text_color(rgb(ui::ACCENT))
                            })
                            .hover(|s| s.bg(rgb(ui::HOVER)))
                            .child(ui::icon(if document.table.is_some() {
                                ui::Icon::Table
                            } else {
                                ui::Icon::Terminal
                            }))
                            .child(div().truncate().child(document.title.render()))
                            .when(document.dirty(), |d| {
                                d.child(div().text_color(rgb(ui::WARNING)).child("•"))
                            })
                            .when(self.running_document == Some(id), |d| {
                                d.child(div().text_color(rgb(ui::MUTED)).child("…"))
                            })
                            .on_click(cx.listener(move |this, _, window, cx| {
                                this.select_document(index, window, cx)
                            }))
                            .child(
                                ui::icon_button_enabled(
                                    ("close-database-document", id),
                                    ui::Icon::Close,
                                    i18n_key::DATABASE_CLOSE_PAGE,
                                    self.running_document != Some(id),
                                )
                                .size(px(22.))
                                .on_mouse_down(MouseButton::Left, |_, window, cx| {
                                    window.prevent_default();
                                    cx.stop_propagation();
                                })
                                .on_click(cx.listener(
                                    move |this, _, window, cx| {
                                        this.close_document(id, window, cx);
                                        cx.stop_propagation();
                                    },
                                )),
                            )
                    }))
                    .map(|content| ui::scrollable(content, None)),
            )
            .child(
                ui::icon_button(
                    "new-database-query",
                    ui::Icon::Plus,
                    i18n_key::DATABASE_NEW_QUERY,
                )
                .tab_index(0)
                .on_click(cx.listener(|this, _, window, cx| this.new_query(window, cx))),
            )
            .into_any_element()
    }
}

#[cfg(test)]
mod tests {
    use super::retain_filter_selection;

    #[test]
    fn filtering_pins_an_unfinished_edit_and_releases_it_after_completion() {
        let previous = [8, 3, 6, 4];
        let mut filtered = vec![8, 4];
        assert_eq!(
            retain_filter_selection(&mut filtered, &previous, Some((8, 0)), Some((6, 2))),
            Some((6, 2))
        );
        assert_eq!(filtered, vec![8, 4, 6]);

        let mut finished = vec![8, 4];
        assert_eq!(
            retain_filter_selection(&mut finished, &filtered, Some((6, 2)), None),
            None
        );
        assert_eq!(finished, vec![8, 4]);
    }

    #[test]
    fn filtering_keeps_visible_selection_and_never_duplicates_the_edit_row() {
        let previous = [5, 9, 2];
        let mut filtered = vec![9, 2];
        assert_eq!(
            retain_filter_selection(&mut filtered, &previous, Some((2, 1)), None),
            Some((2, 1))
        );
        assert_eq!(
            retain_filter_selection(&mut filtered, &previous, Some((2, 1)), Some((9, 0))),
            Some((9, 0))
        );
        assert_eq!(filtered, vec![9, 2]);

        let mut empty = Vec::new();
        assert_eq!(
            retain_filter_selection(&mut empty, &previous, None, Some((2, 1))),
            Some((2, 1))
        );
        assert_eq!(empty, vec![2]);
    }
}
