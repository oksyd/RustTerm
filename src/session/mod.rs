//! Single-owner terminal actors with bounded local PTY, SSH, and Telnet transports.
//! Hiding only suppresses snapshots; disconnecting freezes a readable archive;
//! closing releases the actor and its retained history.

use crate::session::state::LifecycleEvent;

use crate::i18n::messages as i18n_key;

mod actor;
mod budget;
mod protection;
pub use protection::{SessionProtection, ShellAction};
mod container;
pub(crate) mod exec;
#[cfg(target_os = "linux")]
mod local;
#[cfg(windows)]
#[path = "local/windows.rs"]
mod local;
mod resources;
mod sftp;
mod ssh;
mod state;
pub(crate) mod stream;
mod telnet;
mod transport;
mod zmodem;

pub use crate::terminal::SelectionKind;
pub use local::LocalOptions;
pub use resources::{
    DiskUsage, MemoryUsage, NetworkUsage, ResourceClient, ResourceError, ResourceSnapshot,
};
pub use sftp::{
    SftpClient, SftpConflictPolicy, SftpEntry, SftpError, SftpTextDocument, SftpTransfer,
    SftpTransferProgress, SftpTransferState,
};
pub use ssh::{
    AuthenticationChallenge, AuthenticationPrompt, DiscoveredService, DiscoveryStatus,
    ForwardController, ForwardRegistry, ForwardSettings, ForwardState, ForwardStatus,
    HostKeyChallenge, SshAuth, SshOptions,
};
pub(crate) use ssh::{TunnelConnector, connect_tunnel};
pub use telnet::TelnetOptions;
pub use zmodem::{
    Direction as ZmodemDirection, Progress as ZmodemProgress, Selection as ZmodemSelection,
    ZmodemPhase, ZmodemStatus,
};

use crate::terminal::{MAX_COLUMNS, MAX_ROWS, Snapshot, Terminal};
use anyhow::Result;
use state::Shared;
use std::{
    sync::{
        Arc, OnceLock,
        atomic::{AtomicBool, Ordering},
    },
    time::SystemTime,
};
use tokio::{
    runtime::Runtime,
    sync::{mpsc, oneshot},
};

