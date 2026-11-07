use crate::i18n::messages as i18n_key;

use crate::{
    config::{Connection, Profile, Secret, SqlEngine},
    database::{DatabaseClient, DatabaseError, QueryOutput},
    rdp::{RdpError, RdpEvent},
    redis::{RedisClient, RedisError, RedisValue},
    session::{FailureKind, HostKeyChallenge, SshOptions},
};
use std::{sync::OnceLock, time::Duration};
use tokio::{
    runtime::Runtime,
    sync::{mpsc, oneshot},
};
use tokio_util::sync::CancellationToken;

pub struct ProbeRequest {
    pub profile: Profile,
    pub secret: Secret,
    pub sentinel_secret: Option<Secret>,
    /// The target SSH options, or the database/Redis connection's SSH route.
    pub ssh: Option<SshOptions>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ProbeKind {
    Authenticated,
    DatabaseReady,
    ElasticsearchReady,
    SqliteReady,
    TelnetReachable,
    RdpAuthenticated,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ProbeOutcome {
    pub kind: ProbeKind,
    pub server: Option<ServerVersion>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ServerVersion {
    pub product: &'static str,
    pub version: Option<String>,
}

impl ProbeOutcome {
    fn new(kind: ProbeKind) -> Self {
        Self { kind, server: None }
    }

    fn with_server(kind: ProbeKind, product: &'static str, version: Option<&str>) -> Self {
        Self {
            kind,
            server: Some(ServerVersion {
                product,
                version: version.and_then(display_version),
            }),
        }
    }
}

// Version lookup is optional metadata: it must not turn an authenticated,
// queryable connection into a failed test or add a full query-timeout delay.
const VERSION_TIMEOUT: Duration = Duration::from_secs(2);

fn display_version(value: &str) -> Option<String> {
    let value: String = value.trim().chars()
        .filter(|c| !c.is_control() && !matches!(c, '\u{061c}' | '\u{200e}' | '\u{200f}' | '\u{2028}'..='\u{202e}' | '\u{2066}'..='\u{2069}'))
        .take(160)
        .collect();
    let value = value.trim();
    (!value.is_empty()).then(|| value.to_owned())
}

fn scalar(output: &QueryOutput) -> Option<&str> {
    let [set] = output.results.as_slice() else {
        return None;
    };
    let [row] = set.rows.as_slice() else {
        return None;
    };
    let [Some(value)] = row.as_slice() else {
        return None;
    };
    (!set.truncated).then_some(value.as_str())
}

fn redis_version(reply: &RedisValue) -> Option<&str> {
    let RedisValue::Bytes(bytes) = reply else {
        return None;
    };
    std::str::from_utf8(bytes).ok()?.lines().find_map(|line| {
        let (key, value) = line.split_once(':')?;
        (key == "redis_version").then_some(value.trim())
    })
}

#[derive(Debug, thiserror::Error)]
pub enum ProbeError {
    #[error("{}", crate::i18n::tr(i18n_key::CONNECTIONS_CONNECTION_TEST_CANCELLED))]
    Cancelled,
    #[error("{}", crate::i18n::tr(i18n_key::CONNECTIONS_CONNECTION_TEST_TIMED_OUT))]
    Timeout,
    #[error(
        "{}",
        crate::i18n::tr(i18n_key::CONNECTIONS_COULD_NOT_START_THE_CONNECTION_TEST)
    )]
    Runtime,
    #[error(
        "{}",
        crate::i18n::tr(
            i18n_key::CONNECTIONS_THIS_CONNECTION_TYPE_DOES_NOT_SUPPORT_A_CONNECTION_TEST
        )
    )]
    Unsupported,
    #[error(
        "{}",
        crate::i18n::tr(i18n_key::CONNECTIONS_INVALID_CONNECTION_TEST_SETTINGS)
    )]
    InvalidConfig,
    #[error(
        "{}",
        crate::i18n::tr(i18n_key::CONNECTIONS_COULD_NOT_REACH_THE_SERVER)
    )]
    Network,
    #[error("{message}")]
    Ssh { kind: FailureKind, message: String },
    #[error(transparent)]
    Redis(#[from] RedisError),
    #[error(transparent)]
    Database(#[from] DatabaseError),
    #[error(transparent)]
    Rdp(#[from] RdpError),
    #[error(transparent)]
    Elasticsearch(#[from] crate::elasticsearch::ElasticsearchError),
}

/// Tests an unsaved connection draft without storing credentials or starting a
/// shell/desktop. Dropping this future cancels its worker and any pending prompt.
pub async fn test(
    request: ProbeRequest,
    host_keys: mpsc::Sender<HostKeyChallenge>,
    certificate_events: mpsc::Sender<RdpEvent>,
    cancel: CancellationToken,
) -> Result<ProbeOutcome, ProbeError> {
    let stop = cancel.child_token();
    let _guard = CancelOnDrop(stop.clone());
    let (reply, result) = oneshot::channel();
    runtime()?.spawn(async move {
        let result = tokio::select! {
            biased;
            _ = stop.cancelled() => Err(ProbeError::Cancelled),
            result = run(request, host_keys, certificate_events, stop.clone()) => result,
        };
        let _ = reply.send(result);
    });
    result.await.map_err(|_| ProbeError::Runtime)?
}

async fn run(
    request: ProbeRequest,
    host_keys: mpsc::Sender<HostKeyChallenge>,
    certificate_events: mpsc::Sender<RdpEvent>,
    stop: CancellationToken,
) -> Result<ProbeOutcome, ProbeError> {
    let ProbeRequest {
        profile,
        secret,
        sentinel_secret,
        ssh,
    } = request;
    let expects_ssh = match &profile.connection {
        Connection::Ssh(_) => true,
        Connection::Redis(config) => config.ssh_profile.is_some(),
        Connection::Elasticsearch(config) => config.ssh_profile.is_some(),
        Connection::Mysql(config)
        | Connection::Postgres(config)
        | Connection::ClickHouse(config) => config.ssh_profile.is_some(),
        _ => false,
    };
    if expects_ssh != ssh.is_some() {
        return Err(ProbeError::InvalidConfig);
    }
    match profile.connection {
        Connection::Local(_) => Err(ProbeError::Unsupported),
        Connection::Ssh(config) => {
            config.validate().map_err(|_| ProbeError::InvalidConfig)?;
            let options = ssh.ok_or(ProbeError::InvalidConfig)?;
            let connection = crate::session::connect_tunnel(options, host_keys, stop.child_token())
                .await
                .map_err(|(kind, message)| ProbeError::Ssh { kind, message })?;
            let version = connection.server_identification().map(|id| {
                id.strip_prefix("SSH-2.0-")
                    .or_else(|| id.strip_prefix("SSH-1.99-"))
                    .unwrap_or(id)
            });
            let outcome = ProbeOutcome::with_server(ProbeKind::Authenticated, "SSH", version);
            drop(connection);
            Ok(outcome)
        }
        Connection::Telnet(config) => {
            config.validate().map_err(|_| ProbeError::InvalidConfig)?;
            let host = crate::config::known_hosts::normalize_host(&config.host)
                .map_err(|_| ProbeError::InvalidConfig)?;
            let socket = tokio::time::timeout(
                Duration::from_secs(config.timeout_secs),
                tokio::net::TcpStream::connect((host, config.port)),
            )
            .await
            .map_err(|_| ProbeError::Timeout)?
            .map_err(|_| ProbeError::Network)?;
            drop(socket);
            Ok(ProbeOutcome::new(ProbeKind::TelnetReachable))
        }
        Connection::Rdp(config) => {
            crate::rdp::probe(config, secret, &certificate_events).await?;
            Ok(ProbeOutcome::new(ProbeKind::RdpAuthenticated))
        }
        Connection::Redis(config) => {
            let database = config.database;
            let password =
                (!secret.expose().is_empty() || config.credential.is_some()).then_some(secret);
            let client = RedisClient::connect_authenticated(
                config,
                password,
                sentinel_secret,
                ssh.map(|ssh| (ssh, host_keys)),
            )
            .await?;
            let result = client.command(database, vec![b"PING".to_vec()]).await;
            if result.as_ref() != Ok(&RedisValue::Bytes(b"PONG".to_vec())) {
                client.close();
                result?;
                return Err(RedisError::Protocol.into());
            }
            let info = tokio::time::timeout(
                VERSION_TIMEOUT,
                client.command(database, vec![b"INFO".to_vec(), b"server".to_vec()]),
            )
            .await
            .ok()
            .and_then(Result::ok);
            let outcome = ProbeOutcome::with_server(
                ProbeKind::DatabaseReady,
                "Redis",
                info.as_ref().and_then(redis_version),
            );
            client.close();
            Ok(outcome)
        }
        Connection::Elasticsearch(config) => {
            let client = crate::elasticsearch::ElasticsearchClient::connect(
                config,
                secret,
                ssh.map(|options| (options, host_keys)),
            )
            .await?;
            let outcome = ProbeOutcome::with_server(
                ProbeKind::ElasticsearchReady,
                "Elasticsearch",
                Some(client.server_version()),
            );
            client.close();
            Ok(outcome)
        }
        connection @ (Connection::Mysql(_)
        | Connection::Postgres(_)
        | Connection::ClickHouse(_)
        | Connection::Sqlite(_)) => {
            let local = matches!(connection, Connection::Sqlite(_));
            let config = connection.database().expect("database connection");
            let engine = config.engine();
            let client =
                DatabaseClient::connect(config, secret, ssh.map(|ssh| (ssh, host_keys))).await?;
            let result = client.execute("SELECT 1".into(), 1).await;
            if result.as_ref().ok().and_then(scalar) != Some("1") {
                client.close();
                result?;
                return Err(DatabaseError::Protocol.into());
            }
            let query = match engine {
                SqlEngine::Mysql | SqlEngine::ClickHouse => "SELECT VERSION()",
                SqlEngine::Postgres => "SHOW server_version",
                SqlEngine::Sqlite => "SELECT sqlite_version()",
            };
            let version = tokio::time::timeout(VERSION_TIMEOUT, client.execute(query.into(), 1))
                .await
                .ok()
                .and_then(Result::ok);
            let outcome = ProbeOutcome::with_server(
                if local {
                    ProbeKind::SqliteReady
                } else {
                    ProbeKind::DatabaseReady
                },
                engine.label(),
                version.as_ref().and_then(scalar),
            );
            client.close();
            Ok(outcome)
        }
    }
}

struct CancelOnDrop(CancellationToken);
impl Drop for CancelOnDrop {
    fn drop(&mut self) {
        self.0.cancel();
    }
}

fn runtime() -> Result<&'static Runtime, ProbeError> {
    static RUNTIME: OnceLock<Result<Runtime, std::io::Error>> = OnceLock::new();
    RUNTIME
        .get_or_init(|| {
            tokio::runtime::Builder::new_multi_thread()
                .worker_threads(1)
                .thread_name("rustterm-probe")
                .enable_all()
                .build()
        })
        .as_ref()
        .map_err(|_| ProbeError::Runtime)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{RdpConfig, RedisConfig, TelnetConfig, Uuid};
    use tokio::{io::AsyncReadExt, net::TcpListener};

    #[test]
    fn displayed_versions_are_bounded_and_cannot_inject_control_text() {
        assert_eq!(
            display_version(" 8.0.36-ubuntu "),
            Some("8.0.36-ubuntu".into())
        );
        assert_eq!(
            display_version("\r\n\u{202e}7.4.2\u{2069}\0"),
            Some("7.4.2".into())
        );
        assert_eq!(display_version("\r\n \t"), None);
        assert_eq!(
            display_version(&"界".repeat(1000)).unwrap().chars().count(),
            160
        );
    }

    #[tokio::test]
    async fn redis_probe_reports_version_and_keeps_success_if_info_is_denied_or_stalls() {
        use std::io::Write;
        for reply in [
            Some("# Server\r\nredis_version:7.2.4\r\n"),
            Some("NOPERM"),
            Some("# Server\r\nother_field:unknown\r\n"),
            None,
        ] {
            let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
            let profile = Profile::redis(
                "Redis",
                RedisConfig {
                    host: "127.0.0.1".into(),
                    port: listener.local_addr().unwrap().port(),
                    timeout_secs: 10,
                    ..Default::default()
                },
            );
            let server = std::thread::spawn(move || {
                let (mut stream, _) = listener.accept().unwrap();
                stream
                    .set_read_timeout(Some(Duration::from_secs(5)))
                    .unwrap();
                let mut parser = redis::Parser::new();
                for name in [b"PING".as_slice(), b"INFO"] {
                    let redis::Value::Array(args) = parser.parse_value(&mut stream).unwrap() else {
                        panic!("expected array")
                    };
                    assert_eq!(args[0], redis::Value::BulkString(name.to_vec()));
                    if name == b"PING" {
                        stream.write_all(b"+PONG\r\n").unwrap();
                    } else {
                        assert_eq!(args[1], redis::Value::BulkString(b"server".to_vec()));
                        match reply {
                            Some("NOPERM") => stream
                                .write_all(b"-NOPERM INFO is not allowed\r\n")
                                .unwrap(),
                            Some(body) => stream
                                .write_all(format!("${}\r\n{body}\r\n", body.len()).as_bytes())
                                .unwrap(),
                            None => {}
                        }
                    }
                }
                let mut byte = [0];
                assert_eq!(std::io::Read::read(&mut stream, &mut byte).unwrap(), 0);
            });
            let outcome = tokio::time::timeout(
                Duration::from_secs(4),
                probe(profile, CancellationToken::new()),
            )
            .await
            .unwrap()
            .unwrap();
            assert_eq!(outcome.kind, ProbeKind::DatabaseReady);
            let server_version = outcome.server.unwrap();
            assert_eq!(server_version.product, "Redis");
            assert_eq!(
                server_version.version.as_deref(),
                reply
                    .filter(|value| value.contains("redis_version:"))
                    .map(|_| "7.2.4")
            );
            server.join().unwrap();
        }
    }

    pub(super) async fn probe(
        profile: Profile,
        cancel: CancellationToken,
    ) -> Result<ProbeOutcome, ProbeError> {
        let (host_keys, _host_keys) = mpsc::channel(1);
        let (certificates, _certificates) = mpsc::channel(1);
        test(
            ProbeRequest {
                profile,
                secret: Secret::new(String::new()),
                sentinel_secret: None,
                ssh: None,
            },
            host_keys,
            certificates,
            cancel,
        )
        .await
    }

    #[tokio::test]
    async fn sqlite_probe_opens_existing_files_without_credentials_or_creation() {
        let directory = tempfile::tempdir().unwrap();
        let mut config = crate::config::SqliteConfig {
            path: directory.path().join("probe.sqlite"),
            ..Default::default()
        };
        let profile = Profile::sqlite("Local database", config.clone());
        assert!(probe(profile, CancellationToken::new()).await.is_err());
        assert!(!config.path.exists());
        config.read_only = false;
        crate::database::create_sqlite(config.clone())
            .await
            .unwrap();
        for read_only in [true, false] {
            config.read_only = read_only;
            let profile = Profile::sqlite("Local database", config.clone());
            assert_eq!(
                probe(profile, CancellationToken::new()).await.unwrap(),
                ProbeOutcome::with_server(
                    ProbeKind::SqliteReady,
                    "SQLite",
                    Some(rusqlite::version())
                ),
            );
        }
    }

    #[tokio::test]
    async fn telnet_probe_checks_reachability_without_sending_login_input() {
        tokio::time::timeout(Duration::from_secs(3), async {
            let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
            let profile = Profile::telnet(
                "test",
                TelnetConfig {
                    host: "127.0.0.1".into(),
                    port: listener.local_addr().unwrap().port(),
                    ..TelnetConfig::default()
                },
            );
            let outcome = probe(profile, CancellationToken::new()).await.unwrap();
            assert_eq!(outcome, ProbeOutcome::new(ProbeKind::TelnetReachable));
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut received = [0; 1];
            assert_eq!(socket.read(&mut received).await.unwrap(), 0);
        })
        .await
        .unwrap();
    }

    #[tokio::test]
    async fn cancelled_or_invalid_probes_do_not_attempt_a_connection() {
        let cancelled = CancellationToken::new();
        cancelled.cancel();
        let invalid = Profile::telnet(
            "test",
            TelnetConfig {
                host: "127.0.0.1".into(),
                port: 0,
                ..TelnetConfig::default()
            },
        );
        assert!(matches!(
            probe(invalid.clone(), cancelled).await,
            Err(ProbeError::Cancelled)
        ));
        assert!(matches!(
            probe(invalid, CancellationToken::new()).await,
            Err(ProbeError::InvalidConfig)
        ));
        let missing_route = Profile::redis(
            "test",
            RedisConfig {
                host: "127.0.0.1".into(),
                ssh_profile: Some(Uuid::new_v4()),
                ..RedisConfig::default()
            },
        );
        assert!(matches!(
            probe(missing_route, CancellationToken::new()).await,
            Err(ProbeError::InvalidConfig)
        ));
    }

    #[tokio::test]
    async fn dropping_an_inflight_probe_closes_the_handshake_socket() {
        tokio::time::timeout(Duration::from_secs(3), async {
            let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
            let profile = Profile::rdp(
                "test",
                RdpConfig {
                    host: "127.0.0.1".into(),
                    port: listener.local_addr().unwrap().port(),
                    username: "test".into(),
                    ..RdpConfig::default()
                },
            );
            let task = tokio::spawn(probe(profile, CancellationToken::new()));
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut negotiation = vec![0; rustrdp::connection::negotiation_request().len()];
            socket.read_exact(&mut negotiation).await.unwrap();
            assert_eq!(negotiation, rustrdp::connection::negotiation_request());
            task.abort();
            let mut received = [0; 1];
            assert_eq!(socket.read(&mut received).await.unwrap(), 0);
        })
        .await
        .unwrap();
    }
}
