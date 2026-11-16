use super::*;
use gpui::{PromptLevel, Timer};
use rustterm::i18n::messages as i18n_key;
use rustterm::rdp::CertificateDecision;
use std::time::Duration;

impl RdpView {
    pub(super) fn connect_after_layout(
        &mut self,
        secret: Secret,
        ssh: Option<rustterm::session::SshOptions>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if !self.config.uses_dynamic_resolution() {
            self.connect(secret, ssh, window, cx);
            return;
        }
        cx.on_next_frame(window, move |this, window, cx| {
            if this.closed || this.state != State::Connecting {
                return;
            }
            if this.visible && resize_dimensions(this.layout_size, window.scale_factor()).is_none()
            {
                // Frame callbacks precede layout. Allow one actual layout pass
                // before the handshake; background or zero-sized panes retain
                // their configured fallback and synchronize when shown.
                cx.on_next_frame(window, move |this, window, cx| {
                    this.connect(secret, ssh, window, cx);
                });
                // This callback runs outside drawing. Animation requests need
                // a current rendered view; refreshing the window does not.
                window.refresh();
            } else {
                this.connect(secret, ssh, window, cx);
            }
        });
        window.refresh();
    }

    fn record_error(&self, error: &rustterm::rdp::RdpError) {
        if let Some(code) = error.server_code() {
            self.window_state.record_values(
                error.diagnostic_code(),
                [self.index as u64, u64::from(code), 0],
            );
        } else {
            self.window_state.record_with_context(
                error.diagnostic_code(),
                error.diagnostic_context(),
                self.index,
            );
        }
    }

    pub(super) fn connect(
        &mut self,
        secret: Secret,
        ssh: Option<rustterm::session::SshOptions>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.closed || self.state != State::Connecting {
            return;
        }
        self.cursor.clear(window);
        self.cursor.update(None);
        self.pointer_task = None;
        self.close_application_windows(cx);
        self.applications = remote_app::Applications::default();
        self.cancel_file_clipboard_read();
        self.file_transfer = None;
        self.audio_muted = false;
        self.microphone_enabled = self.config.audio_input;
        self.clipboard = clipboard::ClipboardState::default();
        // Resolve automatic DPI for this session without changing the saved profile.
        self.device_scale = window.scale_factor();
        let config = connection_config(&self.config, self.layout_size, self.device_scale);
        self.desktop_scale = config.desktop_scale.expect("session DPI is resolved");
        if config.uses_dynamic_resolution() {
            self.desktop_size = (config.width, config.height);
        }
        self.window_state.record_values(
            "rdp-initial-desktop",
            [
                self.index as u64,
                u64::from(config.width),
                u64::from(config.height),
            ],
        );
        let handle = match RdpClient::connect_with_ssh(
            config,
            secret,
            Some(self.window_state.cache_dir.to_path_buf()),
            ssh,
        ) {
            Ok(handle) => handle,
            Err(error) => {
                self.record_error(&error);
                self.state = State::Failed;
                self.error = Some(Message::from_error(error));
                return;
            }
        };
        self.sender = Some(handle.sender());
        if self.window_state.is_locked() {
            self.set_locked(true, cx);
        }
        if self.window_state.is_locked() {
            self.send(RdpCommand::SetAudioMuted(true), cx);
        }
        self.frames = Some(handle.frames.clone());
        if self.config.remote_app.is_none() {
            let mut pointer = handle.pointer;
            self.pointer_task = Some(cx.spawn(async move |this, cx| {
                while pointer.changed().await.is_ok() {
                    let state = pointer.borrow_and_update().clone();
                    if this
                        .update(cx, |this, cx| {
                            if this.closed {
                                return false;
                            }
                            this.cursor.update(state);
                            this.hide_local_pointer =
                                this.cursor.hides_platform_cursor().unwrap_or(false);
                            if this.surface_visible() {
                                cx.notify();
                            }
                            true
                        })
                        .ok()
                        != Some(true)
                    {
                        break;
                    }
                }
            }));
        }
        let mut frames = handle.frames;
        self.frame_task = Some(cx.spawn(async move |this, cx| {
            while frames.changed().await.is_ok() {
                // Mark only this receiver's revision. The view independently
                // borrows the newest frame when GPUI is ready to paint it.
                let desktop = frames
                    .borrow_and_update()
                    .as_ref()
                    .map(|frame| ((frame.width, frame.height), frame.bits_per_pixel));
                if this
                    .update(cx, |this, cx| {
                        if this.closed {
                            return false;
                        }
                        let mut activity = false;
                        if let Some((size, depth)) = desktop {
                            if this.desktop_size != size {
                                this.desktop_size = size;
                                this.window_state.record_values(
                                    "rdp-desktop-resized",
                                    [this.index as u64, u64::from(size.0), u64::from(size.1)],
                                );
                                this.request_resize(cx);
                                activity = true;
                            }
                            this.negotiated_color_depth = Some(depth);
                        }
                        this.refresh_application_frames(cx);
                        if this.surface_visible() {
                            cx.notify();
                        } else if !this.unread {
                            this.unread = true;
                            activity = true;
                        }
                        if activity {
                            cx.emit(PaneEvent::Activity);
                        }
                        true
                    })
                    .ok()
                    != Some(true)
                {
                    break;
                }
            }
        }));
        let mut events = handle.events;
        self.event_task = Some(cx.spawn_in(window, async move |this, cx| {
            while let Some(event) = events.recv().await {
                if cx
                    .update(|window, cx| {
                        this.update(cx, |this, cx| {
                            if this.closed {
                                return false;
                            }
                            this.handle_event(event, window, cx);
                            true
                        })
                    })
                    .ok()
                    .and_then(Result::ok)
                    != Some(true)
                {
                    break;
                }
            }
        }));
    }

