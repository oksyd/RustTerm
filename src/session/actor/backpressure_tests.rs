use super::*;
use crate::session::{SelectionKind, Session};
use tokio::{sync::oneshot, task::JoinHandle, time::timeout};

struct BlockedWriter {
    session: Session,
    output: mpsc::Sender<Output>,
    writes: mpsc::Receiver<WriteCommand>,
    exit: oneshot::Sender<Result<Option<u32>, String>>,
    actor: JoinHandle<()>,
}

impl BlockedWriter {
    fn start() -> Self {
        let (session, terminal, commands) = Session::create(80, 3, 0).unwrap();
        session.shared.state(SessionState::Connected);
        let (output, receive) = mpsc::channel(2);
        let (input, writes) = mpsc::channel(1);
        input
            .try_send(WriteCommand::Data(b"occupied".to_vec()))
            .unwrap();
        let (exit, exited) = oneshot::channel();
        let actor = tokio::spawn(run(
            terminal,
            commands,
            session.shared.clone(),
            Some(Transport {
                output: receive,
                input,
                exit: exited,
                tasks: vec![],
                reaper: None,
            }),
        ));
        Self {
            session,
            output,
            writes,
            exit,
            actor,
        }
    }

    async fn snapshot_containing(&self, text: &str) {
        let mut snapshots = self.session.subscribe_snapshots();
        timeout(Duration::from_secs(1), async {
            loop {
                self.session.request_snapshot();
                snapshots.changed().await.unwrap();
                let snapshot = snapshots.borrow_and_update();
                if snapshot.lines.iter().any(|line| {
                    line.cells
                        .iter()
                        .map(|cell| cell.text.as_str())
                        .collect::<String>()
                        .contains(text)
                }) {
                    break;
                }
            }
        })
        .await
        .unwrap();
    }

    async fn next_write(&mut self) -> WriteCommand {
        timeout(Duration::from_secs(1), self.writes.recv())
            .await
            .unwrap()
            .unwrap()
    }

    async fn close(self) {
        self.exit.send(Ok(None)).unwrap();
        self.session.close();
        timeout(Duration::from_secs(1), self.actor)
            .await
            .unwrap()
            .unwrap();
    }
}

#[tokio::test]
async fn blocked_writer_keeps_output_snapshot_and_copy_live_without_reordering_input() {
    let mut fixture = BlockedWriter::start();
    fixture
        .session
        .send(Command::Input(b"first".to_vec()))
        .unwrap();
    fixture
        .session
        .send(Command::Input(b"second".to_vec()))
        .unwrap();
    fixture.session.resize(81, 3).unwrap();
    fixture
        .output
        .send(Output::Data(b"screen".to_vec()))
        .await
        .unwrap();
    fixture.snapshot_containing("screen").await;
    fixture
        .session
        .send(Command::BeginSelection {
            column: 0,
            row: 0,
            kind: SelectionKind::Word,
        })
        .unwrap();
    let copied = timeout(
        Duration::from_secs(1),
        fixture.session.copy_selection().unwrap(),
    )
    .await
    .unwrap()
    .unwrap();
    assert_eq!(copied.as_deref(), Some("screen"));
    assert!(
        matches!(fixture.next_write().await, WriteCommand::Data(bytes) if bytes == b"occupied")
    );
    for expected in [b"first".as_slice(), b"second".as_slice()] {
        assert!(
            matches!(fixture.next_write().await, WriteCommand::UserInput(bytes) if bytes == expected)
        );
        fixture.session.shared.input_completed();
    }
    assert!(matches!(
        fixture.next_write().await,
        WriteCommand::Resize(81, 3)
    ));
    assert_eq!(
        fixture
            .session
            .shared
            .unacknowledged_inputs
            .load(Ordering::Acquire),
        0
    );
    fixture.close().await;
}

