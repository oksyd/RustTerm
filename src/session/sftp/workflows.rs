use crate::i18n::messages as i18n_key;

use super::{
    CHUNK, CLEANUP_TIMEOUT, ClientInner, Connection, MAX_ENTRIES, MAX_LIST_BYTES,
    OPERATION_TIMEOUT, REQUEST_TIMEOUT, SftpError, TransferState, bounded, ensure_absent,
    join_remote, list_directory, open_regular_file, remote_temporary, set_progress, validate_name,
    validate_target,
};
use russh_sftp::{
    client::{RawSftpSession, error::Error as ProtocolError},
    protocol::{FileAttributes, OpenFlags, StatusCode},
};
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, VecDeque};
use std::{
    io,
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
    time::Duration,
};
use tokio::io::AsyncSeekExt;
use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
use tokio_util::sync::CancellationToken;

const MAX_DEPTH: usize = 64;
pub const MAX_TEXT_BYTES: usize = 1024 * 1024;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum SftpConflictPolicy {
    #[default]
    Rename,
    Skip,
    Overwrite,
}

#[derive(Clone)]
pub struct SftpTextDocument {
    pub path: String,
    pub text: String,
    digest: [u8; 32],
    permissions: Option<u32>,
}

pub(super) struct TransferPlan {
    client: std::sync::Weak<ClientInner>,
    local: PathBuf,
    remote: String,
    upload: bool,
    policy: SftpConflictPolicy,
    running: tokio::sync::Mutex<()>,
    jobs: Mutex<Option<Vec<Job>>>,
    checkpoints: Mutex<BTreeMap<usize, Checkpoint>>,
    completed: Mutex<BTreeMap<usize, u64>>,
}

#[derive(Clone)]
struct Job {
    local: PathBuf,
    remote: String,
    directory: bool,
    size: u64,
}

enum Checkpoint {
    Upload {
        temporary: String,
        identity: LocalIdentity,
        offset: u64,
    },
    Download {
        temporary: tempfile::NamedTempFile,
        identity: RemoteIdentity,
        offset: u64,
    },
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct LocalIdentity {
    size: u64,
    modified: Option<std::time::SystemTime>,
    #[cfg(unix)]
    device: u64,
    #[cfg(unix)]
    inode: u64,
}
impl LocalIdentity {
    fn new(metadata: &std::fs::Metadata) -> Self {
        #[cfg(unix)]
        use std::os::unix::fs::MetadataExt;
        Self {
            size: metadata.len(),
            modified: metadata.modified().ok(),
            #[cfg(unix)]
            device: metadata.dev(),
            #[cfg(unix)]
            inode: metadata.ino(),
        }
    }
}
#[derive(Clone, Debug, PartialEq, Eq)]
struct RemoteIdentity {
    size: Option<u64>,
    modified: Option<u32>,
}
impl From<&FileAttributes> for RemoteIdentity {
    fn from(value: &FileAttributes) -> Self {
        Self {
            size: value.size,
            modified: value.mtime,
        }
    }
}

fn failure(message: &'static str) -> SftpError {
    SftpError::Remote(crate::i18n::tr(message).into())
}

impl TransferPlan {
    pub(super) fn new(
        local: PathBuf,
        remote: String,
        upload: bool,
        policy: SftpConflictPolicy,
        client: &Arc<ClientInner>,
    ) -> Self {
        Self {
            client: Arc::downgrade(client),
            local,
            remote,
            upload,
            policy,
            running: tokio::sync::Mutex::new(()),
            jobs: Mutex::new(None),
            checkpoints: Mutex::new(BTreeMap::new()),
            completed: Mutex::new(BTreeMap::new()),
        }
    }