const COMMAND_CAPACITY: usize = 64;
pub const MAX_INPUT_BYTES: usize = 64 * 1024;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FailureKind {
    Spawn,
    Read,
    Write,
    Resize,
    Process,
    Backpressure,
    Dns,
    Network,
    Timeout,
    Authentication,
    HostKeyMismatch,
    HostKeyRejected,
    KnownHosts,
    PrivateKey,
    RemoteClosed,
    Protocol,
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SessionState {
    Connecting,
    WaitingHostKey {
        host: String,
        port: u16,
        algorithm: String,
        fingerprint: String,
    },
    WaitingAuthentication,
    Connected,
    Closing,
    Disconnected {
        exit_code: Option<u32>,
    },
    Failed {
        kind: FailureKind,
        message: String,
    },
}
impl SessionState {
    pub fn is_running(&self) -> bool {
        matches!(
            self,
            Self::Connecting
                | Self::WaitingHostKey { .. }
                | Self::WaitingAuthentication
                | Self::Connected
                | Self::Closing
        )
    }
}

#[derive(Debug)]
pub enum Command {
    Input(Vec<u8>),
    Key {
        key: String,
        ctrl: bool,
        alt: bool,
        shift: bool,
    },
    Paste(String),
    InsertCommandDraft {
        id: u64,
    },
    Mouse {
        button: u8,
        column: u16,
        row: u16,
        pressed: bool,
        motion: bool,
        shift: bool,
        alt: bool,
        ctrl: bool,
    },
    Resize {
        columns: u16,
        rows: u16,
    },
    Scroll(i32),
    ScrollFractional(f64),
    /// Scroll a mouse-unaware alternate-screen application, if it still owns the terminal.
    AlternateScroll(i32),
    /// Absolute viewport distance from the live terminal bottom, in rows.
    ScrollTo(usize),
    ScrollToBottom,
    NavigateCommand {
        previous: bool,
        failed_only: bool,
    },
    SelectCommandOutput {
        id: Option<u64>,
    },
    RevealCommand {
        id: u64,
    },
    ToggleCommandFold {
        id: u64,
    },
    RevealCommandBlock {
        id: u64,
    },
    SelectCommandBlock {
        id: u64,
    },
    BeginSelection {
        column: u16,
        row: u16,
        kind: SelectionKind,
    },
    BeginSelectionAt {
        position: crate::terminal::SelectionPoint,
        kind: SelectionKind,
        extend: bool,
    },
    UpdateSelectionAt(crate::terminal::SelectionPoint),
    UpdateSelection {
        column: u16,
        row: u16,
    },
    ClearSelection,
    SelectAll,
    Search {
        query: String,
        case_sensitive: bool,
    },
    SearchScoped {
        query: String,
        case_sensitive: bool,
        scope: crate::terminal::SearchScope,
    },
    CancelSearch,
    NextMatch {
        backwards: bool,
    },
    SetHighlight(bool),
    SetSelectionHighlight(bool),
    SetWordDelimiters(String),
    SetCommandOutputCache(bool),
    SetTheme(crate::config::Theme),
    SetHistoryBudget {
        lines: usize,
        bytes: usize,
    },
    Focus(bool),
    TrustHostKey(bool),
}
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum SendError {
    #[error("{}", crate::i18n::tr(i18n_key::CONNECTIONS_PRODUCTION_BLOCKED))]
    Protected,
    #[error("{}", crate::i18n::tr(i18n_key::TERMINAL_PRODUCTION_PASTE_REVIEW))]
    PasteReviewRequired,
    #[error("{}", crate::i18n::tr(i18n_key::TERMINAL_INVALID_SHELL_ACTION))]
    InvalidShellAction,
    #[error(
        "{}",
        crate::i18n::tr(i18n_key::CONNECTIONS_THE_SESSION_INPUT_QUEUE_IS_FULL_PLEASE_RETRY_LATER)
    )]
    Busy,
    #[error("{}", crate::i18n::tr(i18n_key::CONNECTIONS_THE_SESSION_IS_CLOSED))]
    Closed,
    #[error(
        "{}",
        crate::i18n::tr(i18n_key::CONNECTIONS_THE_SESSION_IS_NOT_CONNECTED)
    )]
    NotConnected,
    #[error(
        "{}",
        crate::i18n::tr(i18n_key::CONNECTIONS_A_SINGLE_INPUT_OR_QUERY_EXCEEDS_THE_64_KIB_LIMIT)
    )]
    TooLarge,
    #[error(
        "{}",
        crate::i18n::tr(i18n_key::CONNECTIONS_TERMINAL_SIZE_MUST_BE_2_TO_500_COLUMNS_AND_1)
    )]
    InvalidSize,
    #[error("{}", crate::i18n::tr(i18n_key::SETTINGS_WORD_DELIMITERS_INVALID))]
    InvalidWordDelimiters,
    #[error(
        "{}",
        crate::i18n::tr(i18n_key::CONNECTIONS_NO_HOST_KEY_IS_WAITING_FOR_CONFIRMATION)
    )]
    NoHostKeyPrompt,
    #[error(
        "{}",
        crate::i18n::tr(
            i18n_key::CONNECTIONS_FILE_TRANSFER_IS_USING_THE_TERMINAL_CHANNEL_FINISH_OR_CANCEL
        )
    )]
    TransferBusy,
    #[error(
        "{}",
        crate::i18n::tr(
            i18n_key::CONNECTIONS_THE_FILE_TRANSFER_REQUEST_HAS_ENDED_OR_WAS_ALREADY_HANDLED
        )
    )]
    NoTransfer,
}

/// Completion of one admitted resize in the terminal actor.
///
/// This confirms the core grid has been resized, not that a new snapshot has
/// been published or that the PTY/SSH peer has received the size change.
#[derive(Clone, Debug)]
pub struct ResizeReceipt(Arc<AtomicBool>);

impl ResizeReceipt {
    /// Remains false if the actor closes before processing this resize.
    pub fn is_complete(&self) -> bool {
        self.0.load(Ordering::Acquire)
    }
}

