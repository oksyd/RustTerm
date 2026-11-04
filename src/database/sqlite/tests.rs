use super::*;
use crate::{
    config::{DatabaseConfig, Secret},
    database::{DatabaseClient, TableInfo, create_sqlite},
};
use std::time::Instant;

fn config(directory: &tempfile::TempDir) -> SqliteConfig {
    SqliteConfig {
        path: directory.path().join("local.sqlite"),
        read_only: false,
        query_timeout_secs: 10,
    }
}

async fn connect(config: SqliteConfig) -> DatabaseClient {
    DatabaseClient::connect(
        DatabaseConfig::Sqlite(config),
        Secret::new(String::new()),
        None,
    )
    .await
    .unwrap()
}

#[tokio::test]
async fn explicit_create_preserves_existing_paths_and_open_never_creates() {
    let directory = tempfile::tempdir().unwrap();
    let config = config(&directory);
    assert!(matches!(
        DatabaseClient::connect(
            DatabaseConfig::Sqlite(config.clone()),
            Secret::new(String::new()),
            None
        )
        .await,
        Err(DatabaseError::FileOpen)
    ));
    assert!(!config.path.exists());
    create_sqlite(config.clone()).await.unwrap();
    let original = std::fs::read(&config.path).unwrap();
    assert_eq!(&original[..16], b"SQLite format 3\0");
    assert_eq!(
        create_sqlite(config.clone()).await,
        Err(DatabaseError::FileExists)
    );
    assert_eq!(std::fs::read(&config.path).unwrap(), original);

    let bad = SqliteConfig {
        path: directory.path().join("not-a-database"),
        ..config
    };
    std::fs::write(&bad.path, b"This existing document must not be changed").unwrap();
    assert!(matches!(
        DatabaseClient::connect(
            DatabaseConfig::Sqlite(bad.clone()),
            Secret::new(String::new()),
            None
        )
        .await,
        Err(DatabaseError::InvalidFile)
    ));
    assert_eq!(
        std::fs::read(&bad.path).unwrap(),
        b"This existing document must not be changed"
    );
}

#[tokio::test]
async fn sqlite_dynamic_values_empty_columns_and_read_only_metadata() {
    let directory = tempfile::tempdir().unwrap();
    let mut config = config(&directory);
    create_sqlite(config.clone()).await.unwrap();
    let writer = connect(config.clone()).await;
    writer.execute("PRAGMA journal_mode=WAL; CREATE TABLE sqliteAbc(id INTEGER PRIMARY KEY, value TEXT); CREATE INDEX value_index ON sqliteAbc(value); CREATE TEMP TABLE session(value); INSERT INTO sqliteAbc(value) VALUES ('one');".into(), 100).await.unwrap();
    writer
        .execute(
            "CREATE TABLE child(parent INTEGER REFERENCES sqliteAbc(id))".into(),
            10,
        )
        .await
        .unwrap();
    assert!(
        writer
            .execute("INSERT INTO child VALUES (999)".into(), 10)
            .await
            .is_err()
    );
    let tables = writer.tables().await.unwrap();
    assert!(
        tables
            .iter()
            .any(|table| table.schema == "main" && table.name == "sqliteAbc")
    );
    assert!(
        tables
            .iter()
            .any(|table| table.schema == "temp" && table.name == "session")
    );
    let output = writer.execute("VALUES (1), (2.5), ('hello'), (X'00FF'), (NULL); SELECT id AS empty_column FROM sqliteAbc WHERE 0".into(), 100).await.unwrap();
    assert_eq!(
        output.results[0].rows,
        vec![
            vec![Some("1".into())],
            vec![Some("2.5".into())],
            vec![Some("hello".into())],
            vec![Some("0x00ff".into())],
            vec![None]
        ]
    );
    assert_eq!(output.results[1].columns, ["empty_column"]);
    assert!(output.results[1].rows.is_empty());

    config.read_only = true;
    let reader = connect(config).await;
    let table = TableInfo {
        schema: "main".into(),
        name: "sqliteAbc".into(),
        kind: "TABLE".into(),
    };
    assert!(reader.tables().await.unwrap().contains(&table));
    assert_eq!(
        reader.columns(table.clone()).await.unwrap().results[0]
            .rows
            .len(),
        2
    );
    assert!(
        !reader.indexes(table).await.unwrap().results[0]
            .rows
            .is_empty()
    );
    assert_eq!(
        reader
            .execute("PRAGMA journal_mode".into(), 10)
            .await
            .unwrap()
            .results[0]
            .rows[0][0]
            .as_deref(),
        Some("wal")
    );
    for sql in [
        "INSERT INTO sqliteAbc(value) VALUES ('forbidden')".to_owned(),
        "PRAGMA query_only=OFF; INSERT INTO sqliteAbc(value) VALUES ('forbidden')".to_owned(),
        format!(
            "ATTACH '{}' AS other",
            directory.path().join("other.sqlite").display()
        ),
        format!(
            "VACUUM INTO '{}'",
            directory.path().join("copy.sqlite").display()
        ),
    ] {
        assert!(reader.execute(sql, 10).await.is_err());
        assert!(reader.is_connected());
    }
    assert!(!directory.path().join("other.sqlite").exists());
    assert!(!directory.path().join("copy.sqlite").exists());
    assert_eq!(
        reader
            .execute("SELECT count(*) FROM sqliteAbc".into(), 10)
            .await
            .unwrap()
            .results[0]
            .rows[0][0]
            .as_deref(),
        Some("1")
    );
    writer.close();
    reader.close();
}

