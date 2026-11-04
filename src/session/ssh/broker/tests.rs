use super::*;
use crate::config::{ForwardKind, PortForward};
use crate::session::{ResourceClient, ResourceError};
use russh::{
    Channel, ChannelId, ChannelMsg,
    keys::{Algorithm, PrivateKey, PublicKeyOrCertificate},
    server,
};
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicBool, AtomicUsize, Ordering},
};
use tokio::{sync::Notify, task::JoinHandle};

type HeldChannel = (Channel<server::Msg>, server::ChannelOpenHandle);

#[derive(Default)]
struct Peer {
    sessions: AtomicUsize,
    held: Mutex<Vec<HeldChannel>>,
    closed: Mutex<HashSet<ChannelId>>,
    changed: Notify,
    hold_sftp: AtomicBool,
    remote_enabled: AtomicBool,
    remote_requested: Notify,
    remote_release: Notify,
    remote_cancelled: Notify,
    exec_channels: Mutex<HashSet<ChannelId>>,
    exec_commands: Mutex<Vec<Vec<u8>>>,
    exec_started: Notify,
    resource_script: Mutex<Vec<u8>>,
    resource_output: Mutex<Vec<u8>>,
    resource_exit: AtomicUsize,
    stall_exec: AtomicBool,
}

struct Server(Arc<Peer>);
impl server::Handler for Server {
    type Error = russh::Error;

    async fn auth_password(&mut self, _: &str, _: &str) -> Result<server::Auth, Self::Error> {
        Ok(server::Auth::Accept)
    }

    async fn channel_open_session(
        &mut self,
        channel: Channel<server::Msg>,
        reply: server::ChannelOpenHandle,
        _: &mut server::Session,
    ) -> Result<(), Self::Error> {
        let index = self.0.sessions.fetch_add(1, Ordering::SeqCst);
        if index > 0 && self.0.hold_sftp.load(Ordering::SeqCst) {
            self.0.held.lock().unwrap().push((channel, reply));
            self.0.changed.notify_one();
        } else {
            reply.accept().await;
        }
        Ok(())
    }

    async fn channel_open_direct_tcpip(
        &mut self,
        channel: Channel<server::Msg>,
        host: &str,
        _: u32,
        _: &str,
        _: u32,
        reply: server::ChannelOpenHandle,
        _: &mut server::Session,
    ) -> Result<(), Self::Error> {
        if host == "held.example" {
            self.0.held.lock().unwrap().push((channel, reply));
            self.0.changed.notify_one();
        } else {
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
        if self.0.exec_channels.lock().unwrap().contains(&channel) {
            self.0
                .resource_script
                .lock()
                .unwrap()
                .extend_from_slice(data);
            return Ok(());
        }
        session.data(channel, data.to_vec())?;
        Ok(())
    }

    async fn exec_request(
        &mut self,
        channel: ChannelId,
        command: &[u8],
        session: &mut server::Session,
    ) -> Result<(), Self::Error> {
        self.0.exec_channels.lock().unwrap().insert(channel);
        self.0.exec_commands.lock().unwrap().push(command.to_vec());
        session.channel_success(channel)?;
        self.0.exec_started.notify_one();
        Ok(())
    }

    async fn channel_eof(
        &mut self,
        channel: ChannelId,
        session: &mut server::Session,
    ) -> Result<(), Self::Error> {
        if self.0.exec_channels.lock().unwrap().contains(&channel)
            && !self.0.stall_exec.load(Ordering::SeqCst)
        {
            for chunk in self.0.resource_output.lock().unwrap().chunks(8192) {
                session.data(channel, chunk.to_vec())?;
            }
            session
                .exit_status_request(channel, self.0.resource_exit.load(Ordering::SeqCst) as u32)?;
            session.eof(channel)?;
        }
        Ok(())
    }

    async fn channel_close(
        &mut self,
        channel: ChannelId,
        _: &mut server::Session,
    ) -> Result<(), Self::Error> {
        self.0.closed.lock().unwrap().insert(channel);
        self.0.changed.notify_one();
        Ok(())
    }

    async fn tcpip_forward(
        &mut self,
        _: &str,
        _: &mut u32,
        _: &mut server::Session,
    ) -> Result<bool, Self::Error> {
        self.0.remote_requested.notify_one();
        self.0.remote_release.notified().await;
        self.0.remote_enabled.store(true, Ordering::SeqCst);
        Ok(true)
    }

    async fn cancel_tcpip_forward(
        &mut self,
        _: &str,
        _: u32,
        _: &mut server::Session,
    ) -> Result<bool, Self::Error> {
        self.0.remote_enabled.store(false, Ordering::SeqCst);
        self.0.remote_cancelled.notify_one();
        Ok(true)
    }
}

struct Client;
impl client::Handler for Client {
    type Error = russh::Error;

