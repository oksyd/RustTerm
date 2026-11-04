use super::{ForwardController, ForwardState, SshAuth, SshOptions};
use crate::{
    config::{ForwardKind, KnownHosts, PortForward},
    session::{Command, Session, SessionState},
};
use russh::{
    Channel, ChannelId,
    keys::{Algorithm, PrivateKey},
    server,
};
use std::{
    collections::{HashMap, HashSet},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    },
    time::Duration,
};
use tokio::{net::TcpListener, sync::Notify, task::JoinHandle};
use tokio_util::sync::CancellationToken;

#[derive(Default)]
struct Peer {
    accepted: AtomicUsize,
    authenticated: Mutex<Vec<usize>>,
    shell: AtomicUsize,
    forwards: Mutex<Vec<usize>>,
    handles: Mutex<HashMap<usize, server::Handle>>,
    closed: Mutex<HashSet<usize>>,
    hold_forward: AtomicBool,
    forward_started: Notify,
    release_forward: Notify,
}

struct Server {
    id: usize,
    peer: Arc<Peer>,
}

impl server::Handler for Server {
    type Error = russh::Error;

    async fn auth_password(
        &mut self,
        user: &str,
        password: &str,
    ) -> Result<server::Auth, Self::Error> {
        if user != "test" || password != "test" {
            return Ok(server::Auth::reject());
        }
        self.peer.authenticated.lock().unwrap().push(self.id);
        Ok(server::Auth::Accept)
    }

    async fn channel_open_session(
        &mut self,
        _: Channel<server::Msg>,
        reply: server::ChannelOpenHandle,
        _: &mut server::Session,
    ) -> Result<(), Self::Error> {
        reply.accept().await;
        Ok(())
    }

    async fn pty_request(
        &mut self,
        channel: ChannelId,
        _: &str,
        _: u32,
        _: u32,
        _: u32,
        _: u32,
        _: &[(russh::Pty, u32)],
        session: &mut server::Session,
    ) -> Result<(), Self::Error> {
        session.channel_success(channel)
    }

    async fn shell_request(
        &mut self,
        channel: ChannelId,
        session: &mut server::Session,
    ) -> Result<(), Self::Error> {
        self.peer.shell.store(self.id, Ordering::Release);
        session.channel_success(channel)
    }

    async fn data(
        &mut self,
        channel: ChannelId,
        data: &[u8],
        session: &mut server::Session,
    ) -> Result<(), Self::Error> {
        if self.peer.shell.load(Ordering::Acquire) == self.id {
            session.data(channel, data.to_vec())?;
        }
        Ok(())
    }

    async fn tcpip_forward(
        &mut self,
        _: &str,
        _: &mut u32,
        _: &mut server::Session,
    ) -> Result<bool, Self::Error> {
        self.peer.forwards.lock().unwrap().push(self.id);
        if self.peer.hold_forward.swap(false, Ordering::AcqRel) {
            self.peer.forward_started.notify_one();
            self.peer.release_forward.notified().await;
        }
        Ok(true)
    }

    async fn cancel_tcpip_forward(
        &mut self,
        _: &str,
        _: u32,
        _: &mut server::Session,
    ) -> Result<bool, Self::Error> {
        Ok(true)
    }
}

struct Fixture {
    session: Session,
    peer: Arc<Peer>,
    stop: CancellationToken,
    server: Option<JoinHandle<()>>,
    _directory: tempfile::TempDir,
}

impl Fixture {
    async fn new(enabled: bool, hold_forward: bool) -> Self {
        Self::with_destination(enabled, hold_forward, 2223).await
    }