enum ActorCommand {
    User(Command),
    CopySelection {
        search_query: bool,
        reply: oneshot::Sender<Option<String>>,
    },
    CopyCommandBlock {
        id: u64,
        part: crate::terminal::BlockCopy,
        reply: oneshot::Sender<Option<String>>,
    },
    Resize {
        columns: u16,
        rows: u16,
        receipt: ResizeReceipt,
    },
    ZmodemStart {
        id: u64,
        selection: ZmodemSelection,
    },
    ZmodemCancel {
        id: u64,
    },
}

pub struct Session {
    commands: mpsc::Sender<ActorCommand>,
    shared: Arc<Shared>,
    protection: SessionProtection,
}
impl Session {
    /// An ephemeral container shell; it shares SSH authentication, not terminal input.
    pub fn spawn_container(
        client: crate::docker::DockerClient,
        id: String,
        shell: crate::docker::Shell,
        columns: u16,
        rows: u16,
        history: usize,
    ) -> Result<Self> {
        let (mut handle, terminal, commands) = Self::create(columns, rows, history)?;
        handle.protection = client.protection();
        runtime()?.spawn(container::run(
            client,
            id,
            shell,
            terminal,
            commands,
            handle.shared.clone(),
            columns,
            rows,
        ));
        Ok(handle)
    }
    fn create(
        columns: u16,
        rows: u16,
        history: usize,
    ) -> Result<(Self, Terminal, mpsc::Receiver<ActorCommand>)> {
        validate_size(columns, rows)?;
        let terminal = Terminal::new(columns, rows, history);
        let shared = Shared::new(&terminal);
        budget::register(&shared);
        let (commands, receiver) = mpsc::channel(COMMAND_CAPACITY);
        Ok((
            Self {
                commands,
                shared,
                protection: SessionProtection::Standard,
            },
            terminal,
            receiver,
        ))
    }
    pub fn spawn_local(
        options: LocalOptions,
        columns: u16,
        rows: u16,
        history: usize,
    ) -> Result<Self> {
        let runtime = runtime()?;
        let (handle, terminal, commands) = Self::create(columns, rows, history)?;
        runtime.spawn(local::run(
            options,
            terminal,
            columns,
            rows,
            commands,
            handle.shared.clone(),
        ));
        Ok(handle)
    }
    pub fn spawn_ssh(options: SshOptions, columns: u16, rows: u16, history: usize) -> Result<Self> {
        anyhow::ensure!(
            !options.host.trim().is_empty() && options.port > 0 && !options.username.is_empty(),
            crate::i18n::tr(i18n_key::CONNECTIONS_SSH_HOST_PORT_AND_USERNAME_ARE_REQUIRED)
        );
        let runtime = runtime()?;
        let (mut handle, terminal, commands) = Self::create(columns, rows, history)?;
        handle.protection = options.production_protection.into();
        runtime.spawn(ssh::run(
            options,
            terminal,
            columns,
            rows,
            commands,
            handle.shared.clone(),
        ));
        Ok(handle)
    }
    pub fn spawn_telnet(
        options: TelnetOptions,
        columns: u16,
        rows: u16,
        history: usize,
    ) -> Result<Self> {
        Self::spawn_telnet_with_ssh(options, None, columns, rows, history)
    }
    pub fn spawn_telnet_with_ssh(
        mut options: TelnetOptions,
        ssh: Option<SshOptions>,
        columns: u16,
        rows: u16,
        history: usize,
    ) -> Result<Self> {
        options.host = crate::config::known_hosts::normalize_host(&options.host)?;
        anyhow::ensure!(
            options.port > 0,
            crate::i18n::tr(i18n_key::CONNECTIONS_TELNET_PORT_IS_REQUIRED)
        );
        let runtime = runtime()?;
        let (handle, terminal, commands) = Self::create(columns, rows, history)?;
        handle.shared.local_echo.store(true, Ordering::Release);
        runtime.spawn(telnet::run(
            options,
            ssh,
            terminal,
            columns,
            rows,
            commands,
            handle.shared.clone(),
        ));
        Ok(handle)
    }
    /// Nonblocking admission. Errors mean that the command was not accepted.
    pub fn send(&self, command: Command) -> std::result::Result<(), SendError> {
        self.send_checked(command, false)
    }