    pub(super) async fn run(
        &self,
        raw: &RawSftpSession,
        stop: &CancellationToken,
        state: &TransferState,
    ) -> Result<(), SftpError> {
        let _running = self.running.try_lock().map_err(|_| SftpError::Busy)?;
        let existing = self.jobs.lock().unwrap_or_else(|e| e.into_inner()).clone();
        let jobs = match existing {
            Some(jobs) => jobs,
            None => {
                let jobs =
                    bounded(&state.cancel, Duration::from_secs(600), self.manifest(raw)).await?;
                *self.jobs.lock().unwrap_or_else(|e| e.into_inner()) = Some(jobs.clone());
                jobs
            }
        };
        let total = jobs
            .iter()
            .try_fold(0u64, |sum, job| sum.checked_add(job.size))
            .ok_or_else(|| failure(i18n_key::CONNECTIONS_THE_FILE_IS_TOO_LARGE))?;
        set_progress(state, Some(0), Some(total), None);
        let mut base = 0;
        for (id, job) in jobs.iter().enumerate() {
            if state.cancel.is_cancelled() {
                return Err(SftpError::Cancelled);
            }
            if let Some(done) = self
                .completed
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .get(&id)
                .copied()
            {
                base += done;
                set_progress(state, Some(base), None, None);
                continue;
            }
            if job.directory {
                if self.upload {
                    match remote_metadata(raw, &job.remote).await? {
                        Some(attrs) if attrs.is_dir() && !attrs.is_symlink() => {}
                        Some(_) => {
                            return Err(failure(
                                i18n_key::SFTP_A_TRANSFER_DESTINATION_CHANGED_TYPE_REFRESH_AND_TRY_AGAIN,
                            ));
                        }
                        None => {
                            bounded(
                                &state.cancel,
                                REQUEST_TIMEOUT,
                                raw.mkdir(
                                    job.remote.clone(),
                                    FileAttributes {
                                        permissions: Some(0o700),
                                        ..FileAttributes::empty()
                                    },
                                ),
                            )
                            .await?;
                        }
                    }
                } else {
                    match tokio::fs::symlink_metadata(&job.local).await {
                        Ok(metadata) if metadata.is_dir() && !metadata.file_type().is_symlink() => {
                        }
                        Ok(_) => {
                            return Err(failure(
                                i18n_key::SFTP_A_TRANSFER_DESTINATION_CHANGED_TYPE_REFRESH_AND_TRY_AGAIN,
                            ));
                        }
                        Err(error) if error.kind() == io::ErrorKind::NotFound => {
                            tokio::fs::create_dir(&job.local).await?;
                        }
                        Err(error) => return Err(error.into()),
                    }
                }
                self.completed
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .insert(id, 0);
                continue;
            }
            let checkpoint = self
                .checkpoints
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .remove(&id);
            let (result, checkpoint) = if self.upload {
                self.upload_file(raw, stop, state, job, checkpoint, base)
                    .await
            } else {
                self.download_file(raw, state, job, checkpoint, base).await
            };
            match result {
                Ok(done) => {
                    base += done;
                    self.completed
                        .lock()
                        .unwrap_or_else(|e| e.into_inner())
                        .insert(id, done);
                }
                Err(error) => {
                    if let Some(checkpoint) = checkpoint {
                        self.checkpoints
                            .lock()
                            .unwrap_or_else(|e| e.into_inner())
                            .insert(id, checkpoint);
                    }
                    return Err(error);
                }
            }
        }
        set_progress(state, Some(base), Some(base), None);
        Ok(())
    }

