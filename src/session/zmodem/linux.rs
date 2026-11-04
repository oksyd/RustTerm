use crate::i18n::messages as i18n_key;

use super::{
    CANCEL_BYTES, CHUNK_BYTES, Direction, Event, FinalHeaderScanner, FinishGate, IO_TIMEOUT,
    InputRoute, Outcome, Progress, Request, Selection,
};
use anyhow::{Context as _, Result, anyhow, bail};
use std::collections::HashSet;
use std::ffi::{CString, OsStr, OsString};
use std::fs::File;
use std::io::{Read, Write};
use std::os::fd::{AsRawFd, FromRawFd, RawFd};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::{Component, Path};
use std::process::Stdio;
use std::{
    path::PathBuf,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};

const MAX_FILES: usize = 64;
const MAX_BATCH_BYTES: u64 = 4 * 1024 * 1024 * 1024;
const MAX_FILE_BYTES: u64 = u32::MAX as u64;
const IDLE_TIMEOUT: Duration = Duration::from_secs(120);

pub(super) struct Worker {
    pub input: InputRoute,
    pub events: mpsc::Sender<Event>,
    pub cancel: CancellationToken,
    pub progress: Arc<Mutex<Progress>>,
    pub finished: Arc<AtomicBool>,
    pub finalized: Arc<AtomicBool>,
}

pub(super) fn find_helper(direction: Direction) -> Result<PathBuf> {
    let name = match direction {
        Direction::Download => "rz",
        Direction::Upload => "sz",
    };
    for directory in ["/usr/bin", "/bin", "/usr/local/bin"] {
        let path = Path::new(directory).join(name);
        if std::fs::metadata(&path)
            .is_ok_and(|metadata| metadata.is_file() && metadata.permissions().mode() & 0o111 != 0)
        {
            return Ok(path);
        }
    }
    bail!(crate::trf!(
        i18n_key::CONNECTIONS_THE_LRZSZ_PROGRAM_NAME_WAS_NOT_FOUND_RUN_SUDO_APT,
        name = name
    ))
}

struct Prepared {
    // Drop the temporary directory while its /proc/self/fd parent still exists.
    _stage: tempfile::TempDir,
    directory: File,
    destination: Option<File>,
    destination_path: Option<PathBuf>,
    source_paths: Vec<PathBuf>,
    names: Vec<OsString>,
    total: u64,
}

impl Prepared {
    fn create(selection: Selection, cancel: &CancellationToken) -> Result<Self> {
        match selection {
            Selection::DownloadDirectory(path) => {
                let destination = open_directory(&path)?;
                let parent = fd_path(destination.as_raw_fd());
                let stage = tempfile::Builder::new()
                    .prefix(".rustterm-zmodem-")
                    .permissions(std::fs::Permissions::from_mode(0o700))
                    .tempdir_in(parent)?;
                let directory = open_child_directory(
                    destination.as_raw_fd(),
                    stage.path().file_name().context(crate::i18n::tr(
                        i18n_key::CONNECTIONS_THE_TEMPORARY_DIRECTORY_NAME_IS_MISSING,
                    ))?,
                )?;
                Ok(Self {
                    _stage: stage,
                    directory,
                    destination: Some(destination),
                    destination_path: Some(path),
                    source_paths: Vec::new(),
                    names: Vec::new(),
                    total: 0,
                })
            }
            Selection::UploadFiles(paths) => {
                if paths.is_empty() || paths.len() > MAX_FILES {
                    bail!(crate::trf!(
                        i18n_key::CONNECTIONS_SELECT_1_TO_LIMIT_FILES_PER_ZMODEM_BATCH,
                        limit = MAX_FILES
                    ));
                }
                let stage = tempfile::Builder::new()
                    .prefix("rustterm-zmodem-")
                    .permissions(std::fs::Permissions::from_mode(0o700))
                    .tempdir()?;
                let directory = open_directory(stage.path())?;
                let mut prepared = Self {
                    _stage: stage,
                    directory,
                    destination: None,
                    destination_path: None,
                    source_paths: paths,
                    names: Vec::new(),
                    total: 0,
                };
                let mut seen = HashSet::new();
                for source in &prepared.source_paths {
                    check_cancel(cancel)?;
                    let name = source.file_name().context(crate::i18n::tr(
                        i18n_key::CONNECTIONS_AN_UPLOAD_FILE_HAS_NO_FILENAME,
                    ))?;
                    validate_name(name)?;
                    if !seen.insert(name.to_owned()) {
                        bail!(crate::i18n::tr(
                            i18n_key::CONNECTIONS_AN_UPLOAD_BATCH_CANNOT_CONTAIN_DUPLICATE_FILENAMES
                        ));
                    }
                    let parent = open_directory(source.parent().context(crate::i18n::tr(
                        i18n_key::CONNECTIONS_THE_UPLOAD_PATH_HAS_NO_PARENT_DIRECTORY,
                    ))?)?;
                    let mut input = open_regular(parent.as_raw_fd(), name)?;
                    let size = input.metadata()?.len();
                    if size > MAX_FILE_BYTES {
                        bail!(crate::i18n::tr(
                            i18n_key::CONNECTIONS_EACH_ZMODEM_FILE_MUST_BE_SMALLER_THAN_4_GIB
                        ));
                    }
                    prepared.total = prepared
                        .total
                        .checked_add(size)
                        .context(crate::i18n::tr(i18n_key::CONNECTIONS_FILE_SIZE_OVERFLOW))?;
                    if prepared.total > MAX_BATCH_BYTES {
                        bail!(crate::i18n::tr(
                            i18n_key::CONNECTIONS_A_ZMODEM_BATCH_CANNOT_EXCEED_4_GIB
                        ));
                    }
                    let mut output = create_file(prepared.directory.as_raw_fd(), name)?;
                    let mut buffer = [0_u8; CHUNK_BYTES];
                    let mut copied = 0_u64;
                    loop {
                        check_cancel(cancel)?;
                        let count = input.read(&mut buffer)?;
                        if count == 0 {
                            break;
                        }
                        copied += count as u64;
                        if copied > size {
                            bail!(crate::i18n::tr(
                                i18n_key::CONNECTIONS_THE_UPLOAD_FILE_GREW_DURING_PREPARATION_PLEASE_RETRY
                            ));
                        }
                        output.write_all(&buffer[..count])?;
                    }
                    if copied != size {
                        bail!(crate::i18n::tr(
                            i18n_key::CONNECTIONS_THE_UPLOAD_FILE_CHANGED_DURING_PREPARATION_PLEASE_RETRY
                        ));
                    }
                    prepared.names.push(name.to_owned());
                }
                Ok(prepared)
            }
        }
    }

