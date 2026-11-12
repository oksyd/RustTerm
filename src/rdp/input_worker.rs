//! Ordered input delivery independent of graphics decoding. Non-input commands
//! form barriers: the next input waits until their protocol outputs are queued.
use super::{commands, protocol, transport::WriterSender, *};
use tokio::{task::JoinHandle, time::Instant};

pub(super) struct CommandCompletion(Option<oneshot::Sender<()>>);
impl Drop for CommandCompletion {
    fn drop(&mut self) {
        if let Some(done) = self.0.take() {
            let _ = done.send(());
        }
    }
}

pub(super) struct InputWorker {
    pub commands: commands::Receiver,
    pub activity: watch::Receiver<Option<Instant>>,
    pub failures: mpsc::Receiver<RdpError>,
    // Also held while the protocol loop encodes and queues synthetic input.
    // Graphics decoding never takes this lock.
    pub order: Arc<tokio::sync::Mutex<()>>,
    latency: Arc<Mutex<RdpLatency>>,
    stop: CancellationToken,
    task: Option<JoinHandle<commands::Receiver>>,
}

impl InputWorker {
    pub fn start(
        source: &mut commands::Receiver,
        encoder: rustrdp::InputController,
        writer: WriterSender,
        events: mpsc::Sender<RdpEvent>,
    ) -> Self {
        let mut source = std::mem::replace(source, commands::closed());
        let (forward, commands) = commands::channel(1, COMMAND_BYTE_LIMIT);
        let (activity_tx, activity) = watch::channel(None);
        let (failures_tx, failures) = mpsc::channel(1);
        let latency = Arc::new(Mutex::new(RdpLatency::default()));
        let measured = latency.clone();
        let stop = CancellationToken::new();
        let cancelled = stop.clone();
        let order = Arc::new(tokio::sync::Mutex::new(()));
        let input_order = order.clone();
        let task = tokio::spawn(async move {
            let work = async {
                let mut rejection = None;
                let mut burst = 0;
                while let Some(mut queued) = source.recv().await {
                    // The mailbox and writer may both stay ready during a burst.
                    // Give the transport a chance to flush accepted input.
                    if burst == 8 {
                        tokio::task::yield_now().await;
                        burst = 0;
                    }
                    burst += 1;
                    if !queued.synchronize
                        && matches!(
                            queued.command,
                            RdpCommand::Key { .. }
                                | RdpCommand::Unicode(_)
                                | RdpCommand::Pointer { .. }
                                | RdpCommand::MouseButton { .. }
                                | RdpCommand::Wheel { .. }
                        )
                    {
                        let _order = input_order.lock().await;
                        measured
                            .lock()
                            .unwrap_or_else(|e| e.into_inner())
                            .record(queued.queued_at.elapsed());
                        let result = encoder.encode(protocol::input(queued.command));
                        match result {
                            Ok(Some(batch)) => {
                                for packet in batch.packets {
                                    writer
                                        .enqueue_input(packet, batch.generation.clone())
                                        .await?;
                                }
                                rejection = None;
                                // One watch value coalesces pointer redraws and idle activity.
                                activity_tx.send_replace(Some(Instant::now()));
                            }
                            Ok(None) => {}
                            Err(
                                error @ (rustrdp::Error::Unsupported(_) | rustrdp::Error::Limit(_)),
                            ) => {
                                protocol::notify_rejection(error, None, &events, &mut rejection)
                                    .await?;
                            }
                            Err(error) => return Err(RdpError::from(error)),
                        }
                    } else {
                        let (done, finished) = oneshot::channel();
                        queued.completion = Some(CommandCompletion(Some(done)));
                        forward
                            .try_send(queued, None)
                            .map_err(|_| RdpError::Disconnected)?;
                        finished.await.map_err(|_| RdpError::Disconnected)?;
                    }
                }
                Ok(())
            };
            let result = tokio::select! {
                biased;
                _ = cancelled.cancelled() => Ok(()),
                result = work => result,
            };
            if let Err(error) = result {
                let _ = failures_tx.try_send(error);
            }
            source
        });
        Self {
            commands,
            activity,
            failures,
            order,
            latency,
            stop,
            task: Some(task),
        }
    }

    pub fn take_latency(&self) -> RdpLatency {
        std::mem::take(&mut self.latency.lock().unwrap_or_else(|e| e.into_inner()))
    }

    pub async fn finish(&mut self, source: &mut commands::Receiver) -> Result<()> {
        self.stop.cancel();
        // Keep the join handle owned by self across await, so cancellation still
        // aborts it through Drop instead of detaching a second mailbox consumer.
        let recovered = self
            .task
            .as_mut()
            .expect("input worker is running")
            .await
            .map_err(|_| RdpError::Runtime)?;
        self.task.take();
        *source = recovered;
        Ok(())
    }
}