    async fn manifest(&self, raw: &RawSftpSession) -> Result<Vec<Job>, SftpError> {
        let mut pending = VecDeque::from([(self.local.clone(), self.remote.clone(), 0)]);
        let mut jobs = Vec::new();
        let mut name_bytes = 0usize;
        while let Some((mut local, mut remote, depth)) = pending.pop_front() {
            if jobs.len() >= MAX_ENTRIES || depth > MAX_DEPTH {
                return Err(failure(
                    i18n_key::SFTP_THE_DIRECTORY_TREE_EXCEEDS_THE_TRANSFER_LIMIT_10_000_ENTRIES,
                ));
            }
            let (directory, size) = if self.upload {
                let metadata = tokio::fs::symlink_metadata(&local).await?;
                if metadata.file_type().is_symlink() || (!metadata.is_file() && !metadata.is_dir())
                {
                    return Err(failure(
                        i18n_key::SFTP_DIRECTORY_TRANSFERS_DO_NOT_FOLLOW_SYMBOLIC_LINKS_OR_SPECIAL_FILES,
                    ));
                }
                (
                    metadata.is_dir(),
                    if metadata.is_file() {
                        metadata.len()
                    } else {
                        0
                    },
                )
            } else {
                let metadata = raw.lstat(remote.clone()).await?.attrs;
                regular_or_directory(&metadata)?;
                (
                    metadata.is_dir(),
                    if metadata.is_dir() {
                        0
                    } else {
                        metadata.size.unwrap_or(0)
                    },
                )
            };
            if self.upload {
                if let Some(attrs) = remote_metadata(raw, &remote).await? {
                    let merge = directory
                        && attrs.is_dir()
                        && !attrs.is_symlink()
                        && self.policy != SftpConflictPolicy::Rename;
                    if !merge {
                        match self.policy {
                            SftpConflictPolicy::Skip => continue,
                            SftpConflictPolicy::Rename => {
                                remote = unique_remote(raw, &remote).await?;
                            }
                            SftpConflictPolicy::Overwrite
                                if !directory && !attrs.is_dir() && !attrs.is_symlink() => {}
                            SftpConflictPolicy::Overwrite => {
                                return Err(failure(
                                    i18n_key::SFTP_OVERWRITE_CANNOT_REPLACE_FOLDERS_SYMBOLIC_LINKS_OR_SPECIAL_FILES,
                                ));
                            }
                        }
                    }
                }
            } else if let Ok(metadata) = tokio::fs::symlink_metadata(&local).await {
                let merge = directory
                    && metadata.is_dir()
                    && !metadata.file_type().is_symlink()
                    && self.policy != SftpConflictPolicy::Rename;
                if !merge {
                    match self.policy {
                        SftpConflictPolicy::Skip => continue,
                        SftpConflictPolicy::Rename => {
                            local = unique_local(&local).await?;
                        }
                        SftpConflictPolicy::Overwrite
                            if !directory
                                && metadata.is_file()
                                && !metadata.file_type().is_symlink() => {}
                        SftpConflictPolicy::Overwrite => {
                            return Err(failure(
                                i18n_key::SFTP_OVERWRITE_CANNOT_REPLACE_FOLDERS_SYMBOLIC_LINKS_OR_SPECIAL_FILES,
                            ));
                        }
                    }
                }
            }
            name_bytes = name_bytes.saturating_add(remote.len() + local.as_os_str().len());
            if name_bytes > MAX_LIST_BYTES {
                return Err(failure(
                    i18n_key::SFTP_THE_TRANSFER_CONTAINS_TOO_MANY_LONG_FILENAMES,
                ));
            }
            jobs.push(Job {
                local: local.clone(),
                remote: remote.clone(),
                directory,
                size,
            });
            if directory {
                if self.upload {
                    let mut entries = tokio::fs::read_dir(&local).await?;
                    while let Some(entry) = entries.next_entry().await? {
                        if jobs.len() + pending.len() >= MAX_ENTRIES {
                            return Err(failure(
                                i18n_key::SFTP_THE_DIRECTORY_TREE_EXCEEDS_THE_TRANSFER_LIMIT_10_000_ENTRIES,
                            ));
                        }
                        let name = entry
                            .file_name()
                            .into_string()
                            .map_err(|_| failure(i18n_key::SFTP_A_FILENAME_IS_NOT_VALID_UTF_8))?;
                        pending.push_back((entry.path(), join_remote(&remote, &name)?, depth + 1));
                    }
                } else {
                    // Read from the source path, even if the local destination was renamed.
                    let entries = list_directory(raw, &remote).await?;
                    for entry in entries {
                        if jobs.len() + pending.len() >= MAX_ENTRIES {
                            return Err(failure(
                                i18n_key::SFTP_THE_DIRECTORY_TREE_EXCEEDS_THE_TRANSFER_LIMIT_10_000_ENTRIES,
                            ));
                        }
                        validate_name(&entry.name)?;
                        pending.push_back((local.join(&entry.name), entry.path, depth + 1));
                    }
                }
            }
        }
        Ok(jobs)
    }