    fn entries(&self) -> Result<(Vec<OsString>, u64)> {
        let mut names = Vec::new();
        let mut total = 0_u64;
        for entry in std::fs::read_dir(fd_path(self.directory.as_raw_fd()))? {
            let entry = entry?;
            let name = entry.file_name();
            validate_name(&name)?;
            let file = open_regular(self.directory.as_raw_fd(), &name)?;
            let size = file.metadata()?.len();
            if size > MAX_FILE_BYTES {
                bail!(crate::i18n::tr(
                    i18n_key::CONNECTIONS_EACH_ZMODEM_FILE_MUST_BE_SMALLER_THAN_4_GIB
                ));
            }
            total = total
                .checked_add(size)
                .context(crate::i18n::tr(i18n_key::CONNECTIONS_FILE_SIZE_OVERFLOW))?;
            names.push(name);
            if names.len() > MAX_FILES || total > MAX_BATCH_BYTES {
                bail!(crate::i18n::tr(
                    i18n_key::CONNECTIONS_ZMODEM_RECEPTION_EXCEEDED_THE_LIMIT_OF_64_FILES_OR_4
                ));
            }
        }
        names.sort();
        Ok((names, total))
    }

    fn publish(&self, cancel: &CancellationToken) -> (Vec<PathBuf>, Result<u64>) {
        let mut files = Vec::new();
        let result = (|| {
            let destination = self.destination.as_ref().context(crate::i18n::tr(
                i18n_key::CONNECTIONS_THE_DOWNLOAD_DIRECTORY_IS_MISSING,
            ))?;
            let path = self.destination_path.as_ref().context(crate::i18n::tr(
                i18n_key::CONNECTIONS_THE_DOWNLOAD_PATH_IS_MISSING,
            ))?;
            let (names, total) = self.entries()?;
            if names.is_empty() {
                bail!(crate::i18n::tr(
                    i18n_key::CONNECTIONS_THE_REMOTE_HOST_DID_NOT_SEND_ANY_FILES
                ));
            }
            for name in names {
                check_cancel(cancel)?;
                let file = open_regular(self.directory.as_raw_fd(), &name)?;
                // Never propagate executable/setuid bits supplied by the remote peer.
                file.set_permissions(std::fs::Permissions::from_mode(0o600))?;
                file.sync_all()?;
                publish_no_replace(self.directory.as_raw_fd(), destination.as_raw_fd(), &name)
                    .with_context(|| {
                        crate::trf!(
                            i18n_key::CONNECTIONS_CANNOT_SAVE_NAME_EXISTING_FILES_WILL_NOT_BE_OVERWRITTEN,
                            name = name.to_string_lossy()
                        )
                    })?;
                files.push(path.join(&name));
            }
            destination.sync_all()?;
            Ok(total)
        })();
        (files, result)
    }
}

fn check_cancel(cancel: &CancellationToken) -> Result<()> {
    if cancel.is_cancelled() {
        bail!(crate::i18n::tr(i18n_key::CONNECTIONS_ZMODEM_CANCELLED));
    }
    Ok(())
}

fn fd_path(fd: RawFd) -> PathBuf {
    PathBuf::from(format!("/proc/self/fd/{fd}"))
}

fn c_name(name: &OsStr) -> Result<CString> {
    CString::new(name.as_bytes()).context(crate::i18n::tr(
        i18n_key::CONNECTIONS_THE_FILENAME_CONTAINS_A_NUL_BYTE,
    ))
}

fn validate_name(name: &OsStr) -> Result<()> {
    let bytes = name.as_bytes();
    if bytes.is_empty()
        || bytes.len() > 255
        || bytes[0] == b'.'
        || bytes == b"-"
        || bytes.contains(&b'/')
        || bytes.contains(&b'\\')
        || bytes.iter().any(|byte| *byte < 0x20 || *byte == 0x7f)
    {
        bail!(crate::i18n::tr(
            i18n_key::CONNECTIONS_UNSAFE_OR_UNSUPPORTED_ZMODEM_FILENAME
        ));
    }
    Ok(())
}

