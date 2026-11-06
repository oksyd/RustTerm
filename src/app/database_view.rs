mod chrome;
mod connection;
mod documents;
mod keyboard;
mod render;
mod results;
mod sizing;
mod table_workspace;
mod transaction;
mod values;

use super::{
    interaction::WindowState,
    layout::Axis,
    pane::PaneEvent,
    text_input::{InputEvent, TextInput},
    ui,
};
use documents::Document;
use gpui::{
    App, AppContext, ClipboardItem, Context, Entity, EventEmitter, FocusHandle, Focusable,
    KeyDownEvent, Subscription, Task, Window,
};
use rustterm::i18n::messages as i18n_key;
use rustterm::{
    config::{DatabaseConfig, Secret, Settings, SqlEngine},
    database::{
        DEFAULT_ROW_LIMIT, DatabaseClient, DatabaseError, MAX_ROWS, QueryOutput, ResultSet,
        TableInfo, quote_identifier,
    },
    i18n::{Message, tr},
    session::SshOptions,
};
use sizing::ResizeDrag;
use std::sync::Arc;
use values::ValueView;

pub const BROWSER_MIN_WIDTH: f32 = 760.;
const HISTORY_ITEMS: usize = 50;
const HISTORY_BYTES: usize = 256 * 1024;

pub struct DatabaseLaunch {
    pub config: DatabaseConfig,
    pub secret: Secret,
    pub ssh: Option<SshOptions>,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum State {
    Connecting,
    WaitingHostKey,
    Connected,
    Disconnected,
    Failed,
}
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
enum TableAction {
    Data,
    Columns,
    Indexes,
}

pub struct DatabaseView {
    index: usize,
    title: String,
    pane_close_visible: bool,
    config: DatabaseConfig,
    ssh_route: Option<String>,
    window_state: WindowState,
    focus: FocusHandle,
    menu_open: bool,
    menu_index: usize,
    menu_return_focus: Option<FocusHandle>,
    client: Option<DatabaseClient>,
    state: State,
    busy: Option<&'static str>,
    error: Option<Message>,
    notice: Option<rustterm::i18n::Message>,
    success_notice: Option<rustterm::i18n::Message>,
    success_task: Option<Task<()>>,
    closed: bool,
    visible: bool,
    unread: bool,
    native_pending: bool,
    generation: usize,
    task: Option<Task<()>>,
    watch_task: Option<Task<()>>,
    trust_task: Option<Task<()>>,
    _subscriptions: Vec<Subscription>,
    database_input: Entity<TextInput>,
    table_filter: Entity<TextInput>,
    databases: Vec<String>,
    database_open: bool,
    database_scroll: gpui::UniformListScrollHandle,
    tables: Vec<TableInfo>,
    visible_tables: Vec<usize>,
    selected_table: Option<TableInfo>,
    documents: Vec<Document>,
    active_document: usize,
    next_document_id: u64,
    running_document: Option<u64>,
    resize_drag: Option<ResizeDrag>,
    table_scroll: gpui::UniformListScrollHandle,
    viewport_size: (f32, f32),
    browser_open: bool,
    browser_width: f32,
}

impl EventEmitter<PaneEvent> for DatabaseView {}
impl Focusable for DatabaseView {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus.clone()
    }
}