    async fn upload_file(
        &self,
        raw: &RawSftpSession,
        stop: &CancellationToken,
        state: &TransferState,
        job: &Job,
        previous: Option<Checkpoint>,
        base: u64,
    ) -> (Result<u64, SftpError>, Option<Checkpoint>) {
        let mut checkpoint = previous;
        let result = async {
            let path = job.local.clone();
            let file = tokio::task::spawn_blocking(move || open_regular_file(&path))
                .await
                .map_err(|e| SftpError::Remote(e.to_string()))??;
            let identity = LocalIdentity::new(&file.metadata()?);
            let (temporary, offset, created) = match &checkpoint {
                Some(Checkpoint::Upload {
                    temporary,
                    identity: before,
                    offset,
                }) if *before == identity => (temporary.clone(), *offset, false),
                Some(_) => {
                    return Err(failure(
                        i18n_key::SFTP_THE_SOURCE_CHANGED_SINCE_THE_INTERRUPTED_TRANSFER_RESTART_THE_TRANSFER,
                    ));
                }
                None => (remote_temporary(&job.remote)?, 0, true),
            };
            if offset > identity.size {
                return Err(failure(
                    i18n_key::SFTP_THE_PARTIAL_FILE_IS_LONGER_THAN_ITS_SOURCE_RESTART_THE,
                ));
            }
            let flags = OpenFlags::WRITE
                | if created {
                    OpenFlags::CREATE | OpenFlags::EXCLUDE
                } else {
                    OpenFlags::empty()
                };
            let handle = bounded(
                stop,
                REQUEST_TIMEOUT,
                raw.open(
                    temporary.clone(),
                    flags,
                    FileAttributes {
                        permissions: Some(0o600),
                        ..FileAttributes::empty()
                    },
                ),
            )
            .await?
            .handle;
            if !created {
                let partial = raw.fstat(handle.clone()).await?.attrs;
                if partial.size.is_none_or(|size| size < offset) {
                    return Err(failure(
                        i18n_key::SFTP_THE_PARTIAL_UPLOAD_CHANGED_ON_THE_SERVER_RESTART_THE_TRANSFER,
                    ));
                }
                raw.fsetstat(
                    handle.clone(),
                    FileAttributes {
                        size: Some(offset),
                        ..FileAttributes::empty()
                    },
                )
                .await?;
            }
            checkpoint = Some(Checkpoint::Upload {
                temporary: temporary.clone(),
                identity: identity.clone(),
                offset,
            });
            let mut input = tokio::fs::File::from_std(file);
            input.seek(io::SeekFrom::Start(offset)).await?;
            let mut offset = offset;
            let mut buffer = vec![0; CHUNK];
            loop {
                let count =
                    bounded(&state.cancel, REQUEST_TIMEOUT, input.read(&mut buffer)).await?;
                if count == 0 {
                    break;
                }
                bounded(
                    &state.cancel,
                    REQUEST_TIMEOUT,
                    raw.write(handle.clone(), offset, buffer[..count].to_vec()),
                )
                .await?;
                offset = offset
                    .checked_add(count as u64)
                    .ok_or_else(|| failure(i18n_key::CONNECTIONS_THE_FILE_IS_TOO_LARGE))?;
                if let Some(Checkpoint::Upload {
                    offset: confirmed, ..
                }) = &mut checkpoint
                {
                    *confirmed = offset;
                }
                set_progress(state, Some(base + offset), None, None);
            }
            if LocalIdentity::new(&input.metadata().await?) != identity {
                return Err(failure(
                    i18n_key::SFTP_THE_SOURCE_CHANGED_WHILE_IT_WAS_BEING_UPLOADED_RESTART_THE,
                ));
            }
            bounded(stop, REQUEST_TIMEOUT, raw.close(handle)).await?;
            let replace = self.policy == SftpConflictPolicy::Overwrite;
            if !replace {
                bounded(
                    &state.cancel,
                    REQUEST_TIMEOUT,
                    ensure_absent(raw, &job.remote),
                )
                .await?;
            }
            bounded(
                stop,
                REQUEST_TIMEOUT,
                commit_remote(raw, &temporary, &job.remote, replace),
            )
            .await?;
            checkpoint = None;
            Ok(offset)
        }
        .await;
        (result, checkpoint)
    }

