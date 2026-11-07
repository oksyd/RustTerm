use super::*;
use crate::config::SqlConfig;
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::{TcpListener, TcpStream},
};

fn config(engine: SqlEngine, port: u16) -> SqlConfig {
    let mut config = match engine {
        SqlEngine::Mysql => SqlConfig::mysql(),
        SqlEngine::Postgres => SqlConfig::postgres(),
        SqlEngine::Sqlite | SqlEngine::ClickHouse => {
            panic!("server fixture requires a server engine")
        }
    };
    config.host = "127.0.0.1".into();
    config.port = port;
    config.tls = false;
    config
}

fn pg_message(output: &mut Vec<u8>, tag: u8, body: &[u8]) {
    output.push(tag);
    output.extend_from_slice(&(body.len() as u32 + 4).to_be_bytes());
    output.extend_from_slice(body);
}
fn pg_columns(output: &mut Vec<u8>) {
    pg_named_columns(output, &["value", "nullable"]);
}
fn pg_named_columns(output: &mut Vec<u8>, names: &[&str]) {
    let mut body = (names.len() as i16).to_be_bytes().to_vec();
    for name in names {
        body.extend_from_slice(name.as_bytes());
        body.push(0);
        body.extend_from_slice(&0_u32.to_be_bytes());
        body.extend_from_slice(&0_i16.to_be_bytes());
        body.extend_from_slice(&25_u32.to_be_bytes());
        body.extend_from_slice(&(-1_i16).to_be_bytes());
        body.extend_from_slice(&(-1_i32).to_be_bytes());
        body.extend_from_slice(&0_i16.to_be_bytes());
    }
    pg_message(output, b'T', &body);
}
fn pg_row(output: &mut Vec<u8>, value: &str) {
    let mut body = 2_i16.to_be_bytes().to_vec();
    body.extend_from_slice(&(value.len() as i32).to_be_bytes());
    body.extend_from_slice(value.as_bytes());
    body.extend_from_slice(&(-1_i32).to_be_bytes());
    pg_message(output, b'D', &body);
}
async fn pg_start(stream: &mut TcpStream) {
    let length = stream.read_u32().await.unwrap() as usize;
    let mut startup = vec![0; length - 4];
    stream.read_exact(&mut startup).await.unwrap();
    let mut response = Vec::new();
    pg_message(&mut response, b'R', &0_u32.to_be_bytes());
    pg_message(&mut response, b'Z', b"I");
    stream.write_all(&response).await.unwrap();
}
async fn pg_query_raw(stream: &mut TcpStream) -> String {
    assert_eq!(stream.read_u8().await.unwrap(), b'Q');
    let length = stream.read_u32().await.unwrap() as usize;
    let mut query = vec![0; length - 4];
    stream.read_exact(&mut query).await.unwrap();
    assert_eq!(query.pop(), Some(0));
    String::from_utf8(query).unwrap()
}

fn untag_query(query: &str) -> &str {
    if query.starts_with("/* rustterm:") {
        query
            .split_once(" */ ")
            .expect("request marker terminates")
            .1
    } else {
        query
    }
}
async fn pg_query(stream: &mut TcpStream) -> String {
    untag_query(&pg_query_raw(stream).await).to_owned()
}

