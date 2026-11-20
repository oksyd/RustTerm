//! TLS/authentication and activation. Early graphics, channel requests and UDP
//! data belong to this attempt and move into the runtime only after activation.
use super::*;

pub(super) enum Started {
    Ready(Box<Ready>),
    Redirect(crate::rdp::handoff::Handoff),
}
pub(super) struct Ready<S = TlsStream<NetworkStream>> {
    pub config: RdpConfig,
    pub session: rustrdp::Session,
    pub framed: Framed<S>,
    pub sidebands: Sidebands,
    pub startup_tunnels: std::collections::VecDeque<(
        rustrdp::multitransport::TransportProtocol,
        tokio_util::bytes::Bytes,
    )>,
    pub last_rejection: Option<(Option<u64>, RdpError)>,
    pub startup_audio: Vec<rustrdp::AudioEvent>,
    pub startup_files: Vec<Event>,
    pub frame_snapshot: FrameSnapshot,
    pub completed_frame: Option<RdpFrame>,
    pub completed_acknowledgement: Option<rustrdp::FrameToken>,
}
struct Handshake<'a> {
    framed: Stream,
    sidebands: Sidebands,
    connector: Connector,
    events: &'a mpsc::Sender<RdpEvent>,
    handshake_deadline: Instant,
    startup_tunnels: std::collections::VecDeque<(
        rustrdp::multitransport::TransportProtocol,
        tokio_util::bytes::Bytes,
    )>,
    startup_tunnel_bytes: usize,
    startup_tunnel_limit: usize,
    last_rejection: Option<(Option<u64>, RdpError)>,
    startup_audio: Vec<rustrdp::AudioEvent>,
    startup_files: Vec<Event>,
    frame_snapshot: FrameSnapshot,
    completed_frame: Option<RdpFrame>,
    completed_acknowledgement: Option<rustrdp::FrameToken>,
}
enum Advance {
    Pending,
    Redirect(crate::rdp::handoff::Handoff),
    Output(Box<rustrdp::HandshakeOutput>),
}

fn client_config(config: &RdpConfig, tunneled: bool) -> Result<ClientConfig> {
    let desktop_scale = config.desktop_scale.unwrap_or(100);
    let client_config = ClientConfig {
        username: config.username.clone(),
        domain: config.domain.clone().unwrap_or_default(),
        client_name: "RustTerm".into(),
        remote_app: config
            .remote_app
            .as_ref()
            .map(|app| rustrdp::remote_app::RemoteApp {
                program: app.program.clone(),
                arguments: app.arguments.clone(),
                working_directory: app.working_directory.clone(),
                local_move_size: cfg!(target_os = "linux"),
            }),
        width: config.width
            * u16::from(if config.remote_app.is_some() {
                1
            } else {
                config.monitor_count
            }),
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
        // Capability is independent of the current local display mode.
        dynamic_resolution: config.remote_app.is_none() && config.monitor_count == 1,
        bulk_compression: config.compression.then_some(BulkCompression::Rdp5),
        allow_lossy_bitmaps: config.allow_lossy_bitmaps,
        // GNOME requires RDPGFX even when AVC/lossy bitmap decoding is disabled.
        graphics_pipeline: config.color_depth == 32 && config.remote_app.is_none(),
        multitransport: !tunneled,
        frame_acknowledgements: if config.remote_app.is_some() {
            None
        } else {
            std::num::NonZeroU32::new(3)
        },
        monitors: if config.monitor_count > 1 && config.remote_app.is_none() {
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
            max_channel_message: crate::rdp::MAX_CLIPBOARD_CONTENT_BYTES + 8,
            ..rustrdp::Limits::default()
        },
    };
    client_config
        .validate()
        .map_err(|_| RdpError::InvalidConfig)?;
    Ok(client_config)
}
pub(super) async fn connect(
    attempt: ConnectionAttempt,
    tunnel: Option<TunnelConnector>,
    events: &mpsc::Sender<RdpEvent>,
) -> Result<Started> {
    let ConnectionAttempt {
        config,
        password,
        handoff,
    } = attempt;
    let client_config = client_config(&config, tunnel.is_some())?;
    let duration = Duration::from_secs(config.timeout_secs);
    let (mut framed, negotiation, public_key) = if let Some(handoff) = &handoff {
        // The authenticated source supplied the destination certificate. It is
        // scoped to this attempt and must never replace the saved profile pin.
        let challenge = Arc::new(Mutex::new(None));
        let request = Zeroizing::new(rustrdp::connection::redirection_request(&handoff.routing)?);
        begin_with_request(
            &config,
            Some(handoff.certificate_pin.clone()),
            challenge,
            &request,
            true,
            tunnel.as_ref(),
        )
        .await?
    } else {
        begin_trusted(&config, events, tunnel.as_ref()).await?
    };
    let (socket, secured) = framed.get_ref().get_ref();
    let peer = socket
        .peer_addr()
        .map_err(|error| RdpError::io(ConnectionStage::SocketSetup, error))?;
    let certificate = secured
        .peer_certificates()
        .and_then(|chain| chain.first())
        .ok_or(RdpError::Tls)?;
    let sidebands = Sidebands::new(
        peer,
        config.host.clone(),
        tls::fingerprint(certificate.as_ref()),
        duration,
    );
    let deadline = Instant::now() + duration;
    tokio::time::timeout_at(
        deadline,
        authenticate(
            &mut framed,
            negotiation,
            &config,
            password.as_ref(),
            handoff.as_ref(),
            public_key,
        ),
    )
    .await
    .map_err(|_| RdpError::network(framed.stage(), ConnectionFailure::TimedOut))??;
    drop(password);
    drop(handoff);
    framed.set_stage(ConnectionStage::SessionSetup);
    let startup_tunnel_limit = client_config.limits.max_channel_message;
    let connector = Connector::from_negotiation(client_config, negotiation)?;
    let handshake = Handshake {
        framed,
        sidebands,
        connector,
        events,
        handshake_deadline: deadline,
        startup_tunnels: Default::default(),
        startup_tunnel_bytes: 0,
        startup_tunnel_limit,
        last_rejection: None,
        startup_audio: Vec::new(),
        startup_files: Vec::new(),
        frame_snapshot: FrameSnapshot::with_audit(
            std::env::var_os("RUSTTERM_RDP_AUDIT_DAMAGE").is_some_and(|v| v == "1"),
        ),
        completed_frame: None,
        completed_acknowledgement: None,
    };
    tokio::time::timeout_at(deadline, handshake.activate(config))
        .await
        .map_err(|_| {
            RdpError::network(ConnectionStage::SessionSetup, ConnectionFailure::TimedOut)
        })?
}