/// Pin each parent directory without following user-controlled symlinks.
fn open_directory(path: &Path) -> Result<File> {
    if !path.is_absolute() {
        bail!(crate::i18n::tr(
            i18n_key::CONNECTIONS_FILE_SELECTIONS_MUST_USE_ABSOLUTE_PATHS
        ));
    }
    let root = c"/";
    // SAFETY: valid C path and flags; ownership is transferred once below.
    let root_fd = unsafe {
        libc::open(
            root.as_ptr(),
            libc::O_RDONLY | libc::O_DIRECTORY | libc::O_CLOEXEC,
        )
    };
    if root_fd < 0 {
        return Err(std::io::Error::last_os_error().into());
    }
    // SAFETY: open returned a new owned descriptor.
    let mut directory = unsafe { File::from_raw_fd(root_fd) };
    for component in path.components() {
        let name = match component {
            Component::RootDir | Component::CurDir => continue,
            Component::Normal(name) => name,
            _ => bail!(crate::i18n::tr(
                i18n_key::CONNECTIONS_FILE_PATHS_CANNOT_CONTAIN_PARENT_DIRECTORY_COMPONENTS
            )),
        };
        let name = c_name(name)?;
        // SAFETY: parent descriptor is live and name is NUL terminated.
        let fd = unsafe {
            libc::openat(
                directory.as_raw_fd(),
                name.as_ptr(),
                libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
            )
        };
        if fd < 0 {
            return Err(std::io::Error::last_os_error()).context(crate::i18n::tr(
                i18n_key::CONNECTIONS_CANNOT_SAFELY_OPEN_THE_DIRECTORY_WITHOUT_FOLLOWING_SYMBOLIC_LINKS,
            ));
        }
        // SAFETY: openat returned a new owned descriptor.
        directory = unsafe { File::from_raw_fd(fd) };
    }
    Ok(directory)
}

fn open_regular(directory: RawFd, name: &OsStr) -> Result<File> {
    let name = c_name(name)?;
    // O_NONBLOCK avoids hanging on an unexpected FIFO before fstat.
    // SAFETY: live directory descriptor and valid C string.
    let fd = unsafe {
        libc::openat(
            directory,
            name.as_ptr(),
            libc::O_RDONLY | libc::O_NOFOLLOW | libc::O_NONBLOCK | libc::O_CLOEXEC,
        )
    };
    if fd < 0 {
        return Err(std::io::Error::last_os_error().into());
    }
    // SAFETY: openat returned a new owned descriptor.
    let file = unsafe { File::from_raw_fd(fd) };
    let metadata = file.metadata()?;
    if !metadata.is_file() || metadata.nlink() != 1 {
        bail!(crate::i18n::tr(
            i18n_key::CONNECTIONS_ZMODEM_ACCEPTS_ONLY_REGULAR_FILES_NOT_SYMBOLIC_LINKS_OR_HARD
        ));
    }
    Ok(file)
}

fn open_child_directory(parent: RawFd, name: &OsStr) -> Result<File> {
    let name = c_name(name)?;
    // SAFETY: the parent descriptor is live and the name is NUL terminated.
    let fd = unsafe {
        libc::openat(
            parent,
            name.as_ptr(),
            libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
        )
    };
    if fd < 0 {
        return Err(std::io::Error::last_os_error().into());
    }
    // SAFETY: openat returned a new owned descriptor.
    Ok(unsafe { File::from_raw_fd(fd) })
}

fn create_file(directory: RawFd, name: &OsStr) -> Result<File> {
    let name = c_name(name)?;
    // SAFETY: live directory descriptor, valid C string, creation mode provided.
    let fd = unsafe {
        libc::openat(
            directory,
            name.as_ptr(),
            libc::O_WRONLY | libc::O_CREAT | libc::O_EXCL | libc::O_NOFOLLOW | libc::O_CLOEXEC,
            0o600,
        )
    };
    if fd < 0 {
        return Err(std::io::Error::last_os_error().into());
    }
    // SAFETY: openat returned a new owned descriptor.
    Ok(unsafe { File::from_raw_fd(fd) })
}

fn publish_no_replace(source: RawFd, destination: RawFd, name: &OsStr) -> Result<()> {
    let name = c_name(name)?;
    // SAFETY: both directory descriptors remain live; both names are valid C strings.
    let result = unsafe {
        libc::renameat2(
            source,
            name.as_ptr(),
            destination,
            name.as_ptr(),
            libc::RENAME_NOREPLACE,
        )
    };
    if result == 0 {
        return Ok(());
    }
    let error = std::io::Error::last_os_error();
    // Older filesystems may not implement RENAME_NOREPLACE. linkat is also
    // atomic and refuses an existing destination, including a symlink.
    if !matches!(
        error.raw_os_error(),
        Some(libc::EINVAL | libc::ENOSYS | libc::EOPNOTSUPP)
    ) {
        return Err(error.into());
    }
    // SAFETY: the same live descriptors and valid names as above.
    if unsafe { libc::linkat(source, name.as_ptr(), destination, name.as_ptr(), 0) } != 0 {
        return Err(std::io::Error::last_os_error().into());
    }
    // SAFETY: remove only the original private staging name after publication.
    if unsafe { libc::unlinkat(source, name.as_ptr(), 0) } != 0 {
        return Err(std::io::Error::last_os_error().into());
    }
    Ok(())
}