#[tokio::test]
async fn postgres_streams_multiple_results_and_reuses_after_truncation() {
    tokio::time::timeout(Duration::from_secs(5), async {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let config = config(SqlEngine::Postgres, listener.local_addr().unwrap().port());
        let server = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            pg_start(&mut stream).await;
            assert_eq!(pg_query(&mut stream).await, "fixture");
            let mut response = Vec::new();
            pg_columns(&mut response);
            pg_row(&mut response, "12345678901234567890.123456789");
            pg_message(&mut response, b'C', b"SELECT 1\0");
            pg_columns(&mut response);
            pg_message(&mut response, b'C', b"SELECT 0\0");
            pg_message(&mut response, b'C', b"UPDATE 5\0");
            pg_message(&mut response, b'Z', b"I");
            stream.write_all(&response).await.unwrap();
            assert_eq!(pg_query(&mut stream).await, "truncate");
            let mut response = Vec::new();
            pg_columns(&mut response);
            pg_row(&mut response, "first");
            pg_row(&mut response, "second");
            pg_message(&mut response, b'C', b"SELECT 2\0");
            pg_message(&mut response, b'Z', b"I");
            stream.write_all(&response).await.unwrap();
            assert_eq!(pg_query(&mut stream).await, "after");
            let mut response = Vec::new();
            pg_columns(&mut response);
            pg_row(&mut response, "usable");
            pg_message(&mut response, b'C', b"SELECT 1\0");
            pg_message(&mut response, b'Z', b"I");
            stream.write_all(&response).await.unwrap();
        });
        let client = DatabaseClient::connect(
            DatabaseConfig::Postgres(config),
            Secret::new(String::new()),
            None,
        )
        .await
        .unwrap();
        let result = client.execute("fixture".into(), 1).await.unwrap();
        assert_eq!(result.results.len(), 3);
        assert_eq!(result.results[0].columns, ["value", "nullable"]);
        assert_eq!(
            result.results[0].rows,
            [vec![Some("12345678901234567890.123456789".into()), None]]
        );
        assert!(!result.results[0].truncated);
        assert!(result.results[1].rows.is_empty());
        assert_eq!(result.results[2].affected_rows, 5);
        assert!(client.is_connected());
        let result = client.execute("truncate".into(), 1).await.unwrap();
        assert!(result.results[0].truncated);
        assert_eq!(result.results[0].rows.len(), 1);
        assert!(client.is_connected());
        let next = client.execute("after".into(), 1).await.unwrap();
        assert_eq!(next.results[0].rows[0][0].as_deref(), Some("usable"));
        server.await.unwrap();
    })
    .await
    .unwrap();
}

#[tokio::test]
async fn cancelling_postgres_uses_control_connection_and_preserves_session() {
    tokio::time::timeout(Duration::from_secs(5), async {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let config = config(SqlEngine::Postgres, listener.local_addr().unwrap().port());
        let (started, received) = oneshot::channel();
        let server = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            pg_start(&mut stream).await;
            let tagged = pg_query_raw(&mut stream).await;
            assert_eq!(untag_query(&tagged), "wait");
            let marker = tagged.split_once(" */ ").unwrap().0.to_owned() + " */";
            started.send(()).unwrap();
            let (mut control, _) = listener.accept().await.unwrap();
            pg_start(&mut control).await;
            let cancellation = pg_query(&mut control).await;
            assert!(cancellation.contains("FROM pg_catalog.pg_stat_activity"));
            assert!(cancellation.contains(&marker));
            assert!(cancellation.contains("usename = session_user"));
            let mut response = Vec::new();
            pg_columns(&mut response);
            pg_row(&mut response, "t");
            pg_message(&mut response, b'C', b"SELECT 1\0");
            pg_message(&mut response, b'Z', b"I");
            control.write_all(&response).await.unwrap();
            let mut response = Vec::new();
            pg_message(&mut response, b'E', b"SERROR\0C57014\0Mcancelled\0\0");
            pg_message(&mut response, b'Z', b"I");
            stream.write_all(&response).await.unwrap();
            assert_eq!(pg_query(&mut stream).await, "after");
            let mut response = Vec::new();
            pg_columns(&mut response);
            pg_row(&mut response, "usable");
            pg_message(&mut response, b'C', b"SELECT 1\0");
            pg_message(&mut response, b'Z', b"I");
            stream.write_all(&response).await.unwrap();
        });
        let client = DatabaseClient::connect(
            DatabaseConfig::Postgres(config),
            Secret::new(String::new()),
            None,
        )
        .await
        .unwrap();
        let query_client = client.clone();
        let query = tokio::spawn(async move { query_client.execute("wait".into(), 1).await });
        received.await.unwrap();
        client.cancel();
        assert_eq!(query.await.unwrap().unwrap_err(), DatabaseError::Cancelled);
        assert!(client.is_connected());
        assert_eq!(
            client.execute("after".into(), 1).await.unwrap().results[0].rows[0][0].as_deref(),
            Some("usable")
        );
        server.await.unwrap();
    })
    .await
    .unwrap();
}

