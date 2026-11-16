use super::*;
use crate::database::test_wire as wire;
use crate::{
    config::{Secret, SqlConfig},
    database::{mysql, postgres},
};
use rustls::pki_types::{CertificateDer, PrivateKeyDer, pem::PemObject};
use std::{
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::TcpStream,
};

fn tls() -> tokio_rustls::TlsAcceptor {
    let cert = CertificateDer::from_pem_slice(include_bytes!(
        "../../../tests/fixtures/elasticsearch/server.pem"
    ))
    .unwrap();
    let key = PrivateKeyDer::from_pem_slice(include_bytes!(
        "../../../tests/fixtures/elasticsearch/server-key.pem"
    ))
    .unwrap();
    let mut config = rustls::ServerConfig::builder()
        .with_no_client_auth()
        .with_single_cert(vec![cert], key)
        .unwrap();
    config.send_tls13_tickets = 0;
    tokio_rustls::TlsAcceptor::from(Arc::new(config))
}

#[tokio::test]
async fn sql_tls_keeps_target_identity_and_rejects_wrong_names_before_authentication() {
    tokio::time::timeout(Duration::from_secs(10), async {
        for mysql in [false, true] {
            for valid in [true, false] {
                let (client, server) = tokio::io::duplex(32);
                let forward = Forward::start(client, CancellationToken::new())
                    .await
                    .unwrap();
                let authenticated = Arc::new(AtomicBool::new(false));
                let observed = authenticated.clone();
                let remote = tokio::spawn(async move {
                    let mut wire = wire::Wire::new(server, mysql);
                    if mysql {
                        wire.greeting(true).await;
                        assert_eq!(wire.read_packet().await.len(), 32);
                    } else {
                        let mut request = [0; 8];
                        wire.stream.read_exact(&mut request).await.unwrap();
                        assert_eq!(request, [0, 0, 0, 8, 4, 210, 22, 47]);
                        wire.stream.write_all(b"S").await.unwrap();
                    }
                    let Ok(stream) = tls().accept(wire.stream).await else {
                        return;
                    };
                    let mut wire = wire::Wire::new(stream, mysql);
                    wire.login().await;
                    observed.store(true, Ordering::SeqCst);
                    assert_eq!(wire.query().await, "UPDATE fixture");
                    wire.complete(1, false).await;
                    let _ = wire.stream.read_u8().await;
                });
                let config = SqlConfig {
                    host: if valid { "127.0.0.1" } else { "wrong.invalid" }.into(),
                    tls: true,
                    ..SqlConfig::mysql()
                };
                let ca = include_bytes!("../../../tests/fixtures/elasticsearch/ca.pem").to_vec();
                let result = if mysql {
                    let mut options = mysql::Connection::options(
                        config,
                        Secret::new(String::new()),
                        None,
                        CancellationToken::new(),
                    )
                    .unwrap();
                    options.driver = mysql_async::OptsBuilder::from_opts(options.driver)
                        .ssl_opts(Some(
                            mysql_async::SslOpts::default()
                                .with_disable_built_in_roots(true)
                                .with_root_certs(vec![ca.into()]),
                        ))
                        .into();
                    async {
                        let mut connection = options.connect_forwarded(Some(forward)).await?;
                        assert_eq!(
                            connection.execute("UPDATE fixture", 10).await?[0].affected_rows,
                            1
                        );
                        connection.close().await;
                        Ok::<_, DatabaseError>(())
                    }
                    .await
                } else {
                    let mut options = postgres::Connection::options(
                        config,
                        Secret::new(String::new()),
                        None,
                        CancellationToken::new(),
                    )
                    .unwrap();
                    let mut roots = rustls::RootCertStore::empty();
                    roots
                        .add(CertificateDer::from_pem_slice(&ca).unwrap())
                        .unwrap();
                    options.tls = tokio_postgres_rustls::MakeRustlsConnect::new(
                        rustls::ClientConfig::builder()
                            .with_root_certificates(roots)
                            .with_no_client_auth(),
                    );
                    async {
                        let mut connection = options.connect_forwarded(Some(forward)).await?;
                        assert_eq!(
                            connection.execute("UPDATE fixture", 10).await?[0].affected_rows,
                            1
                        );
                        Ok::<_, DatabaseError>(())
                    }
                    .await
                };
                assert_eq!(
                    result,
                    if valid {
                        Ok(())
                    } else {
                        Err(DatabaseError::Tls)
                    },
                    "mysql={mysql}, valid={valid}"
                );
                remote.await.unwrap();
                assert_eq!(authenticated.load(Ordering::SeqCst), valid);
            }
        }
    })
    .await
    .unwrap();
}

#[tokio::test]
async fn forwarding_preserves_fragmentation_backpressure_and_half_close() {
    tokio::time::timeout(Duration::from_secs(5), async {
        let (client, mut remote) = tokio::io::duplex(7);
        let forward = Forward::start(client, CancellationToken::new())
            .await
            .unwrap();
        assert!(forward.address.ip().is_loopback());
        let mut socket = TcpStream::connect(forward.address).await.unwrap();
        let payload: Vec<_> = (0..100_000).map(|n| (n % 251) as u8).collect();
        let expected = payload.clone();
        let server = tokio::spawn(async move {
            let mut received = Vec::new();
            remote.read_to_end(&mut received).await.unwrap();
            assert_eq!(received, expected);
            remote.write_all(&received).await.unwrap();
            remote.shutdown().await.unwrap();
        });
        socket.write_all(&payload).await.unwrap();
        socket.shutdown().await.unwrap();
        let mut received = Vec::new();
        socket.read_to_end(&mut received).await.unwrap();
        assert_eq!(received, payload);
        server.await.unwrap();
        assert!(TcpStream::connect(forward.address).await.is_err());
    })
    .await
    .unwrap();
}

#[tokio::test]
async fn forwarding_releases_its_listener_and_stream_on_drop_and_cancellation() {
    for connected in [false, true] {
        for cancel in [false, true] {
            tokio::time::timeout(Duration::from_secs(5), async {
                let (client, mut remote) = tokio::io::duplex(8);
                let stop = CancellationToken::new();
                let mut forward = Forward::start(client, stop.clone()).await.unwrap();
                let address = forward.address;
                let socket = if connected {
                    let mut socket = TcpStream::connect(address).await.unwrap();
                    socket.write_all(b"ready").await.unwrap();
                    let mut received = [0; 5];
                    remote.read_exact(&mut received).await.unwrap();
                    assert_eq!(&received, b"ready");
                    Some(socket)
                } else {
                    assert!(TcpListener::bind(address).await.is_err());
                    None
                };
                if cancel {
                    stop.cancel();
                    (&mut forward.task).await.unwrap();
                } else {
                    drop(forward);
                }
                assert_eq!(remote.read(&mut [0; 1]).await.unwrap(), 0);
                if let Some(mut socket) = socket {
                    assert_eq!(socket.read(&mut [0; 1]).await.unwrap(), 0);
                } else {
                    // Rebinding checks release without a platform-dependent failed connection.
                    let _listener = TcpListener::bind(address).await.unwrap();
                }
            })
            .await
            .unwrap_or_else(|_| {
                panic!("forwarding cleanup timed out: connected={connected}, cancel={cancel}")
            });
        }
    }
}