    async fn with_destination(enabled: bool, hold_forward: bool, destination_port: u16) -> Self {
        let peer = Arc::new(Peer::default());
        peer.hold_forward.store(hold_forward, Ordering::Release);
        let host = PrivateKey::random(&mut rand::rng(), Algorithm::Ed25519).unwrap();
        let config = Arc::new(server::Config {
            keys: vec![host.clone()],
            auth_rejection_time: Duration::ZERO,
            auth_rejection_time_initial: Some(Duration::ZERO),
            ..Default::default()
        });
        let listener = TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let stop = CancellationToken::new();
        let server_stop = stop.clone();
        let server_peer = peer.clone();
        let server = tokio::spawn(async move {
            let mut connections = tokio::task::JoinSet::new();
            loop {
                tokio::select! {
                    biased;
                    _ = server_stop.cancelled() => break,
                    Some(result) = connections.join_next(), if !connections.is_empty() => {
                        result.unwrap();
                    },
                    stream = listener.accept() => {
                        let (stream, _) = stream.unwrap();
                        let peer = server_peer.clone();
                        let config = config.clone();
                        let id = peer.accepted.fetch_add(1, Ordering::AcqRel) + 1;
                        connections.spawn(async move {
                            let connection = server::run_stream(config, stream, Server { id, peer: peer.clone() })
                                .await
                                .unwrap();
                            peer.handles.lock().unwrap().insert(id, connection.handle());
                            let _ = connection.await;
                            peer.handles.lock().unwrap().remove(&id);
                            peer.closed.lock().unwrap().insert(id);
                        });
                    },
                }
            }
            while let Some(result) = connections.join_next().await {
                result.unwrap();
            }
        });
        let directory = tempfile::tempdir().unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(directory.path(), std::fs::Permissions::from_mode(0o700))
                .unwrap();
        }
        let known_hosts_path = directory.path().join("hosts.toml");
        let public = host.public_key();
        KnownHosts::trust_and_save(
            &known_hosts_path,
            "127.0.0.1",
            port,
            public.algorithm().as_ref(),
            &public.to_openssh().unwrap(),
        )
        .unwrap();
        let session = Session::spawn_ssh(
            SshOptions {
                host: "127.0.0.1".into(),
                port,
                username: "test".into(),
                auth: SshAuth::Password("test".into()),
                known_hosts_path,
                timeout_secs: 2,
                keepalive_secs: 0,
                keepalive_max: 2,
                shell_integration: false,
                jump_hosts: Vec::new(),
                forwards: vec![PortForward {
                    kind: ForwardKind::Remote,
                    bind_host: "127.0.0.1".into(),
                    bind_port: 2222,
                    destination_host: "127.0.0.1".into(),
                    destination_port,
                    enabled,
                }],
                authentication: None,
            },
            80,
            8,
            0,
        )
        .unwrap();
        let fixture = Self {
            session,
            peer,
            stop,
            server: Some(server),
            _directory: directory,
        };
        wait_for(|| matches!(fixture.session.state(), SessionState::Connected)).await;
        fixture
    }

    fn forwarding(&self) -> ForwardController {
        self.session.forwarding().expect("forwarding controller")
    }

    async fn echo(&self, marker: &str) {
        assert_eq!(self.session.state(), SessionState::Connected);
        self.session
            .send(Command::Input(format!("{marker}\r\n").into_bytes()))
            .unwrap();
        let mut snapshots = self.session.subscribe_snapshots();
        loop {
            if self.session.request_snapshot() {
                snapshots.changed().await.unwrap();
            }
            let text: String = self
                .session
                .snapshot()
                .lines
                .iter()
                .flat_map(|line| &line.cells)
                .map(|cell| cell.text.as_str())
                .collect();
            if text.contains(marker) {
                break;
            }
            assert_eq!(self.session.state(), SessionState::Connected);
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    }

    async fn finish(&mut self) {
        self.session.close();
        wait_for(|| {
            self.peer.closed.lock().unwrap().len() == self.peer.accepted.load(Ordering::Acquire)
        })
        .await;
        self.stop.cancel();
        self.server.take().unwrap().await.unwrap();
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        self.session.close();
        self.stop.cancel();
        if let Some(server) = self.server.take() {
            server.abort();
        }
    }
}

