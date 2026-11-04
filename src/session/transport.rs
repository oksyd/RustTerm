use crate::i18n::messages as i18n_key;

use super::{FailureKind, state::Shared};
use crate::terminal::Terminal;
use std::{io, time::Duration};
use tokio::{
    io::{AsyncWrite, AsyncWriteExt},
    sync::{mpsc, oneshot},
    task::JoinHandle,
};
use tokio_util::sync::CancellationToken;

pub(super) const OUTPUT_CAPACITY: usize = 32;
pub(super) const WRITE_CAPACITY: usize = 16;
pub(super) const IO_TIMEOUT: Duration = Duration::from_secs(2);
pub(super) const WRITE_STALL_TIMEOUT: Duration = Duration::from_secs(30);
pub(super) const SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(5);

#[derive(Debug)]
pub(super) enum Output {
    Data(Vec<u8>),
    Eof,
    Error(FailureKind, String),
}
pub(super) enum WriteCommand {
    Data(Vec<u8>),
    UserInput(Vec<u8>),
    Resize(u16, u16),
}
pub(super) struct Transport {
    pub(super) output: mpsc::Receiver<Output>,
    pub(super) input: mpsc::Sender<WriteCommand>,
    pub(super) exit: oneshot::Receiver<std::result::Result<Option<u32>, String>>,
    pub(super) tasks: Vec<JoinHandle<()>>,
    pub(super) reaper: Option<JoinHandle<()>>,
}

/// Bound lack of progress, not the total duration of a slow paste. Cancellation
/// never retries bytes: a partially delivered command belongs to this session.
pub(super) async fn write_all_cancellable(
    writer: &mut (impl AsyncWrite + Unpin),
    mut bytes: &[u8],
    stop: &CancellationToken,
    stall_timeout: Duration,
) -> io::Result<()> {
    let mut deadline = tokio::time::Instant::now() + stall_timeout;
    while !bytes.is_empty() {
        let result = tokio::select! {
            biased;
            _ = stop.cancelled() => return Err(io::ErrorKind::ConnectionAborted.into()),
            _ = tokio::time::sleep_until(deadline) => return Err(io::ErrorKind::TimedOut.into()),
            result = writer.write(bytes) => result,
        };
        match result {
            Ok(0) => return Err(io::ErrorKind::WriteZero.into()),
            Ok(written) => {
                bytes = &bytes[written..];
                deadline = tokio::time::Instant::now() + stall_timeout;
            }
            // An interrupted syscall made no progress. Keep the deadline and
            // let cancellation/timers run even if interruption repeats.
            Err(error) if error.kind() == io::ErrorKind::Interrupted => {
                tokio::task::yield_now().await;
            }
            Err(error) => return Err(error),
        }
    }
    Ok(())
}

impl Transport {
    pub(super) async fn shutdown_filtered(
        self,
        shared: &Shared,
        exit_received: bool,
        terminal: &mut Terminal,
        discard_protocol: bool,
    ) -> std::result::Result<Option<u32>, (FailureKind, &'static str)> {
        self.shutdown_inner(
            shared,
            exit_received,
            terminal,
            SHUTDOWN_TIMEOUT,
            discard_protocol,
        )
        .await
    }

    #[cfg(test)]
    async fn shutdown_with_timeout(
        self,
        shared: &Shared,
        exit_received: bool,
        terminal: &mut Terminal,
        timeout: Duration,
    ) -> std::result::Result<Option<u32>, (FailureKind, &'static str)> {
        self.shutdown_inner(shared, exit_received, terminal, timeout, false)
            .await
    }

