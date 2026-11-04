//! Shared SSH authentication and pinned-host verification for terminal and
//! direct-tcpip transports. This module never starts a shell or allocates a PTY.
use crate::i18n::messages as i18n_key;

use super::{
    SshAuth, SshOptions,
    broker::{self, TcpRequest},
    forwarding::ForwardController,
};
use crate::config::{HostKeyStatus, KnownHosts};
use crate::session::{FailureKind, SessionState, state::Shared};
use anyhow::{Context as _, Result};
use russh::{
    client,
    keys::{HashAlg, PrivateKeyWithHashAlg, PublicKeyOrCertificate, decode_secret_key},
};
use std::{
    future::Future,
    pin::Pin,
    task::{Context as TaskContext, Poll},
};
use std::{
    path::PathBuf,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};
use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};
use tokio::sync::{mpsc, oneshot};
use tokio_util::sync::CancellationToken;
use zeroize::Zeroizing;

/// Approve only after independently verifying the fingerprint. Dropping the
/// reply rejects this connection; no authentication precedes a true reply.
pub struct HostKeyChallenge {
    pub host: String,
    pub port: u16,
    pub algorithm: String,
    pub fingerprint: String,
    pub reply: oneshot::Sender<bool>,
}

pub struct AuthenticationPrompt {
    pub label: String,
    pub echo: bool,
}
pub struct AuthenticationChallenge {
    pub host: String,
    pub username: String,
    pub name: String,
    pub instructions: String,
    pub prompts: Vec<AuthenticationPrompt>,
    pub reply: oneshot::Sender<Vec<crate::config::Secret>>,
    pub cancelled: CancellationToken,
}

pub(super) type SshResult<T> = std::result::Result<T, (FailureKind, String)>;
#[derive(Clone)]
pub(super) enum Ui {
    Terminal(Arc<Shared>),
    Tunnel {
        trust: mpsc::Sender<HostKeyChallenge>,
        closed: CancellationToken,
    },
    Forwarding {
        controller: ForwardController,
        closed: CancellationToken,
    },
}
struct Verification {
    ui: Ui,
    stop: CancellationToken,
    waiting: AtomicBool,
    failure: Mutex<Option<(FailureKind, String)>>,
}
impl Verification {
    fn fail(&self, kind: FailureKind, message: impl Into<String>) {
        let message = message.into();
        if let Ui::Terminal(shared) = &self.ui {
            shared.failed(kind, message.clone());
        }
        *self.failure.lock().unwrap_or_else(|e| e.into_inner()) = Some((kind, message));
    }
    async fn confirm(&self, host: &str, port: u16, algorithm: &str, fingerprint: String) -> bool {
        let (reply, answer) = oneshot::channel();
        self.waiting.store(true, Ordering::Release);
        let accepted = match &self.ui {
            Ui::Terminal(shared) => {
                *shared.trust.lock().unwrap_or_else(|e| e.into_inner()) = Some(reply);
                if shared.live_state(SessionState::WaitingHostKey {
                    host: host.into(),
                    port,
                    algorithm: algorithm.into(),
                    fingerprint,
                }) {
                    tokio::select! { biased; _ = self.stop.cancelled() => false, accepted = answer => accepted.unwrap_or(false) }
                } else {
                    false
                }
            }
            Ui::Tunnel { trust, .. } => {
                let challenge = HostKeyChallenge {
                    host: host.into(),
                    port,
                    algorithm: algorithm.into(),
                    fingerprint,
                    reply,
                };
                let delivered = tokio::select! { biased; _ = self.stop.cancelled() => false, sent = trust.send(challenge) => sent.is_ok() };
                if delivered {
                    tokio::select! { biased; _ = self.stop.cancelled() => false, accepted = answer => accepted.unwrap_or(false) }
                } else {
                    false
                }
            }
            Ui::Forwarding { .. } => false,
        };
        self.waiting.store(false, Ordering::Release);
        if let Ui::Terminal(shared) = &self.ui {
            shared
                .trust
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .take();
        }
        accepted && !self.stop.is_cancelled()
    }
}
pub(super) struct HostVerifier {
    host: String,
    port: u16,
    path: PathBuf,
    verification: Arc<Verification>,
    _parents: Vec<client::Handle<HostVerifier>>,
    forwarding: Option<ForwardController>,
}
impl client::Handler for HostVerifier {
    type Error = anyhow::Error;
    async fn server_channel_open_forwarded_tcpip(
        &mut self,
        channel: russh::Channel<client::Msg>,
        connected_address: &str,
        connected_port: u32,
        _: &str,
        _: u32,
        reply: client::ChannelOpenHandle,
        _: &mut client::Session,
    ) -> Result<()> {
        if !self.verification.stop.is_cancelled()
            && let Some(forwarding) = &self.forwarding
        {
            forwarding.incoming(channel, connected_address, connected_port, reply);
        }
        Ok(())
    }
    async fn check_server_key(&mut self, server_key: &PublicKeyOrCertificate) -> Result<bool> {
        let state = &self.verification;
        if state.stop.is_cancelled() {
            return Ok(false);
        }
        if server_key.certificate().is_some() {
            state.fail(FailureKind::HostKeyRejected, crate::i18n::tr(i18n_key::CONNECTIONS_ONLY_PINNED_HOST_PUBLIC_KEYS_ARE_SUPPORTED_HOST_CERTIFICATES_ARE));
            return Ok(false);
        }
        let key = server_key.public_key();
        let algorithm = key.algorithm().to_string();
        let encoded = key.to_openssh()?;
        let fingerprint = key.fingerprint(HashAlg::Sha256).to_string();
        let (path, host, port, check_algorithm, check_key) = (
            self.path.clone(),
            self.host.clone(),
            self.port,
            algorithm.clone(),
            encoded.clone(),
        );
        let status = tokio::task::spawn_blocking(move || {
            KnownHosts::load(&path)
                .map(|known| known.check(&host, port, &check_algorithm, &check_key))
        })
        .await?;
        if state.stop.is_cancelled() {
            return Ok(false);
        }
        match status {
            Err(error) => {
                state.fail(FailureKind::KnownHosts, error.to_string());
                return Ok(false);
            }
            Ok(HostKeyStatus::Trusted) => return Ok(true),
            Ok(HostKeyStatus::Changed { .. }) => {
                state.fail(FailureKind::HostKeyMismatch, crate::trf!(i18n_key::CONNECTIONS_THE_KEY_FOR_HOST_PORT_HAS_CHANGED_ALGORITHM_FINGERPRINT_AUTHENTICATION, host = self.host, port = self.port, algorithm = algorithm, fingerprint = fingerprint));
                return Ok(false);
            }
            Ok(HostKeyStatus::Unknown) => {}
        }
        if matches!(&state.ui, Ui::Forwarding { .. }) {
            // The primary connection already verified this chain. An auxiliary
            // connection must never acquire new trust or change terminal state.
            state.fail(
                FailureKind::HostKeyRejected,
                crate::trf!(
                    i18n_key::CONNECTIONS_THE_FORWARDING_CONNECTION_DOES_NOT_TRUST_HOST_PORT_ALGORITHM_FINGERPRINT,
                    host = self.host,
                    port = self.port,
                    algorithm = algorithm,
                    fingerprint = fingerprint
                ),
            );
            return Ok(false);
        }
        if !state
            .confirm(&self.host, self.port, &algorithm, fingerprint)
            .await
        {
            if !state.stop.is_cancelled() {
                state.fail(
                    FailureKind::HostKeyRejected,
                    crate::i18n::tr(i18n_key::CONNECTIONS_THE_HOST_KEY_WAS_REJECTED),
                );
            }
            return Ok(false);
        }
        let (path, host) = (self.path.clone(), self.host.clone());
        let saved = tokio::task::spawn_blocking(move || {
            KnownHosts::trust_and_save(&path, &host, port, &algorithm, &encoded)
        })
        .await?;
        if state.stop.is_cancelled() {
            return Ok(false);
        }
        if let Err(error) = saved {
            state.fail(FailureKind::KnownHosts, error.to_string());
            return Ok(false);
        }
        Ok(match &state.ui {
            Ui::Terminal(shared) => shared.live_state(SessionState::Connecting),
            Ui::Tunnel { .. } | Ui::Forwarding { .. } => true,
        })
    }

