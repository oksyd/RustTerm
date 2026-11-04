use crate::i18n::messages as i18n_key;

use crate::{
    config::{Connection, Profile, Secret},
    database::{DatabaseClient, DatabaseError},
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
pub enum ProbeOutcome {
    Authenticated,
    DatabaseReady,
    SqliteReady,
    TelnetReachable,
    RdpAuthenticated,
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
        Connection::Mysql(config) | Connection::Postgres(config) => config.ssh_profile.is_some(),
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
            drop(connection);
            Ok(ProbeOutcome::Authenticated)
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
            Ok(ProbeOutcome::TelnetReachable)
        }
        Connection::Rdp(config) => {
            crate::rdp::probe(config, secret, &certificate_events).await?;
            Ok(ProbeOutcome::RdpAuthenticated)
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
            client.close();
            if result? != RedisValue::Bytes(b"PONG".to_vec()) {
                return Err(RedisError::Protocol.into());
            }
            Ok(ProbeOutcome::DatabaseReady)
        }
        connection @ (Connection::Mysql(_) | Connection::Postgres(_) | Connection::Sqlite(_)) => {
            let local = matches!(connection, Connection::Sqlite(_));
            let config = connection.database().expect("database connection");
            let client =
                DatabaseClient::connect(config, secret, ssh.map(|ssh| (ssh, host_keys))).await?;
            let result = client.execute("SELECT 1".into(), 1).await;
            client.close();
            let result = result?;
            if !matches!(result.results.as_slice(), [set]
                if !set.truncated && set.rows == [vec![Some("1".to_owned())]])
            {
                return Err(DatabaseError::Protocol.into());
            }
            Ok(if local {
                ProbeOutcome::SqliteReady
            } else {
                ProbeOutcome::DatabaseReady
            })
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

    async fn probe(
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
                ProbeOutcome::SqliteReady,
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
            assert_eq!(outcome, ProbeOutcome::TelnetReachable);
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
