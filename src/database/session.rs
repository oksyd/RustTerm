//! Serialized database requests, cancellation and transaction ownership.

use super::*;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
#[repr(u8)]
pub enum TransactionStatus {
    #[default]
    Idle,
    Active,
    Failed,
    Unknown,
}

#[derive(Clone, Copy)]
pub(super) enum TransactionAction {
    Begin,
    Commit,
    Rollback,
}

pub(super) enum Command {
    Sql { sql: String, row_limit: usize },
    Apply(Vec<table::GuardedStatement>),
    Transaction(TransactionAction),
}

pub(super) enum Connection {
    Mysql(mysql::Connection),
    Postgres(postgres::Connection),
    Sqlite(sqlite::Connection),
}
impl Connection {
    pub(super) async fn open(
        config: DatabaseConfig,
        secret: Secret,
        tunnel: Option<crate::session::TunnelConnector>,
        stop: CancellationToken,
    ) -> Result<Self> {
        match config {
            DatabaseConfig::Mysql(config) => mysql::Connection::open(config, secret, tunnel, stop)
                .await
                .map(Self::Mysql),
            DatabaseConfig::Postgres(config) => {
                postgres::Connection::open(config, secret, tunnel, stop)
                    .await
                    .map(Self::Postgres)
            }
            DatabaseConfig::Sqlite(config) => sqlite::Connection::open(config, stop)
                .await
                .map(Self::Sqlite),
        }
        .map_err(|error| match error {
            DatabaseError::Disconnected => DatabaseError::Connection,
            error => error,
        })
    }
    async fn execute(
        &mut self,
        sql: &str,
        limit: usize,
        cancel: &CancellationToken,
        marker: &str,
    ) -> Result<QueryOutput> {
        let tagged;
        let sql = if matches!(self, Self::Sqlite(_)) {
            sql
        } else {
            // Never rewrite statement boundaries: SQL quoting modes belong to
            // the server and can change within the submitted script.
            tagged = format!("{marker} {sql}");
            &tagged
        };
        let start = Instant::now();
        let results = match self {
            Self::Mysql(connection) => connection.execute(sql, limit).await?,
            Self::Postgres(connection) => connection.execute(sql, limit).await?,
            Self::Sqlite(connection) => connection.execute(sql, limit, cancel.clone()).await?,
        };
        Ok(QueryOutput {
            results,
            elapsed: start.elapsed(),
        })
    }
    fn cancellation(&self, marker: String) -> CancelHandle {
        match self {
            Self::Mysql(connection) => connection.cancellation(marker),
            Self::Postgres(connection) => connection.cancellation(marker),
            Self::Sqlite(connection) => connection.cancellation(),
        }
    }
    fn transaction_status(&self) -> Option<TransactionStatus> {
        match self {
            Self::Sqlite(connection) => connection.transaction_status(),
            _ => None,
        }
    }
    async fn synchronize(&mut self) -> Result<()> {
        match self {
            Self::Mysql(connection) => connection.synchronize().await,
            Self::Postgres(connection) => connection.synchronize().await,
            Self::Sqlite(_) => Ok(()),
        }
    }
    async fn run(
        &mut self,
        command: &Command,
        transaction: TransactionStatus,
        cancel: &CancellationToken,
        marker: &str,
    ) -> Result<QueryOutput> {
        if cancel.is_cancelled() {
            return Err(DatabaseError::Cancelled);
        }
        match command {
            Command::Sql { sql, row_limit } => self.execute(sql, *row_limit, cancel, marker).await,
            Command::Transaction(action) => {
                let sql = match action {
                    TransactionAction::Begin if transaction == TransactionStatus::Idle => "BEGIN",
                    TransactionAction::Commit if transaction == TransactionStatus::Active => {
                        "COMMIT"
                    }
                    TransactionAction::Rollback => "ROLLBACK",
                    _ => return Err(DatabaseError::TransactionState),
                };
                self.execute(sql, 1, cancel, marker).await
            }
            Command::Apply(statements) => {
                let nested = match transaction {
                    TransactionStatus::Idle => false,
                    TransactionStatus::Active => true,
                    TransactionStatus::Failed => return Err(DatabaseError::TransactionState),
                    TransactionStatus::Unknown => {
                        return Err(DatabaseError::TransactionStateUnknown);
                    }
                };
                let savepoint = format!("rustterm_grid_{}", uuid::Uuid::new_v4().simple());
                let begin = if nested {
                    format!("SAVEPOINT {savepoint}")
                } else {
                    "BEGIN".into()
                };
                let commit = if nested {
                    format!("RELEASE SAVEPOINT {savepoint}")
                } else {
                    "COMMIT".into()
                };
                self.execute(&begin, 1, cancel, marker).await?;
                let start = Instant::now();
                let result = async {
                    for statement in statements {
                        if cancel.is_cancelled() {
                            return Err(DatabaseError::Cancelled);
                        }
                        let output = self.execute(&statement.sql, 1, cancel, marker).await?;
                        let affected: u64 = output
                            .results
                            .iter()
                            .map(|result| result.affected_rows)
                            .sum();
                        if affected != statement.expected_rows {
                            return Err(DatabaseError::Conflict);
                        }
                    }
                    self.execute(&commit, 1, cancel, marker).await?;
                    Ok(QueryOutput {
                        results: Vec::new(),
                        elapsed: start.elapsed(),
                    })
                }
                .await;
                if result.is_err() {
                    let cleanup = CancellationToken::new();
                    let rollback = if nested {
                        format!("ROLLBACK TO SAVEPOINT {savepoint}")
                    } else {
                        "ROLLBACK".into()
                    };
                    self.execute(&rollback, 1, &cleanup, marker)
                        .await
                        .map_err(|_| DatabaseError::TransactionRecovery)?;
                    if nested {
                        self.execute(&commit, 1, &cleanup, marker)
                            .await
                            .map_err(|_| DatabaseError::TransactionRecovery)?;
                    }
                }
                result
            }
        }
    }
    async fn shutdown(&mut self) {
        if let Self::Sqlite(connection) = self {
            connection.shutdown().await;
        }
    }
}