    pub fn protection(&self) -> SessionProtection {
        self.protection
    }

    pub fn send_shell_action(&self, action: ShellAction) -> std::result::Result<(), SendError> {
        if !self.protection.allows_enhancements() {
            return Err(SendError::Protected);
        }
        let command = action.command().ok_or(SendError::InvalidShellAction)?;
        self.send(Command::Input(command.into_bytes()))
    }

    /// Call only after a preview has been approved for this exact session.
    /// The UI must discard approval when its target session changes.
    pub fn paste_confirmed(&self, text: String) -> std::result::Result<(), SendError> {
        self.send_checked(Command::Paste(text), true)
    }

    fn send_checked(
        &self,
        command: Command,
        paste_confirmed: bool,
    ) -> std::result::Result<(), SendError> {
        if self.shared.actor_stop.is_cancelled() {
            return Err(SendError::Closed);
        }
        if let Command::Resize { columns, rows } = command {
            return self.resize(columns, rows).map(|_| ());
        }
        if let Command::TrustHostKey(accept) = command {
            return self
                .shared
                .trust
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .take()
                .ok_or(SendError::NoHostKeyPrompt)?
                .send(accept)
                .map_err(|_| SendError::NoHostKeyPrompt);
        }
        let input = matches!(
            command,
            Command::Input(_)
                | Command::Key { .. }
                | Command::Paste(_)
                | Command::InsertCommandDraft { .. }
                | Command::AlternateScroll(_)
                | Command::Mouse { .. }
                | Command::Focus(_)
        );
        if input && (self.shared.io_stop.is_cancelled() || self.state() != SessionState::Connected)
        {
            return Err(SendError::NotConnected);
        }
        if input && self.zmodem_status().is_some_and(|s| s.is_active()) {
            return Err(SendError::TransferBusy);
        }
        match &command {
            Command::SetWordDelimiters(text) if !crate::terminal::valid_word_delimiters(text) => {
                return Err(SendError::InvalidWordDelimiters);
            }
            Command::Input(bytes) if bytes.len() > MAX_INPUT_BYTES => {
                return Err(SendError::TooLarge);
            }
            Command::Paste(text)
            | Command::Key { key: text, .. }
            | Command::Search { query: text, .. }
            | Command::SearchScoped { query: text, .. }
                if text.len() > MAX_INPUT_BYTES =>
            {
                return Err(SendError::TooLarge);
            }
            _ => {}
        }
        if let Command::Paste(text) = &command
            && !paste_confirmed
            && self.protection.requires_paste_review(text)
        {
            return Err(SendError::PasteReviewRequired);
        }
        let tracked = is_user_input(&command);
        if tracked {
            self.shared
                .unacknowledged_inputs
                .fetch_add(1, Ordering::AcqRel);
        }
        let result = self
            .commands
            .try_send(ActorCommand::User(command))
            .map_err(map_send_error);
        if tracked && result.is_err() {
            self.shared.input_completed();
        }
        result
    }

    /// Nonblocking resize admission in the same FIFO as input and view commands.
    /// Errors mean the resize was not accepted; the receipt identifies only this
    /// invocation, including when another resize requests identical dimensions.
    pub fn resize(&self, columns: u16, rows: u16) -> std::result::Result<ResizeReceipt, SendError> {
        if self.shared.actor_stop.is_cancelled() {
            return Err(SendError::Closed);
        }
        validate_size(columns, rows)?;
        let receipt = ResizeReceipt(Arc::new(AtomicBool::new(false)));
        self.commands
            .try_send(ActorCommand::Resize {
                columns,
                rows,
                receipt: receipt.clone(),
            })
            .map_err(map_send_error)?;
        Ok(receipt)
    }

