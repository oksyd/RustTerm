use rustterm::{
    config::{DatabaseConfig, Secret, SqlConfig},
    database::{DatabaseClient, DatabaseError, TransactionStatus},
};
use std::time::Duration;
use tokio::net::TcpListener;
#[path = "database/live.rs"]
mod live;
#[path = "database/wire.rs"]
mod wire;

async fn connect(mysql: bool, port: u16) -> DatabaseClient {
    let config = SqlConfig {
        port,
        tls: false,
        ..if mysql {
            SqlConfig::mysql()
        } else {
            SqlConfig::postgres()
        }
    };
    DatabaseClient::connect(
        if mysql {
            DatabaseConfig::Mysql(config)
        } else {
            DatabaseConfig::Postgres(config)
        },
        Secret::new(String::new()),
        None,
    )
    .await
    .unwrap()
}

#[tokio::test]
async fn sql_results_keep_empty_columns_values_and_statement_boundaries() {
    tokio::time::timeout(Duration::from_secs(10), async {
        for mysql in [false, true] {
            let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
            let port = listener.local_addr().unwrap().port();
            let server = tokio::spawn(async move {
                let (stream, _) = listener.accept().await.unwrap();
                let mut wire = wire::Wire::new(stream, mysql);
                if mysql {
                    wire.greeting(false).await;
                }
                wire.login().await;
                assert!(
                    wire.query()
                        .await
                        .ends_with("SELECT empty; SELECT values; UPDATE fixture")
                );
                wire.rows(&[("empty", false)], &[], true).await;
                wire.rows(
                    &[
                        ("text", false),
                        ("number", false),
                        ("nullable", false),
                        ("empty", false),
                    ],
                    &[vec![
                        Some("中文".as_bytes()),
                        Some(b"18446744073709551615"),
                        None,
                        Some(b""),
                    ]],
                    true,
                )
                .await;
                wire.complete(3, false).await;
                assert!(wire.query().await.ends_with("SELECT again"));
                wire.rows(&[("again", false)], &[vec![Some(b"ok")]], false)
                    .await;
            });
            let client = connect(mysql, port).await;
            let output = client
                .execute("SELECT empty; SELECT values; UPDATE fixture".into(), 10)
                .await
                .unwrap();
            assert_eq!(output.results.len(), 3, "mysql={mysql}");
            assert_eq!(output.results[0].columns, ["empty"], "mysql={mysql}");
            assert!(output.results[0].rows.is_empty());
            assert_eq!(
                output.results[1].rows,
                [vec![
                    Some("中文".into()),
                    Some("18446744073709551615".into()),
                    None,
                    Some(String::new())
                ]]
            );
            assert_eq!(output.results[2].affected_rows, 3);
            assert_eq!(
                client
                    .execute("SELECT again".into(), 10)
                    .await
                    .unwrap()
                    .results[0]
                    .rows,
                [vec![Some("ok".into())]]
            );
            client.close();
            server.await.unwrap();
        }
    })
    .await
    .unwrap();
}

#[tokio::test]
async fn sql_limits_errors_and_binary_values_preserve_connection_and_transactions() {
    tokio::time::timeout(Duration::from_secs(10), async {
        for mysql in [false, true] {
            let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
            let port = listener.local_addr().unwrap().port();
            let server = tokio::spawn(async move {
                let (stream, _) = listener.accept().await.unwrap();
                let mut wire = wire::Wire::new(stream, mysql);
                if mysql {
                    wire.greeting(false).await;
                }
                wire.login().await;
                assert!(
                    wire.query()
                        .await
                        .ends_with("SELECT bounded; UPDATE fixture")
                );
                wire.rows(
                    &[("value", false)],
                    &[
                        vec![Some(b"first")],
                        vec![Some(b"second")],
                        vec![Some(b"third")],
                    ],
                    true,
                )
                .await;
                wire.complete(5, false).await;
                assert!(wire.query().await.ends_with("SELECT broken"));
                if mysql {
                    wire.packet(b"\xff\x28\x04#42000fixture syntax error").await;
                } else {
                    wire.message(b'E', b"SERROR\0C42601\0Mfixture syntax error\0\0")
                        .await;
                    wire.ready(b'I').await;
                }
                assert!(wire.query().await.ends_with("SELECT binary"));
                let binary: &[u8] = if mysql { b"ABC" } else { b"\\x414243" };
                wire.rows(
                    &[("binary", true), ("text", false)],
                    &[vec![Some(binary), Some(b"ABC")]],
                    false,
                )
                .await;
                for command in ["BEGIN", "ROLLBACK"] {
                    assert!(wire.query().await.ends_with(command));
                    wire.complete(0, false).await;
                }
            });
            let client = connect(mysql, port).await;
            let output = client
                .execute("SELECT bounded; UPDATE fixture".into(), 1)
                .await
                .unwrap();
            assert_eq!(output.results.len(), 2);
            assert!(output.results[0].truncated);
            assert_eq!(output.results[0].rows, [vec![Some("first".into())]]);
            assert_eq!(output.results[1].affected_rows, 5);
            assert!(matches!(
                client.execute("SELECT broken".into(), 10).await,
                Err(DatabaseError::Server { .. })
            ));
            assert!(client.is_connected());
            assert_eq!(
                client
                    .execute("SELECT binary".into(), 10)
                    .await
                    .unwrap()
                    .results[0]
                    .rows,
                [vec![
                    Some(if mysql { "0x414243" } else { "\\x414243" }.into()),
                    Some("ABC".into())
                ]]
            );
            client.begin().await.unwrap();
            assert_eq!(client.transaction_status(), TransactionStatus::Active);
            client.rollback().await.unwrap();
            assert_eq!(client.transaction_status(), TransactionStatus::Idle);
            client.close();
            server.await.unwrap();
        }
    })
    .await
    .unwrap();
}

#[tokio::test]
async fn sql_authentication_errors_are_distinct_from_protocol_errors() {
    use tokio::io::AsyncReadExt;
    tokio::time::timeout(Duration::from_secs(5), async {
        for mysql in [false, true] {
            let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
            let port = listener.local_addr().unwrap().port();
            let server = tokio::spawn(async move {
                let (stream, _) = listener.accept().await.unwrap();
                let mut wire = wire::Wire::new(stream, mysql);
                if mysql {
                    wire.greeting(false).await;
                    wire.read_packet().await;
                    wire.packet(b"\xff\x15\x04#28000fixture access denied")
                        .await;
                } else {
                    let length = wire.stream.read_u32().await.unwrap();
                    let mut startup = vec![0; length as usize - 4];
                    wire.stream.read_exact(&mut startup).await.unwrap();
                    wire.message(b'E', b"SFATAL\0C28P01\0Mfixture access denied\0\0")
                        .await;
                }
            });
            let config = SqlConfig {
                port,
                tls: false,
                ..if mysql {
                    SqlConfig::mysql()
                } else {
                    SqlConfig::postgres()
                }
            };
            let result = DatabaseClient::connect(
                if mysql {
                    DatabaseConfig::Mysql(config)
                } else {
                    DatabaseConfig::Postgres(config)
                },
                Secret::new("invalid".into()),
                None,
            )
            .await;
            assert!(
                matches!(result, Err(DatabaseError::Authentication)),
                "mysql={mysql}"
            );
            server.await.unwrap();
        }
    })
    .await
    .unwrap();
}