impl Worker {
    pub(super) async fn run(
        self,
        helper: PathBuf,
        request: Request,
        selection: Selection,
        receiver: mpsc::Receiver<Vec<u8>>,
    ) {
        let cancel = self.cancel.clone();
        let prepared =
            tokio::task::spawn_blocking(move || Prepared::create(selection, &cancel)).await;
        let mut outcome = match prepared {
            Ok(Ok(prepared)) => self.transfer(helper, request, prepared, receiver).await,
            Ok(Err(error)) => failed(error, self.cancel.is_cancelled(), Vec::new()),
            Err(error) => failed(error.into(), self.cancel.is_cancelled(), Vec::new()),
        };
        let mut route = self.input.lock().await;
        route.sender.take();
        route.finalized = true;
        self.finalized.store(true, Ordering::Release);
        if self.cancel.is_cancelled() && !outcome.success {
            outcome.cancelled = true;
            outcome.message = crate::i18n::tr(
                i18n_key::CONNECTIONS_ZMODEM_CANCELLED_UNFINISHED_TEMPORARY_FILES_WERE_CLEANED_UP,
            )
            .into();
            if !outcome.files.is_empty() {
                outcome.message.push_str(&crate::trf!(
                    i18n_key::TERMINAL_COMPLETED_FILES_RETAINED,
                    count = outcome.files.len()
                ));
            }
        }
        if !outcome.success && !self.finished.load(Ordering::Acquire) {
            let _ = self.events.send(Event::Write(CANCEL_BYTES.to_vec())).await;
        }
        // All accepted input has drained. Holding the routing mutex makes
        // this the final event; a concurrent feed instead returns its bytes.
        let _ = self.events.send(Event::Finished(outcome)).await;
    }

    async fn send(&self, event: Event) -> Result<()> {
        tokio::select! {
            biased;
            _ = self.cancel.cancelled() => bail!(crate::i18n::tr(i18n_key::CONNECTIONS_ZMODEM_CANCELLED)),
            result = tokio::time::timeout(IO_TIMEOUT, self.events.send(event)) => {
                result.context(crate::i18n::tr(i18n_key::CONNECTIONS_ZMODEM_OUTPUT_TIMED_OUT))?.context(crate::i18n::tr(i18n_key::CONNECTIONS_THE_ZMODEM_EVENT_CHANNEL_IS_CLOSED))
            }
        }
    }

    async fn transfer(
        &self,
        helper: PathBuf,
        request: Request,
        prepared: Prepared,
        receiver: mpsc::Receiver<Vec<u8>>,
    ) -> Outcome {
        let mut files = Vec::new();
        let result = self
            .transfer_inner(helper, &request, &prepared, receiver)
            .await;
        let mut result = match result {
            Ok(()) if request.direction == Direction::Download => {
                let cancel = self.cancel.clone();
                // fsync can block on a slow target filesystem; keep it off the async executor.
                match tokio::task::spawn_blocking(move || {
                    let result = prepared.publish(&cancel);
                    drop(prepared);
                    result
                })
                .await
                {
                    Ok((published, result)) => {
                        files = published;
                        result.map(|_| ())
                    }
                    Err(error) => Err(error.into()),
                }
            }
            Ok(()) => {
                files = prepared.source_paths.clone();
                drop(prepared);
                Ok(())
            }
            Err(error) => {
                drop(prepared);
                Err(error)
            }
        };
        if self.cancel.is_cancelled() && result.is_ok() {
            result = Err(anyhow!(crate::i18n::tr(
                i18n_key::CONNECTIONS_ZMODEM_CANCELLED
            )));
        }
        match result {
            Ok(()) => Outcome {
                success: true,
                cancelled: false,
                message: crate::trf!(
                    i18n_key::TERMINAL_ZMODEM_COMPLETED_FILES,
                    count = files.len()
                ),
                files,
            },
            Err(error) => failed(error, self.cancel.is_cancelled(), files),
        }
    }

