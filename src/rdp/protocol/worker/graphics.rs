use super::*;

impl<R: tokio::io::AsyncRead + Unpin> Worker<'_, R> {
    pub(super) async fn publish_frame(&mut self) -> Result<Step> {
        let started = Instant::now();
        let mut frame = if !self.session.graphics_frame_in_progress() {
            // Include legacy updates or local cursor changes after END.
            // The snapshot cache shares all unchanged completed tiles.
            self.graphics.completed = None;
            let mut frame = self.graphics.snapshot.capture(&mut self.session);
            frame.acknowledgement = self.graphics.acknowledgement.clone();
            let elapsed = started.elapsed();
            self.performance.snapshot_time += elapsed;
            self.performance.max_snapshot_time = self.performance.max_snapshot_time.max(elapsed);
            frame
        } else if let Some(frame) = self.graphics.completed.take() {
            frame
        } else {
            self.graphics.clock.suspend();
            return Ok(Step::Skip);
        };
        *self.graphics.sequence = self.graphics.sequence.wrapping_add(1);
        frame.sequence = *self.graphics.sequence;
        frame.published_at = std::time::Instant::now();
        *self
            .graphics
            .presentation
            .lock()
            .unwrap_or_else(|e| e.into_inner()) = frame.acknowledgement.clone();
        self.graphics.frames.send_replace(Some(Arc::new(frame)));
        self.graphics.clock.published(started);
        self.performance.frames += 1;
        Ok(Step::Skip)
    }
    pub(super) async fn report_performance(&mut self) -> Result<Step> {
        let now = Instant::now();
        self.performance_deadline = now + Duration::from_secs(5);
        if self.performance.packets == 0 && self.performance.frames == 0 {
            self.performance_started = now;
        } else if let Ok(permit) = self.events.try_reserve() {
            self.performance.interval = now - self.performance_started;
            self.performance.surface_updates = self.session.info().surface_updates();
            self.performance.nscodec = self.session.info().nscodec();
            self.performance.frame_acknowledgements = self.session.info().frame_acknowledgements()
                || self.graphics.acknowledgement.is_some();
            self.performance.graphics_codec = self.session.graphics_codec();
            self.performance.writer_latency = self.writer.take_latency();
            let input_latency = self.input_worker.take_latency();
            self.performance.command_latency.samples += input_latency.samples;
            self.performance.command_latency.total += input_latency.total;
            self.performance.command_latency.max =
                self.performance.command_latency.max.max(input_latency.max);
            self.performance.udp_connections = self.sidebands.connected();
            self.performance.snapshot_audit = self
                .graphics
                .snapshot
                .take_audit()
                .map(|audit| (audit.captures, audit.checked_tiles, audit.mismatched_tiles));
            if self.performance.snapshot_audit.is_some() {
                self.performance.avc_decode_audit = Some(self.session.take_avc_decode_counts());
            }
            permit.send(RdpEvent::Performance(std::mem::take(&mut self.performance)));
            self.performance_started = now;
        }
        Ok(Step::Skip)
    }
    pub(super) async fn process_packet(
        &mut self,
        packet: tokio_util::bytes::Bytes,
        tunnel: Option<rustrdp::multitransport::TransportProtocol>,
    ) -> Result<Step> {
        let outputs = {
            if tunnel.is_none()
                && let Some(payload) = self.session.server_redirection(&packet)?
            {
                return Ok(Step::Redirect(crate::rdp::handoff::Handoff::parse(
                    payload,
                )?));
            }
            let started = Instant::now();
            let mut snapshot_time = Duration::ZERO;
            let on_frame = |completed: rustrdp::GraphicsFrame<'_>| {
                // END can be followed by the next frame in the same PDU.
                // Capture here, before those later updates mutate the desktop.
                let capture_started = Instant::now();
                let mut frame = self.graphics.snapshot.capture_graphics(
                    completed.graphics,
                    completed.damage,
                    completed.bits_per_pixel,
                );
                self.graphics.acknowledgement = completed.acknowledgement;
                frame.acknowledgement = self.graphics.acknowledgement.clone();
                self.graphics.completed = Some(frame);
                let elapsed = capture_started.elapsed();
                snapshot_time += elapsed;
                self.performance.snapshot_time += elapsed;
                self.performance.max_snapshot_time =
                    self.performance.max_snapshot_time.max(elapsed);
                self.performance.completed_frames += 1;
                self.graphics.clock.graphics_dirty(Instant::now());
            };
            let mut outputs = decode_packet(|| {
                if let Some(protocol) = tunnel {
                    self.session
                        .process_tunnel_with_frames(protocol, &packet, on_frame)
                } else {
                    self.session.process_with_frames(&packet, on_frame)
                }
            })?;
            if self.graphics.separate_pointer {
                let current = self.session.graphics().pointer();
                self.graphics.pointer.send_if_modified(|previous| {
                    if previous.as_ref().is_some_and(|old| {
                        old.image == current.image
                            && old.position_revision == current.position_revision
                    }) {
                        return false;
                    }
                    *previous = Some(current);
                    true
                });
            }
            // MS-RDPEGFX 3.3.5.12: the complete output bitmap has been
            // copied by on_frame. ACK now; monitor refresh is unrelated
            // to receipt, and must not add a display interval to the RTT.
            if let Some(token) = &self.graphics.acknowledgement {
                let acknowledgements = self.session.acknowledge_graphics_output(token)?;
                self.performance.acknowledged_frames += acknowledgements.len() as u64;
                outputs.extend(acknowledgements);
            }
            let elapsed = started.elapsed().saturating_sub(snapshot_time);
            self.performance.packets += 1;
            self.performance.fastpath_packets += u64::from(tunnel.is_none() && packet[0] & 3 == 0);
            self.performance.udp_packets += u64::from(tunnel.is_some());
            self.performance.wire_bytes += packet.len() as u64;
            self.performance.server_graphics_updates += u64::from(
                outputs
                    .iter()
                    .any(|event| matches!(event, Event::GraphicsChanged | Event::Resized { .. })),
            );
            self.performance.process_time += elapsed;
            self.performance.max_process_time = self.performance.max_process_time.max(elapsed);
            outputs
        };
        Ok(Step::Outputs(outputs))
    }
    pub(super) async fn activation(&mut self, was_active: bool) -> Result<()> {
        self.session.resume_input();
        if self.session.is_active() {
            self.activation_deadline = None;
            if self.session.graphics_frame_in_progress() && self.graphics.completed.is_none() {
                self.graphics.clock.suspend();
            }
        } else {
            if let Some(cancel) = self.clipboard.queued_paste.take() {
                cancel.cancel();
            }
            if self
                .clipboard
                .request
                .as_ref()
                .is_some_and(|request| request.paste.is_some() && !request.paste_cancelled())
            {
                let request = self
                    .clipboard
                    .request
                    .take()
                    .expect("pending paste was checked");
                self.events
                    .send(RdpEvent::Clipboard {
                        revision: request.revision,
                        event: RdpClipboardEvent::PasteCancelled,
                    })
                    .await
                    .map_err(|_| RdpError::Disconnected)?;
            }
            self.activation_deadline
                .get_or_insert_with(|| Instant::now() + self.duration);
            self.graphics.clock.suspend();
            if was_active {
                // Discard the old activation once. Frames received while the
                // next activation is finalizing must survive until presentation.
                self.graphics.completed = None;
                self.graphics.acknowledgement = None;
                self.graphics.receive_deadline = None;
            }
        }
        Ok(())
    }
}
