mod changes;
mod editing;
mod render;
pub(super) mod snapshot;

use super::*;
use rustterm::database::table::{
    CellValue, ColumnFilter, FilterOperator, PageRequest, RowChange, SortOrder, TablePage,
    TableSchema,
};
use rustterm::i18n::messages as i18n_key;
use std::collections::{BTreeMap, HashMap};

#[derive(Clone, Copy, PartialEq, Eq)]
enum Picker {
    Column,
    Operator,
}

pub(super) struct TableWorkspace {
    pub info: TableInfo,
    pub schema: Option<TableSchema>,
    pub page: Option<TablePage>,
    pub request: PageRequest,
    pub changes: BTreeMap<usize, RowChange>,
    submission_uncertain: bool,
    pub section: TableAction,
    sections: HashMap<TableAction, GridSnapshot>,
    filter_input: Entity<TextInput>,
    filter_column: usize,
    filter_operator: FilterOperator,
    picker: Option<Picker>,
    edit: Option<CellEdit>,
    edit_subscription: Option<Subscription>,
    review: bool,
    preview: Vec<String>,
    _subscription: Subscription,
}

#[derive(Clone)]
struct CellEdit {
    row: usize,
    column: usize,
    input: Entity<TextInput>,
    mode: CellMode,
    expanded: bool,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum CellMode {
    Value,
    Null,
    Default,
}

#[derive(Clone)]
struct GridSnapshot {
    output: Option<Arc<QueryOutput>>,
    result_index: usize,
    visible_rows: Vec<usize>,
    sort: Option<(usize, bool)>,
    selected_cell: Option<(usize, usize)>,
    column_widths: Vec<Vec<f32>>,
    result_scroll: gpui::UniformListScrollHandle,
    horizontal_scroll: gpui::ScrollHandle,
    filter: String,
}

impl GridSnapshot {
    fn take(document: &mut Document, cx: &mut Context<DatabaseView>) -> Self {
        let filter = document.result_filter.read(cx).value().to_owned();
        document
            .result_filter
            .update(cx, |input, cx| input.clear(cx));
        document.value_view = None;
        Self {
            output: document.output.take(),
            result_index: std::mem::take(&mut document.result_index),
            visible_rows: std::mem::take(&mut document.visible_rows),
            sort: document.sort.take(),
            selected_cell: document.selected_cell.take(),
            column_widths: std::mem::take(&mut document.column_widths),
            result_scroll: std::mem::take(&mut document.result_scroll),
            horizontal_scroll: std::mem::replace(
                &mut document.horizontal_scroll,
                gpui::ScrollHandle::new(),
            ),
            filter,
        }
    }

    fn restore(self, document: &mut Document, cx: &mut Context<DatabaseView>) {
        document.output = self.output;
        document.result_index = self.result_index;
        document.visible_rows = self.visible_rows;
        document.sort = self.sort;
        document.selected_cell = self.selected_cell;
        document.column_widths = self.column_widths;
        document.result_scroll = self.result_scroll;
        document.horizontal_scroll = self.horizontal_scroll;
        document
            .result_filter
            .update(cx, |input, cx| input.set_value(self.filter, cx));
    }
}

impl TableWorkspace {
    pub(super) fn mark_submission_uncertain(&mut self) {
        self.submission_uncertain = true;
    }
    pub(super) fn editing(&self) -> bool {
        self.edit.is_some()
    }

    pub(super) fn editing_cell(&self) -> Option<(usize, usize)> {
        self.edit.as_ref().map(|edit| (edit.row, edit.column))
    }

    pub(super) fn composing(&self, cx: &App) -> bool {
        self.filter_input.read(cx).is_composing()
            || self
                .edit
                .as_ref()
                .is_some_and(|edit| edit.input.read(cx).is_composing())
    }

    pub(super) fn invalidate(&mut self) {
        self.page = None;
        self.sections.clear();
        self.edit = None;
        self.edit_subscription = None;
        self.review = false;
    }

    fn new(info: TableInfo, id: u64, cx: &mut Context<DatabaseView>) -> Self {
        let filter_input = cx.new(|cx| TextInput::new(i18n_key::DATABASE_FILTER_VALUE, false, cx));
        let subscription = cx.subscribe(&filter_input, move |this, _, event, cx| {
            if matches!(event, InputEvent::Submit) && this.documents[this.active_document].id == id
            {
                this.apply_table_filter(cx);
            }
        });
        Self {
            info,
            schema: None,
            page: None,
            request: PageRequest::default(),
            changes: BTreeMap::new(),
            submission_uncertain: false,
            section: TableAction::Data,
            sections: HashMap::new(),
            filter_input,
            filter_column: 0,
            filter_operator: FilterOperator::Equals,
            picker: None,
            edit: None,
            edit_subscription: None,
            review: false,
            preview: Vec::new(),
            _subscription: subscription,
        }
    }
}

impl DatabaseView {
    pub(super) fn refresh_active_table(&mut self, cx: &mut Context<Self>) {
        if !self.available() || !self.finish_cell_edit_first(cx) {
            return;
        }
        let document = &mut self.documents[self.active_document];
        let Some(table) = &mut document.table else {
            return;
        };
        if !table.changes.is_empty() {
            return;
        }
        table.invalidate();
        table.schema = None;
        table.request.filter = None;
        table.request.order = None;
        table.request.offset = 0;
        table.filter_column = 0;
        table.filter_input.update(cx, |input, cx| input.clear(cx));
        let section = table.section;
        let request = table.request.clone();
        if section == TableAction::Data {
            self.load_table_page(request, cx);
        } else {
            self.load_table_structure(section, cx);
        }
    }

