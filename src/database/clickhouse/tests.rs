use super::*;
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::{TcpListener, TcpStream},
};

fn json(rows: &[Vec<Option<&str>>]) -> String {
    let columns = rows.first().map_or(1, Vec::len);
    serde_json::json!({"meta": (0..columns).map(|i| serde_json::json!({"name":format!("c{i}"),"type":"String"})).collect::<Vec<_>>(),"data":rows,"rows":rows.len()}).to_string()
}
async fn request(stream: &mut TcpStream) -> (String, String) {
    let mut bytes = Vec::new();
    while !bytes.ends_with(b"\r\n\r\n") {
        bytes.push(
            tokio::time::timeout(Duration::from_secs(5), stream.read_u8())
                .await
                .unwrap()
                .unwrap(),
        );
        assert!(bytes.len() < 8192);
    }
    let headers = String::from_utf8(bytes).unwrap();
    let length: usize = headers
        .lines()
        .find_map(|line| {
            let (name, value) = line.split_once(':')?;
            name.eq_ignore_ascii_case("content-length")
                .then(|| value.trim().parse().unwrap())
        })
        .unwrap_or(0);
    let mut body = vec![0; length];
    stream.read_exact(&mut body).await.unwrap();
    (headers, String::from_utf8(body).unwrap())
}
async fn respond(stream: &mut TcpStream, status: u16, extra: &str, body: &str) {
    stream.write_all(format!("HTTP/1.1 {status} Response\r\nContent-Length: {}\r\nConnection: close\r\n{extra}\r\n{body}", body.len()).as_bytes()).await.unwrap();
}
async fn listener() -> (TcpListener, SqlConfig) {
    let listener = TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
    let config = SqlConfig {
        port: listener.local_addr().unwrap().port(),
        database: String::new(),
        ..SqlConfig::clickhouse()
    };
    (listener, config)
}
#[test]
fn exact_values_nulls_limits_and_late_exceptions() {
    let values = [
        vec![
            Some("18446744073709551615"),
            Some("12345678901234567890.123456789"),
            None,
            Some("['a','b']"),
        ],
        vec![Some("1"), Some("2"), Some("NULL"), Some("[]")],
    ];
    let result = parse_response(json(&values).as_bytes(), 1, 0).unwrap();
    assert_eq!(
        result.rows[0],
        values[0]
            .iter()
            .map(|v| v.map(str::to_owned))
            .collect::<Vec<_>>()
    );
    assert!(result.truncated);
    let bad = format!("{}\nCode: 395. DB::Exception: late failure", json(&values));
    assert!(
        matches!(parse_response(bad.as_bytes(), 10, 0), Err(DatabaseError::Server {code,..}) if code == "395")
    );
    assert!(matches!(
        parse_response(br#"{"meta":[],"data":[["x"]],"rows":1}"#, 10, 0),
        Err(DatabaseError::Protocol)
    ));
    assert!(matches!(
        parse_response(b"not JSON", 1, 0),
        Err(DatabaseError::ResultFormat)
    ));
}
#[tokio::test]
async fn authenticated_session_script_limits_and_transaction_guards() {
    let (listener, mut config) = listener().await;
    config.database = "default".into();
    let server = tokio::spawn(async move {
        let mut session = None;
        for (index, sql) in [
            "SELECT version()",
            "USE `default`",
            "SELECT 'a;\\'b';",
            "SELECT 2;",
        ]
        .into_iter()
        .enumerate()
        {
            let (mut stream, _) = listener.accept().await.unwrap();
            let (headers, body) = request(&mut stream).await;
            assert_eq!(body, sql);
            assert!(
                headers
                    .to_ascii_lowercase()
                    .contains("authorization: basic zgvmyxvsddpwyxnzd29yza==")
            );
            let target = headers
                .lines()
                .next()
                .unwrap()
                .split_whitespace()
                .nth(1)
                .unwrap();
            assert!(!target.contains("password"));
            let url = url::Url::parse(&format!("http://localhost{target}")).unwrap();
            let params: std::collections::HashMap<_, _> = url.query_pairs().into_owned().collect();
            assert_eq!(params["default_format"], "JSONCompact");
            assert_eq!(params["max_result_rows"], "2");
            if index == 0 {
                session = Some(params["session_id"].clone());
                assert_eq!(params["database"], "default");
            } else {
                assert_eq!(session.as_deref(), Some(params["session_id"].as_str()));
                assert_eq!(params["session_check"], "1");
                assert!(!params.contains_key("database"));
            }
            respond(
                &mut stream,
                200,
                "",
                &json(&[vec![Some(if index == 0 { "25.8.1" } else { "2" })]]),
            )
            .await;
        }
    });
    let client = DatabaseClient::connect(
        DatabaseConfig::ClickHouse(config),
        Secret::new("password".into()),
        None,
    )
    .await
    .unwrap();
    assert!(matches!(
        client.begin().await,
        Err(DatabaseError::TransactionsUnsupported)
    ));
    assert!(matches!(
        client.execute("BEGIN".into(), 1).await,
        Err(DatabaseError::TransactionsUnsupported)
    ));
    assert_eq!(client.transaction_status(), TransactionStatus::Idle);
    let output = client
        .execute("SELECT 'a;\\'b'; SELECT 2;".into(), 1)
        .await
        .unwrap();
    assert_eq!(output.results.len(), 2);
    assert!(client.is_connected());
    client.close();
    server.await.unwrap();
}
#[tokio::test]
async fn cancellation_kills_the_exact_server_query_without_locking_its_session() {
    cancellation(false).await;
}
#[tokio::test]
async fn timeout_kills_the_query_and_keeps_the_connection_usable() {
    cancellation(true).await;
}
async fn cancellation(timeout: bool) {
    let (listener, mut config) = listener().await;
    config.query_timeout_secs = if timeout { 1 } else { 60 };
    let (started, ready) = oneshot::channel();
    let server = tokio::spawn(async move {
        let (mut opening, _) = listener.accept().await.unwrap();
        request(&mut opening).await;
        respond(&mut opening, 200, "", &json(&[vec![Some("25.8.1")]])).await;
        let (mut running, _) = listener.accept().await.unwrap();
        let (headers, _) = request(&mut running).await;
        let target = headers
            .lines()
            .next()
            .unwrap()
            .split_whitespace()
            .nth(1)
            .unwrap();
        let url = url::Url::parse(&format!("http://localhost{target}")).unwrap();
        let query_id = url
            .query_pairs()
            .find(|(k, _)| k == "query_id")
            .unwrap()
            .1
            .into_owned();
        started.send(()).unwrap();
        // The first KILL can arrive before ClickHouse registers the HTTP query.
        let (mut early, _) = listener.accept().await.unwrap();
        request(&mut early).await;
        respond(&mut early, 200, "", &json(&[])).await;
        let (mut control, _) = listener.accept().await.unwrap();
        let (headers, body) = request(&mut control).await;
        assert!(!headers.contains("session_id="));
        assert_eq!(
            body,
            format!(
                "KILL QUERY WHERE query_id = {} SYNC",
                quote_literal(SqlEngine::ClickHouse, &query_id)
            )
        );
        respond(
            &mut running,
            500,
            "X-ClickHouse-Exception-Code: 394\r\n",
            "Code: 394. DB::Exception: Query was cancelled",
        )
        .await;
        respond(&mut control, 200, "", &json(&[vec![Some("finished")]])).await;
        let (mut next, _) = listener.accept().await.unwrap();
        request(&mut next).await;
        respond(&mut next, 200, "", &json(&[vec![Some("1")]])).await;
    });
    let client = DatabaseClient::connect(
        DatabaseConfig::ClickHouse(config),
        Secret::new(String::new()),
        None,
    )
    .await
    .unwrap();
    let worker = client.clone();
    let query = tokio::spawn(async move { worker.execute("SELECT sleep(3)".into(), 1).await });
    ready.await.unwrap();
    if !timeout {
        client.cancel();
    }
    let error = query.await.unwrap().unwrap_err();
    assert_eq!(
        error,
        if timeout {
            DatabaseError::Timeout
        } else {
            DatabaseError::Cancelled
        }
    );
    assert!(client.is_connected());
    assert_eq!(
        client.execute("SELECT 1".into(), 1).await.unwrap().results[0].rows[0][0].as_deref(),
        Some("1")
    );
    client.close();
    server.await.unwrap();
}
#[tokio::test]
async fn redirects_do_not_forward_credentials_or_replay_queries() {
    let (listener, config) = listener().await;
    let server = tokio::spawn(async move {
        let (mut stream, _) = listener.accept().await.unwrap();
        request(&mut stream).await;
        respond(
            &mut stream,
            302,
            "Location: http://example.invalid/\r\n",
            "redirect",
        )
        .await;
        assert!(
            tokio::time::timeout(Duration::from_millis(100), listener.accept())
                .await
                .is_err()
        );
    });
    assert!(
        matches!(DatabaseClient::connect(DatabaseConfig::ClickHouse(config),Secret::new("password".into()),None).await,Err(DatabaseError::Server {code,..}) if code == "302")
    );
    server.await.unwrap();
}
#[test]
fn clickhouse_profile_round_trip_preserves_credential_reference() {
    let mut config = SqlConfig::clickhouse();
    config.credential = Some(crate::config::CredentialRef::new());
    let profile = crate::config::Profile::clickhouse("Analytics", config);
    let text = toml::to_string(&profile).unwrap();
    assert!(text.contains("clickhouse"));
    let restored: crate::config::Profile = toml::from_str(&text).unwrap();
    assert_eq!(profile.connection, restored.connection);
    assert_eq!(restored.connection.sql().unwrap().0, SqlEngine::ClickHouse);
}

#[test]
fn raw_json_preserves_numeric_precision_and_distinguishes_null_from_text() {
    let body = br#"{"meta":[{"name":"integer"},{"name":"decimal"},{"name":"absent"},{"name":"text"},{"name":"array"}],"data":[[18446744073709551616,12345678901234567890.123456789,null,"NULL",[1,"x",null]]],"rows":1}"#;
    let result = parse_response(body, 1, 0).unwrap();
    assert_eq!(
        result.rows[0],
        vec![
            Some("18446744073709551616".into()),
            Some("12345678901234567890.123456789".into()),
            None,
            Some("NULL".into()),
            Some("[1,\"x\",null]".into())
        ]
    );
}
#[test]
fn clickhouse_sql_boundaries_keep_comments_identifiers_and_heredocs_intact() {
    let script = "SELECT $text$a;b$text$; /* outer /* inner ; */ end */ SELECT 1 AS `a\\`;b`; // ;\nSELECT 3;";
    let statements = sql::statements(script, SqlEngine::ClickHouse).unwrap();
    assert_eq!(statements.len(), 3);
    assert!(statements.iter().all(|s| s.complete));
    assert_eq!(
        &script[statements[0].range.clone()],
        "SELECT $text$a;b$text$;"
    );
    assert_eq!(
        &script[statements[1].range.clone()],
        "SELECT 1 AS `a\\`;b`;"
    );
}
#[tokio::test]
async fn authentication_errors_and_result_size_are_reported_without_unbounded_reads() {
    for oversized in [false, true] {
        let (listener, config) = listener().await;
        let server = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            request(&mut stream).await;
            if oversized {
                let header = format!(
                    "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                    MAX_RESULT_BYTES + 1
                );
                stream.write_all(header.as_bytes()).await.unwrap();
                let bytes = vec![b'x'; MAX_RESULT_BYTES + 1];
                let _ = stream.write_all(&bytes).await;
            } else {
                respond(
                    &mut stream,
                    403,
                    "X-ClickHouse-Exception-Code: 516\r\n",
                    "authentication failed",
                )
                .await;
            }
        });
        let result = DatabaseClient::connect(
            DatabaseConfig::ClickHouse(config),
            Secret::new(String::new()),
            None,
        )
        .await;
        assert!(matches!(
            (&result, oversized),
            (Err(DatabaseError::Limit), true) | (Err(DatabaseError::Authentication), false)
        ));
        server.await.unwrap();
    }
}
#[tokio::test]
async fn untrusted_tls_is_rejected_before_credentials_are_sent() {
    use rustls::pki_types::{CertificateDer, PrivateKeyDer, pem::PemObject};
    let (listener, mut config) = listener().await;
    config.tls = true;
    let server = rustls::ServerConfig::builder_with_provider(Arc::new(
        rustls::crypto::ring::default_provider(),
    ))
    .with_safe_default_protocol_versions()
    .unwrap()
    .with_no_client_auth()
    .with_single_cert(
        vec![
            CertificateDer::from_pem_slice(include_bytes!(
                "../../../tests/fixtures/elasticsearch/server.pem"
            ))
            .unwrap(),
        ],
        PrivateKeyDer::from_pem_slice(include_bytes!(
            "../../../tests/fixtures/elasticsearch/server-key.pem"
        ))
        .unwrap(),
    )
    .unwrap();
    let server = tokio::spawn(async move {
        let (socket, _) = listener.accept().await.unwrap();
        assert!(
            tokio_rustls::TlsAcceptor::from(Arc::new(server))
                .accept(socket)
                .await
                .is_err()
        );
    });
    assert_eq!(
        DatabaseClient::connect(
            DatabaseConfig::ClickHouse(config),
            Secret::new("secret".into()),
            None
        )
        .await
        .err(),
        Some(DatabaseError::Tls)
    );
    server.await.unwrap();
}

