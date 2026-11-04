//! Bounded SQL sessions with an explicit lifetime and no automatic query replay.
use crate::i18n::messages as i18n_key;

mod mysql;
mod postgres;
mod session;
pub mod sql;
mod sqlite;
pub mod table;
#[cfg(test)]
mod tests;
#[cfg(unix)]
mod transport;
#[cfg(windows)]
#[path = "transport_windows.rs"]
mod transport;

pub use session::TransactionStatus;
use session::{CancelHandle, Command, Connection, TransactionAction, actor};

use crate::config::{DatabaseConfig, Secret, SqlEngine, SqliteConfig};
use crate::session::{HostKeyChallenge, SshOptions};
use std::{
    sync::{
        Arc, Mutex, OnceLock,
        atomic::{AtomicU8, Ordering},
    },
    time::{Duration, Instant},
};
use tokio::{
    runtime::Runtime,
    sync::{mpsc, oneshot},
};
use tokio_util::sync::CancellationToken;

pub const DEFAULT_ROW_LIMIT: usize = 1_000;
pub const MAX_ROWS: usize = 10_000;
pub const MAX_RESULT_BYTES: usize = 8 * 1024 * 1024;
const MAX_SQL_BYTES: usize = 1024 * 1024;
const MAX_COLUMNS: usize = 1024;
const MAX_RESULT_SETS: usize = 64;

