mod startup;
mod worker;

use super::downloads::{Completed as DownloadCompleted, Downloads, Next as DownloadNext};
use super::file_transfer::{Completed as FileCompleted, Transfers};
use super::frames::{FrameClock, FrameSnapshot};
use super::{
    tls,
    transport::{
        Framed, Writer,
        sidebands::{self, Sidebands},
    },
    *,
};
use rustrdp::{
    BulkCompression, ClientConfig, ColorDepth, Connector, Event, Input, MouseButton,
    SecurityProtocol, connection::NegotiationResponse,
};
use std::{collections::BTreeMap, sync::Mutex, time::Duration};
use tokio::{
    net::TcpStream,
    task::JoinSet,
    time::{Instant, sleep_until, timeout},
};
use tokio_rustls::client::TlsStream;
use zeroize::Zeroizing;

use crate::session::{TunnelConnector, stream::NetworkStream};
type Stream = Framed<TlsStream<NetworkStream>>;

pub(super) fn decode_packet<R>(decode: impl FnOnce() -> R) -> R {
    // A single graphics PDU can run for tens of milliseconds without yielding.
    // Hand off this worker's ready tasks (including its non-stealable LIFO slot)
    // before decoding, so queued input/ACK writes and other sessions keep moving.
    tokio::task::block_in_place(decode)
}

enum Interaction {
    Command(QueuedCommand),
    Presented(Option<rustrdp::FrameToken>),
}

/// Service ready input and frame feedback before decoding more graphics, while
/// bounding each burst so network reads, publication and timers still get polled.
struct InteractionBudget(usize);

impl InteractionBudget {
    const BURST: usize = 8;

    fn new() -> Self {
        Self(Self::BURST)
    }

    fn reset(&mut self) {
        self.0 = Self::BURST;
    }

    fn is_exhausted(&self) -> bool {
        self.0 == 0
    }

    fn next(
        &mut self,
        commands: &mut commands::Receiver,
        presented: &mut watch::Receiver<Option<rustrdp::FrameToken>>,
    ) -> Option<Interaction> {
        if self.is_exhausted() {
            return None;
        }
        let next = if presented.has_changed().unwrap_or(false) {
            Interaction::Presented(presented.borrow_and_update().clone())
        } else {
            // Keep FIFO ordering, including button/key barriers between moves.
            Interaction::Command(commands.try_recv().ok()?)
        };
        self.0 -= 1;
        Some(next)
    }
}

enum Delivery {
    Paste { revision: u64 },
    File { offer_id: u64, bytes: u64 },
}

type Deliveries = JoinSet<(Delivery, Result<()>)>;

const MAX_DELIVERIES: usize = 256;

async fn send_with_receipt(
    writer: &Writer,
    deliveries: &mut Deliveries,
    packet: Vec<u8>,
    cancel: Option<CancellationToken>,
    delivery: Delivery,
) -> Result<()> {
    debug_assert!(deliveries.len() < MAX_DELIVERIES);
    let (receipt, received) = oneshot::channel();
    writer.enqueue(packet, cancel, Some(receipt)).await?;
    deliveries.spawn(async move {
        (
            delivery,
            received.await.unwrap_or(Err(RdpError::Disconnected)),
        )
    });
    Ok(())
}

async fn send_paste(
    session: &mut rustrdp::Session,
    writer: &Writer,
    input_order: &tokio::sync::Mutex<()>,
    deliveries: &mut Deliveries,
    cancel: CancellationToken,
    revision: u64,
) -> Result<()> {
    let _order = input_order.lock().await;
    // The complete chord is one packet: cancellation must never leave
    // modifiers pressed in the remote session.
    let mut paste = session.input(Input::Paste)?;
    let [Event::Send(packet)] = paste.as_mut_slice() else {
        return Err(RdpError::Protocol("paste input packet count"));
    };
    send_with_receipt(
        writer,
        deliveries,
        std::mem::take(packet),
        Some(cancel),
        Delivery::Paste { revision },
    )
    .await
}

#[derive(Clone, Debug)]
struct ClipboardRequest {
    revision: u64,
    deadline: Instant,
    paste: Option<CancellationToken>,
}
impl ClipboardRequest {
    fn paste_cancelled(&self) -> bool {
        self.paste
            .as_ref()
            .is_some_and(CancellationToken::is_cancelled)
    }
}