fn mysql_packet(output: &mut Vec<u8>, sequence: &mut u8, body: &[u8]) {
    output.extend_from_slice(&(body.len() as u32).to_le_bytes()[..3]);
    output.push(*sequence);
    *sequence += 1;
    output.extend_from_slice(body);
}
async fn mysql_read_raw(stream: &mut TcpStream) -> Vec<u8> {
    let mut header = [0; 4];
    stream.read_exact(&mut header).await.unwrap();
    let length = u32::from_le_bytes([header[0], header[1], header[2], 0]) as usize;
    let mut body = vec![0; length];
    stream.read_exact(&mut body).await.unwrap();
    body
}
async fn mysql_read(stream: &mut TcpStream) -> Vec<u8> {
    let bytes = mysql_read_raw(stream).await;
    if bytes.starts_with(b"\x03/* rustterm:") {
        let mut query = vec![3];
        query.extend_from_slice(untag_query(std::str::from_utf8(&bytes[1..]).unwrap()).as_bytes());
        query
    } else {
        bytes
    }
}

fn mysql_ok(output: &mut Vec<u8>, sequence: &mut u8, affected: u8, more: bool) {
    mysql_packet(
        output,
        sequence,
        &[0, affected, 0, if more { 10 } else { 2 }, 0, 0, 0],
    );
}
fn mysql_columns(output: &mut Vec<u8>, sequence: &mut u8, binary: bool) {
    mysql_named_columns(output, sequence, binary, &["value", "nullable"]);
}
fn mysql_named_columns(output: &mut Vec<u8>, sequence: &mut u8, binary: bool, names: &[&str]) {
    mysql_packet(output, sequence, &[names.len() as u8]);
    for &name in names {
        let mut body = Vec::new();
        for field in ["def", "", "", "", name, ""] {
            body.push(field.len() as u8);
            body.extend_from_slice(field.as_bytes());
        }
        body.extend_from_slice(&[
            12,
            33,
            0,
            0,
            4,
            0,
            0,
            253,
            if binary { 128 } else { 0 },
            0,
            0,
            0,
            0,
        ]);
        mysql_packet(output, sequence, &body);
    }
    mysql_eof(output, sequence, false);
}
fn mysql_eof(output: &mut Vec<u8>, sequence: &mut u8, more: bool) {
    mysql_packet(
        output,
        sequence,
        &[0xfe, 0, 0, if more { 10 } else { 2 }, 0],
    );
}
fn mysql_row(output: &mut Vec<u8>, sequence: &mut u8, value: &str) {
    let mut row = vec![value.len() as u8];
    row.extend_from_slice(value.as_bytes());
    row.push(251);
    mysql_packet(output, sequence, &row);
}
async fn mysql_start(stream: &mut TcpStream) {
    let capabilities: u32 = 1 | 4 | 8 | 0x200 | 0x2000 | 0x8000 | 0x10000 | 0x20000 | 0x80000;
    let mut handshake = b"\x0a8.0.36\0".to_vec();
    handshake.extend_from_slice(&1_u32.to_le_bytes());
    handshake.extend_from_slice(b"12345678\0");
    handshake.extend_from_slice(&(capabilities as u16).to_le_bytes());
    handshake.extend_from_slice(&[33, 2, 0]);
    handshake.extend_from_slice(&((capabilities >> 16) as u16).to_le_bytes());
    handshake.push(21);
    handshake.extend_from_slice(&[0; 10]);
    handshake.extend_from_slice(b"901234567890\0mysql_native_password\0");
    let mut response = Vec::new();
    mysql_packet(&mut response, &mut 0, &handshake);
    stream.write_all(&response).await.unwrap();
    mysql_read(stream).await;
    let mut response = Vec::new();
    mysql_ok(&mut response, &mut 2, 0, false);
    stream.write_all(&response).await.unwrap();
    let setup = mysql_read(stream).await;
    assert_eq!(setup[0], 3);
    assert!(setup[1..].starts_with(b"SET NAMES"));
    let mut response = Vec::new();
    mysql_ok(&mut response, &mut 1, 0, false);
    stream.write_all(&response).await.unwrap();
}