    async fn disconnected(&mut self, reason: client::DisconnectReason<Self::Error>) -> Result<()> {
        if let Ui::Tunnel { closed, .. } | Ui::Forwarding { closed, .. } = &self.verification.ui {
            // Connection completion is separate from user cancellation so an
            // authentication error is not replaced by a cancelled-stage error.
            closed.cancel();
        }
        match reason {
            client::DisconnectReason::ReceivedDisconnect(_) => Ok(()),
            client::DisconnectReason::Error(error) => {
                // Record the cause before russh releases the channel readers.
                // Their EOF can otherwise make actor cleanup cancel the monitor
                // before it observes the completed connection handle.
                if !self.verification.stop.is_cancelled()
                    && let Ui::Terminal(shared) = &self.verification.ui
                {
                    shared
                        .remote_failure
                        .lock()
                        .unwrap_or_else(|e| e.into_inner())
                        .get_or_insert_with(|| (FailureKind::RemoteClosed, error.to_string()));
                }
                Err(error)
            }
        }
    }
}

pub(super) async fn stage<T, E: std::fmt::Display>(
    future: impl Future<Output = std::result::Result<T, E>>,
    timeout: Duration,
    kind: FailureKind,
    stop: &CancellationToken,
) -> SshResult<T> {
    tokio::select! {
        biased;
        _ = stop.cancelled() => Err((FailureKind::Network, crate::i18n::tr(i18n_key::CONNECTIONS_CONNECTION_CANCELLED).into())),
        result = tokio::time::timeout(timeout, future) => match result {
            Ok(Ok(value)) => Ok(value), Ok(Err(error)) => Err((kind, error.to_string())),
            Err(_) => Err((FailureKind::Timeout, crate::i18n::tr(i18n_key::CONNECTIONS_AN_SSH_CONNECTION_STAGE_TIMED_OUT).into())),
        }
    }
}

struct CancelOnDrop(Option<CancellationToken>);
impl Drop for CancelOnDrop {
    fn drop(&mut self) {
        if let Some(stop) = &self.0 {
            stop.cancel();
        }
    }
}

pub(super) async fn connect_authenticated(
    mut options: SshOptions,
    ui: Ui,
    stop: CancellationToken,
) -> SshResult<client::Handle<HostVerifier>> {
    let mut guard = CancelOnDrop(Some(stop.clone()));
    if options.jump_hosts.len() > 8 {
        return Err((
            FailureKind::Protocol,
            crate::i18n::tr(i18n_key::CONNECTIONS_TOO_MANY_SSH_JUMP_HOSTS_OR_PORT_FORWARDS).into(),
        ));
    }
    options.host = crate::config::known_hosts::normalize_host(&options.host)
        .map_err(|error| (FailureKind::Dns, error.to_string()))?;
    for hop in &mut options.jump_hosts {
        hop.validate()
            .map_err(|error| (FailureKind::Protocol, error.to_string()))?;
        hop.host = crate::config::known_hosts::normalize_host(&hop.host)
            .map_err(|error| (FailureKind::Dns, error.to_string()))?;
    }
    crate::config::ssh::validate_forwards(&options.forwards)
        .map_err(|error| (FailureKind::Protocol, error.to_string()))?;
    let timeout = Duration::from_secs(options.timeout_secs.clamp(1, 600));
    let mut parents: Vec<client::Handle<HostVerifier>> = Vec::new();
    for hop in &options.jump_hosts {
        let stream = if let Some(parent) = parents.last() {
            let channel = stage(
                parent.channel_open_direct_tcpip(&hop.host, u32::from(hop.port), "127.0.0.1", 0),
                timeout,
                FailureKind::Network,
                &stop,
            )
            .await?;
            Some(Box::new(channel.into_stream()) as Box<dyn SshStream>)
        } else {
            None
        };
        let auth = match &hop.identity_file {
            Some(path) => SshAuth::PrivateKey {
                path: path.clone(),
                passphrase: None,
            },
            None => SshAuth::Agent {
                socket: hop.agent_socket.clone(),
            },
        };
        let hop_options = SshOptions {
            host: hop.host.clone(),
            port: hop.port,
            username: hop.username.clone(),
            auth,
            known_hosts_path: options.known_hosts_path.clone(),
            timeout_secs: options.timeout_secs,
            keepalive_secs: options.keepalive_secs,
            keepalive_max: options.keepalive_max,
            shell_integration: false,
            jump_hosts: Vec::new(),
            forwards: Vec::new(),
            authentication: options.authentication.clone(),
        };
        parents.push(
            connect_one(
                hop_options,
                ui.clone(),
                stop.clone(),
                stream,
                Vec::new(),
                None,
            )
            .await?,
        );
    }
    let stream = if let Some(parent) = parents.last() {
        let channel = stage(
            parent.channel_open_direct_tcpip(
                &options.host,
                u32::from(options.port),
                "127.0.0.1",
                0,
            ),
            timeout,
            FailureKind::Network,
            &stop,
        )
        .await?;
        Some(Box::new(channel.into_stream()) as Box<dyn SshStream>)
    } else {
        None
    };
    // Only the final host owns the remote listeners. Jump hosts cannot route
    // unsolicited forwarded channels through the target's listener registry.
    let forwarding = match &ui {
        Ui::Forwarding { controller, .. } => Some(controller.clone()),
        _ => None,
    };
    let handle = connect_one(options, ui, stop, stream, parents, forwarding).await?;
    guard.0 = None;
    Ok(handle)
}