    async fn check_server_key(&mut self, _: &PublicKeyOrCertificate) -> Result<bool, Self::Error> {
        Ok(true)
    }
}

struct Fixture {
    peer: Arc<Peer>,
    shell: Channel<client::Msg>,
    sftp: mpsc::Sender<OpenRequest>,
    forwards: mpsc::Sender<ForwardRequest>,
    tunnel: mpsc::Sender<TcpRequest>,
    resources: ResourceClient,
    stop: CancellationToken,
    broker: JoinHandle<()>,
    server: JoinHandle<()>,
}

impl Fixture {
    async fn new(tunnel_mode: bool) -> Self {
        let peer = Arc::new(Peer::default());
        let key = PrivateKey::random(&mut rand::rng(), Algorithm::Ed25519).unwrap();
        let config = Arc::new(server::Config {
            keys: vec![key],
            auth_rejection_time: Duration::ZERO,
            auth_rejection_time_initial: Some(Duration::ZERO),
            ..Default::default()
        });
        let (local, remote) = tokio::io::duplex(64 * 1024);
        let server_peer = peer.clone();
        let server = tokio::spawn(async move {
            let connection = server::run_stream(config, remote, Server(server_peer))
                .await
                .unwrap();
            let _ = connection.await;
        });
        let mut handle = client::connect_stream(Arc::new(client::Config::default()), local, Client)
            .await
            .unwrap();
        assert!(
            handle
                .authenticate_password("test", "test")
                .await
                .unwrap()
                .success()
        );
        let shell = handle.channel_open_session().await.unwrap();
        let (sftp, sftp_rx) = mpsc::channel(16);
        let (forwards, forwards_rx) = mpsc::channel(32);
        let (tunnel, tunnel_rx) = mpsc::channel(32);
        let stop = CancellationToken::new();
        let (resources, resources_rx) = ResourceClient::new(stop.clone());
        let cancelled = stop.clone();
        let broker = tokio::spawn(async move {
            if tunnel_mode {
                run_tunnel(&handle, tunnel_rx, &cancelled, Duration::from_millis(40)).await;
            } else {
                // A shared handle exercises each broker's request semantics;
                // transport isolation is tested with distinct connections.
                tokio::join!(
                    run_shell(&handle, sftp_rx, resources_rx, &cancelled),
                    run_forwarding(&handle, forwards_rx, &cancelled, Duration::from_millis(40)),
                );
            }
            let _ = handle
                .disconnect(russh::Disconnect::ByApplication, "", "")
                .await;
            let _ = handle.await;
        });
        Self {
            peer,
            shell,
            sftp,
            forwards,
            tunnel,
            resources,
            stop,
            broker,
            server,
        }
    }

    async fn open_forward(&self, host: &str) -> Result<russh::ChannelStream<client::Msg>, String> {
        let (reply, receive) = oneshot::channel();
        self.forwards
            .send(ForwardRequest::Open(TcpRequest {
                host: host.into(),
                port: 22,
                reply,
            }))
            .await
            .unwrap();
        receive.await.unwrap().map_err(|(_, message)| message)
    }

    async fn remote_forward(&self) -> Result<(), String> {
        let (reply, receive) = oneshot::channel();
        self.forwards
            .send(ForwardRequest::Remote {
                rule: PortForward {
                    kind: ForwardKind::Remote,
                    bind_host: "127.0.0.1".into(),
                    bind_port: 2222,
                    destination_host: "127.0.0.1".into(),
                    destination_port: 22,
                    enabled: true,
                },
                enabled: true,
                reply,
            })
            .await
            .unwrap();
        receive.await.unwrap()
    }