#[tokio::test]
async fn sqlite_result_limits_preserve_session_and_execute_requested_batch() {
    let directory = tempfile::tempdir().unwrap();
    let config = config(&directory);
    create_sqlite(config.clone()).await.unwrap();
    let client = connect(config.clone()).await;
    let output = client
        .execute(
            "SELECT 1 UNION ALL SELECT 2; CREATE TABLE after_result(value)".into(),
            1,
        )
        .await
        .unwrap();
    assert!(output.results[0].truncated);
    assert!(client.is_connected());
    assert_eq!(client.tables().await.unwrap()[0].name, "after_result");
    assert_eq!(
        client
            .execute(format!("SELECT zeroblob({})", MAX_RESULT_BYTES + 1), 10)
            .await
            .unwrap_err(),
        DatabaseError::Limit
    );
    assert!(client.is_connected());
}

#[tokio::test]
async fn cancellation_interrupts_sqlite_and_releases_the_write_lock_before_reply() {
    let directory = tempfile::tempdir().unwrap();
    let config = config(&directory);
    create_sqlite(config.clone()).await.unwrap();
    let client = connect(config.clone()).await;
    assert!(
        client
            .execute("PRAGMA busy_timeout=600000".into(), 10)
            .await
            .is_err()
    );
    assert_eq!(
        client
            .execute("PRAGMA busy_timeout".into(), 10)
            .await
            .unwrap()
            .results[0]
            .rows[0][0]
            .as_deref(),
        Some("100")
    );
    client
        .execute("CREATE TABLE numbers(value)".into(), 10)
        .await
        .unwrap();
    let observer = NativeConnection::open(&config.path).unwrap();
    observer.busy_timeout(Duration::ZERO).unwrap();
    let running = client.clone();
    let query = tokio::spawn(async move {
        running.execute("BEGIN IMMEDIATE; WITH RECURSIVE n(x) AS (VALUES(1) UNION ALL SELECT x+1 FROM n WHERE x<1000000000) INSERT INTO numbers SELECT x FROM n".into(), 10).await
    });
    let deadline = Instant::now() + Duration::from_secs(3);
    loop {
        match observer.execute_batch("BEGIN IMMEDIATE; ROLLBACK") {
            Err(rusqlite::Error::SqliteFailure(code, _))
                if code.code == ErrorCode::DatabaseBusy =>
            {
                break;
            }
            Ok(()) => {}
            other => panic!("unexpected lock probe: {other:?}"),
        }
        assert!(Instant::now() < deadline);
        tokio::time::sleep(Duration::from_millis(2)).await;
    }
    client.cancel();
    let result = tokio::time::timeout(Duration::from_secs(2), query)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(result.unwrap_err(), DatabaseError::Cancelled);
    // These are immediate operations, not a retry loop: a premature cancellation
    // reply would leave the SQLite worker and its write transaction alive.
    observer.execute_batch("BEGIN IMMEDIATE; ROLLBACK").unwrap();
    assert_eq!(
        observer
            .query_row("SELECT count(*) FROM numbers", [], |row| row
                .get::<_, i64>(0))
            .unwrap(),
        0
    );
}