    fn handle_event(&mut self, event: RdpEvent, window: &mut Window, cx: &mut Context<Self>) {
        if self.sender.is_none() {
            return;
        }
        match event {
            RdpEvent::RemoteApp(event) => self.remote_app_event(event, window, cx),
            RdpEvent::Performance(sample) => {
                performance::record_backend(&self.window_state, self.index, &sample);
                self.performance.flush(&self.window_state, self.index);
                return;
            }
            RdpEvent::Connecting => {
                self.close_application_windows(cx);
                self.applications = remote_app::Applications::default();
                self.resize_task = None;
                self.last_resize = None;
                self.cancel_file_clipboard_read();
                self.content_publication = None;
                self.incoming_files = None;
                self.file_export_snapshot = None;
                self.clipboard = clipboard::ClipboardState::default();
                self.file_transfer = None;
                self.release_control(cx);
                self.state = State::Connecting;
            }
            RdpEvent::Connected => {
                self.retry_allowed = false;
                self.state = State::Connected;
                self.error = None;
                if self.config.audio_output {
                    self.send(
                        RdpCommand::SetAudioMuted(
                            self.audio_muted || self.window_state.is_locked(),
                        ),
                        cx,
                    );
                }
                self.request_resize(cx);
            }
            RdpEvent::Disconnected => {
                self.close_application_windows(cx);
                self.content_publication = None;
                self.performance.flush(&self.window_state, self.index);
                self.retry_allowed = false;
                self.incoming_files = None;
                self.file_export_snapshot = None;
                self.release_control(cx);
                self.state = State::Disconnected;
                self.notice = None;
                self.clipboard = clipboard::ClipboardState::default();
                self.file_transfer = None;
            }
            RdpEvent::Failed(error) => {
                self.close_application_windows(cx);
                self.content_publication = None;
                self.performance.flush(&self.window_state, self.index);
                self.retry_allowed = matches!(
                    error,
                    rustterm::rdp::RdpError::Connection
                        | rustterm::rdp::RdpError::Network { .. }
                        | rustterm::rdp::RdpError::Timeout
                );
                self.incoming_files = None;
                self.file_export_snapshot = None;
                self.record_error(&error);
                self.release_control(cx);
                self.state = State::Failed;
                self.error = Some(Message::from_error(error));
                self.notice = None;
                self.clipboard = clipboard::ClipboardState::default();
                self.file_transfer = None;
            }
            RdpEvent::RequestRejected(error) => {
                self.window_state.record("rdp-request-rejected", self.index);
                self.record_error(&error);
                self.error = Some(Message::from_error(error));
                self.notice = None;
                self.clipboard.feedback = None;
            }
            RdpEvent::CertificateAccepted { fingerprint, save } => {
                self.state = State::Connecting;
                if save {
                    self.config.certificate_sha256 = Some(fingerprint.clone());
                    cx.emit(PaneEvent::RdpCertificateTrusted {
                        index: self.index,
                        fingerprint,
                    });
                }
            }
            RdpEvent::SshHostKey(challenge) => {
                self.state = State::WaitingHostKey;
                let prompts = self.window_state.prompts.clone();
                let target = self.config.host.clone();
                let target_port = self.config.port;
                cx.spawn_in(window, async move |this, cx| {
                    let permit = loop {
                        if challenge.reply.is_closed() {
                            return;
                        }
                        let current = this
                            .update(cx, |this, _| {
                                (
                                    !this.closed && this.state == State::WaitingHostKey,
                                    this.surface_visible(),
                                )
                            })
                            .ok();
                        match current {
                            Some((true, true)) => {
                                if let Some(permit) = prompts.acquire() {
                                    break permit;
                                }
                            }
                            Some((true, false)) => {}
                            _ => return,
                        }
                        Timer::after(Duration::from_millis(100)).await;
                    };
                    let detail = rustterm::trf!(
                        i18n_key::CONNECTIONS_SSH_FINGERPRINT_TUNNEL,
                        host = challenge.host,
                        port = challenge.port,
                        algorithm = challenge.algorithm,
                        fingerprint = challenge.fingerprint,
                        target = target,
                        target_port = target_port
                    );
                    let response = cx.update(|window, cx| {
                        window.prompt(
                            PromptLevel::Warning,
                            tr(i18n_key::REDIS_VERIFY_THE_SSH_TUNNEL_HOST_KEY),
                            Some(&detail),
                            &[
                                tr(i18n_key::COMMON_CANCEL),
                                tr(i18n_key::TERMINAL_TRUST_AND_SAVE),
                            ],
                            cx,
                        )
                    });
                    let trusted = match response {
                        Ok(response) => response.await == Ok(1),
                        Err(_) => false,
                    };
                    let current = this
                        .update(cx, |this, cx| {
                            let valid = permit.is_valid()
                                && !this.closed
                                && !this.window_state.is_locked()
                                && this.state == State::WaitingHostKey;
                            if valid {
                                this.state = State::Connecting;
                                cx.emit(PaneEvent::Activity);
                                cx.notify();
                            }
                            valid
                        })
                        .unwrap_or(false);
                    let _ = challenge.reply.send(trusted && current);
                    drop(permit);
                })
                .detach();
            }
            RdpEvent::CertificateChallenge {
                host,
                port,
                fingerprint,
                previous_fingerprint,
                reply,
            } => {
                self.release_control(cx);
                self.state = State::WaitingCertificate;
                let prompts = self.window_state.prompts.clone();
                // Only a pending trust challenge waits on a timer. An idle
                // desktop has no polling or redraw timer.
                cx.spawn_in(window, async move |this, cx| {
                    let permit = loop {
                        if reply.is_closed() {
                            return;
                        }
                        let current = this.update(cx, |this, _| {
                            (!this.closed && this.state == State::WaitingCertificate,
                             this.surface_visible())
                        }).ok();
                        match current {
                            Some((true, true)) => {
                                if let Some(permit) = prompts.acquire() {
                                    break permit;
                                }
                            }
                            Some((true, false)) => {}
                            _ => return,
                        }
                        Timer::after(Duration::from_millis(100)).await;
                    };
                    let detail = match previous_fingerprint {
                        Some(previous) => rustterm::trf!(
                            i18n_key::RDP_THE_RDP_SERVER_CERTIFICATE_HAS_CHANGED_SERVER_HOST_PORT_SAVED,
                            host = host, port = port, previous = previous, fingerprint = fingerprint
                        ),
                        None => rustterm::trf!(
                            i18n_key::RDP_SERVER_HOST_PORT_CERTIFICATE_SHA_256_FINGERPRINT_VERIFY_THIS_FINGERPRINT,
                            host = host, port = port, fingerprint = fingerprint
                        ),
                    };
                    let response = cx.update(|window, cx| window.prompt(
                        PromptLevel::Warning,
                        tr(i18n_key::RDP_VERIFY_RDP_SERVER_CERTIFICATE),
                        Some(&detail),
                        &[tr(i18n_key::COMMON_CANCEL), tr(i18n_key::RDP_TRUST_ONCE), tr(i18n_key::TERMINAL_TRUST_AND_SAVE)],
                        cx,
                    ));
                    let answer = match response {
                        Ok(response) => response.await.ok(),
                        Err(_) => None,
                    };
                    let valid = permit.is_valid();
                    let current = this.update(cx, |this, cx| {
                        let current = valid && !this.window_state.is_locked()
                            && !this.closed && this.state == State::WaitingCertificate;
                        if current {
                            this.state = State::Connecting;
                            cx.emit(PaneEvent::Activity);
                            cx.notify();
                        }
                        current
                    }).unwrap_or(false);
                    let decision = match (current, answer) {
                        (true, Some(1)) => CertificateDecision::Once,
                        (true, Some(2)) => CertificateDecision::Save,
                        _ => CertificateDecision::Reject,
                    };
                    let _ = reply.send(decision);
                    // A native prompt cannot be dismissed programmatically.
                    // Its permit remains exclusive even when its pane closes.
                    drop(permit);
                }).detach();
            }
            RdpEvent::Clipboard {
                revision,
                event: RdpClipboardEvent::Offer,
            } => {
                self.receive_clipboard_offer(revision, window, cx);
            }
            RdpEvent::FileTransfer {
                offer_id,
                files,
                bytes_sent,
                available,
            } => {
                if offer_id >= self.clipboard.revision()
                    && self
                        .file_transfer
                        .as_ref()
                        .is_none_or(|current| offer_id >= current.offer_id)
                {
                    self.file_transfer = Some(FileTransferStatus {
                        offer_id,
                        files,
                        bytes_sent,
                        available,
                    });
                    if !available && offer_id == self.clipboard.revision() {
                        self.clipboard.clear_files();
                    }
                }
            }
            RdpEvent::RemoteFilesAvailable {
                revision,
                offer_id,
                files,
                total_bytes,
            } => {
                self.receive_remote_files(revision, offer_id, files, total_bytes, cx);
            }
            RdpEvent::RemoteFilesProgress {
                offer_id,
                bytes_received,
                ..
            } => {
                if let Some(transfer) = &mut self.incoming_files
                    && transfer.offer_id == offer_id
                {
                    transfer.received = bytes_received;
                }
            }
            RdpEvent::RemoteFilesReady {
                offer_id,
                directory,
                paths,
            } => {
                self.publish_remote_files(offer_id, directory, paths, cx);
            }
            RdpEvent::RemoteFilesFailed { offer_id, error } => {
                if self
                    .incoming_files
                    .as_ref()
                    .is_some_and(|transfer| transfer.offer_id == offer_id)
                {
                    self.incoming_files = None;
                    self.error = Some(Message::from_error(error));
                }
            }
            RdpEvent::RemoteFilesCancelled { offer_id } => {
                if self
                    .incoming_files
                    .as_ref()
                    .is_some_and(|transfer| transfer.offer_id == offer_id && !transfer.ready)
                {
                    self.incoming_files = None;
                    self.clipboard.feedback = Some(clipboard::Feedback::Message(
                        i18n_key::COMMON_FILE_TRANSFER_CANCELLED,
                    ));
                }
            }
            RdpEvent::Clipboard {
                revision,
                event: RdpClipboardEvent::Content(content),
            } => {
                if self.window_state.is_locked() {
                    return;
                }
                self.clipboard
                    .receive(revision, RdpClipboardEvent::Content(content));
                if let Some(content) = self.clipboard.remote_content.clone() {
                    self.publish_remote_content(revision, content, cx);
                }
            }
            RdpEvent::Clipboard { revision, event } => {
                if self.window_state.is_locked() {
                    return;
                }
                let text_received = matches!(&event, RdpClipboardEvent::Text(_));
                let error_code = match &event {
                    RdpClipboardEvent::Rejected(error) => Some(error.diagnostic_code()),
                    _ => None,
                };
                let diagnostic = match &event {
                    RdpClipboardEvent::Published => Some("rdp-clipboard-published"),
                    RdpClipboardEvent::PasteSent => Some("rdp-clipboard-paste-sent"),
                    RdpClipboardEvent::PasteCancelled => Some("rdp-clipboard-paste-cancelled"),
                    RdpClipboardEvent::Rejected(_) => Some("rdp-clipboard-rejected"),
                    RdpClipboardEvent::Offer
                    | RdpClipboardEvent::Text(_)
                    | RdpClipboardEvent::Content(_)
                    | RdpClipboardEvent::Unavailable => None,
                };
                if self.clipboard.receive(revision, event)
                    && let Some(event) = diagnostic
                {
                    self.window_state.record(event, self.index);
                    if let Some(code) = error_code {
                        self.window_state.record(code, self.index);
                    }
                }
                if text_received {
                    if revision == self.clipboard.revision() {
                        self.file_export_snapshot = None;
                    }
                    self.sync_remote_clipboard(window, cx);
                }
            }
        }
        if !self.visible || !self.focus.is_focused(window) {
            self.unread = true;
        }
        cx.emit(PaneEvent::Activity);
        cx.notify();
    }
}