#[derive(Clone, PartialEq, Eq, thiserror::Error)]
pub enum DatabaseError {
    #[error(
        "{}",
        crate::i18n::tr(i18n_key::DATABASE_SSH_TUNNEL_PLATFORM_UNAVAILABLE)
    )]
    SshTunnelPlatformUnavailable,
    #[error(
        "{}",
        crate::i18n::tr(i18n_key::DATABASE_INVALID_DATABASE_CONNECTION_SETTINGS)
    )]
    InvalidConfig,
    #[error(
        "{}",
        crate::i18n::tr(i18n_key::DATABASE_THE_SQLITE_FILE_ALREADY_EXISTS_CHOOSE_ANOTHER_PATH)
    )]
    FileExists,
    #[error(
        "{}",
        crate::i18n::tr(i18n_key::DATABASE_COULD_NOT_OPEN_THE_LOCAL_SQLITE_FILE)
    )]
    FileOpen,
    #[error(
        "{}",
        crate::i18n::tr(i18n_key::DATABASE_THE_SQLITE_FILE_IS_NOT_WRITABLE_OPEN_IT_IN_READ)
    )]
    FileReadOnly,
    #[error(
        "{}",
        crate::i18n::tr(i18n_key::DATABASE_THE_SELECTED_FILE_IS_NOT_A_VALID_SQLITE_DATABASE)
    )]
    InvalidFile,
    #[error(
        "{}",
        crate::i18n::tr(i18n_key::DATABASE_COULD_NOT_CREATE_THE_SQLITE_DATABASE_FILE)
    )]
    FileCreate,
    #[error(
        "{}",
        crate::i18n::tr(i18n_key::DATABASE_COULD_NOT_CONNECT_TO_THE_DATABASE)
    )]
    Connection,
    #[error(
        "{}",
        crate::i18n::tr(i18n_key::DATABASE_DATABASE_TLS_VERIFICATION_OR_HANDSHAKE_FAILED)
    )]
    Tls,
    #[error(
        "{}",
        crate::i18n::tr(i18n_key::DATABASE_DATABASE_AUTHENTICATION_FAILED)
    )]
    Authentication,
    #[error("{}", crate::trf!(i18n_key::DATABASE_DATABASE_REJECTED_THE_REQUEST_CODE_CODE_MESSAGE_EARLIER_STATEMENTS_MAY, code = code, message = message))]
    Server { code: String, message: String },
    #[error(
        "{}",
        crate::i18n::tr(
            i18n_key::DATABASE_THE_DATABASE_RETURNED_AN_INVALID_OR_UNSUPPORTED_RESPONSE
        )
    )]
    Protocol,
    #[error(
        "{}",
        crate::i18n::tr(i18n_key::DATABASE_THE_DATABASE_IS_BUSY_WAIT_FOR_THE_CURRENT_REQUEST)
    )]
    Busy,
    #[error(
        "{}",
        crate::i18n::tr(
            i18n_key::DATABASE_THE_DATABASE_IS_DISCONNECTED_A_SUBMITTED_STATEMENT_MAY_HAVE_COMPLETED
        )
    )]
    Disconnected,
    #[error(
        "{}",
        crate::i18n::tr(
            i18n_key::DATABASE_THE_DATABASE_REQUEST_WAS_CANCELLED_A_SUBMITTED_STATEMENT_MAY_HAVE
        )
    )]
    Cancelled,
    #[error(
        "{}",
        crate::i18n::tr(
            i18n_key::DATABASE_THE_DATABASE_REQUEST_TIMED_OUT_A_SUBMITTED_STATEMENT_MAY_HAVE
        )
    )]
    Timeout,
    #[error(
        "{}",
        crate::i18n::tr(
            i18n_key::DATABASE_THE_QUERY_OUTCOME_COULD_NOT_BE_CONFIRMED_RELOAD_BEFORE_RETRYING
        )
    )]
    OutcomeUnknown,
    #[error(
        "{}",
        crate::i18n::tr(i18n_key::DATABASE_THE_DATABASE_REQUEST_OR_RESULT_EXCEEDS_THE_SIZE_LIMIT)
    )]
    Limit,
    #[error(
        "{}",
        crate::i18n::tr(
            i18n_key::DATABASE_THE_ROW_CHANGED_OR_WAS_REMOVED_BY_ANOTHER_SESSION_REFRESH
        )
    )]
    Conflict,
    #[error(
        "{}",
        crate::i18n::tr(i18n_key::DATABASE_THIS_TABLE_CANNOT_BE_EDITED_SAFELY)
    )]
    TableReadOnly,
    #[error(
        "{}",
        crate::i18n::tr(i18n_key::DATABASE_THE_TABLE_EDIT_CONTAINS_AN_INVALID_VALUE_OR_COLUMN)
    )]
    InvalidTableEdit,
    #[error(
        "{}",
        crate::i18n::tr(
            i18n_key::DATABASE_THE_TRANSACTION_STATE_IS_UNKNOWN_RECONNECT_BEFORE_EDITING_DATA
        )
    )]
    TransactionStateUnknown,
    #[error(
        "{}",
        crate::i18n::tr(
            i18n_key::DATABASE_THE_TRANSACTION_CANNOT_PERFORM_THIS_ACTION_IN_ITS_CURRENT_STATE
        )
    )]
    TransactionState,
    #[error(
        "{}",
        crate::i18n::tr(
            i18n_key::DATABASE_THE_TRANSACTION_COULD_NOT_BE_RESTORED_RECONNECT_BEFORE_CONTINUING
        )
    )]
    TransactionRecovery,
    #[error(
        "{}",
        crate::i18n::tr(i18n_key::DATABASE_THE_DATABASE_BACKGROUND_RUNTIME_COULD_NOT_START)
    )]
    Runtime,
    #[error(
        "{}",
        crate::i18n::tr(i18n_key::DATABASE_COULD_NOT_ESTABLISH_THE_DATABASE_SSH_TUNNEL)
    )]
    SshConnection,
    #[error(
        "{}",
        crate::i18n::tr(i18n_key::DATABASE_DATABASE_SSH_AUTHENTICATION_FAILED)
    )]
    SshAuthentication,
    #[error(
        "{}",
        crate::i18n::tr(i18n_key::DATABASE_THE_DATABASE_SSH_HOST_KEY_WAS_REJECTED_OR_CHANGED)
    )]
    SshHostKey,
    #[error(
        "{}",
        crate::i18n::tr(i18n_key::DATABASE_COULD_NOT_READ_OR_SAVE_DATABASE_SSH_HOST_TRUST)
    )]
    SshKnownHosts,
}
impl std::fmt::Debug for DatabaseError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Server { code, .. } => f
                .debug_struct("Server")
                .field("code", code)
                .finish_non_exhaustive(),
            _ => std::fmt::Display::fmt(self, f),
        }
    }
}
fn server_error(code: String, message: &str) -> DatabaseError {
    DatabaseError::Server {
        code,
        message: message
            .chars()
            .filter(|ch| !ch.is_control() || matches!(ch, '\n' | '\t'))
            .take(2048)
            .collect(),
    }
}
pub type Result<T> = std::result::Result<T, DatabaseError>;

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ResultSet {
    pub columns: Vec<String>,
    pub rows: Vec<Vec<Option<String>>>,
    pub affected_rows: u64,
    pub truncated: bool,
}
#[derive(Clone, Debug)]
pub struct QueryOutput {
    pub results: Vec<ResultSet>,
    pub elapsed: Duration,
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TableInfo {
    pub schema: String,
    pub name: String,
    pub kind: String,
}

struct Request {
    command: Command,
    cancel: CancellationToken,
    reply: oneshot::Sender<Result<QueryOutput>>,
}
struct Inner {
    queue: mpsc::Sender<Request>,
    stop: CancellationToken,
    engine: SqlEngine,
    read_only: bool,
    active: Arc<Mutex<Option<CancellationToken>>>,
    transaction: Arc<AtomicU8>,
}
impl Drop for Inner {
    fn drop(&mut self) {
        self.stop.cancel();
    }
}
#[derive(Clone)]
pub struct DatabaseClient(Arc<Inner>);

impl DatabaseClient {
    pub async fn connect(
        config: DatabaseConfig,
        secret: Secret,
        ssh: Option<(SshOptions, mpsc::Sender<HostKeyChallenge>)>,
    ) -> Result<Self> {
        config
            .validate()
            .map_err(|_| DatabaseError::InvalidConfig)?;
        let engine = config.engine();
        let read_only = matches!(&config, DatabaseConfig::Sqlite(config) if config.read_only);
        let server = config.server();
        if secret.expose().len() > 65_536
            || server.and_then(|config| config.ssh_profile).is_some() != ssh.is_some()
            || (server.is_none() && !secret.expose().is_empty())
        {
            return Err(DatabaseError::InvalidConfig);
        }
        let stop = CancellationToken::new();
        let mut cancellation = CancelOnDrop(Some(stop.clone()));
        let worker_stop = stop.clone();
        let (queue, receive) = mpsc::channel(1);
        let (connected, ready) = oneshot::channel();
        let active = Arc::new(Mutex::new(None));
        let worker_active = active.clone();
        let transaction = Arc::new(AtomicU8::new(TransactionStatus::Idle as u8));
        let worker_transaction = transaction.clone();
        runtime()?.spawn(async move {
            let guard = CancelOnDrop(Some(worker_stop.clone()));
            let query_timeout = Duration::from_secs(config.query_timeout_secs());
            let connection_timeout =
                Duration::from_secs(config.server().map_or(10, |config| config.timeout_secs));
            let opening_stop = worker_stop.clone();
            let open = async move {
                let tunnel = match ssh {
                    Some((options, trust)) => Some(
                        crate::session::connect_tunnel(options, trust, opening_stop.child_token())
                            .await
                            .map_err(|(kind, _)| ssh_error(kind))?,
                    ),
                    None => None,
                };
                tokio::time::timeout(
                    connection_timeout,
                    Connection::open(config, secret, tunnel, opening_stop.clone()),
                )
                .await
                .map_err(|_| DatabaseError::Timeout)?
            };
            let opened = tokio::select! {
                biased;
                _ = worker_stop.cancelled() => Err(DatabaseError::Cancelled),
                result = open => result,
            };
            match opened {
                Ok(connection) => {
                    if connected.send(Ok(())).is_ok() {
                        actor(
                            connection,
                            receive,
                            worker_stop.clone(),
                            query_timeout,
                            engine,
                            worker_active,
                            worker_transaction,
                        )
                        .await;
                    }
                }
                Err(error) => {
                    let _ = connected.send(Err(error));
                }
            }
            drop(guard);
        });
        ready.await.map_err(|_| DatabaseError::Disconnected)??;
        cancellation.0 = None;
        Ok(Self(Arc::new(Inner {
            queue,
            stop,
            engine,
            read_only,
            active,
            transaction,
        })))
    }
    pub fn close(&self) {
        self.0.stop.cancel();
    }
    pub fn cancel(&self) {
        if let Some(cancel) = self.0.active.lock().unwrap().as_ref() {
            cancel.cancel();
        }
    }
    pub fn is_read_only(&self) -> bool {
        self.0.read_only
    }
    pub fn transaction_status(&self) -> TransactionStatus {
        match self.0.transaction.load(Ordering::Acquire) {
            0 => TransactionStatus::Idle,
            1 => TransactionStatus::Active,
            2 => TransactionStatus::Failed,
            _ => TransactionStatus::Unknown,
        }
    }
    pub fn is_connected(&self) -> bool {
        !self.0.stop.is_cancelled() && !self.0.queue.is_closed()
    }
    pub async fn closed(&self) {
        self.0.stop.cancelled().await;
    }
    pub async fn execute(&self, sql: String, row_limit: usize) -> Result<QueryOutput> {
        if sql.trim().is_empty()
            || sql.contains('\0')
            || sql.len() > MAX_SQL_BYTES
            || !(1..=MAX_ROWS).contains(&row_limit)
        {
            return Err(DatabaseError::Limit);
        }
        self.request(Command::Sql { sql, row_limit }).await
    }