#[derive(Clone)]
pub(super) struct RedirectionState {
    pub tunnel: Option<TunnelConnector>,
    pub pointer: watch::Sender<Option<rustrdp::graphics::PointerState>>,
    pub cache_directory: Option<std::path::PathBuf>,
    pub presentation: Arc<Mutex<Option<rustrdp::FrameToken>>>,
    pub audio_muted: Arc<AtomicBool>,
    pub microphone_enabled: Arc<AtomicBool>,
    pub locked: Arc<AtomicBool>,
    pub capture_transmission: Arc<Mutex<CancellationToken>>,
}

pub(super) async fn run(
    config: RdpConfig,
    password: Secret,
    mut commands: commands::Receiver,
    events: &mpsc::Sender<RdpEvent>,
    frames: watch::Sender<Option<Arc<RdpFrame>>>,
    mut presented: watch::Receiver<Option<rustrdp::FrameToken>>,
    redirection: RedirectionState,
) -> Result<()> {
    let mut password = Some(password);
    let mut handoff: Option<super::handoff::Handoff> = None;
    let mut frame_sequence = 0;
    for _ in 0..8 {
        let mut attempt_config = config.clone();
        if let Some(next) = &handoff {
            attempt_config.username = next.username.clone();
            attempt_config.domain = next.domain.clone();
        }
        let attempt = ConnectionAttempt {
            config: attempt_config,
            password: password.take(),
            handoff,
        };
        handoff = run_connection(
            attempt,
            &mut commands,
            events,
            frames.clone(),
            &mut presented,
            redirection.clone(),
            &mut frame_sequence,
        )
        .await?;
        if handoff.is_none() {
            return Ok(());
        }
        // Old-session commands and presentation tokens do not belong to the
        // destination desktop. All connection-owned transfers have been dropped.
        while commands.try_recv().is_ok() {}
        *redirection
            .presentation
            .lock()
            .unwrap_or_else(|e| e.into_inner()) = None;
        presented.borrow_and_update();
        frames.send_replace(None);
        events
            .send(RdpEvent::Connecting)
            .await
            .map_err(|_| RdpError::Disconnected)?;
    }
    Err(RdpError::Limit("server redirection count"))
}

struct ConnectionAttempt {
    config: RdpConfig,
    password: Option<Secret>,
    handoff: Option<super::handoff::Handoff>,
}

async fn run_connection(
    attempt: ConnectionAttempt,
    commands: &mut commands::Receiver,
    events: &mpsc::Sender<RdpEvent>,
    frames: watch::Sender<Option<Arc<RdpFrame>>>,
    presented: &mut watch::Receiver<Option<rustrdp::FrameToken>>,
    redirection: RedirectionState,
    frame_sequence: &mut u64,
) -> Result<Option<super::handoff::Handoff>> {
    let mut redirection = redirection;
    redirection.pointer.send_replace(None);
    let ready = match startup::connect(attempt, redirection.tunnel.take(), events).await? {
        startup::Started::Redirect(handoff) => return Ok(Some(handoff)),
        startup::Started::Ready(ready) => ready,
    };
    let frontend = worker::Frontend {
        events,
        frames,
        presented,
        frame_sequence,
        redirection,
    };
    let mut worker = worker::Worker::start(*ready, commands, frontend).await?;
    let result = worker.run().await;
    worker.input_worker.finish(commands).await?;
    result
}

fn bundle_capture(outputs: Vec<Event>) -> Vec<Event> {
    let mut bundled = Vec::new();
    for output in outputs {
        match output {
            Event::Send(packet) => {
                if let Some(Event::Send(previous)) = bundled.last_mut() {
                    previous.extend(packet);
                } else {
                    bundled.push(Event::Send(packet));
                }
            }
            Event::SendTunnel { protocol, payloads } => {
                if let Some(Event::SendTunnel {
                    protocol: prior,
                    payloads: previous,
                }) = bundled.last_mut()
                    && *prior == protocol
                {
                    previous.extend(payloads);
                } else {
                    bundled.push(Event::SendTunnel { protocol, payloads });
                }
            }
            event => bundled.push(event),
        }
    }
    bundled
}