fn connection_config(saved: &RdpConfig, layout: (f32, f32), device_scale: f32) -> RdpConfig {
    let mut config = saved.clone();
    config.desktop_scale = Some(
        saved
            .desktop_scale
            .unwrap_or_else(|| automatic_desktop_scale(device_scale)),
    );
    if config.uses_dynamic_resolution()
        && let Some((width, height)) = resize_dimensions(layout, device_scale)
    {
        config.width = width;
        config.height = height;
    }
    config
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn initial_handshake_uses_the_available_pane_and_keeps_saved_preferences() {
        let saved = RdpConfig::default();
        let before = saved.clone();
        let config = connection_config(&saved, (1441., 900. + ui::PANE_HEADER_HEIGHT), 1.25);
        assert_eq!((config.width, config.height), (1800, 1125));
        assert_eq!(config.desktop_scale, Some(125));
        assert_eq!(saved, before);
    }

    #[test]
    fn fixed_remote_app_and_multi_monitor_connections_keep_their_initial_size() {
        let fixed = RdpConfig {
            width: 1920,
            height: 1080,
            dynamic_resolution: false,
            ..Default::default()
        };
        let multi = RdpConfig {
            monitor_count: 2,
            dynamic_resolution: true,
            ..fixed.clone()
        };
        let application = RdpConfig {
            dynamic_resolution: true,
            remote_app: Some(rustterm::config::RdpRemoteApp {
                program: "||notepad".into(),
                ..Default::default()
            }),
            ..fixed.clone()
        };
        for saved in [fixed, multi, application] {
            let config = connection_config(&saved, (800., 600.), 2.);
            assert_eq!((config.width, config.height), (1920, 1080));
        }
    }

    #[test]
    fn unmeasured_panes_keep_the_fallback_and_explicit_desktop_scale() {
        let saved = RdpConfig {
            desktop_scale: Some(100),
            ..Default::default()
        };
        for layout in [(0., 0.), (800., ui::PANE_HEADER_HEIGHT)] {
            let config = connection_config(&saved, layout, 2.);
            assert_eq!((config.width, config.height), (saved.width, saved.height));
            assert_eq!(config.desktop_scale, Some(100));
        }
    }
}