    pub async fn begin(&self) -> Result<()> {
        self.request(Command::Transaction(TransactionAction::Begin))
            .await
            .map(|_| ())
    }

    pub async fn commit(&self) -> Result<()> {
        self.request(Command::Transaction(TransactionAction::Commit))
            .await
            .map(|_| ())
    }

    pub async fn rollback(&self) -> Result<()> {
        self.request(Command::Transaction(TransactionAction::Rollback))
            .await
            .map(|_| ())
    }

    pub async fn apply_batch(&self, statements: Vec<table::GuardedStatement>) -> Result<()> {
        if self.is_read_only() {
            return Err(DatabaseError::TableReadOnly);
        }
        if statements.is_empty()
            || statements.len() > MAX_ROWS
            || statements
                .iter()
                .map(|statement| statement.sql.len())
                .sum::<usize>()
                > MAX_SQL_BYTES
            || statements.iter().any(|statement| {
                statement.sql.is_empty()
                    || statement.sql.len() > MAX_SQL_BYTES
                    || statement.sql.contains('\0')
            })
        {
            return Err(DatabaseError::Limit);
        }
        self.request(Command::Apply(statements)).await.map(|_| ())
    }

    async fn request(&self, command: Command) -> Result<QueryOutput> {
        if !self.is_connected() {
            return Err(DatabaseError::Disconnected);
        }
        let (reply, response) = oneshot::channel();
        let cancel = CancellationToken::new();
        {
            let mut active = self.0.active.lock().unwrap();
            if active.is_some() {
                return Err(DatabaseError::Busy);
            }
            *active = Some(cancel.clone());
        }
        if let Err(error) = self.0.queue.try_send(Request {
            command,
            cancel,
            reply,
        }) {
            self.0.active.lock().unwrap().take();
            return Err(match error {
                mpsc::error::TrySendError::Full(_) => DatabaseError::Busy,
                mpsc::error::TrySendError::Closed(_) => DatabaseError::Disconnected,
            });
        }
        response.await.map_err(|_| DatabaseError::Disconnected)?
    }
    pub async fn databases(&self) -> Result<Vec<String>> {
        let sql = match self.0.engine {
            SqlEngine::Mysql => "SHOW DATABASES",
            SqlEngine::Postgres => {
                "SELECT datname FROM pg_catalog.pg_database WHERE datallowconn AND NOT datistemplate ORDER BY datname"
            }
            SqlEngine::Sqlite => "SELECT 'main' UNION ALL SELECT 'temp'",
        };
        let output = self.execute(sql.into(), MAX_ROWS).await?;
        ensure_complete(&output)?;
        Ok(output
            .results
            .into_iter()
            .flat_map(|result| result.rows)
            .filter_map(|row| row.into_iter().next().flatten())
            .collect())
    }
    pub async fn tables(&self) -> Result<Vec<TableInfo>> {
        let sql = match self.0.engine {
            SqlEngine::Mysql => {
                "SELECT TABLE_SCHEMA, TABLE_NAME, TABLE_TYPE FROM information_schema.TABLES WHERE TABLE_SCHEMA = DATABASE() ORDER BY TABLE_SCHEMA, TABLE_NAME"
            }
            SqlEngine::Postgres => {
                "SELECT n.nspname, c.relname, CASE c.relkind WHEN 'v' THEN 'VIEW' WHEN 'm' THEN 'MATERIALIZED VIEW' WHEN 'f' THEN 'FOREIGN TABLE' ELSE 'TABLE' END FROM pg_catalog.pg_class c JOIN pg_catalog.pg_namespace n ON n.oid = c.relnamespace WHERE c.relkind IN ('r','p','v','m','f') AND n.nspname !~ '^pg_' AND n.nspname <> 'information_schema' ORDER BY n.nspname, c.relname"
            }
            SqlEngine::Sqlite => {
                "SELECT 'main', name, upper(type) FROM main.sqlite_schema WHERE type IN ('table','view') AND name NOT GLOB 'sqlite_*' UNION ALL SELECT 'temp', name, upper(type) FROM temp.sqlite_schema WHERE type IN ('table','view') AND name NOT GLOB 'sqlite_*' ORDER BY 1, 2"
            }
        };
        let output = self.execute(sql.into(), MAX_ROWS).await?;
        ensure_complete(&output)?;
        output
            .results
            .into_iter()
            .flat_map(|result| result.rows)
            .map(|row| {
                let mut fields = row.into_iter();
                Ok(TableInfo {
                    schema: fields.next().flatten().ok_or(DatabaseError::Protocol)?,
                    name: fields.next().flatten().ok_or(DatabaseError::Protocol)?,
                    kind: fields.next().flatten().ok_or(DatabaseError::Protocol)?,
                })
            })
            .collect()
    }
    pub async fn columns(&self, table: TableInfo) -> Result<QueryOutput> {
        check_table(&table)?;
        let schema = quote_literal(self.0.engine, &table.schema);
        let name = quote_literal(self.0.engine, &table.name);
        let sql = if self.0.engine == SqlEngine::Sqlite {
            check_sqlite_schema(&table.schema)?;
            format!(
                "SELECT cid, name, type, \"notnull\", dflt_value, pk, hidden FROM pragma_table_xinfo({name}, {schema}) ORDER BY cid"
            )
        } else {
            format!(
                "SELECT column_name, data_type, is_nullable, column_default, character_maximum_length, numeric_precision, numeric_scale FROM information_schema.columns WHERE table_schema = {schema} AND table_name = {name} ORDER BY ordinal_position"
            )
        };
        self.execute(sql, MAX_ROWS).await
    }
    pub async fn indexes(&self, table: TableInfo) -> Result<QueryOutput> {
        check_table(&table)?;
        let schema = quote_literal(self.0.engine, &table.schema);
        let name = quote_literal(self.0.engine, &table.name);
        let sql = match self.0.engine {
            SqlEngine::Mysql => format!(
                "SELECT INDEX_NAME, NON_UNIQUE, SEQ_IN_INDEX, COLUMN_NAME, INDEX_TYPE FROM information_schema.STATISTICS WHERE TABLE_SCHEMA = {schema} AND TABLE_NAME = {name} ORDER BY INDEX_NAME, SEQ_IN_INDEX"
            ),
            SqlEngine::Postgres => format!(
                "SELECT indexname, indexdef FROM pg_catalog.pg_indexes WHERE schemaname = {schema} AND tablename = {name} ORDER BY indexname"
            ),
            SqlEngine::Sqlite => {
                check_sqlite_schema(&table.schema)?;
                format!(
                    "SELECT l.name, l.\"unique\", l.origin, l.partial, x.seqno, x.cid, x.name AS column_name, x.desc, x.coll, x.key FROM pragma_index_list({name}, {schema}) AS l LEFT JOIN pragma_index_xinfo(l.name, {schema}) AS x ORDER BY l.seq, x.seqno"
                )
            }
        };
        self.execute(sql, MAX_ROWS).await
    }
}

/// Create a new local database explicitly; an existing path is never replaced.
/// This can be awaited outside Tokio (including on the GUI background executor).
pub async fn create_sqlite(config: SqliteConfig) -> Result<()> {
    config
        .validate()
        .map_err(|_| DatabaseError::InvalidConfig)?;
    runtime()?
        .spawn_blocking(move || sqlite::create(config))
        .await
        .map_err(|_| DatabaseError::Runtime)?
}

fn check_sqlite_schema(schema: &str) -> Result<()> {
    if matches!(schema, "main" | "temp") {
        Ok(())
    } else {
        Err(DatabaseError::InvalidConfig)
    }
}

fn check_table(table: &TableInfo) -> Result<()> {
    if [&table.schema, &table.name]
        .iter()
        .any(|name| name.is_empty() || name.len() > 1024 || name.contains('\0'))
    {
        return Err(DatabaseError::InvalidConfig);
    }
    Ok(())
}
fn ensure_complete(output: &QueryOutput) -> Result<()> {
    if output.results.iter().any(|result| result.truncated) {
        Err(DatabaseError::Limit)
    } else {
        Ok(())
    }
}
pub fn quote_identifier(engine: SqlEngine, value: &str) -> String {
    let quote = match engine {
        SqlEngine::Mysql => '`',
        SqlEngine::Postgres | SqlEngine::Sqlite => '"',
    };
    format!(
        "{quote}{}{quote}",
        value.replace(quote, &format!("{quote}{quote}"))
    )
}
fn quote_literal(engine: SqlEngine, value: &str) -> String {
    match engine {
        SqlEngine::Mysql => format!("CONVERT(X'{}' USING utf8mb4)", hex(value.as_bytes())),
        SqlEngine::Postgres => format!("E'{}'", value.replace('\\', "\\\\").replace('\'', "''")),
        SqlEngine::Sqlite => format!("'{}'", value.replace('\'', "''")),
    }
}
fn hex(bytes: &[u8]) -> String {
    const DIGITS: &[u8] = b"0123456789abcdef";
    let mut result = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        result.push(DIGITS[(byte >> 4) as usize] as char);
        result.push(DIGITS[(byte & 15) as usize] as char);
    }
    result
}