    /// Request a view of the actor's current grid without waiting for transport
    /// capacity. This samples state; use command receipts for FIFO barriers.
    pub fn request_snapshot(&self) -> bool {
        if self.shared.actor_stop.is_cancelled() {
            return false;
        }
        if self.shared.snapshot_pending.swap(true, Ordering::AcqRel) {
            return false;
        }
        self.shared.snapshot_notify.notify_one();
        true
    }
    pub fn snapshot(&self) -> Arc<Snapshot> {
        self.shared.snapshot.borrow().clone()
    }
    /// Observe completed snapshots without polling. Publications coalesce to the
    /// newest frame and do not change the terminal's data revision.
    pub fn subscribe_snapshots(&self) -> tokio::sync::watch::Receiver<Arc<Snapshot>> {
        self.shared.snapshot.subscribe()
    }
    /// Observe the latest completed command, including while snapshots are hidden.
    /// The initial value is retained state, not a new completion event.
    pub fn subscribe_command_completions(
        &self,
    ) -> tokio::sync::watch::Receiver<Option<crate::terminal::ShellCommand>> {
        self.shared.command_completion.subscribe()
    }

    pub fn command_history(&self) -> Arc<[crate::terminal::ShellCommand]> {
        self.shared
            .command_history
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .clone()
    }
    /// Latest directory reported by the shell, including while the view is hidden.
    pub fn current_directory(&self) -> Option<String> {
        self.shared
            .shell_state
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .current_directory
            .clone()
    }
    /// Whether the live shell accepts a draft without executing it.
    pub fn can_insert_command(&self) -> bool {
        !self.shared.io_stop.is_cancelled()
            && !self.is_read_only()
            && self.state() == SessionState::Connected
            && self
                .shared
                .shell_state
                .lock()
                .unwrap_or_else(|error| error.into_inner())
                .can_insert_command
    }
    pub fn id(&self) -> uuid::Uuid {
        self.shared.id
    }
    pub fn created_at(&self) -> SystemTime {
        self.shared.created_at
    }
    pub fn ended_at(&self) -> Option<SystemTime> {
        *self
            .shared
            .ended_at
            .lock()
            .unwrap_or_else(|e| e.into_inner())
    }
    pub fn final_alt_snapshot(&self) -> Option<Arc<Snapshot>> {
        self.shared
            .final_alternate
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }
    pub fn state(&self) -> SessionState {
        self.shared.current_state()
    }
    /// File operations share this session's authenticated SSH connection, but use
    /// independent channels so browsing and transfers do not consume shell bytes.
    pub fn forward_statuses(&self) -> Vec<ForwardStatus> {
        self.shared
            .forwarding
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .as_ref()
            .map(|forwarding| forwarding.statuses())
            .unwrap_or_default()
    }
    pub fn forwarding(&self) -> Option<ForwardController> {
        self.shared
            .forwarding
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }
    pub fn set_forward_enabled(
        &self,
        index: usize,
        enabled: bool,
    ) -> std::result::Result<(), SendError> {
        if self.shared.io_stop.is_cancelled() {
            return Err(SendError::NotConnected);
        }
        self.shared
            .forwarding
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .as_ref()
            .ok_or(SendError::NotConnected)?
            .set_enabled(index, enabled)
    }
    pub fn sftp(&self) -> Option<SftpClient> {
        if self.shared.io_stop.is_cancelled() || self.state() != SessionState::Connected {
            return None;
        }
        self.shared
            .sftp
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }
    pub fn resources(&self) -> Option<ResourceClient> {
        if !self.protection.allows_enhancements()
            || self.shared.io_stop.is_cancelled()
            || self.state() != SessionState::Connected
        {
            return None;
        }
        self.shared
            .resources
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .clone()
    }
    pub fn docker(&self) -> Option<crate::docker::DockerClient> {
        if self.shared.io_stop.is_cancelled() || self.state() != SessionState::Connected {
            return None;
        }
        self.shared
            .docker
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .clone()
    }
    pub fn zmodem_status(&self) -> Option<ZmodemStatus> {
        self.shared
            .zmodem
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }
    pub fn accept_zmodem(
        &self,
        id: u64,
        selection: ZmodemSelection,
    ) -> std::result::Result<(), SendError> {
        if self.shared.io_stop.is_cancelled() {
            return Err(SendError::Closed);
        }
        if !self
            .zmodem_status()
            .is_some_and(|s| s.id == id && s.phase == ZmodemPhase::Waiting)
        {
            return Err(SendError::NoTransfer);
        }
        self.commands
            .try_send(ActorCommand::ZmodemStart { id, selection })
            .map_err(map_send_error)
    }
    pub fn cancel_zmodem(&self, id: u64) -> std::result::Result<(), SendError> {
        if !self
            .zmodem_status()
            .is_some_and(|s| s.id == id && s.is_active())
        {
            return Err(SendError::NoTransfer);
        }
        self.commands
            .try_send(ActorCommand::ZmodemCancel { id })
            .map_err(map_send_error)
    }
    pub fn dismiss_zmodem(&self, id: u64) {
        let mut status = self.shared.zmodem.lock().unwrap_or_else(|e| e.into_inner());
        if status
            .as_ref()
            .is_some_and(|s| s.id == id && !s.is_active())
        {
            *status = None;
            self.shared.changed();
        }
    }
    pub fn revision(&self) -> u64 {
        *self.shared.revision.borrow()
    }
    /// Received terminal output batches, excluding focus, resize and other UI changes.
    pub fn output_revision(&self) -> u64 {
        self.shared.output_revision.load(Ordering::Acquire)
    }
    /// Observe output, connection and transfer changes without requesting snapshots.
    /// Subscribe before reading session state so a concurrent change remains pending.
    pub fn subscribe_changes(&self) -> tokio::sync::watch::Receiver<u64> {
        self.shared.revision.subscribe()
    }
    pub fn snapshot_count(&self) -> u64 {
        self.shared.snapshot_count.load(Ordering::Acquire)
    }
    /// Copy the selection in command order. Dropping the receipt discards the result.
    pub fn copy_selection(
        &self,
    ) -> std::result::Result<oneshot::Receiver<Option<String>>, SendError> {
        self.selection_text(false)
    }