    async fn shutdown_inner(
        self,
        shared: &Shared,
        exit_received: bool,
        terminal: &mut Terminal,
        timeout: Duration,
        discard_protocol: bool,
    ) -> std::result::Result<Option<u32>, (FailureKind, &'static str)> {
        shared.io_stop.cancel();
        drop(self.input);
        let mut output = self.output;
        let aborts: Vec<_> = self.tasks.iter().map(JoinHandle::abort_handle).collect();
        let joins = async move {
            let mut failed = false;
            for task in self.tasks {
                failed |= task.await.is_err();
            }
            if let Some(reaper) = self.reaper {
                failed |= reaper.await.is_err();
            }
            failed
        };
        tokio::pin!(joins);
        let deadline = tokio::time::sleep(timeout);
        tokio::pin!(deadline);
        let mut output_open = true;
        let result = loop {
            tokio::select! {
                failed = &mut joins => break if failed { Err((FailureKind::Process, crate::i18n::tr(i18n_key::CONNECTIONS_A_TRANSPORT_WORKER_ENDED_UNEXPECTEDLY))) } else { Ok(()) },
                _ = &mut deadline => {
                    // Stop accepting output before aborting a worker that may
                    // already hold a received chunk awaiting channel capacity.
                    output.close();
                    // The process reaper must retain ownership even if SIGKILL
                    // cannot finish while the child is stuck in kernel I/O.
                    for abort in &aborts { abort.abort(); }
                    break Err((FailureKind::Timeout, crate::i18n::tr(i18n_key::CONNECTIONS_SESSION_CLEANUP_TIMED_OUT_PROCESS_CLEANUP_MAY_STILL_BE_RUNNING)));
                }
                message = output.recv(), if output_open => match message {
                    Some(Output::Data(bytes)) if !discard_protocol => { terminal.feed(&bytes); shared.changed(); },
                    None => output_open = false,
                    _ => {},
                }
            }
        };
        while let Ok(message) = output.try_recv() {
            if let Output::Data(bytes) = message
                && !discard_protocol
            {
                terminal.feed(&bytes);
                shared.changed();
            }
        }
        result?;
        if exit_received {
            return Ok(None);
        }
        let mut exit = self.exit;
        match exit.try_recv() {
            Ok(Ok(code)) => Ok(code),
            Ok(Err(_)) => Err((
                FailureKind::Process,
                crate::i18n::tr(
                    i18n_key::CONNECTIONS_LOCAL_PROCESS_TERMINATION_FAILED_RESOURCE_CLEANUP_COULD_NOT_BE_CONFIRMED,
                ),
            )),
            Err(_) => Err((
                FailureKind::Process,
                crate::i18n::tr(i18n_key::CONNECTIONS_THE_TRANSPORT_TASK_DID_NOT_PROVIDE_AN_EXIT_RESULT),
            )),
        }
    }
}

