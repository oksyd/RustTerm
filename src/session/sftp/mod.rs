//! Bounded SFTP work on independent channels of an authenticated SSH connection.
//!
//! Public futures may be polled by any executor. All protocol and filesystem I/O
//! runs on the session runtime. No password or private key is retained here.

use crate::i18n::messages as i18n_key;

mod workflows;
pub use workflows::{SftpConflictPolicy, SftpTextDocument};

use russh::{Channel, ChannelMsg, client};
use russh_sftp::{
    client::{Config, RawSftpSession, error::Error as ProtocolError},
    protocol::{FileAttributes, OpenFlags, StatusCode},
};
use std::{
    future::Future,
    io,
    path::{Path, PathBuf},
    pin::Pin,
    sync::{Arc, Mutex},
    task::{Context, Poll},
    time::Duration,
};
use tokio::{
    io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt, ReadBuf},
    sync::{Semaphore, mpsc, oneshot},
};
use tokio_util::sync::CancellationToken;

const CHUNK: usize = 32 * 1024;
const REQUEST_TIMEOUT: Duration = Duration::from_secs(15);
const OPERATION_TIMEOUT: Duration = Duration::from_secs(45);
const CLEANUP_TIMEOUT: Duration = Duration::from_secs(3);
const MAX_ENTRIES: usize = 10_000;
const MAX_LIST_BYTES: usize = 4 * 1024 * 1024;
const MAX_PATH_BYTES: usize = 16 * 1024;