struct Budget {
    bytes: usize,
    rows: usize,
    limit: usize,
}
impl Budget {
    fn new(limit: usize) -> Self {
        Self {
            bytes: 0,
            rows: 0,
            limit,
        }
    }
    fn columns(&mut self, columns: &[String]) -> Result<()> {
        let size = columns
            .iter()
            .map(|column| column.len() + size_of::<String>())
            .sum::<usize>();
        if columns.len() > MAX_COLUMNS || size > MAX_RESULT_BYTES.saturating_sub(self.bytes) {
            return Err(DatabaseError::Limit);
        }
        self.bytes += size;
        Ok(())
    }
    fn row(&mut self, values: &[Option<String>]) -> bool {
        let size = values
            .iter()
            .map(|value| value.as_ref().map_or(0, String::len) + size_of::<Option<String>>())
            .sum::<usize>()
            + size_of::<Vec<Option<String>>>();
        if self.rows == self.limit || size > MAX_RESULT_BYTES.saturating_sub(self.bytes) {
            return false;
        }
        self.rows += 1;
        self.bytes += size;
        true
    }
}

struct Collector {
    results: Vec<ResultSet>,
    current: ResultSet,
    budget: Budget,
}
impl Collector {
    fn new(limit: usize) -> Self {
        Self {
            results: Vec::new(),
            current: ResultSet::default(),
            budget: Budget::new(limit),
        }
    }
    fn needs_columns(&self) -> bool {
        self.current.columns.is_empty()
    }
    fn columns(&mut self, columns: Vec<String>) -> Result<()> {
        if self.results.len() >= MAX_RESULT_SETS {
            return Err(DatabaseError::Limit);
        }
        self.budget.columns(&columns)?;
        self.current.columns = columns;
        Ok(())
    }
    fn row(&mut self, values: Vec<Option<String>>) -> bool {
        if !self.budget.row(&values) {
            self.current.truncated = true;
            return false;
        }
        self.current.rows.push(values);
        true
    }
    fn retaining_rows(&self) -> bool {
        !self.current.truncated
    }
    fn complete(&mut self, affected_rows: u64) -> Result<()> {
        if self.results.len() >= MAX_RESULT_SETS {
            return Err(DatabaseError::Limit);
        }
        self.current.affected_rows = affected_rows;
        self.results.push(std::mem::take(&mut self.current));
        Ok(())
    }
    fn finish(mut self) -> Vec<ResultSet> {
        if self.current.truncated {
            self.results.push(self.current);
        }
        self.results
    }
}
fn display_bytes(bytes: &[u8]) -> String {
    match std::str::from_utf8(bytes) {
        Ok(text)
            if !text
                .chars()
                .any(|ch| ch.is_control() && !matches!(ch, '\n' | '\r' | '\t')) =>
        {
            text.to_owned()
        }
        _ => format!("0x{}", hex(bytes)),
    }
}
fn classify(error: sqlx::Error) -> DatabaseError {
    match error {
        sqlx::Error::Database(error) => {
            let code = error
                .code()
                .map_or_else(String::new, |code| code.into_owned());
            if code.starts_with("28") {
                DatabaseError::Authentication
            } else {
                server_error(code, error.message())
            }
        }
        sqlx::Error::Tls(_) => DatabaseError::Tls,
        sqlx::Error::Io(_) => DatabaseError::Disconnected,
        sqlx::Error::Configuration(_) => DatabaseError::InvalidConfig,
        _ => DatabaseError::Protocol,
    }
}

struct CancelOnDrop(Option<CancellationToken>);
impl Drop for CancelOnDrop {
    fn drop(&mut self) {
        if let Some(stop) = &self.0 {
            stop.cancel();
        }
    }
}
fn runtime() -> Result<&'static Runtime> {
    static RUNTIME: OnceLock<std::result::Result<Runtime, std::io::Error>> = OnceLock::new();
    RUNTIME
        .get_or_init(|| {
            tokio::runtime::Builder::new_multi_thread()
                .worker_threads(2)
                .thread_name("rustterm-sql")
                .enable_all()
                .build()
        })
        .as_ref()
        .map_err(|_| DatabaseError::Runtime)
}
fn ssh_error(kind: crate::session::FailureKind) -> DatabaseError {
    use crate::session::FailureKind;
    match kind {
        FailureKind::Authentication | FailureKind::PrivateKey => DatabaseError::SshAuthentication,
        FailureKind::HostKeyMismatch | FailureKind::HostKeyRejected => DatabaseError::SshHostKey,
        FailureKind::KnownHosts => DatabaseError::SshKnownHosts,
        FailureKind::Timeout => DatabaseError::Timeout,
        _ => DatabaseError::SshConnection,
    }
}
