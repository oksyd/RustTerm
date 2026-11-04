use crate::i18n::messages as i18n_key;

use super::{
    ActorCommand, FailureKind, SessionState, actor,
    state::Shared,
    transport::{
        OUTPUT_CAPACITY, Output, Transport, WRITE_CAPACITY, WRITE_STALL_TIMEOUT, WriteCommand,
    },
};
use crate::terminal::Terminal;
use anyhow::{Context as _, Result};
use portable_pty::{Child, CommandBuilder, MasterPty, PtySize, native_pty_system};
use rustix::{
    fs::{OFlags, fcntl_getfl, fcntl_setfl},
    io::{fcntl_dupfd_cloexec, read, write},
    process::{Pid, PidfdFlags, pidfd_open},
};
use std::sync::{Arc, Mutex};
use std::{
    io,
    os::fd::{BorrowedFd, OwnedFd},
};
use tokio::io::{Interest, unix::AsyncFd};
use tokio::sync::{mpsc, oneshot};

mod options;
pub use options::LocalOptions;

type SharedMaster = Arc<Mutex<Box<dyn MasterPty + Send>>>;
type PtyChild = Box<dyn Child + Send + Sync>;

struct LocalPty {
    reader: AsyncFd<OwnedFd>,
    writer: AsyncFd<OwnedFd>,
    pidfd: AsyncFd<OwnedFd>,
    process: ChildProcess,
}

// Keep ownership even if registration fails after spawning, or a task is dropped.
// Only this exceptional cleanup path needs a blocking waiter.
struct ChildProcess {
    child: Option<PtyChild>,
    master: SharedMaster,
    _integration: Option<tempfile::TempDir>,
}

impl Drop for ChildProcess {
    fn drop(&mut self) {
        let Some(mut child) = self.child.take() else {
            return;
        };
        let master = self.master.clone();
        tokio::task::spawn_blocking(move || {
            match child.try_wait() {
                Ok(Some(_)) => return,
                Err(error) if error.raw_os_error() == Some(libc::ECHILD) => return,
                _ => {}
            }
            let _ = terminate_child(&mut child, &master);
            loop {
                match child.wait() {
                    Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
                    _ => break,
                }
            }
        });
    }
}

fn open_local(options: LocalOptions, columns: u16, rows: u16) -> Result<LocalPty> {
    let pair = native_pty_system()
        .openpty(pty_size(columns, rows))
        .context(crate::i18n::tr(
            i18n_key::CONNECTIONS_FAILED_TO_CREATE_A_PTY,
        ))?;
    let raw_fd = pair.master.as_raw_fd().context(crate::i18n::tr(
        i18n_key::CONNECTIONS_FAILED_TO_OPEN_PTY_OUTPUT,
    ))?;
    // SAFETY: the master owns this descriptor throughout duplication and fcntl.
    let fd = unsafe { BorrowedFd::borrow_raw(raw_fd) };
    fcntl_setfl(fd, fcntl_getfl(fd)? | OFlags::NONBLOCK).context(crate::i18n::tr(
        i18n_key::CONNECTIONS_FAILED_TO_SET_THE_PTY_TO_NONBLOCKING_MODE,
    ))?;
    let reader_fd = fcntl_dupfd_cloexec(fd, 0)?;
    // SAFETY: AsyncFd owns this descriptor, which is never closed or replaced
    // separately while registered with the reactor.
    let reader = unsafe { AsyncFd::register_with_interest(reader_fd, Interest::READABLE) }
        .context(crate::i18n::tr(
            i18n_key::CONNECTIONS_FAILED_TO_OPEN_PTY_OUTPUT,
        ))?;
    let writer_fd = fcntl_dupfd_cloexec(fd, 0)?;
    // SAFETY: this separate OwnedFd also remains valid and unchanged until its
    // AsyncFd is dropped.
    let writer = unsafe { AsyncFd::register_with_interest(writer_fd, Interest::WRITABLE) }
        .context(crate::i18n::tr(
            i18n_key::CONNECTIONS_FAILED_TO_OPEN_PTY_INPUT,
        ))?;
    let mut command = CommandBuilder::new(&options.shell);
    let integration =
        crate::terminal::integration::prepare(&options.shell, &options.args, &options.env)?;
    command.args(
        integration
            .as_ref()
            .map_or(options.args.as_slice(), |integration| {
                integration.arguments.as_slice()
            }),
    );
    command.env("TERM", "xterm-256color");
    command.env("COLORTERM", "truecolor");
    if let Some(cwd) = options.cwd {
        command.cwd(cwd);
    }
    for (key, value) in options.env {
        command.env(key, value);
    }
    if let Some(integration) = &integration {
        for (key, value) in &integration.environment {
            command.env(key, value);
        }
    }
    let child = pair.slave.spawn_command(command).context(crate::i18n::tr(
        i18n_key::CONNECTIONS_FAILED_TO_START_THE_LOCAL_SHELL,
    ))?;
    drop(pair.slave);
    let process = ChildProcess {
        child: Some(child),
        master: Arc::new(Mutex::new(pair.master)),
        _integration: integration.map(|integration| integration.directory),
    };
    let pid = process
        .child
        .as_ref()
        .and_then(|child| child.process_id())
        .and_then(|pid| i32::try_from(pid).ok())
        .and_then(Pid::from_raw)
        .context(crate::i18n::tr(
            i18n_key::CONNECTIONS_FAILED_TO_START_THE_LOCAL_SHELL,
        ))?;
    let pidfd = pidfd_open(pid, PidfdFlags::NONBLOCK)?;
    // SAFETY: AsyncFd retains exclusive ownership of this unchanged pidfd.
    let pidfd = unsafe { AsyncFd::register_with_interest(pidfd, Interest::READABLE) }.context(
        crate::i18n::tr(i18n_key::CONNECTIONS_FAILED_TO_START_THE_LOCAL_SHELL),
    )?;
    Ok(LocalPty {
        reader,
        writer,
        pidfd,
        process,
    })
}

