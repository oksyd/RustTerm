use crate::i18n::messages as i18n_key;

mod broker;
mod connection;
mod dial;
mod forwarding;

#[cfg(test)]
mod isolation_tests;

pub use connection::{AuthenticationChallenge, AuthenticationPrompt, HostKeyChallenge};
pub(crate) use connection::{TunnelConnector, connect_tunnel};
pub use forwarding::{ForwardController, ForwardState, ForwardStatus};

use super::{
    ActorCommand, FailureKind, SessionState, actor,
    state::Shared,
    transport::{
        IO_TIMEOUT, OUTPUT_CAPACITY, Output, Transport, WRITE_CAPACITY, WRITE_STALL_TIMEOUT,
        WriteCommand, write_all_cancellable,
    },
};
use crate::config::{ForwardKind, known_hosts::normalize_host};
use crate::terminal::Terminal;
use connection::{SshResult, Ui, connect_authenticated};
use russh::{ChannelMsg, client};
use std::future::Future;
use std::{path::PathBuf, sync::Arc, time::Duration};
use tokio::sync::{mpsc, oneshot};
use zeroize::Zeroize;

/// Secrets are intentionally excluded from Debug and serialization.
#[derive(Clone)]
pub enum SshAuth {
    Password(String),
    PrivateKey {
        path: PathBuf,
        passphrase: Option<String>,
    },
    Agent {
        socket: Option<PathBuf>,
    },
    KeyboardInteractive,
}
impl Drop for SshAuth {
    fn drop(&mut self) {
        match self {
            Self::Password(value) => value.zeroize(),
            Self::PrivateKey { passphrase, .. } => {
                if let Some(value) = passphrase {
                    value.zeroize();
                }
            }
            Self::Agent { .. } | Self::KeyboardInteractive => {}
        }
    }
}
#[derive(Clone)]
pub struct SshOptions {
    pub host: String,
    pub port: u16,
    pub username: String,
    pub auth: SshAuth,
    pub known_hosts_path: PathBuf,
    pub timeout_secs: u64,
    pub keepalive_secs: u64,
    pub keepalive_max: usize,
    pub shell_integration: bool,
    pub jump_hosts: Vec<crate::config::SshJumpHost>,
    pub forwards: Vec<crate::config::PortForward>,
    pub authentication: Option<mpsc::Sender<AuthenticationChallenge>>,
}
pub(super) async fn run(
    options: SshOptions,
    terminal: Terminal,
    columns: u16,
    rows: u16,
    commands: mpsc::Receiver<ActorCommand>,
    shared: Arc<Shared>,
) {
    let result = connect(options, columns, rows, shared.clone()).await;
    let transport = match result {
        Ok(transport) => Some(transport),
        Err((kind, message)) => {
            if !matches!(shared.current_state(), SessionState::Failed { .. }) {
                if shared.io_stop.is_cancelled() {
                    shared.state(SessionState::Disconnected { exit_code: None });
                } else {
                    shared.failed(kind, message);
                }
            }
            None
        }
    };
    actor::run(terminal, commands, shared, transport).await;
}

