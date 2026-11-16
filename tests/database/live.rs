use rustterm::{
    config::{DatabaseConfig, Secret, SqlConfig},
    database::{DatabaseClient, DatabaseError, TransactionStatus, processlist::Visibility},
};
use std::time::Duration;

async fn exercise(mysql: bool) {
    let prefix = if mysql {
        "RUSTTERM_MYSQL_TEST"
    } else {
        "RUSTTERM_POSTGRES_TEST"
    };
    let config = SqlConfig {
        port: std::env::var(format!("{prefix}_PORT"))
            .expect("set disposable database port")
            .parse()
            .unwrap(),
        username: if mysql { "root" } else { "postgres" }.into(),
        database: if mysql { "mysql" } else { "postgres" }.into(),
        tls: false,
        query_timeout_secs: 60,
        ..SqlConfig::postgres()
    };
    let config = if mysql {
        DatabaseConfig::Mysql(config)
    } else {
        DatabaseConfig::Postgres(config)
    };
    let password = std::env::var(format!("{prefix}_PASSWORD")).unwrap_or_default();
    let client = DatabaseClient::connect(config.clone(), Secret::new(password.clone()), None)
        .await
        .unwrap();
    let output = client.execute("CREATE TEMPORARY TABLE rustterm_driver_fixture (id INTEGER PRIMARY KEY, value TEXT); INSERT INTO rustterm_driver_fixture VALUES (1, '中文'); SELECT * FROM rustterm_driver_fixture WHERE id = 0; SELECT * FROM rustterm_driver_fixture".into(), 10).await.unwrap();
    assert_eq!(output.results.len(), 4);
    assert_eq!(output.results[1].affected_rows, 1);
    assert_eq!(output.results[2].columns, ["id", "value"]);
    assert!(output.results[2].rows.is_empty());
    assert_eq!(
        output.results[3].rows,
        [vec![Some("1".into()), Some("中文".into())]]
    );
    if mysql {
        assert_eq!(
            client
                .execute(
                    "UPDATE rustterm_driver_fixture SET value = value WHERE id = 1".into(),
                    1
                )
                .await
                .unwrap()
                .results[0]
                .affected_rows,
            1
        );
    }
    client.begin().await.unwrap();
    client
        .execute(
            "UPDATE rustterm_driver_fixture SET value = 'changed' WHERE id = 1".into(),
            10,
        )
        .await
        .unwrap();
    assert_eq!(client.transaction_status(), TransactionStatus::Active);
    client.rollback().await.unwrap();
    assert_eq!(
        client
            .execute("SELECT value FROM rustterm_driver_fixture".into(), 10)
            .await
            .unwrap()
            .results[0]
            .rows,
        [vec![Some("中文".into())]]
    );
    assert!(matches!(
        client
            .execute(
                "SELECT missing_column FROM rustterm_driver_fixture".into(),
                10
            )
            .await,
        Err(DatabaseError::Server { .. })
    ));
    assert!(client.is_connected());

    let observer = DatabaseClient::connect(config, Secret::new(password), None)
        .await
        .unwrap();
    let worker = client.clone();
    let query = tokio::spawn(async move {
        worker
            .execute(
                if mysql {
                    "SELECT id, SLEEP(20) FROM rustterm_driver_fixture"
                } else {
                    "SELECT pg_sleep(20)"
                }
                .into(),
                10,
            )
            .await
    });
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            let active = if mysql {
                let monitor = observer.mysql_monitor().unwrap();
                let snapshot = monitor.snapshot().await.unwrap();
                assert_eq!(snapshot.visibility, Visibility::All);
                let active = snapshot.processes.iter().any(|process| process.sql.as_deref().is_some_and(|sql| sql.ends_with("SELECT id, SLEEP(20) FROM rustterm_driver_fixture")));
                monitor.close();
                active
            } else {
                let output = observer.execute("SELECT count(*) FROM pg_catalog.pg_stat_activity WHERE pid <> pg_catalog.pg_backend_pid() AND state = 'active' AND query LIKE '%SELECT pg_sleep(20)'".into(), 1).await.unwrap();
                output.results[0].rows[0][0].as_deref() == Some("1")
            };
            if active { break; }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }).await.unwrap();
    client.cancel();
    assert_eq!(query.await.unwrap().unwrap_err(), DatabaseError::Cancelled);
    assert_eq!(
        client
            .execute("SELECT value FROM rustterm_driver_fixture".into(), 1)
            .await
            .unwrap()
            .results[0]
            .rows,
        [vec![Some("中文".into())]]
    );
    observer.close();
    client.close();
}

#[tokio::test]
#[ignore = "requires RUSTTERM_MYSQL_TEST_PORT on a disposable MySQL server"]
async fn mysql_transactions_monitoring_and_query_cancellation() {
    tokio::time::timeout(Duration::from_secs(30), exercise(true))
        .await
        .unwrap();
}

#[tokio::test]
#[ignore = "requires RUSTTERM_POSTGRES_TEST_PORT on a disposable PostgreSQL server"]
async fn postgres_transactions_and_query_cancellation() {
    tokio::time::timeout(Duration::from_secs(30), exercise(false))
        .await
        .unwrap();
}