    async fn download_file(
        &self,
        raw: &RawSftpSession,
        state: &TransferState,
        job: &Job,
        previous: Option<Checkpoint>,
        base: u64,
    ) -> (Result<u64, SftpError>, Option<Checkpoint>) {
        let mut checkpoint = previous;
        let result = async {
            let attrs = bounded(
                &state.cancel,
                REQUEST_TIMEOUT,
                raw.lstat(job.remote.clone()),
            )
            .await?
            .attrs;
            regular_or_directory(&attrs)?;
            if attrs.is_dir() {
                return Err(failure(
                    i18n_key::SFTP_A_TRANSFER_SOURCE_CHANGED_TYPE_RESTART_THE_TRANSFER,
                ));
            }
            let handle = bounded(
                &state.cancel,
                REQUEST_TIMEOUT,
                raw.open(job.remote.clone(), OpenFlags::READ, FileAttributes::empty()),
            )
            .await?
            .handle;
            let identity = RemoteIdentity::from(
                &bounded(&state.cancel, REQUEST_TIMEOUT, raw.fstat(handle.clone()))
                    .await?
                    .attrs,
            );
            let offset = match &checkpoint {
                Some(Checkpoint::Download {
                    identity: before,
                    offset,
                    ..
                }) if *before == identity
                    && identity.modified.is_some()
                    && identity.size.is_some_and(|size| *offset <= size) =>
                {
                    *offset
                }
                Some(_) => {
                    return Err(failure(
                        i18n_key::SFTP_THE_SOURCE_CHANGED_OR_CANNOT_BE_VERIFIED_RESTART_THE_TRANSFER,
                    ));
                }
                None => {
                    let parent = job.local.parent().unwrap_or(Path::new("."));
                    let temporary = tempfile::Builder::new()
                        .prefix(".rustterm-download-")
                        .suffix(".part")
                        .tempfile_in(parent)?;
                    checkpoint = Some(Checkpoint::Download {
                        temporary,
                        identity: identity.clone(),
                        offset: 0,
                    });
                    0
                }
            };
            let Some(Checkpoint::Download { temporary, .. }) = &checkpoint else {
                unreachable!()
            };
            let mut output = tokio::fs::File::from_std(temporary.as_file().try_clone()?);
            output.set_len(offset).await?;
            output.seek(io::SeekFrom::Start(offset)).await?;
            let mut offset = offset;
            let copied = async {
                loop {
                    let bytes = bounded(
                        &state.cancel,
                        REQUEST_TIMEOUT,
                        read_chunk(raw, &handle, offset, CHUNK as u32),
                    )
                    .await?;
                    let Some(bytes) = bytes else { break };
                    bounded(&state.cancel, REQUEST_TIMEOUT, output.write_all(&bytes)).await?;
                    offset += bytes.len() as u64;
                    if let Some(Checkpoint::Download {
                        offset: confirmed, ..
                    }) = &mut checkpoint
                    {
                        *confirmed = offset;
                    }
                    set_progress(state, Some(base + offset), None, None);
                }
                bounded(&state.cancel, REQUEST_TIMEOUT, output.flush()).await?;
                bounded(&state.cancel, REQUEST_TIMEOUT, output.sync_all()).await?;
                let after = raw.fstat(handle.clone()).await?.attrs;
                if RemoteIdentity::from(&after) != identity {
                    return Err(failure(
                        i18n_key::SFTP_THE_REMOTE_FILE_CHANGED_WHILE_DOWNLOADING_RESTART_THE_TRANSFER,
                    ));
                }
                bounded(&state.cancel, REQUEST_TIMEOUT, raw.close(handle)).await?;
                Ok::<_, SftpError>(())
            }
            .await;
            if copied.is_err() {
                let _ = tokio::time::timeout(CLEANUP_TIMEOUT, output.flush()).await;
            }
            drop(output);
            copied?;
            if state.cancel.is_cancelled() {
                return Err(SftpError::Cancelled);
            }
            let Some(Checkpoint::Download { temporary, .. }) = checkpoint.take() else {
                unreachable!()
            };
            let target = job.local.clone();
            let replace = self.policy == SftpConflictPolicy::Overwrite;
            let committed = tokio::task::spawn_blocking(move || {
                if replace {
                    if let Ok(attrs) = std::fs::symlink_metadata(&target)
                        && (!attrs.is_file() || attrs.file_type().is_symlink())
                    {
                        return Err(failure(
                            i18n_key::SFTP_OVERWRITE_CANNOT_REPLACE_FOLDERS_SYMBOLIC_LINKS_OR_SPECIAL_FILES,
                        ));
                    }
                    temporary
                        .persist(target)
                        .map_err(|e| SftpError::Local(e.error))?;
                } else {
                    temporary
                        .persist_noclobber(target)
                        .map_err(|e| SftpError::Local(e.error))?;
                }
                Ok::<_, SftpError>(())
            })
            .await
            .map_err(|e| SftpError::Remote(e.to_string()))?;
            committed?;
            Ok(offset)
        }
        .await;
        (result, checkpoint)
    }