    pub(super) fn finish_cell_edit_first(&mut self, cx: &mut Context<Self>) -> bool {
        if self.table_workspace().is_some_and(TableWorkspace::editing) {
            self.documents[self.active_document].error =
                Some(i18n_key::DATABASE_STAGE_OR_CANCEL_THE_CURRENT_CELL_EDIT_FIRST.into());
            cx.notify();
            false
        } else {
            true
        }
    }

    pub(super) fn dismiss_table_overlay(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        if self.busy.is_some() {
            return false;
        }
        let Some(table) = self.documents[self.active_document].table.as_mut() else {
            return false;
        };
        let dismissed = if table.picker.take().is_some() || table.edit.take().is_some() {
            true
        } else if table.review {
            table.review = false;
            true
        } else {
            false
        };
        if dismissed {
            if table.edit.is_none() {
                table.edit_subscription = None;
            }
            self.documents[self.active_document].error = None;
            self.documents[self.active_document].filter_rows(cx);
            self.focus.focus(window);
            cx.notify();
        }
        dismissed
    }

    pub(super) fn table_workspace(&self) -> Option<&TableWorkspace> {
        self.documents[self.active_document].table.as_ref()
    }

    pub(super) fn table_action(&mut self, action: TableAction, cx: &mut Context<Self>) {
        if !self.interactive() {
            return;
        }
        let Some(info) = self.selected_table.clone() else {
            return;
        };
        let index = if let Some(index) = self
            .documents
            .iter()
            .position(|doc| doc.table.as_ref().is_some_and(|table| table.info == info))
        {
            index
        } else {
            if !self.available() {
                return;
            }
            let id = self.next_document_id;
            self.next_document_id += 1;
            let title = format!("{}.{}", info.schema, info.name);
            let mut document = Document::new(id, title, self.engine(), cx);
            document.editor_open = false;
            document
                .row_limit
                .update(cx, |input, cx| input.set_value("200", cx));
            document.table = Some(TableWorkspace::new(info, id, cx));
            self.documents.push(document);
            self.documents.len() - 1
        };
        self.active_document = index;
        if self.viewport_size.0 < BROWSER_MIN_WIDTH {
            self.browser_open = false;
        }
        self.select_table_section(action, cx);
    }

    pub(super) fn select_table_section(&mut self, section: TableAction, cx: &mut Context<Self>) {
        if !self.available() {
            cx.notify();
            return;
        }
        if !self.finish_cell_edit_first(cx) {
            return;
        }
        let document = &mut self.documents[self.active_document];
        let Some(table) = document.table.as_ref() else {
            return;
        };
        if table.section != section {
            let previous = table.section;
            let snapshot = GridSnapshot::take(document, cx);
            let table = document.table.as_mut().unwrap();
            table.sections.insert(previous, snapshot);
            table.section = section;
            table.edit = None;
            table.edit_subscription = None;
            table.review = false;
            table.picker = None;
            if let Some(snapshot) = table.sections.remove(&section) {
                snapshot.restore(document, cx);
            }
            document.result_revision += 1;
        }
        if document.output.is_none() {
            if section == TableAction::Data {
                let request = document.table.as_ref().unwrap().request.clone();
                self.load_table_page(request, cx);
            } else {
                self.load_table_structure(section, cx);
            }
        }
        cx.notify();
    }

    fn load_table_structure(&mut self, section: TableAction, cx: &mut Context<Self>) {
        let document = &self.documents[self.active_document];
        let Some(table) = &document.table else { return };
        let Some(client) = self.client.clone() else {
            return;
        };
        let info = table.info.clone();
        let id = document.id;
        let generation = self.generation;
        self.busy = Some(i18n_key::DATABASE_LOADING_DATABASE_OBJECTS);
        self.running_document = Some(id);
        let task = cx.background_executor().spawn(async move {
            match section {
                TableAction::Columns => client.columns(info).await,
                TableAction::Indexes => client.indexes(info).await,
                TableAction::Data => unreachable!(),
            }
        });
        self.task = Some(cx.spawn(async move |this, cx| {
            let result = task.await;
            let _ = this.update(cx, |this, cx| {
                if this.closed || this.generation != generation {
                    return;
                }
                this.busy = None;
                this.running_document = None;
                if let Some(index) = this.document_index(id) {
                    match result {
                        Ok(output) => this.documents[index].install_output(output, cx),
                        Err(error) => {
                            this.documents[index].error = Some(Message::from_error(error))
                        }
                    }
                }
                cx.notify();
            });
        }));
    }

