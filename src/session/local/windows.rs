//! ConPTY uses blocking pipe handles. Keep those operations off the UI/runtime
//! workers, with the same bounded channels as the Linux transport.
#[path = "options.rs"]
mod options;
use super::{
    ActorCommand, FailureKind, SessionState, actor,
    state::Shared,
    transport::{OUTPUT_CAPACITY, Output, Transport, WRITE_CAPACITY, WriteCommand},
};
use crate::terminal::Terminal;
pub use options::LocalOptions;
use portable_pty::{CommandBuilder, PtySize, native_pty_system};
use std::{
    io::{Read, Write},
    sync::{Arc, Mutex},
    time::Duration,
};
use tokio::sync::{mpsc, oneshot};

fn size(columns: u16, rows: u16) -> PtySize {
    PtySize {
        rows,
        cols: columns,
        pixel_width: 0,
        pixel_height: 0,
    }
}

fn open(
    options: LocalOptions,
    columns: u16,
    rows: u16,
    shared: Arc<Shared>,
) -> anyhow::Result<Transport> {
    let pair = native_pty_system().openpty(size(columns, rows))?;
    let mut command = CommandBuilder::new(&options.shell);
    command.args(&options.args);
    command.env("TERM", "xterm-256color");
    command.env("COLORTERM", "truecolor");
    if let Some(cwd) = options.cwd {
        command.cwd(cwd);
    }
    for (key, value) in options.env {
        command.env(key, value);
    }
    let mut reader = pair.master.try_clone_reader()?;
    let mut writer = pair.master.take_writer()?;
    let mut child = pair.slave.spawn_command(command)?;
    drop(pair.slave);
    let master = Arc::new(Mutex::new(pair.master));
    let (output_tx, output) = mpsc::channel(OUTPUT_CAPACITY);
    let (input, mut commands) = mpsc::channel(WRITE_CAPACITY);
    let (exit_tx, exit) = oneshot::channel();
    let reader_tx = output_tx.clone();
    let read_task = tokio::task::spawn_blocking(move || {
        let mut bytes = [0; 8192];
        loop {
            let message = match reader.read(&mut bytes) {
                Ok(0) => Output::Eof,
                Ok(count) => Output::Data(bytes[..count].to_vec()),
                Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
                Err(error) => Output::Error(FailureKind::Read, error.to_string()),
            };
            let finished = !matches!(message, Output::Data(_));
            if reader_tx.blocking_send(message).is_err() || finished {
                break;
            }
        }
    });
    let resize = master.clone();
    let writer_shared = shared.clone();
    let write_task = tokio::task::spawn_blocking(move || {
        while let Some(command) = commands.blocking_recv() {
            if writer_shared.io_stop.is_cancelled() {
                break;
            }
            let tracked = matches!(command, WriteCommand::UserInput(_));
            let result = match command {
                WriteCommand::Resize(columns, rows) => resize
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .resize(size(columns, rows)),
                WriteCommand::Data(bytes) | WriteCommand::UserInput(bytes) => {
                    writer.write_all(&bytes).map_err(Into::into)
                }
            };
            if tracked && result.is_ok() {
                writer_shared.input_completed();
            }
            if let Err(error) = result {
                let _ =
                    output_tx.blocking_send(Output::Error(FailureKind::Write, error.to_string()));
                break;
            }
        }
    });
    let reaper = tokio::spawn(async move {
        let mut terminating = false;
        let result = loop {
            match child.try_wait() {
                Ok(Some(status)) => break Ok(Some(status.exit_code())),
                Err(error) => {
                    let _ = child.kill();
                    break Err(error.to_string());
                }
                Ok(None) => {}
            }
            if shared.io_stop.is_cancelled() && !terminating {
                let _ = child.kill();
                terminating = true;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        };
        drop(master);
        let _ = exit_tx.send(result);
    });
    Ok(Transport {
        output,
        input,
        exit,
        tasks: vec![read_task, write_task],
        reaper: Some(reaper),
    })
}

pub(super) async fn run(
    options: LocalOptions,
    terminal: Terminal,
    columns: u16,
    rows: u16,
    commands: mpsc::Receiver<ActorCommand>,
    shared: Arc<Shared>,
) {
    let worker_shared = shared.clone();
    let transport =
        tokio::task::spawn_blocking(move || open(options, columns, rows, worker_shared)).await;
    let transport = match transport {
        Ok(Ok(transport)) => {
            shared.live_state(SessionState::Connected);
            Some(transport)
        }
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
            None
        }
    };
    actor::run(terminal, commands, shared, transport).await;
}