fn pty_size(columns: u16, rows: u16) -> PtySize {
    PtySize {
        cols: columns,
        rows,
        pixel_width: 0,
        pixel_height: 0,
    }
}

async fn read_output(reader: AsyncFd<OwnedFd>, output: mpsc::Sender<Output>, shared: Arc<Shared>) {
    let mut buffer = [0_u8; 8192];
    loop {
        let result = tokio::select! {
            biased;
            _ = shared.io_stop.cancelled() => break,
            result = reader.async_io(Interest::READABLE, |fd| {
                read(fd, &mut buffer).map_err(io::Error::from)
            }) => result,
        };
        let message = match result {
            Ok(0) => Output::Eof,
            Ok(count) => Output::Data(buffer[..count].to_vec()),
            Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
            // Linux PTYs use EIO when the last slave descriptor closes.
            Err(error) if error.raw_os_error() == Some(libc::EIO) => Output::Eof,
            Err(error) => Output::Error(FailureKind::Read, error.to_string()),
        };
        let finished = !matches!(message, Output::Data(_));
        // A successful read transfers ownership to the actor, including during
        // cancellation. Dropping this send would silently discard received output.
        if output.send(message).await.is_err() || finished {
            break;
        }
    }
}

async fn write_input(
    master: SharedMaster,
    writer: AsyncFd<OwnedFd>,
    mut input: mpsc::Receiver<WriteCommand>,
    output: mpsc::Sender<Output>,
    shared: Arc<Shared>,
) {
    loop {
        let command = tokio::select! {
            biased;
            _ = shared.io_stop.cancelled() => break,
            command = input.recv() => match command {
                Some(command) => command,
                None => break,
            },
        };
        let tracked = matches!(&command, WriteCommand::UserInput(_));
        let result = match command {
            WriteCommand::Resize(columns, rows) => master
                .lock()
                .unwrap_or_else(|error| error.into_inner())
                .resize(pty_size(columns, rows))
                .map_err(|error| (FailureKind::Resize, error.to_string())),
            WriteCommand::Data(data) | WriteCommand::UserInput(data) => {
                write_cancellable(&writer, &data, &shared)
                    .await
                    .map_err(|error| (FailureKind::Write, error.to_string()))
            }
        };
        if tracked && result.is_ok() {
            shared.input_completed();
        }
        if let Err((kind, message)) = result {
            let _ = output.send(Output::Error(kind, message)).await;
            break;
        }
    }
}

async fn write_cancellable(
    writer: &AsyncFd<OwnedFd>,
    mut data: &[u8],
    shared: &Shared,
) -> io::Result<()> {
    let mut deadline = tokio::time::Instant::now() + WRITE_STALL_TIMEOUT;
    while !data.is_empty() {
        let result = tokio::select! {
            biased;
            _ = shared.io_stop.cancelled() => return Err(io::ErrorKind::ConnectionAborted.into()),
            result = tokio::time::timeout_at(deadline, writer.async_io(Interest::WRITABLE, |fd| {
                write(fd, data).map_err(io::Error::from)
            })) => result.map_err(|_| io::Error::from(io::ErrorKind::TimedOut))?,
        };
        match result {
            Ok(0) => return Err(io::ErrorKind::WriteZero.into()),
            Ok(count) => {
                data = &data[count..];
                deadline = tokio::time::Instant::now() + WRITE_STALL_TIMEOUT;
            }
            Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
            Err(error) => return Err(error),
        }
    }
    Ok(())
}