#[derive(Debug, thiserror::Error)]
pub enum SftpError {
    #[error(
        "{}",
        crate::i18n::tr(i18n_key::CONNECTIONS_THE_SSH_CONNECTION_IS_DISCONNECTED)
    )]
    Disconnected,
    #[error("{}", crate::i18n::tr(i18n_key::CONNECTIONS_SFTP_OPERATION_CANCELLED))]
    Cancelled,
    #[error("{}", crate::i18n::tr(i18n_key::CONNECTIONS_SFTP_OPERATION_TIMED_OUT))]
    Timeout,
    #[error(
        "{}",
        crate::i18n::tr(i18n_key::CONNECTIONS_SFTP_IS_BUSY_PLEASE_RETRY_LATER)
    )]
    Busy,
    #[error("{}", crate::trf!(i18n_key::CONNECTIONS_INVALID_PATH_ERROR, error = .0))]
    InvalidPath(String),
    #[error("{}", crate::trf!(i18n_key::CONNECTIONS_SFTP_ERROR, error = .0))]
    Remote(String),
    #[error("{}", crate::trf!(i18n_key::CONNECTIONS_LOCAL_FILE_ERROR, error = .0.to_string()))]
    Local(#[from] io::Error),
}
impl From<ProtocolError> for SftpError {
    fn from(error: ProtocolError) -> Self {
        match error {
            ProtocolError::Timeout => Self::Timeout,
            other => Self::Remote(other.to_string()),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SftpEntry {
    pub name: String,
    pub path: String,
    pub is_dir: bool,
    pub is_symlink: bool,
    pub size: u64,
    pub modified: Option<u64>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SftpTransferState {
    Queued,
    Running,
    Completed,
    Cancelled,
    Failed(String),
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SftpTransferProgress {
    pub bytes_done: u64,
    pub total_bytes: Option<u64>,
    pub state: SftpTransferState,
}
struct TransferState {
    progress: Mutex<SftpTransferProgress>,
    cancel: CancellationToken,
}
/// Dropping a clone never cancels a transfer. Cancellation is explicit.
#[derive(Clone)]
pub struct SftpTransfer(Arc<TransferState>, Option<Arc<workflows::TransferPlan>>);
impl SftpTransfer {
    pub fn progress(&self) -> SftpTransferProgress {
        self.0
            .progress
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }
    pub fn cancel(&self) {
        self.0.cancel.cancel();
    }
    pub fn retry(&self, client: &SftpClient) -> Result<Self, SftpError> {
        if matches!(
            self.progress().state,
            SftpTransferState::Queued | SftpTransferState::Running
        ) {
            return Err(SftpError::Busy);
        }
        let plan = self.1.clone().ok_or(SftpError::Busy)?;
        client.start_transfer(PathBuf::new(), String::new(), false, Some(plan))
    }
}

pub(super) struct OpenRequest {
    pub reply: oneshot::Sender<Result<OpenedChannel, SftpError>>,
    pub cancel: CancellationToken,
}

pub(super) struct OpenedChannel(Option<Channel<client::Msg>>);
impl OpenedChannel {
    pub(super) fn new(channel: Channel<client::Msg>) -> Self {
        Self(Some(channel))
    }

    fn into_stream(mut self) -> russh::ChannelStream<client::Msg> {
        self.0.take().expect("opened SSH channel").into_stream()
    }
}
impl Drop for OpenedChannel {
    fn drop(&mut self) {
        // A reply can be dropped after a successful send but before reception.
        // Channel itself has no close-on-drop guard; ChannelStream does.
        if let Some(channel) = self.0.take() {
            drop(channel.into_stream());
        }
    }
}

struct ClientInner {
    open: mpsc::Sender<OpenRequest>,
    stop: CancellationToken,
    operations: Arc<Semaphore>,
    transfers: Arc<Semaphore>,
}
#[derive(Clone)]
pub struct SftpClient(Arc<ClientInner>);

enum Request {
    List(String),
    Canonicalize(String),
    IsDirectory(String),
    Mkdir(String),
    Rename(String, String),
    Remove(String, bool),
    ReadText(String),
    SaveText(SftpTextDocument, String),
}
enum Response {
    Entries(Vec<SftpEntry>),
    Path(String),
    IsDirectory(bool),
    Done,
    Text(SftpTextDocument),
}

impl SftpClient {
    pub fn is_connected(&self) -> bool {
        !self.0.stop.is_cancelled() && !self.0.open.is_closed()
    }

    pub async fn closed(&self) {
        tokio::select! {
            _ = self.0.stop.cancelled() => {},
            _ = self.0.open.closed() => {},
        }
    }

    pub async fn read_text(&self, path: &str) -> Result<SftpTextDocument, SftpError> {
        validate_path(path)?;
        match self.request(Request::ReadText(path.into())).await? {
            Response::Text(document) => Ok(document),
            _ => unreachable!(),
        }
    }
    pub async fn save_text(
        &self,
        original: SftpTextDocument,
        content: String,
    ) -> Result<SftpTextDocument, SftpError> {
        validate_target(&original.path)?;
        match self.request(Request::SaveText(original, content)).await? {
            Response::Text(document) => Ok(document),
            _ => unreachable!(),
        }
    }
    pub fn upload_tree(
        &self,
        local: PathBuf,
        remote: String,
        policy: SftpConflictPolicy,
    ) -> Result<SftpTransfer, SftpError> {
        validate_target(&remote)?;
        let plan = Arc::new(workflows::TransferPlan::new(
            local, remote, true, policy, &self.0,
        ));
        self.start_transfer(PathBuf::new(), String::new(), false, Some(plan))
    }
    pub fn download_tree(
        &self,
        remote: String,
        local: PathBuf,
        policy: SftpConflictPolicy,
    ) -> Result<SftpTransfer, SftpError> {
        validate_path(&remote)?;
        validate_local_target(&local)?;
        let plan = Arc::new(workflows::TransferPlan::new(
            local, remote, false, policy, &self.0,
        ));
        self.start_transfer(PathBuf::new(), String::new(), false, Some(plan))
    }
    pub(super) fn new(stop: CancellationToken) -> (Self, mpsc::Receiver<OpenRequest>) {
        let (open, receive) = mpsc::channel(4);
        (
            Self(Arc::new(ClientInner {
                open,
                stop,
                operations: Arc::new(Semaphore::new(4)),
                transfers: Arc::new(Semaphore::new(2)),
            })),
            receive,
        )
    }

    pub async fn list_dir(&self, path: &str) -> Result<Vec<SftpEntry>, SftpError> {
        validate_path(path)?;
        match self.request(Request::List(path.into())).await? {
            Response::Entries(entries) => Ok(entries),
            _ => unreachable!("list request returns entries"),
        }
    }
    pub async fn canonicalize(&self, path: &str) -> Result<String, SftpError> {
        validate_path(path)?;
        match self.request(Request::Canonicalize(path.into())).await? {
            Response::Path(path) => Ok(path),
            _ => unreachable!("realpath request returns a path"),
        }
    }
    /// Follows symbolic links and reports whether the target is a directory.
    pub async fn is_dir(&self, path: &str) -> Result<bool, SftpError> {
        validate_path(path)?;
        match self.request(Request::IsDirectory(path.into())).await? {
            Response::IsDirectory(is_dir) => Ok(is_dir),
            _ => unreachable!("stat request returns the target type"),
        }
    }
    pub async fn mkdir(&self, path: &str) -> Result<(), SftpError> {
        validate_target(path)?;
        self.request(Request::Mkdir(path.into())).await.map(|_| ())
    }
    /// SFTP v3 RENAME refuses an existing destination. Do not replace this with
    /// the OpenSSH POSIX rename extension, which deliberately overwrites files.
    pub async fn rename(&self, old: &str, new: &str) -> Result<(), SftpError> {
        validate_target(old)?;
        validate_target(new)?;
        self.request(Request::Rename(old.into(), new.into()))
            .await
            .map(|_| ())
    }
    /// Removes one file/symlink or one empty directory, never recursively.
    pub async fn remove(&self, path: &str, is_dir: bool) -> Result<(), SftpError> {
        validate_target(path)?;
        self.request(Request::Remove(path.into(), is_dir))
            .await
            .map(|_| ())
    }

    async fn request(&self, request: Request) -> Result<Response, SftpError> {
        let permit = self
            .0
            .operations
            .clone()
            .try_acquire_owned()
            .map_err(|_| SftpError::Busy)?;
        if self.0.stop.is_cancelled() {
            return Err(SftpError::Disconnected);
        }
        let cancel = self.0.stop.child_token();
        // A dropped UI request cancels its worker, which still awaits cleanup.
        let _cancel_on_drop = CancelOnDrop(cancel.clone());
        let inner = self.0.clone();
        let (send, receive) = oneshot::channel();
        super::runtime()
            .map_err(|e| SftpError::Remote(e.to_string()))?
            .spawn(async move {
                let _permit = permit;
                let result = async {
                    let mut connection = Connection::open(&inner, &cancel).await?;
                    let save = matches!(&request, Request::SaveText(..));
                    let operation = execute_request(&connection.raw, request, &inner.stop, &cancel);
                    let result = if save {
                        // Saves own their cancellation boundaries so a pending
                        // create/commit can finish before cleanup or shutdown.
                        operation.await
                    } else {
                        bounded(&cancel, OPERATION_TIMEOUT, operation).await
                    };
                    connection.shutdown().await;
                    result
                }
                .await;
                let _ = send.send(result);
            });
        receive.await.map_err(|_| SftpError::Disconnected)?
    }

    pub fn upload(&self, local: PathBuf, remote: String) -> Result<SftpTransfer, SftpError> {
        validate_target(&remote)?;
        self.transfer(local, remote, true)
    }
    pub fn download(&self, remote: String, local: PathBuf) -> Result<SftpTransfer, SftpError> {
        validate_path(&remote)?;
        validate_local_target(&local)?;
        self.transfer(local, remote, false)
    }
    fn transfer(
        &self,
        local: PathBuf,
        remote: String,
        upload: bool,
    ) -> Result<SftpTransfer, SftpError> {
        self.start_transfer(local, remote, upload, None)
    }
    fn start_transfer(
        &self,
        local: PathBuf,
        remote: String,
        upload: bool,
        plan: Option<Arc<workflows::TransferPlan>>,
    ) -> Result<SftpTransfer, SftpError> {
        if self.0.stop.is_cancelled() {
            return Err(SftpError::Disconnected);
        }
        let permit = self
            .0
            .transfers
            .clone()
            .try_acquire_owned()
            .map_err(|_| SftpError::Busy)?;
        let operation = self
            .0
            .operations
            .clone()
            .try_acquire_owned()
            .map_err(|_| SftpError::Busy)?;
        let state = Arc::new(TransferState {
            progress: Mutex::new(SftpTransferProgress {
                bytes_done: 0,
                total_bytes: None,
                state: SftpTransferState::Queued,
            }),
            cancel: self.0.stop.child_token(),
        });
        let transfer = SftpTransfer(state.clone(), plan.clone());
        let inner = self.0.clone();
        super::runtime()
            .map_err(|e| SftpError::Remote(e.to_string()))?
            .spawn(async move {
                let (_permit, _operation) = (permit, operation);
                set_progress(&state, None, None, Some(SftpTransferState::Running));
                let result = async {
                    let mut connection = Connection::open(&inner, &state.cancel).await?;
                    let result = if let Some(plan) = &plan {
                        plan.run(&connection.raw, &inner.stop, &state).await
                    } else if upload {
                        upload_file(&connection.raw, &inner.stop, &state, local, &remote).await
                    } else {
                        download_file(&connection.raw, &state, &remote, local).await
                    };
                    connection.shutdown().await;
                    result
                }
                .await;
                let outcome = match result {
                    Ok(()) => SftpTransferState::Completed,
                    Err(SftpError::Cancelled) => SftpTransferState::Cancelled,
                    Err(error) => SftpTransferState::Failed(error.to_string()),
                };
                set_progress(&state, None, None, Some(outcome));
            });
        Ok(transfer)
    }
}

fn set_progress(
    state: &TransferState,
    bytes: Option<u64>,
    total: Option<u64>,
    status: Option<SftpTransferState>,
) {
    let mut progress = state.progress.lock().unwrap_or_else(|e| e.into_inner());
    if let Some(bytes) = bytes {
        progress.bytes_done = bytes;
        if progress.total_bytes.is_some_and(|total| bytes > total) {
            progress.total_bytes = None;
        }
    }
    if let Some(total) = total {
        progress.total_bytes = Some(total);
    }
    if let Some(status) = status {
        progress.state = status;
    }
}

async fn bounded<T, E: Into<SftpError>>(
    cancel: &CancellationToken,
    timeout: Duration,
    future: impl Future<Output = Result<T, E>>,
) -> Result<T, SftpError> {
    tokio::select! {
        biased;
        _ = cancel.cancelled() => Err(SftpError::Cancelled),
        result = tokio::time::timeout(timeout, future) => result.map_err(|_| SftpError::Timeout)?.map_err(Into::into),
    }
}

struct CancelOnDrop(CancellationToken);
impl Drop for CancelOnDrop {
    fn drop(&mut self) {
        self.0.cancel();
    }
}

struct Connection {
    raw: RawSftpSession,
    stop: CancellationToken,
    ended: Option<oneshot::Receiver<()>>,
}
impl Connection {
    async fn open(inner: &ClientInner, cancel: &CancellationToken) -> Result<Self, SftpError> {
        let (reply, receive) = oneshot::channel();
        inner
            .open
            .try_send(OpenRequest {
                reply,
                cancel: cancel.clone(),
            })
            .map_err(|error| match error {
                mpsc::error::TrySendError::Full(_) => SftpError::Busy,
                mpsc::error::TrySendError::Closed(_) => SftpError::Disconnected,
            })?;
        let mut channel = bounded(cancel, REQUEST_TIMEOUT, async {
            receive.await.map_err(|_| SftpError::Disconnected)?
        })
        .await?;
        bounded(cancel, REQUEST_TIMEOUT, async {
            let channel = channel.0.as_mut().expect("opened SSH channel");
            channel
                .request_subsystem(true, "sftp")
                .await
                .map_err(|e| SftpError::Remote(e.to_string()))?;
            loop {
                match channel.wait().await {
                    Some(ChannelMsg::Success) => return Ok::<_, SftpError>(()),
                    Some(ChannelMsg::WindowAdjusted { .. }) => {}
                    _ => {
                        return Err(SftpError::Remote(
                            crate::i18n::tr(
                                i18n_key::CONNECTIONS_THE_SERVER_REFUSED_THE_SFTP_SUBSYSTEM,
                            )
                            .into(),
                        ));
                    }
                }
            }
        })
        .await?;
        let stop = inner.stop.child_token();
        let (ended, receive_ended) = oneshot::channel();
        let stream = TrackedStream {
            stream: channel.into_stream(),
            cancelled: Box::pin(stop.clone().cancelled_owned()),
            ended: Some(ended),
        };
        let raw = RawSftpSession::new_with_config(
            stream,
            Config {
                max_packet_len: 256 * 1024,
                max_concurrent_reads: 1,
                max_concurrent_writes: 1,
                max_write_packet_len: CHUNK as u32,
                request_timeout_secs: REQUEST_TIMEOUT.as_secs(),
            },
        );
        let mut connection = Self {
            raw,
            stop,
            ended: Some(receive_ended),
        };
        match bounded(cancel, REQUEST_TIMEOUT, connection.raw.init()).await {
            Ok(version) if version.version == 3 => Ok(connection),
            result => {
                connection.shutdown().await;
                Err(result.err().unwrap_or_else(|| {
                    SftpError::Remote(
                        crate::i18n::tr(i18n_key::CONNECTIONS_THE_SERVER_DID_NOT_NEGOTIATE_SFTP_V3)
                            .into(),
                    )
                }))
            }
        }
    }
    async fn shutdown(&mut self) {
        let _ = self.raw.close_session();
        // Wake both library tasks even if the remote channel stops consuming.
        self.stop.cancel();
        if let Some(ended) = self.ended.take() {
            let _ = tokio::time::timeout(CLEANUP_TIMEOUT, ended).await;
        }
    }
}
impl Drop for Connection {
    fn drop(&mut self) {
        self.stop.cancel();
    }
}

async fn list_directory(raw: &RawSftpSession, path: &str) -> Result<Vec<SftpEntry>, SftpError> {
    let handle = raw.opendir(path).await?.handle;
    let result = async {
        let mut entries = Vec::new();
        let mut bytes = 0usize;
        loop {
            match raw.readdir(handle.clone()).await {
                Ok(page) => {
                    if page.files.is_empty() {
                        return Err(SftpError::Remote(crate::i18n::tr(i18n_key::CONNECTIONS_THE_SERVER_RETURNED_AN_EMPTY_DIRECTORY_PAGE_WITHOUT_ENDING_THE).into()));
                    }
                    for entry in page.files {
                        if entry.filename == "." || entry.filename == ".." {
                            continue;
                        }
                        let entry_path = join_remote(path, &entry.filename)?;
                        bytes = bytes.saturating_add(entry.filename.len() + entry_path.len());
                        if entries.len() == MAX_ENTRIES || bytes > MAX_LIST_BYTES {
                            return Err(SftpError::Remote(
                                crate::i18n::tr(i18n_key::CONNECTIONS_THE_DIRECTORY_EXCEEDS_10_000_ENTRIES_OR_4_MIB_OF).into(),
                            ));
                        }
                        entries.push(SftpEntry {
                            name: entry.filename,
                            path: entry_path,
                            is_dir: entry.attrs.is_dir(),
                            is_symlink: entry.attrs.is_symlink(),
                            size: entry.attrs.size.unwrap_or(0),
                            modified: entry.attrs.mtime.map(u64::from),
                        });
                    }
                }
                Err(ProtocolError::Status(status)) if status.status_code == StatusCode::Eof => {
                    break;
                }
                Err(error) => return Err(error.into()),
            }
        }
        Ok(entries)
    }
    .await;
    // Release the handle on validation and READDIR errors too. A failed CLOSE
    // must not hide the original reason that the listing could not be read.
    let closed = raw.close(handle).await;
    let mut entries = result?;
    closed?;
    entries.sort_by(|a, b| b.is_dir.cmp(&a.is_dir).then_with(|| a.name.cmp(&b.name)));
    Ok(entries)
}

async fn execute_request(
    raw: &RawSftpSession,
    request: Request,
    stop: &CancellationToken,
    cancel: &CancellationToken,
) -> Result<Response, SftpError> {
    match request {
        Request::ReadText(path) => workflows::read_text(raw, path).await.map(Response::Text),
        Request::SaveText(original, content) => {
            workflows::save_text(raw, stop, cancel, original, content)
                .await
                .map(Response::Text)
        }
        Request::Canonicalize(path) => {
            let response = raw.realpath(path).await?;
            let path = response
                .files
                .into_iter()
                .next()
                .ok_or_else(|| {
                    SftpError::Remote(
                        crate::i18n::tr(i18n_key::CONNECTIONS_THE_SERVER_RETURNED_AN_EMPTY_PATH)
                            .into(),
                    )
                })?
                .filename;
            validate_path(&path)?;
            Ok(Response::Path(path))
        }
        Request::List(path) => list_directory(raw, &path).await.map(Response::Entries),
        Request::IsDirectory(path) => {
            Ok(Response::IsDirectory(raw.stat(path).await?.attrs.is_dir()))
        }
        Request::Mkdir(path) => {
            raw.mkdir(path, FileAttributes::empty()).await?;
            Ok(Response::Done)
        }
        Request::Rename(old, new) => {
            ensure_absent(raw, &new).await?;
            raw.rename(old, new).await?;
            Ok(Response::Done)
        }
        Request::Remove(path, is_dir) => {
            // Always unlink a symlink, including a link to a directory.
            let metadata = raw.lstat(path.clone()).await?.attrs;
            if is_dir && !metadata.is_symlink() {
                raw.rmdir(path).await?;
            } else {
                raw.remove(path).await?;
            }
            Ok(Response::Done)
        }
    }
}

async fn ensure_absent(raw: &RawSftpSession, path: &str) -> Result<(), SftpError> {
    match raw.lstat(path).await {
        Err(ProtocolError::Status(status)) if status.status_code == StatusCode::NoSuchFile => {
            Ok(())
        }
        Err(error) => Err(error.into()),
        Ok(_) => Err(SftpError::Remote(crate::trf!(
            i18n_key::CONNECTIONS_THE_DESTINATION_ALREADY_EXISTS_AND_WILL_NOT_BE_OVERWRITTEN_PATH,
            path = path
        ))),
    }
}

async fn upload_file(
    raw: &RawSftpSession,
    stop: &CancellationToken,
    state: &TransferState,
    local: PathBuf,
    remote: &str,
) -> Result<(), SftpError> {
    let (file, total) = bounded(&state.cancel, REQUEST_TIMEOUT, async {
        tokio::task::spawn_blocking(move || {
            let file = open_regular_file(&local)?;
            let total = file.metadata()?.len();
            Ok::<_, io::Error>((file, total))
        })
        .await
        .map_err(|e| SftpError::Remote(e.to_string()))?
        .map_err(SftpError::from)
    })
    .await?;
    let mut local = tokio::fs::File::from_std(file);
    set_progress(state, None, Some(total), None);
    bounded(&state.cancel, REQUEST_TIMEOUT, ensure_absent(raw, remote)).await?;
    let temporary = remote_temporary(remote)?;
    let mut created = false;
    let mut handle = None;
    if state.cancel.is_cancelled() {
        return Err(SftpError::Cancelled);
    }
    let result: Result<(), SftpError> = async {
        // Creation is allowed to finish before cancellation cleanup: otherwise a
        // late OPEN response could create a file after REMOVE has completed.
        let opened = bounded(
            stop,
            REQUEST_TIMEOUT,
            raw.open(
                temporary.clone(),
                OpenFlags::WRITE | OpenFlags::CREATE | OpenFlags::EXCLUDE,
                FileAttributes {
                    permissions: Some(0o600),
                    ..FileAttributes::empty()
                },
            ),
        )
        .await?;
        created = true;
        handle = Some(opened.handle.clone());
        let mut offset = 0u64;
        let mut buffer = vec![0; CHUNK];
        // Metadata size is a progress hint. Virtual files may report zero and
        // ordinary sources may change; only the reader's EOF ends the copy.
        loop {
            let count = bounded(&state.cancel, REQUEST_TIMEOUT, local.read(&mut buffer)).await?;
            if count == 0 {
                break;
            }
            bounded(
                &state.cancel,
                REQUEST_TIMEOUT,
                raw.write(opened.handle.clone(), offset, buffer[..count].to_vec()),
            )
            .await?;
            offset = offset.checked_add(count as u64).ok_or_else(|| {
                SftpError::Remote(
                    crate::i18n::tr(i18n_key::CONNECTIONS_THE_FILE_IS_TOO_LARGE).into(),
                )
            })?;
            set_progress(state, Some(offset), None, None);
        }
        set_progress(state, Some(offset), Some(offset), None);
        bounded(stop, REQUEST_TIMEOUT, raw.close(opened.handle)).await?;
        handle = None;
        bounded(&state.cancel, REQUEST_TIMEOUT, ensure_absent(raw, remote)).await?;
        // Once the commit request is issued, wait for its acknowledgement even
        // if the user cancels. Never claim cancellation after a successful rename.
        bounded(stop, REQUEST_TIMEOUT, raw.rename(temporary.clone(), remote)).await?;
        Ok(())
    }
    .await;
    if let Err(error) = result {
        if let Some(handle) = handle {
            let _ = bounded(stop, CLEANUP_TIMEOUT, raw.close(handle)).await;
        }
        if created {
            match bounded(stop, CLEANUP_TIMEOUT, raw.remove(temporary.clone())).await {
                Ok(_) => {}
                Err(cleanup) => {
                    return Err(SftpError::Remote(crate::trf!(
                        i18n_key::CONNECTIONS_ERROR_TEMPORARY_FILE_CLEANUP_WAS_NOT_CONFIRMED_TEMPORARY_CLEANUP_CHECK,
                        error = error.to_string(),
                        temporary = temporary,
                        cleanup = cleanup.to_string(),
                        remote = remote
                    )));
                }
            }
        } else {
            return Err(SftpError::Remote(crate::trf!(
                i18n_key::CONNECTIONS_ERROR_TEMPORARY_FILE_CREATION_WAS_NOT_CONFIRMED_CHECK_TEMPORARY,
                error = error.to_string(),
                temporary = temporary
            )));
        }
        return Err(error);
    }
    Ok(())
}

async fn download_file(
    raw: &RawSftpSession,
    state: &TransferState,
    remote: &str,
    local: PathBuf,
) -> Result<(), SftpError> {
    let opened = bounded(
        &state.cancel,
        REQUEST_TIMEOUT,
        raw.open(remote, OpenFlags::READ, FileAttributes::empty()),
    )
    .await?
    .handle;
    let attrs = bounded(&state.cancel, REQUEST_TIMEOUT, raw.fstat(opened.clone()))
        .await?
        .attrs;
    if attrs.is_dir() {
        return Err(SftpError::Remote(
            crate::i18n::tr(i18n_key::CONNECTIONS_A_DIRECTORY_CANNOT_BE_DOWNLOADED_AS_A_FILE)
                .into(),
        ));
    }
    if attrs
        .permissions
        .is_some_and(|mode| mode & 0o170000 != 0 && mode & 0o170000 != 0o100000)
    {
        return Err(SftpError::Remote(
            crate::i18n::tr(i18n_key::SFTP_ONLY_REGULAR_FILES_CAN_BE_DOWNLOADED).into(),
        ));
    }
    set_progress(state, None, attrs.size, None);
    let target = local.clone();
    let temporary = bounded(&state.cancel, REQUEST_TIMEOUT, async {
        tokio::task::spawn_blocking(move || create_download_temp(&target))
            .await
            .map_err(|e| SftpError::Remote(e.to_string()))?
            .map_err(SftpError::from)
    })
    .await?;
    let mut output = tokio::fs::File::from_std(temporary.as_file().try_clone()?);
    let result = receive_file(raw, state, &opened, &mut output).await;
    if let Err(error) = result {
        // A cancelled tokio file write can still be finishing in its blocking
        // worker. Drain it before removing the temporary file, including Windows.
        let _ = tokio::time::timeout(CLEANUP_TIMEOUT, output.flush()).await;
        drop(output);
        let path = temporary.path().to_owned();
        if let Err(cleanup) = temporary.close() {
            return Err(SftpError::Remote(crate::trf!(
                i18n_key::CONNECTIONS_ERROR_LOCAL_TEMPORARY_FILE_CLEANUP_FAILED_PATH_CLEANUP,
                error = error.to_string(),
                path = path.display().to_string(),
                cleanup = cleanup.to_string()
            )));
        }
        return Err(error);
    }
    drop(output);
    if state.cancel.is_cancelled() {
        let path = temporary.path().to_owned();
        return match temporary.close() {
            Ok(()) => Err(SftpError::Cancelled),
            Err(error) => Err(SftpError::Remote(crate::trf!(
                i18n_key::CONNECTIONS_OPERATION_CANCELLED_LOCAL_TEMPORARY_FILE_CLEANUP_FAILED_PATH_ERROR,
                path = path.display().to_string(),
                error = error.to_string()
            ))),
        };
    }
    // Commit is not cancelled mid-flight: persist_noclobber may already have
    // published the file. Its result is the authoritative completion result.
    tokio::task::spawn_blocking(move || match temporary.persist_noclobber(local) {
        Ok(_) => Ok(()),
        Err(error) => {
            let original = error.error;
            let temporary = error.file;
            let path = temporary.path().to_owned();
            temporary.close().map_err(|cleanup| {
                SftpError::Remote(crate::trf!(
                    i18n_key::CONNECTIONS_ERROR_LOCAL_TEMPORARY_FILE_CLEANUP_FAILED_PATH_CLEANUP,
                    error = original.to_string(),
                    path = path.display().to_string(),
                    cleanup = cleanup.to_string()
                ))
            })?;
            Err(SftpError::Local(original))
        }
    })
    .await
    .map_err(|e| SftpError::Remote(e.to_string()))??;
    Ok(())
}

async fn receive_file(
    raw: &RawSftpSession,
    state: &TransferState,
    opened: &str,
    output: &mut tokio::fs::File,
) -> Result<(), SftpError> {
    let mut offset = 0u64;
    loop {
        let requested = CHUNK as u32;
        let read = bounded(&state.cancel, REQUEST_TIMEOUT, async {
            match raw.read(opened, offset, requested).await {
                Ok(data) => Ok(Some(data.data)),
                Err(ProtocolError::Status(status)) if status.status_code == StatusCode::Eof => {
                    Ok(None)
                }
                Err(error) => Err(SftpError::from(error)),
            }
        })
        .await?;
        let Some(bytes) = read else {
            break;
        };
        if bytes.is_empty() || bytes.len() > requested as usize {
            return Err(SftpError::Remote(
                crate::i18n::tr(
                    i18n_key::CONNECTIONS_THE_SERVER_RETURNED_AN_INVALID_FILE_DATA_LENGTH,
                )
                .into(),
            ));
        }
        bounded(&state.cancel, REQUEST_TIMEOUT, output.write_all(&bytes)).await?;
        offset = offset.checked_add(bytes.len() as u64).ok_or_else(|| {
            SftpError::Remote(crate::i18n::tr(i18n_key::CONNECTIONS_THE_FILE_IS_TOO_LARGE).into())
        })?;
        set_progress(state, Some(offset), None, None);
    }
    set_progress(state, Some(offset), Some(offset), None);
    bounded(&state.cancel, REQUEST_TIMEOUT, raw.close(opened)).await?;
    bounded(&state.cancel, REQUEST_TIMEOUT, output.flush()).await?;
    bounded(&state.cancel, REQUEST_TIMEOUT, output.sync_all()).await?;
    Ok(())
}

fn open_regular_file(path: &Path) -> io::Result<std::fs::File> {
    let mut options = std::fs::OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(libc::O_NONBLOCK | libc::O_NOFOLLOW);
    }
    let file = options.open(path)?;
    if !file.metadata()?.is_file() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            crate::i18n::tr(i18n_key::CONNECTIONS_ONLY_REGULAR_FILES_CAN_BE_UPLOADED),
        ));
    }
    Ok(file)
}
fn create_download_temp(target: &Path) -> io::Result<tempfile::NamedTempFile> {
    // symlink_metadata also detects dangling links. Persist performs the final
    // atomic no-overwrite check, so a target created during transfer is safe.
    match std::fs::symlink_metadata(target) {
        Ok(_) => {
            return Err(io::Error::new(
                io::ErrorKind::AlreadyExists,
                crate::i18n::tr(
                    i18n_key::CONNECTIONS_THE_DOWNLOAD_DESTINATION_ALREADY_EXISTS_AND_WILL_NOT_BE_OVERWRITTEN,
                ),
            ));
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(error) => return Err(error),
    }
    tempfile::Builder::new()
        .prefix(".rustterm-download-")
        .suffix(".part")
        .tempfile_in(
            target
                .parent()
                .filter(|p| !p.as_os_str().is_empty())
                .unwrap_or_else(|| Path::new(".")),
        )
}
fn validate_path(path: &str) -> Result<(), SftpError> {
    if path.is_empty() || path.len() > MAX_PATH_BYTES || path.contains('\0') {
        return Err(SftpError::InvalidPath(
            crate::i18n::tr(i18n_key::CONNECTIONS_THE_PATH_IS_EMPTY_CONTAINS_NUL_OR_EXCEEDS_16_KIB)
                .into(),
        ));
    }
    Ok(())
}
fn validate_name(name: &str) -> Result<(), SftpError> {
    validate_path(name)?;
    if name == "." || name == ".." || name.contains('/') {
        return Err(SftpError::InvalidPath(
            crate::i18n::tr(
                i18n_key::CONNECTIONS_A_DIRECTORY_ENTRY_CONTAINS_A_PATH_SEPARATOR_OR_A_SPECIAL,
            )
            .into(),
        ));
    }
    Ok(())
}
fn validate_target(path: &str) -> Result<(), SftpError> {
    validate_path(path)?;
    validate_name(path.rsplit('/').next().unwrap_or_default())
}
fn validate_local_target(path: &Path) -> Result<(), SftpError> {
    if path.file_name().is_none() {
        return Err(SftpError::InvalidPath(
            crate::i18n::tr(i18n_key::CONNECTIONS_CHOOSE_A_COMPLETE_LOCAL_FILENAME).into(),
        ));
    }
    Ok(())
}
fn join_remote(directory: &str, name: &str) -> Result<String, SftpError> {
    validate_name(name)?;
    let joined = format!("{}/{name}", directory.trim_end_matches('/'));
    validate_path(&joined)?;
    Ok(joined)
}
fn remote_temporary(target: &str) -> Result<String, SftpError> {
    validate_target(target)?;
    let directory = target
        .rsplit_once('/')
        .map(|(directory, _)| directory)
        .unwrap_or(".");
    join_remote(
        directory,
        &format!(".rustterm-upload-{}.part", uuid::Uuid::new_v4()),
    )
}

/// The library owns two internal tasks. Cancellation wakes blocked reads/writes;
/// the completion receiver observes both halves releasing the underlying stream.
struct TrackedStream<S> {
    stream: S,
    cancelled: Pin<Box<dyn Future<Output = ()> + Send>>,
    ended: Option<oneshot::Sender<()>>,
}
impl<S> Drop for TrackedStream<S> {
    fn drop(&mut self) {
        if let Some(ended) = self.ended.take() {
            let _ = ended.send(());
        }
    }
}
impl<S: AsyncRead + Unpin> AsyncRead for TrackedStream<S> {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        if self.cancelled.as_mut().poll(cx).is_ready() {
            return Poll::Ready(Err(io::ErrorKind::ConnectionAborted.into()));
        }
        Pin::new(&mut self.stream).poll_read(cx, buf)
    }
}
impl<S: AsyncWrite + Unpin> AsyncWrite for TrackedStream<S> {
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        if self.cancelled.as_mut().poll(cx).is_ready() {
            return Poll::Ready(Err(io::ErrorKind::ConnectionAborted.into()));
        }
        Pin::new(&mut self.stream).poll_write(cx, buf)
    }
    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        if self.cancelled.as_mut().poll(cx).is_ready() {
            return Poll::Ready(Err(io::ErrorKind::ConnectionAborted.into()));
        }
        Pin::new(&mut self.stream).poll_flush(cx)
    }
    fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        if self.cancelled.as_mut().poll(cx).is_ready() {
            return Poll::Ready(Err(io::ErrorKind::ConnectionAborted.into()));
        }
        Pin::new(&mut self.stream).poll_shutdown(cx)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn execute_request(
        raw: &RawSftpSession,
        request: Request,
    ) -> Result<Response, SftpError> {
        super::execute_request(
            raw,
            request,
            &CancellationToken::new(),
            &CancellationToken::new(),
        )
        .await
    }

    #[test]
    fn remote_names_follow_posix_path_rules() {
        for name in ["", ".", "..", "../secret", "/root", "a/b", "\0"] {
            assert!(join_remote("/home/test", name).is_err(), "{name:?}");
        }
        for name in ["backup:2026", "a\\b", "C:report", "..\\report"] {
            assert_eq!(
                join_remote("/home/test", name).unwrap(),
                format!("/home/test/{name}")
            );
            assert!(validate_target(&format!("/home/test/{name}")).is_ok());
        }
        assert_eq!(join_remote("/", "文件.txt").unwrap(), "/文件.txt");
        assert_eq!(join_remote("relative", "file").unwrap(), "relative/file");
    }

    #[tokio::test]
    async fn directory_handles_close_after_listing_errors_without_hiding_the_cause() {
        use russh_sftp::protocol::{File, Handle, Name, Status};

        struct ListingServer(Arc<Mutex<Vec<String>>>);
        impl russh_sftp::server::Handler for ListingServer {
            type Error = StatusCode;

            fn unimplemented(&self) -> Self::Error {
                StatusCode::OpUnsupported
            }

            async fn opendir(&mut self, id: u32, path: String) -> Result<Handle, Self::Error> {
                Ok(Handle { id, handle: path })
            }

            async fn readdir(&mut self, id: u32, path: String) -> Result<Name, Self::Error> {
                let files = match path.as_str() {
                    "/read-failure" => return Err(StatusCode::PermissionDenied),
                    "/empty-page" => Vec::new(),
                    "/invalid-name" => vec![File::dummy("../outside")],
                    _ => return Err(StatusCode::Eof),
                };
                Ok(Name { id, files })
            }

            async fn close(&mut self, id: u32, handle: String) -> Result<Status, Self::Error> {
                let fail = matches!(handle.as_str(), "/read-failure" | "/close-failure");
                self.0.lock().unwrap().push(handle);
                if fail {
                    Err(StatusCode::Failure)
                } else {
                    Ok(ok_status(id))
                }
            }
        }

        let closed = Arc::new(Mutex::new(Vec::new()));
        let (client_stream, server_stream) = tokio::io::duplex(4096);
        let server = tokio::spawn(russh_sftp::server::run(
            server_stream,
            ListingServer(closed.clone()),
        ));
        let raw = RawSftpSession::new(client_stream);
        tokio::time::timeout(Duration::from_secs(2), async {
            raw.init().await.unwrap();
            assert!(list_directory(&raw, "/empty-directory").await.unwrap().is_empty());
            let error = list_directory(&raw, "/read-failure").await.unwrap_err();
            assert!(matches!(error, SftpError::Remote(message) if message.starts_with(&StatusCode::PermissionDenied.to_string())));
            assert!(list_directory(&raw, "/empty-page").await.is_err());
            assert!(matches!(list_directory(&raw, "/invalid-name").await, Err(SftpError::InvalidPath(_))));
            let error = list_directory(&raw, "/close-failure").await.unwrap_err();
            assert!(matches!(error, SftpError::Remote(message) if message.starts_with(&StatusCode::Failure.to_string())));
            assert_eq!(
                *closed.lock().unwrap(),
                ["/empty-directory", "/read-failure", "/empty-page", "/invalid-name", "/close-failure"]
            );
            raw.close_session().unwrap();
            server.await.unwrap();
        })
        .await
        .unwrap();
    }

    #[tokio::test]
    async fn listing_accepts_colons_and_backslashes_in_remote_names() {
        let remote = Arc::new(MemoryRemote::default());
        for name in ["normal", "backup:2026", "a\\b"] {
            remote
                .directories
                .lock()
                .unwrap()
                .insert(format!("/home/test/{name}"));
        }
        let (client_stream, server_stream) = tokio::io::duplex(4096);
        let server = tokio::spawn(russh_sftp::server::run(
            server_stream,
            TestSftp {
                remote,
                listed: false,
            },
        ));
        let client = RawSftpSession::new(client_stream);
        tokio::time::timeout(Duration::from_secs(2), async {
            client.init().await.unwrap();
            let Response::Entries(entries) =
                execute_request(&client, Request::List("/home/test".into()))
                    .await
                    .unwrap()
            else {
                panic!("expected a directory listing");
            };
            assert_eq!(
                entries
                    .iter()
                    .map(|entry| entry.name.as_str())
                    .collect::<Vec<_>>(),
                ["a\\b", "backup:2026", "normal"]
            );
            for entry in entries {
                assert_eq!(entry.path, format!("/home/test/{}", entry.name));
                assert!(entry.is_dir);
            }
            client.close_session().unwrap();
            server.await.unwrap();
        })
        .await
        .unwrap();
    }

    #[tokio::test]
    async fn symlink_targets_are_resolved_on_demand_without_changing_link_operations() {
        use std::sync::atomic::Ordering;

        let remote = Arc::new(MemoryRemote::default());
        remote
            .directories
            .lock()
            .unwrap()
            .insert("/home/test/target-dir".into());
        remote
            .files
            .lock()
            .unwrap()
            .insert("/home/test/target-file".into(), b"original".to_vec());
        for (name, target) in [
            ("dir-link", "target-dir"),
            ("file-link", "target-file"),
            ("dangling", "missing"),
        ] {
            remote
                .links
                .lock()
                .unwrap()
                .insert(format!("/home/test/{name}"), format!("/home/test/{target}"));
        }
        let (client_stream, server_stream) = tokio::io::duplex(4096);
        let server = tokio::spawn(russh_sftp::server::run(
            server_stream,
            TestSftp {
                remote: remote.clone(),
                listed: false,
            },
        ));
        let client = RawSftpSession::new(client_stream);
        tokio::time::timeout(Duration::from_secs(2), async {
            client.init().await.unwrap();
            let Response::Entries(entries) =
                execute_request(&client, Request::List("/home/test".into()))
                    .await
                    .unwrap()
            else {
                panic!("expected a directory listing");
            };
            for name in ["dir-link", "file-link", "dangling"] {
                let entry = entries.iter().find(|entry| entry.name == name).unwrap();
                assert!(entry.is_symlink);
                assert!(!entry.is_dir);
                assert_eq!(entry.path, format!("/home/test/{name}"));
            }
            assert_eq!(remote.stats.load(Ordering::Relaxed), 0);
            assert!(matches!(
                execute_request(&client, Request::IsDirectory("/home/test/dir-link".into()))
                    .await
                    .unwrap(),
                Response::IsDirectory(true)
            ));
            assert!(matches!(
                execute_request(&client, Request::IsDirectory("/home/test/file-link".into()))
                    .await
                    .unwrap(),
                Response::IsDirectory(false)
            ));
            assert!(matches!(
                execute_request(&client, Request::IsDirectory("/home/test/dangling".into())).await,
                Err(SftpError::Remote(_))
            ));
            assert_eq!(remote.stats.load(Ordering::Relaxed), 3);
            execute_request(
                &client,
                Request::Rename(
                    "/home/test/dir-link".into(),
                    "/home/test/renamed-link".into(),
                ),
            )
            .await
            .unwrap();
            assert_eq!(
                remote.links.lock().unwrap().get("/home/test/renamed-link"),
                Some(&"/home/test/target-dir".to_owned())
            );
            execute_request(
                &client,
                Request::Remove("/home/test/renamed-link".into(), true),
            )
            .await
            .unwrap();
            execute_request(
                &client,
                Request::Remove("/home/test/dangling".into(), false),
            )
            .await
            .unwrap();
            assert!(
                remote
                    .directories
                    .lock()
                    .unwrap()
                    .contains("/home/test/target-dir")
            );
            assert_eq!(
                remote.files.lock().unwrap().get("/home/test/target-file"),
                Some(&b"original".to_vec())
            );
            assert!(
                !remote
                    .links
                    .lock()
                    .unwrap()
                    .contains_key("/home/test/renamed-link")
            );
            assert!(
                !remote
                    .links
                    .lock()
                    .unwrap()
                    .contains_key("/home/test/dangling")
            );
            client.close_session().unwrap();
            server.await.unwrap();
        })
        .await
        .unwrap();
    }
    #[test]
    fn download_commit_never_overwrites_and_cleans_temporary() {
        let directory = tempfile::tempdir().unwrap();
        let target = directory.path().join("result");
        let temporary = create_download_temp(&target).unwrap();
        let temporary_path = temporary.path().to_owned();
        std::fs::write(&target, b"existing").unwrap();
        let error = temporary.persist_noclobber(&target).unwrap_err();
        drop(error);
        assert_eq!(std::fs::read(&target).unwrap(), b"existing");
        assert!(!temporary_path.exists());
        assert!(create_download_temp(&target).is_err());
    }

    fn running_transfer() -> TransferState {
        TransferState {
            progress: Mutex::new(SftpTransferProgress {
                bytes_done: 0,
                total_bytes: None,
                state: SftpTransferState::Running,
            }),
            cancel: CancellationToken::new(),
        }
    }

    #[tokio::test]
    async fn downloads_read_to_eof_despite_zero_or_stale_size_hints() {
        let remote = Arc::new(MemoryRemote::default());
        let (client_stream, server_stream) = tokio::io::duplex(4096);
        let server = tokio::spawn(russh_sftp::server::run(
            server_stream,
            TestSftp {
                remote: remote.clone(),
                listed: false,
            },
        ));
        let client = RawSftpSession::new(client_stream);
        let directory = tempfile::tempdir().unwrap();
        tokio::time::timeout(Duration::from_secs(5), async {
            client.init().await.unwrap();
            for (index, (reported, length)) in [
                (0, CHUNK + 7),
                (3, CHUNK + 7),
                (CHUNK as u64 * 2, 7),
                (0, 0),
            ]
            .into_iter()
            .enumerate()
            {
                let path = format!("/home/test/source-{index}");
                let bytes: Vec<_> = (0..length).map(|offset| (offset % 251) as u8).collect();
                remote
                    .files
                    .lock()
                    .unwrap()
                    .insert(path.clone(), bytes.clone());
                remote
                    .reported_sizes
                    .lock()
                    .unwrap()
                    .insert(path.clone(), reported);
                let target = directory.path().join(format!("download-{index}"));
                let state = running_transfer();
                download_file(&client, &state, &path, target.clone())
                    .await
                    .unwrap();
                assert_eq!(std::fs::read(target).unwrap(), bytes);
                let progress = state.progress.lock().unwrap();
                assert_eq!(progress.bytes_done, length as u64);
                assert_eq!(progress.total_bytes, Some(length as u64));
            }
            client.close_session().unwrap();
            server.await.unwrap();
        })
        .await
        .unwrap();
    }

    #[tokio::test]
    async fn uploads_read_to_eof_when_sources_grow_after_metadata_capture() {
        use std::sync::atomic::Ordering;

        let remote = Arc::new(MemoryRemote::default());
        let (client_stream, server_stream) = tokio::io::duplex(4096);
        let server = tokio::spawn(russh_sftp::server::run(
            server_stream,
            TestSftp {
                remote: remote.clone(),
                listed: false,
            },
        ));
        let client = Arc::new(RawSftpSession::new(client_stream));
        let directory = tempfile::tempdir().unwrap();
        tokio::time::timeout(Duration::from_secs(5), async {
            client.init().await.unwrap();
            for length in [0, CHUNK + 3] {
                let source = directory.path().join(format!("source-{length}"));
                let mut bytes = vec![b'a'; length];
                std::fs::write(&source, &bytes).unwrap();
                if length == 0 {
                    remote.pause_create.store(true, Ordering::Release);
                } else {
                    remote.pause_write.store(true, Ordering::Release);
                }
                let state = Arc::new(running_transfer());
                let upload_client = client.clone();
                let upload_state = state.clone();
                let upload_source = source.clone();
                let target = format!("/home/test/upload-{length}");
                let upload_target = target.clone();
                let task = tokio::spawn(async move {
                    upload_file(
                        &upload_client,
                        &CancellationToken::new(),
                        &upload_state,
                        upload_source,
                        &upload_target,
                    )
                    .await
                });
                if length == 0 {
                    remote.create_started.notified().await;
                } else {
                    remote.write_started.notified().await;
                }
                bytes.extend_from_slice(b"grown after the size hint");
                std::fs::write(&source, &bytes).unwrap();
                if length == 0 {
                    remote.resume_create.notify_one();
                } else {
                    remote.resume_write.notify_one();
                }
                task.await.unwrap().unwrap();
                assert_eq!(remote.files.lock().unwrap().get(&target), Some(&bytes));
                let progress = state.progress.lock().unwrap();
                assert_eq!(progress.bytes_done, bytes.len() as u64);
                assert_eq!(progress.total_bytes, Some(bytes.len() as u64));
            }
            assert!(
                !remote
                    .files
                    .lock()
                    .unwrap()
                    .keys()
                    .any(|path| path.contains(".rustterm-upload-"))
            );
            client.close_session().unwrap();
            server.await.unwrap();
        })
        .await
        .unwrap();
    }
    #[test]
    fn dropping_transfer_clone_does_not_cancel() {
        let transfer = SftpTransfer(
            Arc::new(TransferState {
                progress: Mutex::new(SftpTransferProgress {
                    bytes_done: 0,
                    total_bytes: None,
                    state: SftpTransferState::Queued,
                }),
                cancel: CancellationToken::new(),
            }),
            None,
        );
        drop(transfer.clone());
        assert!(!transfer.0.cancel.is_cancelled());
        transfer.cancel();
        assert!(transfer.0.cancel.is_cancelled());
    }

    struct MemorySshClient;

    impl client::Handler for MemorySshClient {
        type Error = russh::Error;

        async fn check_server_key(
            &mut self,
            _: &russh::keys::PublicKeyOrCertificate,
        ) -> Result<bool, Self::Error> {
            Ok(true)
        }
    }

    async fn memory_ssh_connection(
        remote: Arc<MemoryRemote>,
    ) -> (
        client::Handle<MemorySshClient>,
        tokio::task::JoinHandle<Result<(), russh::Error>>,
    ) {
        use russh::{
            keys::{Algorithm, PrivateKey},
            server,
        };
        let host = PrivateKey::random(&mut rand::rng(), Algorithm::Ed25519).unwrap();
        let config = Arc::new(server::Config {
            keys: vec![host],
            auth_rejection_time: Duration::ZERO,
            auth_rejection_time_initial: Some(Duration::ZERO),
            ..Default::default()
        });
        let (client_stream, server_stream) = tokio::io::duplex(64 * 1024);
        let handler = TestSsh {
            channels: Default::default(),
            remote,
        };
        let server = tokio::spawn(async move {
            server::run_stream(config, server_stream, handler)
                .await
                .unwrap()
                .await
        });
        let mut handle = client::connect_stream(
            Arc::new(client::Config::default()),
            client_stream,
            MemorySshClient,
        )
        .await
        .unwrap();
        assert!(
            handle
                .authenticate_password("test", "test")
                .await
                .unwrap()
                .success()
        );
        (handle, server)
    }

    #[tokio::test]
    async fn abandoned_open_replies_close_confirmed_ssh_channels() {
        tokio::time::timeout(Duration::from_secs(5), async {
            let remote = Arc::new(MemoryRemote::default());
            let (handle, server) = memory_ssh_connection(remote.clone()).await;
            for abandoned in ["dropped", "cancelled", "already closed"] {
                let channel = OpenedChannel::new(handle.channel_open_session().await.unwrap());
                let (reply, receive) = oneshot::channel();
                let cancel = CancellationToken::new();
                let request = OpenRequest {
                    reply,
                    cancel: cancel.clone(),
                };
                if abandoned == "already closed" {
                    drop(receive);
                    assert!(request.reply.send(Ok(channel)).is_err());
                } else {
                    assert!(request.reply.send(Ok(channel)).is_ok());
                    if abandoned == "cancelled" {
                        cancel.cancel();
                        let result = bounded(&cancel, REQUEST_TIMEOUT, async {
                            receive.await.map_err(|_| SftpError::Disconnected)?
                        })
                        .await;
                        assert!(matches!(result, Err(SftpError::Cancelled)));
                    } else {
                        drop(receive);
                    }
                }
                remote.channel_closed.notified().await;
                assert!(!handle.is_closed());
            }
            handle
                .disconnect(russh::Disconnect::ByApplication, "", "")
                .await
                .unwrap();
            // russh reports a locally initiated client shutdown as Disconnect.
            assert!(matches!(handle.await, Err(russh::Error::Disconnect)));
            server.await.unwrap().unwrap();
        })
        .await
        .unwrap();
    }

    #[tokio::test]
    async fn dropping_public_save_future_cleans_remote_file_before_closing_channel() {
        use std::sync::atomic::Ordering;

        tokio::time::timeout(Duration::from_secs(5), async {
            let remote = Arc::new(MemoryRemote::default());
            remote
                .files
                .lock()
                .unwrap()
                .insert("/config".into(), b"original".to_vec());
            let (handle, server) = memory_ssh_connection(remote.clone()).await;
            let (sftp, mut requests) = SftpClient::new(CancellationToken::new());

            let mut read = Box::pin(sftp.read_text("/config"));
            let request = tokio::select! {
                request = requests.recv() => request.unwrap(),
                _ = &mut read => panic!("read finished before its channel was opened"),
            };
            let channel = handle.channel_open_session().await.unwrap();
            assert!(request.reply.send(Ok(OpenedChannel::new(channel))).is_ok());
            let document = read.await.unwrap();
            remote.channel_closed.notified().await;

            remote.pause_write.store(true, Ordering::Release);
            let mut save = Box::pin(sftp.save_text(document, "local edit".into()));
            let request = tokio::select! {
                request = requests.recv() => request.unwrap(),
                _ = &mut save => panic!("save finished before its channel was opened"),
            };
            let channel = handle.channel_open_session().await.unwrap();
            assert!(request.reply.send(Ok(OpenedChannel::new(channel))).is_ok());
            tokio::select! {
                _ = remote.write_started.notified() => {},
                _ = &mut save => panic!("save finished before the paused write"),
            }
            assert_eq!(remote.files.lock().unwrap().len(), 2);

            // Dropping the public future must cancel its detached worker without
            // dropping the worker's cleanup or closing the SSH channel too early.
            drop(save);
            remote.resume_write.notify_one();
            remote.channel_closed.notified().await;
            {
                let files = remote.files.lock().unwrap();
                assert_eq!(files.len(), 1);
                assert_eq!(files["/config"], b"original");
            }
            assert!(!handle.is_closed());
            drop(sftp);
            handle
                .disconnect(russh::Disconnect::ByApplication, "", "")
                .await
                .unwrap();
            assert!(matches!(handle.await, Err(russh::Error::Disconnect)));
            server.await.unwrap().unwrap();
        })
        .await
        .unwrap();
    }

    /// An actual SSH transport plus SFTP v3 packets, backed by a tiny in-memory
    /// filesystem. This checks the public executor bridge and shell isolation.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn authenticated_connection_supports_sftp_without_reauth_or_shell_interference() {
        use crate::session::{Command, Session, SessionState, SshAuth, SshOptions};
        use russh::{
            keys::{Algorithm, PrivateKey},
            server,
        };
        use std::sync::atomic::Ordering;

        let remote = Arc::new(MemoryRemote::default());
        let host = PrivateKey::random(&mut rand::rng(), Algorithm::Ed25519).unwrap();
        let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0))
            .await
            .unwrap();
        let port = listener.local_addr().unwrap().port();
        let config = Arc::new(server::Config {
            keys: vec![host.clone()],
            auth_rejection_time: Duration::ZERO,
            auth_rejection_time_initial: Some(Duration::ZERO),
            ..Default::default()
        });
        let server_remote = remote.clone();
        let server = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let handler = TestSsh {
                channels: Default::default(),
                remote: server_remote,
            };
            let connection = server::run_stream(config, stream, handler).await.unwrap();
            let _ = connection.await;
        });
        let directory = tempfile::tempdir().unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(directory.path(), std::fs::Permissions::from_mode(0o700))
                .unwrap();
        }
        let known_hosts = directory.path().join("hosts.toml");
        let public = host.public_key();
        crate::config::KnownHosts::trust_and_save(
            &known_hosts,
            "127.0.0.1",
            port,
            public.algorithm().as_ref(),
            &public.to_openssh().unwrap(),
        )
        .unwrap();
        let session = Session::spawn_ssh(
            SshOptions {
                host: "127.0.0.1".into(),
                port,
                username: "test".into(),
                auth: SshAuth::Password("test".into()),
                known_hosts_path: known_hosts,
                timeout_secs: 5,
                keepalive_secs: 0,
                keepalive_max: 2,
                shell_integration: false,
                jump_hosts: Vec::new(),
                forwards: Vec::new(),
                auto_forward: Default::default(),
                forwarding_registry: None,
                authentication: None,
            },
            80,
            24,
            10,
        )
        .unwrap();
        wait_for(|| matches!(session.state(), SessionState::Connected)).await;
        let client = session.sftp().unwrap();
        // No Tokio reactor is entered on this caller's thread.
        let background_client = client.clone();
        let background = std::thread::spawn(move || {
            assert!(tokio::runtime::Handle::try_current().is_err());
            without_runtime(background_client.canonicalize("."))
        });
        let canonical = tokio::task::spawn_blocking(move || background.join().unwrap())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(canonical, "/home/test");

        client.mkdir("/home/test/sub").await.unwrap();
        assert!(
            client
                .list_dir("/home/test")
                .await
                .unwrap()
                .iter()
                .any(|e| e.name == "sub" && e.is_dir)
        );
        let source = directory.path().join("source");
        let bytes: Vec<u8> = (0..(CHUNK * 2 + 7)).map(|n| (n % 251) as u8).collect();
        std::fs::write(&source, &bytes).unwrap();
        let upload = client
            .upload(source.clone(), "/home/test/sub/a".into())
            .unwrap();
        wait_transfer(&upload).await;
        assert_eq!(upload.progress().state, SftpTransferState::Completed);
        assert_eq!(upload.progress().bytes_done, bytes.len() as u64);
        assert_eq!(
            remote.files.lock().unwrap().get("/home/test/sub/a"),
            Some(&bytes)
        );
        let duplicate = client
            .upload(source.clone(), "/home/test/sub/a".into())
            .unwrap();
        wait_transfer(&duplicate).await;
        assert!(matches!(
            duplicate.progress().state,
            SftpTransferState::Failed(_)
        ));
        assert!(
            client
                .rename("/home/test/sub/a", "/home/test/sub/a")
                .await
                .is_err()
        );
        client
            .rename("/home/test/sub/a", "/home/test/sub/b")
            .await
            .unwrap();

        let target = directory.path().join("download");
        let download = client
            .download("/home/test/sub/b".into(), target.clone())
            .unwrap();
        wait_transfer(&download).await;
        assert_eq!(download.progress().state, SftpTransferState::Completed);
        assert_eq!(std::fs::read(&target).unwrap(), bytes);
        let duplicate = client
            .download("/home/test/sub/b".into(), target.clone())
            .unwrap();
        wait_transfer(&duplicate).await;
        assert!(matches!(
            duplicate.progress().state,
            SftpTransferState::Failed(_)
        ));
        assert_eq!(std::fs::read(&target).unwrap(), bytes);

        remote.pause_write.store(true, Ordering::Release);
        let cancelled = client
            .upload(source, "/home/test/cancelled".into())
            .unwrap();
        tokio::time::timeout(Duration::from_secs(5), remote.write_started.notified())
            .await
            .unwrap();
        cancelled.cancel();
        remote.resume_write.notify_one();
        wait_transfer(&cancelled).await;
        assert_eq!(cancelled.progress().state, SftpTransferState::Cancelled);
        assert!(
            !remote
                .files
                .lock()
                .unwrap()
                .keys()
                .any(|p| p.contains(".rustterm-upload-") || p.ends_with("/cancelled"))
        );
        session
            .send(Command::Input(b"shell-still-works".to_vec()))
            .unwrap();
        wait_for(|| remote.shell_input.lock().unwrap().as_slice() == b"shell-still-works").await;
        assert_eq!(remote.authentications.load(Ordering::Acquire), 1);
        assert!(matches!(session.state(), SessionState::Connected));
        assert!(client.remove("/home/test/sub", true).await.is_err());
        client.remove("/home/test/sub/b", false).await.unwrap();
        client.remove("/home/test/sub", true).await.unwrap();
        session.disconnect();
        wait_for(|| matches!(session.state(), SessionState::Disconnected { .. })).await;
        assert!(client.canonicalize(".").await.is_err());
        session.close();
        tokio::time::timeout(Duration::from_secs(5), server)
            .await
            .unwrap()
            .unwrap();
    }

    async fn wait_for(mut condition: impl FnMut() -> bool) {
        tokio::time::timeout(Duration::from_secs(5), async {
            while !condition() {
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .expect("SFTP test condition timed out");
    }
    async fn wait_transfer(transfer: &SftpTransfer) {
        wait_for(|| {
            !matches!(
                transfer.progress().state,
                SftpTransferState::Queued | SftpTransferState::Running
            )
        })
        .await;
    }
    fn without_runtime<F: Future>(future: F) -> F::Output {
        struct ThreadWaker(std::thread::Thread);
        impl std::task::Wake for ThreadWaker {
            fn wake(self: Arc<Self>) {
                self.0.unpark();
            }
            fn wake_by_ref(self: &Arc<Self>) {
                self.0.unpark();
            }
        }
        let waker = std::task::Waker::from(Arc::new(ThreadWaker(std::thread::current())));
        let mut context = Context::from_waker(&waker);
        let mut future = std::pin::pin!(future);
        loop {
            if let Poll::Ready(value) = future.as_mut().poll(&mut context) {
                return value;
            }
            std::thread::park();
        }
    }
    #[tokio::test]
    async fn recursive_transfers_resume_confirmed_offsets_and_preserve_conflicts() {
        let remote = Arc::new(MemoryRemote::default());
        let (client_stream, server_stream) = tokio::io::duplex(8192);
        let server = tokio::spawn(russh_sftp::server::run(
            server_stream,
            TestSftp {
                remote: remote.clone(),
                listed: false,
            },
        ));
        let raw = RawSftpSession::new(client_stream);
        let (client, _requests) = SftpClient::new(CancellationToken::new());
        let directory = tempfile::tempdir().unwrap();
        let source = directory.path().join("source");
        std::fs::create_dir_all(source.join("nested")).unwrap();
        let contents = vec![0x7d; CHUNK * 2 + 17];
        std::fs::write(source.join("nested/data.bin"), &contents).unwrap();
        tokio::time::timeout(Duration::from_secs(3), async {
            raw.init().await.unwrap();
            *remote.fail_write_offset.lock().unwrap() = Some(CHUNK as u64);
            let plan = workflows::TransferPlan::new(
                source,
                "/target".into(),
                true,
                SftpConflictPolicy::Rename,
                &client.0,
            );
            assert!(
                plan.run(&raw, &client.0.stop, &running_transfer())
                    .await
                    .is_err()
            );
            assert!(
                !remote
                    .files
                    .lock()
                    .unwrap()
                    .contains_key("/target/nested/data.bin")
            );
            plan.run(&raw, &client.0.stop, &running_transfer())
                .await
                .unwrap();
            assert_eq!(
                remote.files.lock().unwrap()["/target/nested/data.bin"],
                contents
            );
            let destination = directory.path().join("download");
            let download = workflows::TransferPlan::new(
                destination.clone(),
                "/target".into(),
                false,
                SftpConflictPolicy::Rename,
                &client.0,
            );
            *remote.fail_read_offset.lock().unwrap() = Some(CHUNK as u64);
            assert!(
                download
                    .run(&raw, &client.0.stop, &running_transfer())
                    .await
                    .is_err()
            );
            assert!(!destination.join("nested/data.bin").exists());
            // A different source must not be appended to a retained local partial.
            remote
                .files
                .lock()
                .unwrap()
                .get_mut("/target/nested/data.bin")
                .unwrap()
                .push(1);
            assert!(
                download
                    .run(&raw, &client.0.stop, &running_transfer())
                    .await
                    .is_err()
            );
            remote
                .files
                .lock()
                .unwrap()
                .get_mut("/target/nested/data.bin")
                .unwrap()
                .pop();
            download
                .run(&raw, &client.0.stop, &running_transfer())
                .await
                .unwrap();
            assert_eq!(
                std::fs::read(destination.join("nested/data.bin")).unwrap(),
                contents
            );
            let rename = workflows::TransferPlan::new(
                destination.clone(),
                "/target".into(),
                false,
                SftpConflictPolicy::Rename,
                &client.0,
            );
            rename
                .run(&raw, &client.0.stop, &running_transfer())
                .await
                .unwrap();
            assert_eq!(
                std::fs::read(directory.path().join("download (1)/nested/data.bin")).unwrap(),
                contents
            );
            raw.close_session().unwrap();
            server.await.unwrap();
        })
        .await
        .unwrap();
    }

    #[tokio::test]
    async fn remote_edit_rejects_changed_content_without_overwriting_it() {
        let remote = Arc::new(MemoryRemote::default());
        remote
            .files
            .lock()
            .unwrap()
            .insert("/config".into(), b"original".to_vec());
        let (client_stream, server_stream) = tokio::io::duplex(4096);
        let server = tokio::spawn(russh_sftp::server::run(
            server_stream,
            TestSftp {
                remote: remote.clone(),
                listed: false,
            },
        ));
        let raw = RawSftpSession::new(client_stream);
        raw.init().await.unwrap();
        let document = workflows::read_text(&raw, "/config".into()).await.unwrap();
        remote
            .files
            .lock()
            .unwrap()
            .insert("/config".into(), b"remote edit".to_vec());
        assert!(
            workflows::save_text(
                &raw,
                &CancellationToken::new(),
                &CancellationToken::new(),
                document,
                "local edit".into(),
            )
            .await
            .is_err()
        );
        assert_eq!(remote.files.lock().unwrap()["/config"], b"remote edit");
        raw.close_session().unwrap();
        server.await.unwrap();
    }

    #[tokio::test]
    async fn remote_edit_cancellation_cleans_confirmed_temporary_files() {
        use std::sync::atomic::Ordering;

        tokio::time::timeout(Duration::from_secs(5), async {
            for cancel_during_create in [true, false] {
                let remote = Arc::new(MemoryRemote::default());
                remote
                    .files
                    .lock()
                    .unwrap()
                    .insert("/config".into(), b"original".to_vec());
                let (client_stream, server_stream) = tokio::io::duplex(4096);
                let server = tokio::spawn(russh_sftp::server::run(
                    server_stream,
                    TestSftp {
                        remote: remote.clone(),
                        listed: false,
                    },
                ));
                let raw = Arc::new(RawSftpSession::new(client_stream));
                raw.init().await.unwrap();
                let document = workflows::read_text(&raw, "/config".into()).await.unwrap();
                if cancel_during_create {
                    remote.pause_create.store(true, Ordering::Release);
                } else {
                    remote.pause_write.store(true, Ordering::Release);
                }
                let stop = CancellationToken::new();
                let cancel = stop.child_token();
                let save_cancel = cancel.clone();
                let save_raw = raw.clone();
                let save = tokio::spawn(async move {
                    super::execute_request(
                        &save_raw,
                        Request::SaveText(document, "local edit".into()),
                        &stop,
                        &save_cancel,
                    )
                    .await
                });
                if cancel_during_create {
                    remote.create_started.notified().await;
                } else {
                    remote.write_started.notified().await;
                    assert_eq!(remote.files.lock().unwrap().len(), 2);
                }
                cancel.cancel();
                if cancel_during_create {
                    remote.resume_create.notify_one();
                } else {
                    remote.resume_write.notify_one();
                }
                assert!(matches!(save.await.unwrap(), Err(SftpError::Cancelled)));
                {
                    let files = remote.files.lock().unwrap();
                    assert_eq!(files.len(), 1);
                    assert_eq!(files["/config"], b"original");
                }
                raw.close_session().unwrap();
                server.await.unwrap();
            }
        })
        .await
        .unwrap();
    }

    #[tokio::test]
    async fn remote_edit_reports_success_when_cancelled_after_commit() {
        use std::sync::atomic::Ordering;

        tokio::time::timeout(Duration::from_secs(5), async {
            let remote = Arc::new(MemoryRemote::default());
            remote
                .files
                .lock()
                .unwrap()
                .insert("/config".into(), b"original".to_vec());
            remote.pause_commit.store(true, Ordering::Release);
            let (client_stream, server_stream) = tokio::io::duplex(4096);
            let server = tokio::spawn(russh_sftp::server::run(
                server_stream,
                TestSftp {
                    remote: remote.clone(),
                    listed: false,
                },
            ));
            let raw = Arc::new(RawSftpSession::new(client_stream));
            raw.init().await.unwrap();
            let document = workflows::read_text(&raw, "/config".into()).await.unwrap();
            let stop = CancellationToken::new();
            let cancel = stop.child_token();
            let save_cancel = cancel.clone();
            let save_raw = raw.clone();
            let save = tokio::spawn(async move {
                super::execute_request(
                    &save_raw,
                    Request::SaveText(document, "local edit".into()),
                    &stop,
                    &save_cancel,
                )
                .await
            });
            remote.commit_started.notified().await;
            assert_eq!(remote.files.lock().unwrap()["/config"], b"local edit");
            cancel.cancel();
            remote.resume_commit.notify_one();
            let Response::Text(document) = save.await.unwrap().unwrap() else {
                panic!("save returned a non-text response");
            };
            assert_eq!(document.text, "local edit");
            // The returned baseline must support the next save without a false conflict.
            workflows::save_text(
                &raw,
                &CancellationToken::new(),
                &CancellationToken::new(),
                document,
                "second edit".into(),
            )
            .await
            .unwrap();
            assert_eq!(remote.files.lock().unwrap().len(), 1);
            assert_eq!(remote.files.lock().unwrap()["/config"], b"second edit");
            raw.close_session().unwrap();
            server.await.unwrap();
        })
        .await
        .unwrap();
    }

    #[derive(Default)]
    struct MemoryRemote {
        files: Mutex<std::collections::BTreeMap<String, Vec<u8>>>,
        reported_sizes: Mutex<std::collections::BTreeMap<String, u64>>,
        directories: Mutex<std::collections::BTreeSet<String>>,
        links: Mutex<std::collections::BTreeMap<String, String>>,
        stats: std::sync::atomic::AtomicUsize,
        authentications: std::sync::atomic::AtomicUsize,
        shell_input: Mutex<Vec<u8>>,
        pause_write: std::sync::atomic::AtomicBool,
        pause_create: std::sync::atomic::AtomicBool,
        pause_commit: std::sync::atomic::AtomicBool,
        fail_write_offset: Mutex<Option<u64>>,
        fail_read_offset: Mutex<Option<u64>>,
        create_started: tokio::sync::Notify,
        resume_create: tokio::sync::Notify,
        write_started: tokio::sync::Notify,
        resume_write: tokio::sync::Notify,
        commit_started: tokio::sync::Notify,
        resume_commit: tokio::sync::Notify,
        channel_closed: tokio::sync::Notify,
    }
    struct TestSsh {
        channels: std::collections::HashMap<russh::ChannelId, Channel<russh::server::Msg>>,
        remote: Arc<MemoryRemote>,
    }
    impl russh::server::Handler for TestSsh {
        type Error = russh::Error;
        async fn auth_password(
            &mut self,
            user: &str,
            password: &str,
        ) -> Result<russh::server::Auth, Self::Error> {
            self.remote
                .authentications
                .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            Ok(if user == "test" && password == "test" {
                russh::server::Auth::Accept
            } else {
                russh::server::Auth::reject()
            })
        }
        async fn channel_open_session(
            &mut self,
            channel: Channel<russh::server::Msg>,
            reply: russh::server::ChannelOpenHandle,
            _: &mut russh::server::Session,
        ) -> Result<(), Self::Error> {
            self.channels.insert(channel.id(), channel);
            reply.accept().await;
            Ok(())
        }
        async fn channel_close(
            &mut self,
            channel: russh::ChannelId,
            _: &mut russh::server::Session,
        ) -> Result<(), Self::Error> {
            self.channels.remove(&channel);
            self.remote.channel_closed.notify_one();
            Ok(())
        }
        async fn pty_request(
            &mut self,
            channel: russh::ChannelId,
            _: &str,
            _: u32,
            _: u32,
            _: u32,
            _: u32,
            _: &[(russh::Pty, u32)],
            session: &mut russh::server::Session,
        ) -> Result<(), Self::Error> {
            session.channel_success(channel)
        }
        async fn shell_request(
            &mut self,
            channel: russh::ChannelId,
            session: &mut russh::server::Session,
        ) -> Result<(), Self::Error> {
            self.channels.remove(&channel);
            session.channel_success(channel)
        }
        async fn subsystem_request(
            &mut self,
            channel: russh::ChannelId,
            name: &str,
            session: &mut russh::server::Session,
        ) -> Result<(), Self::Error> {
            if name != "sftp" {
                return session.channel_failure(channel);
            }
            session.channel_success(channel)?;
            let stream = self.channels.remove(&channel).unwrap().into_stream();
            russh_sftp::server::run(
                stream,
                TestSftp {
                    remote: self.remote.clone(),
                    listed: false,
                },
            )
            .await;
            Ok(())
        }
        async fn data(
            &mut self,
            channel: russh::ChannelId,
            data: &[u8],
            session: &mut russh::server::Session,
        ) -> Result<(), Self::Error> {
            // SFTP data is consumed by the channel stream. Only the shell's
            // marker is observed here; never echo binary SFTP framing.
            if data == b"shell-still-works" {
                self.remote
                    .shell_input
                    .lock()
                    .unwrap()
                    .extend_from_slice(data);
                session.data(channel, data.to_vec())?;
            }
            Ok(())
        }
    }
    struct TestSftp {
        remote: Arc<MemoryRemote>,
        listed: bool,
    }
    fn ok_status(id: u32) -> russh_sftp::protocol::Status {
        russh_sftp::protocol::Status {
            id,
            status_code: StatusCode::Ok,
            error_message: String::new(),
            language_tag: String::new(),
        }
    }
    impl russh_sftp::server::Handler for TestSftp {
        type Error = StatusCode;
        fn unimplemented(&self) -> Self::Error {
            StatusCode::OpUnsupported
        }
        async fn realpath(
            &mut self,
            id: u32,
            _: String,
        ) -> Result<russh_sftp::protocol::Name, Self::Error> {
            Ok(russh_sftp::protocol::Name {
                id,
                files: vec![russh_sftp::protocol::File::dummy("/home/test")],
            })
        }
        async fn lstat(
            &mut self,
            id: u32,
            path: String,
        ) -> Result<russh_sftp::protocol::Attrs, Self::Error> {
            let attrs = if let Some(bytes) = self.remote.files.lock().unwrap().get(&path) {
                FileAttributes {
                    size: Some(bytes.len() as u64),
                    permissions: Some(0o100600),
                    mtime: Some(1),
                    ..FileAttributes::empty()
                }
            } else if self.remote.directories.lock().unwrap().contains(&path) {
                FileAttributes {
                    permissions: Some(0o40700),
                    ..FileAttributes::empty()
                }
            } else if self.remote.links.lock().unwrap().contains_key(&path) {
                FileAttributes {
                    permissions: Some(0o120777),
                    ..FileAttributes::empty()
                }
            } else {
                return Err(StatusCode::NoSuchFile);
            };
            Ok(russh_sftp::protocol::Attrs { id, attrs })
        }
        async fn stat(
            &mut self,
            id: u32,
            path: String,
        ) -> Result<russh_sftp::protocol::Attrs, Self::Error> {
            self.remote
                .stats
                .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            let target = self
                .remote
                .links
                .lock()
                .unwrap()
                .get(&path)
                .cloned()
                .unwrap_or(path);
            self.lstat(id, target).await
        }
        async fn fstat(
            &mut self,
            id: u32,
            handle: String,
        ) -> Result<russh_sftp::protocol::Attrs, Self::Error> {
            let mut result = self.lstat(id, handle.clone()).await?;
            if let Some(size) = self.remote.reported_sizes.lock().unwrap().get(&handle) {
                result.attrs.size = Some(*size);
            }
            Ok(result)
        }
        async fn fsetstat(
            &mut self,
            id: u32,
            handle: String,
            attrs: FileAttributes,
        ) -> Result<russh_sftp::protocol::Status, Self::Error> {
            if let Some(size) = attrs.size {
                self.remote
                    .files
                    .lock()
                    .unwrap()
                    .get_mut(&handle)
                    .ok_or(StatusCode::NoSuchFile)?
                    .truncate(size as usize);
            }
            Ok(ok_status(id))
        }
        async fn open(
            &mut self,
            id: u32,
            filename: String,
            flags: OpenFlags,
            _: FileAttributes,
        ) -> Result<russh_sftp::protocol::Handle, Self::Error> {
            if flags.contains(OpenFlags::CREATE)
                && self
                    .remote
                    .pause_create
                    .swap(false, std::sync::atomic::Ordering::AcqRel)
            {
                self.remote.create_started.notify_one();
                self.remote.resume_create.notified().await;
            }
            let mut files = self.remote.files.lock().unwrap();
            if flags.contains(OpenFlags::CREATE) {
                assert!(flags.contains(OpenFlags::EXCLUDE));
                if files.contains_key(&filename) {
                    return Err(StatusCode::Failure);
                }
                files.insert(filename.clone(), Vec::new());
            } else if !files.contains_key(&filename) {
                return Err(StatusCode::NoSuchFile);
            }
            Ok(russh_sftp::protocol::Handle {
                id,
                handle: filename,
            })
        }
        async fn close(
            &mut self,
            id: u32,
            _: String,
        ) -> Result<russh_sftp::protocol::Status, Self::Error> {
            Ok(ok_status(id))
        }
        async fn read(
            &mut self,
            id: u32,
            handle: String,
            offset: u64,
            len: u32,
        ) -> Result<russh_sftp::protocol::Data, Self::Error> {
            let mut fail = self.remote.fail_read_offset.lock().unwrap();
            if *fail == Some(offset) {
                *fail = None;
                return Err(StatusCode::Failure);
            }
            drop(fail);
            let files = self.remote.files.lock().unwrap();
            let bytes = files.get(&handle).ok_or(StatusCode::NoSuchFile)?;
            let offset = offset as usize;
            if offset >= bytes.len() {
                return Err(StatusCode::Eof);
            }
            assert!(len as usize <= CHUNK);
            Ok(russh_sftp::protocol::Data {
                id,
                data: bytes[offset..bytes.len().min(offset + len as usize)].to_vec(),
            })
        }
        async fn write(
            &mut self,
            id: u32,
            handle: String,
            offset: u64,
            data: Vec<u8>,
        ) -> Result<russh_sftp::protocol::Status, Self::Error> {
            if self
                .remote
                .pause_write
                .swap(false, std::sync::atomic::Ordering::AcqRel)
            {
                self.remote.write_started.notify_one();
                self.remote.resume_write.notified().await;
            }
            assert!(data.len() <= CHUNK);
            let mut files = self.remote.files.lock().unwrap();
            let bytes = files.get_mut(&handle).ok_or(StatusCode::NoSuchFile)?;
            assert_eq!(offset as usize, bytes.len());
            bytes.extend(data);
            let mut fail = self.remote.fail_write_offset.lock().unwrap();
            if *fail == Some(offset) {
                *fail = None;
                return Err(StatusCode::Failure);
            }
            Ok(ok_status(id))
        }
        async fn extended(
            &mut self,
            id: u32,
            request: String,
            data: Vec<u8>,
        ) -> Result<russh_sftp::protocol::Packet, Self::Error> {
            if request != "posix-rename@openssh.com" {
                return Err(StatusCode::OpUnsupported);
            }
            let mut fields = data.as_slice();
            let mut path = || {
                let length = u32::from_be_bytes(fields[..4].try_into().unwrap()) as usize;
                let value = std::str::from_utf8(&fields[4..4 + length])
                    .unwrap()
                    .to_owned();
                fields = &fields[4 + length..];
                value
            };
            let source = path();
            let target = path();
            {
                let mut files = self.remote.files.lock().unwrap();
                let bytes = files.remove(&source).ok_or(StatusCode::NoSuchFile)?;
                files.insert(target, bytes);
            }
            if self
                .remote
                .pause_commit
                .swap(false, std::sync::atomic::Ordering::AcqRel)
            {
                self.remote.commit_started.notify_one();
                self.remote.resume_commit.notified().await;
            }
            Ok(russh_sftp::protocol::Packet::Status(ok_status(id)))
        }
        async fn rename(
            &mut self,
            id: u32,
            old: String,
            new: String,
        ) -> Result<russh_sftp::protocol::Status, Self::Error> {
            let mut files = self.remote.files.lock().unwrap();
            let mut links = self.remote.links.lock().unwrap();
            if files.contains_key(&new)
                || links.contains_key(&new)
                || self.remote.directories.lock().unwrap().contains(&new)
            {
                return Err(StatusCode::Failure);
            }
            if let Some(target) = links.remove(&old) {
                links.insert(new, target);
                return Ok(ok_status(id));
            }
            let bytes = files.remove(&old).ok_or(StatusCode::NoSuchFile)?;
            files.insert(new, bytes);
            Ok(ok_status(id))
        }
        async fn remove(
            &mut self,
            id: u32,
            path: String,
        ) -> Result<russh_sftp::protocol::Status, Self::Error> {
            if self.remote.links.lock().unwrap().remove(&path).is_none() {
                self.remote
                    .files
                    .lock()
                    .unwrap()
                    .remove(&path)
                    .ok_or(StatusCode::NoSuchFile)?;
            }
            Ok(ok_status(id))
        }
        async fn mkdir(
            &mut self,
            id: u32,
            path: String,
            _: FileAttributes,
        ) -> Result<russh_sftp::protocol::Status, Self::Error> {
            if !self.remote.directories.lock().unwrap().insert(path) {
                return Err(StatusCode::Failure);
            }
            Ok(ok_status(id))
        }
        async fn rmdir(
            &mut self,
            id: u32,
            path: String,
        ) -> Result<russh_sftp::protocol::Status, Self::Error> {
            if self
                .remote
                .files
                .lock()
                .unwrap()
                .keys()
                .any(|p| p.starts_with(&format!("{path}/")))
            {
                return Err(StatusCode::Failure);
            }
            if !self.remote.directories.lock().unwrap().remove(&path) {
                return Err(StatusCode::NoSuchFile);
            }
            Ok(ok_status(id))
        }
        async fn opendir(
            &mut self,
            id: u32,
            path: String,
        ) -> Result<russh_sftp::protocol::Handle, Self::Error> {
            self.listed = false;
            Ok(russh_sftp::protocol::Handle { id, handle: path })
        }
        async fn readdir(
            &mut self,
            id: u32,
            path: String,
        ) -> Result<russh_sftp::protocol::Name, Self::Error> {
            if self.listed {
                return Err(StatusCode::Eof);
            }
            self.listed = true;
            let prefix = format!("{path}/");
            let mut files = vec![
                russh_sftp::protocol::File::dummy("."),
                russh_sftp::protocol::File::dummy(".."),
            ];
            for directory in self.remote.directories.lock().unwrap().iter() {
                if let Some(name) = directory.strip_prefix(&prefix)
                    && !name.contains('/')
                {
                    files.push(russh_sftp::protocol::File::new(
                        name,
                        FileAttributes {
                            permissions: Some(0o40700),
                            ..FileAttributes::empty()
                        },
                    ));
                }
            }
            for path in self.remote.links.lock().unwrap().keys() {
                if let Some(name) = path.strip_prefix(&prefix)
                    && !name.contains('/')
                {
                    files.push(russh_sftp::protocol::File::new(
                        name,
                        FileAttributes {
                            permissions: Some(0o120777),
                            ..FileAttributes::empty()
                        },
                    ));
                }
            }
            for (path, content) in self.remote.files.lock().unwrap().iter() {
                if let Some(name) = path.strip_prefix(&prefix)
                    && !name.contains('/')
                {
                    files.push(russh_sftp::protocol::File::new(
                        name,
                        FileAttributes {
                            permissions: Some(0o100600),
                            size: Some(content.len() as u64),
                            mtime: Some(1),
                            ..FileAttributes::empty()
                        },
                    ));
                }
            }
            Ok(russh_sftp::protocol::Name { id, files })
        }
    }
}
