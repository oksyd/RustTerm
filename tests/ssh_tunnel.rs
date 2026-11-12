//! A real in-process russh server with a minimal RESP fixture behind direct-tcpip.
//! No external server, PTY, local forwarding listener or GUI is started.
use russh::{
    Channel, ChannelId,
    keys::{Algorithm, PrivateKey, PublicKey, ssh_key::LineEnding},
    server::{self, VerifiedPublicKey},
};
use rustterm::{
    config::{KnownHosts, RedisConfig, Secret, Uuid},
    redis::{RedisClient, RedisError, RedisValue},
    session::{HostKeyChallenge, SshAuth, SshOptions},
};
use std::{
    path::Path,
    sync::{
        Arc, Mutex,
        atomic::{AtomicUsize, Ordering},
    },
    time::Duration,
};
use tokio::{net::TcpListener, sync::mpsc, task::JoinHandle};

#[derive(Default)]
struct Events {
    authentications: AtomicUsize,
    shell_channels: AtomicUsize,
    connections: AtomicUsize,
    destinations: Mutex<Vec<(String, u32)>>,
    redis_commands: Mutex<Vec<Vec<Vec<u8>>>>,
    http_requests: Mutex<Vec<String>>,
}
struct Active(Arc<Events>);
impl Drop for Active {
    fn drop(&mut self) {
        self.0.connections.fetch_sub(1, Ordering::SeqCst);
    }
}
#[derive(Clone)]
struct Handler {
    events: Arc<Events>,
    key: Option<PublicKey>,
    pending: Vec<u8>,
    reject_forward: bool,
}
impl server::Handler for Handler {
    type Error = russh::Error;
    async fn auth_password(
        &mut self,
        user: &str,
        password: &str,
    ) -> Result<server::Auth, Self::Error> {
        self.events.authentications.fetch_add(1, Ordering::SeqCst);
        Ok(if user == "ssh-user" && password == "ssh-secret" {
            server::Auth::Accept
        } else {
            server::Auth::reject()
        })
    }
    async fn auth_publickey(
        &mut self,
        user: &str,
        key: VerifiedPublicKey<'_>,
    ) -> Result<server::Auth, Self::Error> {
        self.events.authentications.fetch_add(1, Ordering::SeqCst);
        Ok(
            if user == "ssh-user" && self.key.as_ref() == Some(key.public_key) {
                server::Auth::Accept
            } else {
                server::Auth::reject()
            },
        )
    }
    async fn channel_open_session(
        &mut self,
        _channel: Channel<server::Msg>,
        _reply: server::ChannelOpenHandle,
        _session: &mut server::Session,
    ) -> Result<(), Self::Error> {
        self.events.shell_channels.fetch_add(1, Ordering::SeqCst);
        Ok(())
    }
    async fn channel_open_direct_tcpip(
        &mut self,
        _channel: Channel<server::Msg>,
        host: &str,
        port: u32,
        _originator: &str,
        _originator_port: u32,
        reply: server::ChannelOpenHandle,
        _session: &mut server::Session,
    ) -> Result<(), Self::Error> {
        self.events
            .destinations
            .lock()
            .unwrap()
            .push((host.into(), port));
        if !self.reject_forward {
            reply.accept().await;
        }
        Ok(())
    }
    async fn data(
        &mut self,
        channel: ChannelId,
        data: &[u8],
        session: &mut server::Session,
    ) -> Result<(), Self::Error> {
        self.pending.extend(data);
        if self.pending.starts_with(b"GET ") || self.pending.starts_with(b"POST ") {
            let Some(end) = self
                .pending
                .windows(4)
                .position(|window| window == b"\r\n\r\n")
            else {
                return Ok(());
            };
            let header = String::from_utf8_lossy(&self.pending[..end]);
            let length = header
                .lines()
                .find_map(|line| {
                    let (key, value) = line.split_once(':')?;
                    key.eq_ignore_ascii_case("content-length")
                        .then(|| value.trim().parse::<usize>().unwrap())
                })
                .unwrap_or(0);
            if self.pending.len() < end + 4 + length {
                return Ok(());
            }
            let request = String::from_utf8(std::mem::take(&mut self.pending)).unwrap();
            let body = if request.contains("default_format=JSONCompact") {
                r#"{"meta":[{"name":"version()","type":"String"}],"data":[["25.8.1"]],"rows":1}"#
            } else if request.starts_with("GET / HTTP/") {
                r#"{"cluster_name":"via-ssh","version":{"number":"8.19.0"}}"#
            } else {
                r#"{"hits":{"hits":[{"_source":{"route":"ssh"}}]}}"#
            };
            let response = format!(
                "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            );
            self.events.http_requests.lock().unwrap().push(request);
            session.data(channel, response.into_bytes())?;
            return Ok(());
        }
        let Ok(redis::Value::Array(values)) = redis::parse_redis_value(&self.pending) else {
            return Ok(());
        };
        let values: Vec<Vec<u8>> = values
            .into_iter()
            .map(|value| match value {
                redis::Value::BulkString(bytes) => bytes,
                _ => panic!("command argument is not binary"),
            })
            .collect();
        self.pending.clear();
        let response = match values[0].as_slice() {
            b"AUTH"
                if values
                    == [
                        b"AUTH".to_vec(),
                        b"redis-user".to_vec(),
                        b"redis-secret".to_vec(),
                    ] =>
            {
                b"+OK\r\n".to_vec()
            }
            b"SELECT" => b"+OK\r\n".to_vec(),
            b"PING" => b"+PONG\r\n".to_vec(),
            b"GET" => b"$8\r\nvia-ssh\0\r\n".to_vec(),
            _ => b"-ERR fixture command rejected\r\n".to_vec(),
        };
        self.events.redis_commands.lock().unwrap().push(values);
        session.data(channel, response)?;
        Ok(())
    }
}
struct Fixture {
    port: u16,
    host_key: PrivateKey,
    events: Arc<Events>,
    task: JoinHandle<()>,
}
impl Fixture {
    async fn new(key: Option<PublicKey>, reject_forward: bool) -> Self {
        let listener = TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let host_key = PrivateKey::random(&mut rand::rng(), Algorithm::Ed25519).unwrap();
        let events = Arc::new(Events::default());
        let handler = Handler {
            events: events.clone(),
            key,
            pending: Vec::new(),
            reject_forward,
        };
        let config = Arc::new(server::Config {
            server_id: russh::SshId::Standard("SSH-2.0-RustTermTest_1.2".into()),
            keys: vec![host_key.clone()],
            auth_rejection_time: Duration::ZERO,
            auth_rejection_time_initial: Some(Duration::ZERO),
            ..Default::default()
        });
        let task = tokio::spawn(async move {
            let mut sessions = tokio::task::JoinSet::new();
            loop {
                tokio::select! {
                    connection = listener.accept() => {
                        let Ok((socket, _)) = connection else { break };
                        let handler = handler.clone(); let config = config.clone();
                        sessions.spawn(async move {
                            handler.events.connections.fetch_add(1, Ordering::SeqCst);
                            let _active = Active(handler.events.clone());
                            if let Ok(session) = server::run_stream(config, socket, handler).await { let _ = session.await; }
                        });
                    }
                    _ = sessions.join_next(), if !sessions.is_empty() => {}
                }
            }
        });
        Self {
            port,
            host_key,
            events,
            task,
        }
    }
    fn ssh(&self, path: &Path, auth: SshAuth) -> SshOptions {
        SshOptions {
            host: "127.0.0.1".into(),
            port: self.port,
            username: "ssh-user".into(),
            auth,
            known_hosts_path: path.into(),
            timeout_secs: 1,
            keepalive_secs: 0,
            keepalive_max: 2,
            shell_integration: false,
            jump_hosts: Vec::new(),
            forwards: Vec::new(),
            auto_forward: Default::default(),
            forwarding_registry: None,
            authentication: None,
        }
    }
    fn redis(&self) -> RedisConfig {
        RedisConfig {
            host: "redis.internal.invalid".into(),
            port: 6381,
            username: Some("redis-user".into()),
            timeout_secs: 1,
            ssh_profile: Some(Uuid::new_v4()),
            ..Default::default()
        }
    }
    fn trust(&self, path: &Path, key: &PrivateKey) {
        KnownHosts::trust_and_save(
            path,
            "127.0.0.1",
            self.port,
            "ssh-ed25519",
            &key.public_key().to_openssh().unwrap(),
        )
        .unwrap();
    }
}

#[tokio::test]
async fn ssh_connection_probe_reports_server_identification_without_opening_a_shell() {
    use rustterm::{
        config::{Profile, SshConfig},
        connection_probe::{self, ProbeKind, ProbeRequest},
    };
    let fixture = Fixture::new(None, false).await;
    let directory = private_directory();
    let options = fixture.ssh(
        &directory.path().join("known_hosts"),
        SshAuth::Password("ssh-secret".into()),
    );
    let profile = Profile::ssh(
        "SSH",
        SshConfig {
            host: "127.0.0.1".into(),
            port: fixture.port,
            username: "ssh-user".into(),
            ..Default::default()
        },
    );
    let (hosts, mut challenges) = mpsc::channel(1);
    let (certificates, _) = mpsc::channel(1);
    let probe = tokio::spawn(connection_probe::test(
        ProbeRequest {
            profile,
            secret: Secret::new("ssh-secret".into()),
            sentinel_secret: None,
            ssh: Some(options),
        },
        hosts,
        certificates,
        tokio_util::sync::CancellationToken::new(),
    ));
    let trust = challenge(&mut challenges).await;
    assert_eq!(fixture.events.authentications.load(Ordering::SeqCst), 0);
    trust.reply.send(true).unwrap();
    let outcome = tokio::time::timeout(Duration::from_secs(3), probe)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    assert_eq!(outcome.kind, ProbeKind::Authenticated);
    let info = outcome.server.unwrap();
    assert_eq!(info.product, "SSH");
    assert_eq!(info.version.as_deref(), Some("RustTermTest_1.2"));
    assert_eq!(fixture.events.shell_channels.load(Ordering::SeqCst), 0);
    assert!(fixture.events.destinations.lock().unwrap().is_empty());
    disconnected(&fixture.events).await;
}
impl Drop for Fixture {
    fn drop(&mut self) {
        self.task.abort();
    }
}
fn private_directory() -> tempfile::TempDir {
    let directory = tempfile::tempdir().unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(directory.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
    }
    directory
}
async fn challenge(receiver: &mut mpsc::Receiver<HostKeyChallenge>) -> HostKeyChallenge {
    tokio::time::timeout(Duration::from_secs(3), receiver.recv())
        .await
        .unwrap()
        .unwrap()
}
async fn disconnected(events: &Events) {
    tokio::time::timeout(Duration::from_secs(3), async {
        while events.connections.load(Ordering::SeqCst) != 0 {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
}

#[tokio::test]
async fn elasticsearch_uses_ssh_dns_and_preserves_http_authentication_without_a_pty() {
    use rustterm::{
        config::{ElasticsearchAuth, ElasticsearchConfig},
        elasticsearch::{ElasticsearchClient, Request},
    };
    let fixture = Fixture::new(None, false).await;
    let directory = private_directory();
    let path = directory.path().join("hosts.toml");
    let (trust, mut prompts) = mpsc::channel(1);
    let ssh = fixture.ssh(&path, SshAuth::Password("ssh-secret".into()));
    let config = ElasticsearchConfig {
        endpoint: "http://elastic.internal.invalid:9200".into(),
        auth: ElasticsearchAuth::ApiKey,
        ssh_profile: Some(Uuid::new_v4()),
        ..Default::default()
    };
    let pending = tokio::spawn(ElasticsearchClient::connect(
        config,
        Secret::new("a2V5OnNlY3JldA==".into()),
        Some((ssh, trust)),
    ));
    challenge(&mut prompts).await.reply.send(true).unwrap();
    let client = tokio::time::timeout(Duration::from_secs(3), pending)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    let response = client
        .request(Request::Search {
            index: Some("logs".into()),
            body: "{}".into(),
        })
        .await
        .unwrap();
    assert_eq!(response.json["hits"]["hits"][0]["_source"]["route"], "ssh");
    assert_eq!(
        *fixture.events.destinations.lock().unwrap(),
        [
            ("elastic.internal.invalid".into(), 9200),
            ("elastic.internal.invalid".into(), 9200)
        ]
    );
    assert_eq!(fixture.events.shell_channels.load(Ordering::SeqCst), 0);
    assert!(
        fixture
            .events
            .http_requests
            .lock()
            .unwrap()
            .iter()
            .all(|request| request.contains("ApiKey a2V5OnNlY3JldA=="))
    );
    client.close();
    disconnected(&fixture.events).await;
}

#[tokio::test]
async fn tunnel_waits_for_trust_then_authenticates_and_forwards_without_pty() {
    let fixture = Fixture::new(None, false).await;
    let directory = private_directory();
    let path = directory.path().join("hosts.toml");
    let (trust, mut prompts) = mpsc::channel(1);
    let ssh = fixture.ssh(&path, SshAuth::Password("ssh-secret".into()));
    let config = fixture.redis();
    let connect = tokio::spawn(RedisClient::connect_via_ssh(
        config,
        Some(Secret::new("redis-secret".into())),
        ssh,
        trust,
    ));
    let question = challenge(&mut prompts).await;
    assert_eq!(question.host, "127.0.0.1");
    assert_eq!(question.port, fixture.port);
    assert_eq!(question.algorithm, "ssh-ed25519");
    assert!(question.fingerprint.starts_with("SHA256:"));
    tokio::time::sleep(Duration::from_millis(1150)).await;
    assert_eq!(fixture.events.authentications.load(Ordering::SeqCst), 0);
    assert!(!connect.is_finished());
    question.reply.send(true).unwrap();
    let client = tokio::time::timeout(Duration::from_secs(3), connect)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    assert_eq!(
        client
            .command(2, vec![b"GET".to_vec(), vec![255, 0]])
            .await
            .unwrap(),
        RedisValue::Bytes(b"via-ssh\0".to_vec())
    );
    assert_eq!(
        *fixture.events.destinations.lock().unwrap(),
        [("redis.internal.invalid".into(), 6381)]
    );
    assert_eq!(fixture.events.shell_channels.load(Ordering::SeqCst), 0);
    assert_eq!(fixture.events.authentications.load(Ordering::SeqCst), 1);
    assert!(path.exists());
    client.close();
    disconnected(&fixture.events).await;
}

#[tokio::test]
async fn tunnel_rejection_changed_key_and_cancel_never_authenticate() {
    let fixture = Fixture::new(None, false).await;
    let directory = private_directory();
    let path = directory.path().join("hosts.toml");
    for accept in [false, true] {
        let (trust, mut prompts) = mpsc::channel(1);
        let connect = tokio::spawn(RedisClient::connect_via_ssh(
            fixture.redis(),
            Some(Secret::new("redis-secret".into())),
            fixture.ssh(&path, SshAuth::Password("ssh-secret".into())),
            trust,
        ));
        let question = challenge(&mut prompts).await;
        if !accept {
            question.reply.send(false).unwrap();
            assert!(matches!(
                connect.await.unwrap(),
                Err(RedisError::SshHostKeyRejected)
            ));
        } else {
            connect.abort();
            let _ = connect.await;
            drop(question);
        }
        disconnected(&fixture.events).await;
        assert_eq!(fixture.events.authentications.load(Ordering::SeqCst), 0);
    }
    let other = PrivateKey::random(&mut rand::rng(), Algorithm::Ed25519).unwrap();
    fixture.trust(&path, &other);
    let (trust, mut prompts) = mpsc::channel(1);
    let result = RedisClient::connect_via_ssh(
        fixture.redis(),
        None,
        fixture.ssh(&path, SshAuth::Password("ssh-secret".into())),
        trust,
    )
    .await;
    assert!(matches!(result, Err(RedisError::SshHostKeyChanged)));
    assert!(prompts.try_recv().is_err());
    disconnected(&fixture.events).await;
    assert_eq!(fixture.events.authentications.load(Ordering::SeqCst), 0);
    assert!(fixture.events.destinations.lock().unwrap().is_empty());
}

#[tokio::test]
async fn tunnel_auth_and_forwarding_failures_never_fall_back_to_direct() {
    let fixture = Fixture::new(None, true).await;
    let directory = private_directory();
    let path = directory.path().join("hosts.toml");
    fixture.trust(&path, &fixture.host_key);
    for (password, expected) in [
        ("incorrect", RedisError::SshAuthentication),
        ("ssh-secret", RedisError::SshConnection),
    ] {
        let (trust, _prompts) = mpsc::channel(1);
        let result = RedisClient::connect_via_ssh(
            fixture.redis(),
            None,
            fixture.ssh(&path, SshAuth::Password(password.into())),
            trust,
        )
        .await;
        assert!(matches!(result, Err(error) if error == expected));
        disconnected(&fixture.events).await;
    }
    assert!(fixture.events.redis_commands.lock().unwrap().is_empty());
    assert!(matches!(
        RedisClient::connect(fixture.redis(), None).await,
        Err(RedisError::InvalidConfig)
    ));
}

#[tokio::test]
async fn tunnel_accepts_encrypted_private_key_authentication() {
    let key = PrivateKey::random(&mut rand::rng(), Algorithm::Ed25519).unwrap();
    let fixture = Fixture::new(Some(key.public_key().clone()), false).await;
    let directory = private_directory();
    let path = directory.path().join("hosts.toml");
    fixture.trust(&path, &fixture.host_key);
    let private_key = directory.path().join("id_ed25519");
    let encrypted = key.encrypt(&mut rand::rng(), "key-passphrase").unwrap();
    std::fs::write(
        &private_key,
        encrypted.to_openssh(LineEnding::LF).unwrap().as_bytes(),
    )
    .unwrap();
    let (trust, _prompts) = mpsc::channel(1);
    let mut options = fixture.ssh(
        &path,
        SshAuth::PrivateKey {
            path: private_key,
            passphrase: Some("key-passphrase".into()),
        },
    );
    options.timeout_secs = 5;
    let client = RedisClient::connect_via_ssh(
        fixture.redis(),
        Some(Secret::new("redis-secret".into())),
        options,
        trust,
    )
    .await
    .unwrap();
    assert_eq!(
        client.command(0, vec![b"PING".to_vec()]).await.unwrap(),
        RedisValue::Bytes(b"PONG".to_vec())
    );
    client.close();
    disconnected(&fixture.events).await;
    assert_eq!(fixture.events.authentications.load(Ordering::SeqCst), 1);
    assert_eq!(fixture.events.shell_channels.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn clickhouse_uses_ssh_dns_and_sensitive_auth_without_a_shell() {
    use rustterm::{
        config::{DatabaseConfig, SqlConfig},
        database::DatabaseClient,
    };
    let fixture = Fixture::new(None, false).await;
    let directory = private_directory();
    let (trust, mut prompts) = mpsc::channel(1);
    let config = SqlConfig {
        host: "clickhouse.internal.invalid".into(),
        ssh_profile: Some(Uuid::new_v4()),
        ..SqlConfig::clickhouse()
    };
    let ssh = fixture.ssh(
        &directory.path().join("hosts.toml"),
        SshAuth::Password("ssh-secret".into()),
    );
    let pending = tokio::spawn(DatabaseClient::connect(
        DatabaseConfig::ClickHouse(config),
        Secret::new("password".into()),
        Some((ssh, trust)),
    ));
    challenge(&mut prompts).await.reply.send(true).unwrap();
    let client = tokio::time::timeout(Duration::from_secs(3), pending)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    assert_eq!(
        client
            .execute("SELECT version()".into(), 1)
            .await
            .unwrap()
            .results[0]
            .rows[0][0]
            .as_deref(),
        Some("25.8.1")
    );
    assert_eq!(
        *fixture.events.destinations.lock().unwrap(),
        [
            ("clickhouse.internal.invalid".into(), 8123),
            ("clickhouse.internal.invalid".into(), 8123),
            ("clickhouse.internal.invalid".into(), 8123)
        ]
    );
    assert_eq!(fixture.events.shell_channels.load(Ordering::SeqCst), 0);
    assert!(
        fixture
            .events
            .http_requests
            .lock()
            .unwrap()
            .iter()
            .all(|request| request.contains("Basic ZGVmYXVsdDpwYXNzd29yZA=="))
    );
    client.close();
    disconnected(&fixture.events).await;
}