#[derive(Clone)]
pub(super) enum CancelHandle {
    Mysql(sqlx::mysql::MySqlConnectOptions, String),
    Postgres(sqlx::postgres::PgConnectOptions, String),
    Sqlite(Arc<rusqlite::InterruptHandle>),
}
impl CancelHandle {
    async fn cancel(&self) -> Result<bool> {
        use sqlx::{Connection as _, Decode, MySql, Row};
        match self {
            Self::Mysql(options, marker) => {
                let mut control = sqlx::MySqlConnection::connect_with(options)
                    .await
                    .map_err(classify)?;
                let query = format!(
                    "SELECT ID FROM information_schema.PROCESSLIST WHERE USER = SUBSTRING_INDEX(USER(), '@', 1) AND COMMAND = 'Query' AND LEFT(INFO, {}) = '{}' LIMIT 2",
                    marker.len(),
                    marker
                );
                let matches = sqlx::raw_sql(sqlx::AssertSqlSafe(query.as_str()))
                    .fetch_all(&mut control)
                    .await
                    .map_err(classify)?;
                let result = if matches.len() == 1 {
                    let id = <&[u8] as Decode<MySql>>::decode(
                        matches[0].try_get_raw(0).map_err(classify)?,
                    )
                    .map_err(|_| DatabaseError::Protocol)?;
                    let id: u64 = std::str::from_utf8(id)
                        .map_err(|_| DatabaseError::Protocol)?
                        .parse()
                        .map_err(|_| DatabaseError::Protocol)?;
                    let sql = format!("KILL QUERY {id}");
                    sqlx::raw_sql(sqlx::AssertSqlSafe(sql.as_str()))
                        .execute(&mut control)
                        .await
                        .map(|_| true)
                        .map_err(classify)
                } else {
                    Ok(false)
                };
                let _ = control.close().await;
                result
            }
            Self::Postgres(options, marker) => {
                let mut control = sqlx::PgConnection::connect_with(options)
                    .await
                    .map_err(classify)?;
                let sql = format!(
                    "SELECT CASE WHEN count(*) = 1 THEN pg_catalog.pg_cancel_backend(min(pid)) ELSE false END FROM pg_catalog.pg_stat_activity WHERE pid <> pg_catalog.pg_backend_pid() AND usename = session_user AND datname = current_database() AND state = 'active' AND left(query, {}) = '{}'",
                    marker.len(),
                    marker
                );
                let row = sqlx::raw_sql(sqlx::AssertSqlSafe(sql.as_str()))
                    .fetch_one(&mut control)
                    .await
                    .map_err(classify)?;
                let result = row
                    .try_get_raw(0)
                    .map_err(classify)?
                    .as_str()
                    .map(|value| value == "t")
                    .map_err(|_| DatabaseError::Protocol);
                let _ = control.close().await;
                result
            }
            Self::Sqlite(interrupt) => {
                interrupt.interrupt();
                Ok(true)
            }
        }
    }
}