fn handle_audio(
    event: rustrdp::AudioEvent,
    playback: &mut Option<audio::Playback>,
    session: &mut rustrdp::Session,
) -> Result<Vec<rustrdp::Event>> {
    let token = match &event {
        rustrdp::AudioEvent::Samples(samples) => Some(samples.token),
        _ => None,
    };
    if let Some(playback) = playback
        && playback.push(event).is_ok()
    {
        return Ok(Vec::new());
    }
    match token {
        Some(token) => Ok(session.confirm_audio(token, 0)?),
        None => Ok(Vec::new()),
    }
}

fn prepare_command(
    command: &RdpCommand,
    clipboard_revision: &mut u64,
    clipboard_request: &mut Option<ClipboardRequest>,
    last_rejection: &mut Option<(Option<u64>, RdpError)>,
) -> Option<u64> {
    // Each explicit clipboard action needs its own result, even when a retry
    // fails identically. Ordinary input still coalesces capability rejections.
    if let RdpCommand::Clipboard { revision, .. } = command {
        *last_rejection = None;
        let cancelled = clipboard_request
            .take()
            .filter(|request| request.paste.is_some())
            .map(|request| request.revision);
        *clipboard_revision = *revision;
        return cancelled;
    }
    // Navigation, new input and focus loss cancel a deferred paste, so a late
    // clipboard ACK cannot type into a different target. Key-up does not.
    if command.invalidates_paste()
        && clipboard_request
            .as_ref()
            .is_some_and(|request| request.paste.is_some())
    {
        return clipboard_request.take().map(|request| request.revision);
    }
    None
}

pub(super) async fn notify_rejection(
    error: impl Into<RdpError>,
    revision: Option<u64>,
    events: &mpsc::Sender<RdpEvent>,
    previous: &mut Option<(Option<u64>, RdpError)>,
) -> Result<()> {
    let error = error.into();
    // Coalesce only delivered rejections. Wait for the UI to receive the result
    // instead of dropping it and leaving a completed request visibly pending.
    if previous
        .as_ref()
        .is_none_or(|(previous_revision, previous_error)| {
            *previous_revision != revision || previous_error != &error
        })
    {
        let event = match revision {
            Some(revision) => RdpEvent::Clipboard {
                revision,
                event: RdpClipboardEvent::Rejected(error.clone()),
            },
            None => RdpEvent::RequestRejected(error.clone()),
        };
        events
            .send(event)
            .await
            .map_err(|_| RdpError::Disconnected)?;
        *previous = Some((revision, error));
    }
    Ok(())
}

pub(super) async fn probe(
    config: RdpConfig,
    password: Secret,
    events: &mpsc::Sender<RdpEvent>,
    tunnel: Option<TunnelConnector>,
) -> Result<()> {
    let (mut framed, negotiation, public_key) =
        begin_trusted(&config, events, tunnel.as_ref()).await?;
    timeout(
        Duration::from_secs(config.timeout_secs),
        nla(
            &mut framed,
            negotiation.protocol,
            &config,
            &password,
            public_key,
        ),
    )
    .await
    .map_err(|_| RdpError::network(ConnectionStage::Authentication, ConnectionFailure::TimedOut))?
}

async fn begin_trusted(
    config: &RdpConfig,
    events: &mpsc::Sender<RdpEvent>,
    tunnel: Option<&TunnelConnector>,
) -> Result<(Stream, NegotiationResponse, Vec<u8>)> {
    let mut pin = config.certificate_sha256.clone();
    let mut accepted = None;
    let connected = loop {
        let challenge = Arc::new(Mutex::new(None));
        let result = begin(config, pin.clone(), challenge.clone(), tunnel).await;
        match result {
            Ok(connected) => {
                if let Some((fingerprint, save)) = accepted.take() {
                    events
                        .send(RdpEvent::CertificateAccepted { fingerprint, save })
                        .await
                        .map_err(|_| RdpError::Disconnected)?;
                }
                break connected;
            }
            Err(error) => {
                let fingerprint = challenge.lock().unwrap_or_else(|e| e.into_inner()).take();
                let Some(fingerprint) = fingerprint else {
                    return Err(error);
                };
                let (reply, answer) = oneshot::channel();
                events
                    .send(RdpEvent::CertificateChallenge {
                        host: config.host.clone(),
                        port: config.port,
                        fingerprint: fingerprint.clone(),
                        previous_fingerprint: pin.clone(),
                        reply,
                    })
                    .await
                    .map_err(|_| RdpError::Disconnected)?;
                let decision = answer.await.unwrap_or(CertificateDecision::Reject);
                if decision == CertificateDecision::Reject {
                    return Err(RdpError::CertificateRejected);
                }
                pin = Some(fingerprint.clone());
                accepted = Some((fingerprint, decision == CertificateDecision::Save));
            }
        }
    };
    Ok(connected)
}