    async fn transfer_inner(
        &self,
        helper: PathBuf,
        request: &Request,
        prepared: &Prepared,
        receiver: mpsc::Receiver<Vec<u8>>,
    ) -> Result<()> {
        check_cancel(&self.cancel)?;
        let mut command = tokio::process::Command::new(helper);
        command
            .args([
                "--binary",
                "--protect",
                "--escape",
                "--restricted",
                "--verbose",
                "--timeout",
                "100",
            ])
            .env_clear()
            .env("PATH", "/usr/bin:/bin")
            .env("LC_ALL", "C")
            .env("SHELL", "/bin/rbash")
            .current_dir(fd_path(prepared.directory.as_raw_fd()))
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        if request.direction == Direction::Download {
            // The long option works in 0.12.21rc; its -j shorthand does not.
            // It strips even absolute paths before checkpath/fopen. Never
            // enable --allow-remote-commands, --overwrite, or --append.
            command.arg("--junk-path");
        } else {
            command.args(["--windowsize", "65536", "--"]);
            command.args(&prepared.names);
        }
        // SAFETY: this pre_exec closure uses only async-signal-safe syscalls.
        // RLIMIT_FSIZE also bounds a malicious peer between directory polls.
        unsafe {
            command.pre_exec(|| {
                let limit = libc::rlimit {
                    rlim_cur: MAX_FILE_BYTES,
                    rlim_max: MAX_FILE_BYTES,
                };
                if libc::setrlimit(libc::RLIMIT_FSIZE, &limit) != 0 {
                    return Err(std::io::Error::last_os_error());
                }
                if libc::prctl(libc::PR_SET_NO_NEW_PRIVS, 1, 0, 0, 0) != 0 {
                    return Err(std::io::Error::last_os_error());
                }
                Ok(())
            });
        }
        let mut child = command.spawn().context(crate::i18n::tr(
            i18n_key::CONNECTIONS_CANNOT_START_THE_LRZSZ_PROGRAM,
        ))?;
        let stdin = child.stdin.take().context(crate::i18n::tr(
            i18n_key::CONNECTIONS_LRZSZ_STANDARD_INPUT_IS_MISSING,
        ))?;
        let mut stdout = child.stdout.take().context(crate::i18n::tr(
            i18n_key::CONNECTIONS_LRZSZ_STANDARD_OUTPUT_IS_MISSING,
        ))?;
        let mut stderr = child.stderr.take().context(crate::i18n::tr(
            i18n_key::CONNECTIONS_LRZSZ_STANDARD_ERROR_IS_MISSING,
        ))?;
        let armed = Arc::new(AtomicBool::new(false));
        let finished = self.finished.clone();
        let input_cancel = self.cancel.child_token();
        let mut input_task = tokio::spawn(pump_input(
            stdin,
            receiver,
            request.initial.clone(),
            request.direction,
            armed.clone(),
            finished.clone(),
            self.events.clone(),
            input_cancel.clone(),
        ));
        let mut input_joined = false;
        let mut out_buffer = [0_u8; 8192];
        let mut err_buffer = [0_u8; 2048];
        let mut scanner = FinalHeaderScanner::default();
        let mut report = Reporter::new(request.direction, prepared.total);
        let mut stdout_done = false;
        let mut stderr_done = false;
        let mut exit = None;
        let mut interval = tokio::time::interval(Duration::from_millis(250));
        interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        let mut last_activity = tokio::time::Instant::now();
        let result: Result<()> = async {
            loop {
                if exit.is_some() && stdout_done && stderr_done { break; }
                tokio::select! {
                    biased;
                    _ = self.cancel.cancelled() => bail!(crate::i18n::tr(i18n_key::CONNECTIONS_ZMODEM_CANCELLED)),
                    joined = &mut input_task, if !input_joined => {
                        input_joined = true;
                        joined.context(crate::i18n::tr(i18n_key::CONNECTIONS_THE_ZMODEM_INPUT_TASK_FAILED))??;
                    }
                    status = child.wait(), if exit.is_none() => {
                        exit = Some(status.context(crate::i18n::tr(i18n_key::CONNECTIONS_CANNOT_WAIT_FOR_LRZSZ_TO_EXIT))?);
                        // Stop accepting protocol input; future calls are
                        // terminal bytes. Closing the queue drains prior reads.
                        self.input.lock().await.sender.take();
                    }
                    read = stdout.read(&mut out_buffer), if !stdout_done => {
                        let count = read.context(crate::i18n::tr(i18n_key::CONNECTIONS_FAILED_TO_READ_LRZSZ_OUTPUT))?;
                        if count == 0 { stdout_done = true; continue; }
                        last_activity = tokio::time::Instant::now();
                        if scanner.feed(&out_buffer[..count]) { armed.store(true, Ordering::Release); }
                        self.send(Event::Write(out_buffer[..count].to_vec())).await?;
                    }
                    read = stderr.read(&mut err_buffer), if !stderr_done => {
                        let count = read.context(crate::i18n::tr(i18n_key::CONNECTIONS_FAILED_TO_READ_LRZSZ_PROGRESS))?;
                        if count == 0 { stderr_done = true; report.flush(); continue; }
                        last_activity = tokio::time::Instant::now();
                        report.feed(&err_buffer[..count]);
                    }
                    _ = interval.tick() => {
                        if last_activity.elapsed() > IDLE_TIMEOUT { bail!(crate::i18n::tr(i18n_key::CONNECTIONS_ZMODEM_TIMED_OUT_WAITING_FOR_THE_REMOTE_HOST)); }
                        if request.direction == Direction::Download {
                            let (_, bytes) = prepared.entries()?;
                            report.progress.bytes_transferred = bytes;
                        }
                        let changed = {
                            let mut current = self.progress.lock().unwrap_or_else(|error| error.into_inner());
                            if *current == report.progress { false } else { *current = report.progress.clone(); true }
                        };
                        if changed { self.send(Event::Progress(report.progress.clone())).await?; }
                    }
                }
            }
            if !exit.is_some_and(|status| status.success()) {
                bail!(crate::trf!(i18n_key::CONNECTIONS_LRZSZ_TRANSFER_FAILED_ERROR, error = report.diagnostic()));
            }
            if !finished.load(Ordering::Acquire) {
                bail!(crate::trf!(i18n_key::CONNECTIONS_ZMODEM_DID_NOT_RECEIVE_A_COMPLETE_CLOSING_HANDSHAKE_ERROR, error = report.diagnostic()));
            }
            if report.skipped { bail!(crate::i18n::tr(i18n_key::CONNECTIONS_THE_REMOTE_HOST_SKIPPED_A_FILE_POSSIBLY_BECAUSE_THE_DESTINATION)); }
            Ok(())
        }.await;

        self.input.lock().await.sender.take();
        if result.is_err() {
            input_cancel.cancel();
            let _ = child.start_kill();
            let _ = child.wait().await;
        }
        // Joining the input pump preserves terminal bytes queued in the same
        // read as OO and makes helper destruction deterministic on Drop.
        if !input_joined {
            match tokio::time::timeout(IO_TIMEOUT, &mut input_task).await {
                Ok(joined) if result.is_ok() => {
                    joined.context(crate::i18n::tr(
                        i18n_key::CONNECTIONS_THE_ZMODEM_INPUT_TASK_FAILED,
                    ))??;
                }
                Ok(_) => {}
                Err(_) => {
                    input_cancel.cancel();
                    let _ = input_task.await;
                }
            }
        }
        result?;
        let mut final_progress = report.progress;
        let (count, bytes) = if request.direction == Direction::Download {
            let (names, bytes) = prepared.entries()?;
            (names.len(), bytes)
        } else {
            (prepared.names.len(), prepared.total)
        };
        final_progress.files_completed = count;
        final_progress.bytes_transferred = bytes;
        final_progress.total_bytes = Some(bytes);
        *self
            .progress
            .lock()
            .unwrap_or_else(|error| error.into_inner()) = final_progress.clone();
        self.send(Event::Progress(final_progress)).await
    }
}