    async fn echo(&mut self) {
        self.shell.data(&b"shell remains usable"[..]).await.unwrap();
        loop {
            match self.shell.wait().await {
                Some(ChannelMsg::Data { data }) => {
                    assert_eq!(data.as_ref(), b"shell remains usable");
                    break;
                }
                Some(ChannelMsg::WindowAdjusted { .. }) => {}
                other => panic!("shell closed during a channel request: {other:?}"),
            }
        }
    }

    async fn release_held(&self) {
        let held = std::mem::take(&mut *self.peer.held.lock().unwrap());
        let ids: Vec<_> = held.iter().map(|(channel, _)| channel.id()).collect();
        for (_channel, reply) in held {
            reply.accept().await;
        }
        loop {
            let changed = self.peer.changed.notified();
            if ids
                .iter()
                .all(|id| self.peer.closed.lock().unwrap().contains(id))
            {
                break;
            }
            changed.await;
        }
    }

    async fn finish(self) {
        self.stop.cancel();
        self.broker.await.unwrap();
        self.server.await.unwrap();
    }
}

#[tokio::test]
async fn stalled_sftp_opens_are_bounded_and_do_not_block_forwarding_or_the_shell() {
    tokio::time::timeout(Duration::from_secs(3), async {
        let mut fixture = Fixture::new(false).await;
        fixture.peer.hold_sftp.store(true, Ordering::SeqCst);
        let mut receivers = Vec::new();
        for _ in 0..MAX_SFTP_OPENS {
            let (reply, receive) = oneshot::channel();
            fixture
                .sftp
                .send(OpenRequest {
                    reply,
                    cancel: CancellationToken::new(),
                })
                .await
                .unwrap();
            receivers.push(receive);
        }
        loop {
            let changed = fixture.peer.changed.notified();
            if fixture.peer.held.lock().unwrap().len() == MAX_SFTP_OPENS {
                break;
            }
            changed.await;
        }
        let (reply, receive) = oneshot::channel();
        fixture
            .sftp
            .send(OpenRequest {
                reply,
                cancel: CancellationToken::new(),
            })
            .await
            .unwrap();
        assert!(matches!(receive.await.unwrap(), Err(SftpError::Busy)));
        let mut sample = Box::pin(fixture.resources.sample(false));
        loop {
            let changed = fixture.peer.changed.notified();
            if fixture.peer.held.lock().unwrap().len() == MAX_SFTP_OPENS + 1 {
                break;
            }
            tokio::select! {
                result = &mut sample => panic!("resource OPEN shared the SFTP limit: {result:?}"),
                _ = changed => {},
            }
        }
        drop(sample);
        drop(fixture.open_forward("ready.example").await.unwrap());
        fixture.echo().await;
        drop(receivers);
        fixture.release_held().await;
        fixture.peer.hold_sftp.store(false, Ordering::SeqCst);
        let (reply, receive) = oneshot::channel();
        fixture
            .sftp
            .send(OpenRequest {
                reply,
                cancel: CancellationToken::new(),
            })
            .await
            .unwrap();
        drop(receive.await.unwrap().unwrap());
        fixture.finish().await;
    })
    .await
    .unwrap();
}

#[tokio::test]
async fn resource_exec_cancellation_preserves_the_shell_and_allows_resampling() {
    tokio::time::timeout(Duration::from_secs(3), async {
        let mut fixture = Fixture::new(false).await;
        fixture.peer.stall_exec.store(true, Ordering::SeqCst);
        let client = fixture.resources.clone();
        let mut sample = Box::pin(client.sample(false));
        tokio::select! {
            result = &mut sample => panic!("exec was not held: {result:?}"),
            _ = fixture.peer.exec_started.notified() => {},
        }
        assert!(matches!(client.sample(false).await, Err(ResourceError::Busy)));
        drop(sample);
        loop {
            let changed = fixture.peer.changed.notified();
            if !fixture.peer.closed.lock().unwrap().is_empty() { break; }
            changed.await;
        }
        fixture.echo().await;
        fixture.peer.stall_exec.store(false, Ordering::SeqCst);
        *fixture.peer.resource_output.lock().unwrap() = b"RTRES\t1\nBOOT\t01234567-89ab-cdef-0123-456789abcdef\nUPTIME\t120.0\nLOAD\t0.2\t0.3\t0.4\nCPU\t2\t10\t0\t0\t90\t0\t0\t0\t0\nMEM\t1000\t400\t100\t25\nNET\teth0\t1024\t2048\nDISKS\nEND\n".to_vec();
        let snapshot = client.sample(true).await.unwrap();
        assert_eq!(snapshot.memory.total_bytes, 1_024_000);
        assert!(snapshot.cpu_percent.is_none());
        assert!(snapshot.disks_sampled_at.is_some());
        assert_eq!(fixture.peer.exec_commands.lock().unwrap().as_slice(), [b"sh -s -- 0".to_vec(), b"sh -s -- 1".to_vec()]);
        assert!(fixture.peer.resource_script.lock().unwrap().windows(b"/proc/stat".len()).any(|bytes| bytes == b"/proc/stat"));
        fixture.finish().await;
        client.closed().await;
        assert!(matches!(client.sample(false).await, Err(ResourceError::Disconnected)));
    }).await.unwrap();
}

#[tokio::test]
async fn resource_output_limit_and_nonzero_exit_do_not_disconnect_the_shell() {
    tokio::time::timeout(Duration::from_secs(3), async {
        let mut fixture = Fixture::new(false).await;
        *fixture.peer.resource_output.lock().unwrap() = vec![b'x'; 128 * 1024 + 1];
        assert!(matches!(
            fixture.resources.sample(false).await,
            Err(ResourceError::OutputLimit)
        ));
        fixture.echo().await;
        fixture.peer.resource_output.lock().unwrap().clear();
        fixture.peer.resource_exit.store(17, Ordering::SeqCst);
        assert!(matches!(
            fixture.resources.sample(false).await,
            Err(ResourceError::ExitStatus(17))
        ));
        assert!(!fixture.stop.is_cancelled());
        fixture.finish().await;
    })
    .await
    .unwrap();
}

#[tokio::test]
async fn forwarding_timeout_keeps_the_shell_alive_and_closes_late_confirmation() {
    tokio::time::timeout(Duration::from_secs(3), async {
        let mut fixture = Fixture::new(false).await;
        assert!(fixture.open_forward("held.example").await.is_err());
        assert!(!fixture.stop.is_cancelled());
        fixture.echo().await;
        fixture.release_held().await;
        drop(fixture.open_forward("ready.example").await.unwrap());
        fixture.finish().await;
    })
    .await
    .unwrap();
}

#[tokio::test]
async fn late_remote_listener_is_cancelled_before_the_endpoint_can_be_reused() {
    tokio::time::timeout(Duration::from_secs(3), async {
        let mut fixture = Fixture::new(false).await;
        assert!(fixture.remote_forward().await.is_err());
        assert_eq!(fixture.remote_forward().await.unwrap_err(), busy());
        assert!(!fixture.stop.is_cancelled());
        fixture.peer.remote_release.notify_one();
        fixture.peer.remote_cancelled.notified().await;
        assert!(!fixture.peer.remote_enabled.load(Ordering::SeqCst));
        fixture.echo().await;
        fixture.peer.remote_release.notify_one();
        fixture.remote_forward().await.unwrap();
        assert!(fixture.peer.remote_enabled.load(Ordering::SeqCst));
        fixture.finish().await;
    })
    .await
    .unwrap();
}

#[tokio::test]
async fn stalled_tunnel_open_does_not_block_or_disconnect_other_destinations() {
    use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};

    tokio::time::timeout(Duration::from_secs(3), async {
        let fixture = Fixture::new(true).await;
        let (reply, stalled) = oneshot::channel();
        fixture
            .tunnel
            .send(TcpRequest {
                host: "held.example".into(),
                port: 22,
                reply,
            })
            .await
            .unwrap();
        let (reply, ready) = oneshot::channel();
        fixture
            .tunnel
            .send(TcpRequest {
                host: "ready.example".into(),
                port: 22,
                reply,
            })
            .await
            .unwrap();
        let mut stream = ready.await.unwrap().unwrap();
        assert!(matches!(
            stalled.await.unwrap(),
            Err((FailureKind::Timeout, _))
        ));
        assert!(!fixture.stop.is_cancelled());
        stream.write_all(b"still connected").await.unwrap();
        let mut echo = [0; 15];
        stream.read_exact(&mut echo).await.unwrap();
        assert_eq!(&echo, b"still connected");
        fixture.release_held().await;
        drop(stream);
        fixture.finish().await;
    })
    .await
    .unwrap();
}