trait SshStream: AsyncRead + AsyncWrite + Unpin + Send {}
impl<T: AsyncRead + AsyncWrite + Unpin + Send> SshStream for T {}

async fn connect_one(
    mut options: SshOptions,
    ui: Ui,
    stop: CancellationToken,
    stream: Option<Box<dyn SshStream>>,
    parents: Vec<client::Handle<HostVerifier>>,
    forwarding: Option<ForwardController>,
) -> SshResult<client::Handle<HostVerifier>> {
    let mut guard = CancelOnDrop(Some(stop.clone()));
    let timeout = Duration::from_secs(options.timeout_secs.clamp(1, 600));
    let stream: Box<dyn SshStream> = if let Some(stream) = stream {
        stream
    } else {
        let addresses = stage(
            tokio::net::lookup_host((options.host.as_str(), options.port)),
            timeout,
            FailureKind::Dns,
            &stop,
        )
        .await?
        .collect::<Vec<_>>();
        if addresses.is_empty() {
            return Err((
                FailureKind::Dns,
                crate::i18n::tr(i18n_key::CONNECTIONS_THE_HOSTNAME_DID_NOT_RESOLVE_TO_AN_ADDRESS)
                    .into(),
            ));
        }
        let stream = stage(
            super::dial::connect(addresses),
            timeout,
            FailureKind::Network,
            &stop,
        )
        .await?;
        stream
            .set_nodelay(true)
            .map_err(|error| (FailureKind::Network, error.to_string()))?;
        Box::new(stream)
    };
    let config = Arc::new(client::Config {
        channel_buffer_size: 32,
        window_size: 256 * 1024,
        maximum_packet_size: 32 * 1024,
        keepalive_interval: (options.keepalive_secs > 0)
            .then(|| Duration::from_secs(options.keepalive_secs)),
        keepalive_max: options.keepalive_max.max(1),
        nodelay: true,
        ..Default::default()
    });
    let verification = Arc::new(Verification {
        ui,
        stop: stop.clone(),
        waiting: AtomicBool::new(false),
        failure: Mutex::new(None),
    });
    let verifier = HostVerifier {
        host: options.host.clone(),
        port: options.port,
        path: options.known_hosts_path.clone(),
        verification: verification.clone(),
        _parents: parents,
        forwarding,
    };
    let connection =
        client::connect_stream(config, CancelStream::new(stream, stop.clone()), verifier);
    tokio::pin!(connection);
    let mut remaining = timeout;
    let mut previous = tokio::time::Instant::now();
    let mut timer = tokio::time::interval(Duration::from_millis(100));
    let mut handle = loop {
        tokio::select! {
            biased;
            _ = stop.cancelled() => return Err((FailureKind::Network, crate::i18n::tr(i18n_key::CONNECTIONS_CONNECTION_CANCELLED).into())),
            result = &mut connection => break result.map_err(|error| verification.failure.lock().unwrap_or_else(|e| e.into_inner()).take().unwrap_or((FailureKind::Protocol, error.to_string())))?,
            _ = timer.tick() => {
                let now = tokio::time::Instant::now(); let elapsed = now - previous; previous = now;
                if !verification.waiting.load(Ordering::Acquire) {
                    remaining = remaining.saturating_sub(elapsed);
                    if remaining.is_zero() { return Err((FailureKind::Timeout, crate::i18n::tr(i18n_key::CONNECTIONS_SSH_HANDSHAKE_TIMED_OUT).into())); }
                }
            }
        }
    };
    if let Ui::Terminal(shared) = &verification.ui
        && !shared.live_state(SessionState::WaitingAuthentication)
    {
        return Err((
            FailureKind::Network,
            crate::i18n::tr(i18n_key::CONNECTIONS_CONNECTION_CANCELLED).into(),
        ));
    }
    let authenticated = match &mut options.auth {
        SshAuth::Password(password) => {
            let password = Zeroizing::new(std::mem::take(password));
            stage(
                handle.authenticate_password(&options.username, password.as_str()),
                timeout,
                FailureKind::Authentication,
                &stop,
            )
            .await?
        }
        SshAuth::PrivateKey { path, passphrase } => {
            let path = path.clone();
            let supplied = passphrase.take().map(Zeroizing::new);
            let read_path = path.clone();
            let key = stage(
                tokio::task::spawn_blocking(move || {
                    read_private_key(&read_path, supplied.as_deref().map(String::as_str))
                }),
                timeout,
                FailureKind::PrivateKey,
                &stop,
            )
            .await?;
            let key = match key {
                Ok(key) => key,
                Err(error)
                    if options.authentication.is_some()
                        && matches!(
                            error.downcast_ref::<russh::keys::Error>(),
                            Some(russh::keys::Error::KeyIsEncrypted)
                        ) =>
                {
                    let answers = request_authentication(
                        &options,
                        &verification.ui,
                        crate::i18n::tr(i18n_key::CONNECTIONS_PRIVATE_KEY_PASSPHRASE).into(),
                        path.display().to_string(),
                        vec![AuthenticationPrompt {
                            label: crate::i18n::tr(i18n_key::CONNECTIONS_PRIVATE_KEY_PASSPHRASE)
                                .into(),
                            echo: false,
                        }],
                        &stop,
                    )
                    .await?;
                    let secret = answers
                        .into_iter()
                        .next()
                        .ok_or_else(|| (FailureKind::Authentication, error.to_string()))?;
                    stage(
                        tokio::task::spawn_blocking(move || {
                            read_private_key(&path, Some(secret.expose()))
                        }),
                        timeout,
                        FailureKind::PrivateKey,
                        &stop,
                    )
                    .await?
                    .map_err(|e| (FailureKind::PrivateKey, e.to_string()))?
                }
                Err(error) => return Err((FailureKind::PrivateKey, error.to_string())),
            };
            let hash = stage(
                handle.best_supported_rsa_hash(),
                timeout,
                FailureKind::Authentication,
                &stop,
            )
            .await?
            .flatten();
            stage(
                handle.authenticate_publickey(
                    &options.username,
                    PrivateKeyWithHashAlg::new(Arc::new(key), hash),
                ),
                timeout,
                FailureKind::Authentication,
                &stop,
            )
            .await?
        }
        SshAuth::Agent { socket } => {
            authenticate_agent(
                &mut handle,
                &options.username,
                socket.as_deref(),
                timeout,
                &stop,
            )
            .await?
        }
        SshAuth::KeyboardInteractive => {
            keyboard_authenticate(&mut handle, &options, &verification.ui, timeout, &stop).await?;
            russh::client::AuthResult::Success
        }
    };
    if !authenticated.success() {
        if matches!(
            authenticated,
            russh::client::AuthResult::Failure {
                partial_success: true,
                ..
            }
        ) {
            keyboard_authenticate(&mut handle, &options, &verification.ui, timeout, &stop).await?;
        } else {
            return Err((
                FailureKind::Authentication,
                crate::i18n::tr(i18n_key::CONNECTIONS_SSH_AUTHENTICATION_FAILED).into(),
            ));
        }
    }
    guard.0 = None;
    Ok(handle)
}

