use rustterm::{
    config::{DatabaseConfig, Secret, SqlConfig},
    database::{
        DatabaseClient, DatabaseError, TableInfo, TransactionStatus,
        table::{CellValue, PageRequest},
    },
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
                let json = r#"{"msg":"示例"}"#.as_bytes();
                let binary: &[u8] = if mysql {
                    json
                } else {
                    b"\\x7b226d7367223a22e7a4bae4be8b227d"
                };
                let opaque: &[u8] = if mysql { &[65, 0, 255] } else { b"\\x4100ff" };
                wire.rows(
                    &[("binary", true), ("text", false)],
                    &[
                        vec![Some(binary), Some(b"\\x4142")],
                        vec![Some(opaque), Some(b"0x4100ff")],
                    ],
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
                [
                    vec![
                        Some(
                            if mysql {
                                r#"{"msg":"示例"}"#
                            } else {
                                "\\x7b226d7367223a22e7a4bae4be8b227d"
                            }
                            .into()
                        ),
                        Some("\\x4142".into())
                    ],
                    vec![
                        Some(if mysql { "0x4100ff" } else { "\\x4100ff" }.into()),
                        Some("0x4100ff".into())
                    ]
                ]
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
async fn server_table_binary_previews_keep_the_bytes_used_to_edit() {
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
            wire.query().await;
            let (data_type, relation, extra): (&[u8], &[u8], &[u8]) = if mysql {
                (b"blob", b"BASE TABLE", b"InnoDB")
            } else {
                (b"bytea", b"r", b"bytea")
            };
            wire.rows(
                &[("name", false); 8],
                &[vec![
                    Some(b"payload"),
                    Some(data_type),
                    Some(b"NO"),
                    None,
                    Some(b""),
                    Some(b"1"),
                    Some(relation),
                    Some(extra),
                ]],
                false,
            )
            .await;
            let sql = wire.query().await;
            assert!(
                sql.contains(if mysql { "HEX(" } else { "pg_catalog.encode(" }),
                "{sql}"
            );
            wire.rows(
                &[("encoded", false)],
                &[
                    vec![Some(b"307834313432")],
                    vec![Some(b"00ff")],
                    vec![Some(b"")],
                ],
                false,
            )
            .await;
        });
        let client = connect(mysql, port).await;
        let schema = client
            .table_schema(TableInfo {
                schema: "fixture".into(),
                name: "cache".into(),
                kind: "TABLE".into(),
            })
            .await
            .unwrap();
        let page = client
            .table_page(&schema, PageRequest::default())
            .await
            .unwrap();
        assert_eq!(page.result.columns, ["payload"]);
        for (row, (display, bytes)) in [
            ("0x4142", b"0x4142".as_slice()),
            ("0x00ff", &[0, 255]),
            ("0x", &[]),
        ]
        .into_iter()
        .enumerate()
        {
            assert_eq!(page.result.rows[row][0].as_deref(), Some(display));
            assert_eq!(page.originals[row][0], CellValue::Binary(bytes.to_vec()));
            let original = &page.originals[row][0];
            assert_eq!(
                schema
                    .parse_cell(0, Some(original), &original.display().unwrap())
                    .unwrap(),
                *original
            );
        }
        client.close();
        server.await.unwrap();
    }
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