#[tokio::test]
async fn mysql_streams_empty_and_multiple_results_and_stops_at_limit() {
    tokio::time::timeout(Duration::from_secs(5), async {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let config = config(SqlEngine::Mysql, listener.local_addr().unwrap().port());
        let server = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            mysql_start(&mut stream).await;
            assert_eq!(mysql_read(&mut stream).await, b"\x03fixture");
            let mut response = Vec::new();
            let mut sequence = 1;
            mysql_columns(&mut response, &mut sequence, false);
            mysql_row(
                &mut response,
                &mut sequence,
                "12345678901234567890.123456789",
            );
            mysql_eof(&mut response, &mut sequence, true);
            mysql_columns(&mut response, &mut sequence, false);
            mysql_eof(&mut response, &mut sequence, true);
            mysql_ok(&mut response, &mut sequence, 5, false);
            stream.write_all(&response).await.unwrap();
            assert_eq!(mysql_read(&mut stream).await, b"\x03binary");
            let mut response = Vec::new();
            let mut sequence = 1;
            mysql_columns(&mut response, &mut sequence, true);
            mysql_row(&mut response, &mut sequence, "A");
            mysql_eof(&mut response, &mut sequence, false);
            stream.write_all(&response).await.unwrap();
            assert_eq!(mysql_read(&mut stream).await, b"\x03truncate");
            let mut response = Vec::new();
            let mut sequence = 1;
            mysql_columns(&mut response, &mut sequence, false);
            mysql_row(&mut response, &mut sequence, "first");
            mysql_row(&mut response, &mut sequence, "second");
            mysql_eof(&mut response, &mut sequence, false);
            stream.write_all(&response).await.unwrap();
            assert_eq!(mysql_read(&mut stream).await, b"\x03after");
            let mut response = Vec::new();
            let mut sequence = 1;
            mysql_columns(&mut response, &mut sequence, false);
            mysql_row(&mut response, &mut sequence, "usable");
            mysql_eof(&mut response, &mut sequence, false);
            stream.write_all(&response).await.unwrap();
        });
        let client = DatabaseClient::connect(
            DatabaseConfig::Mysql(config),
            Secret::new(String::new()),
            None,
        )
        .await
        .unwrap();
        let result = client.execute("fixture".into(), 1).await.unwrap();
        assert_eq!(result.results.len(), 3);
        assert_eq!(result.results[0].columns, ["value", "nullable"]);
        assert_eq!(
            result.results[0].rows,
            [vec![Some("12345678901234567890.123456789".into()), None]]
        );
        assert!(!result.results[0].truncated);
        assert!(result.results[1].rows.is_empty());
        assert_eq!(result.results[2].affected_rows, 5);
        assert!(client.is_connected());
        let binary = client.execute("binary".into(), 1).await.unwrap();
        assert_eq!(binary.results[0].rows[0], [Some("0x41".into()), None]);
        let result = client.execute("truncate".into(), 1).await.unwrap();
        assert!(result.results[0].truncated);
        assert!(client.is_connected());
        let next = client.execute("after".into(), 1).await.unwrap();
        assert_eq!(next.results[0].rows[0][0].as_deref(), Some("usable"));
        server.await.unwrap();
    })
    .await
    .unwrap();
}

#[test]
fn identifiers_literals_and_diagnostics_preserve_boundaries() {
    assert_eq!(quote_identifier(SqlEngine::Mysql, "a`b"), "`a``b`");
    assert_eq!(quote_identifier(SqlEngine::Postgres, "a\"b"), "\"a\"\"b\"");
    assert_eq!(
        quote_literal(SqlEngine::Mysql, "a'b"),
        "CONVERT(X'612762' USING utf8mb4)"
    );
    assert_eq!(quote_literal(SqlEngine::Postgres, "a'\\b"), "E'a''\\\\b'");
    assert_eq!(display_bytes(&[0, 255]), "0x00ff");
    let error = server_error("42601".into(), "private sql value");
    assert!(!format!("{error:?}").contains("private"));
    assert!(error.to_string().contains("private sql value"));
}

