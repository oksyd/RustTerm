//! Opt-in test against a disposable ClickHouse server. Creates and drops a unique database.
use rustterm::{
    config::{DatabaseConfig, Profile, Secret, SqlConfig, SqlEngine},
    connection_probe::{self, ProbeRequest},
    database::{
        DatabaseClient, DatabaseError, TableInfo, quote_identifier,
        table::{CellValue, ColumnFilter, FilterOperator, PageRequest},
    },
};
use std::time::Duration;

#[tokio::test]
#[ignore = "requires RUSTTERM_CLICKHOUSE_TEST_PORT on a disposable local ClickHouse server"]
async fn live_clickhouse_queries_metadata_limits_cancellation_and_version() {
    let config = SqlConfig {
        port: std::env::var("RUSTTERM_CLICKHOUSE_TEST_PORT")
            .expect("set test port")
            .parse()
            .unwrap(),
        ..SqlConfig::clickhouse()
    };
    let client = DatabaseClient::connect(
        DatabaseConfig::ClickHouse(config.clone()),
        Secret::new(String::new()),
        None,
    )
    .await
    .unwrap();
    let special = client
        .execute("SELECT nan, inf, -inf, NULL, 'NULL'".into(), 1)
        .await
        .unwrap();
    assert!(special.results[0].rows[0][..3].iter().all(Option::is_some));
    assert_eq!(special.results[0].rows[0][3], None);
    assert_eq!(special.results[0].rows[0][4].as_deref(), Some("NULL"));
    let database = format!("rustterm_test_{}", uuid::Uuid::new_v4().simple());
    client
        .execute(format!("CREATE DATABASE {database}"), 1)
        .await
        .unwrap();
    client
        .execute(format!("USE {database}; SET max_threads=1;"), 1)
        .await
        .unwrap();
    assert_eq!(
        client
            .execute("SELECT currentDatabase()".into(), 1)
            .await
            .unwrap()
            .results[0]
            .rows[0][0]
            .as_deref(),
        Some(database.as_str())
    );
    let table = TableInfo {
        schema: database.clone(),
        name: "events`\\'".into(),
        kind: "MergeTree".into(),
    };
    let name = quote_identifier(SqlEngine::ClickHouse, &table.name);
    client.execute(format!("CREATE TABLE {name} (id UInt64, amount Decimal(38,9), label Nullable(String), tags Array(String), INDEX by_id id TYPE minmax GRANULARITY 1) ENGINE=MergeTree ORDER BY id"),1).await.unwrap();
    client.execute(format!("INSERT INTO {name} VALUES (18446744073709551615,12345678901234567890.123456789,NULL,['a','b']),(2,1.25,'中文',[])"),1).await.unwrap();
    let selected = DatabaseClient::connect(
        DatabaseConfig::ClickHouse(SqlConfig {
            database: database.clone(),
            ..config.clone()
        }),
        Secret::new(String::new()),
        None,
    )
    .await
    .unwrap();
    assert!(selected.tables().await.unwrap().contains(&table));
    let details = selected.table_details(table.clone()).await.unwrap();
    assert_eq!(details.results[0].rows[0][0].as_deref(), Some("MergeTree"));
    assert!(
        !selected.parts(table.clone()).await.unwrap().results[0]
            .rows
            .is_empty()
    );
    selected.close();
    assert!(client.databases().await.unwrap().contains(&database));
    assert!(client.tables().await.unwrap().contains(&table));
    assert_eq!(
        client.columns(table.clone()).await.unwrap().results[0]
            .rows
            .len(),
        4
    );
    assert_eq!(
        client.indexes(table.clone()).await.unwrap().results[0].rows[0][0].as_deref(),
        Some("by_id")
    );
    assert!(
        client.definition(table.clone()).await.unwrap().results[0].rows[0][0]
            .as_deref()
            .unwrap()
            .contains("MergeTree")
    );
    let schema = client.table_schema(table).await.unwrap();
    assert!(!schema.editable());
    let page = client
        .table_page(&schema, PageRequest::default())
        .await
        .unwrap();
    assert_eq!(page.result.rows.len(), 2);
    let row = &page.result.rows[1];
    assert_eq!(row[0].as_deref(), Some("18446744073709551615"));
    assert_eq!(row[1].as_deref(), Some("12345678901234567890.123456789"));
    assert_eq!(row[2], None);
    assert!(row[3].as_deref().unwrap().contains('a'));
    let filtered = client
        .table_page(
            &schema,
            PageRequest {
                filter: Some(ColumnFilter {
                    column: 2,
                    operator: FilterOperator::Contains,
                    value: CellValue::Text("中".into()),
                }),
                ..Default::default()
            },
        )
        .await
        .unwrap();
    assert_eq!(filtered.result.rows.len(), 1);
    let limited = client
        .execute("SELECT number FROM numbers(100)".into(), 3)
        .await
        .unwrap();
    assert_eq!(limited.results[0].rows.len(), 3);
    assert!(limited.results[0].truncated);
    assert!(matches!(
        client.execute("SELECT throwIf(1)".into(), 1).await,
        Err(DatabaseError::Server { .. })
    ));
    assert!(client.is_connected());
    let worker = client.clone();
    let running = tokio::spawn(async move {
        worker
            .execute("SELECT sleepEachRow(0.01) FROM numbers(200)".into(), 1000)
            .await
    });
    tokio::time::sleep(Duration::from_millis(200)).await;
    client.cancel();
    let cancelled = running.await.unwrap();
    assert!(
        matches!(cancelled, Err(DatabaseError::Cancelled)),
        "{cancelled:?}"
    );
    assert!(client.is_connected());
    assert_eq!(
        client.execute("SELECT 1".into(), 1).await.unwrap().results[0].rows[0][0].as_deref(),
        Some("1")
    );
    client
        .execute(format!("DROP DATABASE {database} SYNC"), 1)
        .await
        .unwrap();
    client.close();
    let (hosts, _) = tokio::sync::mpsc::channel(1);
    let (certificates, _) = tokio::sync::mpsc::channel(1);
    let outcome = connection_probe::test(
        ProbeRequest {
            profile: Profile::clickhouse("Analytics", config),
            secret: Secret::new(String::new()),
            sentinel_secret: None,
            ssh: None,
        },
        hosts,
        certificates,
        tokio_util::sync::CancellationToken::new(),
    )
    .await
    .unwrap();
    let info = outcome.server.unwrap();
    assert_eq!(info.product, "ClickHouse");
    assert!(info.version.is_some());
    eprintln!("Validated ClickHouse {}", info.version.unwrap());
}

