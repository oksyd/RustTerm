use super::{
    Collector, DatabaseError, MAX_COLUMNS, MAX_RESULT_BYTES, MAX_SQL_BYTES, Result, ResultSet,
};
use crate::config::SqliteConfig;
use rusqlite::{
    Batch, Connection as NativeConnection, ErrorCode, InterruptHandle, OpenFlags,
    fallible_iterator::FallibleIterator,
    hooks::{AuthAction, AuthContext, Authorization},
    limits::Limit,
    types::ValueRef,
};
use std::{
    fs::{File, OpenOptions},
    sync::Arc,
    time::Duration,
};
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

type Work = (NativeConnection, Result<Vec<ResultSet>>);

pub(super) struct Connection {
    connection: Option<NativeConnection>,
    pending: Option<JoinHandle<Work>>,
    interrupt: Arc<InterruptHandle>,
    stop: CancellationToken,
}

impl Connection {
    pub(super) async fn open(config: SqliteConfig, stop: CancellationToken) -> Result<Self> {
        let worker_stop = stop.clone();
        let connection = super::runtime()?
            .spawn_blocking(move || open(&config, worker_stop))
            .await
            .map_err(|_| DatabaseError::Runtime)??;
        Ok(Self {
            interrupt: Arc::new(connection.get_interrupt_handle()),
            connection: Some(connection),
            pending: None,
            stop,
        })
    }

    pub(super) async fn execute(
        &mut self,
        sql: &str,
        limit: usize,
        cancel: CancellationToken,
    ) -> Result<Vec<ResultSet>> {
        let connection = self.connection.take().ok_or(DatabaseError::Disconnected)?;
        let sql = sql.to_owned();
        let stop = self.stop.clone();
        self.pending = Some(super::runtime()?.spawn_blocking(move || {
            let progress_stop = stop.clone();
            let progress_cancel = cancel.clone();
            let result = connection
                .progress_handler(
                    1000,
                    Some(move || progress_stop.is_cancelled() || progress_cancel.is_cancelled()),
                )
                .map_err(classify)
                .and_then(|_| execute(&connection, &sql, limit, &stop, &cancel));
            (connection, result)
        }));
        // Keep the JoinHandle in self: if the actor cancels this future, shutdown
        // can still interrupt and join the actual SQLite worker before replying.
        let work = self.pending.as_mut().unwrap().await;
        self.pending = None;
        let (connection, result) = work.map_err(|_| DatabaseError::Runtime)?;
        self.connection = Some(connection);
        result
    }

    pub(super) fn cancellation(&self) -> super::CancelHandle {
        super::CancelHandle::Sqlite(self.interrupt.clone())
    }

    pub(super) fn transaction_status(&self) -> Option<super::TransactionStatus> {
        self.connection.as_ref().map(|connection| {
            if connection.is_autocommit() {
                super::TransactionStatus::Idle
            } else {
                super::TransactionStatus::Active
            }
        })
    }

    pub(super) async fn shutdown(&mut self) {
        self.stop.cancel();
        self.interrupt.interrupt();
        if let Some(work) = self.pending.take()
            && let Ok((connection, _)) = work.await
        {
            self.connection = Some(connection);
        }
        // Dropping on a blocking worker also rolls back any uncommitted
        // transaction and releases file locks before the caller is notified.
        if let Some(connection) = self.connection.take() {
            let _ = tokio::task::spawn_blocking(move || drop(connection)).await;
        }
    }
}

impl Drop for Connection {
    fn drop(&mut self) {
        self.stop.cancel();
        self.interrupt.interrupt();
    }
}

