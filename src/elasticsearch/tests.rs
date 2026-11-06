use super::*;
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::TcpListener,
};

const INFO: &str = r#"{"cluster_name":"fixture","version":{"number":"8.19.0"}}"#;

#[tokio::test]
async fn connection_probe_reuses_the_version_from_the_handshake() {
    use crate::{
        config::Profile,
        connection_probe::{self, ProbeKind, ProbeRequest},
    };
    let (config, server) = mock(vec![(200, INFO.into())]).await;
    let (hosts, _) = mpsc::channel(1);
    let (certificates, _) = mpsc::channel(1);
    let outcome = connection_probe::test(
        ProbeRequest {
            profile: Profile::elasticsearch("ES", config),
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
    assert_eq!(outcome.kind, ProbeKind::ElasticsearchReady);
    let info = outcome.server.unwrap();
    assert_eq!(info.product, "Elasticsearch");
    assert_eq!(info.version.as_deref(), Some("8.19.0"));
    assert_eq!(server.await.unwrap().len(), 1);
}

pub(super) async fn read_request(
    socket: &mut (impl tokio::io::AsyncRead + Unpin),
) -> (String, String) {
    let mut bytes = Vec::new();
    loop {
        let byte = tokio::time::timeout(Duration::from_secs(3), socket.read_u8())
            .await
            .unwrap()
            .unwrap();
        bytes.push(byte);
        if bytes.ends_with(b"\r\n\r\n") {
            break;
        }
        assert!(bytes.len() < 8192);
    }
    let headers = String::from_utf8(bytes).unwrap();
    let length = headers
        .lines()
        .find_map(|line| {
            let (key, value) = line.split_once(':')?;
            key.eq_ignore_ascii_case("content-length")
                .then(|| value.trim().parse::<usize>().unwrap())
        })
        .unwrap_or(0);
    assert!(length <= MAX_QUERY_BYTES);
    let mut body = vec![0; length];
    socket.read_exact(&mut body).await.unwrap();
    (headers, String::from_utf8(body).unwrap())
}

#[tokio::test]
async fn empty_indices_are_valid_and_large_lists_are_bounded() {
    let names: Vec<_> = (0..MAX_INDICES + 3)
        .rev()
        .map(|i| serde_json::json!({"name":format!("logs-{i:05}"),"attributes":["open"]}))
        .collect();
    let (config, server) = mock(vec![
        (200, INFO.into()),
        (200, "{}".into()),
        (200, r#"{"indices":[]}"#.into()),
        (200, serde_json::json!({"indices":names}).to_string()),
    ])
    .await;
    let client = ElasticsearchClient::connect(config, Secret::new(String::new()), None)
        .await
        .unwrap();
    for _ in 0..2 {
        let list = client.indices().await.unwrap();
        assert!(list.indices.is_empty());
        assert!(!list.truncated);
    }
    let list = client.indices().await.unwrap();
    assert_eq!(list.indices.len(), MAX_INDICES);
    assert!(list.truncated);
    assert_eq!(list.indices[0].index, "logs-00000");
    server.await.unwrap();
}

#[tokio::test]
async fn cancelling_a_request_allows_an_immediate_retry_on_the_same_client() {
    let listener = TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
    let config = ElasticsearchConfig {
        endpoint: format!("http://{}", listener.local_addr().unwrap()),
        ..Default::default()
    };
    let (started, ready) = tokio::sync::oneshot::channel();
    let server = tokio::spawn(async move {
        for (i, body) in [INFO, ""].iter().enumerate() {
            let (mut socket, _) = listener.accept().await.unwrap();
            read_request(&mut socket).await;
            if i == 1 {
                // The worker must release this stream when its future is dropped.
                let _ = started.send(());
                let mut byte = [0];
                assert_eq!(
                    tokio::time::timeout(Duration::from_secs(2), socket.read(&mut byte))
                        .await
                        .unwrap()
                        .unwrap(),
                    0
                );
                break;
            }
            socket
                .write_all(
                    format!(
                        "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                        body.len()
                    )
                    .as_bytes(),
                )
                .await
                .unwrap();
        }
        let (mut socket, _) = listener.accept().await.unwrap();
        read_request(&mut socket).await;
        socket
            .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\n{}")
            .await
            .unwrap();
    });
    let client = ElasticsearchClient::connect(config, Secret::new(String::new()), None)
        .await
        .unwrap();
    let mut pending = Box::pin(client.request(Request::Mapping(None)));
    tokio::select! {
        _ = ready => {},
        _ = &mut pending => panic!("request should still be waiting"),
    }
    drop(pending);
    let response = tokio::time::timeout(
        Duration::from_secs(2),
        client.request(Request::Mapping(None)),
    )
    .await
    .unwrap()
    .unwrap();
    assert_eq!(response.status, 200);
    server.await.unwrap();
}

#[tokio::test]
async fn timeout_applies_while_reading_the_body() {
    let listener = TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
    let config = ElasticsearchConfig {
        endpoint: format!("http://{}", listener.local_addr().unwrap()),
        query_timeout_secs: 1,
        ..Default::default()
    };
    let server = tokio::spawn(async move {
        let (mut socket, _) = listener.accept().await.unwrap();
        read_request(&mut socket).await;
        socket
            .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 100\r\n\r\n{")
            .await
            .unwrap();
        let mut byte = [0];
        let _ = socket.read(&mut byte).await;
    });
    let result = tokio::time::timeout(
        Duration::from_secs(3),
        ElasticsearchClient::connect(config, Secret::new(String::new()), None),
    )
    .await
    .unwrap();
    assert!(matches!(result, Err(ElasticsearchError::Timeout)));
    server.await.unwrap();
}

#[tokio::test]
async fn redirects_are_not_followed_with_credentials() {
    let destination = TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
    let listener = TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
    let config = ElasticsearchConfig {
        endpoint: format!("http://{}", listener.local_addr().unwrap()),
        auth: ElasticsearchAuth::ApiKey,
        ..Default::default()
    };
    let address = destination.local_addr().unwrap();
    let server = tokio::spawn(async move {
        let (mut socket, _) = listener.accept().await.unwrap();
        read_request(&mut socket).await;
        socket.write_all(format!("HTTP/1.1 302 Found\r\nLocation: http://{address}/\r\nContent-Length: 2\r\nConnection: close\r\n\r\n{{}}").as_bytes()).await.unwrap();
    });
    assert!(
        ElasticsearchClient::connect(config, Secret::new("dGVzdDprZXk=".into()), None)
            .await
            .is_err()
    );
    server.await.unwrap();
    assert!(
        tokio::time::timeout(Duration::from_millis(100), destination.accept())
            .await
            .is_err()
    );
}

#[test]
fn formatting_cannot_expand_a_small_response_without_bound() {
    let value = serde_json::json!({"small":[1,2,3]});
    assert_eq!(
        serde_json::from_str::<Value>(&format_json(&value).unwrap()).unwrap(),
        value
    );
    let value = serde_json::json!({"large":"x".repeat(MAX_RESPONSE_BYTES)});
    assert!(matches!(
        format_json(&value),
        Err(ElasticsearchError::TooLarge)
    ));
}

async fn mock(
    responses: Vec<(u16, String)>,
) -> (
    ElasticsearchConfig,
    tokio::task::JoinHandle<Vec<(String, String)>>,
) {
    let listener = TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
    let config = ElasticsearchConfig {
        endpoint: format!("http://{}/elastic", listener.local_addr().unwrap()),
        timeout_secs: 2,
        query_timeout_secs: 2,
        ..Default::default()
    };
    let task = tokio::spawn(async move {
        let mut requests = Vec::new();
        for (status, body) in responses {
            let (mut socket, _) = tokio::time::timeout(Duration::from_secs(3), listener.accept())
                .await
                .unwrap()
                .unwrap();
            requests.push(read_request(&mut socket).await);
            let reply = format!(
                "HTTP/1.1 {status} fixture\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            );
            let _ = socket.write_all(reply.as_bytes()).await;
        }
        requests
    });
    (config, task)
}

#[tokio::test]
async fn authenticated_requests_list_indices_read_mappings_and_execute_json_queries() {
    for auth in [
        ElasticsearchAuth::None,
        ElasticsearchAuth::Basic,
        ElasticsearchAuth::ApiKey,
    ] {
        let (mut config,server)=mock(vec![
            (200,INFO.into()),
            (200,r#"{"indices":[{"name":"logs-b","attributes":["open"]},{"name":"logs-a","attributes":["closed"]}]}"#.into()),
            (200,r#"{"logs-a":{"mappings":{"properties":{"message":{"type":"text"}}}}}"#.into()),
            (200,r#"{"hits":{"total":{"value":1},"hits":[{"_source":{"message":"中文"}}]}}"#.into()),
        ]).await;
        config.auth = auth;
        config.username = "user".into();
        let client =
            ElasticsearchClient::connect(config, Secret::new("a2V5OnNlY3JldA==".into()), None)
                .await
                .unwrap();
        let indices = client.indices().await.unwrap();
        assert_eq!(
            indices
                .indices
                .iter()
                .map(|index| index.index.as_str())
                .collect::<Vec<_>>(),
            ["logs-a", "logs-b"]
        );
        let mapping = client
            .request(Request::Mapping(Some("logs-a".into())))
            .await
            .unwrap();
        assert_eq!(
            mapping.json["logs-a"]["mappings"]["properties"]["message"]["type"],
            "text"
        );
        let response = client
            .request(Request::Search {
                index: Some("logs-a".into()),
                body: DEFAULT_QUERY.into(),
            })
            .await
            .unwrap();
        assert_eq!(
            response.json["hits"]["hits"][0]["_source"]["message"],
            "中文"
        );
        client.close();
        let requests = server.await.unwrap();
        assert!(requests[0].0.starts_with("GET /elastic/ HTTP/1.1"));
        assert!(requests[1].0.starts_with("GET /elastic/_resolve/index/*?"));
        assert!(requests[2].0.starts_with("GET /elastic/logs-a/_mapping "));
        assert!(requests[3].0.starts_with("POST /elastic/logs-a/_search "));
        assert_eq!(requests[3].1, DEFAULT_QUERY);
        for (headers, _) in requests {
            let lower = headers.to_lowercase();
            match auth {
                ElasticsearchAuth::None => assert!(!lower.contains("authorization:")),
                ElasticsearchAuth::Basic => assert!(lower.contains("authorization: basic ")),
                ElasticsearchAuth::ApiKey => assert!(headers.contains("ApiKey a2V5OnNlY3JldA==")),
            }
        }
    }
}

#[tokio::test]
async fn index_discovery_uses_the_api_available_in_the_server_version() {
    for (version, resolve) in [
        ("7.4.2", false),
        ("7.8.1", false),
        ("7.9.0", true),
        ("7.9.0-SNAPSHOT", true),
        ("8.19.0", true),
        ("9.1.0", true),
        ("unrecognized", false),
    ] {
        let info = serde_json::json!({"cluster_name":"fixture","version":{"number":version}});
        let indices = if resolve {
            serde_json::json!({"indices":[{"name":"logs-b","attributes":["open"]},{"name":"logs-a","attributes":["closed"]}]})
        } else {
            serde_json::json!({"logs-b":{"settings":{"index":{"uuid":"fixture-b"}}},"logs-a":{"settings":{"index":{"uuid":"fixture-a"}}}})
        };
        let (mut config, server) = mock(vec![
            (200, info.to_string()),
            (200, indices.to_string()),
            (200, indices.to_string()),
            (200, r#"{"logs-b":{"mappings":{"properties":{}}}}"#.into()),
            (200, r#"{"hits":{"hits":[]}}"#.into()),
        ])
        .await;
        config.auth = ElasticsearchAuth::Basic;
        config.username = "operator".into();
        let client =
            ElasticsearchClient::connect(config, Secret::new("test-password".into()), None)
                .await
                .unwrap();
        // Refreshes and cloned clients must retain the negotiated API choice.
        for current in [&client, &client.clone()] {
            let list = current.indices().await.unwrap();
            assert_eq!(
                list.indices
                    .iter()
                    .map(|index| index.index.as_str())
                    .collect::<Vec<_>>(),
                ["logs-a", "logs-b"]
            );
            assert!(!list.truncated);
            if !resolve {
                assert!(list.indices.iter().all(|index| index.attributes.is_empty()));
            }
        }
        assert_eq!(
            client
                .request(Request::Mapping(Some("logs-b".into())))
                .await
                .unwrap()
                .status,
            200
        );
        assert_eq!(
            client
                .request(Request::Search {
                    index: Some("logs-b".into()),
                    body: DEFAULT_QUERY.into()
                })
                .await
                .unwrap()
                .status,
            200
        );
        let requests = server.await.unwrap();
        for (headers, _) in &requests[1..3] {
            let path = headers
                .lines()
                .next()
                .unwrap()
                .split_whitespace()
                .nth(1)
                .unwrap();
            let url = url::Url::parse(&format!("http://example.invalid{path}")).unwrap();
            assert_eq!(
                url.path(),
                if resolve {
                    "/elastic/_resolve/index/*"
                } else {
                    "/elastic/_settings"
                },
                "{version}"
            );
            assert!(
                url.query_pairs()
                    .any(|(key, value)| key == "expand_wildcards" && value == "all")
            );
            assert!(url.query_pairs().any(|(key, value)| key == "filter_path"
                && value
                    == if resolve {
                        "indices.name,indices.attributes"
                    } else {
                        "*.settings.index.uuid"
                    }));
            assert!(
                headers
                    .to_ascii_lowercase()
                    .contains("authorization: basic ")
            );
        }
        assert!(requests[3].0.starts_with("GET /elastic/logs-b/_mapping "));
        assert!(requests[4].0.starts_with("POST /elastic/logs-b/_search "));
    }
}

#[tokio::test]
async fn legacy_index_discovery_keeps_empty_lists_limits_and_errors_distinct() {
    let indices: serde_json::Map<String, Value> = (0..MAX_INDICES + 3)
        .map(|index| {
            (
                format!("logs-{index:05}"),
                serde_json::json!({"settings":{"index":{"uuid":"fixture"}}}),
            )
        })
        .collect();
    let (config, server) = mock(vec![
        (
            200,
            r#"{"cluster_name":"fixture","version":{"number":"7.4.2"}}"#.into(),
        ),
        (200, "{}".into()),
        (200, Value::Object(indices).to_string()),
        (200, r#"{"logs":{"settings":{}}}"#.into()),
        (
            503,
            r#"{"error":"private upstream detail","status":503}"#.into(),
        ),
        (403, r#"{"error":"access denied","status":403}"#.into()),
    ])
    .await;
    let client = ElasticsearchClient::connect(config, Secret::new(String::new()), None)
        .await
        .unwrap();
    let empty = client.indices().await.unwrap();
    assert!(empty.indices.is_empty());
    assert!(!empty.truncated);
    let list = client.indices().await.unwrap();
    assert_eq!(list.indices.len(), MAX_INDICES);
    assert!(list.truncated);
    assert_eq!(list.indices[0].index, "logs-00000");
    assert!(matches!(
        client.indices().await,
        Err(ElasticsearchError::Protocol)
    ));
    let error = match client.indices().await {
        Err(error) => error,
        Ok(_) => panic!("HTTP failure was treated as an index list"),
    };
    assert!(matches!(error, ElasticsearchError::HttpStatus(503)));
    assert!(error.to_string().contains("503"));
    assert!(!error.to_string().contains("private upstream detail"));
    assert!(matches!(
        client.indices().await,
        Err(ElasticsearchError::Unauthorized)
    ));
    assert_eq!(server.await.unwrap().len(), 6);
}

#[tokio::test]
async fn http_errors_are_displayed_as_json_and_invalid_queries_are_not_sent() {
    let (config, server) = mock(vec![
        (200, INFO.into()),
        (
            400,
            r#"{"error":{"type":"parsing_exception"},"status":400}"#.into(),
        ),
    ])
    .await;
    let client = ElasticsearchClient::connect(config, Secret::new(String::new()), None)
        .await
        .unwrap();
    assert!(matches!(
        client
            .request(Request::Search {
                index: None,
                body: "[invalid]".into()
            })
            .await,
        Err(ElasticsearchError::InvalidQuery)
    ));
    let response = client
        .request(Request::Search {
            index: None,
            body: "{}".into(),
        })
        .await
        .unwrap();
    assert_eq!(response.status, 400);
    assert_eq!(response.json["error"]["type"], "parsing_exception");
    assert_eq!(server.await.unwrap().len(), 2);
}

#[tokio::test]
async fn rejected_credentials_malformed_json_and_large_responses_fail_cleanly() {
    for (status, body, expected) in [
        (401, "private authentication detail".into(), "auth"),
        (200, "not json".into(), "json"),
        (200, "x".repeat(MAX_RESPONSE_BYTES + 1), "size"),
    ] {
        let (config, server) = mock(vec![(status, body)]).await;
        let error =
            match ElasticsearchClient::connect(config, Secret::new(String::new()), None).await {
                Ok(_) => panic!("invalid response accepted"),
                Err(error) => error,
            };
        assert!(matches!(
            (expected, error),
            ("auth", ElasticsearchError::Unauthorized)
                | ("json", ElasticsearchError::Protocol)
                | ("size", ElasticsearchError::TooLarge)
        ));
        assert!(!error.to_string().contains("private authentication detail"));
        server.await.unwrap();
    }
}

#[tokio::test]
async fn closing_a_client_interrupts_an_inflight_request() {
    let listener = TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
    let config = ElasticsearchConfig {
        endpoint: format!("http://{}", listener.local_addr().unwrap()),
        ..Default::default()
    };
    let (started, ready) = tokio::sync::oneshot::channel();
    let server = tokio::spawn(async move {
        let (mut socket, _) = listener.accept().await.unwrap();
        read_request(&mut socket).await;
        socket
            .write_all(
                format!(
                    "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{INFO}",
                    INFO.len()
                )
                .as_bytes(),
            )
            .await
            .unwrap();
        drop(socket);
        let (mut socket, _) = listener.accept().await.unwrap();
        read_request(&mut socket).await;
        started.send(()).unwrap();
        let mut byte = [0];
        let _ = socket.read(&mut byte).await;
    });
    let client = ElasticsearchClient::connect(config, Secret::new(String::new()), None)
        .await
        .unwrap();
    let pending = tokio::spawn({
        let client = client.clone();
        async move { client.request(Request::Mapping(None)).await }
    });
    ready.await.unwrap();
    client.close();
    assert!(matches!(
        tokio::time::timeout(Duration::from_secs(1), pending)
            .await
            .unwrap()
            .unwrap(),
        Err(ElasticsearchError::Cancelled)
    ));
    tokio::time::timeout(Duration::from_secs(1), server)
        .await
        .unwrap()
        .unwrap();
}

#[test]
fn query_validation_accepts_only_bounded_json_objects() {
    assert!(validate_query(DEFAULT_QUERY).is_ok());
    for body in ["", "[]", "null", "{", "42"] {
        assert!(validate_query(body).is_err());
    }
    assert!(
        validate_query(&format!(
            "{{\"value\":\"{}\"}}",
            "x".repeat(MAX_QUERY_BYTES)
        ))
        .is_err()
    );
}

#[test]
fn requests_preserve_endpoint_prefix_and_encode_index_names() {
    let config = ElasticsearchConfig {
        endpoint: "https://example.test/elastic".into(),
        ..Default::default()
    };
    let url = request_url(&config, &Request::Mapping(Some("logs-2026".into()))).unwrap();
    assert_eq!(
        url.as_str(),
        "https://example.test/elastic/logs-2026/_mapping"
    );
    for index in ["..", "../other", "a/b", "a?x", "a%2fb"] {
        assert!(request_url(&config, &Request::Mapping(Some(index.into()))).is_err());
    }
    assert_eq!(
        request_url(
            &config,
            &Request::Search {
                index: None,
                body: DEFAULT_QUERY.into()
            }
        )
        .unwrap()
        .path(),
        "/elastic/_search"
    );
}