    pub(super) fn load_table_page(&mut self, mut request: PageRequest, cx: &mut Context<Self>) {
        if !self.available() {
            return;
        }
        if !self.finish_cell_edit_first(cx) {
            return;
        }
        match self.limit(cx) {
            Ok(limit) if limit != request.limit => {
                request.limit = limit;
                request.offset = 0;
            }
            Ok(_) => {}
            Err(error) => {
                self.documents[self.active_document].error = Some(error.into());
                cx.notify();
                return;
            }
        }
        let document = &self.documents[self.active_document];
        let Some(table) = &document.table else { return };
        if !table.changes.is_empty() {
            self.documents[self.active_document].error = Some(
                i18n_key::DATABASE_SUBMIT_OR_DISCARD_PENDING_CHANGES_BEFORE_RELOADING_DATA.into(),
            );
            cx.notify();
            return;
        }
        let Some(client) = self.client.clone() else {
            return;
        };
        let id = document.id;
        let info = table.info.clone();
        let schema = table.schema.clone();
        let generation = self.generation;
        self.busy = Some(i18n_key::DATABASE_LOADING_TABLE_DATA);
        self.running_document = Some(id);
        self.documents[self.active_document].error = None;
        let task = cx.background_executor().spawn(async move {
            let schema = match schema {
                Some(schema) => schema,
                None => client.table_schema(info).await?,
            };
            let start = std::time::Instant::now();
            let page = client.table_page(&schema, request.clone()).await?;
            Ok::<_, DatabaseError>((schema, page, request, start.elapsed()))
        });
        self.task = Some(cx.spawn(async move |this, cx| {
            let result = task.await;
            let _ = this.update(cx, |this, cx| {
                if this.closed || this.generation != generation {
                    return;
                }
                this.busy = None;
                this.running_document = None;
                if let Some(index) = this.document_index(id) {
                    match result {
                        Ok((schema, page, request, elapsed)) => {
                            let output = QueryOutput {
                                results: vec![page.result.clone()],
                                elapsed,
                            };
                            let document = &mut this.documents[index];
                            let table = document.table.as_mut().unwrap();
                            table.schema = Some(schema);
                            table.page = Some(page);
                            table.request = request;
                            table.edit = None;
                            table.edit_subscription = None;
                            table.review = false;
                            document.install_output(output, cx);
                            this.update_completions(cx);
                        }
                        Err(error) => {
                            this.documents[index].error = Some(Message::from_error(error))
                        }
                    }
                }
                cx.emit(PaneEvent::Activity);
                cx.notify();
            });
        }));
        cx.notify();
    }

    fn apply_table_filter(&mut self, cx: &mut Context<Self>) {
        if self.input_composing(cx) {
            return;
        }
        let Some(table) = self.table_workspace() else {
            return;
        };
        let Some(schema) = &table.schema else { return };
        let Some(column) = schema.columns.get(table.filter_column) else {
            return;
        };
        let text = table.filter_input.read(cx).value();
        let value = if !table.filter_operator.needs_value() {
            Ok(CellValue::Null)
        } else if table.filter_operator == FilterOperator::Contains {
            Ok(CellValue::Text(text.into()))
        } else {
            CellValue::parse(column, text)
        };
        let value = match value {
            Ok(value) => value,
            Err(error) => {
                self.documents[self.active_document].error = Some(Message::from_error(error));
                cx.notify();
                return;
            }
        };
        let mut request = table.request.clone();
        request.filter = Some(ColumnFilter {
            column: table.filter_column,
            operator: table.filter_operator,
            value,
        });
        request.offset = 0;
        self.load_table_page(request, cx);
    }

    fn clear_table_filter(&mut self, cx: &mut Context<Self>) {
        let Some(table) = self.table_workspace() else {
            return;
        };
        let mut request = table.request.clone();
        request.filter = None;
        request.offset = 0;
        self.load_table_page(request, cx);
    }

    pub(super) fn sort_table(&mut self, column: usize, cx: &mut Context<Self>) {
        let Some(table) = self.table_workspace() else {
            return;
        };
        let mut request = table.request.clone();
        request.order = match request.order {
            Some(order) if order.column == column && !order.descending => Some(SortOrder {
                column,
                descending: true,
            }),
            Some(order) if order.column == column => None,
            _ => Some(SortOrder {
                column,
                descending: false,
            }),
        };
        request.offset = 0;
        self.load_table_page(request, cx);
    }

    fn table_page_step(&mut self, next: bool, cx: &mut Context<Self>) {
        let Some(table) = self.table_workspace() else {
            return;
        };
        let Some(page) = &table.page else { return };
        if next && !page.has_next {
            return;
        }
        let mut request = table.request.clone();
        request.offset = if next {
            page.offset.saturating_add(request.limit as u64)
        } else {
            page.offset.saturating_sub(request.limit as u64)
        };
        self.load_table_page(request, cx);
    }
}