#[tokio::test]
#[ignore = "requires a disposable local ClickHouse and RUSTTERM_CLICKHOUSE_TEST_READONLY_USER"]
async fn live_readonly_account_preserves_null_precision_and_special_values() {
    let config = SqlConfig {
        port: std::env::var("RUSTTERM_CLICKHOUSE_TEST_PORT")
            .unwrap()
            .parse()
            .unwrap(),
        username: std::env::var("RUSTTERM_CLICKHOUSE_TEST_READONLY_USER").unwrap(),
        ..SqlConfig::clickhouse()
    };
    let client = DatabaseClient::connect(
        DatabaseConfig::ClickHouse(config),
        Secret::new(String::new()),
        None,
    )
    .await
    .unwrap();
    let output=client.execute("SELECT NULL, '\\\\N', 'a\\tb', nan, inf, toDecimal128('12345678901234567890.123456789',9), toUInt64(18446744073709551615)".into(),1).await.unwrap();
    assert_eq!(
        output.results[0].rows[0],
        vec![
            None,
            Some("\\N".into()),
            Some("a\tb".into()),
            Some("nan".into()),
            Some("inf".into()),
            Some("12345678901234567890.123456789".into()),
            Some("18446744073709551615".into())
        ]
    );
    assert!(client.databases().await.is_ok());
    assert!(
        client
            .execute("SELECT number FROM numbers(100)".into(), 3)
            .await
            .unwrap()
            .results[0]
            .truncated
    );
    let worker = client.clone();
    let running = tokio::spawn(async move {
        worker
            .execute("SELECT sleepEachRow(0.01) FROM numbers(200)".into(), 1000)
            .await
    });
    tokio::time::sleep(Duration::from_millis(200)).await;
    client.cancel();
    let result = running.await.unwrap();
    assert!(
        matches!(result, Err(DatabaseError::Cancelled)),
        "{result:?}"
    );
    assert!(client.execute("SELECT 1".into(), 1).await.is_ok());
    client.close();
}
