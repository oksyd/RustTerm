use super::*;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn runtime_preserves_early_pixels_input_and_fragmented_server_disconnect() {
    for remote_disconnect in [true, false] {
        let (session, tail) = crate::rdp::input_worker::test_session_with_tail();
        let (socket, mut peer) = tokio::io::duplex(65536);
        let config = RdpConfig {
            width: 320,
            height: 200,
            timeout_secs: 2,
            audio_output: false,
            audio_input: false,
            ..RdpConfig::default()
        };
        let ready = startup::Ready {
            config,
            session,
            framed: Framed::new(socket),
            sidebands: Sidebands::new(
                None,
                "offline.test".into(),
                String::new(),
                Duration::from_secs(2),
            ),
            startup_tunnels: Default::default(),
            last_rejection: None,
            startup_audio: Vec::new(),
            startup_files: Vec::new(),
            frame_snapshot: FrameSnapshot::default(),
            completed_frame: None,
            completed_acknowledgement: None,
        };
        let (sender, mut commands) = commands::channel(16, 65536);
        let (events, mut received) = mpsc::channel(16);
        let (frames, mut displayed) = watch::channel(None);
        let (presented_sender, mut presented) = watch::channel(None);
        let (pointer, cursor) = watch::channel(None);
        let mut sequence = 40;
        let redirection = RedirectionState {
            tunnel: None,
            pointer,
            cache_directory: None,
            presentation: Arc::new(Mutex::new(None)),
            audio_muted: Arc::new(AtomicBool::new(false)),
            microphone_enabled: Arc::new(AtomicBool::new(false)),
            locked: Arc::new(AtomicBool::new(false)),
            capture_transmission: Arc::new(Mutex::new(CancellationToken::new())),
        };
        let frontend = Frontend {
            events: &events,
            frames,
            presented: &mut presented,
            frame_sequence: &mut sequence,
            redirection,
        };
        let mut worker = Worker::start(ready, &mut commands, frontend).await.unwrap();
        assert!(matches!(received.try_recv(), Ok(RdpEvent::Connected)));
        assert!(cursor.borrow().is_some());
        let (finished, completion) = oneshot::channel();
        let run = async {
            let result = worker.run().await;
            let _ = finished.send(());
            worker.input_worker.finish(&mut commands).await.unwrap();
            assert!(
                matches!(result, Ok(None)),
                "runtime did not close cleanly (remote={remote_disconnect}): {:?}",
                result.as_ref().err()
            );
        };
        let user_and_server = async {
            displayed.changed().await.unwrap();
            {
                let frame = displayed.borrow();
                let frame = frame.as_ref().unwrap();
                assert_eq!((frame.width, frame.height, frame.sequence), (320, 200, 41));
                let tile = frame
                    .tiles
                    .iter()
                    .find(|tile| tile.x == 0 && tile.y == 0)
                    .unwrap();
                let offset = (4 * usize::from(tile.width) + 3) * 4;
                assert_eq!(
                    &tile.pixels[offset..offset + 8],
                    &[0x11, 0x22, 0x33, 255, 0x44, 0x55, 0x66, 255]
                );
            }
            // Verify bytes observed by the peer, not just admission to an input queue.
            let mut expected_session = crate::rdp::input_worker::test_session();
            let mut expected = Vec::new();
            for pressed in [true, false] {
                for event in expected_session
                    .input(Input::Key {
                        scancode: 0x1e,
                        pressed,
                    })
                    .unwrap()
                {
                    if let Event::Send(packet) = event {
                        expected.extend(packet);
                    }
                }
                sender
                    .try_send(
                        QueuedCommand::new(
                            RdpCommand::Key {
                                scancode: 0x1e,
                                pressed,
                            },
                            None,
                        ),
                        None,
                    )
                    .unwrap();
            }
            let mut delivered = vec![0; expected.len()];
            peer.read_exact(&mut delivered).await.unwrap();
            assert_eq!(delivered, expected);
            if remote_disconnect {
                // Feed the rest of the recording one byte at a time, including its
                // disconnect PDU; keep the transport open until the loop recognizes it.
                for byte in tail {
                    peer.write_all(&[*byte]).await.unwrap();
                }
                let _keep_transport_open = peer;
                completion.await.unwrap();
            } else {
                drop(presented_sender);
                let _keep_transport_open = peer;
                completion.await.unwrap();
            }
        };
        timeout(Duration::from_secs(3), async {
            tokio::join!(run, user_and_server);
        })
        .await
        .unwrap();
    }
}
