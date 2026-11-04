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

type Stream = Framed<TlsStream<TcpStream>>;

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
    deliveries: &mut Deliveries,
    cancel: CancellationToken,
    revision: u64,
) -> Result<()> {
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

pub(super) struct RedirectionState {
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
    let RedirectionState {
        audio_muted,
        microphone_enabled,
        locked,
        capture_transmission,
    } = redirection;
    let desktop_scale = config.desktop_scale.unwrap_or(100);
    let frame_interval = Duration::from_secs(1) / u32::from(config.max_fps);
    let client_config = ClientConfig {
        username: config.username.clone(),
        domain: config.domain.clone().unwrap_or_default(),
        client_name: "RustTerm".into(),
        width: config.width * u16::from(config.monitor_count),
        height: config.height,
        color_depth: match config.color_depth {
            16 => ColorDepth::Bpp16,
            32 => ColorDepth::Bpp32,
            _ => return Err(RdpError::InvalidConfig),
        },
        desktop_scale,
        clipboard: config.clipboard,
        clipboard_files: config.clipboard && config.clipboard_files,
        audio_output: config.audio_output,
        dynamic_resolution: config.uses_dynamic_resolution(),
        bulk_compression: config.compression.then_some(BulkCompression::Rdp5),
        allow_lossy_bitmaps: config.allow_lossy_bitmaps,
        graphics_pipeline: config.allow_lossy_bitmaps && config.color_depth == 32,
        multitransport: true,
        frame_acknowledgements: std::num::NonZeroU32::new(3),
        monitors: if config.monitor_count > 1 {
            (0..config.monitor_count)
                .map(|index| rustrdp::Monitor {
                    left: i32::from(config.width) * i32::from(index),
                    top: 0,
                    width: config.width,
                    height: config.height,
                    primary: index == 0,
                    scale: desktop_scale,
                })
                .collect()
        } else {
            Vec::new()
        },
        audio_input: config.audio_input,
        shared_drives: config
            .shared_folders
            .iter()
            .map(|folder| folder.name.clone())
            .collect(),
        limits: rustrdp::Limits {
            max_width: 8192,
            max_height: 8192,
            max_bitmap_bytes: MAX_FRAME_PIXELS * 4,
            max_channel_message: super::MAX_CLIPBOARD_CONTENT_BYTES + 8,
            ..rustrdp::Limits::default()
        },
    };
    client_config
        .validate()
        .map_err(|_| RdpError::InvalidConfig)?;
    let duration = Duration::from_secs(config.timeout_secs);
    let (mut framed, negotiation, public_key) = begin_trusted(&config, events).await?;
    let (socket, secured) = framed.get_ref().get_ref();
    let peer = socket.peer_addr().map_err(|_| RdpError::Connection)?;
    let certificate = secured
        .peer_certificates()
        .and_then(|chain| chain.first())
        .ok_or(RdpError::Tls)?;
    let mut sidebands = Sidebands::new(
        peer,
        config.host.clone(),
        tls::fingerprint(certificate.as_ref()),
        duration,
    );
    let mut startup_tunnels = std::collections::VecDeque::new();
    let mut startup_tunnel_bytes = 0usize;
    let startup_tunnel_limit = client_config.limits.max_channel_message;

    let mut last_rejection = None;
    let mut clipboard_revision = 0;
    let mut startup_audio = Vec::new();
    let mut startup_files = Vec::new();
    let mut frame_snapshot = FrameSnapshot::with_audit(
        std::env::var_os("RUSTTERM_RDP_AUDIT_DAMAGE").is_some_and(|value| value == "1"),
    );
    let mut completed_frame = None;
    let mut completed_acknowledgement = None;
    let handshake_deadline = Instant::now() + duration;
    let mut session = tokio::time::timeout_at(handshake_deadline, async {
        nla(
            &mut framed,
            negotiation.protocol,
            &config,
            &password,
            public_key,
        )
        .await?;
        let mut connector = Connector::from_negotiation(client_config, negotiation)?;
        framed.write(&connector.start()?).await?;
        loop {
            let output = tokio::select! {
                packet = framed.read_pdu() => connector.advance_with_frames(&packet?, |completed| {
                let mut frame = frame_snapshot.capture_graphics(
                    completed.graphics,
                    completed.damage,
                    completed.bits_per_pixel,
                );
                completed_acknowledgement = completed.acknowledgement;
                frame.acknowledgement = completed_acknowledgement.clone();
                completed_frame = Some(frame);

                })?,
                Some(update) = sidebands.next() => match update {
                    sidebands::Update::Connected { request_id, result } => rustrdp::HandshakeOutput {
                        events: connector.complete_multitransport(request_id, if result.is_ok() { 0 } else { 0x8000_4004 })?,
                        ..rustrdp::HandshakeOutput::default()
                    },
                    sidebands::Update::Data { protocol, packet } => {
                        startup_tunnel_bytes = startup_tunnel_bytes.checked_add(packet.len()).filter(|bytes| *bytes <= startup_tunnel_limit).ok_or(RdpError::Limit("startup tunnel bytes"))?;
                        if startup_tunnels.len() >= 4096 { return Err(RdpError::Limit("startup tunnel packet count")); }
                        startup_tunnels.push_back((protocol, packet));
                        continue;
                    }
                    sidebands::Update::Closed { .. } => return Err(RdpError::Connection),
                },
            };
            for packet in output.packets {
                framed.write(&packet).await?;
            }
            for event in output.events {
                match event {
                    Event::ClipboardOffer => events
                        .send(RdpEvent::Clipboard {
                            revision: clipboard_revision,
                            event: RdpClipboardEvent::Offer,
                        })
                        .await
                        .map_err(|_| RdpError::Disconnected)?,
                    Event::ClipboardText(text) => events
                        .send(RdpEvent::Clipboard {
                            revision: clipboard_revision,
                            event: RdpClipboardEvent::Text(text),
                        })
                        .await
                        .map_err(|_| RdpError::Disconnected)?,
                    Event::ClipboardContent(content) => events
                        .send(RdpEvent::Clipboard {
                            revision: clipboard_revision,
                            event: RdpClipboardEvent::Content(content),
                        })
                        .await
                        .map_err(|_| RdpError::Disconnected)?,
                    Event::ClipboardUnavailable => events
                        .send(RdpEvent::Clipboard {
                            revision: clipboard_revision,
                            event: RdpClipboardEvent::Unavailable,
                        })
                        .await
                        .map_err(|_| RdpError::Disconnected)?,
                    Event::RequestRejected(error) => {
                        notify_rejection(error, None, events, &mut last_rejection).await?
                    }
                    Event::Disconnected { .. } => return Err(RdpError::Disconnected),
                    event @ (Event::Capture(_) | Event::DriveRequest(_)) => {
                        if startup_files.len() >= 128 {
                            return Err(RdpError::Limit("startup redirection request count"));
                        }
                        startup_files.push(event);
                    }
                    Event::Audio(event) => {
                        if startup_audio.len() >= 128 {
                            return Err(RdpError::Limit("startup audio event count"));
                        }
                        startup_audio.push(event);
                    }
                    Event::ClipboardFileRequest(_) | Event::ClipboardFilesReleased { .. } => {}
                    event @ (Event::ClipboardFiles { .. }
                    | Event::ClipboardFileContents { .. }
                    | Event::ClipboardRemoteFilesCancelled { .. }) => {
                        if startup_files.len() >= 32 {
                            return Err(RdpError::Limit("startup clipboard file event count"));
                        }
                        startup_files.push(event);
                    }
                    Event::Send(packet) => framed.write(&packet).await?,
                    Event::MultitransportRequest(request) => {
                        let id = request.id;
                        if !sidebands.open(request, Some(handshake_deadline)) {
                            for response in connector.complete_multitransport(id, 0x8000_4004)? {
                                if let Event::Send(packet) = response { framed.write(&packet).await?; }
                            }
                        }
                    }
                    Event::SendTunnel { protocol, payloads } => sidebands.send(protocol, payloads, None).await?,
                    Event::MultitransportSynchronized { packets } => {
                        for packet in packets { framed.write(&packet).await?; }
                    },
                    Event::GraphicsChanged
                    | Event::Resized { .. }
                    | Event::GraphicsFrameBegin { .. }
                    | Event::GraphicsFrameEnd { .. }
                    | Event::ClipboardPublished
                    | Event::ClipboardPublishRejected => {}
                }
            }
            if let Some(session) = output.session {
                break Ok::<_, RdpError>(session);
            }
        }
    })
    .await
    .map_err(|_| RdpError::Timeout)??;
    drop(password);
    let (mut framed, outgoing) = framed.split();
    let mut writer = Writer::new(outgoing, duration);
    let mut deliveries = Deliveries::new();
    let mut frame_clock = FrameClock::new(frame_interval, Instant::now());
    let mut frame_sequence = 0u64;
    let mut frame_receive_deadline = session
        .graphics_frame_in_progress()
        .then(|| Instant::now() + duration);
    let mut performance = RdpPerformance::default();
    let mut performance_started = Instant::now();
    let mut performance_deadline = performance_started + Duration::from_secs(5);
    let mut activation_deadline = None;
    let mut clipboard_request: Option<ClipboardRequest> = None;
    let mut queued_paste: Option<CancellationToken> = None;
    let mut files = Transfers::new();
    let mut downloads = Downloads::new();
    let mut remote_files: Option<(u64, Vec<rustrdp::ClipboardFile>)> = None;
    let mut download_deadline = None;
    let mut progress = BTreeMap::new();
    let mut progress_deadline = None;
    let mut capture: Option<audio::Capture> = None;
    let mut drives = if config.shared_folders.is_empty() {
        None
    } else {
        let folders = config.shared_folders.clone();
        let share_lock = locked.clone();
        Some(
            timeout(
                duration,
                tokio::task::spawn_blocking(move || {
                    super::shared_drives::SharedDrives::new(&folders, share_lock)
                }),
            )
            .await
            .map_err(|_| RdpError::Timeout)?
            .map_err(|_| RdpError::Runtime)??,
        )
    };
    let mut playback = if config.audio_output {
        audio::Playback::new(audio_muted).ok()
    } else {
        None
    };
    events
        .send(RdpEvent::Connected)
        .await
        .map_err(|_| RdpError::Disconnected)?;
    let mut initial_outputs = std::collections::VecDeque::new();
    initial_outputs.extend(startup_files);
    for event in startup_audio {
        initial_outputs.extend(handle_audio(event, &mut playback, &mut session)?);
    }
    if config.audio_output && playback.is_none() {
        notify_rejection(
            RdpError::AudioUnavailable,
            None,
            events,
            &mut last_rejection,
        )
        .await?;
    }
    let mut yield_deadline = Instant::now() + Duration::from_millis(2);
    loop {
        // Buffered packets and local commands can remain ready indefinitely.
        // Let the writer and other sessions run between bounded batches of work.
        if Instant::now() >= yield_deadline {
            tokio::task::yield_now().await;
            yield_deadline = Instant::now() + Duration::from_millis(2);
        }
        enum Incoming {
            Pdu {
                packet: tokio_util::bytes::Bytes,
                tunnel: Option<rustrdp::multitransport::TransportProtocol>,
            },
            TunnelConnected(u32, Result<()>),
            TunnelClosed(rustrdp::multitransport::TransportProtocol),
            Command(QueuedCommand),
            Publish,
            Performance,
            Presented(Option<rustrdp::FrameToken>),
            Progress,
            Download(Option<DownloadCompleted>),
            DownloadTimedOut,
            ClipboardTimedOut,
            Outputs(Vec<rustrdp::Event>),
            File(Option<std::result::Result<FileCompleted, tokio::task::JoinError>>),
            Audio(Option<audio::Completion>),
            Capture(Option<audio::Captured>),
            Drive(Option<rustrdp::DriveCompletion>),
            Delivered(Option<std::result::Result<(Delivery, Result<()>), tokio::task::JoinError>>),
        }
        // Each iteration adds at most one receipt. Reap completions first and
        // apply backpressure before accepting work when all slots are occupied.
        let incoming = if let Some(delivered) = deliveries.try_join_next() {
            Incoming::Delivered(Some(delivered))
        } else if deliveries.len() >= MAX_DELIVERIES {
            Incoming::Delivered(deliveries.join_next().await)
        } else if let Some(completed) = playback
            .as_mut()
            .and_then(|playback| playback.completions.try_recv().ok())
        {
            // Acknowledge consumed/dropped sound before accepting another block.
            Incoming::Audio(Some(completed))
        } else if let Some(completed) = drives
            .as_mut()
            .and_then(|drives| drives.completions.try_recv().ok())
        {
            Incoming::Drive(Some(completed))
        } else if let Some((protocol, packet)) = startup_tunnels.pop_front() {
            Incoming::Pdu {
                packet,
                tunnel: Some(protocol),
            }
        } else if !initial_outputs.is_empty() {
            Incoming::Outputs(initial_outputs.drain(..).collect())
        } else if clipboard_request
            .as_ref()
            .is_some_and(ClipboardRequest::paste_cancelled)
        {
            // Drain to the already accepted cancellation. A replacement paste
            // owns its feedback; the old ACK or timeout must not overwrite it.
            match commands.recv().await {
                Some(command) => Incoming::Command(command),
                None => return Ok(()),
            }
        } else if frame_clock.is_due(Instant::now()) {
            Incoming::Publish
        } else {
            tokio::select! {
                result = framed.read_pdu() => Incoming::Pdu { packet: result?, tunnel: None },
                Some(update) = sidebands.next() => match update {
                    sidebands::Update::Connected { request_id, result } => Incoming::TunnelConnected(request_id, result),
                    sidebands::Update::Data { protocol, packet } => Incoming::Pdu { packet, tunnel: Some(protocol) },
                    sidebands::Update::Closed { protocol } => Incoming::TunnelClosed(protocol),
                },
                command = commands.recv() => match command { Some(command) => Incoming::Command(command), None => return Ok(()) },
                changed = presented.changed() => {
                    if changed.is_err() { return Ok(()); }
                    Incoming::Presented(presented.borrow_and_update().clone())
                },
                _ = sleep_until(frame_clock.deadline().unwrap_or_else(Instant::now)), if frame_clock.deadline().is_some() => Incoming::Publish,
                _ = sleep_until(frame_receive_deadline.unwrap_or_else(Instant::now)), if frame_receive_deadline.is_some() => return Err(RdpError::Timeout),
                _ = sleep_until(performance_deadline) => Incoming::Performance,
                _ = sleep_until(progress_deadline.unwrap_or_else(Instant::now)), if progress_deadline.is_some() => Incoming::Progress,
                _ = sleep_until(download_deadline.unwrap_or_else(Instant::now)), if download_deadline.is_some() => Incoming::DownloadTimedOut,
                _ = sleep_until(activation_deadline.unwrap_or_else(Instant::now)), if activation_deadline.is_some() => return Err(RdpError::Timeout),
                _ = sleep_until(clipboard_request.as_ref().map_or_else(Instant::now, |request| request.deadline)), if clipboard_request.is_some() => Incoming::ClipboardTimedOut,
                file = files.next(), if files.is_busy() => Incoming::File(file),
                completed = downloads.next(), if downloads.is_busy() => Incoming::Download(completed),
                delivered = deliveries.join_next(), if !deliveries.is_empty() => Incoming::Delivered(delivered),
                error = writer.errors.recv() => return Err(error.unwrap_or(RdpError::Disconnected)),
                captured = async { match &mut capture { Some(capture) => capture.events.recv().await, None => std::future::pending().await } } => Incoming::Capture(captured),
                completion = async { match &mut drives { Some(drives) => drives.completions.recv().await, None => std::future::pending().await } } => Incoming::Drive(completion),
                completed = async {
                    match &mut playback {
                        Some(playback) => playback.completions.recv().await,
                        None => std::future::pending().await,
                    }
                } => Incoming::Audio(completed),
            }
        };
        let command_revision = match &incoming {
            Incoming::Command(QueuedCommand {
                command: RdpCommand::Clipboard { revision, .. },
                ..
            }) => Some(*revision),
            _ => None,
        };
        let mut capture_packet = None;
        let was_active = session.is_active();
        let outputs = match incoming {
            Incoming::Outputs(outputs) => outputs,
            Incoming::TunnelConnected(id, result) => {
                session.complete_multitransport(id, if result.is_ok() { 0 } else { 0x8000_4004 })?
            }
            Incoming::TunnelClosed(protocol) => session
                .close_multitransport(protocol)
                .map_err(|_| RdpError::Connection)?,
            Incoming::Presented(Some(token)) => {
                let outputs = session.acknowledge_frame(&token)?;
                performance.acknowledged_frames += outputs.len() as u64;
                outputs
            }
            Incoming::Presented(None) => continue,
            Incoming::Delivered(Some(Ok((Delivery::Paste { revision }, result)))) => {
                let event = match result {
                    Ok(()) => RdpClipboardEvent::PasteSent,
                    Err(RdpError::ClipboardUnavailable) => RdpClipboardEvent::PasteCancelled,
                    Err(error) => return Err(error),
                };
                events
                    .send(RdpEvent::Clipboard { revision, event })
                    .await
                    .map_err(|_| RdpError::Disconnected)?;
                continue;
            }
            Incoming::Delivered(Some(Ok((Delivery::File { offer_id, bytes }, result)))) => {
                result?;
                if let Some(event) = files.sent(offer_id, bytes) {
                    if matches!(
                        event,
                        RdpEvent::FileTransfer {
                            available: false,
                            ..
                        }
                    ) {
                        progress.remove(&(false, offer_id));
                        events
                            .send(event)
                            .await
                            .map_err(|_| RdpError::Disconnected)?;
                    } else {
                        progress.insert((false, offer_id), event);
                        progress_deadline
                            .get_or_insert_with(|| Instant::now() + Duration::from_millis(100));
                    }
                }
                continue;
            }
            Incoming::Delivered(_) => return Err(RdpError::Runtime),
            Incoming::Capture(Some(audio::Captured::Opened {
                generation,
                available,
            })) => {
                if !available {
                    capture = None;
                    notify_rejection(
                        RdpError::MicrophoneUnavailable,
                        None,
                        events,
                        &mut last_rejection,
                    )
                    .await?;
                }
                session.confirm_capture(generation, available)?
            }
            Incoming::Capture(Some(audio::Captured::Samples {
                generation,
                data,
                transmission,
                discontinuity,
            })) => {
                if transmission.is_cancelled()
                    || locked.load(Ordering::Acquire)
                    || !microphone_enabled.load(Ordering::Acquire)
                {
                    continue;
                }
                let mut outputs = if discontinuity {
                    session.reset_capture(generation)?
                } else {
                    Vec::new()
                };
                outputs.extend(session.capture_samples(generation, &data)?);
                capture_packet = Some(transmission);
                bundle_capture(outputs)
            }
            Incoming::Capture(Some(audio::Captured::Failed) | None) => {
                capture = None;
                notify_rejection(
                    RdpError::MicrophoneUnavailable,
                    None,
                    events,
                    &mut last_rejection,
                )
                .await?;
                session.close_capture()?
            }
            Incoming::Drive(Some(completion)) => session.complete_drive(completion)?,
            Incoming::Drive(None) => return Err(RdpError::FileUnavailable),
            Incoming::Audio(Some(audio::Completion::Consumed { token, elapsed_ms })) => {
                session.confirm_audio(token, elapsed_ms)?
            }
            Incoming::Audio(Some(audio::Completion::Unavailable)) => {
                notify_rejection(
                    RdpError::AudioUnavailable,
                    None,
                    events,
                    &mut last_rejection,
                )
                .await?;
                continue;
            }
            Incoming::Audio(None) => {
                playback = None;
                notify_rejection(
                    RdpError::AudioUnavailable,
                    None,
                    events,
                    &mut last_rejection,
                )
                .await?;
                continue;
            }
            Incoming::File(Some(Ok(FileCompleted::Prepared { revision, result }))) => {
                if revision != clipboard_revision
                    || clipboard_request.as_ref().is_none_or(|request| {
                        request.revision != revision
                            || request.paste_cancelled()
                            || Instant::now() >= request.deadline
                    })
                {
                    continue;
                }
                match result {
                    Ok(sources) => match session
                        .set_clipboard_files(revision, sources.descriptions())
                    {
                        Ok(outputs) => {
                            let event = files.retain(revision, sources);
                            events
                                .send(event)
                                .await
                                .map_err(|_| RdpError::Disconnected)?;
                            outputs
                        }
                        Err(
                            error @ (rustrdp::Error::Unsupported(_)
                            | rustrdp::Error::Limit(_)
                            | rustrdp::Error::Invalid(_)),
                        ) => {
                            clipboard_request = None;
                            notify_rejection(error, Some(revision), events, &mut last_rejection)
                                .await?;
                            continue;
                        }
                        Err(error) => return Err(error.into()),
                    },
                    Err(error) => {
                        clipboard_request = None;
                        notify_rejection(error, Some(revision), events, &mut last_rejection)
                            .await?;
                        continue;
                    }
                }
            }
            Incoming::File(Some(Ok(FileCompleted::Read { request, result }))) => {
                if !files.contains(request.offer_id) {
                    continue;
                }
                match result {
                    Ok(data) => {
                        let bytes = data.len() as u64;
                        let response = session.clipboard_file_contents(request.id, Some(data))?;
                        let last_packet = response
                            .iter()
                            .rposition(|event| matches!(event, Event::Send(_)));
                        let mut outputs = Vec::new();
                        for (index, event) in response.into_iter().enumerate() {
                            match event {
                                Event::Send(packet) if Some(index) == last_packet => {
                                    send_with_receipt(
                                        &writer,
                                        &mut deliveries,
                                        packet,
                                        None,
                                        Delivery::File {
                                            offer_id: request.offer_id,
                                            bytes,
                                        },
                                    )
                                    .await?;
                                    files.queued(request.offer_id);
                                }
                                Event::Send(packet) => writer.enqueue(packet, None, None).await?,
                                event => outputs.push(event),
                            }
                        }
                        outputs
                    }
                    Err(error) => {
                        let outputs = session.clipboard_file_contents(request.id, None)?;
                        notify_rejection(error, None, events, &mut last_rejection).await?;
                        outputs
                    }
                }
            }
            Incoming::File(Some(Err(_))) => {
                files.cancel();
                notify_rejection(RdpError::FileUnavailable, None, events, &mut last_rejection)
                    .await?;
                session.cancel_clipboard_files()?
            }
            Incoming::File(None) => continue,
            Incoming::Download(Some(mut completed)) => {
                let offer_id = completed.offer_id;
                if remote_files.as_ref().is_none_or(|(id, _)| *id != offer_id) {
                    continue;
                }
                if download_deadline.is_some_and(|deadline| Instant::now() >= deadline) {
                    downloads.cancel();
                    completed.result = Err(RdpError::Timeout);
                }
                match completed.result {
                    Ok(DownloadNext::Request {
                        index,
                        offset,
                        length,
                    }) => {
                        download_deadline = Some(Instant::now() + duration);
                        progress.insert(
                            (true, offer_id),
                            RdpEvent::RemoteFilesProgress {
                                offer_id,
                                bytes_received: completed.bytes_received,
                                total_bytes: completed.total_bytes,
                            },
                        );
                        progress_deadline
                            .get_or_insert_with(|| Instant::now() + Duration::from_millis(100));
                        session.request_clipboard_file(offer_id, index, offset, length)?
                    }
                    result => {
                        remote_files = None;
                        download_deadline = None;
                        progress.remove(&(true, offer_id));
                        let event = match result {
                            Ok(DownloadNext::Ready { directory, paths }) => {
                                RdpEvent::RemoteFilesReady {
                                    offer_id,
                                    directory,
                                    paths,
                                }
                            }
                            Err(error) => RdpEvent::RemoteFilesFailed { offer_id, error },
                            Ok(DownloadNext::Request { .. }) => unreachable!(),
                        };
                        events
                            .send(event)
                            .await
                            .map_err(|_| RdpError::Disconnected)?;
                        session.cancel_remote_files(offer_id)?
                    }
                }
            }
            Incoming::Download(None) => continue,
            Incoming::DownloadTimedOut => {
                downloads.cancel();
                download_deadline = None;
                let Some((offer_id, _)) = remote_files.take() else {
                    continue;
                };
                progress.remove(&(true, offer_id));
                events
                    .send(RdpEvent::RemoteFilesFailed {
                        offer_id,
                        error: RdpError::Timeout,
                    })
                    .await
                    .map_err(|_| RdpError::Disconnected)?;
                session.cancel_remote_files(offer_id)?
            }
            Incoming::Progress => {
                // Coalesce byte counters and reserve UI capacity for clipboard
                // handshakes. Retry the last update even when no more bytes arrive.
                while events.capacity() > 8 {
                    let Some((offer_id, event)) = progress.pop_first() else {
                        break;
                    };
                    match events.try_send(event) {
                        Ok(()) => {}
                        Err(mpsc::error::TrySendError::Full(event)) => {
                            progress.insert(offer_id, event);
                            break;
                        }
                        Err(mpsc::error::TrySendError::Closed(_)) => {
                            return Err(RdpError::Disconnected);
                        }
                    }
                }
                progress_deadline =
                    (!progress.is_empty()).then(|| Instant::now() + Duration::from_millis(100));
                continue;
            }
            Incoming::Publish => {
                let started = Instant::now();
                let mut frame = if !session.graphics_frame_in_progress() {
                    // Include legacy updates or local cursor changes after END.
                    // The snapshot cache shares all unchanged completed tiles.
                    completed_frame = None;
                    let mut frame = frame_snapshot.capture(&mut session);
                    frame.acknowledgement = completed_acknowledgement.clone();
                    let elapsed = started.elapsed();
                    performance.snapshot_time += elapsed;
                    performance.max_snapshot_time = performance.max_snapshot_time.max(elapsed);
                    frame
                } else if let Some(frame) = completed_frame.take() {
                    frame
                } else {
                    frame_clock.suspend();
                    continue;
                };
                frame_sequence = frame_sequence.wrapping_add(1);
                frame.sequence = frame_sequence;
                frame.published_at = std::time::Instant::now();
                frames.send_replace(Some(Arc::new(frame)));
                frame_clock.published(started);
                performance.frames += 1;
                continue;
            }
            Incoming::Performance => {
                let now = Instant::now();
                performance_deadline = now + Duration::from_secs(5);
                if performance.packets == 0 && performance.frames == 0 {
                    performance_started = now;
                } else if let Ok(permit) = events.try_reserve() {
                    performance.interval = now - performance_started;
                    performance.surface_updates = session.info().surface_updates();
                    performance.nscodec = session.info().nscodec();
                    performance.frame_acknowledgements = session.info().frame_acknowledgements()
                        || completed_acknowledgement.is_some();
                    performance.graphics_codec = session.graphics_codec();
                    performance.udp_connections = sidebands.connected();
                    performance.snapshot_audit = frame_snapshot
                        .take_audit()
                        .map(|audit| (audit.captures, audit.checked_tiles, audit.mismatched_tiles));
                    if performance.snapshot_audit.is_some() {
                        performance.avc_decode_audit = Some(session.take_avc_decode_counts());
                    }
                    permit.send(RdpEvent::Performance(std::mem::take(&mut performance)));
                    performance_started = now;
                }
                continue;
            }
            Incoming::Pdu { packet, tunnel } => {
                let started = Instant::now();
                let mut snapshot_time = Duration::ZERO;
                let on_frame = |completed: rustrdp::GraphicsFrame<'_>| {
                    // END can be followed by the next frame in the same PDU.
                    // Capture here, before those later updates mutate the desktop.
                    let capture_started = Instant::now();
                    let mut frame = frame_snapshot.capture_graphics(
                        completed.graphics,
                        completed.damage,
                        completed.bits_per_pixel,
                    );
                    completed_acknowledgement = completed.acknowledgement;
                    frame.acknowledgement = completed_acknowledgement.clone();
                    completed_frame = Some(frame);
                    let elapsed = capture_started.elapsed();
                    snapshot_time += elapsed;
                    performance.snapshot_time += elapsed;
                    performance.max_snapshot_time = performance.max_snapshot_time.max(elapsed);
                    performance.completed_frames += 1;
                    frame_clock.dirty(Instant::now());
                };
                let outputs = if let Some(protocol) = tunnel {
                    session.process_tunnel_with_frames(protocol, &packet, on_frame)?
                } else {
                    session.process_with_frames(&packet, on_frame)?
                };
                let elapsed = started.elapsed().saturating_sub(snapshot_time);
                performance.packets += 1;
                performance.fastpath_packets += u64::from(tunnel.is_none() && packet[0] & 3 == 0);
                performance.udp_packets += u64::from(tunnel.is_some());
                performance.wire_bytes += packet.len() as u64;
                performance.server_graphics_updates +=
                    u64::from(outputs.iter().any(|event| {
                        matches!(event, Event::GraphicsChanged | Event::Resized { .. })
                    }));
                performance.process_time += elapsed;
                performance.max_process_time = performance.max_process_time.max(elapsed);
                outputs
            }
            Incoming::ClipboardTimedOut => {
                files.cancel_preparation();
                if clipboard_request
                    .as_ref()
                    .is_some_and(ClipboardRequest::paste_cancelled)
                {
                    continue;
                }
                if let Some(request) = clipboard_request.take() {
                    notify_rejection(
                        RdpError::ClipboardUnavailable,
                        Some(request.revision),
                        events,
                        &mut last_rejection,
                    )
                    .await?;
                }
                continue;
            }
            Incoming::Command(QueuedCommand { command, paste }) => {
                if matches!(command, RdpCommand::Clipboard { .. }) {
                    files.cancel_preparation();
                }
                if let Some(revision) = prepare_command(
                    &command,
                    &mut clipboard_revision,
                    &mut clipboard_request,
                    &mut last_rejection,
                ) {
                    events
                        .send(RdpEvent::Clipboard {
                            revision,
                            event: RdpClipboardEvent::PasteCancelled,
                        })
                        .await
                        .map_err(|_| RdpError::Disconnected)?;
                }
                let result = match command {
                    RdpCommand::ReceiveFiles { offer_id } => {
                        let Some((id, offered)) = &remote_files else {
                            continue;
                        };
                        if *id != offer_id || download_deadline.is_some() {
                            continue;
                        }
                        match downloads.begin(offer_id, offered.clone()) {
                            Ok(()) => {
                                download_deadline = Some(Instant::now() + duration);
                                continue;
                            }
                            Err(error) => {
                                remote_files = None;
                                events
                                    .send(RdpEvent::RemoteFilesFailed { offer_id, error })
                                    .await
                                    .map_err(|_| RdpError::Disconnected)?;
                                session.cancel_remote_files(offer_id)
                            }
                        }
                    }
                    RdpCommand::CancelReceiveFiles { offer_id } => {
                        if remote_files.as_ref().is_some_and(|(id, _)| *id == offer_id) {
                            downloads.cancel();
                            download_deadline = None;
                            progress.remove(&(true, offer_id));
                        }
                        session.cancel_remote_files(offer_id)
                    }
                    RdpCommand::PasteRemote => {
                        let cancel = paste.expect("remote paste carries a cancellation token");
                        if session.is_active() && !cancel.is_cancelled() {
                            queued_paste = Some(cancel.clone());
                            send_paste(
                                &mut session,
                                &writer,
                                &mut deliveries,
                                cancel,
                                clipboard_revision,
                            )
                            .await?;
                        }
                        continue;
                    }
                    RdpCommand::SetMicrophoneEnabled(_) | RdpCommand::SetLocked(_) => {
                        if let Some(capture) = &capture {
                            capture.refresh();
                        }
                        continue;
                    }
                    RdpCommand::SetAudioMuted(_) => {
                        if let Some(playback) = &playback {
                            playback.refresh_mute();
                        }
                        continue;
                    }
                    RdpCommand::Resize { width, height } => session.resize(width & !1, height),
                    RdpCommand::Clipboard {
                        revision,
                        action:
                            action @ (RdpClipboardAction::SendText(_)
                            | RdpClipboardAction::PasteText(_)
                            | RdpClipboardAction::SendContent(_)
                            | RdpClipboardAction::PasteContent(_)),
                    } => {
                        if paste.as_ref().is_some_and(CancellationToken::is_cancelled) {
                            clipboard_request = Some(ClipboardRequest {
                                revision,
                                deadline: Instant::now() + duration,
                                paste,
                            });
                            continue;
                        }
                        if paste.is_some() && !session.is_active() {
                            notify_rejection(
                                RdpError::ClipboardUnavailable,
                                Some(revision),
                                events,
                                &mut last_rejection,
                            )
                            .await?;
                            continue;
                        }
                        let result = match action {
                            RdpClipboardAction::SendText(text)
                            | RdpClipboardAction::PasteText(text) => session.set_clipboard(text),
                            RdpClipboardAction::SendContent(content)
                            | RdpClipboardAction::PasteContent(content) => {
                                session.set_clipboard_content(content)
                            }
                            _ => unreachable!("clipboard content action was matched above"),
                        };
                        if result.is_ok() {
                            clipboard_request = Some(ClipboardRequest {
                                revision,
                                deadline: Instant::now() + duration,
                                paste,
                            });
                        }
                        result
                    }
                    RdpCommand::Clipboard {
                        revision,
                        action:
                            RdpClipboardAction::SendFiles(paths) | RdpClipboardAction::PasteFiles(paths),
                    } => {
                        if !config.clipboard || !config.clipboard_files {
                            notify_rejection(
                                RdpError::ClipboardUnavailable,
                                Some(revision),
                                events,
                                &mut last_rejection,
                            )
                            .await?;
                            continue;
                        }
                        if paste.as_ref().is_some_and(CancellationToken::is_cancelled) {
                            clipboard_request = Some(ClipboardRequest {
                                revision,
                                deadline: Instant::now() + duration,
                                paste,
                            });
                            continue;
                        }
                        if paste.is_some() && !session.is_active() {
                            notify_rejection(
                                RdpError::ClipboardUnavailable,
                                Some(revision),
                                events,
                                &mut last_rejection,
                            )
                            .await?;
                            continue;
                        }
                        match files.prepare(revision, paths) {
                            Ok(()) => {
                                clipboard_request = Some(ClipboardRequest {
                                    revision,
                                    deadline: Instant::now() + duration,
                                    paste,
                                })
                            }
                            Err(error) => {
                                notify_rejection(error, Some(revision), events, &mut last_rejection)
                                    .await?
                            }
                        }
                        continue;
                    }
                    RdpCommand::Clipboard {
                        action: RdpClipboardAction::CancelFiles,
                        ..
                    } => {
                        files.cancel();
                        session.cancel_clipboard_files()
                    }
                    RdpCommand::Clipboard {
                        action: RdpClipboardAction::RequestText,
                        ..
                    } => session.request_clipboard(),
                    RdpCommand::Clipboard {
                        action: RdpClipboardAction::Cancel,
                        ..
                    } => Ok(Vec::new()),
                    command => session.input(input(command)),
                };
                match result {
                    Ok(outputs) => outputs,
                    Err(error @ (rustrdp::Error::Unsupported(_) | rustrdp::Error::Limit(_))) => {
                        notify_rejection(error, command_revision, events, &mut last_rejection)
                            .await?;
                        continue;
                    }
                    Err(error) => return Err(error.into()),
                }
            }
        };
        let mut outputs = std::collections::VecDeque::from(outputs);
        while let Some(output) = outputs.pop_front() {
            match output {
                Event::Capture(rustrdp::CaptureEvent::Open(request)) => {
                    capture = None;
                    match audio::Capture::new(
                        request,
                        microphone_enabled.clone(),
                        locked.clone(),
                        capture_transmission.clone(),
                    ) {
                        Ok(worker) => capture = Some(worker),
                        Err(error) => {
                            outputs.extend(session.confirm_capture(request.generation, false)?);
                            notify_rejection(error, None, events, &mut last_rejection).await?;
                        }
                    }
                }
                Event::Capture(rustrdp::CaptureEvent::Close) => {
                    capture = None;
                }
                Event::DriveRequest(request) => {
                    if let Some(drives) = &drives {
                        drives.request(request)?;
                    } else {
                        return Err(RdpError::Protocol("drive request without redirection"));
                    }
                }
                Event::Send(packet) => writer.enqueue(packet, capture_packet.clone(), None).await?,
                Event::MultitransportRequest(request) => {
                    let id = request.id;
                    if !sidebands.open(request, None) {
                        outputs.extend(session.complete_multitransport(id, 0x8000_4004)?);
                    }
                }
                Event::SendTunnel { protocol, payloads } => sidebands
                    .send(protocol, payloads, capture_packet.clone())
                    .await
                    .map_err(|_| RdpError::Connection)?,
                Event::MultitransportSynchronized { mut packets } => {
                    let last = packets
                        .pop()
                        .ok_or(RdpError::Protocol("empty multitransport synchronization"))?;
                    for packet in packets {
                        writer.enqueue(packet, None, None).await?;
                    }
                    let (receipt, flushed) = oneshot::channel();
                    writer.enqueue(last, None, Some(receipt)).await?;
                    timeout(duration, flushed)
                        .await
                        .map_err(|_| RdpError::Timeout)?
                        .map_err(|_| RdpError::Connection)??;
                }
                Event::GraphicsChanged | Event::Resized { .. } => {
                    frame_clock.dirty(Instant::now());
                }
                Event::GraphicsFrameBegin { .. } => {
                    frame_receive_deadline = Some(Instant::now() + duration);
                }
                Event::GraphicsFrameEnd { .. } => {
                    frame_receive_deadline = None;
                }
                Event::Audio(event) => {
                    outputs.extend(handle_audio(event, &mut playback, &mut session)?)
                }
                Event::ClipboardFileRequest(request) => {
                    if !files.request(request) {
                        outputs.extend(session.clipboard_file_contents(request.id, None)?);
                    }
                }
                Event::ClipboardFilesReleased { offer_id } => {
                    if let Some(event) = files.release(offer_id) {
                        progress.remove(&(false, offer_id));
                        events
                            .send(event)
                            .await
                            .map_err(|_| RdpError::Disconnected)?;
                    }
                }
                Event::ClipboardFiles {
                    offer_id,
                    files: offered,
                } => {
                    let Some(total_bytes) = offered
                        .iter()
                        .try_fold(0u64, |total, file| total.checked_add(file.size))
                    else {
                        events
                            .send(RdpEvent::RemoteFilesFailed {
                                offer_id,
                                error: RdpError::FileSelection,
                            })
                            .await
                            .map_err(|_| RdpError::Disconnected)?;
                        outputs.extend(session.cancel_remote_files(offer_id)?);
                        continue;
                    };
                    let count = offered.len();
                    remote_files = Some((offer_id, offered));
                    events
                        .send(RdpEvent::RemoteFilesAvailable {
                            revision: clipboard_revision,
                            offer_id,
                            files: count,
                            total_bytes,
                        })
                        .await
                        .map_err(|_| RdpError::Disconnected)?;
                }
                Event::ClipboardFileContents { request, data } => {
                    if remote_files
                        .as_ref()
                        .is_none_or(|(id, _)| *id != request.offer_id)
                        || downloads.active_offer() != Some(request.offer_id)
                    {
                        continue;
                    }
                    let result =
                        if download_deadline.is_some_and(|deadline| Instant::now() >= deadline) {
                            Err(RdpError::Timeout)
                        } else {
                            data.ok_or(RdpError::FileDownload)
                                .and_then(|data| downloads.accept(request, data))
                        };
                    if let Err(error) = result {
                        downloads.cancel();
                        remote_files = None;
                        download_deadline = None;
                        progress.remove(&(true, request.offer_id));
                        events
                            .send(RdpEvent::RemoteFilesFailed {
                                offer_id: request.offer_id,
                                error,
                            })
                            .await
                            .map_err(|_| RdpError::Disconnected)?;
                        outputs.extend(session.cancel_remote_files(request.offer_id)?);
                    } else {
                        download_deadline = Some(Instant::now() + duration);
                    }
                }
                Event::ClipboardRemoteFilesCancelled { offer_id } => {
                    if remote_files.as_ref().is_some_and(|(id, _)| *id == offer_id) {
                        remote_files = None;
                        downloads.cancel();
                        download_deadline = None;
                        progress.remove(&(true, offer_id));
                        events
                            .send(RdpEvent::RemoteFilesCancelled { offer_id })
                            .await
                            .map_err(|_| RdpError::Disconnected)?;
                    }
                }
                Event::ClipboardOffer => events
                    .send(RdpEvent::Clipboard {
                        revision: clipboard_revision,
                        event: RdpClipboardEvent::Offer,
                    })
                    .await
                    .map_err(|_| RdpError::Disconnected)?,
                Event::ClipboardText(text) => events
                    .send(RdpEvent::Clipboard {
                        revision: clipboard_revision,
                        event: RdpClipboardEvent::Text(text),
                    })
                    .await
                    .map_err(|_| RdpError::Disconnected)?,
                Event::ClipboardContent(content) => events
                    .send(RdpEvent::Clipboard {
                        revision: clipboard_revision,
                        event: RdpClipboardEvent::Content(content),
                    })
                    .await
                    .map_err(|_| RdpError::Disconnected)?,
                Event::ClipboardUnavailable => events
                    .send(RdpEvent::Clipboard {
                        revision: clipboard_revision,
                        event: RdpClipboardEvent::Unavailable,
                    })
                    .await
                    .map_err(|_| RdpError::Disconnected)?,
                Event::ClipboardPublished => {
                    let Some(request) = clipboard_request.take() else {
                        continue;
                    };
                    if request.paste_cancelled() {
                        clipboard_request = Some(request);
                    } else if Instant::now() >= request.deadline
                        || (request.paste.is_some() && !session.clipboard_ready())
                    {
                        notify_rejection(
                            RdpError::ClipboardUnavailable,
                            Some(request.revision),
                            events,
                            &mut last_rejection,
                        )
                        .await?;
                    } else {
                        last_rejection = None;
                        if let Some(cancel) = request.paste {
                            queued_paste = Some(cancel.clone());
                            send_paste(
                                &mut session,
                                &writer,
                                &mut deliveries,
                                cancel,
                                request.revision,
                            )
                            .await?;
                        } else {
                            events
                                .send(RdpEvent::Clipboard {
                                    revision: request.revision,
                                    event: RdpClipboardEvent::Published,
                                })
                                .await
                                .map_err(|_| RdpError::Disconnected)?;
                        }
                    }
                }
                Event::ClipboardPublishRejected => {
                    if clipboard_request
                        .as_ref()
                        .is_some_and(ClipboardRequest::paste_cancelled)
                    {
                        continue;
                    }
                    if let Some(request) = clipboard_request.take() {
                        notify_rejection(
                            RdpError::ClipboardUnavailable,
                            Some(request.revision),
                            events,
                            &mut last_rejection,
                        )
                        .await?;
                    }
                }
                Event::RequestRejected(error) => {
                    notify_rejection(error, command_revision, events, &mut last_rejection).await?
                }
                Event::Disconnected { .. } => return Ok(()),
            }
        }
        if session.is_active() {
            activation_deadline = None;
            if session.graphics_frame_in_progress() && completed_frame.is_none() {
                frame_clock.suspend();
            }
        } else {
            if let Some(cancel) = queued_paste.take() {
                cancel.cancel();
            }
            if clipboard_request
                .as_ref()
                .is_some_and(|request| request.paste.is_some() && !request.paste_cancelled())
            {
                let request = clipboard_request.take().expect("pending paste was checked");
                events
                    .send(RdpEvent::Clipboard {
                        revision: request.revision,
                        event: RdpClipboardEvent::PasteCancelled,
                    })
                    .await
                    .map_err(|_| RdpError::Disconnected)?;
            }
            activation_deadline.get_or_insert_with(|| Instant::now() + duration);
            frame_clock.suspend();
            if was_active {
                // Discard the old activation once. Frames received while the
                // next activation is finalizing must survive until presentation.
                completed_frame = None;
                completed_acknowledgement = None;
                frame_receive_deadline = None;
            }
        }
    }
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

async fn notify_rejection(
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
) -> Result<()> {
    let (mut framed, negotiation, public_key) = begin_trusted(&config, events).await?;
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
    .map_err(|_| RdpError::Timeout)?
}

async fn begin_trusted(
    config: &RdpConfig,
    events: &mpsc::Sender<RdpEvent>,
) -> Result<(Stream, NegotiationResponse, Vec<u8>)> {
    let duration = Duration::from_secs(config.timeout_secs);
    let mut pin = config.certificate_sha256.clone();
    let mut accepted = None;
    let connected = loop {
        let challenge = Arc::new(Mutex::new(None));
        let result = timeout(duration, begin(config, pin.clone(), challenge.clone())).await;
        match result {
            Ok(Ok(connected)) => {
                if let Some((fingerprint, save)) = accepted.take() {
                    events
                        .send(RdpEvent::CertificateAccepted { fingerprint, save })
                        .await
                        .map_err(|_| RdpError::Disconnected)?;
                }
                break connected;
            }
            result => {
                let fingerprint = challenge.lock().unwrap_or_else(|e| e.into_inner()).take();
                let Some(fingerprint) = fingerprint else {
                    return match result {
                        Ok(Err(error)) => Err(error),
                        _ => Err(RdpError::Timeout),
                    };
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
) -> Result<(Stream, NegotiationResponse, Vec<u8>)> {
    let socket = TcpStream::connect((config.host.as_str(), config.port))
        .await
        .map_err(|_| RdpError::Connection)?;
    socket.set_nodelay(true).map_err(|_| RdpError::Connection)?;
    let mut framed = Framed::new(socket);
    framed
        .write(&rustrdp::connection::negotiation_request())
        .await?;
    let negotiation =
        rustrdp::connection::parse_negotiation_response_details(&framed.read_pdu().await?)?;
    let name = rustls::pki_types::ServerName::try_from(config.host.clone())
        .map_err(|_| RdpError::InvalidConfig)?;
    let stream = tokio_rustls::TlsConnector::from(tls::config(pin, challenge)?)
        .connect(name, framed.into_stream()?)
        .await
        .map_err(|_| RdpError::Tls)?;
    let certificate = stream
        .get_ref()
        .1
        .peer_certificates()
        .and_then(|certs| certs.first())
        .ok_or(RdpError::Tls)?;
    let key = tls::public_key(certificate.as_ref())?;
    Ok((Framed::new(stream), negotiation, key))
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

fn input(command: RdpCommand) -> Input {
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