fn failed(error: anyhow::Error, cancelled: bool, files: Vec<PathBuf>) -> Outcome {
    let mut message = format!("{error:#}");
    message = message
        .chars()
        .filter(|character| !character.is_control())
        .take(1024)
        .collect();
    if !files.is_empty() {
        message.push_str(crate::i18n::tr(
            i18n_key::CONNECTIONS_COMPLETED_FILES_WERE_RETAINED,
        ));
    }
    Outcome {
        success: false,
        cancelled,
        message,
        files,
    }
}

#[allow(clippy::too_many_arguments)]
async fn pump_input(
    mut stdin: tokio::process::ChildStdin,
    mut receiver: mpsc::Receiver<Vec<u8>>,
    initial: Vec<u8>,
    direction: Direction,
    armed: Arc<AtomicBool>,
    finished: Arc<AtomicBool>,
    events: mpsc::Sender<Event>,
    cancel: CancellationToken,
) -> Result<()> {
    let mut gate = FinishGate::new(direction);
    let mut next = Some(initial);
    loop {
        let bytes = if let Some(bytes) = next.take() {
            bytes
        } else {
            tokio::select! {
                biased;
                _ = cancel.cancelled() => return Ok(()),
                bytes = receiver.recv() => match bytes { Some(bytes) => bytes, None => return Ok(()) },
            }
        };
        let (protocol, terminal) = gate.feed(&bytes, armed.load(Ordering::Acquire));
        if !protocol.is_empty() {
            tokio::select! {
                biased;
                _ = cancel.cancelled() => return Ok(()),
                result = tokio::time::timeout(IO_TIMEOUT, stdin.write_all(&protocol)) => {
                    result.context(crate::i18n::tr(i18n_key::CONNECTIONS_WRITING_TO_LRZSZ_INPUT_TIMED_OUT))?.context(crate::i18n::tr(i18n_key::CONNECTIONS_LRZSZ_INPUT_IS_CLOSED))?;
                }
            }
        }
        if gate.finished {
            finished.store(true, Ordering::Release);
        }
        if !terminal.is_empty() {
            tokio::select! {
                biased;
                _ = cancel.cancelled() => return Ok(()),
                result = tokio::time::timeout(IO_TIMEOUT, events.send(Event::Terminal(terminal))) => {
                    result.context(crate::i18n::tr(i18n_key::CONNECTIONS_TERMINAL_TRAILING_OUTPUT_TIMED_OUT))?.context(crate::i18n::tr(i18n_key::CONNECTIONS_THE_TERMINAL_EVENT_CHANNEL_IS_CLOSED))?;
                }
            }
        }
    }
}

struct Reporter {
    line: Vec<u8>,
    diagnostic: String,
    progress: Progress,
    base: u64,
    current: u64,
    seen_file: bool,
    skipped: bool,
}

impl Reporter {
    fn new(direction: Direction, total: u64) -> Self {
        Self {
            line: Vec::new(),
            diagnostic: String::new(),
            base: 0,
            current: 0,
            seen_file: false,
            skipped: false,
            progress: Progress {
                direction,
                bytes_transferred: 0,
                total_bytes: (direction == Direction::Upload).then_some(total),
                files_completed: 0,
            },
        }
    }

    fn feed(&mut self, bytes: &[u8]) {
        for &byte in bytes {
            if matches!(byte, b'\r' | b'\n') {
                self.flush();
            } else if self.line.len() < 2048 {
                self.line.push(byte);
            }
        }
    }

    fn flush(&mut self) {
        if self.line.is_empty() {
            return;
        }
        let bytes = std::mem::take(&mut self.line);
        let line: String = String::from_utf8_lossy(&bytes)
            .chars()
            .filter(|character| !character.is_control())
            .collect();
        let line = line.trim();
        if line.is_empty() {
            return;
        }
        self.skipped |= line.to_ascii_lowercase().contains("skipped:");
        if line.starts_with("Sending:") || line.starts_with("Receiving:") {
            if self.seen_file {
                self.base = self.base.saturating_add(self.current);
                self.progress.files_completed += 1;
            }
            self.current = 0;
            self.seen_file = true;
        }
        if let Some(numbers) = line
            .strip_prefix("Bytes Sent:")
            .or_else(|| line.strip_prefix("Bytes received:"))
            && let Some(value) = numbers
                .trim_start()
                .split(|c: char| !c.is_ascii_digit())
                .next()
            && let Ok(value) = value.parse::<u64>()
        {
            self.current = self.current.max(value);
            self.progress.bytes_transferred = self.base.saturating_add(self.current);
        }
        self.diagnostic = line.chars().take(512).collect();
    }

