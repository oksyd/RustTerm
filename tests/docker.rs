//! Docker requests share one authenticated SSH connection; fixture channels use real
//! SSH framing and HTTP/1.1, including chunked logs and exec protocol upgrades.
use russh::{
    Channel, ChannelId, Pty,
    keys::{Algorithm, PrivateKey},
    server,
};
use rustterm::{
    config::KnownHosts,
    docker::{Action, DockerClient, Shell},
    session::{Command, Session, SessionState, SshAuth, SshOptions},
};
use std::{
    collections::HashMap,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    },
    time::Duration,
};
use tokio::{
    io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader},
    net::TcpListener,
};
const ID: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
const EXEC: &str = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";
#[derive(Default)]
struct Events {
    auth: AtomicUsize,
    opened: AtomicUsize,
    closed: AtomicUsize,
    ptys: AtomicUsize,
    commands: Mutex<Vec<String>>,
    requests: Mutex<Vec<String>>,
    hold_stats: AtomicBool,
    stats_started: tokio::sync::Notify,
}
struct Peer {
    events: Arc<Events>,
    channels: HashMap<ChannelId, Channel<server::Msg>>,
    mode: Mode,
}
#[derive(Clone, Copy)]
enum Mode {
    Mock,
    Missing,
    Real,
}
impl server::Handler for Peer {
    type Error = russh::Error;
    async fn auth_password(&mut self, _: &str, _: &str) -> Result<server::Auth, Self::Error> {
        self.events.auth.fetch_add(1, Ordering::SeqCst);
        Ok(server::Auth::Accept)
    }
    async fn channel_open_session(
        &mut self,
        channel: Channel<server::Msg>,
        reply: server::ChannelOpenHandle,
        _: &mut server::Session,
    ) -> Result<(), Self::Error> {
        self.events.opened.fetch_add(1, Ordering::SeqCst);
        self.channels.insert(channel.id(), channel);
        reply.accept().await;
        Ok(())
    }
    async fn pty_request(
        &mut self,
        id: ChannelId,
        _: &str,
        _: u32,
        _: u32,
        _: u32,
        _: u32,
        _: &[(Pty, u32)],
        session: &mut server::Session,
    ) -> Result<(), Self::Error> {
        self.events.ptys.fetch_add(1, Ordering::SeqCst);
        session.channel_success(id)?;
        Ok(())
    }
    async fn shell_request(
        &mut self,
        id: ChannelId,
        session: &mut server::Session,
    ) -> Result<(), Self::Error> {
        session.channel_success(id)?;
        session.data(id, b"HOST READY\r\n".to_vec())?;
        Ok(())
    }
    async fn data(
        &mut self,
        id: ChannelId,
        data: &[u8],
        session: &mut server::Session,
    ) -> Result<(), Self::Error> {
        if self.channels.contains_key(&id) {
            session.data(id, data.to_vec())?;
        }
        Ok(())
    }
    async fn channel_close(
        &mut self,
        id: ChannelId,
        _: &mut server::Session,
    ) -> Result<(), Self::Error> {
        self.channels.remove(&id);
        self.events.closed.fetch_add(1, Ordering::SeqCst);
        Ok(())
    }
    async fn exec_request(
        &mut self,
        id: ChannelId,
        command: &[u8],
        session: &mut server::Session,
    ) -> Result<(), Self::Error> {
        let command = String::from_utf8(command.to_vec()).unwrap();
        self.events.commands.lock().unwrap().push(command.clone());
        session.channel_success(id)?;
        if matches!(self.mode, Mode::Missing) {
            session.extended_data(id, 1, b"docker: command not found".to_vec())?;
            session.exit_status_request(id, 127)?;
            session.eof(id)?;
            session.close(id)?;
            return Ok(());
        }
        let channel = self.channels.remove(&id).unwrap();
        let events = self.events.clone();
        let mode = self.mode;
        tokio::spawn(async move {
            match mode {
                Mode::Real => bridge_cli(channel, command).await,
                Mode::Mock => mock_http(channel, events).await,
                Mode::Missing => {}
            }
        });
        Ok(())
    }
}
async fn bridge_cli(channel: Channel<server::Msg>, command: String) {
    use std::process::Stdio;
    let mut child = tokio::process::Command::new("sh")
        .arg("-c")
        .arg(command)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .unwrap();
    let mut stdin = child.stdin.take().unwrap();
    let mut stdout = child.stdout.take().unwrap();
    let mut stderr = child.stderr.take().unwrap();
    let (mut read, send) = channel.split();
    let mut incoming = read.make_reader();
    let mut outgoing = send.make_writer();
    // Command uses leading environment assignments: run through the shell itself.
    tokio::select! {
        _=tokio::io::copy(&mut incoming,&mut stdin)=>{},
        _=tokio::io::copy(&mut stdout,&mut outgoing)=>{},
        _=async {let mut buffer=[0u8;4096];loop {let n=stderr.read(&mut buffer).await.unwrap();if n==0 {std::future::pending::<()>().await;}send.extended_data(1,&buffer[..n]).await.unwrap();}}=>{},
    }
    let _ = child.kill().await;
    let _ = child.wait().await;
    let _ = send.eof().await;
    let _ = send.close().await;
}
async fn mock_http(channel: Channel<server::Msg>, events: Arc<Events>) {
    let mut stream = BufReader::new(channel.into_stream());
    loop {
        let mut first = String::new();
        if stream.read_line(&mut first).await.unwrap_or(0) == 0 {
            break;
        }
        let mut length = 0;
        loop {
            let mut line = String::new();
            if stream.read_line(&mut line).await.unwrap_or(0) == 0 {
                return;
            }
            if line == "\r\n" {
                break;
            }
            if let Some((name, value)) = line.split_once(':')
                && name.eq_ignore_ascii_case("content-length")
            {
                length = value.trim().parse().unwrap();
            }
        }
        let mut body = vec![0; length];
        if stream.read_exact(&mut body).await.is_err() {
            return;
        }
        events.requests.lock().unwrap().push(first.trim().into());
        let path = first.split_whitespace().nth(1).unwrap();
        if path.contains("/stats?") && events.hold_stats.load(Ordering::SeqCst) {
            events.stats_started.notify_one();
            let mut byte = [0];
            let _ = stream.read(&mut byte).await;
            return;
        }
        if path.contains("/logs?") {
            let bytes = [&[1, 0, 0, 0, 0, 0, 0, 7][..], b"hello!\n"].concat();
            let response = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: application/vnd.docker.raw-stream\r\nTransfer-Encoding: chunked\r\n\r\n{:x}\r\n",
                bytes.len()
            );
            if stream.write_all(response.as_bytes()).await.is_err() {
                return;
            }
            for byte in bytes {
                if stream.write_all(&[byte]).await.is_err() {
                    return;
                }
            }
            let _ = stream.write_all(b"\r\n").await;
            let mut byte = [0];
            let _ = stream.read(&mut byte).await;
            return;
        }
        if path.ends_with(&format!("/exec/{EXEC}/start")) {
            if stream.write_all(b"HTTP/1.1 101 UPGRADED\r\nContent-Type: application/vnd.docker.raw-stream\r\nConnection: Upgrade\r\nUpgrade: tcp\r\n\r\nCONTAINER READY\r\n").await.is_err(){return;}
            let mut input = [0; 1024];
            while let Ok(n) = stream.read(&mut input).await {
                if n == 0 {
                    break;
                }
                if stream.write_all(&input[..n]).await.is_err() {
                    break;
                }
            }
            return;
        }
        if path.contains("/stop?") {
            return;
        } // Ambiguous remote close; must not be retried.
        if path.ends_with("/slow/json") {
            std::future::pending::<()>().await;
        }
        let (status, json) = if path == "/version" {
            (
                200,
                serde_json::json!({"Version":"29.0.0","ApiVersion":"1.55","MinAPIVersion":"1.40","Os":"linux"}),
            )
        } else if path.ends_with("/containers/json?all=true") {
            (
                200,
                serde_json::json!([{"Id":ID,"Names":["/web"],"Image":"fixture","State":"running","Status":"Up 1 minute","Labels":{"com.docker.compose.project":"demo"},"Ports":[]}]),
            )
        } else if path.contains("/stats?") {
            (
                200,
                serde_json::json!({"memory_stats":{"usage":512,"limit":1024}}),
            )
        } else if path.ends_with("/exec") {
            (201, serde_json::json!({"Id":EXEC}))
        } else if path.contains("/resize?") || path.ends_with("/start") {
            (204, serde_json::Value::Null)
        } else if path.contains("/exec/") {
            (200, serde_json::json!({"Running":false,"ExitCode":0}))
        } else {
            (
                200,
                serde_json::json!({"Id":ID,"Name":"/web","Created":"2026-01-01","Config":{"Image":"fixture","Tty":false},"State":{"Status":"running","Running":true,"StartedAt":"2026-01-01"},"HostConfig":{"RestartPolicy":{"Name":"no"},"NetworkMode":"bridge"},"NetworkSettings":{"Networks":{}}}),
            )
        };
        let body = if status == 204 {
            String::new()
        } else {
            json.to_string()
        };
        let response = format!(
            "HTTP/1.1 {status} OK\r\nContent-Length: {}\r\nContent-Type: application/json\r\n\r\n{body}",
            body.len()
        );
        if stream.write_all(response.as_bytes()).await.is_err() {
            return;
        }
    }
}
struct Fixture {
    session: Session,
    client: DockerClient,
    events: Arc<Events>,
    server: tokio::task::JoinHandle<()>,
    _dir: tempfile::TempDir,
}
impl Drop for Fixture {
    fn drop(&mut self) {
        self.session.disconnect();
        self.server.abort();
    }
}
impl Fixture {
    async fn new(mode: Mode) -> Self {
        let key = PrivateKey::random(&mut rand::rng(), Algorithm::Ed25519).unwrap();
        let listener = TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("known_hosts");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
        }
        let mut known = KnownHosts::default();
        known
            .trust(
                "127.0.0.1",
                port,
                key.public_key().algorithm().as_ref(),
                &key.public_key().to_openssh().unwrap(),
            )
            .unwrap();
        known.save(&path).unwrap();
        let events = Arc::new(Events::default());
        let peer = Peer {
            events: events.clone(),
            channels: HashMap::new(),
            mode,
        };
        let config = Arc::new(server::Config {
            keys: vec![key],
            auth_rejection_time: Duration::ZERO,
            auth_rejection_time_initial: Some(Duration::ZERO),
            ..Default::default()
        });
        let server = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let server = server::run_stream(config, stream, peer).await.unwrap();
            let _ = server.await;
        });
        let session = Session::spawn_ssh(
            SshOptions {
                host: "127.0.0.1".into(),
                port,
                username: "fixture".into(),
                auth: SshAuth::Password("test".into()),
                known_hosts_path: path,
                timeout_secs: 5,
                keepalive_secs: 0,
                keepalive_max: 3,
                shell_integration: false,
                jump_hosts: vec![],
                forwards: vec![],
                auto_forward: Default::default(),
                forwarding_registry: None,
                authentication: None,
            },
            80,
            24,
            1000,
        )
        .unwrap();
        wait(|| session.state() == SessionState::Connected).await;
        let client = session.docker().unwrap();
        Self {
            session,
            client,
            events,
            server,
            _dir: dir,
        }
    }
}
async fn wait(mut condition: impl FnMut() -> bool) {
    tokio::time::timeout(Duration::from_secs(10), async {
        while !condition() {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("condition timed out");
}
#[tokio::test]
async fn api_reuses_authentication_and_management_channel_while_logs_are_independent() {
    let f = Fixture::new(Mode::Mock).await;
    let version = f.client.version().await.unwrap();
    assert_eq!(version.api, "1.47");
    assert_eq!(f.client.containers().await.unwrap()[0].name(), "web");
    assert_eq!(f.events.opened.load(Ordering::SeqCst), 2);
    assert_eq!(f.events.ptys.load(Ordering::SeqCst), 1);
    let mut logs = f.client.logs(ID, false).await.unwrap();
    let mut bytes = Vec::new();
    while bytes.len() < 7 {
        let (_, chunk) = logs.next().await.unwrap().unwrap();
        bytes.extend(chunk);
    }
    assert_eq!(bytes, b"hello!\n");
    assert_eq!(f.client.stats(ID).await.unwrap().memory_usage(), Some(512));
    drop(logs);
    wait(|| f.events.closed.load(Ordering::SeqCst) >= 1).await;
    f.client.action(ID, Action::Start).await.unwrap();
    assert_eq!(f.events.auth.load(Ordering::SeqCst), 1);
    let commands = f.events.commands.lock().unwrap();
    assert_eq!(commands.len(), 2);
    assert!(commands.iter().all(|c| c.ends_with("system dial-stdio")));
    assert_eq!(f.session.state(), SessionState::Connected);
}
#[tokio::test]
async fn monitor_identity_is_shared_by_session_consumers_and_isolates_endpoints() {
    let fixture = Fixture::new(Mode::Mock).await;
    let client = fixture.client.clone();
    assert_eq!(client.id(), fixture.client.id());
    assert_eq!(fixture.session.docker().unwrap().id(), client.id());
    let other = client
        .with_endpoint(rustterm::docker::Endpoint::parse("/run/user/1000/docker.sock").unwrap());
    assert_ne!(client.id(), other.id());
    let other_session = Fixture::new(Mode::Mock).await;
    assert_ne!(client.id(), other_session.client.id());
}

#[tokio::test]
async fn mutations_are_not_retried_after_an_ambiguous_disconnect() {
    let f = Fixture::new(Mode::Mock).await;
    assert!(matches!(
        f.client.action(ID, Action::Stop).await,
        Err(rustterm::docker::Error::Uncertain(_))
    ));
    assert_eq!(
        f.events
            .requests
            .lock()
            .unwrap()
            .iter()
            .filter(|p| p.contains("/stop?"))
            .count(),
        1
    );
    assert_eq!(f.client.containers().await.unwrap().len(), 1);
}
#[tokio::test]
async fn missing_docker_reports_remote_stderr_without_breaking_the_shell() {
    let f = Fixture::new(Mode::Missing).await;
    let error = f.client.version().await.unwrap_err().to_string();
    assert!(error.contains("docker: command not found"), "{error}");
    assert_eq!(f.session.state(), SessionState::Connected);
}
#[tokio::test]
async fn console_uses_engine_upgrade_and_closes_only_its_own_channel() {
    let f = Fixture::new(Mode::Mock).await;
    let console =
        Session::spawn_container(f.client.clone(), ID.into(), Shell::Sh, 80, 24, 1000).unwrap();
    wait(|| console.state() == SessionState::Connected).await;
    console
        .send(Command::Input(b"from-container\r\n".to_vec()))
        .unwrap();
    console
        .send(Command::Resize {
            columns: 120,
            rows: 40,
        })
        .unwrap();
    wait(|| {
        f.events
            .requests
            .lock()
            .unwrap()
            .iter()
            .any(|p| p.contains("h=40&w=120"))
    })
    .await;
    console.disconnect();
    wait(|| !console.state().is_running()).await;
    assert_eq!(f.session.state(), SessionState::Connected);
    f.client.containers().await.unwrap();
    assert_eq!(f.events.ptys.load(Ordering::SeqCst), 1);
}
#[tokio::test]
async fn parent_disconnect_closes_log_channels_and_disables_future_requests() {
    let f = Fixture::new(Mode::Mock).await;
    let mut logs = f.client.logs(ID, false).await.unwrap();
    logs.next().await.unwrap().unwrap();
    f.session.disconnect();
    wait(|| !f.client.is_connected()).await;
    tokio::time::timeout(Duration::from_secs(3), async {
        while logs.next().await.is_some() {}
    })
    .await
    .unwrap();
    assert!(matches!(
        f.client.containers().await,
        Err(rustterm::docker::Error::Disconnected)
    ));
}
#[tokio::test]
#[ignore = "requires Docker CLI and a local daemon; uses only the container ID supplied by RUSTTERM_DOCKER_TEST_CONTAINER"]
async fn real_engine_over_ssh() {
    let id = std::env::var("RUSTTERM_DOCKER_TEST_CONTAINER")
        .expect("isolated disposable fixture container required");
    assert_eq!(id.len(), 64);
    let f = Fixture::new(Mode::Real).await;
    let version = f.client.version().await.unwrap();
    assert!(!version.version.is_empty());
    assert!(
        f.client
            .containers()
            .await
            .unwrap()
            .iter()
            .any(|c| c.id == id)
    );
    let details = f.client.inspect(&id).await.unwrap();
    assert!(details.state.running);
    assert_eq!(
        details
            .config
            .labels
            .as_ref()
            .and_then(|labels| labels.get("com.docker.compose.project"))
            .map(String::as_str),
        Some("rustterm-fixture"),
        "refusing to mutate a non-fixture container"
    );
    f.client.stats(&id).await.unwrap();
    let mut logs = f.client.logs(&id, details.config.tty).await.unwrap();
    assert!(
        tokio::time::timeout(Duration::from_secs(5), logs.next())
            .await
            .unwrap()
            .unwrap()
            .is_ok()
    );
    drop(logs);
    let console =
        Session::spawn_container(f.client.clone(), id.clone(), Shell::Sh, 80, 24, 1000).unwrap();
    wait(|| console.state() == SessionState::Connected).await;
    console
        .send(Command::Input(
            b"printf 'container-console-ok\\n'; exit\n".to_vec(),
        ))
        .unwrap();
    wait(|| !console.state().is_running()).await;
    assert_eq!(
        console.state(),
        SessionState::Disconnected { exit_code: Some(0) }
    );
    f.client.action(&id, Action::Restart).await.unwrap();
    assert!(f.client.inspect(&id).await.unwrap().state.running);
    f.client.action(&id, Action::Stop).await.unwrap();
    assert!(!f.client.inspect(&id).await.unwrap().state.running);
    f.client.action(&id, Action::Start).await.unwrap();
    assert!(f.client.inspect(&id).await.unwrap().state.running);
    assert_eq!(f.events.auth.load(Ordering::SeqCst), 1);
    assert_eq!(f.events.ptys.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn cancelling_a_request_closes_its_channel_and_releases_the_management_lock() {
    let f = Fixture::new(Mode::Mock).await;
    f.events.hold_stats.store(true, Ordering::SeqCst);
    let client = f.client.clone();
    let task = tokio::spawn(async move { client.stats(ID).await });
    tokio::time::timeout(Duration::from_secs(5), f.events.stats_started.notified())
        .await
        .unwrap();
    task.abort();
    let _ = task.await;
    wait(|| f.events.closed.load(Ordering::SeqCst) == 1).await;
    assert_eq!(f.client.containers().await.unwrap().len(), 1);
    assert_eq!(f.session.state(), SessionState::Connected);
}

#[tokio::test]
async fn streaming_channels_are_bounded_and_released_on_drop() {
    let f = Fixture::new(Mode::Mock).await;
    let mut streams = Vec::new();
    for _ in 0..5 {
        streams.push(f.client.logs(ID, false).await.unwrap());
    }
    assert!(matches!(
        f.client.logs(ID, false).await,
        Err(rustterm::docker::Error::Busy)
    ));
    drop(streams);
    wait(|| f.events.closed.load(Ordering::SeqCst) == 5).await;
    let stream = f.client.logs(ID, false).await.unwrap();
    drop(stream);
    f.client.containers().await.unwrap();
}

#[tokio::test]
async fn inspect_rejects_a_response_for_a_different_container() {
    let f = Fixture::new(Mode::Mock).await;
    assert!(matches!(
        f.client.inspect(&"c".repeat(64)).await,
        Err(rustterm::docker::Error::InvalidResponse)
    ));
}