pub(super) async fn actor(
    mut connection: Connection,
    mut queue: mpsc::Receiver<Request>,
    stop: CancellationToken,
    timeout: Duration,
    engine: SqlEngine,
    active: Arc<Mutex<Option<CancellationToken>>>,
    published_transaction: Arc<AtomicU8>,
) {
    let mut transaction = TransactionStatus::Idle;
    loop {
        let mut request = tokio::select! {
            biased;
            _ = stop.cancelled() => break,
            request = queue.recv() => match request { Some(request) => request, None => break },
        };
        if request.reply.is_closed() || request.cancel.is_cancelled() {
            active.lock().unwrap().take();
            let _ = request.reply.send(Err(DatabaseError::Cancelled));
            continue;
        }
        let marker = format!("/* rustterm:{} */", uuid::Uuid::new_v4().simple());
        let cancellation = connection.cancellation(marker.clone());
        enum Completion {
            Finished(Result<QueryOutput>),
            Interrupt(DatabaseError),
            Close,
        }
        let (result, reusable, interrupted) = {
            let operation = connection.run(&request.command, transaction, &request.cancel, &marker);
            tokio::pin!(operation);
            let completion = tokio::select! {
                biased;
                _ = stop.cancelled() => Completion::Close,
                _ = request.cancel.cancelled() => Completion::Interrupt(DatabaseError::Cancelled),
                _ = request.reply.closed() => Completion::Interrupt(DatabaseError::Cancelled),
                _ = tokio::time::sleep(timeout) => Completion::Interrupt(DatabaseError::Timeout),
                result = &mut operation => Completion::Finished(result),
            };
            match completion {
                Completion::Finished(result) => (result, true, false),
                Completion::Close => (Err(DatabaseError::OutcomeUnknown), false, false),
                Completion::Interrupt(error) => {
                    request.cancel.cancel();
                    // Complete the cancellation exchange before allowing the next
                    // query to start. Closing is reserved for failed recovery.
                    let result =
                        match tokio::time::timeout(Duration::from_secs(5), cancellation.cancel())
                            .await
                        {
                            Ok(Ok(_)) => {
                                match tokio::time::timeout(Duration::from_secs(5), &mut operation)
                                    .await
                                {
                                    Ok(observed) => interrupted_result(error, observed),
                                    Err(_) => Err(DatabaseError::OutcomeUnknown),
                                }
                            }
                            _ => Err(DatabaseError::OutcomeUnknown),
                        };
                    (result, true, true)
                }
            }
        };
        let reusable = reusable
            && !matches!(
                result,
                Err(DatabaseError::Disconnected
                    | DatabaseError::Protocol
                    | DatabaseError::Runtime
                    | DatabaseError::TransactionRecovery
                    | DatabaseError::OutcomeUnknown)
            )
            && (!(interrupted || result.is_err())
                || matches!(
                    tokio::time::timeout(Duration::from_secs(5), connection.synchronize()).await,
                    Ok(Ok(()))
                ));
        transaction = connection.transaction_status().unwrap_or_else(|| {
            next_transaction_status(transaction, &request.command, &result, engine)
        });
        published_transaction.store(transaction as u8, Ordering::Release);
        if !reusable {
            stop.cancel();
            connection.shutdown().await;
        }
        active.lock().unwrap().take();
        let _ = request.reply.send(result);
        if !reusable {
            break;
        }
    }
    connection.shutdown().await;
}

