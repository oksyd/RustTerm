//! A real in-process SSH server exercises trust, authentication and terminal I/O.
use russh::{
    Channel, ChannelId, Pty,
    keys::{Algorithm, PrivateKey, PublicKey, ssh_key::LineEnding},
    server::{self, VerifiedPublicKey},
};
use rustterm::config::KnownHosts;
use rustterm::session::{Command, FailureKind, Session, SessionState, SshAuth, SshOptions};
use std::{
    path::Path,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    },
    time::{Duration, Instant},
};
use tokio::{net::TcpListener, sync::Notify};

const BACKPRESSURE_INPUTS: usize = 19;

#[derive(Default)]
struct Backpressure {
    first_input: Notify,
    release_output: Notify,
    all_inputs: Notify,
}

#[derive(Clone)]
struct Handler {
    authentications: Arc<AtomicUsize>,
    allowed_key: Option<PublicKey>,
    received: Arc<Mutex<Vec<u8>>>,
    exec_commands: Arc<Mutex<Vec<String>>>,
    reject_exec: Arc<AtomicBool>,
    bridged: std::collections::HashSet<ChannelId>,
    remote_listeners: Arc<Mutex<std::collections::BTreeMap<u32, tokio::task::JoinHandle<()>>>>,
    backpressure: Option<Arc<Backpressure>>,
}
impl server::Handler for Handler {
    type Error = russh::Error;
    async fn auth_password(
        &mut self,
        user: &str,
        password: &str,
    ) -> Result<server::Auth, Self::Error> {
        self.authentications.fetch_add(1, Ordering::SeqCst);
        Ok(if user == "alice" && password == "secret" {
            server::Auth::Accept
        } else {
            server::Auth::reject()
        })
    }
    async fn auth_keyboard_interactive<'a>(
        &'a mut self,
        user: &str,
        _submethods: &str,
        response: Option<server::Response<'a>>,
    ) -> Result<server::Auth, Self::Error> {
        self.authentications.fetch_add(1, Ordering::SeqCst);
        Ok(match response {
            Some(mut response) => {
                if user == "alice"
                    && response
                        .next()
                        .is_some_and(|answer| answer.as_ref() == b"123456")
                {
                    server::Auth::Accept
                } else {
                    server::Auth::reject()
                }
            }
            None => server::Auth::Partial {
                name: "MFA".into(),
                instructions: "Enter the verification code".into(),
                prompts: vec![("Code".into(), false)].into(),
            },
        })
    }
    async fn channel_open_direct_tcpip(
        &mut self,
        channel: Channel<server::Msg>,
        host: &str,
        port: u32,
        _originator: &str,
        _originator_port: u32,
        reply: server::ChannelOpenHandle,
        _session: &mut server::Session,
    ) -> Result<(), Self::Error> {
        if host == "127.0.0.1" {
            let mut target = tokio::net::TcpStream::connect((host, port as u16))
                .await
                .unwrap();
            self.bridged.insert(channel.id());
            reply.accept().await;
            tokio::spawn(async move {
                let mut stream = channel.into_stream();
                let _ = tokio::io::copy_bidirectional(&mut stream, &mut target).await;
            });
        } else {
            reply.accept().await;
        }
        Ok(())
    }
    async fn tcpip_forward(
        &mut self,
        address: &str,
        port: &mut u32,
        session: &mut server::Session,
    ) -> Result<bool, Self::Error> {
        let listener = TcpListener::bind((address, *port as u16)).await.unwrap();
        *port = u32::from(listener.local_addr().unwrap().port());
        let port = *port;
        let address = address.to_owned();
        let handle = session.handle();
        let task = tokio::spawn(async move {
            let mut streams = tokio::task::JoinSet::new();
            loop {
                tokio::select! {
                    incoming=listener.accept()=>{
                        let Ok((mut stream,origin))=incoming else{break};let handle=handle.clone();let address=address.clone();
                        streams.spawn(async move {let channel=handle.channel_open_forwarded_tcpip(address,port,origin.ip().to_string(),u32::from(origin.port())).await.unwrap();let mut channel=channel.into_stream();let _=tokio::io::copy_bidirectional(&mut stream,&mut channel).await;});
                    },
                    _=streams.join_next(),if !streams.is_empty()=>{},
                }
            }
        });
        self.remote_listeners.lock().unwrap().insert(port, task);
        Ok(true)
    }
    async fn cancel_tcpip_forward(
        &mut self,
        _address: &str,
        port: u32,
        _session: &mut server::Session,
    ) -> Result<bool, Self::Error> {
        if let Some(task) = self.remote_listeners.lock().unwrap().remove(&port) {
            task.abort();
        }
        Ok(true)
    }
    async fn channel_open_confirmation(
        &mut self,
        id: ChannelId,
        _max_packet_size: u32,
        _window_size: u32,
        _session: &mut server::Session,
    ) -> Result<(), Self::Error> {
        self.bridged.insert(id);
        Ok(())
    }
    async fn auth_publickey(
        &mut self,
        user: &str,
        key: VerifiedPublicKey<'_>,
    ) -> Result<server::Auth, Self::Error> {
        self.authentications.fetch_add(1, Ordering::SeqCst);
        Ok(
            if user == "alice" && self.allowed_key.as_ref() == Some(key.public_key) {
                server::Auth::Accept
            } else {
                server::Auth::reject()
            },
        )
    }
    async fn channel_open_session(
        &mut self,
        _channel: Channel<server::Msg>,
        reply: server::ChannelOpenHandle,
        _session: &mut server::Session,
    ) -> Result<(), Self::Error> {
        reply.accept().await;
        Ok(())
    }
    async fn pty_request(
        &mut self,
        channel: ChannelId,
        _term: &str,
        _cols: u32,
        _rows: u32,
        _pw: u32,
        _ph: u32,
        _modes: &[(Pty, u32)],
        session: &mut server::Session,
    ) -> Result<(), Self::Error> {
        // These notifications may arrive before a pending request's reply.
        session.xon_xoff_request(channel, true)?;
        session.channel_success(channel)?;
        Ok(())
    }
    async fn shell_request(
        &mut self,
        channel: ChannelId,
        session: &mut server::Session,
    ) -> Result<(), Self::Error> {
        session.data(channel, b"EARLY\r\n".to_vec())?;
        session.xon_xoff_request(channel, false)?;
        session.channel_success(channel)?;
        session.data(channel, b"READY\r\n".to_vec())?;
        Ok(())
    }
    async fn exec_request(
        &mut self,
        channel: ChannelId,
        command: &[u8],
        session: &mut server::Session,
    ) -> Result<(), Self::Error> {
        self.exec_commands
            .lock()
            .unwrap()
            .push(String::from_utf8(command.to_vec()).unwrap());
        if self.reject_exec.load(Ordering::SeqCst) {
            session.channel_failure(channel)?;
        } else {
            session.xon_xoff_request(channel, true)?;
            session.channel_success(channel)?;
            session.data(channel, b"INTEGRATED\r\n".to_vec())?;
        }
        Ok(())
    }
    async fn window_change_request(
        &mut self,
        channel: ChannelId,
        cols: u32,
        rows: u32,
        _pw: u32,
        _ph: u32,
        session: &mut server::Session,
    ) -> Result<(), Self::Error> {
        session.data(channel, format!("SIZE {cols} {rows}\r\n").into_bytes())?;
        Ok(())
    }
    async fn data(
        &mut self,
        channel: ChannelId,
        data: &[u8],
        session: &mut server::Session,
    ) -> Result<(), Self::Error> {
        if self.bridged.contains(&channel) {
            return Ok(());
        }
        let received = {
            let mut received = self.received.lock().unwrap();
            received.extend_from_slice(data);
            received.len()
        };
        if let Some(backpressure) = &self.backpressure {
            if received == 1 {
                backpressure.first_input.notify_one();
                backpressure.release_output.notified().await;
                // The initial two-byte receive window still has one byte left.
                // Its refill is emitted when the next input is read, strictly
                // after this burst. Exceed both bounded client output queues.
                let mut packet = vec![b'.'; 1022];
                packet.extend_from_slice(b"\r\n");
                for _ in 0..70 {
                    session.data(channel, packet.clone())?;
                }
                session.data(channel, b"BIDIRECTIONAL-OUTPUT\r\n".to_vec())?;
            }
            if received == BACKPRESSURE_INPUTS {
                backpressure.all_inputs.notify_one();
            }
            return Ok(());
        }
        if data == b"exit-only" {
            session.data(channel, b"TAIL-ONLY\r\n".to_vec())?;
            session.exit_status_request(channel, 7)?;
        } else if data == b"finish" {
            session.data(channel, b"TAIL\r\n".to_vec())?;
            session.eof(channel)?;
            session.exit_status_request(channel, 7)?;
            session.close(channel)?;
        } else {
            session.data(channel, data.to_vec())?;
        }
        Ok(())
    }
}
struct Fixture {
    port: u16,
    host_key: PrivateKey,
    handler: Handler,
    task: tokio::task::JoinHandle<()>,
}
impl Fixture {
    async fn new(allowed_key: Option<PublicKey>) -> Self {
        Self::with_backpressure(allowed_key, None).await
    }