    fn diagnostic(&self) -> &str {
        if self.diagnostic.is_empty() {
            crate::i18n::tr(i18n_key::CONNECTIONS_NO_ERROR_DETAILS_WERE_PROVIDED)
        } else {
            &self.diagnostic
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session::zmodem::{
        Detector, Engine, HEADER_LEN, InputState, QUEUE_CAPACITY, hex_header,
    };

    async fn interoperability(direction: Direction) {
        let helper_dir = PathBuf::from(
            std::env::var_os("RUSTTERM_TEST_LRZSZ_DIR")
                .expect("set RUSTTERM_TEST_LRZSZ_DIR to a directory containing rz and sz"),
        );
        let source = tempfile::tempdir().unwrap();
        let destination = tempfile::tempdir().unwrap();
        let mut payload: Vec<u8> = (0..=255).collect();
        payload.extend_from_slice(b"**\x18B0800000000022d\r\nOObinary payload\0\xff");
        std::fs::write(source.path().join("payload.bin"), &payload).unwrap();
        let (remote_name, local_name, remote_directory) = match direction {
            Direction::Download => ("sz", "rz", source.path()),
            Direction::Upload => ("rz", "sz", destination.path()),
        };
        let mut remote = tokio::process::Command::new(helper_dir.join(remote_name));
        remote
            .args(["--binary", "--protect", "--escape"])
            .current_dir(remote_directory)
            .env_clear()
            .env("LC_ALL", "C")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .kill_on_drop(true);
        if direction == Direction::Download {
            remote.args(["--", "payload.bin"]);
        }
        let mut remote = remote.spawn().unwrap();
        let mut remote_input = remote.stdin.take().unwrap();
        let mut remote_output = remote.stdout.take().unwrap();
        let mut detector = Detector::default();
        let mut buffer = [0_u8; 8192];
        let request = loop {
            let count =
                tokio::time::timeout(Duration::from_secs(3), remote_output.read(&mut buffer))
                    .await
                    .unwrap()
                    .unwrap();
            assert_ne!(count, 0, "remote helper exited before autostart");
            if let Some(request) = detector.feed(&buffer[..count]).request {
                break request;
            }
        };
        assert_eq!(request.direction, direction);
        let selection = match direction {
            Direction::Download => Selection::DownloadDirectory(destination.path().to_owned()),
            Direction::Upload => Selection::UploadFiles(vec![source.path().join("payload.bin")]),
        };
        let (events, mut receiver) = mpsc::channel(32);
        let (sender, input_receiver) = mpsc::channel(QUEUE_CAPACITY);
        let input = Arc::new(tokio::sync::Mutex::new(InputState {
            sender: Some(sender),
            finalized: false,
        }));
        let cancel = CancellationToken::new();
        let progress = Arc::new(Mutex::new(Progress {
            direction,
            bytes_transferred: 0,
            total_bytes: None,
            files_completed: 0,
        }));
        let finished = Arc::new(AtomicBool::new(false));
        let finalized = Arc::new(AtomicBool::new(false));
        let engine = Engine {
            input: input.clone(),
            events: events.clone(),
            cancel: cancel.clone(),
            progress: progress.clone(),
            finished: finished.clone(),
            finalized: finalized.clone(),
        };
        let worker = Worker {
            input,
            events,
            cancel,
            progress,
            finished,
            finalized,
        };
        tokio::spawn(worker.run(
            helper_dir.join(local_name),
            request,
            selection,
            input_receiver,
        ));
        let mut visible = Vec::new();
        let mut final_scanner = FinalHeaderScanner::default();
        let mut local_final = false;
        let mut remote_eof = false;
        let mut appended_prompt = false;
        let outcome = tokio::time::timeout(Duration::from_secs(8), async {
            loop {
                tokio::select! {
                    event = receiver.recv() => match event.unwrap() {
                        Event::Write(bytes) => {
                            local_final |= final_scanner.feed(&bytes);
                            remote_input.write_all(&bytes).await.unwrap();
                        }
                        Event::Terminal(bytes) => visible.extend(bytes),
                        Event::Progress(_) => {}
                        Event::Finished(outcome) => break outcome,
                    },
                    read = remote_output.read(&mut buffer), if !remote_eof => {
                        let count = read.unwrap();
                        if count == 0 { remote_eof = true; continue; }
                        let mut bytes = buffer[..count].to_vec();
                        let at_finish = local_final && match direction {
                            Direction::Download => bytes.ends_with(b"OO"),
                            Direction::Upload => bytes.windows(HEADER_LEN).any(|header| hex_header(header) == Some(8)),
                        };
                        if at_finish { bytes.extend_from_slice(b"shell$ "); appended_prompt = true; }
                        if let Err(error) = engine.feed(bytes).await {
                            assert!(error.terminal, "{}", error.message);
                            visible.extend(error.bytes);
                        }
                    }
                }
            }
        }).await.expect("tiny interoperability transfer timed out");
        assert!(outcome.success, "{}", outcome.message);
        assert!(remote.wait().await.unwrap().success());
        assert_eq!(
            std::fs::read(destination.path().join("payload.bin")).unwrap(),
            payload
        );
        assert!(
            appended_prompt,
            "test must exercise a final handshake with terminal bytes in the same chunk"
        );
        assert_eq!(visible, b"shell$ ");
        assert!(
            receiver.try_recv().is_err(),
            "Finished must be the last event"
        );
    }

    #[tokio::test]
    #[ignore = "requires explicit RUSTTERM_TEST_LRZSZ_DIR; sends only a sub-KiB temporary file"]
    async fn zmodem_lrzsz_download_interoperability() {
        interoperability(Direction::Download).await;
    }

    #[tokio::test]
    #[ignore = "requires explicit RUSTTERM_TEST_LRZSZ_DIR; sends only a sub-KiB temporary file"]
    async fn zmodem_lrzsz_upload_interoperability() {
        interoperability(Direction::Upload).await;
    }

    #[test]
    fn zmodem_staging_is_private_and_publishes_without_remote_permissions() {
        let destination = tempfile::tempdir().unwrap();
        let cancel = CancellationToken::new();
        let prepared = Prepared::create(
            Selection::DownloadDirectory(destination.path().to_owned()),
            &cancel,
        )
        .unwrap();
        let staged = fd_path(prepared.directory.as_raw_fd()).join("sample.txt");
        std::fs::write(&staged, b"small payload").unwrap();
        std::fs::set_permissions(&staged, std::fs::Permissions::from_mode(0o777)).unwrap();
        assert_eq!(
            prepared.directory.metadata().unwrap().permissions().mode() & 0o777,
            0o700
        );
        let (files, result) = prepared.publish(&cancel);
        assert_eq!(result.unwrap(), 13);
        assert_eq!(files, vec![destination.path().join("sample.txt")]);
        assert_eq!(std::fs::read(&files[0]).unwrap(), b"small payload");
        assert_eq!(
            std::fs::metadata(&files[0]).unwrap().permissions().mode() & 0o777,
            0o600
        );
        drop(prepared);
        assert_eq!(std::fs::read_dir(destination.path()).unwrap().count(), 1);
        let upload = Prepared::create(Selection::UploadFiles(files), &cancel).unwrap();
        assert_eq!(
            upload.directory.metadata().unwrap().permissions().mode() & 0o777,
            0o700
        );
    }

    #[test]
    fn zmodem_download_never_replaces_existing_file_or_symlink() {
        for symlink in [false, true] {
            let destination = tempfile::tempdir().unwrap();
            let outside = tempfile::NamedTempFile::new().unwrap();
            std::fs::write(outside.path(), b"outside").unwrap();
            let target = destination.path().join("sample.txt");
            if symlink {
                std::os::unix::fs::symlink(outside.path(), &target).unwrap();
            } else {
                std::fs::write(&target, b"existing").unwrap();
            }
            let cancel = CancellationToken::new();
            let prepared = Prepared::create(
                Selection::DownloadDirectory(destination.path().to_owned()),
                &cancel,
            )
            .unwrap();
            std::fs::write(
                fd_path(prepared.directory.as_raw_fd()).join("sample.txt"),
                b"incoming",
            )
            .unwrap();
            let (files, result) = prepared.publish(&cancel);
            assert!(result.is_err());
            assert!(files.is_empty());
            assert_eq!(std::fs::read(outside.path()).unwrap(), b"outside");
            if !symlink {
                assert_eq!(std::fs::read(&target).unwrap(), b"existing");
            }
            drop(prepared);
            assert_eq!(std::fs::read_dir(destination.path()).unwrap().count(), 1);
        }
    }

    #[test]
    fn zmodem_rejects_traversal_symlink_parents_and_special_files() {
        for name in [
            "../escape",
            "/absolute",
            ".hidden",
            "nested/file",
            "back\\slash",
            "-",
        ] {
            assert!(validate_name(OsStr::new(name)).is_err());
        }
        let directory = tempfile::tempdir().unwrap();
        let real = directory.path().join("real");
        let link = directory.path().join("link");
        std::fs::create_dir(&real).unwrap();
        std::os::unix::fs::symlink(&real, &link).unwrap();
        assert!(open_directory(&link).is_err());
        let fd = open_directory(directory.path()).unwrap();
        assert!(open_regular(fd.as_raw_fd(), OsStr::new("link")).is_err());
        assert!(open_regular(fd.as_raw_fd(), OsStr::new("real")).is_err());
    }

    #[test]
    fn zmodem_cancelled_download_cleans_staging_without_publishing() {
        let destination = tempfile::tempdir().unwrap();
        let cancel = CancellationToken::new();
        let prepared = Prepared::create(
            Selection::DownloadDirectory(destination.path().to_owned()),
            &cancel,
        )
        .unwrap();
        std::fs::write(
            fd_path(prepared.directory.as_raw_fd()).join("partial"),
            b"partial",
        )
        .unwrap();
        cancel.cancel();
        let (files, result) = prepared.publish(&cancel);
        assert!(result.is_err());
        assert!(files.is_empty());
        drop(prepared);
        assert_eq!(std::fs::read_dir(destination.path()).unwrap().count(), 0);
    }
}