#[derive(Clone)]
pub(crate) struct TunnelConnector(Arc<TunnelConnection>);
struct TunnelConnection {
    requests: mpsc::Sender<TcpRequest>,
    stop: CancellationToken,
}
impl Drop for TunnelConnection {
    fn drop(&mut self) {
        self.stop.cancel();
    }
}
impl TunnelConnector {
    pub(crate) async fn open(&self, host: String, port: u16) -> SshResult<DirectTcpStream> {
        let host = crate::config::known_hosts::normalize_host(&host)
            .map_err(|e| (FailureKind::Dns, e.to_string()))?;
        if port == 0 {
            return Err((
                FailureKind::Network,
                crate::i18n::tr(i18n_key::CONNECTIONS_INVALID_SSH_ENDPOINT).into(),
            ));
        }
        let (reply, response) = oneshot::channel();
        tokio::select! { biased; _=self.0.stop.cancelled()=>return Err((FailureKind::Network,crate::i18n::tr(i18n_key::CONNECTIONS_CONNECTION_CANCELLED).into())), result=self.0.requests.send(TcpRequest {host,port,reply})=>result.map_err(|e|(FailureKind::Network,e.to_string()))? }
        let stream = tokio::select! { biased; _=self.0.stop.cancelled()=>return Err((FailureKind::Network,crate::i18n::tr(i18n_key::CONNECTIONS_CONNECTION_CANCELLED).into())), result=response=>result.map_err(|e|(FailureKind::Network,e.to_string()))?? };
        Ok(DirectTcpStream {
            stream,
            _connection: self.clone(),
        })
    }
}
pub(crate) struct DirectTcpStream {
    stream: russh::ChannelStream<client::Msg>,
    _connection: TunnelConnector,
}
impl AsyncRead for DirectTcpStream {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut TaskContext<'_>,
        bytes: &mut ReadBuf<'_>,
    ) -> Poll<std::io::Result<()>> {
        Pin::new(&mut self.get_mut().stream).poll_read(cx, bytes)
    }
}
impl AsyncWrite for DirectTcpStream {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut TaskContext<'_>,
        bytes: &[u8],
    ) -> Poll<std::io::Result<usize>> {
        Pin::new(&mut self.get_mut().stream).poll_write(cx, bytes)
    }
    fn poll_flush(self: Pin<&mut Self>, cx: &mut TaskContext<'_>) -> Poll<std::io::Result<()>> {
        Pin::new(&mut self.get_mut().stream).poll_flush(cx)
    }
    fn poll_shutdown(self: Pin<&mut Self>, cx: &mut TaskContext<'_>) -> Poll<std::io::Result<()>> {
        Pin::new(&mut self.get_mut().stream).poll_shutdown(cx)
    }
}
pub(crate) async fn connect_tunnel(
    options: SshOptions,
    trust: mpsc::Sender<HostKeyChallenge>,
    stop: CancellationToken,
) -> SshResult<TunnelConnector> {
    let timeout = Duration::from_secs(options.timeout_secs.clamp(1, 600));
    let closed = stop.child_token();
    let mut handle = connect_authenticated(
        options,
        Ui::Tunnel {
            trust,
            closed: closed.clone(),
        },
        stop.clone(),
    )
    .await?;
    let (requests, receiver) = mpsc::channel::<TcpRequest>(32);
    let connection = TunnelConnector(Arc::new(TunnelConnection {
        requests,
        stop: stop.clone(),
    }));
    tokio::spawn(async move {
        let _guard = CancelOnDrop(Some(stop.clone()));
        broker::run_tunnel(&handle, receiver, &closed, timeout).await;
        stop.cancel();
        let _ = tokio::time::timeout(crate::session::transport::IO_TIMEOUT, &mut handle).await;
    });
    Ok(connection)
}
async fn authenticate_agent(
    handle: &mut client::Handle<HostVerifier>,
    username: &str,
    socket: Option<&std::path::Path>,
    timeout: Duration,
    stop: &CancellationToken,
) -> SshResult<client::AuthResult> {
    use russh::keys::agent::{AgentIdentity, client::AgentClient};
    #[cfg(unix)]
    let mut agent = match socket {
        Some(path) => {
            stage(
                AgentClient::connect_uds(path),
                timeout,
                FailureKind::Authentication,
                stop,
            )
            .await?
        }
        None => {
            stage(
                AgentClient::connect_env(),
                timeout,
                FailureKind::Authentication,
                stop,
            )
            .await?
        }
    };
    #[cfg(windows)]
    let mut agent = stage(
        AgentClient::connect_named_pipe(
            socket.unwrap_or_else(|| std::path::Path::new(r"\\.\pipe\openssh-ssh-agent")),
        ),
        timeout,
        FailureKind::Authentication,
        stop,
    )
    .await?;
    if let Some(binding) = handle.agent_session_binding() {
        stage(
            async {
                match agent.bind_session(binding, false).await {
                    // Legacy agents reject the optional extension. Constrained keys
                    // remain protected by the agent's own signing authorization.
                    Ok(()) | Err(russh::keys::Error::AgentFailure) => Ok(()),
                    Err(error) => Err(error),
                }
            },
            timeout,
            FailureKind::Authentication,
            stop,
        )
        .await?;
    }
    let identities = stage(
        agent.request_identities(),
        timeout,
        FailureKind::Authentication,
        stop,
    )
    .await?;
    let hash = stage(
        handle.best_supported_rsa_hash(),
        timeout,
        FailureKind::Authentication,
        stop,
    )
    .await?
    .flatten();
    for identity in identities.into_iter().take(32) {
        let result = match identity {
            AgentIdentity::PublicKey { key, .. } => {
                stage(
                    handle.authenticate_publickey_with(username, key, hash, &mut agent),
                    timeout,
                    FailureKind::Authentication,
                    stop,
                )
                .await?
            }
            AgentIdentity::Certificate { certificate, .. } => {
                stage(
                    handle.authenticate_certificate_with(username, certificate, hash, &mut agent),
                    timeout,
                    FailureKind::Authentication,
                    stop,
                )
                .await?
            }
        };
        if result.success()
            || matches!(
                result,
                client::AuthResult::Failure {
                    partial_success: true,
                    ..
                }
            )
        {
            return Ok(result);
        }
    }
    Err((
        FailureKind::Authentication,
        crate::i18n::tr(
            i18n_key::CONNECTIONS_THE_SSH_AGENT_HAS_NO_IDENTITY_ACCEPTED_BY_THIS_SERVER,
        )
        .into(),
    ))
}
async fn request_authentication(
    options: &SshOptions,
    ui: &Ui,
    name: String,
    instructions: String,
    prompts: Vec<AuthenticationPrompt>,
    stop: &CancellationToken,
) -> SshResult<Vec<crate::config::Secret>> {
    if prompts.len() > 16
        || name.len() > 4096
        || instructions.len() > 16384
        || prompts.iter().any(|p| p.label.len() > 4096)
    {
        return Err((
            FailureKind::Protocol,
            crate::i18n::tr(
                i18n_key::CONNECTIONS_THE_SSH_AUTHENTICATION_CHALLENGE_EXCEEDS_ITS_LIMITS,
            )
            .into(),
        ));
    }
    let Some(sender) = &options.authentication else {
        return Err((
            FailureKind::Authentication,
            crate::i18n::tr(
                i18n_key::CONNECTIONS_INTERACTIVE_SSH_AUTHENTICATION_REQUIRES_AN_AVAILABLE_PROMPT,
            )
            .into(),
        ));
    };
    let count = prompts.len();
    let (reply, response) = oneshot::channel();
    let instructions = if matches!(ui, Ui::Forwarding { .. }) {
        let purpose = crate::i18n::tr(
            i18n_key::CONNECTIONS_AUTHENTICATE_THE_SEPARATE_SSH_CONNECTION_USED_FOR_PORT_FORWARDING_THE,
        );
        if instructions.is_empty() {
            purpose.to_owned()
        } else {
            format!("{purpose}\n\n{instructions}")
        }
    } else {
        instructions
    };
    let challenge = AuthenticationChallenge {
        host: options.host.clone(),
        username: options.username.clone(),
        name,
        instructions,
        prompts,
        reply,
        cancelled: stop.clone(),
    };
    tokio::select! { biased; _=stop.cancelled()=>return Err((FailureKind::Authentication,crate::i18n::tr(i18n_key::CONNECTIONS_AUTHENTICATION_CANCELLED).into())), sent=sender.send(challenge)=>sent.map_err(|e|(FailureKind::Authentication,e.to_string()))? }
    let answers = tokio::select! { biased; _=stop.cancelled()=>return Err((FailureKind::Authentication,crate::i18n::tr(i18n_key::CONNECTIONS_AUTHENTICATION_CANCELLED).into())), response=response=>response.map_err(|_|(FailureKind::Authentication,crate::i18n::tr(i18n_key::CONNECTIONS_AUTHENTICATION_CANCELLED).into()))? };
    if answers.len() != count || answers.iter().any(|a| a.expose().len() > 4096) {
        return Err((
            FailureKind::Authentication,
            crate::i18n::tr(i18n_key::CONNECTIONS_INVALID_SSH_AUTHENTICATION_RESPONSE).into(),
        ));
    }
    Ok(answers)
}
async fn keyboard_authenticate(
    handle: &mut client::Handle<HostVerifier>,
    options: &SshOptions,
    ui: &Ui,
    timeout: Duration,
    stop: &CancellationToken,
) -> SshResult<()> {
    use client::KeyboardInteractiveAuthResponse as Response;
    let mut response = stage(
        handle.authenticate_keyboard_interactive_start(&options.username, None),
        timeout,
        FailureKind::Authentication,
        stop,
    )
    .await?;
    for _ in 0..16 {
        match response {
            Response::Success => return Ok(()),
            Response::Failure { .. } => {
                return Err((
                    FailureKind::Authentication,
                    crate::i18n::tr(i18n_key::CONNECTIONS_SSH_AUTHENTICATION_FAILED).into(),
                ));
            }
            Response::InfoRequest {
                name,
                instructions,
                prompts,
            } => {
                let answers = request_authentication(
                    options,
                    ui,
                    name,
                    instructions,
                    prompts
                        .into_iter()
                        .map(|p| AuthenticationPrompt {
                            label: p.prompt,
                            echo: p.echo,
                        })
                        .collect(),
                    stop,
                )
                .await?;
                let answers: Vec<_> = answers.iter().map(|a| a.expose().to_owned()).collect();
                response = stage(
                    handle.authenticate_keyboard_interactive_respond(answers),
                    timeout,
                    FailureKind::Authentication,
                    stop,
                )
                .await?;
            }
        }
    }
    Err((
        FailureKind::Authentication,
        crate::i18n::tr(i18n_key::CONNECTIONS_TOO_MANY_SSH_AUTHENTICATION_ROUNDS).into(),
    ))
}