async fn wait_for(mut predicate: impl FnMut() -> bool) {
    while !predicate() {
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
}

#[tokio::test]
async fn stalled_or_failed_forwarding_connection_keeps_shell_usable_and_can_retry() {
    tokio::time::timeout(Duration::from_secs(5), async {
        let mut fixture = Fixture::new(true, true).await;
        let forwarding = fixture.forwarding();
        fixture.peer.forward_started.notified().await;
        let shell_id = fixture.peer.shell.load(Ordering::Acquire);
        let auxiliary_id = fixture.peer.forwards.lock().unwrap()[0];
        assert_ne!(auxiliary_id, shell_id);
        assert_eq!(fixture.peer.accepted.load(Ordering::Acquire), 2);
        assert_eq!(fixture.peer.authenticated.lock().unwrap().len(), 2);
        assert_eq!(forwarding.statuses()[0].state, ForwardState::Starting);
        fixture.echo("shell-during-forward-start").await;

        fixture.peer.release_forward.notify_one();
        wait_for(|| forwarding.statuses()[0].state == ForwardState::Running).await;
        let auxiliary = fixture.peer.handles.lock().unwrap()[&auxiliary_id].clone();
        auxiliary
            .disconnect(
                russh::Disconnect::ByApplication,
                "test disconnect".into(),
                String::new(),
            )
            .await
            .unwrap();
        wait_for(|| matches!(forwarding.statuses()[0].state, ForwardState::Failed(_))).await;
        fixture.echo("shell-after-forward-disconnect").await;
        forwarding.retry(0).unwrap();
        wait_for(|| forwarding.statuses()[0].state == ForwardState::Running).await;
        let retry_id = fixture.peer.forwards.lock().unwrap()[1];
        assert_ne!(retry_id, auxiliary_id);
        assert_ne!(retry_id, shell_id);
        assert_eq!(fixture.peer.accepted.load(Ordering::Acquire), 3);
        assert_eq!(fixture.peer.authenticated.lock().unwrap().len(), 3);
        fixture.echo("shell-after-forward-retry").await;
        fixture.finish().await;
    })
    .await
    .unwrap();
}

#[tokio::test]
async fn forwarding_connects_on_enable_and_closes_when_stopped_or_parent_closes() {
    tokio::time::timeout(Duration::from_secs(5), async {
        let mut fixture = Fixture::new(false, false).await;
        let forwarding = fixture.forwarding();
        fixture.echo("shell-before-forward-enable").await;
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert_eq!(fixture.peer.accepted.load(Ordering::Acquire), 1);
        assert_eq!(fixture.peer.authenticated.lock().unwrap().len(), 1);
        assert_eq!(forwarding.statuses()[0].state, ForwardState::Stopped);

        forwarding.set_enabled(0, true).unwrap();
        wait_for(|| forwarding.statuses()[0].state == ForwardState::Running).await;
        let auxiliary_id = fixture.peer.forwards.lock().unwrap()[0];
        assert_eq!(fixture.peer.accepted.load(Ordering::Acquire), 2);
        forwarding.set_enabled(0, false).unwrap();
        wait_for(|| {
            forwarding.statuses()[0].state == ForwardState::Stopped
                && fixture.peer.closed.lock().unwrap().contains(&auxiliary_id)
        })
        .await;
        fixture.echo("shell-after-forward-stop").await;

        forwarding.set_enabled(0, true).unwrap();
        wait_for(|| forwarding.statuses()[0].state == ForwardState::Running).await;
        assert_eq!(fixture.peer.accepted.load(Ordering::Acquire), 3);
        fixture.finish().await;
        assert_eq!(forwarding.statuses()[0].state, ForwardState::Stopped);
    })
    .await
    .unwrap();
}

#[tokio::test]
async fn only_auxiliary_connection_routes_remote_forwarded_channels() {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    tokio::time::timeout(Duration::from_secs(5), async {
        let target = TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
        let target_port = target.local_addr().unwrap().port();
        let mut fixture = Fixture::with_destination(true, false, target_port).await;
        let forwarding = fixture.forwarding();
        wait_for(|| forwarding.statuses()[0].state == ForwardState::Running).await;
        let shell_id = fixture.peer.shell.load(Ordering::Acquire);
        let auxiliary_id = fixture.peer.forwards.lock().unwrap()[0];
        assert_ne!(shell_id, auxiliary_id);
        let primary = fixture.peer.handles.lock().unwrap()[&shell_id].clone();
        let auxiliary = fixture.peer.handles.lock().unwrap()[&auxiliary_id].clone();

        // The destination is already listening, so rejection must come from the
        // connection's routing policy rather than a failed destination connect.
        assert!(matches!(
            primary
                .channel_open_forwarded_tcpip("127.0.0.1", 2222, "127.0.0.1", 10000)
                .await,
            Err(russh::Error::ChannelOpenFailure(_))
        ));
        let target_task = tokio::spawn(async move {
            let (mut stream, _) = target.accept().await.unwrap();
            let mut request = [0; b"remote request".len()];
            stream.read_exact(&mut request).await.unwrap();
            assert_eq!(&request, b"remote request");
            stream.write_all(b"local response").await.unwrap();
            stream.shutdown().await.unwrap();
        });
        let mut stream = auxiliary
            .channel_open_forwarded_tcpip("127.0.0.1", 2222, "127.0.0.1", 10001)
            .await
            .unwrap()
            .into_stream();
        stream.write_all(b"remote request").await.unwrap();
        let mut response = [0; b"local response".len()];
        stream.read_exact(&mut response).await.unwrap();
        assert_eq!(&response, b"local response");
        drop(stream);
        target_task.await.unwrap();
        fixture.echo("shell-after-forwarded-channel").await;
        fixture.finish().await;
    })
    .await
    .unwrap();
}
