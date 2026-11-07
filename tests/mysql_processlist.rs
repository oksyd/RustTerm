//! Opt-in test against a disposable local MySQL 8 server with account-management privileges.
use rustterm::{
    config::{DatabaseConfig, Secret, SqlConfig},
    database::{DatabaseClient, DatabaseError, TransactionStatus, processlist::Visibility},
};
use std::time::Duration;

#[tokio::test]
#[ignore = "requires RUSTTERM_MYSQL_TEST_PORT on a disposable MySQL 8 server"]
async fn live_mysql_processlist_permissions_roles_and_busy_editor() {
    let config = SqlConfig {
        host: "127.0.0.1".into(),
        port: std::env::var("RUSTTERM_MYSQL_TEST_PORT")
            .expect("set test port")
            .parse()
            .unwrap(),
        username: std::env::var("RUSTTERM_MYSQL_TEST_USER").unwrap_or_else(|_| "root".into()),
        database: String::new(),
        tls: false,
        ..SqlConfig::mysql()
    };
    let admin = DatabaseClient::connect(
        DatabaseConfig::Mysql(config.clone()),
        Secret::new(std::env::var("RUSTTERM_MYSQL_TEST_PASSWORD").unwrap_or_default()),
        None,
    )
    .await
    .unwrap();
    let suffix = uuid::Uuid::new_v4().simple().to_string();
    let suffix = &suffix[..12];
    let own = format!("rt_{suffix}_own");
    let member = format!("rt_{suffix}_member");
    let role = format!("rt_{suffix}_role");
    admin.execute(format!("CREATE USER '{own}'@'127.0.0.1' IDENTIFIED BY ''; CREATE USER '{member}'@'127.0.0.1' IDENTIFIED BY 'processlist-fixture'; CREATE ROLE '{role}'; GRANT PROCESS ON *.* TO '{role}'; GRANT '{role}' TO '{member}'@'127.0.0.1'; SET DEFAULT ROLE '{role}' TO '{member}'@'127.0.0.1';"),1).await.unwrap();
    for (username, expected) in [(&own, Visibility::Own), (&member, Visibility::All)] {
        let client = DatabaseClient::connect(
            DatabaseConfig::Mysql(SqlConfig {
                username: username.clone(),
                ..config.clone()
            }),
            Secret::new(if username == &member {
                "processlist-fixture".into()
            } else {
                String::new()
            }),
            None,
        )
        .await
        .unwrap();
        let monitor = client.mysql_monitor().unwrap();
        let snapshot = monitor.snapshot().await.unwrap();
        assert_eq!(snapshot.visibility, expected);
        if expected == Visibility::Own {
            assert!(snapshot.processes.iter().all(|p| p.user == *username));
        }
        client.begin().await.unwrap();
        let sql = format!("SELECT SLEEP(2), '{}'", "full_statement_".repeat(25));
        let worker = client.clone();
        let pending = tokio::spawn(async move { worker.execute(sql, 1).await });
        let mut found = false;
        for _ in 0..10 {
            let snapshot = monitor.snapshot().await.unwrap();
            if snapshot.processes.iter().any(|p| {
                p.sql
                    .as_ref()
                    .is_some_and(|sql| sql.contains("SLEEP(2)") && sql.len() > 300)
            }) {
                found = true;
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        assert!(found);
        assert!(!pending.is_finished());
        assert_eq!(client.transaction_status(), TransactionStatus::Active);
        pending.await.unwrap().unwrap();
        client.rollback().await.unwrap();
        client.close();
        assert!(matches!(
            monitor.snapshot().await,
            Err(DatabaseError::Disconnected)
        ));
    }
    admin
        .execute(
            format!("DROP USER '{own}'@'127.0.0.1', '{member}'@'127.0.0.1'; DROP ROLE '{role}';"),
            1,
        )
        .await
        .unwrap();
    admin.close();
}