fn open(config: &SqliteConfig, stop: CancellationToken) -> Result<NativeConnection> {
    if stop.is_cancelled() {
        return Err(DatabaseError::Cancelled);
    }
    let metadata = std::fs::metadata(&config.path).map_err(|_| DatabaseError::FileOpen)?;
    if !metadata.is_file() {
        return Err(DatabaseError::FileOpen);
    }
    // No CREATE or URI flags: missing files and URI-like strings cannot silently
    // create a database or change the requested open mode.
    let flags = if config.read_only {
        OpenFlags::SQLITE_OPEN_READ_ONLY
    } else {
        OpenFlags::SQLITE_OPEN_READ_WRITE
    } | OpenFlags::SQLITE_OPEN_NO_MUTEX;
    let connection = NativeConnection::open_with_flags(&config.path, flags)
        .map_err(|_| DatabaseError::FileOpen)?;
    if !config.read_only
        && connection
            .is_readonly(rusqlite::MAIN_DB)
            .map_err(classify)?
    {
        return Err(DatabaseError::FileReadOnly);
    }
    // A bounded busy wait also bounds cancellation while another process holds
    // a database lock (SQLite's progress callback is not called while sleeping).
    connection
        .busy_timeout(Duration::from_millis(100))
        .map_err(classify)?;
    connection
        .set_limit(Limit::SQLITE_LIMIT_LENGTH, MAX_RESULT_BYTES as i32)
        .map_err(classify)?;
    connection
        .set_limit(Limit::SQLITE_LIMIT_SQL_LENGTH, MAX_SQL_BYTES as i32)
        .map_err(classify)?;
    connection
        .set_limit(Limit::SQLITE_LIMIT_COLUMN, MAX_COLUMNS as i32)
        .map_err(classify)?;
    connection
        .progress_handler(1000, Some(move || stop.is_cancelled()))
        .map_err(classify)?;
    connection
        .pragma_update(None, "foreign_keys", true)
        .map_err(classify)?;
    if config.read_only {
        connection
            .pragma_update(None, "query_only", true)
            .map_err(classify)?;
    }
    let read_only = config.read_only;
    connection
        .authorizer(Some(move |context: AuthContext<'_>| {
            // sqlite3_interrupt does not wake SQLite's busy-handler sleep. Keep its
            // wait bounded even when editor SQL attempts to install a longer timeout.
            if matches!(context.action, AuthAction::Pragma { pragma_name, pragma_value: Some(_) }
            if pragma_name.eq_ignore_ascii_case("busy_timeout"))
            {
                Authorization::Deny
            } else if read_only {
                // READ_ONLY protects main; this also prevents ATTACH/VACUUM INTO
                // and changing query_only back off.
                read_only_action(context)
            } else {
                Authorization::Allow
            }
        }))
        .map_err(classify)?;
    connection
        .query_row("SELECT count(*) FROM main.sqlite_schema", [], |_| Ok(()))
        .map_err(|error| match error {
            rusqlite::Error::SqliteFailure(code, _) if code.code == ErrorCode::NotADatabase => {
                DatabaseError::InvalidFile
            }
            error => classify(error),
        })?;
    Ok(connection)
}

fn read_only_action(context: AuthContext<'_>) -> Authorization {
    let allowed = match context.action {
        AuthAction::Read { .. }
        | AuthAction::Select
        | AuthAction::Recursive
        | AuthAction::Transaction { .. }
        | AuthAction::Savepoint { .. } => true,
        AuthAction::Function { function_name } => {
            !function_name.eq_ignore_ascii_case("load_extension")
        }
        AuthAction::Pragma {
            pragma_name,
            pragma_value,
        } => {
            pragma_value.is_none()
                || matches!(
                    pragma_name.to_ascii_lowercase().as_str(),
                    "table_info"
                        | "table_xinfo"
                        | "table_list"
                        | "index_info"
                        | "index_xinfo"
                        | "index_list"
                        | "foreign_key_list"
                        | "integrity_check"
                        | "quick_check"
                )
        }
        _ => false,
    };
    if allowed {
        Authorization::Allow
    } else {
        Authorization::Deny
    }
}