    pub(super) fn cleanup_paths(&self) -> Vec<String> {
        self.checkpoints
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .values()
            .filter_map(|checkpoint| match checkpoint {
                Checkpoint::Upload { temporary, .. } => Some(temporary.clone()),
                _ => None,
            })
            .collect()
    }
}

async fn remote_metadata(
    raw: &RawSftpSession,
    path: &str,
) -> Result<Option<FileAttributes>, SftpError> {
    match raw.lstat(path).await {
        Ok(attrs) => Ok(Some(attrs.attrs)),
        Err(ProtocolError::Status(status)) if status.status_code == StatusCode::NoSuchFile => {
            Ok(None)
        }
        Err(error) => Err(error.into()),
    }
}

impl Drop for TransferPlan {
    fn drop(&mut self) {
        let paths = self.cleanup_paths();
        let Some(client) = self
            .client
            .upgrade()
            .filter(|client| !client.stop.is_cancelled())
        else {
            return;
        };
        if paths.is_empty() {
            return;
        }
        if let Ok(runtime) = crate::session::runtime() {
            runtime.spawn(async move {
                let cancel = client.stop.child_token();
                if let Ok(mut connection) = Connection::open(&client, &cancel).await {
                    for path in paths {
                        let _ =
                            bounded(&cancel, CLEANUP_TIMEOUT, connection.raw.remove(path)).await;
                    }
                    connection.shutdown().await;
                }
            });
        }
    }
}
fn regular_or_directory(attrs: &FileAttributes) -> Result<(), SftpError> {
    if attrs.is_symlink()
        || attrs.permissions.is_some_and(|mode| {
            mode & 0o170000 != 0 && mode & 0o170000 != 0o100000 && mode & 0o170000 != 0o040000
        })
    {
        Err(failure(
            i18n_key::SFTP_DIRECTORY_TRANSFERS_DO_NOT_FOLLOW_SYMBOLIC_LINKS_OR_SPECIAL_FILES,
        ))
    } else {
        Ok(())
    }
}
fn renamed(path: &str, index: usize) -> String {
    format!("{path} ({index})")
}
async fn unique_remote(raw: &RawSftpSession, path: &str) -> Result<String, SftpError> {
    for index in 1..=1000 {
        let candidate = renamed(path, index);
        validate_target(&candidate)?;
        if remote_metadata(raw, &candidate).await?.is_none() {
            return Ok(candidate);
        }
    }
    Err(failure(
        i18n_key::SFTP_TOO_MANY_CONFLICTING_FILENAMES_CHOOSE_ANOTHER_DESTINATION,
    ))
}
async fn unique_local(path: &Path) -> Result<PathBuf, SftpError> {
    let name = path
        .file_name()
        .ok_or_else(|| failure(i18n_key::CONNECTIONS_CHOOSE_A_COMPLETE_LOCAL_FILENAME))?
        .to_string_lossy();
    for index in 1..=1000 {
        let candidate = path.with_file_name(renamed(&name, index));
        match tokio::fs::symlink_metadata(&candidate).await {
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(candidate),
            Err(error) => return Err(error.into()),
            Ok(_) => {}
        }
    }
    Err(failure(
        i18n_key::SFTP_TOO_MANY_CONFLICTING_FILENAMES_CHOOSE_ANOTHER_DESTINATION,
    ))
}

async fn read_chunk(
    raw: &RawSftpSession,
    handle: &str,
    offset: u64,
    length: u32,
) -> Result<Option<Vec<u8>>, SftpError> {
    match raw.read(handle, offset, length).await {
        Ok(data) if !data.data.is_empty() && data.data.len() <= length as usize => {
            Ok(Some(data.data))
        }
        Ok(_) => Err(failure(
            i18n_key::CONNECTIONS_THE_SERVER_RETURNED_AN_INVALID_FILE_DATA_LENGTH,
        )),
        Err(ProtocolError::Status(status)) if status.status_code == StatusCode::Eof => Ok(None),
        Err(error) => Err(error.into()),
    }
}
async fn commit_remote(
    raw: &RawSftpSession,
    source: &str,
    target: &str,
    replace: bool,
) -> Result<(), SftpError> {
    if !replace {
        raw.rename(source, target).await?;
        return Ok(());
    }
    if let Some(attrs) = remote_metadata(raw, target).await? {
        regular_or_directory(&attrs)?;
        if attrs.is_dir() {
            return Err(failure(
                i18n_key::SFTP_OVERWRITE_CANNOT_REPLACE_FOLDERS_SYMBOLIC_LINKS_OR_SPECIAL_FILES,
            ));
        }
    }
    atomic_rename(raw, source, target)
        .await
        .map_err(rename_error)
}

async fn atomic_rename(
    raw: &RawSftpSession,
    source: &str,
    target: &str,
) -> Result<(), ProtocolError> {
    // The OpenSSH extension is atomic. Never remove the original as a fallback.
    let mut payload = Vec::new();
    for name in [source, target] {
        payload.extend_from_slice(&(name.len() as u32).to_be_bytes());
        payload.extend_from_slice(name.as_bytes());
    }
    match raw.extended("posix-rename@openssh.com", payload).await? {
        russh_sftp::protocol::Packet::Status(status) if status.status_code == StatusCode::Ok => {
            Ok(())
        }
        russh_sftp::protocol::Packet::Status(status) => Err(ProtocolError::Status(status)),
        _ => Err(ProtocolError::UnexpectedPacket),
    }
}

fn rename_error(error: ProtocolError) -> SftpError {
    match error {
        ProtocolError::Status(status) if status.status_code == StatusCode::OpUnsupported => {
            failure(
                i18n_key::SFTP_THIS_SERVER_DOES_NOT_SUPPORT_ATOMIC_OVERWRITE_CHOOSE_RENAME_INSTEAD,
            )
        }
        ProtocolError::UnexpectedPacket => {
            failure(i18n_key::SFTP_THE_SERVER_RETURNED_AN_INVALID_RENAME_RESPONSE)
        }
        error => error.into(),
    }
}

pub(super) async fn read_text(
    raw: &RawSftpSession,
    path: String,
) -> Result<SftpTextDocument, SftpError> {
    let attrs = raw.lstat(path.clone()).await?.attrs;
    regular_or_directory(&attrs)?;
    if attrs.is_dir() || attrs.size.is_some_and(|size| size > MAX_TEXT_BYTES as u64) {
        return Err(failure(
            i18n_key::SFTP_PREVIEW_SUPPORTS_UTF_8_TEXT_FILES_UP_TO_1_MIB,
        ));
    }
    let handle = raw
        .open(path.clone(), OpenFlags::READ, FileAttributes::empty())
        .await?
        .handle;
    let mut data = Vec::new();
    while let Some(chunk) = read_chunk(raw, &handle, data.len() as u64, CHUNK as u32).await? {
        if data.len() + chunk.len() > MAX_TEXT_BYTES {
            return Err(failure(
                i18n_key::SFTP_PREVIEW_SUPPORTS_UTF_8_TEXT_FILES_UP_TO_1_MIB,
            ));
        }
        data.extend(chunk);
    }
    raw.close(handle).await?;
    if data.contains(&0) {
        return Err(failure(
            i18n_key::SFTP_PREVIEW_SUPPORTS_UTF_8_TEXT_FILES_UP_TO_1_MIB,
        ));
    }
    let digest = Sha256::digest(&data).into();
    let text = String::from_utf8(data)
        .map_err(|_| failure(i18n_key::SFTP_PREVIEW_SUPPORTS_UTF_8_TEXT_FILES_UP_TO_1_MIB))?;
    Ok(SftpTextDocument {
        path,
        text,
        digest,
        permissions: attrs.permissions.map(|mode| mode & 0o777),
    })
}

pub(super) async fn save_text(
    raw: &RawSftpSession,
    stop: &CancellationToken,
    cancel: &CancellationToken,
    original: SftpTextDocument,
    content: String,
) -> Result<SftpTextDocument, SftpError> {
    if content.len() > MAX_TEXT_BYTES || content.contains('\0') {
        return Err(failure(
            i18n_key::SFTP_PREVIEW_SUPPORTS_UTF_8_TEXT_FILES_UP_TO_1_MIB,
        ));
    }
    let deadline = tokio::time::Instant::now() + OPERATION_TIMEOUT;
    if bounded(
        cancel,
        OPERATION_TIMEOUT,
        read_text(raw, original.path.clone()),
    )
    .await?
    .digest
        != original.digest
    {
        return Err(failure(
            i18n_key::SFTP_THE_REMOTE_FILE_CHANGED_RELOAD_IT_BEFORE_UPLOADING_YOUR_EDITS,
        ));
    }
    if cancel.is_cancelled() || stop.is_cancelled() {
        return Err(SftpError::Cancelled);
    }
    if tokio::time::Instant::now() >= deadline {
        return Err(SftpError::Timeout);
    }
    let temporary = remote_temporary(&original.path)?;
    // A delayed OPEN can create the file after the UI cancels. Await its
    // bounded acknowledgement before deciding whether cleanup is needed.
    let mut create_started = false;
    let mut create_rejected = false;
    let opened = bounded(stop, REQUEST_TIMEOUT, async {
        create_started = true;
        let result = raw
            .open(
                temporary.clone(),
                OpenFlags::WRITE | OpenFlags::CREATE | OpenFlags::EXCLUDE,
                FileAttributes {
                    permissions: Some(0o600),
                    ..FileAttributes::empty()
                },
            )
            .await;
        create_rejected = matches!(
            &result,
            Err(ProtocolError::Status(_) | ProtocolError::Limited(_))
        );
        result
    })
    .await
    .map_err(|error| {
        if !create_started || create_rejected {
            return error;
        }
        SftpError::Remote(crate::trf!(
            i18n_key::CONNECTIONS_ERROR_TEMPORARY_FILE_CREATION_WAS_NOT_CONFIRMED_CHECK_TEMPORARY,
            error = error.to_string(),
            temporary = temporary
        ))
    })?;
    let handle = opened.handle;
    let mut handle_open = true;
    let result = async {
        let remaining = deadline
            .checked_duration_since(tokio::time::Instant::now())
            .ok_or(SftpError::Timeout)?;
        bounded(cancel, remaining, async {
            for (index, chunk) in content.as_bytes().chunks(CHUNK).enumerate() {
                raw.write(handle.clone(), (index * CHUNK) as u64, chunk.to_vec())
                    .await?;
            }
            if let Some(permissions) = original.permissions {
                raw.fsetstat(
                    handle.clone(),
                    FileAttributes {
                        permissions: Some(permissions),
                        ..FileAttributes::empty()
                    },
                )
                .await?;
            }
            raw.close(handle.clone()).await?;
            handle_open = false;
            if read_text(raw, original.path.clone()).await?.digest != original.digest {
                return Err(failure(
                    i18n_key::SFTP_THE_REMOTE_FILE_CHANGED_RELOAD_IT_BEFORE_UPLOADING_YOUR_EDITS,
                ));
            }
            Ok::<_, SftpError>(())
        })
        .await?;
        if cancel.is_cancelled() || stop.is_cancelled() {
            return Err(SftpError::Cancelled);
        }
        // Once commit starts, UI cancellation must not turn an acknowledged
        // save into a failure. A disconnect or timeout can still be ambiguous.
        let mut commit_started = false;
        let mut commit_rejected = false;
        bounded(stop, REQUEST_TIMEOUT, async {
            commit_started = true;
            let result = atomic_rename(raw, &temporary, &original.path).await;
            commit_rejected = matches!(
                &result,
                Err(ProtocolError::Status(_) | ProtocolError::Limited(_))
            );
            result.map_err(rename_error)
        })
        .await
        .map_err(|error| {
            if !commit_started || commit_rejected {
                return error;
            }
            SftpError::Remote(crate::trf!(
                i18n_key::CONNECTIONS_ERROR_VERIFY_THE_REMOTE_FILE_BEFORE_RETRYING_REMOTE,
                error = error.to_string(),
                remote = original.path
            ))
        })
    }
    .await;
    if let Err(error) = result {
        if handle_open {
            let _ = bounded(stop, CLEANUP_TIMEOUT, raw.close(handle)).await;
        }
        let cleanup = bounded(stop, CLEANUP_TIMEOUT, async {
            match raw.remove(temporary.clone()).await {
                Ok(_) => Ok(()),
                Err(ProtocolError::Status(status))
                    if status.status_code == StatusCode::NoSuchFile =>
                {
                    Ok(())
                }
                Err(error) => Err(error),
            }
        })
        .await;
        if let Err(cleanup) = cleanup {
            return Err(SftpError::Remote(crate::trf!(
                i18n_key::CONNECTIONS_ERROR_TEMPORARY_FILE_CLEANUP_WAS_NOT_CONFIRMED_TEMPORARY_CLEANUP_CHECK,
                error = error.to_string(),
                temporary = temporary,
                cleanup = cleanup.to_string(),
                remote = original.path
            )));
        }
        return Err(error);
    }
    Ok(SftpTextDocument {
        path: original.path,
        digest: Sha256::digest(content.as_bytes()).into(),
        text: content,
        permissions: original.permissions,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    #[cfg(unix)]
    #[test]
    fn local_identity_changes_with_source_replacement() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("source");
        std::fs::write(&path, b"abc").unwrap();
        let before = LocalIdentity::new(&std::fs::metadata(&path).unwrap());
        std::fs::remove_file(&path).unwrap();
        std::fs::write(&path, b"changed").unwrap();
        assert_ne!(
            before,
            LocalIdentity::new(&std::fs::metadata(path).unwrap())
        );
    }
    #[tokio::test]
    async fn renamed_download_never_reuses_a_symlink_destination() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("report");
        std::fs::write(&path, b"keep").unwrap();
        #[cfg(unix)]
        std::os::unix::fs::symlink(&path, directory.path().join("report (1)")).unwrap();
        #[cfg(not(unix))]
        std::fs::write(directory.path().join("report (1)"), b"keep too").unwrap();
        assert_eq!(
            unique_local(&path).await.unwrap(),
            directory.path().join("report (2)")
        );
        assert_eq!(std::fs::read(path).unwrap(), b"keep");
    }
}