#[tokio::test]
async fn cancelling_mysql_uses_control_connection_and_preserves_session() {
    tokio::time::timeout(Duration::from_secs(5), async {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let config = config(SqlEngine::Mysql, listener.local_addr().unwrap().port());
        let (started, received) = oneshot::channel();
        let server = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            mysql_start(&mut stream).await;
            let tagged = mysql_read_raw(&mut stream).await;
            let tagged = std::str::from_utf8(&tagged[1..]).unwrap();
            assert_eq!(untag_query(tagged), "wait");
            let marker = tagged.split_once(" */ ").unwrap().0.to_owned() + " */";
            started.send(()).unwrap();
            let (mut control, _) = listener.accept().await.unwrap();
            mysql_start(&mut control).await;
            let ownership = mysql_read(&mut control).await;
            let ownership = std::str::from_utf8(&ownership[1..]).unwrap();
            assert!(ownership.contains("information_schema.PROCESSLIST"));
            assert!(ownership.contains(&marker));
            let mut response = Vec::new();
            let mut sequence = 1;
            mysql_columns(&mut response, &mut sequence, false);
            mysql_row(&mut response, &mut sequence, "42");
            mysql_eof(&mut response, &mut sequence, false);
            control.write_all(&response).await.unwrap();
            assert_eq!(mysql_read(&mut control).await, b"\x03KILL QUERY 42");
            let mut response = Vec::new();
            mysql_ok(&mut response, &mut 1, 0, false);
            control.write_all(&response).await.unwrap();
            let mut response = Vec::new();
            let mut error = vec![0xff];
            error.extend_from_slice(&1317_u16.to_le_bytes());
            error.extend_from_slice(b"#70100Query execution was interrupted");
            mysql_packet(&mut response, &mut 1, &error);
            stream.write_all(&response).await.unwrap();
            assert_eq!(mysql_read(&mut stream).await, b"\x03after");
            let mut response = Vec::new();
            let mut sequence = 1;
            mysql_columns(&mut response, &mut sequence, false);
            mysql_row(&mut response, &mut sequence, "usable");
            mysql_eof(&mut response, &mut sequence, false);
            stream.write_all(&response).await.unwrap();
        });
        let client = DatabaseClient::connect(
            DatabaseConfig::Mysql(config),
            Secret::new(String::new()),
            None,
        )
        .await
        .unwrap();
        let query_client = client.clone();
        let query = tokio::spawn(async move { query_client.execute("wait".into(), 1).await });
        received.await.unwrap();
        client.cancel();
        assert_eq!(query.await.unwrap().unwrap_err(), DatabaseError::Cancelled);
        assert!(client.is_connected());
        assert_eq!(
            client.execute("after".into(), 1).await.unwrap().results[0].rows[0][0].as_deref(),
            Some("usable")
        );
        server.await.unwrap();
    })
    .await
    .unwrap();
}