const MAX_PRIVATE_KEY_BYTES: u64 = 1024 * 1024;
fn read_private_key(
    path: &std::path::Path,
    passphrase: Option<&str>,
) -> Result<russh::keys::PrivateKey> {
    use std::io::Read;
    let mut options = std::fs::OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        // A FIFO must not pin a blocking-pool worker before metadata validation.
        options.custom_flags(libc::O_NONBLOCK | libc::O_CLOEXEC);
    }
    let file = options.open(path).context(crate::i18n::tr(
        i18n_key::CONNECTIONS_CANNOT_OPEN_THE_PRIVATE_KEY_FILE,
    ))?;
    let metadata = file.metadata().context(crate::i18n::tr(
        i18n_key::CONNECTIONS_CANNOT_INSPECT_THE_PRIVATE_KEY_FILE,
    ))?;
    anyhow::ensure!(
        metadata.is_file(),
        crate::i18n::tr(i18n_key::CONNECTIONS_THE_PRIVATE_KEY_MUST_BE_A_REGULAR_FILE)
    );
    anyhow::ensure!(
        metadata.len() <= MAX_PRIVATE_KEY_BYTES,
        crate::i18n::tr(i18n_key::CONNECTIONS_THE_PRIVATE_KEY_FILE_EXCEEDS_THE_1_MIB_LIMIT)
    );
    let mut bytes = Zeroizing::new(Vec::new());
    file.take(MAX_PRIVATE_KEY_BYTES + 1)
        .read_to_end(&mut bytes)
        .context(crate::i18n::tr(
            i18n_key::CONNECTIONS_CANNOT_READ_THE_PRIVATE_KEY_FILE,
        ))?;
    anyhow::ensure!(
        bytes.len() as u64 <= MAX_PRIVATE_KEY_BYTES,
        crate::i18n::tr(i18n_key::CONNECTIONS_THE_PRIVATE_KEY_FILE_EXCEEDS_THE_1_MIB_LIMIT)
    );
    let text = std::str::from_utf8(&bytes).context(crate::i18n::tr(
        i18n_key::CONNECTIONS_INVALID_PRIVATE_KEY_FILE_ENCODING,
    ))?;
    if passphrase.is_none() && text.contains("-----BEGIN ENCRYPTED PRIVATE KEY-----") {
        return Err(russh::keys::Error::KeyIsEncrypted.into());
    }
    decode_secret_key(text, passphrase).context(crate::i18n::tr(
        i18n_key::CONNECTIONS_INVALID_PRIVATE_KEY_FORMAT_OR_PASSPHRASE,
    ))
}