async fn begin(
    config: &RdpConfig,
    pin: Option<String>,
    challenge: tls::Challenge,
    tunnel: Option<&TunnelConnector>,
) -> Result<(Stream, NegotiationResponse, Vec<u8>)> {
    begin_with_request(
        config,
        pin,
        challenge,
        &rustrdp::connection::negotiation_request(),
        false,
        tunnel,
    )
    .await
}

async fn begin_with_request(
    config: &RdpConfig,
    pin: Option<String>,
    challenge: tls::Challenge,
    request: &[u8],
    redirected: bool,
    tunnel: Option<&TunnelConnector>,
) -> Result<(Stream, NegotiationResponse, Vec<u8>)> {
    // One deadline for all pre-authentication steps: reporting the phase must
    // not multiply the configured timeout by the number of phases.
    let deadline = Instant::now() + Duration::from_secs(config.timeout_secs);
    let socket = if let Some(tunnel) = tunnel {
        NetworkStream::Ssh(
            connection_step(deadline, ConnectionStage::Connect, async {
                tunnel
                    .open(config.host.clone(), config.port)
                    .await
                    .map_err(|(kind, message)| RdpError::Ssh { kind, message })
            })
            .await?,
        )
    } else {
        let addresses = connection_step(deadline, ConnectionStage::Resolve, async {
            let addresses: Vec<_> = tokio::net::lookup_host((config.host.as_str(), config.port))
                .await
                .map_err(|error| {
                    let failure = match ConnectionFailure::from(error.kind()) {
                        ConnectionFailure::Other => ConnectionFailure::NameResolution,
                        failure => failure,
                    };
                    RdpError::network(ConnectionStage::Resolve, failure)
                })?
                .collect();
            if addresses.is_empty() {
                return Err(RdpError::network(
                    ConnectionStage::Resolve,
                    ConnectionFailure::NameResolution,
                ));
            }
            Ok(addresses)
        })
        .await?;
        let socket = connection_step(deadline, ConnectionStage::Connect, async {
            TcpStream::connect(addresses.as_slice())
                .await
                .map_err(|error| RdpError::io(ConnectionStage::Connect, error))
        })
        .await?;
        socket
            .set_nodelay(true)
            .map_err(|error| RdpError::io(ConnectionStage::SocketSetup, error))?;
        super::keepalive::configure(&socket, config.keepalive_secs)?;
        NetworkStream::Tcp(socket)
    };
    let mut framed = Framed::new(socket);
    framed.set_stage(ConnectionStage::Negotiation);
    let response = connection_step(deadline, ConnectionStage::Negotiation, async {
        framed.write(request).await?;
        framed.read_pdu().await
    })
    .await?;
    let negotiation = if redirected {
        rustrdp::connection::parse_redirection_negotiation_response(&response)?
    } else {
        rustrdp::connection::parse_negotiation_response_details(&response)?
    };
    let name = rustls::pki_types::ServerName::try_from(config.host.clone())
        .map_err(|_| RdpError::InvalidConfig)?;
    let stream = connection_step(deadline, ConnectionStage::Tls, async {
        tokio_rustls::TlsConnector::from(tls::config(pin, challenge)?)
            .connect(name, framed.into_stream()?)
            .await
            .map_err(|error| {
                // Certificate/protocol failures retain the existing trust gate.
                // Transport failures during TLS retain their network reason.
                if error
                    .get_ref()
                    .is_some_and(|inner| inner.is::<rustls::Error>())
                {
                    RdpError::Tls
                } else {
                    RdpError::io(ConnectionStage::Tls, error)
                }
            })
    })
    .await?;
    let certificate = stream
        .get_ref()
        .1
        .peer_certificates()
        .and_then(|certs| certs.first())
        .ok_or(RdpError::Tls)?;
    let key = tls::public_key(certificate.as_ref())?;
    let mut framed = Framed::new(stream);
    framed.set_stage(ConnectionStage::Authentication);
    Ok((framed, negotiation, key))
}