async fn stage<T, E: std::fmt::Display>(
    future: impl Future<Output = std::result::Result<T, E>>,
    timeout: Duration,
    kind: FailureKind,
    shared: &Shared,
) -> SshResult<T> {
    connection::stage(future, timeout, kind, &shared.io_stop).await
}
async fn connect(
    mut options: SshOptions,
    columns: u16,
    rows: u16,
    shared: Arc<Shared>,
) -> SshResult<Transport> {
    let timeout = Duration::from_secs(options.timeout_secs.clamp(1, 600));
    let shell_integration = options.shell_integration;
    for rule in &mut options.forwards {
        rule.bind_host = normalize_host(&rule.bind_host)
            .map_err(|error| (FailureKind::Protocol, error.to_string()))?;
        if rule.kind != ForwardKind::Dynamic {
            rule.destination_host = normalize_host(&rule.destination_host)
                .map_err(|error| (FailureKind::Protocol, error.to_string()))?;
        }
    }
    let mut handle = connect_authenticated(
        options.clone(),
        Ui::Terminal(shared.clone()),
        shared.io_stop.child_token(),
    )
    .await?;
    let mut startup_output = Vec::new();
    let mut channel = stage(
        handle.channel_open_session(),
        timeout,
        FailureKind::Protocol,
        &shared,
    )
    .await?;
    stage(
        channel.request_pty(
            true,
            "xterm-256color",
            u32::from(columns),
            u32::from(rows),
            0,
            0,
            &[(russh::Pty::IUTF8, 1)],
        ),
        timeout,
        FailureKind::Protocol,
        &shared,
    )
    .await?;
    expect_success(
        &mut channel,
        timeout,
        &shared,
        crate::i18n::tr(i18n_key::CONNECTIONS_THE_SERVER_REFUSED_TO_ALLOCATE_A_PTY),
        &mut startup_output,
    )
    .await?;
    if shell_integration {
        stage(
            channel.exec(true, crate::terminal::integration::remote_command()),
            timeout,
            FailureKind::Protocol,
            &shared,
        )
        .await?;
    } else {
        stage(
            channel.request_shell(true),
            timeout,
            FailureKind::Protocol,
            &shared,
        )
        .await?;
    }
    expect_success(
        &mut channel,
        timeout,
        &shared,
        crate::i18n::tr(if shell_integration {
            i18n_key::CONNECTIONS_THE_SERVER_REFUSED_SHELL_INTEGRATION_DISABLE_IT_IN_THE_SSH
        } else {
            i18n_key::CONNECTIONS_THE_SERVER_REFUSED_TO_START_A_SHELL
        }),
        &mut startup_output,
    )
    .await?;
    let (mut reader, writer) = channel.split();
    let (output_tx, output) = mpsc::channel(OUTPUT_CAPACITY);
    let (input, mut write_rx) = mpsc::channel(WRITE_CAPACITY);
    let (exit_tx, exit) = oneshot::channel();
    let read_output = output_tx.clone();
    let read_shared = shared.clone();
    let reader_task = tokio::spawn(async move {
        for bytes in startup_output {
            if !send_output(&read_output, Output::Data(bytes), &read_shared).await {
                break;
            }
        }
        let mut code = None;
        let mut exit_tx = Some(exit_tx);
        let mut eof_sent = false;
        loop {
            // CancelStream closes the underlying transport. Drain messages
            // already accepted by russh before observing channel closure.
            let message = reader.wait().await;
            match message {
                Some(ChannelMsg::Data { data }) | Some(ChannelMsg::ExtendedData { data, .. }) => {
                    for chunk in data.chunks(8192) {
                        if !send_output(&read_output, Output::Data(chunk.to_vec()), &read_shared)
                            .await
                        {
                            break;
                        }
                    }
                }
                Some(ChannelMsg::ExitStatus { exit_status }) => {
                    code = Some(exit_status);
                    if let Some(exit_tx) = exit_tx.take() {
                        let _ = exit_tx.send(Ok(code));
                    }
                }
                Some(ChannelMsg::ExitSignal { .. }) => {
                    code = None;
                    if let Some(exit_tx) = exit_tx.take() {
                        let _ = exit_tx.send(Ok(code));
                    }
                    *read_shared
                        .remote_failure
                        .lock()
                        .unwrap_or_else(|e| e.into_inner()) = Some((
                        FailureKind::RemoteClosed,
                        crate::i18n::tr(
                            i18n_key::CONNECTIONS_THE_REMOTE_SHELL_WAS_TERMINATED_BY_A_SIGNAL,
                        )
                        .into(),
                    ));
                }
                Some(ChannelMsg::Close) | None => break,
                // EOF and exit-status are independent SSH messages. Forward
                // each immediately so the actor can bound an incomplete close.
                Some(ChannelMsg::Eof) if !eof_sent => {
                    send_output(&read_output, Output::Eof, &read_shared).await;
                    eof_sent = true;
                }
                _ => {}
            }
        }
        if let Some(exit_tx) = exit_tx.take() {
            let _ = exit_tx.send(Ok(code));
        }
        if !eof_sent {
            send_output(&read_output, Output::Eof, &read_shared).await;
        }
    });
    let write_shared = shared.clone();
    let write_output = output_tx.clone();
    let writer_task = tokio::spawn(async move {
        let mut data_writer = writer.make_writer();
        loop {
            let command = tokio::select! { _=write_shared.io_stop.cancelled()=>break, command=write_rx.recv()=>command };
            let Some(command) = command else {
                break;
            };
            let tracked = matches!(&command, WriteCommand::UserInput(_));
            let result = match command {
                WriteCommand::Data(bytes) | WriteCommand::UserInput(bytes) => {
                    write_all_cancellable(
                        &mut data_writer,
                        &bytes,
                        &write_shared.io_stop,
                        WRITE_STALL_TIMEOUT,
                    )
                    .await
                    .map_err(|error| {
                        let kind = if error.kind() == std::io::ErrorKind::TimedOut {
                            FailureKind::Timeout
                        } else {
                            FailureKind::Write
                        };
                        (kind, error.to_string())
                    })
                }
                WriteCommand::Resize(cols, rows) => {
                    stage(
                        writer.window_change(u32::from(cols), u32::from(rows), 0, 0),
                        WRITE_STALL_TIMEOUT,
                        FailureKind::Resize,
                        &write_shared,
                    )
                    .await
                }
            };
            if tracked && result.is_ok() {
                write_shared.input_completed();
            }
            if let Err((kind, message)) = result {
                if !write_shared.io_stop.is_cancelled() {
                    send_output(&write_output, Output::Error(kind, message), &write_shared).await;
                }
                break;
            }
        }
    });
    drop(output_tx);
    let (sftp, sftp_open) = crate::session::sftp::SftpClient::new(shared.io_stop.clone());
    *shared.sftp.lock().unwrap_or_else(|e| e.into_inner()) = Some(sftp);
    let (resources, resource_requests) =
        crate::session::ResourceClient::new(shared.io_stop.clone());
    *shared.resources.lock().unwrap_or_else(|e| e.into_inner()) = Some(resources);
    let (exec, exec_requests) = crate::session::exec::ExecClient::new(shared.io_stop.clone());
    *shared.docker.lock().unwrap_or_else(|e| e.into_inner()) =
        Some(crate::docker::DockerClient::new(exec));
    let (forwarding, forwarding_task) = forwarding::start(options, shared.io_stop.clone());
    *shared.forwarding.lock().unwrap_or_else(|e| e.into_inner()) = Some(forwarding);
    let monitor_shared = shared.clone();
    let monitor = tokio::spawn(async move {
        // The shell reader observes connection closure and wakes the actor;
        // HostVerifier records its cause before russh releases that reader.
        // Pending control requests borrow the handle until cancellation, then
        // release it so its I/O task can be joined below.
        broker::run_shell(
            &handle,
            sftp_open,
            resource_requests,
            exec_requests,
            &monitor_shared.io_stop,
        )
        .await;
        monitor_shared
            .sftp
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .take();
        monitor_shared
            .resources
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .take();
        monitor_shared
            .docker
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .take();
        // CancelStream wakes russh's internal I/O task even if the peer never
        // reads again. Waiting for the handle observes its actual shutdown.
        let _ = tokio::time::timeout(IO_TIMEOUT, &mut handle).await;
    });
    shared.live_state(SessionState::Connected);
    Ok(Transport {
        output,
        input,
        exit,
        tasks: vec![reader_task, writer_task, monitor, forwarding_task],
        reaper: None,
    })
}
async fn expect_success(
    channel: &mut russh::Channel<client::Msg>,
    timeout: Duration,
    shared: &Shared,
    message: &str,
    pending: &mut Vec<Vec<u8>>,
) -> SshResult<()> {
    stage(
        async {
            loop {
                match channel.wait().await {
                    Some(ChannelMsg::Success) => return Ok(()),
                    // Flow-control notifications are independent of request
                    // acknowledgements; Ctrl-S/Ctrl-Q stay owned by the remote PTY.
                    Some(ChannelMsg::WindowAdjusted { .. } | ChannelMsg::XonXoff { .. }) => {}
                    Some(ChannelMsg::Data { data })
                    | Some(ChannelMsg::ExtendedData { data, .. }) => {
                        let used: usize = pending.iter().map(Vec::len).sum();
                        if used.saturating_add(data.len()) > OUTPUT_CAPACITY * 8192 {
                            return Err(crate::i18n::tr(
                                i18n_key::CONNECTIONS_SSH_STARTUP_OUTPUT_EXCEEDED_THE_BUFFER_LIMIT,
                            )
                            .to_owned());
                        }
                        pending.push(data.to_vec());
                    }
                    _ => return Err(message.to_owned()),
                }
            }
        },
        timeout,
        FailureKind::Protocol,
        shared,
    )
    .await
}
async fn send_output(sender: &mpsc::Sender<Output>, output: Output, _shared: &Shared) -> bool {
    // Shutdown continues draining this bounded queue while joining workers.
    // Cancelling here would silently discard an already received output chunk.
    sender.send(output).await.is_ok()
}