fn interrupted_result(reason: DatabaseError, observed: Result<QueryOutput>) -> Result<QueryOutput> {
    match observed {
        Err(DatabaseError::Cancelled) => Err(reason),
        Err(DatabaseError::Server { code, .. })
            if matches!(code.as_str(), "57014" | "70100" | "1317") =>
        {
            Err(reason)
        }
        Err(DatabaseError::Disconnected | DatabaseError::Protocol | DatabaseError::Runtime) => {
            Err(DatabaseError::OutcomeUnknown)
        }
        // A confirmed commit must never be presented as an uncommitted edit.
        result => result,
    }
}

fn next_transaction_status(
    previous: TransactionStatus,
    command: &Command,
    result: &Result<QueryOutput>,
    engine: SqlEngine,
) -> TransactionStatus {
    if matches!(
        result,
        Err(DatabaseError::TransactionState
            | DatabaseError::TransactionStateUnknown
            | DatabaseError::Conflict
            | DatabaseError::InvalidTableEdit
            | DatabaseError::TableReadOnly)
    ) {
        return previous;
    }
    match command {
        Command::Apply(_) => {
            if !matches!(result, Err(DatabaseError::TransactionRecovery)) {
                previous
            } else {
                TransactionStatus::Unknown
            }
        }
        Command::Transaction(action) => {
            if result.is_err() {
                return TransactionStatus::Unknown;
            }
            match action {
                TransactionAction::Begin => TransactionStatus::Active,
                TransactionAction::Commit => TransactionStatus::Idle,
                TransactionAction::Rollback
                    if previous == TransactionStatus::Unknown && engine == SqlEngine::Mysql =>
                {
                    TransactionStatus::Unknown
                }
                TransactionAction::Rollback => TransactionStatus::Idle,
            }
        }
        Command::Sql { sql: text, .. } => {
            let Ok(statements) = sql::statements(text, engine) else {
                return TransactionStatus::Unknown;
            };
            let mut current = previous;
            for statement in statements {
                let text = &text[statement.range];
                if engine == SqlEngine::Mysql && (text.contains("/*!") || text.contains("/*M!")) {
                    return TransactionStatus::Unknown;
                }
                let words = sql::leading_words(text, engine);
                let word = words.first().map(String::as_str).unwrap_or("");
                match word {
                    "BEGIN" | "START" => {
                        if current != TransactionStatus::Unknown || engine != SqlEngine::Mysql {
                            current = TransactionStatus::Active;
                        }
                    }
                    "COMMIT" | "END" | "ROLLBACK" => {
                        if words.iter().any(|word| word == "TO") {
                            if engine == SqlEngine::Postgres {
                                current = TransactionStatus::Active;
                            }
                            continue;
                        }
                        current = if words.iter().any(|word| word == "CHAIN")
                            && !words.iter().any(|word| word == "NO")
                        {
                            TransactionStatus::Active
                        } else if current == TransactionStatus::Unknown
                            && engine == SqlEngine::Mysql
                        {
                            TransactionStatus::Unknown
                        } else {
                            TransactionStatus::Idle
                        };
                    }
                    "" | "SELECT" | "VALUES" | "WITH" | "INSERT" | "UPDATE" | "DELETE"
                    | "REPLACE" | "EXPLAIN" | "SHOW" | "DESCRIBE" | "DESC" | "SAVEPOINT"
                    | "RELEASE" => {}
                    "CREATE" | "ALTER" | "DROP" | "TRUNCATE" | "GRANT" | "REVOKE"
                        if engine == SqlEngine::Postgres => {}
                    "CREATE" | "DROP"
                        if engine == SqlEngine::Mysql
                            && words.get(1).is_some_and(|word| word == "TEMPORARY") => {}
                    "CREATE" | "ALTER" | "DROP" | "TRUNCATE" | "RENAME" | "GRANT" | "REVOKE"
                        if engine == SqlEngine::Mysql && current != TransactionStatus::Unknown =>
                    {
                        current = TransactionStatus::Idle;
                    }
                    _ => current = TransactionStatus::Unknown,
                }
            }
            if result.is_err() {
                if current != previous || current == TransactionStatus::Unknown {
                    TransactionStatus::Unknown
                } else if current == TransactionStatus::Active {
                    if engine == SqlEngine::Postgres {
                        TransactionStatus::Failed
                    } else {
                        TransactionStatus::Unknown
                    }
                } else {
                    current
                }
            } else {
                current
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cancellation_preserves_confirmed_commit_and_recovery_failure() {
        let committed = QueryOutput {
            results: Vec::new(),
            elapsed: Duration::ZERO,
        };
        assert!(interrupted_result(DatabaseError::Cancelled, Ok(committed)).is_ok());
        assert!(matches!(
            interrupted_result(
                DatabaseError::Cancelled,
                Err(DatabaseError::TransactionRecovery)
            ),
            Err(DatabaseError::TransactionRecovery)
        ));
        assert!(matches!(
            interrupted_result(DatabaseError::Cancelled, Err(DatabaseError::Disconnected)),
            Err(DatabaseError::OutcomeUnknown)
        ));
        assert!(matches!(
            interrupted_result(
                DatabaseError::Timeout,
                Err(DatabaseError::Server {
                    code: "57014".into(),
                    message: "cancelled".into()
                })
            ),
            Err(DatabaseError::Timeout)
        ));
        assert!(matches!(
            interrupted_result(DatabaseError::Cancelled, Err(DatabaseError::Conflict)),
            Err(DatabaseError::Conflict)
        ));
    }

    fn status(
        previous: TransactionStatus,
        engine: SqlEngine,
        text: &str,
        success: bool,
    ) -> TransactionStatus {
        let command = Command::Sql {
            sql: text.into(),
            row_limit: 1,
        };
        let result = if success {
            Ok(QueryOutput {
                results: Vec::new(),
                elapsed: Duration::ZERO,
            })
        } else {
            Err(DatabaseError::Server {
                code: "fixture".into(),
                message: "fixture".into(),
            })
        };
        next_transaction_status(previous, &command, &result, engine)
    }

    #[test]
    fn raw_transaction_commands_respect_dialect_and_uncertainty() {
        use TransactionStatus::*;
        assert_eq!(
            status(
                Idle,
                SqlEngine::Postgres,
                "/* BEGIN */ SELECT 'COMMIT'",
                true
            ),
            Idle
        );
        assert_eq!(
            status(Idle, SqlEngine::Postgres, "BEGIN; SELECT 1", true),
            Active
        );
        assert_eq!(
            status(Active, SqlEngine::Postgres, "SELECT missing", false),
            Failed
        );
        assert_eq!(
            status(
                Failed,
                SqlEngine::Postgres,
                "ROLLBACK TO SAVEPOINT edits",
                true
            ),
            Active
        );
        assert_eq!(
            status(Active, SqlEngine::Postgres, "COMMIT AND CHAIN", true),
            Active
        );
        assert_eq!(
            status(Active, SqlEngine::Postgres, "COMMIT AND NO CHAIN", true),
            Idle
        );
        assert_eq!(
            status(
                Active,
                SqlEngine::Mysql,
                "CREATE TEMPORARY TABLE t(id int)",
                true
            ),
            Active
        );
        assert_eq!(
            status(Active, SqlEngine::Mysql, "CREATE TABLE t(id int)", true),
            Idle
        );
        assert_eq!(
            status(Idle, SqlEngine::Mysql, "SET autocommit=0", true),
            Unknown
        );
        assert_eq!(
            status(Unknown, SqlEngine::Mysql, "BEGIN; COMMIT", true),
            Unknown
        );
        assert_eq!(
            status(Active, SqlEngine::Mysql, "/*! COMMIT */", true),
            Unknown
        );
        assert_eq!(
            status(Idle, SqlEngine::Postgres, "BEGIN; SELECT missing", false),
            Unknown
        );
    }
}