async fn connection_step<T>(
    deadline: Instant,
    stage: ConnectionStage,
    work: impl std::future::Future<Output = Result<T>>,
) -> Result<T> {
    tokio::time::timeout_at(deadline, work)
        .await
        .map_err(|_| RdpError::network(stage, ConnectionFailure::TimedOut))?
}

async fn nla(
    framed: &mut Stream,
    protocol: SecurityProtocol,
    config: &RdpConfig,
    password: &Secret,
    key: Vec<u8>,
) -> Result<()> {
    use sspi::{
        credssp::{
            ClientMode, ClientState, CredSspClient, CredSspMode, EarlyUserAuthResult, TsRequest,
        },
        generator::GeneratorState,
    };
    let identity = sspi::AuthIdentity {
        username: sspi::Username::new(&config.username, config.domain.as_deref())
            .map_err(|_| RdpError::Authentication)?,
        password: password.expose().to_owned().into(),
    };
    let mut client = CredSspClient::new(
        key,
        identity.into(),
        CredSspMode::WithCredentials,
        ClientMode::Ntlm(sspi::ntlm::NtlmConfig::default()),
        format!("TERMSRV/{}", config.host),
    )
    .map_err(|_| RdpError::Authentication)?;
    let mut request = TsRequest::default();
    loop {
        let result = {
            let mut generator = client.process(request);
            match generator.start() {
                GeneratorState::Completed(result) => {
                    result.map_err(|_| RdpError::Authentication)?
                }
                // NTLM has no outbound KDC/HTTP request. Never follow server-provided endpoints.
                GeneratorState::Suspended(_) => return Err(RdpError::Authentication),
            }
        };
        let (reply, finished) = match result {
            ClientState::ReplyNeeded(reply) => (reply, false),
            ClientState::FinalMessage(reply) => (reply, true),
        };
        let mut buffer = Zeroizing::new(vec![
            0;
            usize::from(
                reply.buffer_len().map_err(|_| RdpError::Authentication)?
            )
        ]);
        reply
            .encode_ts_request(buffer.as_mut_slice())
            .map_err(|_| RdpError::Authentication)?;
        framed.write(&buffer).await?;
        if finished {
            break;
        }
        let bytes = Zeroizing::new(framed.read_credssp().await?);
        request = TsRequest::from_buffer(&bytes).map_err(|_| RdpError::Authentication)?;
    }
    if protocol == SecurityProtocol::HybridEx {
        let bytes = framed.read_early_auth().await?;
        if !matches!(
            EarlyUserAuthResult::from_buffer(bytes.as_slice()),
            Ok(EarlyUserAuthResult::Success)
        ) {
            return Err(RdpError::Authentication);
        }
    }
    Ok(())
}

