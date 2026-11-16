use super::*;

impl<R: tokio::io::AsyncRead + Unpin> Worker<'_, R> {
    pub(super) async fn output(
        &mut self,
        outputs: Vec<Event>,
        capture_packet: Option<CancellationToken>,
        command_revision: Option<u64>,
        server_update: bool,
    ) -> Result<bool> {
        let mut outputs = std::collections::VecDeque::from(outputs);
        while let Some(output) = outputs.pop_front() {
            match output {
                Event::RemoteApp(event) => {
                    self.events
                        .send(RdpEvent::RemoteApp(event))
                        .await
                        .map_err(|_| RdpError::Disconnected)?;
                }
                Event::Capture(rustrdp::CaptureEvent::Open(request)) => {
                    self.media.capture = None;
                    match audio::Capture::new(
                        request,
                        self.media.microphone_enabled.clone(),
                        self.media.locked.clone(),
                        self.media.capture_transmission.clone(),
                    ) {
                        Ok(worker) => self.media.capture = Some(worker),
                        Err(error) => {
                            outputs
                                .extend(self.session.confirm_capture(request.generation, false)?);
                            notify_rejection(error, None, self.events, &mut self.last_rejection)
                                .await?;
                        }
                    }
                }
                Event::Capture(rustrdp::CaptureEvent::Close) => {
                    self.media.capture = None;
                }
                Event::DriveRequest(request) => {
                    if let Some(device) = &self.media.drives {
                        device.request(request)?;
                    } else {
                        return Err(RdpError::Protocol("drive request without redirection"));
                    }
                }
                Event::Send(packet) => {
                    self.writer
                        .enqueue(packet, capture_packet.clone(), None)
                        .await?
                }
                Event::MultitransportRequest(request) => {
                    let id = request.id;
                    if !self.sidebands.open(request, None) {
                        outputs.extend(self.session.complete_multitransport(id, 0x8000_4004)?);
                    }
                }
                Event::SendTunnel { protocol, payloads } => self
                    .sidebands
                    .send(protocol, payloads, capture_packet.clone())
                    .await
                    .map_err(|error| error.at_stage(ConnectionStage::Multitransport))?,
                Event::MultitransportSynchronized { mut packets } => {
                    let last = packets
                        .pop()
                        .ok_or(RdpError::Protocol("empty multitransport synchronization"))?;
                    for packet in packets {
                        self.writer.enqueue(packet, None, None).await?;
                    }
                    let (receipt, flushed) = oneshot::channel();
                    self.writer.enqueue(last, None, Some(receipt)).await?;
                    timeout(self.duration, flushed)
                        .await
                        .map_err(|_| RdpError::Timeout)?
                        .map_err(|_| RdpError::Connection)??;
                }
                Event::GraphicsChanged | Event::Resized { .. } => {
                    if server_update {
                        self.graphics.clock.graphics_dirty(Instant::now());
                    } else {
                        self.graphics.clock.dirty(Instant::now());
                    }
                }
                Event::GraphicsFrameBegin { .. } => {
                    self.graphics.receive_deadline = Some(Instant::now() + self.duration);
                }
                Event::GraphicsFrameEnd { .. } => {
                    self.graphics.receive_deadline = None;
                }
                Event::Audio(event) => outputs.extend(handle_audio(
                    event,
                    &mut self.media.playback,
                    &mut self.session,
                )?),
                Event::RequestRejected(error) => {
                    notify_rejection(
                        error,
                        command_revision,
                        self.events,
                        &mut self.last_rejection,
                    )
                    .await?
                }
                Event::Disconnected { .. } => return Ok(false),
                event @ (Event::ClipboardFileRequest(_)
                | Event::ClipboardFilesReleased { .. }
                | Event::ClipboardFiles { .. }
                | Event::ClipboardFileContents { .. }
                | Event::ClipboardRemoteFilesCancelled { .. }
                | Event::ClipboardOffer
                | Event::ClipboardText(_)
                | Event::ClipboardContent(_)
                | Event::ClipboardUnavailable
                | Event::ClipboardPublished
                | Event::ClipboardPublishRejected) => {
                    self.clipboard_output(event, &mut outputs).await?
                }
            }
        }
        Ok(true)
    }
}