#[tokio::test]
async fn sqlite_timeout_interrupts_worker_and_retains_transaction_for_rollback() {
    let directory = tempfile::tempdir().unwrap();
    let mut config = config(&directory);
    config.query_timeout_secs = 1;
    create_sqlite(config.clone()).await.unwrap();
    let client = connect(config.clone()).await;
    let output = tokio::time::timeout(Duration::from_secs(3), client.execute("BEGIN IMMEDIATE; WITH RECURSIVE n(x) AS (VALUES(1) UNION ALL SELECT x+1 FROM n WHERE x<1000000000) SELECT sum(x) FROM n".into(), 10)).await.unwrap();
    assert_eq!(output.unwrap_err(), DatabaseError::Timeout);
    assert!(client.is_connected());
    client.rollback().await.unwrap();
    let observer = NativeConnection::open(config.path).unwrap();
    observer.busy_timeout(Duration::ZERO).unwrap();
    observer.execute_batch("BEGIN IMMEDIATE; ROLLBACK").unwrap();
}

#[tokio::test]
async fn managed_transactions_and_guarded_edits_are_atomic() {
    use crate::database::{TransactionStatus, table::GuardedStatement};
    let directory = tempfile::tempdir().unwrap();
    let config = config(&directory);
    create_sqlite(config.clone()).await.unwrap();
    let client = connect(config).await;
    client.execute("CREATE TABLE items(id INTEGER PRIMARY KEY, value TEXT); INSERT INTO items VALUES (1,'before'), (2,'other')".into(), 10).await.unwrap();
    assert_eq!(client.transaction_status(), TransactionStatus::Idle);
    client.begin().await.unwrap();
    assert_eq!(client.transaction_status(), TransactionStatus::Active);
    let update = |id, before: &str, after: &str| GuardedStatement {
        sql: format!("UPDATE items SET value='{after}' WHERE id={id} AND value='{before}'"),
        expected_rows: 1,
    };
    let result = client
        .apply_batch(vec![
            update(1, "before", "changed"),
            update(2, "stale", "wrong"),
        ])
        .await;
    assert_eq!(result, Err(DatabaseError::Conflict));
    assert_eq!(client.transaction_status(), TransactionStatus::Active);
    assert_eq!(
        client
            .execute("SELECT value FROM items WHERE id=1".into(), 10)
            .await
            .unwrap()
            .results[0]
            .rows[0][0]
            .as_deref(),
        Some("before")
    );
    client
        .apply_batch(vec![update(1, "before", "pending")])
        .await
        .unwrap();
    client.rollback().await.unwrap();
    assert_eq!(client.transaction_status(), TransactionStatus::Idle);
    assert_eq!(
        client
            .execute("SELECT value FROM items WHERE id=1".into(), 10)
            .await
            .unwrap()
            .results[0]
            .rows[0][0]
            .as_deref(),
        Some("before")
    );
    client
        .apply_batch(vec![update(1, "before", "committed")])
        .await
        .unwrap();
    assert_eq!(client.transaction_status(), TransactionStatus::Idle);
    assert_eq!(
        client
            .execute("SELECT value FROM items WHERE id=1".into(), 10)
            .await
            .unwrap()
            .results[0]
            .rows[0][0]
            .as_deref(),
        Some("committed")
    );
}