    pub fn selection_search_query(
        &self,
    ) -> std::result::Result<oneshot::Receiver<Option<String>>, SendError> {
        self.selection_text(true)
    }

    fn selection_text(
        &self,
        search_query: bool,
    ) -> std::result::Result<oneshot::Receiver<Option<String>>, SendError> {
        if self.shared.actor_stop.is_cancelled() {
            return Err(SendError::Closed);
        }
        let (sender, receiver) = oneshot::channel();
        self.commands
            .try_send(ActorCommand::CopySelection {
                search_query,
                reply: sender,
            })
            .map_err(map_send_error)?;
        Ok(receiver)
    }
    pub fn copy_command_block(
        &self,
        id: u64,
        part: crate::terminal::BlockCopy,
    ) -> std::result::Result<oneshot::Receiver<Option<String>>, SendError> {
        if self.shared.actor_stop.is_cancelled() {
            return Err(SendError::Closed);
        }
        let (reply, receiver) = oneshot::channel();
        self.commands
            .try_send(ActorCommand::CopyCommandBlock { id, part, reply })
            .map_err(map_send_error)?;
        Ok(receiver)
    }
    pub fn is_read_only(&self) -> bool {
        self.shared.read_only.load(Ordering::Acquire)
    }
    pub fn is_evicted(&self) -> bool {
        self.shared.evicted.load(Ordering::Acquire)
    }
    pub fn set_visible(&self, visible: bool) {
        if self.shared.visible.swap(visible, Ordering::AcqRel) != visible {
            budget::rebalance();
        }
    }
    /// Set the global history capacity target; upstream cache memory is additional.
    pub fn set_global_history_budget(bytes: usize) {
        budget::set_global(bytes);
    }
    /// Assigned history capacity target, not a measurement of allocated memory.
    pub fn allocated_history_budget(&self) -> usize {
        self.shared.allocated_budget.load(Ordering::Acquire)
    }
    /// Disconnect transport and retain a frozen, searchable, selectable terminal.
    pub fn disconnect(&self) {
        if !self.shared.io_stop.is_cancelled() {
            // Record intent before waking workers; late handshake events cannot
            // revive the connection, and accepted input loss remains reportable.
            self.shared.transition(LifecycleEvent::Closing);
            self.shared.io_stop.cancel();
            self.shared
                .trust
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .take();
        }
    }
    /// Release both transport and archive. This operation never joins on the UI.
    pub fn close(&self) {
        // Release is part of the lifecycle, so late worker failures cannot
        // overwrite an intentional close even before cancellation is observed.
        self.shared.transition(LifecycleEvent::Released);
        self.shared.actor_stop.cancel();
        self.disconnect();
        budget::rebalance();
    }
}
impl Drop for Session {
    fn drop(&mut self) {
        self.close();
    }
}
fn is_user_input(command: &Command) -> bool {
    match command {
        Command::Input(bytes) => !bytes.is_empty(),
        Command::Key { key, .. } => !key.is_empty(),
        Command::Paste(text) => !text.is_empty(),
        Command::InsertCommandDraft { .. } => true,
        Command::AlternateScroll(lines) => *lines != 0,
        _ => false,
    }
}
fn validate_size(columns: u16, rows: u16) -> std::result::Result<(), SendError> {
    if !(2..=MAX_COLUMNS).contains(&columns) || !(1..=MAX_ROWS).contains(&rows) {
        Err(SendError::InvalidSize)
    } else {
        Ok(())
    }
}
fn map_send_error<T>(error: mpsc::error::TrySendError<T>) -> SendError {
    match error {
        mpsc::error::TrySendError::Full(_) => SendError::Busy,
        mpsc::error::TrySendError::Closed(_) => SendError::Closed,
    }
}
fn runtime() -> Result<&'static Runtime> {
    static RUNTIME: OnceLock<std::result::Result<Runtime, String>> = OnceLock::new();
    RUNTIME
        .get_or_init(|| {
            tokio::runtime::Builder::new_multi_thread()
                .worker_threads(2)
                .thread_name("rustterm-session")
                .enable_all()
                .build()
                .map_err(|e| e.to_string())
        })
        .as_ref()
        .map_err(|e| anyhow::anyhow!(e.clone()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn closing_does_not_report_input_loss_but_disconnect_keeps_the_warning() {
        use std::{
            future::Future,
            pin::pin,
            task::{Context, Wake, Waker},
        };
        struct InputLossOnCancel(Arc<Shared>);
        impl Wake for InputLossOnCancel {
            fn wake(self: Arc<Self>) {
                // Run the worker's notification synchronously when cancellation
                // wakes it, exercising the close race without scheduling or sleeps.
                self.0.report_input_loss();
            }
        }
        for close in [true, false] {
            let shared = Shared::new(&Terminal::new(80, 24, 10));
            shared.transition(LifecycleEvent::Connected);
            let (commands, _receiver) = mpsc::channel(1);
            let session = Session {
                protection: Default::default(),
                commands,
                shared: shared.clone(),
            };
            session.send(Command::Input(b"pending".to_vec())).unwrap();
            let waker = Waker::from(Arc::new(InputLossOnCancel(shared.clone())));
            let mut cancellation = pin!(shared.io_stop.cancelled());
            assert!(
                cancellation
                    .as_mut()
                    .poll(&mut Context::from_waker(&waker))
                    .is_pending()
            );
            if close {
                session.close();
                assert_eq!(session.state(), SessionState::Closing);
                assert!(!shared.input_loss_reported());
            } else {
                session.disconnect();
                assert!(matches!(
                    session.state(),
                    SessionState::Failed {
                        kind: FailureKind::Write,
                        ..
                    }
                ));
                assert!(shared.input_loss_reported());
            }
        }
    }

    #[test]
    fn rejected_input_is_not_counted_as_an_accepted_submission() {
        let terminal = Terminal::new(80, 24, 10);
        let shared = Shared::new(&terminal);
        shared.transition(LifecycleEvent::Connected);
        let (commands, _receiver) = mpsc::channel(1);
        let session = Session {
            protection: Default::default(),
            commands,
            shared: shared.clone(),
        };
        session.send(Command::Input(b"first".to_vec())).unwrap();
        assert_eq!(
            session.send(Command::Input(b"second".to_vec())),
            Err(SendError::Busy)
        );
        assert_eq!(shared.unacknowledged_inputs.load(Ordering::Acquire), 1);
        shared.io_stop.cancel();
        assert_eq!(
            session.send(Command::Input(b"third".to_vec())),
            Err(SendError::NotConnected)
        );
        assert_eq!(shared.unacknowledged_inputs.load(Ordering::Acquire), 1);
    }
}
