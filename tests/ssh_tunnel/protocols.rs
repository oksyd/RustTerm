use super::*;
use rustterm::{
    config::{DatabaseConfig, RdpConfig, SqlConfig},
    database::{DatabaseClient, DatabaseError},
    rdp::{RdpClient, RdpError, RdpEvent},
    session::{Command, Session, SessionState, TelnetOptions},
};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

async fn target(fixture: &Fixture, host: &str, port: u32) -> TcpListener {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    fixture
        .events
        .routes
        .lock()
        .unwrap()
        .insert((host.into(), port), listener.local_addr().unwrap());
    listener
}
fn ssh(fixture: &Fixture, directory: &Path) -> SshOptions {
    let path = directory.join("hosts.toml");
    fixture.trust(&path, &fixture.host_key);
    fixture.ssh(&path, SshAuth::Password("ssh-secret".into()))
}

#[tokio::test]
async fn telnet_routes_session_and_probe_without_local_dns_or_a_remote_shell() {
    tokio::time::timeout(Duration::from_secs(8), async {
        let fixture = Fixture::new(None, false).await;
        let directory = private_directory();
        let listener = target(&fixture, "telnet.internal.invalid", 23).await;
        let options = TelnetOptions {
            host: "telnet.internal.invalid".into(),
            port: 23,
            timeout_secs: 2,
        };
        let session = Session::spawn_telnet_with_ssh(
            options,
            Some(ssh(&fixture, directory.path())),
            80,
            24,
            100,
        )
        .unwrap();
        let (mut socket, _) = listener.accept().await.unwrap();
        while session.state() != SessionState::Connected {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        session
            .send(Command::Input(b"telnet-via-ssh\r\n".to_vec()))
            .unwrap();
        let mut received = Vec::new();
        while !received
            .windows(b"telnet-via-ssh\r".len())
            .any(|b| b == b"telnet-via-ssh\r")
        {
            let mut bytes = [0; 256];
            let count = socket.read(&mut bytes).await.unwrap();
            assert!(count > 0);
            received.extend_from_slice(&bytes[..count]);
        }
        session.close();
        drop(socket);
        let (trust, _) = mpsc::channel(1);
        let (certificates, _) = mpsc::channel(1);
        let profile = rustterm::config::Profile::telnet(
            "fixture",
            rustterm::config::TelnetConfig {
                host: "telnet.internal.invalid".into(),
                port: 23,
                ssh_profile: Some(Uuid::new_v4()),
                ..Default::default()
            },
        );
        let request = rustterm::connection_probe::ProbeRequest {
            profile,
            secret: Secret::new(String::new()),
            sentinel_secret: None,
            ssh: Some(ssh(&fixture, directory.path())),
        };
        let outcome = rustterm::connection_probe::test(
            request,
            trust,
            certificates,
            tokio_util::sync::CancellationToken::new(),
        )
        .await
        .unwrap();
        assert_eq!(
            outcome.kind,
            rustterm::connection_probe::ProbeKind::TelnetReachable
        );
        assert_eq!(fixture.events.destinations.lock().unwrap().len(), 2);
        assert_eq!(fixture.events.shell_channels.load(Ordering::SeqCst), 0);
    })
    .await
    .unwrap();
}

#[tokio::test]
async fn rdp_waits_for_ssh_trust_and_reports_the_tunneled_servers_negotiation_error() {
    tokio::time::timeout(Duration::from_secs(8), async {
        let fixture = Fixture::new(None, false).await;
        let directory = private_directory();
        let listener = target(&fixture, "rdp.internal.invalid", 3389).await;
        let ssh = fixture.ssh(
            &directory.path().join("hosts.toml"),
            SshAuth::Password("ssh-secret".into()),
        );
        let mut handle = RdpClient::connect_with_ssh(
            RdpConfig {
                host: "rdp.internal.invalid".into(),
                username: "user".into(),
                ssh_profile: Some(Uuid::new_v4()),
                ..Default::default()
            },
            Secret::new("rdp-secret".into()),
            None,
            Some(ssh),
        )
        .unwrap();
        let challenge = loop {
            if let RdpEvent::SshHostKey(challenge) = handle.events.recv().await.unwrap() {
                break challenge;
            }
        };
        assert_eq!(fixture.events.authentications.load(Ordering::SeqCst), 0);
        challenge.reply.send(true).unwrap();
        let (mut stream, _) = listener.accept().await.unwrap();
        let mut header = [0; 4];
        stream.read_exact(&mut header).await.unwrap();
        let mut request = vec![0; u16::from_be_bytes([header[2], header[3]]) as usize - 4];
        stream.read_exact(&mut request).await.unwrap();
        stream
            .write_all(&[3, 0, 0, 19, 14, 0xd0, 0, 0, 0, 0, 0, 3, 0, 8, 0, 1, 0, 0, 0])
            .await
            .unwrap();
        loop {
            if let RdpEvent::Failed(error) = handle.events.recv().await.unwrap() {
                assert_eq!(error, RdpError::ServerNegotiation { code: 1 });
                break;
            }
        }
        assert_eq!(
            *fixture.events.destinations.lock().unwrap(),
            [("rdp.internal.invalid".into(), 3389)]
        );
        assert_eq!(fixture.events.shell_channels.load(Ordering::SeqCst), 0);
        handle.close();
    })
    .await
    .unwrap();
}

async fn mysql_packet(stream: &mut tokio::net::TcpStream, sequence: u8, body: &[u8]) {
    let mut packet = (body.len() as u32).to_le_bytes().to_vec();
    packet[3] = sequence;
    packet.extend(body);
    stream.write_all(&packet).await.unwrap();
}

#[tokio::test]
async fn rdp_certificate_retry_keeps_the_original_target_and_ssh_route() {
    tokio::time::timeout(Duration::from_secs(10), async {
        use rustls::pki_types::{CertificateDer, PrivateKeyDer, pem::PemObject};
        let cert =
            CertificateDer::from_pem_slice(include_bytes!("../fixtures/elasticsearch/server.pem"))
                .unwrap();
        let key = PrivateKeyDer::from_pem_slice(include_bytes!(
            "../fixtures/elasticsearch/server-key.pem"
        ))
        .unwrap();
        let tls = tokio_rustls::TlsAcceptor::from(Arc::new(
            rustls::ServerConfig::builder()
                .with_no_client_auth()
                .with_single_cert(vec![cert], key)
                .unwrap(),
        ));
        let fixture = Fixture::new(None, false).await;
        let directory = private_directory();
        let listener = target(&fixture, "desktop.internal.invalid", 3389).await;
        let server = tokio::spawn(async move {
            for trusted in [false, true] {
                let (mut stream, _) = listener.accept().await.unwrap();
                let mut header = [0; 4];
                stream.read_exact(&mut header).await.unwrap();
                let mut request = vec![0; u16::from_be_bytes([header[2], header[3]]) as usize - 4];
                stream.read_exact(&mut request).await.unwrap();
                stream
                    .write_all(&[3, 0, 0, 19, 14, 0xd0, 0, 0, 0, 0, 0, 2, 0, 8, 0, 2, 0, 0, 0])
                    .await
                    .unwrap();
                assert_eq!(tls.accept(stream).await.is_ok(), trusted);
            }
        });
        let mut handle = RdpClient::connect_with_ssh(
            RdpConfig {
                host: "desktop.internal.invalid".into(),
                username: "operator".into(),
                ssh_profile: Some(Uuid::new_v4()),
                ..Default::default()
            },
            Secret::new("secret".into()),
            None,
            Some(ssh(&fixture, directory.path())),
        )
        .unwrap();
        let mut challenged = false;
        let mut accepted = false;
        loop {
            match handle.events.recv().await.unwrap() {
                RdpEvent::CertificateChallenge {
                    host,
                    port,
                    previous_fingerprint,
                    reply,
                    ..
                } => {
                    assert!(!challenged);
                    assert_eq!((host.as_str(), port), ("desktop.internal.invalid", 3389));
                    assert_eq!(previous_fingerprint, None);
                    challenged = true;
                    reply
                        .send(rustterm::rdp::CertificateDecision::Once)
                        .unwrap();
                }
                RdpEvent::CertificateAccepted { save, .. } => {
                    assert!(!save);
                    accepted = true;
                }
                RdpEvent::Failed(_) => break, // Fixture stops after TLS, before NLA.
                _ => {}
            }
        }
        assert!(challenged && accepted);
        server.await.unwrap();
        assert_eq!(
            *fixture.events.destinations.lock().unwrap(),
            [
                ("desktop.internal.invalid".into(), 3389),
                ("desktop.internal.invalid".into(), 3389)
            ]
        );
        handle.close();
    })
    .await
    .unwrap();
}

#[tokio::test]
async fn remote_protocols_never_fall_back_to_direct_when_the_gateway_rejects_forwarding() {
    tokio::time::timeout(Duration::from_secs(10), async {
        let fixture = Fixture::new(None, true).await;
        let directory = private_directory();
        let decoy = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = decoy.local_addr().unwrap().port();
        let mut rdp = RdpClient::connect_with_ssh(
            RdpConfig {
                host: "127.0.0.1".into(),
                username: "fixture".into(),
                port,
                ssh_profile: Some(Uuid::new_v4()),
                ..Default::default()
            },
            Secret::new("secret".into()),
            None,
            Some(ssh(&fixture, directory.path())),
        )
        .unwrap();
        loop {
            if let RdpEvent::Failed(error) = rdp.events.recv().await.unwrap() {
                assert!(matches!(error, RdpError::Ssh { .. }));
                break;
            }
        }
        rdp.close();
        let telnet = Session::spawn_telnet_with_ssh(
            TelnetOptions {
                host: "127.0.0.1".into(),
                port,
                timeout_secs: 2,
            },
            Some(ssh(&fixture, directory.path())),
            80,
            24,
            100,
        )
        .unwrap();
        while !matches!(telnet.state(), SessionState::Failed { .. }) {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        telnet.close();
        for mysql in [true, false] {
            let config = SqlConfig {
                host: "127.0.0.1".into(),
                port,
                tls: false,
                ssh_profile: Some(Uuid::new_v4()),
                ..if mysql {
                    SqlConfig::mysql()
                } else {
                    SqlConfig::postgres()
                }
            };
            let config = if mysql {
                DatabaseConfig::Mysql(config)
            } else {
                DatabaseConfig::Postgres(config)
            };
            let (trust, _) = mpsc::channel(1);
            assert!(
                DatabaseClient::connect(
                    config,
                    Secret::new("secret".into()),
                    Some((ssh(&fixture, directory.path()), trust))
                )
                .await
                .is_err()
            );
        }
        assert_eq!(fixture.events.destinations.lock().unwrap().len(), 4);
        assert!(
            tokio::time::timeout(Duration::from_millis(100), decoy.accept())
                .await
                .is_err()
        );
    })
    .await
    .unwrap();
}
async fn mysql_read(stream: &mut tokio::net::TcpStream) -> Vec<u8> {
    let query = super::sql_wire::Wire::new(stream, true).query().await;
    let mut packet = vec![3];
    packet.extend(query.into_bytes());
    packet
}

async fn mysql_start(stream: &mut tokio::net::TcpStream) {
    let mut wire = super::sql_wire::Wire::new(stream, true);
    wire.greeting(false).await;
    wire.login().await;
    assert_eq!(
        wire.read_packet().await,
        b"\x03SELECT @@max_allowed_packet,@@wait_timeout"
    );
    wire.rows(
        &[("@@max_allowed_packet", false), ("@@wait_timeout", false)],
        &[vec![Some(b"67108864"), Some(b"28800")]],
        false,
    )
    .await;
    assert_eq!(wire.read_packet().await, b"\x03SET NAMES utf8mb4");
    wire.complete(0, false).await;
}

async fn pg_message(stream: &mut tokio::net::TcpStream, tag: u8, body: &[u8]) {
    let mut packet = vec![tag];
    packet.extend((body.len() as u32 + 4).to_be_bytes());
    packet.extend(body);
    stream.write_all(&packet).await.unwrap();
}

async fn pg_start(stream: &mut tokio::net::TcpStream) {
    let len = stream.read_u32().await.unwrap();
    let mut startup = vec![0; len as usize - 4];
    stream.read_exact(&mut startup).await.unwrap();
    pg_message(stream, b'R', &0u32.to_be_bytes()).await;
    pg_message(stream, b'K', &[0, 0, 0, 1, 0, 0, 0, 2]).await;
    pg_message(stream, b'Z', b"I").await;
}
async fn pg_read(stream: &mut tokio::net::TcpStream) -> Vec<u8> {
    assert_eq!(stream.read_u8().await.unwrap(), b'Q');
    let len = stream.read_u32().await.unwrap();
    let mut query = vec![0; len as usize - 4];
    stream.read_exact(&mut query).await.unwrap();
    query
}

#[tokio::test]
async fn sql_cancellation_uses_the_same_ssh_gateway_as_the_running_query() {
    tokio::time::timeout(Duration::from_secs(10), async {
        for mysql in [true, false] {
            let fixture = Fixture::new(None, false).await;
            let directory = private_directory();
            let host = "database.internal.invalid";
            let mut config = if mysql {
                SqlConfig::mysql()
            } else {
                SqlConfig::postgres()
            };
            let listener = target(&fixture, host, config.port.into()).await;
            let (started, running) = tokio::sync::oneshot::channel();
            let server = tokio::spawn(async move {
                let (mut query, _) = listener.accept().await.unwrap();
                if mysql {
                    mysql_start(&mut query).await;
                    mysql_read(&mut query).await;
                } else {
                    pg_start(&mut query).await;
                    pg_read(&mut query).await;
                }
                started.send(()).unwrap();
                let (mut control, _) = listener.accept().await.unwrap();
                let cancellation = if mysql {
                    mysql_start(&mut control).await;
                    mysql_read(&mut control).await
                } else {
                    pg_start(&mut control).await;
                    pg_read(&mut control).await
                };
                let sql = String::from_utf8(cancellation).unwrap();
                assert!(sql.contains("/* rustterm:"));
                assert!(sql.contains(if mysql {
                    "information_schema.PROCESSLIST"
                } else {
                    "pg_catalog.pg_cancel_backend"
                }));
                // A permission error still proves that the cancellation reaches
                // the original database through a second channel, not direct TCP.
                if mysql {
                    mysql_packet(&mut control, 1, b"\xff\x12\x04#42000fixture denied").await;
                } else {
                    pg_message(&mut control, b'E', b"SERROR\0C42501\0Mfixture denied\0\0").await;
                    pg_message(&mut control, b'Z', b"I").await;
                }
                let mut end = [0; 1];
                let _ = query.read(&mut end).await;
            });
            config.host = host.into();
            config.tls = false;
            config.ssh_profile = Some(Uuid::new_v4());
            let config = if mysql {
                DatabaseConfig::Mysql(config)
            } else {
                DatabaseConfig::Postgres(config)
            };
            let (trust, _) = mpsc::channel(1);
            let client = DatabaseClient::connect(
                config,
                Secret::new(String::new()),
                Some((ssh(&fixture, directory.path()), trust)),
            )
            .await
            .unwrap();
            let query_client = client.clone();
            let query =
                tokio::spawn(async move { query_client.execute("long query".into(), 10).await });
            running.await.unwrap();
            client.cancel();
            assert_eq!(
                query.await.unwrap().unwrap_err(),
                DatabaseError::OutcomeUnknown
            );
            client.close();
            server.await.unwrap();
            assert_eq!(fixture.events.destinations.lock().unwrap().len(), 2);
        }
    })
    .await
    .unwrap();
}

#[tokio::test]
async fn sql_sessions_and_mysql_monitor_use_ssh_channels_on_every_platform() {
    tokio::time::timeout(Duration::from_secs(10), async {
        for mysql in [true, false] {
            let fixture = Fixture::new(None, false).await;
            let directory = private_directory();
            let host = if mysql {
                "mysql.internal.invalid"
            } else {
                "postgres.internal.invalid"
            };
            let port = if mysql { 3306 } else { 5432 };
            let listener = target(&fixture, host, port).await;
            let server = tokio::spawn(async move {
                let (mut stream, _) = listener.accept().await.unwrap();
                if mysql {
                    mysql_start(&mut stream).await;
                    assert!(mysql_read(&mut stream).await.ends_with(b"fixture"));
                    mysql_packet(&mut stream, 1, &[0, 1, 0, 2, 0, 0, 0]).await;
                    let (mut monitor, _) = listener.accept().await.unwrap();
                    mysql_start(&mut monitor).await;
                    assert!(
                        mysql_read(&mut monitor)
                            .await
                            .ends_with(b"SHOW FULL PROCESSLIST")
                    );
                    mysql_packet(&mut monitor, 1, b"\xff\x12\x04#42000fixture denied").await;
                } else {
                    let len = stream.read_u32().await.unwrap();
                    let mut startup = vec![0; len as usize - 4];
                    stream.read_exact(&mut startup).await.unwrap();
                    pg_message(&mut stream, b'R', &0u32.to_be_bytes()).await;
                    pg_message(&mut stream, b'K', &[0, 0, 0, 1, 0, 0, 0, 2]).await;
                    pg_message(&mut stream, b'Z', b"I").await;
                    assert_eq!(stream.read_u8().await.unwrap(), b'Q');
                    let len = stream.read_u32().await.unwrap();
                    let mut query = vec![0; len as usize - 4];
                    stream.read_exact(&mut query).await.unwrap();
                    assert!(query.ends_with(b"fixture\0"));
                    pg_message(&mut stream, b'C', b"UPDATE 1\0").await;
                    pg_message(&mut stream, b'Z', b"I").await;
                    let mut end = [0; 1];
                    let _ = stream.read(&mut end).await;
                }
            });
            let mut config = if mysql {
                SqlConfig::mysql()
            } else {
                SqlConfig::postgres()
            };
            config.host = host.into();
            config.tls = false;
            config.ssh_profile = Some(Uuid::new_v4());
            let config = if mysql {
                DatabaseConfig::Mysql(config)
            } else {
                DatabaseConfig::Postgres(config)
            };
            let (trust, _) = mpsc::channel(1);
            let client = DatabaseClient::connect(
                config,
                Secret::new("database-secret".into()),
                Some((ssh(&fixture, directory.path()), trust)),
            )
            .await
            .unwrap();
            assert_eq!(
                client.execute("fixture".into(), 10).await.unwrap().results[0].affected_rows,
                1
            );
            if mysql {
                assert!(matches!(
                    client.mysql_monitor().unwrap().snapshot().await,
                    Err(DatabaseError::Server { .. })
                ));
            }
            client.close();
            server.await.unwrap();
            assert_eq!(
                fixture.events.destinations.lock().unwrap().len(),
                if mysql { 2 } else { 1 }
            );
            assert!(
                fixture
                    .events
                    .destinations
                    .lock()
                    .unwrap()
                    .iter()
                    .all(|(name, p)| name == host && *p == port)
            );
            assert_eq!(fixture.events.shell_channels.load(Ordering::SeqCst), 0);
        }
    })
    .await
    .unwrap();
}
