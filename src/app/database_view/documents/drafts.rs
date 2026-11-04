use super::*;
use table_workspace::snapshot::TableSnapshot;

pub struct DatabaseDraft {
    config: DatabaseConfig,
    ssh_route: Option<String>,
    documents: Vec<DocumentDraft>,
    active: u64,
    next_id: u64,
    databases: Vec<String>,
    tables: Vec<TableInfo>,
    selected_table: Option<TableInfo>,
    table_filter: String,
    table_scroll: gpui::UniformListScrollHandle,
    browser_open: bool,
    browser_width: f32,
}

struct DocumentDraft {
    id: u64,
    title: Message,
    table: Option<TableSnapshot>,
    sql_input: Entity<TextInput>,
    row_limit: Entity<TextInput>,
    result_filter: Entity<TextInput>,
    error: Option<Message>,
    output: Option<Arc<QueryOutput>>,
    result_index: usize,
    result_revision: usize,
    visible_rows: Vec<usize>,
    sort: Option<(usize, bool)>,
    selected_cell: Option<(usize, usize)>,
    column_widths: Vec<Vec<f32>>,
    value_view: Option<ValueView>,
    result_scroll: gpui::UniformListScrollHandle,
    horizontal_scroll: gpui::ScrollHandle,
    history: Vec<String>,
    history_index: Option<usize>,
    history_draft: String,
    editor_dirty: bool,
    editor_revision: usize,
    editor_open: bool,
}

impl DocumentDraft {
    fn snapshot(document: &Document) -> Self {
        Self {
            id: document.id,
            title: document.title.clone(),
            table: document.table.as_ref().map(TableWorkspace::snapshot),
            sql_input: document.sql_input.clone(),
            row_limit: document.row_limit.clone(),
            result_filter: document.result_filter.clone(),
            error: document.error.clone(),
            output: document.output.clone(),
            result_index: document.result_index,
            result_revision: document.result_revision,
            visible_rows: document.visible_rows.clone(),
            sort: document.sort,
            selected_cell: document.selected_cell,
            column_widths: document.column_widths.clone(),
            value_view: document.value_view.clone(),
            result_scroll: document.result_scroll.clone(),
            horizontal_scroll: document.horizontal_scroll.clone(),
            history: document.history.clone(),
            history_index: document.history_index,
            history_draft: document.history_draft.clone(),
            editor_dirty: document.editor_dirty,
            editor_revision: document.editor_revision,
            editor_open: document.editor_open,
        }
    }

    fn restore(self, same_target: bool, cx: &mut Context<DatabaseView>) -> Option<Document> {
        if !same_target && self.table.is_some() {
            return None;
        }
        let subscriptions =
            Document::bind_inputs(self.id, &self.sql_input, &self.result_filter, cx);
        let mut document = Document {
            id: self.id,
            title: self.title,
            table: self.table.map(|table| table.restore(self.id, cx)),
            sql_input: self.sql_input,
            row_limit: self.row_limit,
            result_filter: self.result_filter,
            error: self.error,
            output: self.output,
            result_index: self.result_index,
            result_revision: self.result_revision + 1,
            visible_rows: self.visible_rows,
            sort: self.sort,
            selected_cell: self.selected_cell,
            column_widths: self.column_widths,
            value_view: self.value_view,
            result_scroll: self.result_scroll,
            horizontal_scroll: self.horizontal_scroll,
            history: self.history,
            history_index: self.history_index,
            history_draft: self.history_draft,
            editor_dirty: self.editor_dirty,
            editor_revision: self.editor_revision,
            editor_open: self.editor_open,
            _subscriptions: subscriptions,
        };
        if !same_target {
            document.output = None;
            document.error = None;
            document.result_index = 0;
            document.visible_rows.clear();
            document.sort = None;
            document.selected_cell = None;
            document.column_widths.clear();
            document.value_view = None;
            document.result_scroll = gpui::UniformListScrollHandle::default();
            document.horizontal_scroll = gpui::ScrollHandle::new();
            document
                .result_filter
                .update(cx, |input, cx| input.clear(cx));
        }
        Some(document)
    }
}

impl DatabaseView {
    pub fn draft(&self, cx: &App) -> DatabaseDraft {
        DatabaseDraft {
            config: self.config.clone(),
            ssh_route: self.ssh_route.clone(),
            documents: self.documents.iter().map(DocumentDraft::snapshot).collect(),
            active: self.documents[self.active_document].id,
            next_id: self.next_document_id,
            databases: self.databases.clone(),
            tables: self.tables.clone(),
            selected_table: self.selected_table.clone(),
            table_filter: self.table_filter.read(cx).value().to_owned(),
            table_scroll: self.table_scroll.clone(),
            browser_open: self.browser_open,
            browser_width: self.browser_width,
        }
    }

    pub fn restore_draft(&mut self, draft: DatabaseDraft, cx: &mut Context<Self>) {
        let same_target =
            same_database(&draft.config, &self.config) && draft.ssh_route == self.ssh_route;
        let documents: Vec<_> = draft
            .documents
            .into_iter()
            .filter_map(|document| document.restore(same_target, cx))
            .collect();
        if !documents.is_empty() {
            self.documents = documents;
        }
        self.active_document = self.document_index(draft.active).unwrap_or(0);
        self.next_document_id = draft.next_id.max(
            self.documents
                .iter()
                .map(|document| document.id)
                .max()
                .unwrap_or(0)
                + 1,
        );
        self.browser_open = draft.browser_open;
        self.browser_width = draft.browser_width;
        if same_target {
            self.databases = draft.databases;
            self.tables = draft.tables;
            self.selected_table = draft.selected_table;
            self.table_filter
                .update(cx, |input, cx| input.set_value(draft.table_filter, cx));
            self.filter_tables(cx);
            self.table_scroll = draft.table_scroll;
        }
        self.update_completions(cx);
        cx.notify();
    }
}

fn same_database(previous: &DatabaseConfig, next: &DatabaseConfig) -> bool {
    match (previous, next) {
        (DatabaseConfig::Mysql(previous), DatabaseConfig::Mysql(next))
        | (DatabaseConfig::Postgres(previous), DatabaseConfig::Postgres(next)) => {
            previous.same_auth_identity(next)
        }
        (DatabaseConfig::Sqlite(previous), DatabaseConfig::Sqlite(next)) => {
            previous.path == next.path
        }
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rustterm::config::{SqlConfig, SqliteConfig};

    #[test]
    fn reconnect_identity_ignores_timeouts_but_not_database_or_file() {
        let original = SqlConfig::postgres();
        let mut changed = original.clone();
        changed.query_timeout_secs += 1;
        assert!(same_database(
            &DatabaseConfig::Postgres(original.clone()),
            &DatabaseConfig::Postgres(changed.clone())
        ));
        changed.database = "another".into();
        assert!(!same_database(
            &DatabaseConfig::Postgres(original),
            &DatabaseConfig::Postgres(changed)
        ));
        let sqlite = |path: &str| {
            DatabaseConfig::Sqlite(SqliteConfig {
                path: path.into(),
                ..Default::default()
            })
        };
        assert!(same_database(
            &sqlite("/tmp/one.db"),
            &sqlite("/tmp/one.db")
        ));
        assert!(!same_database(
            &sqlite("/tmp/one.db"),
            &sqlite("/tmp/two.db")
        ));
    }
}