#[tokio::test]
async fn cancellation_after_commit_keeps_confirmed_batch_success() {
    tokio::time::timeout(Duration::from_secs(5), async {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let config = config(SqlEngine::Postgres, listener.local_addr().unwrap().port());
        let (committing, committed) = oneshot::channel();
        let server = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            pg_start(&mut stream).await;
            for (sql, tag) in [
                ("BEGIN", b"BEGIN\0".as_slice()),
                ("UPDATE items SET value=1", b"UPDATE 1\0".as_slice()),
            ] {
                assert_eq!(pg_query(&mut stream).await, sql);
                let mut response = Vec::new();
                pg_message(&mut response, b'C', tag);
                pg_message(&mut response, b'Z', b"T");
                stream.write_all(&response).await.unwrap();
            }
            assert_eq!(pg_query(&mut stream).await, "COMMIT");
            committing.send(()).unwrap();
            let (mut control, _) = listener.accept().await.unwrap();
            pg_start(&mut control).await;
            assert!(pg_query(&mut control).await.contains("pg_stat_activity"));
            let mut response = Vec::new();
            pg_message(&mut response, b'C', b"COMMIT\0");
            pg_message(&mut response, b'Z', b"I");
            stream.write_all(&response).await.unwrap();
            let mut response = Vec::new();
            pg_columns(&mut response);
            pg_row(&mut response, "f");
            pg_message(&mut response, b'C', b"SELECT 1\0");
            pg_message(&mut response, b'Z', b"I");
            control.write_all(&response).await.unwrap();
        });
        let client = DatabaseClient::connect(
            DatabaseConfig::Postgres(config),
            Secret::new(String::new()),
            None,
        )
        .await
        .unwrap();
        let edit_client = client.clone();
        let edit = tokio::spawn(async move {
            edit_client
                .apply_batch(vec![table::GuardedStatement {
                    sql: "UPDATE items SET value=1".into(),
                    expected_rows: 1,
                }])
                .await
        });
        committed.await.unwrap();
        client.cancel();
        edit.await.unwrap().unwrap();
        assert!(client.is_connected());
        assert_eq!(client.transaction_status(), TransactionStatus::Idle);
        server.await.unwrap();
    })
    .await
    .unwrap();
}

#[tokio::test]
async fn mysql_raw_script_bytes_survive_session_quote_mode_changes() {
    const SCRIPT: &str = r"SET sql_mode='NO_BACKSLASH_ESCAPES'; SELECT '\'; SELECT ';';";
    tokio::time::timeout(Duration::from_secs(5), async {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let config = config(SqlEngine::Mysql, listener.local_addr().unwrap().port());
        let server = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            mysql_start(&mut stream).await;
            let packet = mysql_read_raw(&mut stream).await;
            assert_eq!(packet[0], 3);
            let query = std::str::from_utf8(&packet[1..]).unwrap();
            assert!(query.starts_with("/* rustterm:"));
            assert_eq!(untag_query(query).as_bytes(), SCRIPT.as_bytes());
            assert_eq!(query.matches("/* rustterm:").count(), 1);
            let mut response = Vec::new();
            mysql_ok(&mut response, &mut 1, 0, false);
            stream.write_all(&response).await.unwrap();
        });
        let client = DatabaseClient::connect(
            DatabaseConfig::Mysql(config),
            Secret::new(String::new()),
            None,
        )
        .await
        .unwrap();
        client.execute(SCRIPT.into(), 1).await.unwrap();
        assert_eq!(client.transaction_status(), TransactionStatus::Unknown);
        server.await.unwrap();
    })
    .await
    .unwrap();
}