pub(super) async fn enqueue_write(
    sender: &mpsc::Sender<WriteCommand>,
    command: WriteCommand,
    shared: &Shared,
) -> std::result::Result<(), FailureKind> {
    tokio::select! {
        biased;
        _ = shared.io_stop.cancelled() => Err(FailureKind::Write),
        result = tokio::time::timeout(IO_TIMEOUT, sender.send(command)) => match result {
            Ok(Ok(())) => Ok(()), Ok(Err(_)) => Err(FailureKind::Write), Err(_) => Err(FailureKind::Backpressure),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{
        pin::Pin,
        task::{Context, Poll},
    };
    use tokio::io::AsyncReadExt;

    struct InterruptingWriter {
        interruptions: usize,
        bytes: Vec<u8>,
    }

    impl AsyncWrite for InterruptingWriter {
        fn poll_write(
            mut self: Pin<&mut Self>,
            _: &mut Context<'_>,
            bytes: &[u8],
        ) -> Poll<io::Result<usize>> {
            if self.interruptions > 0 {
                self.interruptions -= 1;
                return Poll::Ready(Err(io::ErrorKind::Interrupted.into()));
            }
            self.bytes.extend_from_slice(bytes);
            Poll::Ready(Ok(bytes.len()))
        }

        fn poll_flush(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<io::Result<()>> {
            Poll::Ready(Ok(()))
        }

        fn poll_shutdown(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<io::Result<()>> {
            Poll::Ready(Ok(()))
        }
    }

    #[tokio::test]
    async fn interrupted_writes_retry_without_extending_the_stall_deadline() {
        let stop = CancellationToken::new();
        let mut writer = InterruptingWriter {
            interruptions: 2,
            bytes: Vec::new(),
        };
        write_all_cancellable(&mut writer, b"abc", &stop, WRITE_STALL_TIMEOUT)
            .await
            .unwrap();
        assert_eq!(writer.bytes, b"abc");

        writer.interruptions = usize::MAX;
        let error = tokio::time::timeout(
            Duration::from_millis(100),
            write_all_cancellable(&mut writer, b"def", &stop, Duration::from_millis(5)),
        )
        .await
        .expect("repeated interruption must not reset the deadline")
        .unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::TimedOut);
        assert!(writer.interruptions < usize::MAX);
        assert_eq!(writer.bytes, b"abc");
    }

    #[tokio::test]
    async fn partial_writes_preserve_bytes_and_cancel_without_replaying() {
        let (mut writer, mut reader) = tokio::io::duplex(2);
        let stop = CancellationToken::new();
        let write_stop = stop.clone();
        let write = tokio::spawn(async move {
            write_all_cancellable(&mut writer, b"abcde", &write_stop, WRITE_STALL_TIMEOUT).await
        });
        let mut prefix = [0; 3];
        reader.read_exact(&mut prefix).await.unwrap();
        assert_eq!(&prefix, b"abc");
        write.await.unwrap().unwrap();
        let mut tail = Vec::new();
        reader.read_to_end(&mut tail).await.unwrap();
        assert_eq!(tail, b"de");

        let (mut writer, mut reader) = tokio::io::duplex(2);
        let write_stop = stop.clone();
        let write = tokio::spawn(async move {
            write_all_cancellable(&mut writer, b"abcdef", &write_stop, WRITE_STALL_TIMEOUT).await
        });
        let mut prefix = [0; 2];
        reader.read_exact(&mut prefix).await.unwrap();
        assert_eq!(&prefix, b"ab");
        stop.cancel();
        let error = write.await.unwrap().unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::ConnectionAborted);
        let mut tail = Vec::new();
        reader.read_to_end(&mut tail).await.unwrap();
        assert!(b"cdef".starts_with(&tail));
    }

    #[tokio::test]
    async fn stalled_writer_times_out_with_no_input_replay() {
        let (mut writer, mut reader) = tokio::io::duplex(2);
        let error = write_all_cancellable(
            &mut writer,
            b"abcd",
            &CancellationToken::new(),
            Duration::from_millis(10),
        )
        .await
        .unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::TimedOut);
        drop(writer);
        let mut received = Vec::new();
        reader.read_to_end(&mut received).await.unwrap();
        assert_eq!(received, b"ab");
    }

    #[tokio::test]
    async fn shutdown_timeout_cancels_io_but_keeps_the_reaper_running() {
        let mut terminal = Terminal::new(80, 24, 10);
        let shared = Shared::new(&terminal);
        let (output_tx, output) = mpsc::channel(1);
        let (input, input_rx) = mpsc::channel(1);
        let (exit_tx, exit) = oneshot::channel();
        let task = tokio::spawn(async move {
            let _owned = (output_tx, input_rx);
            std::future::pending::<()>().await;
        });
        let abort = task.abort_handle();
        let (release_reaper, wait) = oneshot::channel();
        let (reaped_tx, reaped) = oneshot::channel();
        let reaper = tokio::spawn(async move {
            let _owned = exit_tx;
            wait.await.unwrap();
            reaped_tx.send(()).unwrap();
        });
        let reaper_abort = reaper.abort_handle();
        let transport = Transport {
            output,
            input,
            exit,
            tasks: vec![task],
            reaper: Some(reaper),
        };
        let result = transport
            .shutdown_with_timeout(&shared, false, &mut terminal, Duration::from_millis(20))
            .await;
        assert!(matches!(result, Err((FailureKind::Timeout, _))));
        tokio::time::timeout(Duration::from_millis(100), async {
            while !abort.is_finished() {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        assert!(!reaper_abort.is_finished());
        release_reaper.send(()).unwrap();
        tokio::time::timeout(Duration::from_millis(100), reaped)
            .await
            .unwrap()
            .unwrap();
    }

    #[tokio::test]
    async fn shutdown_keeps_tail_after_control_messages_and_reports_reaper_error() {
        let mut terminal = Terminal::new(80, 24, 10);
        let shared = Shared::new(&terminal);
        let (output_tx, output) = mpsc::channel(2);
        let (input, _input_rx) = mpsc::channel(1);
        let (exit_tx, exit) = oneshot::channel();
        output_tx
            .send(Output::Error(FailureKind::Write, "closed".into()))
            .await
            .unwrap();
        output_tx
            .send(Output::Data(b"retained-tail".to_vec()))
            .await
            .unwrap();
        exit_tx
            .send(Err("process could not be reaped".into()))
            .unwrap();
        let transport = Transport {
            output,
            input,
            exit,
            tasks: Vec::new(),
            reaper: None,
        };
        assert!(matches!(
            transport
                .shutdown_filtered(&shared, false, &mut terminal, false)
                .await,
            Err((FailureKind::Process, _))
        ));
        let text: String = terminal.snapshot().lines[0]
            .cells
            .iter()
            .map(|cell| cell.text.as_str())
            .collect();
        assert!(text.contains("retained-tail"));
    }
}