    async fn with_backpressure(
        allowed_key: Option<PublicKey>,
        backpressure: Option<Arc<Backpressure>>,
    ) -> Self {
        let host_key = PrivateKey::random(&mut rand::rng(), Algorithm::Ed25519).unwrap();
        let listener = TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let handler = Handler {
            authentications: Arc::new(AtomicUsize::new(0)),
            allowed_key,
            received: Arc::new(Mutex::new(Vec::new())),
            exec_commands: Arc::new(Mutex::new(Vec::new())),
            reject_exec: Arc::new(AtomicBool::new(false)),
            bridged: std::collections::HashSet::new(),
            remote_listeners: Arc::new(Mutex::new(std::collections::BTreeMap::new())),
            backpressure,
        };
        let mut config = server::Config {
            keys: vec![host_key.clone()],
            auth_rejection_time: Duration::ZERO,
            auth_rejection_time_initial: Some(Duration::ZERO),
            ..Default::default()
        };
        if handler.backpressure.is_some() {
            config.window_size = 2;
        }
        let config = Arc::new(config);
        let handlers = handler.clone();
        let task = tokio::spawn(async move {
            while let Ok((stream, _)) = listener.accept().await {
                let config = config.clone();
                let handler = handlers.clone();
                tokio::spawn(async move {
                    if let Ok(session) = server::run_stream(config, stream, handler).await {
                        let _ = session.await;
                    }
                });
            }
        });
        Self {
            port,
            host_key,
            handler,
            task,
        }
    }
    fn options(&self, path: &Path, auth: SshAuth) -> SshOptions {
        SshOptions {
            host: "127.0.0.1".into(),
            port: self.port,
            username: "alice".into(),
            auth,
            known_hosts_path: path.into(),
            timeout_secs: 5,
            keepalive_secs: 1,
            keepalive_max: 2,
            shell_integration: false,
            jump_hosts: Vec::new(),
            forwards: Vec::new(),
            authentication: None,
        }
    }
    fn trust(&self, path: &Path) {
        let public = self.host_key.public_key();
        let mut hosts = KnownHosts::default();
        hosts
            .trust(
                "127.0.0.1",
                self.port,
                public.algorithm().as_ref(),
                &public.to_openssh().unwrap(),
            )
            .unwrap();
        hosts.save(path).unwrap();
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        for task in
            std::mem::take(&mut *self.handler.remote_listeners.lock().unwrap()).into_values()
        {
            task.abort();
        }
        self.task.abort();
    }
}
async fn until(mut check: impl FnMut() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(10);
    while !check() {
        assert!(Instant::now() < deadline, "SSH condition timed out");
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}
async fn text(session: &Session) -> String {
    let count = session.snapshot_count();
    until(|| session.request_snapshot()).await;
    until(|| session.snapshot_count() > count).await;
    session
        .snapshot()
        .lines
        .iter()
        .map(|line| {
            line.cells
                .iter()
                .map(|cell| cell.text.as_str())
                .collect::<String>()
        })
        .collect::<Vec<_>>()
        .join("\n")
}

#[tokio::test]
async fn shell_integration_starts_with_exec_without_injecting_terminal_input() {
    let server = Fixture::new(None).await;
    let directory = private_directory();
    let path = directory.path().join("hosts.toml");
    server.trust(&path);
    let mut options = server.options(&path, SshAuth::Password("secret".into()));
    options.shell_integration = true;
    let session = Session::spawn_ssh(options, 80, 24, 100).unwrap();
    until(|| session.state() == SessionState::Connected).await;
    assert_eq!(
        server.handler.exec_commands.lock().unwrap().as_slice(),
        [rustterm::terminal::integration::remote_command()]
    );
    assert!(server.handler.received.lock().unwrap().is_empty());
    session
        .send(Command::Input(b"user input".to_vec()))
        .unwrap();
    until(|| server.handler.received.lock().unwrap().as_slice() == b"user input").await;
    session.close();
}

#[tokio::test]
async fn shell_integration_rejection_is_explicit_and_plain_shell_can_be_selected() {
    let server = Fixture::new(None).await;
    let directory = private_directory();
    let path = directory.path().join("hosts.toml");
    server.trust(&path);
    server.handler.reject_exec.store(true, Ordering::SeqCst);
    let mut options = server.options(&path, SshAuth::Password("secret".into()));
    options.shell_integration = true;
    let rejected = Session::spawn_ssh(options.clone(), 80, 24, 100).unwrap();
    until(|| matches!(rejected.state(), SessionState::Failed { .. })).await;
    assert!(matches!(
        rejected.state(),
        SessionState::Failed {
            kind: FailureKind::Protocol,
            ..
        }
    ));
    assert!(server.handler.received.lock().unwrap().is_empty());
    rejected.close();
    options.shell_integration = false;
    let plain = Session::spawn_ssh(options, 80, 24, 100).unwrap();
    until(|| plain.state() == SessionState::Connected).await;
    assert_eq!(server.handler.exec_commands.lock().unwrap().len(), 1);
    assert!(text(&plain).await.contains("READY"));
    plain.close();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn first_host_requires_confirmation_before_password_and_preserves_exit_tail() {
    let server = Fixture::new(None).await;
    let directory = private_directory();
    let path = directory.path().join("hosts.toml");
    let session = Session::spawn_ssh(
        server.options(&path, SshAuth::Password("secret".into())),
        80,
        24,
        100,
    )
    .unwrap();
    until(|| matches!(session.state(), SessionState::WaitingHostKey { .. })).await;
    if let SessionState::WaitingHostKey {
        algorithm,
        fingerprint,
        ..
    } = session.state()
    {
        assert_eq!(algorithm, "ssh-ed25519");
        assert!(fingerprint.starts_with("SHA256:"));
    }
    tokio::time::sleep(Duration::from_millis(1200)).await;
    assert_eq!(server.handler.authentications.load(Ordering::SeqCst), 0);
    assert!(!path.exists());
    session.send(Command::TrustHostKey(true)).unwrap();
    until(|| session.state() == SessionState::Connected).await;
    assert!(path.exists());
    session
        .send(Command::Resize {
            columns: 111,
            rows: 31,
        })
        .unwrap();
    session.send(Command::Input(b"hello\r\n".to_vec())).unwrap();
    session.send(Command::Input(b"finish".to_vec())).unwrap();
    until(|| session.state() == SessionState::Disconnected { exit_code: Some(7) }).await;
    let text = text(&session).await;
    assert!(text.contains("SIZE 111 31"), "{text}");
    assert!(text.contains("hello"), "{text}");
    assert!(text.contains("TAIL"), "{text}");
    assert!(session.is_read_only());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn changed_host_key_is_rejected_without_sending_credentials() {
    let server = Fixture::new(None).await;
    let directory = private_directory();
    let path = directory.path().join("hosts.toml");
    let different = PrivateKey::random(&mut rand::rng(), Algorithm::Ed25519).unwrap();
    let mut hosts = KnownHosts::default();
    hosts
        .trust(
            "127.0.0.1",
            server.port,
            "ssh-ed25519",
            &different.public_key().to_openssh().unwrap(),
        )
        .unwrap();
    hosts.save(&path).unwrap();
    let session = Session::spawn_ssh(
        server.options(&path, SshAuth::Password("secret".into())),
        80,
        24,
        100,
    )
    .unwrap();
    until(|| {
        matches!(
            session.state(),
            SessionState::Failed {
                kind: FailureKind::HostKeyMismatch,
                ..
            }
        )
    })
    .await;
    assert_eq!(server.handler.authentications.load(Ordering::SeqCst), 0);
    assert!(session.send(Command::TrustHostKey(true)).is_err());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn rejection_and_cancellation_never_send_authentication() {
    let server = Fixture::new(None).await;
    let directory = private_directory();
    let path = directory.path().join("hosts.toml");
    for accept in [false, true] {
        let session = Session::spawn_ssh(
            server.options(&path, SshAuth::Password("secret".into())),
            80,
            24,
            100,
        )
        .unwrap();
        until(|| matches!(session.state(), SessionState::WaitingHostKey { .. })).await;
        if accept {
            session.disconnect();
            until(|| matches!(session.state(), SessionState::Disconnected { .. })).await;
        } else {
            session.send(Command::TrustHostKey(false)).unwrap();
            until(|| {
                matches!(
                    session.state(),
                    SessionState::Failed {
                        kind: FailureKind::HostKeyRejected,
                        ..
                    }
                )
            })
            .await;
        }
    }
    assert_eq!(server.handler.authentications.load(Ordering::SeqCst), 0);
    assert!(!path.exists());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn authentication_failure_is_distinct_and_secret_is_not_in_error() {
    let server = Fixture::new(None).await;
    let directory = private_directory();
    let path = directory.path().join("hosts.toml");
    server.trust(&path);
    let session = Session::spawn_ssh(
        server.options(&path, SshAuth::Password("wrong-secret-marker".into())),
        80,
        24,
        100,
    )
    .unwrap();
    until(|| {
        matches!(
            session.state(),
            SessionState::Failed {
                kind: FailureKind::Authentication,
                ..
            }
        )
    })
    .await;
    assert_eq!(server.handler.authentications.load(Ordering::SeqCst), 1);
    assert!(!format!("{:?}", session.state()).contains("wrong-secret-marker"));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn encrypted_private_key_authenticates_and_explicit_disconnect_freezes() {
    let private = PrivateKey::random(&mut rand::rng(), Algorithm::Ed25519).unwrap();
    let server = Fixture::new(Some(private.public_key().clone())).await;
    let directory = private_directory();
    let path = directory.path().join("hosts.toml");
    let key_path = directory.path().join("id_ed25519");
    let encrypted = private.encrypt(&mut rand::rng(), "key-passphrase").unwrap();
    std::fs::write(
        &key_path,
        encrypted.to_openssh(LineEnding::LF).unwrap().as_bytes(),
    )
    .unwrap();
    server.trust(&path);
    let session = Session::spawn_ssh(
        server.options(
            &path,
            SshAuth::PrivateKey {
                path: key_path,
                passphrase: Some("key-passphrase".into()),
            },
        ),
        80,
        24,
        100,
    )
    .unwrap();
    until(|| session.state() == SessionState::Connected).await;
    until(|| server.handler.authentications.load(Ordering::SeqCst) == 1).await;
    session.disconnect();
    until(|| {
        matches!(session.state(), SessionState::Disconnected { .. }) && session.is_read_only()
    })
    .await;
    assert!(
        session
            .send(Command::Input(b"never-sent".to_vec()))
            .is_err()
    );
    assert!(text(&session).await.contains("READY"));
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

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn stalled_handshake_times_out_and_can_be_cancelled() {
    let listener = TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let directory = private_directory();
    let options = SshOptions {
        host: "127.0.0.1".into(),
        port,
        username: "alice".into(),
        auth: SshAuth::Password("unused".into()),
        known_hosts_path: directory.path().join("hosts.toml"),
        timeout_secs: 1,
        keepalive_secs: 1,
        keepalive_max: 2,
        shell_integration: false,
        jump_hosts: Vec::new(),
        forwards: Vec::new(),
        authentication: None,
    };
    let server = tokio::spawn(async move {
        let mut streams = Vec::new();
        while let Ok((stream, _)) = listener.accept().await {
            streams.push(stream);
        }
    });
    let timeout = Session::spawn_ssh(options.clone(), 80, 24, 100).unwrap();
    until(|| {
        matches!(
            timeout.state(),
            SessionState::Failed {
                kind: FailureKind::Timeout,
                ..
            }
        )
    })
    .await;
    let cancelled = Session::spawn_ssh(options, 80, 24, 100).unwrap();
    cancelled.disconnect();
    until(|| cancelled.is_read_only()).await;
    assert!(matches!(
        cancelled.state(),
        SessionState::Disconnected { .. }
    ));
    server.abort();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn malformed_known_hosts_fails_closed_before_authentication() {
    let server = Fixture::new(None).await;
    let directory = private_directory();
    let path = directory.path().join("hosts.toml");
    std::fs::write(&path, "this is not valid trust data").unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
    }
    let session = Session::spawn_ssh(
        server.options(&path, SshAuth::Password("secret".into())),
        80,
        24,
        100,
    )
    .unwrap();
    until(|| {
        matches!(
            session.state(),
            SessionState::Failed {
                kind: FailureKind::KnownHosts,
                ..
            }
        )
    })
    .await;
    assert_eq!(server.handler.authentications.load(Ordering::SeqCst), 0);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn incomplete_exit_status_is_bounded_and_preserves_tail() {
    let server = Fixture::new(None).await;
    let directory = private_directory();
    let path = directory.path().join("hosts.toml");
    server.trust(&path);
    let session = Session::spawn_ssh(
        server.options(&path, SshAuth::Password("secret".into())),
        80,
        24,
        100,
    )
    .unwrap();
    until(|| session.state() == SessionState::Connected).await;
    session.send(Command::Input(b"exit-only".to_vec())).unwrap();
    until(|| {
        matches!(
            session.state(),
            SessionState::Failed {
                kind: FailureKind::Timeout,
                ..
            }
        )
    })
    .await;
    until(|| session.is_read_only()).await;
    assert!(text(&session).await.contains("TAIL-ONLY"));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn simultaneous_input_backpressure_and_output_preserve_window_adjustments() {
    tokio::time::timeout(Duration::from_secs(5), async {
        let backpressure = Arc::new(Backpressure::default());
        let server = Fixture::with_backpressure(None, Some(backpressure.clone())).await;
        let directory = private_directory();
        let path = directory.path().join("hosts");
        server.trust(&path);
        let mut options = server.options(&path, SshAuth::Password("secret".into()));
        options.keepalive_secs = 0;
        let session = Session::spawn_ssh(options, 80, 24, 100).unwrap();
        until(|| session.state() == SessionState::Connected).await;
        session.send(Command::Input(vec![b'x'])).unwrap();
        backpressure.first_input.notified().await;
        for _ in 1..BACKPRESSURE_INPUTS {
            session.send(Command::Input(vec![b'x'])).unwrap();
        }
        // Two bytes use the peer window; the next write waits while 16 writes
        // fill the transport queue. The resize receipt proves all preceding
        // input was processed, without using a sleep to guess queue occupancy.
        let resize = session.resize(81, 24).unwrap();
        until(|| resize.is_complete()).await;
        backpressure.release_output.notify_one();
        backpressure.all_inputs.notified().await;
        loop {
            assert_eq!(session.state(), SessionState::Connected);
            let visible = text(&session).await;
            if visible.contains("BIDIRECTIONAL-OUTPUT") && visible.contains("SIZE 81 24") {
                break;
            }
        }
        assert_eq!(
            server.handler.received.lock().unwrap().as_slice(),
            vec![b'x'; BACKPRESSURE_INPUTS],
        );
        session.close();
    })
    .await
    .unwrap();
}

#[tokio::test]
async fn keyboard_interactive_uses_secret_challenge_responses() {
    tokio::time::timeout(Duration::from_secs(5), async {
        let server = Fixture::new(None).await;
        let directory = private_directory();
        let path = directory.path().join("hosts");
        server.trust(&path);
        let (sender, mut receiver) = tokio::sync::mpsc::channel(1);
        let mut options = server.options(&path, SshAuth::KeyboardInteractive);
        options.authentication = Some(sender);
        let session = Session::spawn_ssh(options, 80, 24, 100).unwrap();
        let challenge = receiver.recv().await.unwrap();
        assert_eq!(challenge.name, "MFA");
        assert_eq!(challenge.prompts.len(), 1);
        assert!(!challenge.prompts[0].echo);
        assert!(
            challenge
                .reply
                .send(vec![rustterm::config::Secret::new("123456".into())])
                .is_ok()
        );
        until(|| session.state() == SessionState::Connected).await;
        assert_eq!(server.handler.authentications.load(Ordering::SeqCst), 2);
        session.close();
    })
    .await
    .unwrap();
}

#[tokio::test]
async fn local_and_socks_forwarding_relay_and_stop_existing_streams() {
    use rustterm::{
        config::{ForwardKind, PortForward},
        session::ForwardState,
    };
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    tokio::time::timeout(Duration::from_secs(5), async {
        let server = Fixture::new(None).await;
        let directory = private_directory();
        let path = directory.path().join("hosts");
        server.trust(&path);
        // Bind both reservations until the last moment to avoid duplicate ephemeral ports.
        let local = TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
        let dynamic = TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
        let local_port = local.local_addr().unwrap().port();
        let dynamic_port = dynamic.local_addr().unwrap().port();
        let mut options = server.options(&path, SshAuth::Password("secret".into()));
        options.forwards = vec![
            PortForward {
                kind: ForwardKind::Local,
                bind_host: "127.0.0.1".into(),
                bind_port: local_port,
                destination_host: "target".into(),
                destination_port: 80,
                enabled: true,
            },
            PortForward {
                kind: ForwardKind::Dynamic,
                bind_host: "127.0.0.1".into(),
                bind_port: dynamic_port,
                destination_host: String::new(),
                destination_port: 0,
                enabled: true,
            },
        ];
        drop((local, dynamic));
        let session = Session::spawn_ssh(options, 80, 24, 100).unwrap();
        until(|| {
            session.forward_statuses().len() == 2
                && session
                    .forward_statuses()
                    .iter()
                    .all(|rule| rule.state == ForwardState::Running)
        })
        .await;
        for (index, port) in [local_port, dynamic_port].into_iter().enumerate() {
            let mut stream = tokio::net::TcpStream::connect(("127.0.0.1", port))
                .await
                .unwrap();
            if index == 1 {
                stream.write_all(&[5, 1, 0]).await.unwrap();
                let mut method = [0; 2];
                stream.read_exact(&mut method).await.unwrap();
                assert_eq!(method, [5, 0]);
                stream
                    .write_all(&[5, 1, 0, 3, 6, b't', b'a', b'r', b'g', b'e', b't', 0, 80])
                    .await
                    .unwrap();
                let mut reply = [0; 10];
                stream.read_exact(&mut reply).await.unwrap();
                assert_eq!(reply[1], 0);
            }
            stream.write_all(b"hello").await.unwrap();
            let mut answer = [0; 5];
            stream.read_exact(&mut answer).await.unwrap();
            assert_eq!(&answer, b"hello");
            session.set_forward_enabled(index, false).unwrap();
            until(|| session.forward_statuses()[index].state == ForwardState::Stopped).await;
            assert_eq!(stream.read(&mut answer).await.unwrap(), 0);
            session.set_forward_enabled(index, true).unwrap();
            until(|| session.forward_statuses()[index].state == ForwardState::Running).await;
            for _ in 0..3 {
                session.set_forward_enabled(index, false).unwrap();
                session.set_forward_enabled(index, true).unwrap();
            }
            tokio::task::yield_now().await;
            until(|| session.forward_statuses()[index].state == ForwardState::Running).await;
        }
        session.close();
    })
    .await
    .unwrap();
}

#[tokio::test]
async fn multiple_jump_hosts_authenticate_each_hop_and_reach_target_shell() {
    tokio::time::timeout(Duration::from_secs(5), async {
        let key = PrivateKey::random(&mut rand::rng(), Algorithm::Ed25519).unwrap();
        let first = Fixture::new(Some(key.public_key().clone())).await;
        let second = Fixture::new(Some(key.public_key().clone())).await;
        let target = Fixture::new(None).await;
        let directory = private_directory();
        let hosts = directory.path().join("hosts");
        let identity = directory.path().join("identity");
        std::fs::write(
            &identity,
            key.to_openssh(LineEnding::LF).unwrap().as_bytes(),
        )
        .unwrap();
        for server in [&first, &second, &target] {
            KnownHosts::trust_and_save(
                &hosts,
                "127.0.0.1",
                server.port,
                "ssh-ed25519",
                &server.host_key.public_key().to_openssh().unwrap(),
            )
            .unwrap();
        }
        let mut options = target.options(&hosts, SshAuth::Password("secret".into()));
        options.jump_hosts = [first.port, second.port]
            .into_iter()
            .map(|port| rustterm::config::SshJumpHost {
                host: "127.0.0.1".into(),
                port,
                username: "alice".into(),
                identity_file: Some(identity.clone()),
                agent_socket: None,
            })
            .collect();
        let session = Session::spawn_ssh(options, 80, 24, 100).unwrap();
        until(|| session.state() == SessionState::Connected).await;
        assert!(text(&session).await.contains("READY"));
        assert_eq!(first.handler.authentications.load(Ordering::SeqCst), 1);
        assert_eq!(second.handler.authentications.load(Ordering::SeqCst), 1);
        assert_eq!(target.handler.authentications.load(Ordering::SeqCst), 1);
        session.close();
    })
    .await
    .unwrap();
}

#[cfg(unix)]
#[tokio::test]
async fn agent_authentication_signs_with_the_selected_socket() {
    tokio::time::timeout(Duration::from_secs(5), async {
        let key = PrivateKey::random(&mut rand::rng(), Algorithm::Ed25519).unwrap();
        let server = Fixture::new(Some(key.public_key().clone())).await;
        let directory = private_directory();
        let hosts = directory.path().join("hosts");
        let socket = directory.path().join("agent");
        server.trust(&hosts);
        let listener = tokio::net::UnixListener::bind(&socket).unwrap();
        let connections = Box::pin(futures_util::stream::unfold(
            listener,
            |listener| async move {
                let (stream, _) = listener.accept().await.ok()?;
                Some((Ok(stream), listener))
            },
        ));
        let agent = tokio::spawn(russh::keys::agent::server::serve(connections, ()));
        let mut admin = russh::keys::agent::client::AgentClient::connect_uds(&socket)
            .await
            .unwrap();
        admin.add_identity(&key, &[]).await.unwrap();
        let session = Session::spawn_ssh(
            server.options(
                &hosts,
                SshAuth::Agent {
                    socket: Some(socket),
                },
            ),
            80,
            24,
            100,
        )
        .unwrap();
        until(|| session.state() == SessionState::Connected).await;
        assert_eq!(server.handler.authentications.load(Ordering::SeqCst), 1);
        session.close();
        agent.abort();
    })
    .await
    .unwrap();
}

#[tokio::test]
async fn remote_forwarding_accepts_registered_routes_and_cancels_listener() {
    use rustterm::{
        config::{ForwardKind, PortForward},
        session::ForwardState,
    };
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    tokio::time::timeout(Duration::from_secs(5), async {
        let server = Fixture::new(None).await;
        let directory = private_directory();
        let path = directory.path().join("hosts");
        server.trust(&path);
        let target = TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
        let target_port = target.local_addr().unwrap().port();
        let reserve = TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
        let remote_port = reserve.local_addr().unwrap().port();
        drop(reserve);
        let echo = tokio::spawn(async move {
            let (stream, _) = target.accept().await.unwrap();
            let (mut read, mut write) = stream.into_split();
            tokio::io::copy(&mut read, &mut write).await.unwrap();
        });
        let mut options = server.options(&path, SshAuth::Password("secret".into()));
        options.forwards.push(PortForward {
            kind: ForwardKind::Remote,
            bind_host: "127.0.0.1".into(),
            bind_port: remote_port,
            destination_host: "127.0.0.1".into(),
            destination_port: target_port,
            enabled: true,
        });
        let session = Session::spawn_ssh(options, 80, 24, 100).unwrap();
        until(|| {
            session
                .forward_statuses()
                .first()
                .is_some_and(|rule| rule.state == ForwardState::Running)
        })
        .await;
        let mut stream = tokio::net::TcpStream::connect(("127.0.0.1", remote_port))
            .await
            .unwrap();
        stream.write_all(b"remote").await.unwrap();
        let mut response = [0; 6];
        stream.read_exact(&mut response).await.unwrap();
        assert_eq!(&response, b"remote");
        session.set_forward_enabled(0, false).unwrap();
        until(|| session.forward_statuses()[0].state == ForwardState::Stopped).await;
        assert_eq!(stream.read(&mut response).await.unwrap(), 0);
        echo.await.unwrap();
        session.close();
    })
    .await
    .unwrap();
}