#[tokio::test]
async fn connection_probes_read_database_versions_without_requiring_metadata_permissions() {
    use crate::{
        config::Profile,
        connection_probe::{self, ProbeKind, ProbeRequest},
    };
    for engine in [SqlEngine::Mysql, SqlEngine::Postgres] {
        for allow_version in [true, false] {
            tokio::time::timeout(Duration::from_secs(5), async {
                let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
                let config = config(engine, listener.local_addr().unwrap().port());
                let version = if engine == SqlEngine::Mysql {
                    "8.0.36"
                } else {
                    "16.2"
                };
                let server = tokio::spawn(async move {
                    let (mut stream, _) = listener.accept().await.unwrap();
                    if engine == SqlEngine::Mysql {
                        mysql_start(&mut stream).await;
                    } else {
                        pg_start(&mut stream).await;
                    }
                    for (query, value) in [
                        ("SELECT 1", "1"),
                        (
                            if engine == SqlEngine::Mysql {
                                "SELECT VERSION()"
                            } else {
                                "SHOW server_version"
                            },
                            version,
                        ),
                    ] {
                        let denied = value == version && !allow_version;
                        let mut response = Vec::new();
                        if engine == SqlEngine::Mysql {
                            assert_eq!(&mysql_read(&mut stream).await[1..], query.as_bytes());
                            let mut sequence = 1;
                            if denied {
                                let mut error = vec![0xff];
                                error.extend_from_slice(&1370_u16.to_le_bytes());
                                error.extend_from_slice(b"#42000version function denied");
                                mysql_packet(&mut response, &mut sequence, &error);
                            } else {
                                mysql_named_columns(
                                    &mut response,
                                    &mut sequence,
                                    false,
                                    &["value"],
                                );
                                let mut row = vec![value.len() as u8];
                                row.extend_from_slice(value.as_bytes());
                                mysql_packet(&mut response, &mut sequence, &row);
                                mysql_eof(&mut response, &mut sequence, false);
                            }
                        } else {
                            assert_eq!(pg_query(&mut stream).await, query);
                            if denied {
                                pg_message(
                                    &mut response,
                                    b'E',
                                    b"SERROR\0C42501\0Mversion query denied\0\0",
                                );
                            } else {
                                pg_named_columns(&mut response, &["value"]);
                                let mut row = 1_i16.to_be_bytes().to_vec();
                                row.extend_from_slice(&(value.len() as i32).to_be_bytes());
                                row.extend_from_slice(value.as_bytes());
                                pg_message(&mut response, b'D', &row);
                                pg_message(&mut response, b'C', b"SELECT 1\0");
                            }
                            pg_message(&mut response, b'Z', b"I");
                        }
                        stream.write_all(&response).await.unwrap();
                    }
                });
                let profile = if engine == SqlEngine::Mysql {
                    Profile::mysql("MySQL", config)
                } else {
                    Profile::postgres("PostgreSQL", config)
                };
                let (hosts, _) = mpsc::channel(1);
                let (certificates, _) = mpsc::channel(1);
                let outcome = connection_probe::test(
                    ProbeRequest {
                        profile,
                        secret: Secret::new(String::new()),
                        sentinel_secret: None,
                        ssh: None,
                    },
                    hosts,
                    certificates,
                    CancellationToken::new(),
                )
                .await
                .unwrap();
                assert_eq!(outcome.kind, ProbeKind::DatabaseReady);
                let info = outcome.server.unwrap();
                assert_eq!(info.product, engine.label());
                assert_eq!(info.version.as_deref(), allow_version.then_some(version));
                server.await.unwrap();
            })
            .await
            .unwrap();
        }
    }
}

async fn mysql_fields(stream: &mut TcpStream, names: &[&str], rows: &[Vec<Option<&str>>]) {
    let mut response = Vec::new();
    let mut sequence = 1;
    mysql_named_columns(&mut response, &mut sequence, false, names);
    for fields in rows {
        let mut row = Vec::new();
        for value in fields {
            if let Some(value) = value {
                if value.len() < 251 {
                    row.push(value.len() as u8);
                } else {
                    row.push(0xfc);
                    row.extend_from_slice(&(value.len() as u16).to_le_bytes());
                }
                row.extend_from_slice(value.as_bytes());
            } else {
                row.push(251);
            }
        }
        mysql_packet(&mut response, &mut sequence, &row);
    }
    mysql_eof(&mut response, &mut sequence, false);
    stream.write_all(&response).await.unwrap();
}