impl Drop for InputWorker {
    fn drop(&mut self) {
        self.stop.cancel();
        if let Some(task) = &self.task {
            task.abort();
        }
    }
}

#[cfg(test)]
pub(super) fn test_session() -> rustrdp::Session {
    let config = rustrdp::ClientConfig {
        username: "offline-example".into(),
        width: 320,
        height: 200,
        ..rustrdp::ClientConfig::default()
    };
    let mut connector = rustrdp::Connector::new(config, rustrdp::SecurityProtocol::Hybrid).unwrap();
    connector.start().unwrap();
    let mut bytes = include_bytes!("../../crates/rustrdp/examples/fixtures/server.rdp").as_slice();
    let mut session: Option<rustrdp::Session> = None;
    while !bytes.is_empty() {
        let (_, length) = rustrdp::wire::frame_size(bytes, 4 * 1024 * 1024)
            .unwrap()
            .unwrap();
        let (packet, rest) = bytes.split_at(length);
        bytes = rest;
        if let Some(active) = &mut session {
            active.process(packet).unwrap();
        } else {
            session = connector.advance(packet).unwrap().session;
        }
        if session.as_ref().is_some_and(rustrdp::Session::is_active) {
            return session.unwrap();
        }
    }
    panic!("offline fixture did not activate a desktop");
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;
    use tokio::{io::AsyncReadExt, time::timeout};

    fn packets(session: &mut rustrdp::Session, command: RdpCommand) -> Vec<u8> {
        session
            .input(protocol::input(command))
            .unwrap()
            .into_iter()
            .flat_map(|event| {
                if let rustrdp::Event::Send(bytes) = event {
                    bytes
                } else {
                    Vec::new()
                }
            })
            .collect()
    }

    fn button(pressed: bool) -> RdpCommand {
        RdpCommand::MouseButton {
            button: RdpMouseButton::Left,
            pressed,
            x: 12,
            y: 20,
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn drag_input_reaches_the_wire_before_blocked_graphics_finishes() {
        let mut expected_session = test_session();
        let mut expected = packets(&mut expected_session, button(true));
        expected.extend(packets(
            &mut expected_session,
            RdpCommand::Pointer { x: 30, y: 40 },
        ));
        expected.extend(packets(&mut expected_session, button(false)));
        let (source_tx, mut source) = commands::channel(16, 4096);
        for command in [
            button(true),
            RdpCommand::Pointer { x: 20, y: 30 },
            RdpCommand::Pointer { x: 30, y: 40 },
            button(false),
        ] {
            source_tx
                .try_send(QueuedCommand::new(command, None), None)
                .unwrap();
        }
        let mut session = test_session();
        session.resume_input();
        let input = session.input_controller();
        let (client, mut peer) = tokio::io::duplex(4096);
        let writer = transport::Writer::new(transport::Framed::new(client), Duration::from_secs(2));
        let (events, _received) = mpsc::channel(16);
        let (started_tx, started_rx) = oneshot::channel();
        let (release_tx, release_rx) = std::sync::mpsc::channel();
        let decoding = tokio::spawn(async move {
            protocol::decode_packet(|| {
                std::hint::black_box(&mut session);
                started_tx.send(()).unwrap();
                let _ = release_rx.recv_timeout(Duration::from_secs(3));
            });
            session
        });
        started_rx.await.unwrap();
        let mut worker = InputWorker::start(&mut source, input, writer.sender(), events);
        let mut received = vec![0; expected.len()];
        let read = timeout(Duration::from_millis(500), peer.read_exact(&mut received)).await;
        let independent = !decoding.is_finished();
        let _ = release_tx.send(());
        let _session = decoding.await.unwrap();
        read.unwrap().unwrap();
        assert!(independent, "drag input waited for graphics decoding");
        assert_eq!(received, expected);
        assert_eq!(worker.take_latency().samples, 3);
        worker.finish(&mut source).await.unwrap();
        source_tx
            .try_send(
                QueuedCommand::new(RdpCommand::Pointer { x: 9, y: 8 }, None),
                None,
            )
            .unwrap();
        assert!(matches!(
            source.recv().await.unwrap().command,
            RdpCommand::Pointer { x: 9, y: 8 }
        ));
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn paste_cancelling_input_waits_for_protocol_cleanup() {
        let mut session = test_session();
        session.resume_input();
        let (tx, mut source) = commands::channel(16, 4096);
        let mut cancelling = QueuedCommand::new(button(true), None);
        cancelling.synchronize = true;
        tx.try_send(cancelling, None).unwrap();
        tx.try_send(QueuedCommand::new(button(false), None), None)
            .unwrap();
        let (client, mut peer) = tokio::io::duplex(4096);
        let writer = transport::Writer::new(transport::Framed::new(client), Duration::from_secs(2));
        let (events, _received) = mpsc::channel(16);
        let mut worker = InputWorker::start(
            &mut source,
            session.input_controller(),
            writer.sender(),
            events,
        );
        let mut command = timeout(Duration::from_secs(1), worker.commands.recv())
            .await
            .unwrap()
            .unwrap();
        assert!(matches!(
            command.command,
            RdpCommand::MouseButton { pressed: true, .. }
        ));
        assert!(
            timeout(Duration::from_millis(20), peer.read_u8())
                .await
                .is_err()
        );
        let completion = command.completion.take();
        let first = packets(&mut session, command.command);
        writer.enqueue(first.clone(), None, None).await.unwrap();
        let mut expected_session = test_session();
        let mut expected = packets(&mut expected_session, button(true));
        assert_eq!(first, expected);
        expected.extend(packets(&mut expected_session, button(false)));
        drop(completion);
        let mut received = vec![0; expected.len()];
        timeout(Duration::from_secs(1), peer.read_exact(&mut received))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(received, expected);
        worker.finish(&mut source).await.unwrap();
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn synthetic_input_is_queued_before_a_new_button_press() {
        let mut session = test_session();
        session.resume_input();
        let (tx, mut source) = commands::channel(16, 4096);
        let (client, mut peer) = tokio::io::duplex(4096);
        let writer = transport::Writer::new(transport::Framed::new(client), Duration::from_secs(2));
        let (events, _received) = mpsc::channel(16);
        let mut worker = InputWorker::start(
            &mut source,
            session.input_controller(),
            writer.sender(),
            events,
        );
        let order = worker.order.clone();
        let guard = order.lock().await;
        tx.try_send(QueuedCommand::new(button(true), None), None)
            .unwrap();
        assert!(
            timeout(Duration::from_millis(20), peer.read_u8())
                .await
                .is_err()
        );
        let awake = session.keep_awake().unwrap();
        let [rustrdp::Event::Send(packet)] = awake.as_slice() else {
            panic!("button changed input state before synthetic input was queued");
        };
        let mut expected = packet.clone();
        writer.enqueue(packet.clone(), None, None).await.unwrap();
        drop(guard);
        expected.extend(packets(&mut test_session(), button(true)));
        let mut received = vec![0; expected.len()];
        timeout(Duration::from_secs(1), peer.read_exact(&mut received))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(received, expected);
        assert!(session.keep_awake().unwrap().is_empty());
        worker.finish(&mut source).await.unwrap();
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn command_barrier_keeps_focus_release_before_later_input() {
        let mut session = test_session();
        session.resume_input();
        let (tx, mut source) = commands::channel(16, 4096);
        for command in [
            RdpCommand::Key {
                scancode: 0x1d,
                pressed: true,
            },
            RdpCommand::ReleaseAll,
            RdpCommand::Pointer { x: 10, y: 11 },
        ] {
            tx.try_send(QueuedCommand::new(command, None), None)
                .unwrap();
        }
        let mut expected_session = test_session();
        let first = packets(
            &mut expected_session,
            RdpCommand::Key {
                scancode: 0x1d,
                pressed: true,
            },
        );
        let mut expected = packets(&mut expected_session, RdpCommand::ReleaseAll);
        expected.extend(packets(
            &mut expected_session,
            RdpCommand::Pointer { x: 10, y: 11 },
        ));
        let (client, mut peer) = tokio::io::duplex(4096);
        let writer = transport::Writer::new(transport::Framed::new(client), Duration::from_secs(2));
        let (events, _received) = mpsc::channel(16);
        let mut worker = InputWorker::start(
            &mut source,
            session.input_controller(),
            writer.sender(),
            events,
        );
        let mut command = timeout(Duration::from_secs(1), worker.commands.recv())
            .await
            .unwrap()
            .unwrap();
        let completion = command.completion.take();
        assert!(matches!(command.command, RdpCommand::ReleaseAll));
        let mut received = vec![0; first.len()];
        peer.read_exact(&mut received).await.unwrap();
        assert_eq!(received, first);
        assert!(
            timeout(Duration::from_millis(20), peer.read_u8())
                .await
                .is_err()
        );
        for event in session.input(rustrdp::Input::ReleaseAll).unwrap() {
            if let rustrdp::Event::Send(bytes) = event {
                writer.enqueue(bytes, None, None).await.unwrap();
            }
        }
        drop(completion);
        let mut received = vec![0; expected.len()];
        timeout(Duration::from_secs(1), peer.read_exact(&mut received))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(received, expected);
        worker.finish(&mut source).await.unwrap();
    }
}