async fn wait_process(
    mut process: ChildProcess,
    pidfd: AsyncFd<OwnedFd>,
    exit: oneshot::Sender<std::result::Result<Option<u32>, String>>,
    shared: Arc<Shared>,
) {
    let mut exit = Some(exit);
    let mut terminating = false;
    loop {
        tokio::select! {
            biased;
            ready = pidfd.readable() => {
                let mut ready = match ready {
                    Ok(ready) => ready,
                    Err(error) => {
                        if let Some(exit) = exit.take() {
                            let _ = exit.send(Err(error.to_string()));
                        }
                        return;
                    }
                };
                match process.child.as_mut().unwrap().try_wait() {
                    Ok(Some(status)) => {
                        process.child.take();
                        if let Some(exit) = exit.take() {
                            let _ = exit.send(Ok(Some(status.exit_code())));
                        }
                        return;
                    }
                    Ok(None) => ready.clear_ready(),
                    Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
                    Err(error) => {
                        if error.raw_os_error() == Some(libc::ECHILD) {
                            process.child.take();
                        }
                        if let Some(exit) = exit.take() {
                            let _ = exit.send(Err(error.to_string()));
                        }
                        return;
                    }
                }
            }
            _ = shared.io_stop.cancelled(), if !terminating => {
                terminating = true;
                if let Err(error) = terminate_child(process.child.as_mut().unwrap(), &process.master)
                    && let Some(exit) = exit.take()
                {
                    let _ = exit.send(Err(error.to_string()));
                }
            }
        }
    }
}

/// Stop the foreground job as well as the original shell group. These IDs come
/// exclusively from the PTY we own, never from terminal output or user text.
fn terminate_child(child: &mut PtyChild, master: &SharedMaster) -> io::Result<()> {
    let foreground = master
        .lock()
        .unwrap_or_else(|error| error.into_inner())
        .process_group_leader();
    let shell = child.process_id().map(|pid| pid as libc::pid_t);
    if let Some(group) = foreground.filter(|group| *group > 0 && Some(*group) != shell) {
        // SAFETY: a negative process-group ID targets the foreground group
        // associated with this PTY; the PTY's child has not been reaped.
        unsafe {
            libc::kill(-group, libc::SIGKILL);
        }
    }
    if let Some(group) = shell.filter(|group| *group > 0) {
        // portable-pty creates a fresh session whose group ID is the child
        // PID. Keeping the Child unreaped prevents PID reuse here.
        if unsafe { libc::kill(-group, libc::SIGKILL) } == 0 {
            return Ok(());
        }
    }
    child.kill()
}

fn start_transport(pty: LocalPty, shared: Arc<Shared>) -> Transport {
    let (output_tx, output) = mpsc::channel(OUTPUT_CAPACITY);
    let (input, write_rx) = mpsc::channel(WRITE_CAPACITY);
    let (exit_tx, exit) = oneshot::channel();
    let LocalPty {
        reader,
        writer,
        pidfd,
        process,
    } = pty;
    let reader_task = tokio::spawn(read_output(reader, output_tx.clone(), shared.clone()));
    let writer_task = tokio::spawn(write_input(
        process.master.clone(),
        writer,
        write_rx,
        output_tx,
        shared.clone(),
    ));
    let reaper = tokio::spawn(wait_process(process, pidfd, exit_tx, shared));
    Transport {
        output,
        input,
        exit,
        tasks: vec![reader_task, writer_task],
        reaper: Some(reaper),
    }
}

pub(super) async fn run(
    options: LocalOptions,
    terminal: Terminal,
    columns: u16,
    rows: u16,
    commands: mpsc::Receiver<ActorCommand>,
    shared: Arc<Shared>,
) {
    let opened = tokio::task::spawn_blocking(move || open_local(options, columns, rows)).await;
    let pty = match opened {
        Ok(Ok(pty)) => pty,
        result => {
            let message = match result {
                Ok(Err(error)) => format!("{error:#}"),
                Err(error) => error.to_string(),
                _ => unreachable!(),
            };
            if shared.io_stop.is_cancelled() {
                shared.state(SessionState::Disconnected { exit_code: None });
            } else {
                shared.failed(FailureKind::Spawn, message);
            }
            actor::run(terminal, commands, shared, None).await;
            return;
        }
    };
    let transport = start_transport(pty, shared.clone());
    shared.live_state(SessionState::Connected);
    actor::run(terminal, commands, shared, Some(transport)).await;
}

#[cfg(test)]
mod tests;