async fn authenticate(
    framed: &mut Stream,
    negotiation: NegotiationResponse,
    config: &RdpConfig,
    password: Option<&Secret>,
    handoff: Option<&crate::rdp::handoff::Handoff>,
    public_key: Vec<u8>,
) -> Result<()> {
    if let Some(handoff) = handoff {
        framed.read_rdstls_capabilities().await?;
        framed.write(&handoff.authentication).await?;
        framed.read_rdstls_result().await?;
    } else {
        nla(
            framed,
            negotiation.protocol,
            config,
            password.ok_or(RdpError::Authentication)?,
            public_key,
        )
        .await?;
    }
    Ok(())
}
impl Handshake<'_> {
    async fn activate(mut self, config: RdpConfig) -> Result<Started> {
        self.framed.write(&self.connector.start()?).await?;
        loop {
            let output = match self.next().await? {
                Advance::Pending => continue,
                Advance::Redirect(handoff) => return Ok(Started::Redirect(handoff)),
                Advance::Output(output) => output,
            };
            for packet in output.packets {
                self.framed.write(&packet).await?;
            }
            for event in output.events {
                self.event(event).await?;
            }
            if let Some(session) = output.session {
                return Ok(Started::Ready(Box::new(Ready {
                    config,
                    session,
                    framed: self.framed,
                    sidebands: self.sidebands,
                    startup_tunnels: self.startup_tunnels,
                    last_rejection: self.last_rejection,
                    startup_audio: self.startup_audio,
                    startup_files: self.startup_files,
                    frame_snapshot: self.frame_snapshot,
                    completed_frame: self.completed_frame,
                    completed_acknowledgement: self.completed_acknowledgement,
                })));
            }
        }
    }
    async fn next(&mut self) -> Result<Advance> {
        let output = tokio::select! {
            packet = self.framed.read_pdu() => {
                let packet = packet?;
                if let Some(payload) = self.connector.server_redirection(&packet)? {
                    return Ok(Advance::Redirect(crate::rdp::handoff::Handoff::parse(payload)?));
                }
                let mut snapshot_error = None;
                let output = self.connector.advance_with_frames(&packet, |completed| {
                    let frame = self.frame_snapshot.capture_graphics(
                        completed.graphics,
                        completed.damage,
                        completed.bits_per_pixel,
                    );
                    let mut frame = match frame {
                        Ok(frame) => frame,
                        Err(error) => { snapshot_error = Some(error); return; }
                    };
                    self.completed_acknowledgement = completed.acknowledgement;
                    frame.acknowledgement = self.completed_acknowledgement.clone();
                    self.completed_frame = Some(frame);
                })?;
                if let Some(error) = snapshot_error { return Err(error); }
                output
            },
            Some(update) = self.sidebands.next() => match update {
                sidebands::Update::Connected { request_id, result } => rustrdp::HandshakeOutput {
                    events: self.connector.complete_multitransport(request_id, if result.is_ok() { 0 } else { 0x8000_4004 })?,
                    ..rustrdp::HandshakeOutput::default()
                },
                sidebands::Update::Data { protocol, packet } => {
                    self.startup_tunnel_bytes = self.startup_tunnel_bytes.checked_add(packet.len()).filter(|bytes| *bytes <= self.startup_tunnel_limit).ok_or(RdpError::Limit("startup tunnel bytes"))?;
                    if self.startup_tunnels.len() >= 4096 { return Err(RdpError::Limit("startup tunnel packet count")); }
                    self.startup_tunnels.push_back((protocol, packet));
                    return Ok(Advance::Pending);
                }
                sidebands::Update::Closed { .. } => return Err(RdpError::network(ConnectionStage::Multitransport, ConnectionFailure::Closed)),
            },
        };
        Ok(Advance::Output(Box::new(output)))
    }
    async fn event(&mut self, event: Event) -> Result<()> {
        match event {
            Event::RemoteApp(event) => self
                .events
                .send(RdpEvent::RemoteApp(event))
                .await
                .map_err(|_| RdpError::Disconnected)?,
            Event::ClipboardOffer => self
                .events
                .send(RdpEvent::Clipboard {
                    revision: 0,
                    event: RdpClipboardEvent::Offer,
                })
                .await
                .map_err(|_| RdpError::Disconnected)?,
            Event::ClipboardText(text) => self
                .events
                .send(RdpEvent::Clipboard {
                    revision: 0,
                    event: RdpClipboardEvent::Text(text),
                })
                .await
                .map_err(|_| RdpError::Disconnected)?,
            Event::ClipboardContent(content) => self
                .events
                .send(RdpEvent::Clipboard {
                    revision: 0,
                    event: RdpClipboardEvent::Content(content),
                })
                .await
                .map_err(|_| RdpError::Disconnected)?,
            Event::ClipboardUnavailable => self
                .events
                .send(RdpEvent::Clipboard {
                    revision: 0,
                    event: RdpClipboardEvent::Unavailable,
                })
                .await
                .map_err(|_| RdpError::Disconnected)?,
            Event::RequestRejected(error) => {
                notify_rejection(error, None, self.events, &mut self.last_rejection).await?
            }
            Event::Disconnected { .. } => return Err(RdpError::Disconnected),
            event @ (Event::Capture(_) | Event::DriveRequest(_)) => {
                if self.startup_files.len() >= 128 {
                    return Err(RdpError::Limit("startup redirection request count"));
                }
                self.startup_files.push(event);
            }
            Event::Audio(event) => {
                if self.startup_audio.len() >= 128 {
                    return Err(RdpError::Limit("startup audio event count"));
                }
                self.startup_audio.push(event);
            }
            Event::ClipboardFileRequest(_) | Event::ClipboardFilesReleased { .. } => {}
            event @ (Event::ClipboardFiles { .. }
            | Event::ClipboardFileContents { .. }
            | Event::ClipboardRemoteFilesCancelled { .. }) => {
                if self.startup_files.len() >= 32 {
                    return Err(RdpError::Limit("startup clipboard file event count"));
                }
                self.startup_files.push(event);
            }
            Event::Send(packet) => self.framed.write(&packet).await?,
            Event::MultitransportRequest(request) => {
                let id = request.id;
                if !self.sidebands.open(request, Some(self.handshake_deadline)) {
                    for response in self.connector.complete_multitransport(id, 0x8000_4004)? {
                        if let Event::Send(packet) = response {
                            self.framed.write(&packet).await?;
                        }
                    }
                }
            }
            Event::SendTunnel { protocol, payloads } => self
                .sidebands
                .send(protocol, payloads, None)
                .await
                .map_err(|error| error.at_stage(ConnectionStage::Multitransport))?,
            Event::MultitransportSynchronized { packets } => {
                for packet in packets {
                    self.framed.write(&packet).await?;
                }
            }
            Event::GraphicsChanged
            | Event::Resized { .. }
            | Event::GraphicsFrameBegin { .. }
            | Event::GraphicsFrameEnd { .. }
            | Event::ClipboardPublished
            | Event::ClipboardPublishRejected => {}
        };
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn display_control_capability_remains_available_when_initial_resolution_is_fixed() {
        let mut config = RdpConfig {
            dynamic_resolution: false,
            ..Default::default()
        };
        let initial = (config.width, config.height);
        let negotiated = client_config(&config, false).unwrap();
        assert!(negotiated.dynamic_resolution);
        assert_eq!((negotiated.width, negotiated.height), initial);
        assert!(!config.uses_dynamic_resolution());
        config.monitor_count = 2;
        assert!(!client_config(&config, false).unwrap().dynamic_resolution);
    }
}
