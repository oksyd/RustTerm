use rustterm::{
    config::{ElasticsearchConfig, Profile, Secret},
    connection_probe::{self, ProbeKind, ProbeRequest},
    elasticsearch::{ElasticsearchClient, Request},
};

/// Opt-in validation against an unauthenticated development cluster. The address
/// stays outside the repository and no document contents are fetched or printed.
#[tokio::test]
#[ignore = "requires RUSTTERM_ES_TEST_URL pointing to a development cluster"]
async fn live_cluster_supports_index_discovery_mapping_and_search() {
    let config = ElasticsearchConfig {
        endpoint: std::env::var("RUSTTERM_ES_TEST_URL").expect("set RUSTTERM_ES_TEST_URL"),
        ..Default::default()
    };
    let client = ElasticsearchClient::connect(config.clone(), Secret::new(String::new()), None)
        .await
        .unwrap();
    let indices = client.indices().await.unwrap();
    eprintln!(
        "Discovered {} indices (truncated: {})",
        indices.indices.len(),
        indices.truncated
    );
    if let Some(index) = indices.indices.first() {
        let mapping = client
            .request(Request::Mapping(Some(index.index.clone())))
            .await
            .unwrap();
        assert_eq!(mapping.status, 200);
        assert!(mapping.json.is_object());
    }
    let response = client
        .request(Request::Search {
            index: None,
            body: r#"{"size":0,"query":{"match_none":{}}}"#.into(),
        })
        .await
        .unwrap();
    assert_eq!(response.status, 200);
    assert_eq!(response.json["timed_out"], false);
    assert_eq!(response.json["_shards"]["failed"], 0);
    assert!(response.json["hits"]["hits"].as_array().unwrap().is_empty());
    let version = client.server_version().to_owned();
    client.close();
    let (hosts, _) = tokio::sync::mpsc::channel(1);
    let (certificates, _) = tokio::sync::mpsc::channel(1);
    let result = connection_probe::test(
        ProbeRequest {
            profile: Profile::elasticsearch("Elasticsearch", config),
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
    assert_eq!(result.kind, ProbeKind::ElasticsearchReady);
    let info = result.server.unwrap();
    assert_eq!(info.product, "Elasticsearch");
    assert_eq!(info.version.as_deref(), Some(version.as_str()));
    eprintln!("Connection test reports Elasticsearch {version}");
}
