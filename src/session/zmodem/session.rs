//! ZMODEM admission and transport ownership. The terminal actor remains the only
//! consumer of remote bytes and the only producer of PTY/SSH writes.
use crate::i18n::messages as i18n_key;

use super::{
    Detector, Direction as ZmodemDirection, Engine, Event, Outcome, Progress as ZmodemProgress,
    Request, Selection as ZmodemSelection,
};
use crate::session::{
    FailureKind,
    state::Shared,
    transport::{SHUTDOWN_TIMEOUT, WriteCommand, enqueue_write},
};
use std::{
    path::PathBuf,
    sync::{Arc, atomic::Ordering},
    time::Duration,
};
use tokio::sync::mpsc;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ZmodemPhase {
    Waiting,
    Running,
    Cancelling,
    Completed,
    Cancelled,
    Failed,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ZmodemStatus {
    pub id: u64,
    pub phase: ZmodemPhase,
    pub progress: ZmodemProgress,
    pub message: String,
    pub files: Vec<PathBuf>,
}
impl ZmodemStatus {
    pub fn is_active(&self) -> bool {
        matches!(
            self.phase,
            ZmodemPhase::Waiting | ZmodemPhase::Running | ZmodemPhase::Cancelling
        )
    }
}

const SELECTION_TIMEOUT: Duration = Duration::from_secs(120);
const MAX_PENDING_BYTES: usize = 64 * 1024;
const CANCEL: &[u8] = b"\x18\x18\x18\x18\x18\x18\x18\x18\x08\x08\x08\x08\x08\x08\x08\x08";

pub(in crate::session) struct Transfer {
    shared: Arc<Shared>,
    wire: mpsc::Sender<WriteCommand>,
    detector: Detector,
    pending: Option<Request>,
    next_request: Option<Request>,
    discard_deferred: bool,
    engine: Option<Engine>,
    deadline: Option<tokio::time::Instant>,
    prefix_deadline: Option<tokio::time::Instant>,
    pub events: mpsc::Receiver<Event>,
    event_sender: mpsc::Sender<Event>,
}

impl Transfer {
    pub fn new(shared: Arc<Shared>, wire: mpsc::Sender<WriteCommand>) -> Self {
        let (event_sender, events) = mpsc::channel(32);
        Self {
            shared,
            wire,
            detector: Detector::default(),
            pending: None,
            next_request: None,
            discard_deferred: false,
            engine: None,
            deadline: None,
            prefix_deadline: None,
            events,
            event_sender,
        }
    }
    pub fn active(&self) -> bool {
        self.pending.is_some() || self.engine.is_some()
    }
    pub fn take_discard_deferred(&mut self) -> bool {
        std::mem::take(&mut self.discard_deferred)
    }
    pub fn note_discarded_input(&self, count: usize) {
        if count > 0 {
            self.update(|s| {
                s.message.push_str(&crate::trf!(
                    i18n_key::TERMINAL_BUFFERED_INPUTS_CANCELLED,
                    count = count
                ))
            });
        }
    }
    pub fn deadline(&self) -> Option<tokio::time::Instant> {
        self.deadline
    }
    pub fn prefix_deadline(&self) -> Option<tokio::time::Instant> {
        self.prefix_deadline
    }
    pub fn flush_prefix(&mut self) -> Vec<u8> {
        self.prefix_deadline = None;
        self.detector.flush()
    }
    fn id(&self) -> Option<u64> {
        self.shared
            .zmodem
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .as_ref()
            .map(|s| s.id)
    }
    fn update(&self, f: impl FnOnce(&mut ZmodemStatus)) {
        if let Some(status) = self
            .shared
            .zmodem
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .as_mut()
        {
            f(status);
            self.shared.changed();
        }
    }
    fn finish(&mut self, outcome: Outcome) {
        self.pending = None;
        self.engine = None;
        self.deadline = None;
        self.discard_deferred |= !outcome.success;
        self.update(|s| {
            s.phase = if outcome.success {
                ZmodemPhase::Completed
            } else if outcome.cancelled {
                ZmodemPhase::Cancelled
            } else {
                ZmodemPhase::Failed
            };
            s.message = outcome.message;
            s.files = outcome.files;
        });
        if let Some(request) = self.next_request.take() {
            self.begin(request);
        }
    }
    async fn cancel_wire(&self) -> std::result::Result<(), FailureKind> {
        enqueue_write(
            &self.wire,
            WriteCommand::Data(CANCEL.to_vec()),
            &self.shared,
        )
        .await
    }
    pub async fn receive(&mut self, bytes: Vec<u8>) -> std::result::Result<Vec<u8>, FailureKind> {
        if self.engine.is_some() {
            return self.feed_engine(bytes).await;
        }
        if let Some(request) = &mut self.pending {
            if request.initial.len().saturating_add(bytes.len()) <= MAX_PENDING_BYTES {
                request.initial.extend_from_slice(&bytes);
            } else {
                self.cancel_wire().await?;
                self.finish(Outcome {
                    success: false,
                    cancelled: false,
                    message: crate::i18n::tr(i18n_key::CONNECTIONS_DATA_RECEIVED_WHILE_WAITING_FOR_FILE_SELECTION_EXCEEDED_THE_LIMIT).into(),
                    files: Vec::new(),
                });
            }
            return Ok(Vec::new());
        }
        self.terminal_bytes(bytes, false)
    }
    fn terminal_bytes(
        &mut self,
        bytes: Vec<u8>,
        finishing: bool,
    ) -> std::result::Result<Vec<u8>, FailureKind> {
        if let Some(request) = self.next_request.as_mut().or(self.pending.as_mut()) {
            if request.initial.len().saturating_add(bytes.len()) > MAX_PENDING_BYTES {
                return Err(FailureKind::Backpressure);
            }
            request.initial.extend_from_slice(&bytes);
            return Ok(Vec::new());
        }
        let detection = self.detector.feed(&bytes);
        self.prefix_deadline = self.detector.has_pending().then(|| {
            tokio::time::Instant::now()
                + if self.detector.pending_header() {
                    Duration::from_secs(2)
                } else {
                    Duration::from_millis(250)
                }
        });
        if let Some(request) = detection.request {
            self.prefix_deadline = None;
            if finishing {
                self.next_request = Some(request);
            } else {
                self.begin(request);
            }
        }
        Ok(detection.visible)
    }
    fn begin(&mut self, request: Request) {
        let id = self.shared.transfer_id.fetch_add(1, Ordering::Relaxed);
        let status = ZmodemStatus {
            id,
            phase: ZmodemPhase::Waiting,
            progress: ZmodemProgress {
                direction: request.direction,
                bytes_transferred: 0,
                total_bytes: None,
                files_completed: 0,
            },
            message: match request.direction {
                ZmodemDirection::Download => crate::i18n::tr(
                    i18n_key::CONNECTIONS_THE_REMOTE_HOST_WANTS_TO_SEND_FILES_CHOOSE_A_DESTINATION,
                )
                .into(),
                ZmodemDirection::Upload => crate::i18n::tr(
                    i18n_key::CONNECTIONS_THE_REMOTE_HOST_IS_WAITING_FOR_FILES_CHOOSE_FILES_TO,
                )
                .into(),
            },
            files: Vec::new(),
        };
        *self.shared.zmodem.lock().unwrap_or_else(|e| e.into_inner()) = Some(status);
        self.shared.changed();
        self.pending = Some(request);
        self.deadline = Some(tokio::time::Instant::now() + SELECTION_TIMEOUT);
    }
    async fn feed_engine(&mut self, bytes: Vec<u8>) -> std::result::Result<Vec<u8>, FailureKind> {
        let engine = self.engine.take().expect("checked active engine");
        let mut completed = false;
        let result = async {
            let feeding = engine.feed(bytes);
            tokio::pin!(feeding);
            let mut visible = Vec::new();
            let result = loop {
                tokio::select! {
                    biased;
                    Some(event) = self.events.recv() => {
                        completed |= matches!(event, Event::Finished(_));
                        visible.extend(self.event(event).await?);
                    }
                    result = &mut feeding => break result,
                }
            };
            if let Err(error) = result {
                if error.terminal {
                    // Finalization can race the outer actor's output selection.
                    // Render all older tail events before the unaccepted suffix.
                    while let Ok(event) = self.events.try_recv() {
                        completed |= matches!(event, Event::Finished(_));
                        visible.extend(self.event(event).await?);
                    }
                    visible.extend(self.terminal_bytes(error.bytes, !completed)?);
                } else if !completed {
                    engine.cancel();
                    self.update(|s| {
                        s.phase = ZmodemPhase::Cancelling;
                        s.message = crate::trf!(
                            i18n_key::CONNECTIONS_TRANSFER_DATA_PROCESSING_FAILED_STOPPING_THE_TRANSFER_ERROR,
                            error = error.message
                        );
                    });
                }
            }
            Ok(visible)
        }
        .await;
        if !completed {
            self.engine = Some(engine);
        }
        result
    }
    pub async fn start(
        &mut self,
        id: u64,
        selection: ZmodemSelection,
    ) -> std::result::Result<(), FailureKind> {
        if self.id() != Some(id) || self.engine.is_some() {
            return Ok(());
        }
        let Some(request) = self.pending.take() else {
            return Ok(());
        };
        self.deadline = None;
        match Engine::start(
            request,
            selection,
            self.event_sender.clone(),
            self.shared.io_stop.child_token(),
        ) {
            Ok(engine) => {
                let progress = engine.status();
                self.engine = Some(engine);
                self.update(|s| {
                    s.phase = ZmodemPhase::Running;
                    s.progress = progress;
                    s.message = crate::i18n::tr(i18n_key::COMMON_TRANSFERRING_FILES).into();
                });
            }
            Err(error) => {
                self.cancel_wire().await?;
                self.finish(Outcome {
                    success: false,
                    cancelled: false,
                    message: crate::trf!(
                        i18n_key::CONNECTIONS_CANNOT_START_FILE_TRANSFER_ERROR,
                        error = format!("{error:#}")
                    ),
                    files: Vec::new(),
                });
            }
        }
        Ok(())
    }
    pub async fn cancel(&mut self, id: u64) -> std::result::Result<(), FailureKind> {
        if self.id() != Some(id) {
            return Ok(());
        }
        if let Some(engine) = &self.engine {
            engine.cancel();
            self.update(|s| {
                s.phase = ZmodemPhase::Cancelling;
                s.message = crate::i18n::tr(i18n_key::CONNECTIONS_CANCELLING_TRANSFER).into();
            });
        } else if self.pending.is_some() {
            self.cancel_wire().await?;
            self.finish(Outcome {
                success: false,
                cancelled: true,
                message: crate::i18n::tr(i18n_key::COMMON_FILE_TRANSFER_CANCELLED).into(),
                files: Vec::new(),
            });
        }
        Ok(())
    }
    pub async fn expire(&mut self) -> std::result::Result<(), FailureKind> {
        if self.pending.is_some() {
            self.cancel_wire().await?;
            self.finish(Outcome {
                success: false,
                cancelled: false,
                message: crate::i18n::tr(
                    i18n_key::CONNECTIONS_FILE_SELECTION_TIMED_OUT_TRANSFER_STOPPED,
                )
                .into(),
                files: Vec::new(),
            });
        }
        Ok(())
    }
    pub async fn event(&mut self, event: Event) -> std::result::Result<Vec<u8>, FailureKind> {
        match event {
            Event::Write(bytes) => {
                enqueue_write(&self.wire, WriteCommand::Data(bytes), &self.shared).await?
            }
            Event::Terminal(bytes) => return self.terminal_bytes(bytes, true),
            Event::Progress(progress) => self.update(|s| s.progress = progress),
            Event::Finished(outcome) => self.finish(outcome),
        }
        Ok(Vec::new())
    }
    pub async fn shutdown(&mut self) -> Vec<u8> {
        if let Some(engine) = &self.engine {
            engine.cancel();
        }
        let mut visible = Vec::new();
        let deadline = tokio::time::Instant::now() + SHUTDOWN_TIMEOUT;
        while self.engine.is_some() {
            let Ok(Some(event)) = tokio::time::timeout_at(deadline, self.events.recv()).await
            else {
                break;
            };
            match event {
                Event::Terminal(bytes) => {
                    if let Ok(bytes) = self.terminal_bytes(bytes, true) {
                        visible.extend(bytes);
                    }
                }
                Event::Finished(outcome) => {
                    self.next_request = None;
                    self.finish(outcome);
                }
                Event::Progress(progress) => self.update(|s| s.progress = progress),
                Event::Write(_) => {}
            }
        }
        if self.active() {
            self.update(|s| {
                s.phase = ZmodemPhase::Cancelled;
                s.message = if self.engine.is_some() {
                    crate::i18n::tr(i18n_key::CONNECTIONS_THE_CONNECTION_ENDED_FILE_TRANSFER_CLEANUP_WAS_NOT_CONFIRMED)
                } else {
                    crate::i18n::tr(
                        i18n_key::CONNECTIONS_THE_CONNECTION_ENDED_THE_FILE_SELECTION_REQUEST_WAS_CANCELLED,
                    )
                }
                .into();
            });
        }
        visible.extend(self.detector.flush());
        visible
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session::{Command, SendError, Session, SessionState};
    use crate::terminal::Terminal;
    const HEADER: &[u8] = b"**\x18B00000000000000\r\n\x11";

    fn transfer() -> (Transfer, Session, mpsc::Receiver<WriteCommand>) {
        let terminal = Terminal::new(80, 24, 0);
        let shared = Shared::new(&terminal);
        shared.state(SessionState::Connected);
        let (commands, _) = mpsc::channel(8);
        let session = Session {
            commands,
            shared: shared.clone(),
        };
        let (wire, writes) = mpsc::channel(8);
        (Transfer::new(shared, wire), session, writes)
    }

    #[tokio::test]
    async fn zmodem_queued_cancel_outcome_cannot_regress_and_block_input_forever() {
        let (mut transfer, session, _) = transfer();
        transfer.receive(HEADER.to_vec()).await.unwrap();
        transfer.pending.take();
        transfer.update(|s| s.phase = ZmodemPhase::Running);
        transfer.engine = Some(Engine::finalized_fixture(
            transfer.event_sender.clone(),
            false,
        ));
        transfer
            .event_sender
            .send(Event::Finished(Outcome {
                success: false,
                cancelled: true,
                message: "cancelled".into(),
                files: vec![],
            }))
            .await
            .unwrap();
        assert!(
            transfer
                .receive(b"unconfirmed protocol bytes".to_vec())
                .await
                .unwrap()
                .is_empty()
        );
        assert!(!transfer.active());
        let status = session.zmodem_status().unwrap();
        assert_eq!(status.phase, ZmodemPhase::Cancelled);
        assert!(!status.is_active());
    }

    #[tokio::test]
    async fn zmodem_finalized_feed_preserves_older_tail_before_new_output() {
        let (mut transfer, _, _) = transfer();
        transfer.receive(HEADER.to_vec()).await.unwrap();
        transfer.pending.take();
        transfer.engine = Some(Engine::finalized_fixture(
            transfer.event_sender.clone(),
            true,
        ));
        transfer
            .event_sender
            .send(Event::Terminal(b"older*".to_vec()))
            .await
            .unwrap();
        transfer
            .event_sender
            .send(Event::Finished(Outcome {
                success: true,
                cancelled: false,
                message: "done".into(),
                files: vec![],
            }))
            .await
            .unwrap();
        assert_eq!(
            transfer.receive(b"newer".to_vec()).await.unwrap(),
            b"older*newer"
        );
        assert!(!transfer.active());
    }

    #[tokio::test]
    async fn zmodem_shutdown_drains_tail_in_order_without_leaking_next_protocol() {
        let (mut transfer, _, _) = transfer();
        transfer.receive(HEADER.to_vec()).await.unwrap();
        transfer.pending.take();
        transfer.engine = Some(Engine::finalized_fixture(
            transfer.event_sender.clone(),
            true,
        ));
        assert_eq!(
            transfer
                .event(Event::Terminal(b"older*".to_vec()))
                .await
                .unwrap(),
            b"older"
        );
        let mut tail = b"newer".to_vec();
        tail.extend_from_slice(HEADER);
        tail.extend_from_slice(b"quarantined");
        transfer
            .event_sender
            .send(Event::Terminal(tail))
            .await
            .unwrap();
        transfer
            .event_sender
            .send(Event::Finished(Outcome {
                success: true,
                cancelled: false,
                message: "done".into(),
                files: vec![],
            }))
            .await
            .unwrap();
        assert_eq!(transfer.shutdown().await, b"*newer");
        assert!(!transfer.active());
    }

    #[tokio::test]
    async fn zmodem_request_quarantines_protocol_and_rejects_interactive_input() {
        let (mut transfer, session, mut writes) = transfer();
        assert_eq!(
            transfer.receive(b"hello\r\n**".to_vec()).await.unwrap(),
            b"hello\r\n"
        );
        assert!(
            transfer
                .receive(HEADER[2..].to_vec())
                .await
                .unwrap()
                .is_empty()
        );
        let status = session.zmodem_status().unwrap();
        assert_eq!(status.phase, ZmodemPhase::Waiting);
        assert_eq!(
            session.send(Command::Paste("must not enter binary data".into())),
            Err(SendError::TransferBusy)
        );
        transfer.cancel(status.id + 1).await.unwrap();
        assert!(transfer.active());
        transfer.cancel(status.id).await.unwrap();
        assert!(matches!(writes.try_recv(), Ok(WriteCommand::Data(bytes)) if bytes == CANCEL));
        assert_eq!(
            session.zmodem_status().unwrap().phase,
            ZmodemPhase::Cancelled
        );
        assert!(transfer.take_discard_deferred());
        assert_eq!(
            transfer.receive(b"prompt$ ".to_vec()).await.unwrap(),
            b"prompt$ "
        );
    }

    #[tokio::test]
    async fn zmodem_pending_bytes_are_bounded_and_plain_prefix_can_flush() {
        let (mut transfer, session, mut writes) = transfer();
        assert_eq!(
            transfer.receive(b"literal*".to_vec()).await.unwrap(),
            b"literal"
        );
        assert_eq!(transfer.flush_prefix(), b"*");
        transfer.receive(HEADER.to_vec()).await.unwrap();
        transfer.receive(vec![0; MAX_PENDING_BYTES]).await.unwrap();
        assert!(!transfer.active());
        assert_eq!(session.zmodem_status().unwrap().phase, ZmodemPhase::Failed);
        assert!(matches!(writes.try_recv(), Ok(WriteCommand::Data(bytes)) if bytes == CANCEL));
    }

    #[tokio::test]
    async fn zmodem_next_transfer_in_final_tail_waits_for_previous_outcome() {
        let (mut transfer, session, _) = transfer();
        transfer.receive(HEADER.to_vec()).await.unwrap();
        let first = session.zmodem_status().unwrap().id;
        // Model a helper that has consumed the admitted request.
        transfer.pending.take();
        transfer.update(|s| s.phase = ZmodemPhase::Running);
        let mut tail = b"first complete\r\n".to_vec();
        tail.extend_from_slice(HEADER);
        assert_eq!(
            transfer.event(Event::Terminal(tail)).await.unwrap(),
            b"first complete\r\n"
        );
        assert_eq!(session.zmodem_status().unwrap().id, first);
        transfer
            .event(Event::Finished(Outcome {
                success: true,
                cancelled: false,
                message: "done".into(),
                files: vec![],
            }))
            .await
            .unwrap();
        let next = session.zmodem_status().unwrap();
        assert_ne!(next.id, first);
        assert_eq!(next.phase, ZmodemPhase::Waiting);
        assert_eq!(transfer.pending.as_ref().unwrap().initial, HEADER);
    }
}
