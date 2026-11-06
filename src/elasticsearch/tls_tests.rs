use super::*;
use rustls::pki_types::{CertificateDer, PrivateKeyDer, pem::PemObject};
use tokio::{io::AsyncWriteExt, net::TcpListener};

// These public test-only certificates cover 127.0.0.1, deliberately not localhost.
const CA: &[u8] = include_bytes!("../../tests/fixtures/elasticsearch/ca.pem");
const CERT: &[u8] = include_bytes!("../../tests/fixtures/elasticsearch/server.pem");
const KEY: &[u8] = include_bytes!("../../tests/fixtures/elasticsearch/server-key.pem");

#[tokio::test]
async fn https_verifies_the_ca_and_hostname_before_sending_credentials() {
    for (trust_ca, hostname, accepted) in [
        (true, "127.0.0.1", true),
        (false, "127.0.0.1", false),
        (true, "localhost", false),
    ] {
        let listener = TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
        let mut server = rustls::ServerConfig::builder_with_provider(Arc::new(
            rustls::crypto::ring::default_provider(),
        ))
        .with_safe_default_protocol_versions()
        .unwrap()
        .with_no_client_auth()
        .with_single_cert(
            vec![CertificateDer::from_pem_slice(CERT).unwrap()],
            PrivateKeyDer::from_pem_slice(KEY).unwrap(),
        )
        .unwrap();
        server.alpn_protocols = vec![b"http/1.1".to_vec()];
        let acceptor = tokio_rustls::TlsAcceptor::from(Arc::new(server));
        let mut file = tempfile::NamedTempFile::new().unwrap();
        std::io::Write::write_all(&mut file, CA).unwrap();
        let config = ElasticsearchConfig {
            endpoint: format!(
                "https://{hostname}:{}",
                listener.local_addr().unwrap().port()
            ),
            auth: ElasticsearchAuth::ApiKey,
            ca_file: trust_ca.then(|| file.path().to_owned()),
            timeout_secs: 2,
            query_timeout_secs: 2,
            ..Default::default()
        };
        let task = tokio::spawn(async move {
            let (socket, _) = listener.accept().await.unwrap();
            let Ok(mut socket) = acceptor.accept(socket).await else {
                return None;
            };
            let (headers, _) = tests::read_request(&mut socket).await;
            let body = r#"{"cluster_name":"secure","version":{"number":"8.19.0"}}"#;
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
            Some(headers)
        });
        let result =
            ElasticsearchClient::connect(config, Secret::new("dGVzdDprZXk=".into()), None).await;
        assert_eq!(result.is_ok(), accepted);
        let headers = tokio::time::timeout(Duration::from_secs(3), task)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(headers.is_some(), accepted);
        if let Some(headers) = headers {
            assert!(headers.contains("ApiKey dGVzdDprZXk="));
        }
    }
}