#[tokio::test]
async fn mysql_monitor_uses_separate_connection_preserves_transaction_and_closes_with_parent() {
    tokio::time::timeout(Duration::from_secs(5), async {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let config = config(SqlEngine::Mysql, listener.local_addr().unwrap().port());
        let (running, started) = oneshot::channel();
        let (release, wait) = oneshot::channel();
        let server = tokio::spawn(async move {
            let (mut main, _) = listener.accept().await.unwrap();
            mysql_start(&mut main).await;
            assert_eq!(mysql_read(&mut main).await, b"\x03BEGIN");
            let mut ok = Vec::new();
            mysql_ok(&mut ok, &mut 1, 0, false);
            main.write_all(&ok).await.unwrap();
            assert_eq!(mysql_read(&mut main).await, b"\x03long query");
            running.send(()).unwrap();
            let (mut monitor, _) = listener.accept().await.unwrap();
            mysql_start(&mut monitor).await;
            for _ in 0..2 {
                assert_eq!(mysql_read(&mut monitor).await, b"\x03SHOW FULL PROCESSLIST");
                mysql_fields(
                    &mut monitor,
                    &[
                        "Id", "User", "Host", "db", "Command", "Time", "State", "Info",
                    ],
                    &[vec![
                        Some("42"),
                        Some("app"),
                        Some("localhost"),
                        None,
                        Some("Query"),
                        Some("20"),
                        Some("executing"),
                        Some("long query"),
                    ]],
                )
                .await;
                assert_eq!(mysql_read(&mut monitor).await, b"\x03SHOW GRANTS");
                mysql_fields(
                    &mut monitor,
                    &["Grants"],
                    &[vec![Some("GRANT SELECT, PROCESS ON *.* TO `app`@`%`")]],
                )
                .await;
            }
            wait.await.unwrap();
            main.write_all(&ok).await.unwrap();
            assert_eq!(mysql_read(&mut main).await, b"\x03ROLLBACK");
            main.write_all(&ok).await.unwrap();
            let mut byte = [0];
            let count = monitor.read(&mut byte).await.unwrap();
            assert_eq!(count, 0);
        });
        let client = DatabaseClient::connect(
            DatabaseConfig::Mysql(config),
            Secret::new(String::new()),
            None,
        )
        .await
        .unwrap();
        let monitor = client.mysql_monitor().unwrap();
        client.begin().await.unwrap();
        let worker = client.clone();
        let pending = tokio::spawn(async move { worker.execute("long query".into(), 1).await });
        started.await.unwrap();
        for _ in 0..2 {
            let snapshot = monitor.snapshot().await.unwrap();
            assert_eq!(snapshot.processes[0].id, 42);
            assert_eq!(snapshot.visibility, processlist::Visibility::All);
            assert_eq!(client.transaction_status(), TransactionStatus::Active);
            assert!(!pending.is_finished());
        }
        release.send(()).unwrap();
        pending.await.unwrap().unwrap();
        client.rollback().await.unwrap();
        client.close();
        assert!(matches!(
            monitor.snapshot().await,
            Err(DatabaseError::Disconnected)
        ));
        server.await.unwrap();
    })
    .await
    .unwrap();
}

#[tokio::test]
async fn cancelling_mysql_monitor_never_cancels_the_editor_connection() {
    tokio::time::timeout(Duration::from_secs(5), async {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let config = config(SqlEngine::Mysql, listener.local_addr().unwrap().port());
        let (sent, received) = oneshot::channel();
        let server = tokio::spawn(async move {
            let (mut main, _) = listener.accept().await.unwrap();
            mysql_start(&mut main).await;
            let (mut monitor, _) = listener.accept().await.unwrap();
            mysql_start(&mut monitor).await;
            assert_eq!(mysql_read(&mut monitor).await, b"\x03SHOW FULL PROCESSLIST");
            sent.send(()).unwrap();
            let mut byte = [0];
            assert_eq!(monitor.read(&mut byte).await.unwrap(), 0);
            assert_eq!(mysql_read(&mut main).await, b"\x03SELECT 1");
            mysql_fields(&mut main, &["value"], &[vec![Some("1")]]).await;
        });
        let client = DatabaseClient::connect(
            DatabaseConfig::Mysql(config),
            Secret::new(String::new()),
            None,
        )
        .await
        .unwrap();
        let monitor = client.mysql_monitor().unwrap();
        let reader = monitor.clone();
        let pending = tokio::spawn(async move { reader.snapshot().await });
        received.await.unwrap();
        assert!(matches!(monitor.snapshot().await, Err(DatabaseError::Busy)));
        pending.abort();
        let _ = pending.await;
        assert_eq!(
            client.execute("SELECT 1".into(), 1).await.unwrap().results[0].rows[0][0].as_deref(),
            Some("1")
        );
        assert!(client.is_connected());
        monitor.close();
        client.close();
        server.await.unwrap();
    })
    .await
    .unwrap();
}