fn execute(
    connection: &NativeConnection,
    sql: &str,
    limit: usize,
    stop: &CancellationToken,
    cancel: &CancellationToken,
) -> Result<Vec<ResultSet>> {
    let mut batch = Batch::new(connection, sql);
    let mut output = Collector::new(limit);
    while let Some(mut statement) = batch.next().map_err(classify)? {
        if stop.is_cancelled() || cancel.is_cancelled() {
            return Err(DatabaseError::Cancelled);
        }
        output.columns(
            statement
                .column_names()
                .iter()
                .map(|name| (*name).to_owned())
                .collect(),
        )?;
        let before = connection.total_changes();
        if statement.column_count() == 0 {
            statement.execute([]).map_err(classify)?;
        } else {
            let count = statement.column_count();
            let mut rows = statement.query([]).map_err(classify)?;
            while let Some(row) = rows.next().map_err(classify)? {
                let mut values = Vec::with_capacity(count);
                for index in 0..count {
                    let value = row.get_ref(index).map_err(classify)?;
                    if matches!(value, ValueRef::Blob(bytes) if bytes.len() > MAX_RESULT_BYTES / 2)
                    {
                        output.current.truncated = true;
                        break;
                    }
                    values.push(match value {
                        ValueRef::Null => None,
                        ValueRef::Integer(value) => Some(value.to_string()),
                        ValueRef::Real(value) => Some(value.to_string()),
                        ValueRef::Text(value) => Some(super::display_bytes(value)),
                        ValueRef::Blob(value) => Some(format!("0x{}", super::hex(value))),
                    });
                }
                if output.current.truncated {
                    break;
                }
                if !output.row(values) {
                    break;
                }
            }
        }
        let affected = if connection.total_changes() != before {
            connection.changes()
        } else {
            0
        };
        output.complete(affected)?;
    }
    Ok(output.finish())
}

fn classify(error: rusqlite::Error) -> DatabaseError {
    match error {
        rusqlite::Error::SqliteFailure(code, _) if code.code == ErrorCode::OperationInterrupted => {
            DatabaseError::Cancelled
        }
        rusqlite::Error::SqliteFailure(code, _) if code.code == ErrorCode::TooBig => {
            DatabaseError::Limit
        }
        rusqlite::Error::SqliteFailure(code, message) => super::server_error(
            code.extended_code.to_string(),
            message.as_deref().unwrap_or("SQLite rejected the request"),
        ),
        error => super::server_error("sqlite".into(), &error.to_string()),
    }
}

pub(super) fn create(config: SqliteConfig) -> Result<()> {
    let mut options = OpenOptions::new();
    options.read(true).write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let file = options.open(&config.path).map_err(|error| {
        if error.kind() == std::io::ErrorKind::AlreadyExists {
            DatabaseError::FileExists
        } else {
            DatabaseError::FileCreate
        }
    })?;
    let result = (|| {
        let connection = NativeConnection::open_with_flags(
            &config.path,
            OpenFlags::SQLITE_OPEN_READ_WRITE | OpenFlags::SQLITE_OPEN_NO_MUTEX,
        )
        .map_err(|_| DatabaseError::FileCreate)?;
        // VACUUM on our newly created, empty file materializes SQLite's own
        // valid empty-database header. Never change an existing journal mode.
        connection
            .execute_batch("VACUUM")
            .map_err(|_| DatabaseError::FileCreate)?;
        connection.close().map_err(|_| DatabaseError::FileCreate)?;
        file.sync_all().map_err(|_| DatabaseError::FileCreate)
    })();
    if result.is_err() && same_file(&file, &config.path) {
        let _ = std::fs::remove_file(&config.path);
    }
    result
}

fn same_file(file: &File, path: &std::path::Path) -> bool {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        match (file.metadata(), std::fs::symlink_metadata(path)) {
            (Ok(opened), Ok(current)) => {
                opened.dev() == current.dev() && opened.ino() == current.ino()
            }
            _ => false,
        }
    }
    #[cfg(not(unix))]
    {
        // Preserve an uncertain path rather than remove somebody else's file.
        let _ = (file, path);
        false
    }
}

#[cfg(test)]
mod tests;