pub(super) fn input(command: RdpCommand) -> Input {
    match command {
        RdpCommand::Key { scancode, pressed } => Input::Key { scancode, pressed },
        RdpCommand::Unicode(text) => Input::Unicode(text),
        RdpCommand::Pointer { x, y } => Input::Pointer { x, y },
        RdpCommand::MouseButton {
            button,
            pressed,
            x,
            y,
        } => Input::MouseButton {
            button: match button {
                RdpMouseButton::Left => MouseButton::Left,
                RdpMouseButton::Right => MouseButton::Right,
                RdpMouseButton::Middle => MouseButton::Middle,
                RdpMouseButton::Back => MouseButton::Back,
                RdpMouseButton::Forward => MouseButton::Forward,
            },
            pressed,
            x,
            y,
        },
        RdpCommand::Wheel {
            horizontal,
            delta,
            x,
            y,
        } => Input::Wheel {
            horizontal,
            delta,
            x,
            y,
        },
        RdpCommand::ReleaseAll => Input::ReleaseAll,
        RdpCommand::PasteRemote => Input::Paste,
        RdpCommand::Resize { .. }
        | RdpCommand::Clipboard { .. }
        | RdpCommand::SetAudioMuted(_)
        | RdpCommand::SetMicrophoneEnabled(_)
        | RdpCommand::RemoteApp(_)
        | RdpCommand::SetLocked(_)
        | RdpCommand::ReceiveFiles { .. }
        | RdpCommand::CancelReceiveFiles { .. } => {
            unreachable!("handled by session channels")
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn interaction_priority_preserves_drag_edges_and_coalesces_pointer_positions() {
        let (sender, mut commands) = commands::channel(32, 4096);
        let (_feedback, mut presented) = watch::channel(None);
        let button = |pressed, x| RdpCommand::MouseButton {
            button: RdpMouseButton::Left,
            pressed,
            x,
            y: 0,
        };
        for command in [
            button(true, 0),
            RdpCommand::Pointer { x: 10, y: 0 },
            RdpCommand::Pointer { x: 20, y: 0 },
            button(false, 20),
            RdpCommand::Pointer { x: 30, y: 0 },
        ] {
            sender
                .try_send(QueuedCommand::new(command, None), None)
                .unwrap();
        }
        let mut priority = InteractionBudget::new();
        let mut received = Vec::new();
        while let Some(Interaction::Command(queued)) = priority.next(&mut commands, &mut presented)
        {
            match queued.command {
                RdpCommand::MouseButton { pressed, x, .. } => received.push((Some(pressed), x)),
                RdpCommand::Pointer { x, .. } => received.push((None, x)),
                _ => panic!("unexpected drag command"),
            }
        }
        assert_eq!(
            received,
            [(Some(true), 0), (None, 20), (Some(false), 20), (None, 30)]
        );
    }

    #[test]
    fn interaction_flood_yields_to_network_and_frame_publication() {
        let (sender, mut commands) = commands::channel(32, 4096);
        let (feedback, mut presented) = watch::channel(None);
        for scancode in 1..=16 {
            sender
                .try_send(
                    QueuedCommand::new(
                        RdpCommand::Key {
                            scancode,
                            pressed: true,
                        },
                        None,
                    ),
                    None,
                )
                .unwrap();
        }
        let mut priority = InteractionBudget::new();
        // A frame receipt must not wait for an input backlog to drain. Using an
        // empty receipt exercises the watch notification without forging tokens.
        feedback.send_replace(None);
        assert!(matches!(
            priority.next(&mut commands, &mut presented),
            Some(Interaction::Presented(None))
        ));
        for expected in 1..InteractionBudget::BURST {
            let Some(Interaction::Command(queued)) = priority.next(&mut commands, &mut presented)
            else {
                panic!("queued input should precede another graphics decode");
            };
            assert!(
                matches!(queued.command, RdpCommand::Key { scancode, .. } if usize::from(scancode) == expected)
            );
        }
        feedback.send_replace(None);
        assert!(priority.next(&mut commands, &mut presented).is_none());
        assert!(!commands.is_empty());
        assert!(presented.has_changed().unwrap());
        // After giving the fair select a turn, priority resumes.
        priority.reset();
        assert!(matches!(
            priority.next(&mut commands, &mut presented),
            Some(Interaction::Presented(None))
        ));
        let Some(Interaction::Command(queued)) = priority.next(&mut commands, &mut presented)
        else {
            panic!("queued input was lost at the burst boundary");
        };
        assert!(matches!(
            queued.command,
            RdpCommand::Key { scancode: 8, .. }
        ));
    }

    #[tokio::test]
    async fn peer_closes_during_negotiation_or_tls_reports_the_actual_phase() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        for tls_started in [false, true] {
            let listener = tokio::net::TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0))
                .await
                .unwrap();
            let address = listener.local_addr().unwrap();
            let peer = tokio::spawn(async move {
                let (mut socket, _) = listener.accept().await.unwrap();
                let mut request = [0; 19];
                socket.read_exact(&mut request).await.unwrap();
                assert_eq!(
                    request.as_slice(),
                    rustrdp::connection::negotiation_request()
                );
                if tls_started {
                    socket
                        .write_all(&[3, 0, 0, 19, 14, 0xd0, 0, 0, 0, 0, 0, 2, 0, 8, 0, 8, 0, 0, 0])
                        .await
                        .unwrap();
                    // Consume the ClientHello so shutdown is a clean EOF rather
                    // than an OS-dependent reset caused by unread socket data.
                    let mut header = [0; 5];
                    socket.read_exact(&mut header).await.unwrap();
                    let length = usize::from(u16::from_be_bytes([header[3], header[4]]));
                    socket.read_exact(&mut vec![0; length]).await.unwrap();
                }
                socket.shutdown().await.unwrap();
            });
            let config = RdpConfig {
                host: address.ip().to_string(),
                port: address.port(),
                timeout_secs: 2,
                ..RdpConfig::default()
            };
            let error = match begin(&config, None, Arc::new(Mutex::new(None)), None).await {
                Ok(_) => panic!("closed peer unexpectedly completed the handshake"),
                Err(error) => error,
            };
            assert_eq!(
                error,
                RdpError::network(
                    if tls_started {
                        ConnectionStage::Tls
                    } else {
                        ConnectionStage::Negotiation
                    },
                    ConnectionFailure::Closed,
                )
            );
            peer.await.unwrap();
        }
    }

    #[tokio::test]
    async fn connection_deadline_preserves_the_timed_out_phase() {
        for stage in [
            ConnectionStage::Resolve,
            ConnectionStage::Connect,
            ConnectionStage::Negotiation,
            ConnectionStage::Tls,
        ] {
            let result =
                connection_step(Instant::now(), stage, std::future::pending::<Result<()>>()).await;
            assert_eq!(
                result,
                Err(RdpError::network(stage, ConnectionFailure::TimedOut))
            );
        }
    }

    #[tokio::test]
    async fn clipboard_retries_each_report_their_rejection() {
        let (events, mut received) = mpsc::channel(4);
        let mut previous = None;
        let mut request = None;
        let mut current_revision = 0;
        for action in [
            RdpClipboardAction::PasteText("local text".into()),
            RdpClipboardAction::SendText("local text".into()),
            RdpClipboardAction::RequestText,
        ] {
            let command = RdpCommand::Clipboard {
                revision: 1,
                action,
            };
            for _ in 0..2 {
                prepare_command(&command, &mut current_revision, &mut request, &mut previous);
                for _ in 0..2 {
                    notify_rejection(
                        RdpError::ClipboardUnavailable,
                        Some(current_revision),
                        &events,
                        &mut previous,
                    )
                    .await
                    .unwrap();
                }
                assert!(matches!(
                    received.try_recv(),
                    Ok(RdpEvent::Clipboard {
                        revision: 1,
                        event: RdpClipboardEvent::Rejected(RdpError::ClipboardUnavailable),
                    })
                ));
                assert!(matches!(
                    received.try_recv(),
                    Err(mpsc::error::TryRecvError::Empty)
                ));
            }
        }
    }

    #[tokio::test]
    async fn rejection_waits_for_event_queue_capacity() {
        let (events, mut received) = mpsc::channel(1);
        let mut previous = None;
        events.try_send(RdpEvent::Connected).unwrap();
        {
            let notification =
                notify_rejection(RdpError::ClipboardUnavailable, None, &events, &mut previous);
            tokio::pin!(notification);
            tokio::select! {
                biased;
                result = &mut notification => panic!("full queue returned {result:?}"),
                _ = std::future::ready(()) => {}
            }
            assert!(matches!(received.try_recv(), Ok(RdpEvent::Connected)));
            notification.await.unwrap();
        }
        assert!(matches!(
            received.try_recv(),
            Ok(RdpEvent::RequestRejected(RdpError::ClipboardUnavailable))
        ));
        assert_eq!(previous, Some((None, RdpError::ClipboardUnavailable)));
    }

    #[test]
    fn queued_barriers_keep_the_worker_revision_until_consumed() {
        let (commands, mut received) = mpsc::channel(2);
        let mut current_revision = 7;
        let mut request = Some(ClipboardRequest {
            revision: 7,
            deadline: Instant::now() + Duration::from_secs(1),
            paste: Some(CancellationToken::new()),
        });
        let mut previous = None;
        commands
            .try_send(RdpCommand::Clipboard {
                revision: 7,
                action: RdpClipboardAction::Cancel,
            })
            .unwrap();
        commands
            .try_send(RdpCommand::Clipboard {
                revision: 8,
                action: RdpClipboardAction::Cancel,
            })
            .unwrap();
        assert_eq!(current_revision, 7);
        let cancelled = prepare_command(
            &received.try_recv().unwrap(),
            &mut current_revision,
            &mut request,
            &mut previous,
        );
        assert_eq!(cancelled, Some(7));
        assert_eq!(current_revision, 7);
        assert!(request.is_none());
        assert_eq!(
            prepare_command(
                &received.try_recv().unwrap(),
                &mut current_revision,
                &mut request,
                &mut previous,
            ),
            None
        );
        assert_eq!(current_revision, 8);
    }

    #[tokio::test]
    async fn queued_rejection_keeps_its_request_revision_across_a_new_barrier() {
        let (events, mut received) = mpsc::channel(2);
        let (commands, mut incoming) = mpsc::channel(1);
        let mut previous = None;
        let mut current_revision = 7;
        let mut request = None;
        notify_rejection(
            RdpError::ClipboardUnavailable,
            Some(current_revision),
            &events,
            &mut previous,
        )
        .await
        .unwrap();
        commands
            .try_send(RdpCommand::Clipboard {
                revision: 8,
                action: RdpClipboardAction::Cancel,
            })
            .unwrap();
        prepare_command(
            &incoming.try_recv().unwrap(),
            &mut current_revision,
            &mut request,
            &mut previous,
        );
        notify_rejection(
            RdpError::ClipboardUnavailable,
            Some(current_revision),
            &events,
            &mut previous,
        )
        .await
        .unwrap();
        assert!(matches!(
            received.try_recv(),
            Ok(RdpEvent::Clipboard {
                revision: 7,
                event: RdpClipboardEvent::Rejected(RdpError::ClipboardUnavailable),
            })
        ));
        assert!(matches!(
            received.try_recv(),
            Ok(RdpEvent::Clipboard {
                revision: 8,
                event: RdpClipboardEvent::Rejected(RdpError::ClipboardUnavailable),
            })
        ));
        assert!(received.is_empty());
    }

    #[test]
    fn releasing_paste_keys_preserves_the_pending_request() {
        let pending = Some(ClipboardRequest {
            revision: 1,
            deadline: Instant::now() + Duration::from_secs(1),
            paste: Some(CancellationToken::new()),
        });
        let mut request = pending.clone();
        let mut current_revision = 1;
        let mut previous = Some((None, RdpError::ClipboardUnavailable));
        assert_eq!(
            prepare_command(
                &RdpCommand::Key {
                    scancode: 0x1d,
                    pressed: false
                },
                &mut current_revision,
                &mut request,
                &mut previous,
            ),
            None
        );
        assert_eq!(
            request.as_ref().map(|r| r.deadline),
            pending.as_ref().map(|r| r.deadline)
        );
        assert!(request.as_ref().unwrap().paste.is_some());
        assert_eq!(previous, Some((None, RdpError::ClipboardUnavailable)));
        assert_eq!(
            prepare_command(
                &RdpCommand::ReleaseAll,
                &mut current_revision,
                &mut request,
                &mut previous,
            ),
            Some(1)
        );
        assert!(request.is_none());
        assert_eq!(
            prepare_command(
                &RdpCommand::ReleaseAll,
                &mut current_revision,
                &mut request,
                &mut previous,
            ),
            None
        );
    }

    #[test]
    fn sending_clipboard_without_pasting_survives_navigation_and_focus_loss() {
        let pending = Some(ClipboardRequest {
            revision: 1,
            deadline: Instant::now() + Duration::from_secs(1),
            paste: None,
        });
        let mut request = pending.clone();
        let mut current_revision = 1;
        let mut previous = None;
        for command in [
            RdpCommand::Key {
                scancode: 0x0f,
                pressed: true,
            },
            RdpCommand::ReleaseAll,
            RdpCommand::Resize {
                width: 800,
                height: 600,
            },
        ] {
            assert_eq!(
                prepare_command(&command, &mut current_revision, &mut request, &mut previous),
                None
            );
            assert_eq!(
                request.as_ref().map(|r| r.deadline),
                pending.as_ref().map(|r| r.deadline)
            );
            assert!(request.as_ref().unwrap().paste.is_none());
        }
        assert_eq!(
            prepare_command(
                &RdpCommand::Clipboard {
                    revision: 2,
                    action: RdpClipboardAction::RequestText
                },
                &mut current_revision,
                &mut request,
                &mut previous,
            ),
            None
        );
        assert!(request.is_none());
        assert_eq!(current_revision, 2);
    }
}
