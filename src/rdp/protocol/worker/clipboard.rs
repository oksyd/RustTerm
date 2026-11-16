use super::*;

impl<R: tokio::io::AsyncRead + Unpin> Worker<'_, R> {
    pub(super) async fn delivered(
        &mut self,
        value: Option<std::result::Result<(Delivery, Result<()>), tokio::task::JoinError>>,
    ) -> Result<Step> {
        match value {
            Some(Ok((Delivery::Paste { revision }, result))) => {
                let event = match result {
                    Ok(()) => RdpClipboardEvent::PasteSent,
                    Err(RdpError::ClipboardUnavailable) => RdpClipboardEvent::PasteCancelled,
                    Err(error) => return Err(error),
                };
                self.events
                    .send(RdpEvent::Clipboard { revision, event })
                    .await
                    .map_err(|_| RdpError::Disconnected)?;
                Ok(Step::Skip)
            }
            Some(Ok((Delivery::File { offer_id, bytes }, result))) => {
                result?;
                if let Some(event) = self.clipboard.files.sent(offer_id, bytes) {
                    if matches!(
                        event,
                        RdpEvent::FileTransfer {
                            available: false,
                            ..
                        }
                    ) {
                        self.clipboard.progress.remove(&(false, offer_id));
                        self.events
                            .send(event)
                            .await
                            .map_err(|_| RdpError::Disconnected)?;
                    } else {
                        self.clipboard.progress.insert((false, offer_id), event);
                        self.clipboard
                            .progress_deadline
                            .get_or_insert_with(|| Instant::now() + Duration::from_millis(100));
                    }
                }
                Ok(Step::Skip)
            }
            _ => Err(RdpError::Runtime),
        }
    }
    pub(super) async fn file_completed(
        &mut self,
        value: Option<std::result::Result<FileCompleted, tokio::task::JoinError>>,
    ) -> Result<Step> {
        let outputs = match value {
            Some(Ok(FileCompleted::Prepared { revision, result })) => {
                if revision != self.clipboard.revision
                    || self.clipboard.request.as_ref().is_none_or(|request| {
                        request.revision != revision
                            || request.paste_cancelled()
                            || Instant::now() >= request.deadline
                    })
                {
                    return Ok(Step::Skip);
                }
                match result {
                    Ok(sources) => match self
                        .session
                        .set_clipboard_files(revision, sources.descriptions())
                    {
                        Ok(outputs) => {
                            let event = self.clipboard.files.retain(revision, sources);
                            self.events
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
                            self.clipboard.request = None;
                            notify_rejection(
                                error,
                                Some(revision),
                                self.events,
                                &mut self.last_rejection,
                            )
                            .await?;
                            return Ok(Step::Skip);
                        }
                        Err(error) => return Err(error.into()),
                    },
                    Err(error) => {
                        self.clipboard.request = None;
                        notify_rejection(
                            error,
                            Some(revision),
                            self.events,
                            &mut self.last_rejection,
                        )
                        .await?;
                        return Ok(Step::Skip);
                    }
                }
            }
            Some(Ok(FileCompleted::Read { request, result })) => {
                if !self.clipboard.files.contains(request.offer_id) {
                    return Ok(Step::Skip);
                }
                match result {
                    Ok(data) => {
                        let bytes = data.len() as u64;
                        let response = self
                            .session
                            .clipboard_file_contents(request.id, Some(data))?;
                        let last_packet = response
                            .iter()
                            .rposition(|event| matches!(event, Event::Send(_)));
                        let mut outputs = Vec::new();
                        for (index, event) in response.into_iter().enumerate() {
                            match event {
                                Event::Send(packet) if Some(index) == last_packet => {
                                    send_with_receipt(
                                        &self.writer,
                                        &mut self.clipboard.deliveries,
                                        packet,
                                        None,
                                        Delivery::File {
                                            offer_id: request.offer_id,
                                            bytes,
                                        },
                                    )
                                    .await?;
                                    self.clipboard.files.queued(request.offer_id);
                                }
                                Event::Send(packet) => {
                                    self.writer.enqueue(packet, None, None).await?
                                }
                                event => outputs.push(event),
                            }
                        }
                        outputs
                    }
                    Err(error) => {
                        let outputs = self.session.clipboard_file_contents(request.id, None)?;
                        notify_rejection(error, None, self.events, &mut self.last_rejection)
                            .await?;
                        outputs
                    }
                }
            }
            Some(Err(_)) => {
                self.clipboard.files.cancel();
                notify_rejection(
                    RdpError::FileUnavailable,
                    None,
                    self.events,
                    &mut self.last_rejection,
                )
                .await?;
                self.session.cancel_clipboard_files()?
            }
            None => return Ok(Step::Skip),
        };
        Ok(Step::Outputs(outputs))
    }
    pub(super) async fn download_completed(
        &mut self,
        value: Option<DownloadCompleted>,
    ) -> Result<Step> {
        let outputs = match value {
            Some(mut completed) => {
                let offer_id = completed.offer_id;
                if self
                    .clipboard
                    .remote_files
                    .as_ref()
                    .is_none_or(|(id, _)| *id != offer_id)
                {
                    return Ok(Step::Skip);
                }
                if self
                    .clipboard
                    .download_deadline
                    .is_some_and(|deadline| Instant::now() >= deadline)
                {
                    self.clipboard.downloads.cancel();
                    completed.result = Err(RdpError::Timeout);
                }
                match completed.result {
                    Ok(DownloadNext::Request {
                        index,
                        offset,
                        length,
                    }) => {
                        self.clipboard.download_deadline = Some(Instant::now() + self.duration);
                        self.clipboard.progress.insert(
                            (true, offer_id),
                            RdpEvent::RemoteFilesProgress {
                                offer_id,
                                bytes_received: completed.bytes_received,
                                total_bytes: completed.total_bytes,
                            },
                        );
                        self.clipboard
                            .progress_deadline
                            .get_or_insert_with(|| Instant::now() + Duration::from_millis(100));
                        self.session
                            .request_clipboard_file(offer_id, index, offset, length)?
                    }
                    result => {
                        self.clipboard.remote_files = None;
                        self.clipboard.download_deadline = None;
                        self.clipboard.progress.remove(&(true, offer_id));
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
                        self.events
                            .send(event)
                            .await
                            .map_err(|_| RdpError::Disconnected)?;
                        self.session.cancel_remote_files(offer_id)?
                    }
                }
            }
            None => return Ok(Step::Skip),
        };
        Ok(Step::Outputs(outputs))
    }
    pub(super) async fn download_timeout(&mut self) -> Result<Step> {
        let outputs = {
            self.clipboard.downloads.cancel();
            self.clipboard.download_deadline = None;
            let Some((offer_id, _)) = self.clipboard.remote_files.take() else {
                return Ok(Step::Skip);
            };
            self.clipboard.progress.remove(&(true, offer_id));
            self.events
                .send(RdpEvent::RemoteFilesFailed {
                    offer_id,
                    error: RdpError::Timeout,
                })
                .await
                .map_err(|_| RdpError::Disconnected)?;
            self.session.cancel_remote_files(offer_id)?
        };
        Ok(Step::Outputs(outputs))
    }
    pub(super) async fn publish_progress(&mut self) -> Result<Step> {
        // Coalesce byte counters and reserve UI capacity for clipboard
        // handshakes. Retry the last update even when no more bytes arrive.
        while self.events.capacity() > 8 {
            let Some((offer_id, event)) = self.clipboard.progress.pop_first() else {
                break;
            };
            match self.events.try_send(event) {
                Ok(()) => {}
                Err(mpsc::error::TrySendError::Full(event)) => {
                    self.clipboard.progress.insert(offer_id, event);
                    break;
                }
                Err(mpsc::error::TrySendError::Closed(_)) => {
                    return Err(RdpError::Disconnected);
                }
            }
        }
        self.clipboard.progress_deadline = (!self.clipboard.progress.is_empty())
            .then(|| Instant::now() + Duration::from_millis(100));
        Ok(Step::Skip)
    }
    pub(super) async fn clipboard_timeout(&mut self) -> Result<Step> {
        self.clipboard.files.cancel_preparation();
        if self
            .clipboard
            .request
            .as_ref()
            .is_some_and(ClipboardRequest::paste_cancelled)
        {
            return Ok(Step::Skip);
        }
        if let Some(request) = self.clipboard.request.take() {
            notify_rejection(
                RdpError::ClipboardUnavailable,
                Some(request.revision),
                self.events,
                &mut self.last_rejection,
            )
            .await?;
        }
        Ok(Step::Skip)
    }
    pub(super) async fn clipboard_output(
        &mut self,
        event: Event,
        outputs: &mut std::collections::VecDeque<Event>,
    ) -> Result<()> {
        match event {
            Event::ClipboardFileRequest(request) => {
                if !self.clipboard.files.request(request) {
                    outputs.extend(self.session.clipboard_file_contents(request.id, None)?);
                }
            }
            Event::ClipboardFilesReleased { offer_id } => {
                if let Some(event) = self.clipboard.files.release(offer_id) {
                    self.clipboard.progress.remove(&(false, offer_id));
                    self.events
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
                    self.events
                        .send(RdpEvent::RemoteFilesFailed {
                            offer_id,
                            error: RdpError::FileSelection,
                        })
                        .await
                        .map_err(|_| RdpError::Disconnected)?;
                    outputs.extend(self.session.cancel_remote_files(offer_id)?);
                    return Ok(());
                };
                let count = offered.len();
                self.clipboard.remote_files = Some((offer_id, offered));
                self.events
                    .send(RdpEvent::RemoteFilesAvailable {
                        revision: self.clipboard.revision,
                        offer_id,
                        files: count,
                        total_bytes,
                    })
                    .await
                    .map_err(|_| RdpError::Disconnected)?;
            }
            Event::ClipboardFileContents { request, data } => {
                if self
                    .clipboard
                    .remote_files
                    .as_ref()
                    .is_none_or(|(id, _)| *id != request.offer_id)
                    || self.clipboard.downloads.active_offer() != Some(request.offer_id)
                {
                    return Ok(());
                }
                let result = if self
                    .clipboard
                    .download_deadline
                    .is_some_and(|deadline| Instant::now() >= deadline)
                {
                    Err(RdpError::Timeout)
                } else {
                    data.ok_or(RdpError::FileDownload)
                        .and_then(|data| self.clipboard.downloads.accept(request, data))
                };
                if let Err(error) = result {
                    self.clipboard.downloads.cancel();
                    self.clipboard.remote_files = None;
                    self.clipboard.download_deadline = None;
                    self.clipboard.progress.remove(&(true, request.offer_id));
                    self.events
                        .send(RdpEvent::RemoteFilesFailed {
                            offer_id: request.offer_id,
                            error,
                        })
                        .await
                        .map_err(|_| RdpError::Disconnected)?;
                    outputs.extend(self.session.cancel_remote_files(request.offer_id)?);
                } else {
                    self.clipboard.download_deadline = Some(Instant::now() + self.duration);
                }
            }
            Event::ClipboardRemoteFilesCancelled { offer_id } => {
                if self
                    .clipboard
                    .remote_files
                    .as_ref()
                    .is_some_and(|(id, _)| *id == offer_id)
                {
                    self.clipboard.remote_files = None;
                    self.clipboard.downloads.cancel();
                    self.clipboard.download_deadline = None;
                    self.clipboard.progress.remove(&(true, offer_id));
                    self.events
                        .send(RdpEvent::RemoteFilesCancelled { offer_id })
                        .await
                        .map_err(|_| RdpError::Disconnected)?;
                }
            }
            Event::ClipboardOffer => self
                .events
                .send(RdpEvent::Clipboard {
                    revision: self.clipboard.revision,
                    event: RdpClipboardEvent::Offer,
                })
                .await
                .map_err(|_| RdpError::Disconnected)?,
            Event::ClipboardText(text) => self
                .events
                .send(RdpEvent::Clipboard {
                    revision: self.clipboard.revision,
                    event: RdpClipboardEvent::Text(text),
                })
                .await
                .map_err(|_| RdpError::Disconnected)?,
            Event::ClipboardContent(content) => self
                .events
                .send(RdpEvent::Clipboard {
                    revision: self.clipboard.revision,
                    event: RdpClipboardEvent::Content(content),
                })
                .await
                .map_err(|_| RdpError::Disconnected)?,
            Event::ClipboardUnavailable => self
                .events
                .send(RdpEvent::Clipboard {
                    revision: self.clipboard.revision,
                    event: RdpClipboardEvent::Unavailable,
                })
                .await
                .map_err(|_| RdpError::Disconnected)?,
            Event::ClipboardPublished => {
                let Some(request) = self.clipboard.request.take() else {
                    return Ok(());
                };
                if request.paste_cancelled() {
                    self.clipboard.request = Some(request);
                } else if Instant::now() >= request.deadline
                    || (request.paste.is_some() && !self.session.clipboard_ready())
                {
                    notify_rejection(
                        RdpError::ClipboardUnavailable,
                        Some(request.revision),
                        self.events,
                        &mut self.last_rejection,
                    )
                    .await?;
                } else {
                    self.last_rejection = None;
                    if let Some(cancel) = request.paste {
                        self.clipboard.queued_paste = Some(cancel.clone());
                        send_paste(
                            &mut self.session,
                            &self.writer,
                            &self.input_order,
                            &mut self.clipboard.deliveries,
                            cancel,
                            request.revision,
                        )
                        .await?;
                    } else {
                        self.events
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
                if self
                    .clipboard
                    .request
                    .as_ref()
                    .is_some_and(ClipboardRequest::paste_cancelled)
                {
                    return Ok(());
                }
                if let Some(request) = self.clipboard.request.take() {
                    notify_rejection(
                        RdpError::ClipboardUnavailable,
                        Some(request.revision),
                        self.events,
                        &mut self.last_rejection,
                    )
                    .await?;
                }
            }
            _ => unreachable!("clipboard events are dispatched by output"),
        }
        Ok(())
    }
}
