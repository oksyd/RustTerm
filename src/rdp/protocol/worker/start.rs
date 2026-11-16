use super::*;

impl<'a, S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin + Send + 'static>
    Worker<'a, tokio::io::ReadHalf<S>>
{
    pub(in crate::rdp::protocol) async fn start(
        ready: startup::Ready<S>,
        commands: &mut commands::Receiver,
        frontend: Frontend<'a>,
    ) -> Result<Self> {
        let Frontend {
            events,
            frames,
            presented,
            frame_sequence,
            redirection,
        } = frontend;
        let RedirectionState {
            pointer,
            cache_directory,
            presentation,
            audio_muted,
            microphone_enabled,
            locked,
            capture_transmission,
            ..
        } = redirection;
        let startup::Ready {
            config,
            mut session,
            mut framed,
            sidebands,
            startup_tunnels,
            mut last_rejection,
            startup_audio,
            startup_files,
            frame_snapshot,
            mut completed_frame,
            completed_acknowledgement,
        } = ready;
        let duration = Duration::from_secs(config.timeout_secs);
        let frame_interval = Duration::from_secs(1) / u32::from(config.max_fps);
        let clipboard_revision = 0;
        let separate_pointer = config.remote_app.is_none();
        if separate_pointer {
            session.set_pointer_composition(false);
            completed_frame = None;
            pointer.send_replace(Some(session.graphics().pointer()));
        }
        framed.set_stage(ConnectionStage::Session);
        let (framed, outgoing) = framed.split();
        let writer = Writer::new(outgoing, duration);
        let deliveries = Deliveries::new();
        let frame_clock = FrameClock::new(frame_interval, Instant::now());
        let frame_receive_deadline = session
            .graphics_frame_in_progress()
            .then(|| Instant::now() + duration);
        let mut performance = RdpPerformance::default();
        let performance_started = Instant::now();
        let performance_deadline = performance_started + Duration::from_secs(5);
        let activation_deadline = None;
        let desktop_activity =
            crate::rdp::keepalive::DesktopActivity::new(config.prevent_idle, Instant::now());
        let clipboard_request: Option<ClipboardRequest> = None;
        let queued_paste: Option<CancellationToken> = None;
        let files = Transfers::new();
        let downloads = Downloads::new(cache_directory);
        let remote_files: Option<(u64, Vec<rustrdp::ClipboardFile>)> = None;
        let download_deadline = None;
        let progress = BTreeMap::new();
        let progress_deadline = None;
        let capture: Option<audio::Capture> = None;
        let drives = if config.shared_folders.is_empty() {
            None
        } else {
            let folders = config.shared_folders.clone();
            let share_lock = locked.clone();
            Some(
                timeout(
                    duration,
                    tokio::task::spawn_blocking(move || {
                        crate::rdp::shared_drives::SharedDrives::new(&folders, share_lock)
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
        if let Some(token) = &completed_acknowledgement {
            let acknowledgements = session.acknowledge_graphics_output(token)?;
            performance.acknowledged_frames += acknowledgements.len() as u64;
            initial_outputs.extend(acknowledgements);
        }
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
        session.resume_input();
        let input_worker = crate::rdp::input_worker::InputWorker::start(
            commands,
            session.input_controller(),
            writer.sender(),
            events.clone(),
        );
        let input_order = input_worker.order.clone();
        let worker = Worker {
            config,
            events,
            presented,
            session,
            framed,
            writer,
            sidebands,
            duration,
            input_worker,
            input_order,
            last_rejection,
            desktop_activity,
            activation_deadline,
            performance,
            performance_started,
            performance_deadline,
            startup_tunnels,
            initial_outputs,
            graphics: Graphics {
                clock: frame_clock,
                snapshot: frame_snapshot,
                completed: completed_frame,
                acknowledgement: completed_acknowledgement,
                receive_deadline: frame_receive_deadline,
                sequence: frame_sequence,
                frames,
                pointer,
                presentation,
                separate_pointer,
            },
            clipboard: Clipboard {
                revision: clipboard_revision,
                request: clipboard_request,
                queued_paste,
                files,
                downloads,
                remote_files,
                download_deadline,
                progress,
                progress_deadline,
                deliveries,
            },
            media: Media {
                capture,
                drives,
                playback,
                microphone_enabled,
                locked,
                capture_transmission,
            },
        };
        Ok(worker)
    }
}
