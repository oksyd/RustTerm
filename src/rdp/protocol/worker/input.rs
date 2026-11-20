use super::*;

impl<R: tokio::io::AsyncRead + Unpin> Worker<'_, R> {
    pub(super) async fn command(
        &mut self,
        queued: QueuedCommand,
        command_revision: Option<u64>,
    ) -> Result<Step> {
        let QueuedCommand {
            command,
            paste,
            queued_at,
            ..
        } = queued;
        self.performance.command_latency.record(queued_at.elapsed());
        self.desktop_activity.observe(&command, Instant::now());
        if matches!(command, RdpCommand::Clipboard { .. }) {
            self.clipboard.files.cancel_preparation();
        }
        if let Some(revision) = prepare_command(
            &command,
            &mut self.clipboard.revision,
            &mut self.clipboard.request,
            &mut self.last_rejection,
        ) {
            self.events
                .send(RdpEvent::Clipboard {
                    revision,
                    event: RdpClipboardEvent::PasteCancelled,
                })
                .await
                .map_err(|_| RdpError::Disconnected)?;
        }
        let result = match command {
            RdpCommand::ReceiveFiles { offer_id } => {
                let Some((id, offered)) = &self.clipboard.remote_files else {
                    return Ok(Step::Skip);
                };
                if *id != offer_id || self.clipboard.download_deadline.is_some() {
                    return Ok(Step::Skip);
                }
                match self.clipboard.downloads.begin(offer_id, offered.clone()) {
                    Ok(()) => {
                        self.clipboard.download_deadline = Some(Instant::now() + self.duration);
                        return Ok(Step::Skip);
                    }
                    Err(error) => {
                        self.clipboard.remote_files = None;
                        self.events
                            .send(RdpEvent::RemoteFilesFailed { offer_id, error })
                            .await
                            .map_err(|_| RdpError::Disconnected)?;
                        self.session.cancel_remote_files(offer_id)
                    }
                }
            }
            RdpCommand::CancelReceiveFiles { offer_id } => {
                if self
                    .clipboard
                    .remote_files
                    .as_ref()
                    .is_some_and(|(id, _)| *id == offer_id)
                {
                    self.clipboard.downloads.cancel();
                    self.clipboard.download_deadline = None;
                    self.clipboard.progress.remove(&(true, offer_id));
                }
                self.session.cancel_remote_files(offer_id)
            }
            RdpCommand::PasteRemote => {
                let cancel = paste.ok_or(RdpError::Protocol(
                    "missing remote paste cancellation token",
                ))?;
                if self.session.is_active() && !cancel.is_cancelled() {
                    self.clipboard.queued_paste = Some(cancel.clone());
                    send_paste(
                        &mut self.session,
                        &self.writer,
                        &self.input_order,
                        &mut self.clipboard.deliveries,
                        cancel,
                        self.clipboard.revision,
                    )
                    .await?;
                }
                return Ok(Step::Skip);
            }
            RdpCommand::RemoteApp(command) => self.session.remote_app(command),
            RdpCommand::SetMicrophoneEnabled(_) | RdpCommand::SetLocked(_) => {
                if let Some(device) = &self.media.capture {
                    device.refresh();
                }
                return Ok(Step::Skip);
            }
            RdpCommand::SetAudioMuted(_) => {
                if let Some(device) = &self.media.playback {
                    device.refresh_mute();
                }
                return Ok(Step::Skip);
            }
            RdpCommand::Resize { width, height } => self.session.resize(width & !1, height),
            RdpCommand::Clipboard { revision, action } => {
                return self.clipboard_command(revision, action, paste).await;
            }
            command => self.session.input(input(command)?),
        };
        self.command_result(result, command_revision).await
    }
    async fn command_result(
        &mut self,
        result: rustrdp::Result<Vec<Event>>,
        revision: Option<u64>,
    ) -> Result<Step> {
        match result {
            Ok(outputs) => Ok(Step::Outputs(outputs)),
            Err(error @ (rustrdp::Error::Unsupported(_) | rustrdp::Error::Limit(_))) => {
                notify_rejection(error, revision, self.events, &mut self.last_rejection).await?;
                Ok(Step::Skip)
            }
            Err(error) => Err(error.into()),
        }
    }
    async fn clipboard_command(
        &mut self,
        revision: u64,
        action: RdpClipboardAction,
        paste: Option<CancellationToken>,
    ) -> Result<Step> {
        let result = match action {
            action @ (RdpClipboardAction::SendText(_)
            | RdpClipboardAction::PasteText(_)
            | RdpClipboardAction::SendContent(_)
            | RdpClipboardAction::PasteContent(_)) => {
                if !self.begin_publication(revision, &paste).await? {
                    return Ok(Step::Skip);
                }
                let result = match action {
                    RdpClipboardAction::SendText(text) | RdpClipboardAction::PasteText(text) => {
                        self.session.set_clipboard(text)
                    }
                    RdpClipboardAction::SendContent(content)
                    | RdpClipboardAction::PasteContent(content) => {
                        self.session.set_clipboard_content(content)
                    }
                    _ => Err(rustrdp::Error::Invalid("clipboard publication action")),
                };
                if result.is_ok() {
                    self.pending_publication(revision, paste);
                }
                result
            }
            RdpClipboardAction::SendFiles(paths) | RdpClipboardAction::PasteFiles(paths) => {
                if !self.config.clipboard || !self.config.clipboard_files {
                    notify_rejection(
                        RdpError::ClipboardUnavailable,
                        Some(revision),
                        self.events,
                        &mut self.last_rejection,
                    )
                    .await?;
                    return Ok(Step::Skip);
                }
                if !self.begin_publication(revision, &paste).await? {
                    return Ok(Step::Skip);
                }
                match self.clipboard.files.prepare(revision, paths) {
                    Ok(()) => self.pending_publication(revision, paste),
                    Err(error) => {
                        notify_rejection(
                            error,
                            Some(revision),
                            self.events,
                            &mut self.last_rejection,
                        )
                        .await?
                    }
                }
                return Ok(Step::Skip);
            }
            RdpClipboardAction::CancelFiles => {
                self.clipboard.files.cancel();
                self.session.cancel_clipboard_files()
            }
            RdpClipboardAction::RequestText => self.session.request_clipboard(),
            RdpClipboardAction::Cancel => Ok(Vec::new()),
        };
        self.command_result(result, Some(revision)).await
    }
    fn pending_publication(&mut self, revision: u64, paste: Option<CancellationToken>) {
        self.clipboard.request = Some(ClipboardRequest {
            revision,
            deadline: Instant::now() + self.duration,
            paste,
        });
    }
    async fn begin_publication(
        &mut self,
        revision: u64,
        paste: &Option<CancellationToken>,
    ) -> Result<bool> {
        if paste.as_ref().is_some_and(CancellationToken::is_cancelled) {
            // Retain the revision until its already admitted cancellation barrier.
            self.pending_publication(revision, paste.clone());
            return Ok(false);
        }
        if paste.is_some() && !self.session.is_active() {
            notify_rejection(
                RdpError::ClipboardUnavailable,
                Some(revision),
                self.events,
                &mut self.last_rejection,
            )
            .await?;
            return Ok(false);
        }
        Ok(true)
    }
}
