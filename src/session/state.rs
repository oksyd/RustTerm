use crate::i18n::messages as i18n_key;

use super::{
    FailureKind, ForwardController, ResourceClient, SessionState, SftpClient, ZmodemStatus,
};
use crate::terminal::{ShellCommand, Snapshot, Terminal};
use std::{
    sync::{
        Arc, Mutex, OnceLock, RwLock,
        atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering},
    },
    time::SystemTime,
};
use tokio::sync::{Notify, oneshot, watch};
use tokio_util::sync::CancellationToken;

const DEFAULT_HISTORY_BYTES: usize = 64 * 1024 * 1024;

#[derive(Default)]
pub(super) struct ShellState {
    pub(super) current_directory: Option<String>,
    pub(super) can_insert_command: bool,
}

fn input_loss_notice() -> &'static str {
    crate::i18n::tr(
        i18n_key::CONNECTIONS_SOME_ACCEPTED_INPUT_WAS_NOT_CONFIRMED_AS_FULLY_DELIVERED_BEFORE,
    )
}

pub(super) struct Shared {
    pub(super) id: uuid::Uuid,
    pub(super) created_at: SystemTime,
    pub(super) ended_at: Mutex<Option<SystemTime>>,
    pub(super) final_alternate: RwLock<Option<Arc<Snapshot>>>,
    pub(super) remote_failure: Mutex<Option<(FailureKind, String)>>,
    pub(super) state: Mutex<SessionState>,
    pub(super) snapshot: watch::Sender<Arc<Snapshot>>,
    pub(super) command_completion: watch::Sender<Option<ShellCommand>>,
    pub(super) command_history: Mutex<Arc<[ShellCommand]>>,
    pub(super) shell_state: Mutex<ShellState>,
    pub(super) revision: watch::Sender<u64>,
    pub(super) output_revision: AtomicU64,
    pub(super) snapshot_count: AtomicU64,
    pub(super) snapshot_pending: AtomicBool,
    pub(super) snapshot_notify: Notify,
    pub(super) actor_stop: CancellationToken,
    pub(super) io_stop: CancellationToken,
    pub(super) trust: Mutex<Option<oneshot::Sender<bool>>>,
    pub(super) sftp: Mutex<Option<SftpClient>>,
    pub(super) resources: Mutex<Option<ResourceClient>>,
    pub(super) forwarding: Mutex<Option<ForwardController>>,
    pub(super) zmodem: Mutex<Option<ZmodemStatus>>,
    pub(super) transfer_id: AtomicU64,
    pub(super) visible: AtomicBool,
    pub(super) requested_budget: AtomicUsize,
    pub(super) allocated_budget: AtomicUsize,
    pub(super) budget_notify: Notify,
    pub(super) read_only: AtomicBool,
    pub(super) local_echo: AtomicBool,
    pub(super) retained_base: AtomicUsize,
    pub(super) evicted: AtomicBool,
    pub(super) unacknowledged_inputs: AtomicUsize,
    pub(super) input_loss_reported: AtomicBool,
    pub(super) input_loss_message: OnceLock<&'static str>,
}
impl Shared {
    pub(super) fn new(terminal: &Terminal) -> Arc<Self> {
        Arc::new(Self {
            id: uuid::Uuid::new_v4(),
            created_at: SystemTime::now(),
            ended_at: Mutex::new(None),
            final_alternate: RwLock::new(None),
            remote_failure: Mutex::new(None),
            state: Mutex::new(SessionState::Connecting),
            snapshot: watch::Sender::new(Arc::new(terminal.snapshot())),
            command_completion: watch::Sender::new(None),
            command_history: Mutex::new(terminal.command_history()),
            shell_state: Mutex::new(ShellState {
                current_directory: terminal.current_directory().map(str::to_owned),
                can_insert_command: terminal.can_insert_command(),
            }),
            revision: watch::Sender::new(1),
            output_revision: AtomicU64::new(0),
            snapshot_count: AtomicU64::new(1),
            snapshot_pending: AtomicBool::new(false),
            snapshot_notify: Notify::new(),
            actor_stop: CancellationToken::new(),
            io_stop: CancellationToken::new(),
            trust: Mutex::new(None),
            sftp: Mutex::new(None),
            resources: Mutex::new(None),
            forwarding: Mutex::new(None),
            zmodem: Mutex::new(None),
            transfer_id: AtomicU64::new(1),
            visible: AtomicBool::new(true),
            requested_budget: AtomicUsize::new(DEFAULT_HISTORY_BYTES),
            allocated_budget: AtomicUsize::new(DEFAULT_HISTORY_BYTES),
            budget_notify: Notify::new(),
            read_only: AtomicBool::new(false),
            local_echo: AtomicBool::new(false),
            retained_base: AtomicUsize::new(0),
            evicted: AtomicBool::new(false),
            unacknowledged_inputs: AtomicUsize::new(0),
            input_loss_reported: AtomicBool::new(false),
            input_loss_message: OnceLock::new(),
        })
    }
    pub(super) fn input_loss_notice(&self) -> &'static str {
        // Keep one identity for this session's warning even if the UI language changes.
        self.input_loss_message.get_or_init(input_loss_notice)
    }
    pub(super) fn input_completed(&self) {
        let _ =
            self.unacknowledged_inputs
                .try_update(Ordering::AcqRel, Ordering::Acquire, |pending| {
                    pending.checked_sub(1)
                });
    }
    pub(super) fn report_input_loss(&self) {
        if self.actor_stop.is_cancelled() || self.input_loss_reported.swap(true, Ordering::AcqRel) {
            return;
        }
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        match &mut *state {
            SessionState::Failed { message, .. } => {
                if !message.contains(self.input_loss_notice()) {
                    message.push_str(crate::i18n::tr(i18n_key::COMMON_DETAIL_SEPARATOR));
                    message.push_str(self.input_loss_notice());
                }
            }
            _ => {
                *state = SessionState::Failed {
                    kind: FailureKind::Write,
                    message: self.input_loss_notice().into(),
                };
            }
        }
        self.changed();
    }
    pub(super) fn changed(&self) {
        self.revision.send_modify(|revision| {
            *revision = revision.wrapping_add(1);
        });
    }
    pub(super) fn output_received(&self, bytes: &[u8]) {
        if !bytes.is_empty() {
            self.output_revision.fetch_add(1, Ordering::Release);
            self.changed();
        }
    }
    pub(super) fn state(&self, value: SessionState) {
        *self.state.lock().unwrap_or_else(|e| e.into_inner()) = value;
        self.changed();
    }
    pub(super) fn closing(&self) {
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        if state.is_running() && *state != SessionState::Closing {
            *state = SessionState::Closing;
            self.changed();
        }
    }
    pub(super) fn live_state(&self, value: SessionState) -> bool {
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        if self.io_stop.is_cancelled() {
            return false;
        }
        *state = value;
        self.changed();
        true
    }
    pub(super) fn failed(&self, kind: FailureKind, message: impl Into<String>) {
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        let mut message = message.into();
        if self.input_loss_reported.load(Ordering::Acquire)
            && !message.contains(self.input_loss_notice())
        {
            message.push_str(crate::i18n::tr(i18n_key::COMMON_DETAIL_SEPARATOR));
            message.push_str(self.input_loss_notice());
        }
        *state = SessionState::Failed { kind, message };
        self.changed();
    }
    pub(super) fn current_state(&self) -> SessionState {
        self.state.lock().unwrap_or_else(|e| e.into_inner()).clone()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn session_changes_coalesce_without_snapshot_publication() {
        let shared = Shared::new(&Terminal::new(80, 24, 0));
        let mut changes = shared.revision.subscribe();
        let snapshots = shared.snapshot.subscribe();
        let initial = *changes.borrow_and_update();

        shared.state(SessionState::Connected);
        shared.changed();
        assert!(changes.has_changed().unwrap());
        assert_eq!(*changes.borrow_and_update(), initial + 2);
        assert!(!changes.has_changed().unwrap());
        assert!(!snapshots.has_changed().unwrap());

        // Reading state after marking the revision cannot consume a concurrent event.
        shared.closing();
        assert_eq!(shared.current_state(), SessionState::Closing);
        assert!(changes.has_changed().unwrap());
    }

    #[test]
    fn subscription_keeps_changes_between_registration_and_first_read() {
        let shared = Shared::new(&Terminal::new(80, 24, 0));
        shared.changed();
        let mut changes = shared.revision.subscribe();
        assert_eq!(*changes.borrow_and_update(), 2);
        shared.failed(FailureKind::RemoteClosed, "closed");
        assert!(matches!(
            shared.current_state(),
            SessionState::Failed { .. }
        ));
        assert!(changes.has_changed().unwrap());
    }

    #[test]
    fn cancelled_session_cannot_be_revived_by_late_handshake_state() {
        let terminal = Terminal::new(80, 24, 10);
        let shared = Shared::new(&terminal);
        shared.io_stop.cancel();
        shared.state(SessionState::Disconnected { exit_code: None });
        assert!(!shared.live_state(SessionState::WaitingHostKey {
            host: "example.test".into(),
            port: 22,
            algorithm: "ssh-ed25519".into(),
            fingerprint: "SHA256:example".into()
        }));
        assert!(!shared.live_state(SessionState::Connecting));
        assert!(!shared.live_state(SessionState::Connected));
        assert_eq!(
            shared.current_state(),
            SessionState::Disconnected { exit_code: None }
        );
    }
}