#[tokio::test]
async fn saturated_outbox_preserves_accepted_input_and_continues_reading_output() {
    let mut fixture = BlockedWriter::start();
    for value in 0_u8..60 {
        fixture.session.send(Command::Input(vec![value])).unwrap();
    }
    // Ensure the first batch is waiting in the actor before filling the
    // command queue again. The transport remains blocked throughout.
    timeout(
        Duration::from_secs(1),
        fixture.session.copy_selection().unwrap(),
    )
    .await
    .unwrap()
    .unwrap();
    for value in 60_u8..120 {
        fixture.session.send(Command::Input(vec![value])).unwrap();
    }
    let waiting_commands = 120 - (COMMAND_CAPACITY - 1);
    let remaining_capacity = COMMAND_CAPACITY - waiting_commands;
    timeout(Duration::from_secs(1), async {
        while fixture.session.commands.capacity() < remaining_capacity {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    assert_eq!(fixture.session.commands.capacity(), remaining_capacity);
    // More output batches than the output channel can hold prove that the
    // actor still consumes the peer's data while input admission is paused.
    timeout(Duration::from_secs(1), async {
        for text in [b"still ".as_slice(), b"reading", b"\x1b[6n"] {
            fixture
                .output
                .send(Output::Data(text.to_vec()))
                .await
                .unwrap();
        }
    })
    .await
    .unwrap();
    assert!(matches!(fixture.session.state(), SessionState::Connected));
    // Snapshot requests must bypass the saturated write FIFO. The writer is
    // still blocked, yet output remains visible without admitting more input.
    fixture.snapshot_containing("still reading").await;
    assert!(
        matches!(fixture.next_write().await, WriteCommand::Data(bytes) if bytes == b"occupied")
    );
    let mut received = Vec::new();
    let mut replies = 0;
    while received.len() < 120 || replies == 0 {
        match fixture.next_write().await {
            WriteCommand::UserInput(bytes) => {
                received.extend(bytes);
                fixture.session.shared.input_completed();
            }
            WriteCommand::Data(bytes) => {
                assert_eq!(bytes, b"\x1b[1;14R");
                replies += 1;
            }
            WriteCommand::Resize(..) => panic!("unexpected resize"),
        }
    }
    assert_eq!(received, (0_u8..120).collect::<Vec<_>>());
    assert_eq!(replies, 1);
    assert_eq!(
        fixture
            .session
            .shared
            .unacknowledged_inputs
            .load(Ordering::Acquire),
        0
    );
    fixture.snapshot_containing("still reading").await;
    fixture.close().await;
}

#[tokio::test]
async fn zmodem_handshake_quarantines_writes_already_waiting_for_transport_capacity() {
    let mut fixture = BlockedWriter::start();
    fixture
        .output
        .send(Output::Data(
            b"\x1b[?1000h\x1b[?1006h\x1b[?1004hready".to_vec(),
        ))
        .await
        .unwrap();
    fixture.snapshot_containing("ready").await;
    fixture
        .session
        .send(Command::Input(b"do not replay".to_vec()))
        .unwrap();
    fixture
        .session
        .send(Command::Mouse {
            button: 0,
            column: 0,
            row: 0,
            pressed: true,
            motion: false,
            shift: false,
            alt: false,
            ctrl: false,
        })
        .unwrap();
    fixture.session.send(Command::Focus(true)).unwrap();
    fixture.session.resize(90, 3).unwrap();
    // The copy receipt is a FIFO barrier behind all pending ordinary writes.
    timeout(
        Duration::from_secs(1),
        fixture.session.copy_selection().unwrap(),
    )
    .await
    .unwrap()
    .unwrap();
    fixture
        .output
        .send(Output::Data(
            b"\x1b[6n**\x18B00000000000000\r\n\x11".to_vec(),
        ))
        .await
        .unwrap();
    let status = timeout(Duration::from_secs(1), async {
        loop {
            if let Some(status) = fixture.session.zmodem_status() {
                break status;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    fixture.session.resize(100, 3).unwrap();
    timeout(
        Duration::from_secs(1),
        fixture.session.copy_selection().unwrap(),
    )
    .await
    .unwrap()
    .unwrap();
    assert!(
        matches!(fixture.next_write().await, WriteCommand::Data(bytes) if bytes == b"occupied")
    );
    fixture.session.cancel_zmodem(status.id).unwrap();
    assert!(
        matches!(fixture.next_write().await, WriteCommand::Data(bytes) if bytes.starts_with(b"\x18\x18\x18\x18"))
    );
    assert!(matches!(
        fixture.next_write().await,
        WriteCommand::Resize(100, 3)
    ));
    assert_eq!(
        fixture
            .session
            .shared
            .unacknowledged_inputs
            .load(Ordering::Acquire),
        0
    );
    assert!(fixture.writes.try_recv().is_err());
    fixture
        .session
        .send(Command::Input(b"after".to_vec()))
        .unwrap();
    assert!(
        matches!(fixture.next_write().await, WriteCommand::UserInput(bytes) if bytes == b"after")
    );
    fixture.session.shared.input_completed();
    fixture.close().await;
}
