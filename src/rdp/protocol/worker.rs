//! Connection-owned runtime. The outer loop only orders input barriers,
//! protocol effects and activation; handlers own each domain's state changes.
use super::*;

mod clipboard;
mod graphics;
mod input;
mod media;
mod output;
mod start;
#[cfg(test)]
mod tests;

pub(super) struct Frontend<'a> {
    pub events: &'a mpsc::Sender<RdpEvent>,
    pub frames: watch::Sender<Option<Arc<RdpFrame>>>,
    pub presented: &'a mut watch::Receiver<Option<rustrdp::FrameToken>>,
    pub frame_sequence: &'a mut u64,
    pub redirection: RedirectionState,
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
    DesktopActivity,
    Presented(Option<rustrdp::FrameToken>),
    InputActivity,
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

enum Step {
    Skip,
    Outputs(Vec<Event>),
    Capture(Vec<Event>, CancellationToken),
    Redirect(crate::rdp::handoff::Handoff),
}

pub(super) struct Worker<'a, R> {
    config: RdpConfig,
    events: &'a mpsc::Sender<RdpEvent>,
    presented: &'a mut watch::Receiver<Option<rustrdp::FrameToken>>,
    session: rustrdp::Session,
    framed: Framed<R>,
    writer: Writer,
    sidebands: Sidebands,
    duration: Duration,
    pub(super) input_worker: crate::rdp::input_worker::InputWorker,
    input_order: Arc<tokio::sync::Mutex<()>>,
    last_rejection: Option<(Option<u64>, RdpError)>,
    desktop_activity: crate::rdp::keepalive::DesktopActivity,
    activation_deadline: Option<Instant>,
    performance: RdpPerformance,
    performance_started: Instant,
    performance_deadline: Instant,
    startup_tunnels: std::collections::VecDeque<(
        rustrdp::multitransport::TransportProtocol,
        tokio_util::bytes::Bytes,
    )>,
    initial_outputs: std::collections::VecDeque<Event>,
    graphics: Graphics<'a>,
    clipboard: Clipboard,
    media: Media,
}
pub(super) struct Graphics<'a> {
    clock: FrameClock,
    snapshot: FrameSnapshot,
    completed: Option<RdpFrame>,
    acknowledgement: Option<rustrdp::FrameToken>,
    receive_deadline: Option<Instant>,
    sequence: &'a mut u64,
    frames: watch::Sender<Option<Arc<RdpFrame>>>,
    pointer: watch::Sender<Option<rustrdp::graphics::PointerState>>,
    presentation: Arc<Mutex<Option<rustrdp::FrameToken>>>,
    separate_pointer: bool,
}
pub(super) struct Clipboard {
    revision: u64,
    request: Option<ClipboardRequest>,
    queued_paste: Option<CancellationToken>,
    files: Transfers,
    downloads: Downloads,
    remote_files: Option<(u64, Vec<rustrdp::ClipboardFile>)>,
    download_deadline: Option<Instant>,
    progress: BTreeMap<(bool, u64), RdpEvent>,
    progress_deadline: Option<Instant>,
    deliveries: Deliveries,
}
pub(super) struct Media {
    capture: Option<audio::Capture>,
    drives: Option<crate::rdp::shared_drives::SharedDrives>,
    playback: Option<audio::Playback>,
    microphone_enabled: Arc<AtomicBool>,
    locked: Arc<AtomicBool>,
    capture_transmission: Arc<Mutex<CancellationToken>>,
}
impl<R: tokio::io::AsyncRead + Unpin> Worker<'_, R> {
    pub(super) async fn run(&mut self) -> Result<Option<crate::rdp::handoff::Handoff>> {
        let mut yield_deadline = Instant::now() + Duration::from_millis(2);
        let mut interaction_budget = InteractionBudget::new();
        loop {
            if Instant::now() >= yield_deadline {
                tokio::task::yield_now().await;
                yield_deadline = Instant::now() + Duration::from_millis(2);
            }
            let Some(mut incoming) = self.next(&mut interaction_budget).await? else {
                return Ok(None);
            };
            // Keep input ordering through every effect, early return and error.
            let _completion = match &mut incoming {
                Incoming::Command(command) => command.completion.take(),
                _ => None,
            };
            let revision = match &incoming {
                Incoming::Command(QueuedCommand {
                    command: RdpCommand::Clipboard { revision, .. },
                    ..
                }) => Some(*revision),
                _ => None,
            };
            let was_active = self.session.is_active();
            let server_update = matches!(incoming, Incoming::Pdu { .. });
            let order = self.input_order.clone();
            let _activity_order = if matches!(incoming, Incoming::DesktopActivity) {
                Some(order.lock().await)
            } else {
                None
            };
            let (outputs, capture_packet) = match self.dispatch(incoming, revision).await? {
                Step::Skip => continue,
                Step::Redirect(handoff) => return Ok(Some(handoff)),
                Step::Outputs(outputs) => (outputs, None),
                Step::Capture(outputs, cancel) => (outputs, Some(cancel)),
            };
            if !self
                .output(outputs, capture_packet, revision, server_update)
                .await?
            {
                return Ok(None);
            }
            self.activation(was_active).await?;
        }
    }
    async fn next(
        &mut self,
        interaction_budget: &mut InteractionBudget,
    ) -> Result<Option<Incoming>> {
        // Each iteration adds at most one receipt. Reap completions first and
        // apply backpressure before accepting work when all slots are occupied.
        let incoming = if let Some(delivered) = self.clipboard.deliveries.try_join_next() {
            Incoming::Delivered(Some(delivered))
        } else if self.clipboard.deliveries.len() >= MAX_DELIVERIES {
            Incoming::Delivered(self.clipboard.deliveries.join_next().await)
        } else if let Some(completed) = self
            .media
            .playback
            .as_mut()
            .and_then(|device| device.completions.try_recv().ok())
        {
            // Acknowledge consumed/dropped sound before accepting another block.
            Incoming::Audio(Some(completed))
        } else if let Some(completed) = self
            .media
            .drives
            .as_mut()
            .and_then(|device| device.completions.try_recv().ok())
        {
            Incoming::Drive(Some(completed))
        } else if let Some((protocol, packet)) = self.startup_tunnels.pop_front() {
            Incoming::Pdu {
                packet,
                tunnel: Some(protocol),
            }
        } else if !self.initial_outputs.is_empty() {
            Incoming::Outputs(self.initial_outputs.drain(..).collect())
        } else if self
            .clipboard
            .request
            .as_ref()
            .is_some_and(ClipboardRequest::paste_cancelled)
        {
            // Drain to the already accepted cancellation. A replacement paste
            // owns its feedback; the old ACK or timeout must not overwrite it.
            match self.input_worker.commands.recv().await {
                Some(command) => Incoming::Command(command),
                None => return self.input_worker.failures.try_recv().map_or(Ok(None), Err),
            }
        } else if let Some(interaction) =
            interaction_budget.next(&mut self.input_worker.commands, self.presented)
        {
            match interaction {
                Interaction::Command(command) => Incoming::Command(command),
                Interaction::Presented(token) => Incoming::Presented(token),
            }
        } else if !interaction_budget.is_exhausted() && self.graphics.clock.is_due(Instant::now()) {
            Incoming::Publish
        } else {
            // After a full interaction burst, poll every source fairly, including
            // a due publication. Publishing alone must not replenish the budget.
            interaction_budget.reset();
            tokio::select! {
                error = self.input_worker.failures.recv() => return error.map_or(Ok(None), Err),
                changed = self.input_worker.activity.changed() => {
                    if changed.is_err() { return self.input_worker.failures.try_recv().map_or(Ok(None), Err); }
                    Incoming::InputActivity
                },
                result = self.framed.read_pdu() => Incoming::Pdu { packet: result?, tunnel: None },
                Some(update) = self.sidebands.next() => match update {
                    sidebands::Update::Connected { request_id, result } => Incoming::TunnelConnected(request_id, result),
                    sidebands::Update::Data { protocol, packet } => Incoming::Pdu { packet, tunnel: Some(protocol) },
                    sidebands::Update::Closed { protocol } => Incoming::TunnelClosed(protocol),
                },
                command = self.input_worker.commands.recv() => match command { Some(command) => Incoming::Command(command), None => return self.input_worker.failures.try_recv().map_or(Ok(None), Err) },
                changed = self.presented.changed() => {
                    if changed.is_err() { return Ok(None); }
                    Incoming::Presented(self.presented.borrow_and_update().clone())
                },
                _ = sleep_until(self.graphics.clock.deadline().unwrap_or_else(Instant::now)), if self.graphics.clock.deadline().is_some() => Incoming::Publish,
                _ = sleep_until(self.graphics.receive_deadline.unwrap_or_else(Instant::now)), if self.graphics.receive_deadline.is_some() => return Err(RdpError::Timeout),
                _ = sleep_until(self.performance_deadline) => Incoming::Performance,
                _ = sleep_until(self.desktop_activity.deadline().unwrap_or_else(Instant::now)), if self.desktop_activity.deadline().is_some() => Incoming::DesktopActivity,
                _ = sleep_until(self.clipboard.progress_deadline.unwrap_or_else(Instant::now)), if self.clipboard.progress_deadline.is_some() => Incoming::Progress,
                _ = sleep_until(self.clipboard.download_deadline.unwrap_or_else(Instant::now)), if self.clipboard.download_deadline.is_some() => Incoming::DownloadTimedOut,
                _ = sleep_until(self.activation_deadline.unwrap_or_else(Instant::now)), if self.activation_deadline.is_some() => return Err(RdpError::Timeout),
                _ = sleep_until(self.clipboard.request.as_ref().map_or_else(Instant::now, |request| request.deadline)), if self.clipboard.request.is_some() => Incoming::ClipboardTimedOut,
                file = self.clipboard.files.next(), if self.clipboard.files.is_busy() => Incoming::File(file),
                completed = self.clipboard.downloads.next(), if self.clipboard.downloads.is_busy() => Incoming::Download(completed),
                delivered = self.clipboard.deliveries.join_next(), if !self.clipboard.deliveries.is_empty() => Incoming::Delivered(delivered),
                error = self.writer.errors.recv() => return Err(error.unwrap_or(RdpError::Disconnected)),
                captured = async { match &mut self.media.capture { Some(device) => device.events.recv().await, None => std::future::pending().await } } => Incoming::Capture(captured),
                completion = async { match &mut self.media.drives { Some(device) => device.completions.recv().await, None => std::future::pending().await } } => Incoming::Drive(completion),
                completed = async {
                    match &mut self.media.playback {
                        Some(device) => device.completions.recv().await,
                        None => std::future::pending().await,
                    }
                } => Incoming::Audio(completed),
            }
        };
        Ok(Some(incoming))
    }
    async fn dispatch(
        &mut self,
        incoming: Incoming,
        command_revision: Option<u64>,
    ) -> Result<Step> {
        let outputs = match incoming {
            Incoming::InputActivity => {
                if let Some(at) = *self.input_worker.activity.borrow_and_update() {
                    self.desktop_activity.input_at(at);
                }
                self.session.sync_input_pointer()?
            }
            Incoming::DesktopActivity => {
                if let Some(at) = *self.input_worker.activity.borrow() {
                    self.desktop_activity.input_at(at);
                }
                if !self.desktop_activity.poll(
                    Instant::now(),
                    self.session.is_active(),
                    self.media.locked.load(Ordering::Acquire),
                ) {
                    return Ok(Step::Skip);
                }
                self.session.keep_awake()?
            }
            Incoming::Outputs(outputs) => outputs,
            Incoming::TunnelConnected(id, result) => self
                .session
                .complete_multitransport(id, if result.is_ok() { 0 } else { 0x8000_4004 })?,
            Incoming::TunnelClosed(protocol) => {
                self.session.close_multitransport(protocol).map_err(|_| {
                    RdpError::network(ConnectionStage::Multitransport, ConnectionFailure::Closed)
                })?
            }
            Incoming::Presented(Some(token)) => {
                let outputs = self.session.acknowledge_frame(&token)?;
                self.performance.acknowledged_frames += outputs.len() as u64;
                outputs
            }
            Incoming::Presented(None) => return Ok(Step::Skip),
            Incoming::Delivered(value) => return self.delivered(value).await,
            Incoming::Capture(value) => return self.captured(value).await,
            Incoming::Drive(value) => return self.drive(value).await,
            Incoming::Audio(value) => return self.audio_completed(value).await,
            Incoming::File(value) => return self.file_completed(value).await,
            Incoming::Download(value) => return self.download_completed(value).await,
            Incoming::DownloadTimedOut => return self.download_timeout().await,
            Incoming::Progress => return self.publish_progress().await,
            Incoming::Publish => return self.publish_frame().await,
            Incoming::Performance => return self.report_performance().await,
            Incoming::Pdu { packet, tunnel } => return self.process_packet(packet, tunnel).await,
            Incoming::ClipboardTimedOut => return self.clipboard_timeout().await,
            Incoming::Command(queued) => return self.command(queued, command_revision).await,
        };
        Ok(Step::Outputs(outputs))
    }
}