/// Cancellation is part of the underlying socket's readiness, so dropping a
/// stalled handshake/auth operation cannot leave russh's task holding the TCP
/// connection indefinitely.
struct CancelStream<S> {
    stream: S,
    cancelled: Pin<Box<dyn Future<Output = ()> + Send>>,
    token: CancellationToken,
}
impl<S> CancelStream<S> {
    fn new(stream: S, token: CancellationToken) -> Self {
        Self {
            stream,
            cancelled: Box::pin(token.clone().cancelled_owned()),
            token,
        }
    }
    fn cancelled(&mut self, cx: &mut TaskContext<'_>) -> bool {
        self.token.is_cancelled() || self.cancelled.as_mut().poll(cx).is_ready()
    }
}
impl<S: AsyncRead + Unpin> AsyncRead for CancelStream<S> {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut TaskContext<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<std::io::Result<()>> {
        if self.cancelled(cx) {
            return Poll::Ready(Err(std::io::ErrorKind::ConnectionAborted.into()));
        }
        Pin::new(&mut self.stream).poll_read(cx, buf)
    }
}
impl<S: AsyncWrite + Unpin> AsyncWrite for CancelStream<S> {
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut TaskContext<'_>,
        buf: &[u8],
    ) -> Poll<std::io::Result<usize>> {
        if self.cancelled(cx) {
            return Poll::Ready(Err(std::io::ErrorKind::ConnectionAborted.into()));
        }
        Pin::new(&mut self.stream).poll_write(cx, buf)
    }
    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut TaskContext<'_>) -> Poll<std::io::Result<()>> {
        if self.cancelled(cx) {
            return Poll::Ready(Err(std::io::ErrorKind::ConnectionAborted.into()));
        }
        Pin::new(&mut self.stream).poll_flush(cx)
    }
    fn poll_shutdown(
        mut self: Pin<&mut Self>,
        cx: &mut TaskContext<'_>,
    ) -> Poll<std::io::Result<()>> {
        Pin::new(&mut self.stream).poll_shutdown(cx)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session::{
        Session, actor,
        transport::{Output, Transport},
    };
    use crate::terminal::Terminal;
    use tokio::task::JoinHandle;

    fn test_options() -> SshOptions {
        SshOptions {
            host: "server.example".into(),
            port: 22,
            username: "operator".into(),
            auth: SshAuth::Password(String::new()),
            known_hosts_path: PathBuf::new(),
            timeout_secs: 15,
            keepalive_secs: 30,
            keepalive_max: 3,
            shell_integration: false,
            jump_hosts: Vec::new(),
            forwards: Vec::new(),
            authentication: None,
        }
    }

    fn forwarding_ui(options: SshOptions, stop: &CancellationToken) -> (Ui, JoinHandle<()>) {
        assert!(options.forwards.is_empty());
        let (controller, task) = super::super::forwarding::start(options, stop.child_token());
        (
            Ui::Forwarding {
                controller,
                closed: stop.clone(),
            },
            task,
        )
    }

    #[test]
    fn duplicate_forward_options_are_rejected_before_connection_io() {
        use futures_util::FutureExt as _;

        let mut options = test_options();
        options.forwards = crate::config::ssh::parse_forwards("R 127.0.0.1:2222 host:22").unwrap();
        options.forwards.push(options.forwards[0].clone());
        let (trust, _challenges) = mpsc::channel(1);
        let stop = CancellationToken::new();
        let result = connect_authenticated(
            options,
            Ui::Tunnel {
                trust,
                closed: stop.child_token(),
            },
            stop.clone(),
        )
        .now_or_never()
        .expect("invalid options must fail before starting connection I/O");
        let Err((kind, message)) = result else {
            panic!("duplicate forwarding listeners were accepted");
        };
        assert_eq!(kind, FailureKind::Protocol);
        assert!(message.contains(crate::i18n::tr(
            i18n_key::CONNECTIONS_DUPLICATE_SSH_FORWARDING_LISTENER
        )));
        assert!(stop.is_cancelled());
    }

    fn terminal_verifier(shared: &Arc<Shared>) -> HostVerifier {
        HostVerifier {
            host: "example.test".into(),
            port: 22,
            path: PathBuf::new(),
            _parents: Vec::new(),
            forwarding: None,
            verification: Arc::new(Verification {
                ui: Ui::Terminal(shared.clone()),
                stop: shared.io_stop.child_token(),
                waiting: AtomicBool::new(false),
                failure: Mutex::new(None),
            }),
        }
    }

    #[tokio::test]
    async fn forwarding_verifier_requires_existing_trust_without_changing_terminal() {
        const KEY: &str =
            "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAILM+rvN+ot98qgEN796jTiQfZfG1KaT0PtFDJ/XFSqti";
        const OTHER: &str =
            "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIFhmZmZmZmZmZmZmZmZmZmZmZmZmZmZmZmZmZmZmZmZm";
        let key: PublicKeyOrCertificate = russh::keys::PublicKey::from_openssh(KEY).unwrap().into();
        let directory = tempfile::tempdir().unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(directory.path(), std::fs::Permissions::from_mode(0o700))
                .unwrap();
        }
        let path = directory.path().join("known-hosts.toml");
        let shared = Shared::new(&Terminal::new(10, 2, 0));
        shared.state(SessionState::Connected);
        let terminal_revision = *shared.revision.borrow();

        for (trusted, failure) in [
            (None, Some(FailureKind::HostKeyRejected)),
            (Some(KEY), None),
            (Some(OTHER), Some(FailureKind::HostKeyMismatch)),
        ] {
            KnownHosts::default().save(&path).unwrap();
            if let Some(trusted) = trusted {
                KnownHosts::trust_and_save(&path, "example.test", 22, "ssh-ed25519", trusted)
                    .unwrap();
            }
            let original = std::fs::read(&path).unwrap();
            let stop = shared.io_stop.child_token();
            let (ui, task) = forwarding_ui(test_options(), &stop);
            let mut verifier = terminal_verifier(&shared);
            verifier.path = path.clone();
            verifier.verification = Arc::new(Verification {
                ui,
                stop: stop.clone(),
                waiting: AtomicBool::new(false),
                failure: Mutex::new(None),
            });

            let accepted = tokio::time::timeout(
                Duration::from_secs(1),
                client::Handler::check_server_key(&mut verifier, &key),
            )
            .await
            .expect("auxiliary trust verification must not wait for a user prompt")
            .unwrap();
            assert_eq!(accepted, failure.is_none());
            assert_eq!(
                verifier
                    .verification
                    .failure
                    .lock()
                    .unwrap()
                    .as_ref()
                    .map(|(kind, _)| *kind),
                failure,
            );
            assert_eq!(std::fs::read(&path).unwrap(), original);
            assert!(shared.trust.lock().unwrap().is_none());
            assert!(!shared.io_stop.is_cancelled());
            assert_eq!(shared.current_state(), SessionState::Connected);
            assert_eq!(*shared.revision.borrow(), terminal_revision);
            stop.cancel();
            task.await.unwrap();
            assert!(!shared.io_stop.is_cancelled());
        }
    }

    #[tokio::test]
    async fn forwarding_authentication_preserves_server_context_and_isolates_cancellation() {
        let shared = Shared::new(&Terminal::new(10, 2, 0));
        shared.state(SessionState::Connected);
        let terminal_revision = *shared.revision.borrow();
        for outcome in ["reply", "dismiss", "cancel"] {
            let stop = shared.io_stop.child_token();
            let (sender, mut challenges) = mpsc::channel(1);
            let mut options = test_options();
            options.authentication = Some(sender);
            let (ui, task) = forwarding_ui(options.clone(), &stop);
            let request = request_authentication(
                &options,
                &ui,
                "Server MFA".into(),
                "Enter the current token.\nDo not reuse a previous code.".into(),
                vec![AuthenticationPrompt {
                    label: "One-time code".into(),
                    echo: false,
                }],
                &stop,
            );
            tokio::pin!(request);
            let challenge = tokio::select! {
                challenge = challenges.recv() => challenge.unwrap(),
                _ = &mut request => panic!("authentication ended before its challenge"),
            };
            assert_eq!(challenge.host, options.host);
            assert_eq!(challenge.username, options.username);
            assert_eq!(challenge.name, "Server MFA");
            assert_eq!(
                challenge.instructions,
                format!(
                    "{}\n\nEnter the current token.\nDo not reuse a previous code.",
                    crate::i18n::tr(
                        i18n_key::CONNECTIONS_AUTHENTICATE_THE_SEPARATE_SSH_CONNECTION_USED_FOR_PORT_FORWARDING_THE
                    ),
                ),
            );
            assert_eq!(challenge.prompts.len(), 1);
            assert_eq!(challenge.prompts[0].label, "One-time code");
            assert!(!challenge.prompts[0].echo);
            match outcome {
                "reply" => {
                    assert!(
                        challenge
                            .reply
                            .send(vec![crate::config::Secret::new("123456".into())])
                            .is_ok()
                    );
                    let answers = request.await.unwrap();
                    assert_eq!(answers[0].expose(), "123456");
                    assert!(!stop.is_cancelled());
                }
                "dismiss" => {
                    drop(challenge.reply);
                    assert!(matches!(
                        request.await,
                        Err((FailureKind::Authentication, _))
                    ));
                    assert!(!stop.is_cancelled());
                }
                "cancel" => {
                    challenge.cancelled.cancel();
                    assert!(matches!(
                        request.await,
                        Err((FailureKind::Authentication, _))
                    ));
                    assert!(stop.is_cancelled());
                }
                _ => unreachable!(),
            }
            assert!(!shared.io_stop.is_cancelled());
            assert_eq!(shared.current_state(), SessionState::Connected);
            assert_eq!(*shared.revision.borrow(), terminal_revision);
            stop.cancel();
            task.await.unwrap();
        }
    }

    async fn finish_transport(
        terminal: Terminal,
        shared: Arc<Shared>,
        code: Option<u32>,
        tasks: Vec<JoinHandle<()>>,
    ) {
        let (_commands, receive) = mpsc::channel(1);
        let (output_tx, output) = mpsc::channel(1);
        let (input, _input_rx) = mpsc::channel(1);
        let (exit_tx, exit) = oneshot::channel();
        output_tx.send(Output::Eof).await.unwrap();
        exit_tx.send(Ok(code)).unwrap();
        // Finish the transport normally, then exit the frozen archive loop.
        shared.actor_stop.cancel();
        tokio::time::timeout(
            Duration::from_secs(1),
            actor::run(
                terminal,
                receive,
                shared,
                Some(Transport {
                    output,
                    input,
                    exit,
                    tasks,
                    reaper: None,
                }),
            ),
        )
        .await
        .unwrap();
    }

    #[tokio::test]
    async fn ssh_disconnect_failure_survives_actor_cleanup() {
        let terminal = Terminal::new(10, 2, 0);
        let shared = Shared::new(&terminal);
        shared.state(SessionState::Connected);
        let mut verifier = terminal_verifier(&shared);
        // russh invokes this hook before releasing channel readers. The actor
        // then receives their EOF and cancels io_stop before joining the monitor.
        client::Handler::disconnected(
            &mut verifier,
            client::DisconnectReason::Error(anyhow::anyhow!("connection reset by peer")),
        )
        .await
        .unwrap_err();
        finish_transport(terminal, shared.clone(), None, Vec::new()).await;
        assert!(shared.io_stop.is_cancelled());
        assert_eq!(
            shared.current_state(),
            SessionState::Failed {
                kind: FailureKind::RemoteClosed,
                message: "connection reset by peer".into(),
            }
        );
    }

    #[tokio::test]
    async fn ssh_disconnect_cleanup_keeps_normal_exit_and_user_cancellation() {
        for (user_cancelled, exit_code) in [(false, Some(7)), (true, None)] {
            let terminal = Terminal::new(10, 2, 0);
            let shared = Shared::new(&terminal);
            shared.state(SessionState::Connected);
            let mut verifier = terminal_verifier(&shared);
            let (commands, _receive) = mpsc::channel(1);
            let session = Session {
                commands,
                shared: shared.clone(),
            };
            if user_cancelled {
                session.disconnect();
            }
            let teardown = tokio::spawn(async move {
                verifier.verification.stop.cancelled().await;
                client::Handler::disconnected(
                    &mut verifier,
                    client::DisconnectReason::Error(anyhow::anyhow!("connection aborted")),
                )
                .await
                .unwrap_err();
            });
            finish_transport(terminal, shared.clone(), exit_code, vec![teardown]).await;
            assert_eq!(
                shared.current_state(),
                SessionState::Disconnected { exit_code }
            );
        }
    }

    #[tokio::test]
    async fn ssh_disconnect_preserves_shell_signal_reason() {
        let terminal = Terminal::new(10, 2, 0);
        let shared = Shared::new(&terminal);
        shared.state(SessionState::Connected);
        *shared.remote_failure.lock().unwrap() = Some((
            FailureKind::RemoteClosed,
            "shell terminated by SIGTERM".into(),
        ));
        let mut verifier = terminal_verifier(&shared);
        client::Handler::disconnected(
            &mut verifier,
            client::DisconnectReason::Error(anyhow::anyhow!("connection reset by peer")),
        )
        .await
        .unwrap_err();
        finish_transport(terminal, shared.clone(), None, Vec::new()).await;
        assert_eq!(
            shared.current_state(),
            SessionState::Failed {
                kind: FailureKind::RemoteClosed,
                message: "shell terminated by SIGTERM".into(),
            }
        );
    }

    #[test]
    fn private_key_loader_rejects_large_files_without_reading_them() {
        let file = tempfile::NamedTempFile::new().unwrap();
        file.as_file().set_len(MAX_PRIVATE_KEY_BYTES + 1).unwrap();
        let error = read_private_key(file.path(), None).unwrap_err();
        assert!(error.to_string().contains("1 MiB"));
    }
    #[cfg(unix)]
    #[test]
    fn private_key_loader_rejects_fifo_without_waiting_for_writer() {
        use std::os::unix::ffi::OsStrExt;
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("key.fifo");
        let path_c = std::ffi::CString::new(path.as_os_str().as_bytes()).unwrap();
        // SAFETY: path_c is a valid NUL-terminated path in our private test directory.
        assert_eq!(unsafe { libc::mkfifo(path_c.as_ptr(), 0o600) }, 0);
        let error = read_private_key(&path, None).unwrap_err();
        assert!(error.to_string().contains(crate::i18n::tr(
            i18n_key::CONNECTIONS_THE_PRIVATE_KEY_MUST_BE_A_REGULAR_FILE
        )));
    }
}