impl DatabaseView {
    pub fn new(
        index: usize,
        title: String,
        launch: DatabaseLaunch,
        window_state: WindowState,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let DatabaseLaunch {
            config,
            secret,
            ssh,
        } = launch;
        let make = |label: &'static str, value: &str, cx: &mut Context<Self>| {
            cx.new(|cx| {
                let mut input = TextInput::new(label, false, cx);
                input.set_value(value, cx);
                input
            })
        };
        let database = match &config {
            DatabaseConfig::Mysql(config) | DatabaseConfig::Postgres(config) => {
                config.database.as_str()
            }
            DatabaseConfig::Sqlite(_) => "main",
        };
        let database_input = make(i18n_key::DATABASE_DATABASE_NAME, database, cx);
        let table_filter = make(i18n_key::DATABASE_FILTER_TABLES_OR_SCHEMAS, "", cx);
        let focus = cx.focus_handle();
        let subscriptions = vec![
            cx.subscribe_in(&database_input, window, |this, _, event, _, cx| {
                if matches!(event, InputEvent::Submit) {
                    this.change_database(cx);
                }
            }),
            cx.subscribe(&table_filter, |this, _, event, cx| {
                if matches!(event, InputEvent::Changed) {
                    this.filter_tables(cx);
                }
            }),
            cx.on_focus_in(&focus, window, |this, _, cx| {
                this.unread = false;
                cx.emit(PaneEvent::Focused(this.index));
                cx.notify();
            }),
        ];
        let ssh_route = ssh.as_ref().map(|ssh| format!("{}:{}", ssh.host, ssh.port));
        let document = Document::new(
            1,
            rustterm::msg!(i18n_key::DATABASE_QUERY_NUMBER, number = 1),
            config.engine(),
            cx,
        );
        let mut this = Self {
            index,
            title,
            pane_close_visible: false,
            config,
            ssh_route,
            window_state,
            focus,
            menu_open: false,
            menu_index: 0,
            menu_return_focus: None,
            client: None,
            state: State::Connecting,
            busy: None,
            error: None,
            notice: None,
            success_notice: None,
            success_task: None,
            closed: false,
            visible: true,
            unread: false,
            native_pending: false,
            generation: 0,
            task: None,
            watch_task: None,
            trust_task: None,
            _subscriptions: subscriptions,
            database_input,
            table_filter,
            databases: Vec::new(),
            database_open: false,
            database_scroll: gpui::UniformListScrollHandle::default(),
            tables: Vec::new(),
            visible_tables: Vec::new(),
            selected_table: None,
            documents: vec![document],
            active_document: 0,
            next_document_id: 2,
            running_document: None,
            resize_drag: None,
            table_scroll: gpui::UniformListScrollHandle::default(),
            viewport_size: (960., 640.),
            browser_open: true,
            browser_width: 230.,
        };
        this.connect(secret, ssh, window, cx);
        this
    }

    pub fn engine(&self) -> SqlEngine {
        self.config.engine()
    }
    pub fn database(&self) -> &str {
        match &self.config {
            DatabaseConfig::Mysql(config) | DatabaseConfig::Postgres(config) => &config.database,
            DatabaseConfig::Sqlite(_) => "main",
        }
    }
    pub fn focus_handle(&self) -> &FocusHandle {
        &self.focus
    }
    pub fn is_running(&self) -> bool {
        matches!(
            self.state,
            State::Connecting | State::WaitingHostKey | State::Connected
        )
    }
    pub fn has_unread(&self) -> bool {
        self.unread
    }
    pub fn has_unsaved_changes(&self) -> bool {
        self.documents.iter().any(Document::dirty)
            || self.client.as_ref().is_some_and(|client| {
                client.transaction_status() != rustterm::database::TransactionStatus::Idle
            })
    }
    pub fn may_auto_reconnect(&self) -> bool {
        false
    }
    pub fn status_text(&self) -> String {
        tr(match (self.engine(), self.state) {
            (SqlEngine::Sqlite, State::Connecting) => i18n_key::DATABASE_OPENING_DATABASE,
            (SqlEngine::Sqlite, State::Connected) => i18n_key::DATABASE_DATABASE_OPEN,
            (SqlEngine::Sqlite, State::Disconnected) => i18n_key::DATABASE_DATABASE_CLOSED,
            (SqlEngine::Sqlite, State::Failed) => i18n_key::DATABASE_COULD_NOT_OPEN_DATABASE,
            (_, State::Connecting) => i18n_key::COMMON_CONNECTING_PROGRESS,
            (_, State::WaitingHostKey) => i18n_key::TERMINAL_WAITING_FOR_HOST_CONFIRMATION,
            (_, State::Connected) => i18n_key::COMMON_CONNECTED,
            (_, State::Disconnected) => i18n_key::COMMON_DISCONNECTED,
            (_, State::Failed) => i18n_key::REDIS_CONNECTION_FAILED,
        })
        .to_owned()
    }
    pub fn status_color(&self) -> u32 {
        match self.state {
            State::Connecting | State::WaitingHostKey => ui::colors().warning,
            State::Connected => ui::colors().success,
            State::Disconnected => ui::colors().muted,
            State::Failed => ui::colors().danger,
        }
    }
    pub fn status_details(&self) -> String {
        let mut detail = match &self.config {
            DatabaseConfig::Mysql(config) | DatabaseConfig::Postgres(config) => format!(
                "{} · {}:{} · {} · {}",
                self.engine().label(),
                config.host,
                config.port,
                config.database,
                if config.tls { "TLS" } else { "TCP" }
            ),
            DatabaseConfig::Sqlite(config) => format!(
                "SQLite · {} · {}",
                config.path.display(),
                tr(if config.read_only {
                    i18n_key::COMMON_READ_ONLY_BADGE
                } else {
                    i18n_key::DATABASE_READ_WRITE
                })
            ),
        };
        if let Some(route) = &self.ssh_route {
            detail.push_str(&format!(
                " · {}",
                rustterm::trf!(i18n_key::REDIS_VIA_SSH_HOST, host = route)
            ));
        }
        detail
    }
    pub fn diagnostic_state(&self) -> &'static str {
        match self.state {
            State::Connecting => "connecting",
            State::WaitingHostKey => "waiting_host_key",
            State::Connected => "connected",
            State::Disconnected => "disconnected",
            State::Failed => "failed",
        }
    }
    pub fn input_composing(&self, cx: &App) -> bool {
        [
            &self.documents[self.active_document].sql_input,
            &self.database_input,
            &self.table_filter,
            &self.documents[self.active_document].result_filter,
            &self.documents[self.active_document].row_limit,
        ]
        .iter()
        .any(|input| input.read(cx).is_composing())
            || self
                .table_workspace()
                .is_some_and(|table| table.composing(cx))
    }
    pub fn set_pane_close_visible(&mut self, visible: bool, cx: &mut Context<Self>) {
        if self.pane_close_visible != visible {
            self.pane_close_visible = visible;
            cx.notify();
        }
    }

    pub fn minimum_size(&self) -> (f32, f32) {
        (360., 350.)
    }
    pub fn set_layout_size(&mut self, size: (f32, f32), cx: &mut Context<Self>) {
        if self.viewport_size != size {
            self.viewport_size = size;
            self.resize_drag = None;
            cx.notify();
        }
    }
    pub fn can_split(&self, axis: Axis) -> bool {
        let minimum = self.minimum_size();
        match axis {
            Axis::Horizontal => {
                self.viewport_size.0 >= minimum.0 * 2. + 6. && self.viewport_size.1 >= minimum.1
            }
            Axis::Vertical => {
                self.viewport_size.1 >= minimum.1 * 2. + 6. && self.viewport_size.0 >= minimum.0
            }
        }
    }
    pub fn set_visible(&mut self, visible: bool, cx: &mut Context<Self>) {
        if self.window_state.is_locked() {
            self.documents[self.active_document].value_view = None;
        }
        if self.visible != visible {
            self.visible = visible;
            if visible {
                self.unread = false;
            } else {
                self.menu_open = false;
                self.menu_return_focus = None;
                self.resize_drag = None;
                self.success_notice = None;
                self.success_task = None;
            }
            cx.notify();
        }
    }
    pub fn apply_settings(&mut self, _: &Settings, cx: &mut Context<Self>) {
        for input in [
            &self.documents[self.active_document].sql_input,
            &self.database_input,
            &self.table_filter,
            &self.documents[self.active_document].result_filter,
            &self.documents[self.active_document].row_limit,
        ] {
            input.update(cx, |_, cx| cx.notify());
        }
        cx.notify();
    }
    pub fn close(&mut self, cx: &mut Context<Self>) {
        self.closed = true;
        self.disconnect(cx);
    }
    pub fn disconnect_or_reconnect(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if !self.interactive() {
            return;
        }
        if self.is_running() {
            let transaction = self.client.as_ref().is_some_and(|client| {
                client.transaction_status() != rustterm::database::TransactionStatus::Idle
            });
            if transaction || self.busy.is_some() {
                let Some(permit) = self.window_state.prompts.acquire() else {
                    return;
                };
                self.native_pending = true;
                let generation = self.generation;
                let response = window.prompt(gpui::PromptLevel::Warning, tr(i18n_key::DATABASE_DISCONNECT_THE_ACTIVE_DATABASE_SESSION),
                    Some(tr(i18n_key::DATABASE_UNCOMMITTED_TRANSACTIONS_WILL_BE_LOST_A_RUNNING_STATEMENT_MAY_ALREADY)),
                    &[tr(i18n_key::COMMON_CANCEL), tr(i18n_key::COMMON_DISCONNECT)], cx);
                cx.spawn_in(window, async move |this, cx| {
                    let confirmed = response.await == Ok(1);
                    let valid = confirmed && permit.is_valid();
                    drop(permit);
                    let _ = this.update(cx, |this, cx| {
                        this.native_pending = false;
                        if valid && !this.closed && this.generation == generation {
                            this.disconnect(cx);
                        }
                        cx.notify();
                    });
                })
                .detach();
            } else {
                self.disconnect(cx);
            }
        } else {
            cx.emit(PaneEvent::Reconnect(self.index));
        }
    }
    pub fn open_search(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if !self.interactive() {
            return;
        }
        self.browser_open = true;
        self.table_filter.read(cx).focus(window);
        cx.notify();
    }
    pub fn paste_sql(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if !self.interactive() || self.input_composing(cx) {
            return;
        }
        if self.table_workspace().is_some() {
            self.paste_cell(window, cx);
            return;
        }
        if let Some(text) = cx.read_from_clipboard().and_then(|item| item.text()) {
            self.documents[self.active_document].value_view = None;
            self.documents[self.active_document]
                .sql_input
                .update(cx, |input, cx| input.insert_text(&text, cx));
            self.documents[self.active_document]
                .sql_input
                .read(cx)
                .focus(window);
            if self.viewport_size.0 < BROWSER_MIN_WIDTH {
                self.browser_open = false;
            }
            self.documents[self.active_document].editor_open = true;
            cx.notify();
        }
    }

    fn show_success(&mut self, message: &'static str, cx: &mut Context<Self>) {
        if self.closed || !self.visible || self.window_state.is_locked() {
            return;
        }
        self.success_notice = Some(message.into());
        self.success_task = Some(cx.spawn(async move |this, cx| {
            gpui::Timer::after(std::time::Duration::from_secs(3)).await;
            let _ = this.update(cx, |this, cx| {
                this.success_notice = None;
                cx.notify();
            });
        }));
        cx.notify();
    }

    fn interactive(&self) -> bool {
        !self.closed
            && !self.window_state.is_locked()
            && !self.window_state.prompts.is_busy()
            && !self.native_pending
    }
    fn unavailable_reason(&self) -> &'static str {
        if self.native_pending || self.window_state.prompts.is_busy() {
            i18n_key::COMMON_FINISH_THE_CURRENT_DIALOG_FIRST
        } else if self.busy.is_some() {
            i18n_key::COMMON_WAIT_FOR_THE_CURRENT_DATABASE_REQUEST_OR_CANCEL_IT
        } else {
            i18n_key::COMMON_OPEN_A_DATABASE_CONNECTION_FIRST
        }
    }

    fn available(&self) -> bool {
        self.interactive()
            && self.busy.is_none()
            && self
                .client
                .as_ref()
                .is_some_and(DatabaseClient::is_connected)
    }
    fn disconnect(&mut self, cx: &mut Context<Self>) {
        if self.busy == Some(i18n_key::DATABASE_SUBMITTING_CHANGES)
            && let Some(index) = self.running_document.and_then(|id| self.document_index(id))
            && let Some(table) = &mut self.documents[index].table
        {
            table.mark_submission_uncertain();
        }
        self.generation += 1;
        self.documents[self.active_document].value_view = None;
        self.resize_drag = None;
        self.success_notice = None;
        self.success_task = None;
        if let Some(client) = self.client.take() {
            client.close();
        }
        self.task = None;
        self.watch_task = None;
        self.trust_task = None;
        self.busy = None;
        self.running_document = None;
        self.state = State::Disconnected;
        cx.emit(PaneEvent::Activity);
        cx.notify();
    }
    fn fail(&mut self, error: DatabaseError, cx: &mut Context<Self>) {
        self.error = Some(Message::from_error(error));
        if self
            .client
            .as_ref()
            .is_some_and(|client| !client.is_connected())
        {
            self.state = State::Disconnected;
        }
        cx.emit(PaneEvent::Activity);
        cx.notify();
    }
    fn limit(&self, cx: &App) -> Result<usize, &'static str> {
        self.documents[self.active_document]
            .row_limit
            .read(cx)
            .value()
            .trim()
            .parse::<usize>()
            .ok()
            .filter(|limit| (1..=MAX_ROWS).contains(limit))
            .ok_or(i18n_key::DATABASE_ENTER_A_RESULT_ROW_LIMIT_FROM_1_TO_10000)
    }
    fn filter_tables(&mut self, cx: &mut Context<Self>) {
        let filter = self.table_filter.read(cx).value().to_lowercase();
        self.visible_tables = self
            .tables
            .iter()
            .enumerate()
            .filter(|(_, table)| {
                format!("{}.{}", table.schema, table.name)
                    .to_lowercase()
                    .contains(&filter)
            })
            .map(|(i, _)| i)
            .collect();
        self.table_scroll
            .scroll_to_item(0, gpui::ScrollStrategy::Top);
        cx.notify();
    }
    fn refresh_browser(&mut self, cx: &mut Context<Self>) {
        if !self.available() {
            return;
        }
        self.load_browser(cx);
    }
    fn load_browser(&mut self, cx: &mut Context<Self>) {
        let Some(client) = self.client.clone() else {
            return;
        };
        let generation = self.generation;
        self.busy = Some(i18n_key::DATABASE_LOADING_DATABASE_OBJECTS);
        self.error = None;
        let server = self.engine() != SqlEngine::Sqlite;
        let task = cx.background_executor().spawn(async move {
            let databases = if server {
                client.databases().await?
            } else {
                Vec::new()
            };
            let tables = client.tables().await?;
            Ok::<_, DatabaseError>((databases, tables))
        });
        self.task = Some(cx.spawn(async move |this, cx| {
            let result = task.await;
            let _ = this.update(cx, |this, cx| {
                if this.closed || this.generation != generation {
                    return;
                }
                this.busy = None;
                match result {
                    Ok((databases, tables)) => {
                        this.databases = databases;
                        this.tables = tables;
                        if this
                            .selected_table
                            .as_ref()
                            .is_some_and(|table| !this.tables.contains(table))
                        {
                            this.selected_table = None;
                        }
                        this.filter_tables(cx);
                        this.update_completions(cx);
                    }
                    Err(error) => this.fail(error, cx),
                }
                cx.notify();
            });
        }));
        cx.notify();
    }
    fn change_database(&mut self, cx: &mut Context<Self>) {
        if !self.available() || self.input_composing(cx) {
            return;
        }
        let mut config = match &self.config {
            DatabaseConfig::Mysql(config) | DatabaseConfig::Postgres(config) => config.clone(),
            DatabaseConfig::Sqlite(_) => return,
        };
        let database = self.database_input.read(cx).value().to_owned();
        if database == config.database {
            self.database_open = false;
            cx.notify();
            return;
        }
        if self.documents.iter().any(|document| {
            document
                .table
                .as_ref()
                .is_some_and(table_workspace::TableWorkspace::has_local_edit)
        }) {
            self.error = Some(
                i18n_key::DATABASE_SUBMIT_OR_DISCARD_TABLE_EDITS_BEFORE_SWITCHING_DATABASES.into(),
            );
            cx.notify();
            return;
        }
        if self.client.as_ref().is_some_and(|client| {
            client.transaction_status() != rustterm::database::TransactionStatus::Idle
        }) {
            self.error = Some(
                i18n_key::DATABASE_RESOLVE_THE_ACTIVE_TRANSACTION_BEFORE_SWITCHING_DATABASES.into(),
            );
            cx.notify();
            return;
        }
        config.database = database.clone();
        if let Err(error) = config.validate() {
            self.error = Some(Message::from_error(error));
            cx.notify();
            return;
        }
        self.database_open = false;
        cx.emit(PaneEvent::DatabaseChanged {
            index: self.index,
            database,
        });
    }
    fn run_sql(&mut self, cx: &mut Context<Self>) {
        self.run_sql_mode(false, cx);
    }

    fn run_script(&mut self, cx: &mut Context<Self>) {
        self.run_sql_mode(true, cx);
    }

    fn run_sql_mode(&mut self, entire_script: bool, cx: &mut Context<Self>) {
        if !self.available()
            || self.input_composing(cx)
            || self.documents[self.active_document].table.is_some()
        {
            return;
        }
        let input = self.documents[self.active_document].sql_input.read(cx);
        let sql = if entire_script {
            Ok(input.value())
        } else {
            input.execution_sql()
        };
        let sql = match sql {
            Ok(sql) if !sql.trim().is_empty() => sql.to_owned(),
            Ok(_) => {
                self.documents[self.active_document].error =
                    Some(i18n_key::DATABASE_ENTER_SQL_BEFORE_RUNNING_A_QUERY.into());
                cx.notify();
                return;
            }
            Err(error) => {
                self.documents[self.active_document].error = Some(error.into());
                cx.notify();
                return;
            }
        };
        let limit = match self.limit(cx) {
            Ok(limit) => limit,
            Err(error) => {
                self.documents[self.active_document].error = Some(error.into());
                cx.notify();
                return;
            }
        };
        let document = &mut self.documents[self.active_document];
        if document.history.last() != Some(&sql) {
            document.history.push(sql.clone());
            while document.history.len() > HISTORY_ITEMS
                || document.history.iter().map(String::len).sum::<usize>() > HISTORY_BYTES
            {
                document.history.remove(0);
            }
        }
        document.history_index = None;
        document.history_draft.clear();
        self.execute(sql, limit, cx);
    }

    fn execute(&mut self, sql: String, limit: usize, cx: &mut Context<Self>) {
        let Some(client) = self.client.clone() else {
            return;
        };
        let document = &mut self.documents[self.active_document];
        let id = document.id;
        document.error = None;
        document.value_view = None;
        self.busy = Some(i18n_key::DATABASE_RUNNING_SQL);
        self.running_document = Some(id);
        self.error = None;
        self.notice = None;
        self.resize_drag = None;
        self.success_notice = None;
        self.success_task = None;
        let generation = self.generation;
        let task = cx
            .background_executor()
            .spawn(async move { client.execute(sql, limit).await });
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
                if this
                    .client
                    .as_ref()
                    .is_some_and(|client| !client.is_connected())
                {
                    this.state = State::Disconnected;
                }
                this.unread = !this.visible;
                cx.emit(PaneEvent::Activity);
                cx.notify();
            });
        }));
        cx.notify();
    }
    fn test_connection(&mut self, cx: &mut Context<Self>) {
        if !self.available() {
            return;
        }
        let client = self.client.as_ref().unwrap().clone();
        let generation = self.generation;
        self.busy = Some(if self.engine() == SqlEngine::Sqlite {
            i18n_key::DATABASE_CHECKING_DATABASE_ACCESS
        } else {
            i18n_key::DATABASE_TESTING_CONNECTION
        });
        self.error = None;
        self.notice = None;
        let task = cx
            .background_executor()
            .spawn(async move { client.execute("SELECT 1".into(), 1).await });
        self.task = Some(cx.spawn(async move |this, cx| {
            let result = task.await;
            let _ = this.update(cx, |this, cx| {
                if this.closed || this.generation != generation {
                    return;
                }
                this.busy = None;
                match result {
                    Ok(_) => this.show_success(
                        if this.engine() == SqlEngine::Sqlite {
                            i18n_key::DATABASE_DATABASE_ACCESS_CHECK_SUCCEEDED
                        } else {
                            i18n_key::DATABASE_DATABASE_CONNECTION_TEST_SUCCEEDED
                        },
                        cx,
                    ),
                    Err(error) => this.fail(error, cx),
                }
                cx.notify();
            });
        }));
        cx.notify();
    }
    fn cancel_query(&mut self, cx: &mut Context<Self>) {
        if !self.interactive() || self.busy.is_none() {
            return;
        }
        if let Some(client) = &self.client {
            client.cancel();
            self.busy = Some(i18n_key::DATABASE_CANCELLING_QUERY);
        } else {
            self.disconnect(cx);
        }
        cx.notify();
    }
    fn recall(&mut self, previous: bool, cx: &mut Context<Self>) {
        if !self.interactive()
            || self.documents[self.active_document].history.is_empty()
            || self.input_composing(cx)
            || (!previous && self.documents[self.active_document].history_index.is_none())
        {
            return;
        }
        let next = if previous {
            match self.documents[self.active_document].history_index {
                None => {
                    self.documents[self.active_document].history_draft = self.documents
                        [self.active_document]
                        .sql_input
                        .read(cx)
                        .value()
                        .to_owned();
                    Some(self.documents[self.active_document].history.len() - 1)
                }
                Some(index) => Some(index.saturating_sub(1)),
            }
        } else {
            self.documents[self.active_document]
                .history_index
                .and_then(|index| {
                    (index + 1 < self.documents[self.active_document].history.len())
                        .then_some(index + 1)
                })
        };
        self.documents[self.active_document].editor_open = true;
        if self.viewport_size.0 < BROWSER_MIN_WIDTH {
            self.browser_open = false;
        }
        self.documents[self.active_document].history_index = next;
        let sql = next
            .map(|index| self.documents[self.active_document].history[index].clone())
            .unwrap_or_else(|| self.documents[self.active_document].history_draft.clone());
        self.documents[self.active_document]
            .sql_input
            .update(cx, |input, cx| input.set_value(sql, cx));
        self.documents[self.active_document].editor_dirty = true;
        self.documents[self.active_document].editor_revision += 1;
        cx.notify();
    }
    fn key_down(&mut self, event: &KeyDownEvent, window: &mut Window, cx: &mut Context<Self>) {
        if self.menu_open {
            self.menu_key_down(event, window, cx);
            return;
        }
        if !self.interactive() || self.input_composing(cx) {
            return;
        }
        let key = event.keystroke.key.as_str();
        let modifiers = event.keystroke.modifiers;
        if key == "escape" && self.dismiss_table_overlay(window, cx) {
            window.prevent_default();
            cx.stop_propagation();
            return;
        }
        if self.grid_key(event, window, cx) {
            window.prevent_default();
            cx.stop_propagation();
            return;
        }
        if self.documents[self.active_document].value_view.is_some() {
            let handled = if key == "escape" {
                self.close_value(window, cx);
                true
            } else if key == "c" && modifiers.control && !modifiers.alt && !modifiers.platform {
                self.copy_cell(cx);
                true
            } else {
                !modifiers.control
                    && !modifiers.alt
                    && !modifiers.platform
                    && !modifiers.shift
                    && self.scroll_value(key, cx)
            };
            // The viewer owns text/navigation keys, while pane and workspace
            // shortcuts keep their normal behavior. Never submit its hidden editor.
            let hidden_submit = modifiers.control
                && !modifiers.shift
                && !modifiers.alt
                && !modifiers.platform
                && key == "enter";
            if handled || hidden_submit {
                window.prevent_default();
                cx.stop_propagation();
                return;
            }
        }
        if modifiers.control && modifiers.shift && !modifiers.alt && !modifiers.platform {
            match key {
                "t" => cx.emit(PaneEvent::NewTab),
                "w" => cx.emit(PaneEvent::Close(self.index)),
                "j" => cx.emit(PaneEvent::NextPane),
                "d" => cx.emit(PaneEvent::Split(Axis::Horizontal)),
                "e" => cx.emit(PaneEvent::Split(Axis::Vertical)),
                "enter" => cx.emit(PaneEvent::Zoom),
                "f" => self.open_search(window, cx),
                "c" => self.copy_selection(window, cx),
                _ => return,
            }
        } else if modifiers.control
            && !modifiers.shift
            && !modifiers.alt
            && !modifiers.platform
            && key == "enter"
        {
            self.run_sql(cx);
        } else if modifiers.control
            && !modifiers.alt
            && !modifiers.platform
            && matches!(key, "pageup" | "pagedown")
        {
            let count = self.documents.len();
            let index = if key == "pageup" {
                (self.active_document + count - 1) % count
            } else {
                (self.active_document + 1) % count
            };
            self.select_document(index, window, cx);
        } else if modifiers.control && !modifiers.alt && !modifiers.platform && key == "tab" {
            cx.emit(PaneEvent::NextTab);
        } else if key == "f5" {
            if self.table_workspace().is_some() {
                self.refresh_active_table(cx);
            } else {
                self.refresh_browser(cx);
            }
        } else if key == "escape" && self.busy.is_some() {
            self.cancel_query(cx);
        } else {
            return;
        }
        window.prevent_default();
        cx.stop_propagation();
    }
}

impl Drop for DatabaseView {
    fn drop(&mut self) {
        if let Some(client) = &self.client {
            client.close();
        }
    }
}