#[test]
fn readonly_tsv_keeps_null_literals_escapes_and_special_numbers_distinct() {
    let body=b"absent\ttext\tescaped\tnumber\tdecimal\tarray\nNullable(String)\tString\tString\tFloat64\tDecimal(38,9)\tArray(String)\n\\N\t\\\\N\tline\\nnext\\tcell\tinf\t12345678901234567890.123456789\t['a','b']\n";
    let result = parse_tsv(body, 1, 0).unwrap();
    assert_eq!(
        result.rows[0],
        vec![
            None,
            Some("\\N".into()),
            Some("line\nnext\tcell".into()),
            Some("inf".into()),
            Some("12345678901234567890.123456789".into()),
            Some("['a','b']".into())
        ]
    );
    assert!(matches!(
        parse_tsv(b"a\tb\nString\tString\nonly_one\n", 1, 0),
        Err(DatabaseError::ResultFormat)
    ));
}
#[tokio::test]
async fn readonly_profile_falls_back_only_during_handshake() {
    let (listener, config) = listener().await;
    let server = tokio::spawn(async move {
        for step in 0..3 {
            let (mut stream, _) = listener.accept().await.unwrap();
            let (headers, sql) = request(&mut stream).await;
            if step == 0 {
                assert_eq!(sql, "SELECT version()");
                respond(
                    &mut stream,
                    500,
                    "X-ClickHouse-Exception-Code: 164\r\n",
                    "Cannot modify setting in readonly mode",
                )
                .await;
            } else {
                assert!(headers.contains("default_format=TabSeparatedWithNamesAndTypes"));
                assert!(!headers.contains("max_result_rows="));
                if step == 1 {
                    respond(&mut stream, 200, "", "version()\nString\n25.8.1\n").await;
                } else {
                    assert_eq!(sql, "SELECT 1");
                    respond(&mut stream, 200, "", "1\nUInt8\n1\n").await;
                }
            }
        }
    });
    let client = DatabaseClient::connect(
        DatabaseConfig::ClickHouse(config),
        Secret::new(String::new()),
        None,
    )
    .await
    .unwrap();
    assert_eq!(
        client.execute("SELECT 1".into(), 1).await.unwrap().results[0].rows[0][0].as_deref(),
        Some("1")
    );
    client.close();
    server.await.unwrap();
}
#[tokio::test]
async fn server_byte_limits_mark_small_row_counts_as_incomplete() {
    let (listener, config) = listener().await;
    let server = tokio::spawn(async move {
        for step in 0..2 {
            let (mut stream, _) = listener.accept().await.unwrap();
            request(&mut stream).await;
            let headers = if step == 0 {
                String::new()
            } else {
                format!(
                    "X-ClickHouse-Summary: {{\"result_bytes\":\"{}\"}}\r\n",
                    MAX_RESULT_BYTES + 1
                )
            };
            respond(&mut stream, 200, &headers, &json(&[vec![Some("25.8.1")]])).await;
        }
    });
    let client = DatabaseClient::connect(
        DatabaseConfig::ClickHouse(config),
        Secret::new(String::new()),
        None,
    )
    .await
    .unwrap();
    assert!(
        client
            .execute("SELECT 1".into(), 1000)
            .await
            .unwrap()
            .results[0]
            .truncated
    );
    client.close();
    server.await.unwrap();
}
