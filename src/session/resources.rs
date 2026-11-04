use crate::i18n::messages as i18n_key;

mod model;
mod parse;

pub use model::{DiskUsage, MemoryUsage, NetworkUsage, ResourceSnapshot};

use russh::{Channel, ChannelMsg, client};
use std::{
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};
use tokio::{
    io::AsyncWriteExt as _,
    sync::{Semaphore, mpsc, oneshot},
};
use tokio_util::sync::CancellationToken;

const SAMPLE_TIMEOUT: Duration = Duration::from_secs(10);
const MAX_OUTPUT_BYTES: usize = 128 * 1024;
const SCRIPT: &str = include_str!("resources/collect.sh");

#[derive(Debug, thiserror::Error)]
pub enum ResourceError {
    #[error(
        "{}",
        crate::i18n::tr(i18n_key::CONNECTIONS_THE_SSH_CONNECTION_IS_DISCONNECTED)
    )]
    Disconnected,
    #[error(
        "{}",
        crate::i18n::tr(i18n_key::RESOURCES_RESOURCE_SAMPLING_IS_ALREADY_IN_PROGRESS)
    )]
    Busy,
    #[error(
        "{}",
        crate::i18n::tr(i18n_key::RESOURCES_RESOURCE_SAMPLING_WAS_CANCELLED)
    )]
    Cancelled,
    #[error("{}", crate::i18n::tr(i18n_key::RESOURCES_RESOURCE_SAMPLING_TIMED_OUT))]
    Timeout,
    #[error(
        "{}",
        crate::i18n::tr(i18n_key::RESOURCES_RESOURCE_SAMPLING_OUTPUT_EXCEEDED_ITS_LIMIT)
    )]
    OutputLimit,
    #[error(
        "{}",
        crate::i18n::tr(i18n_key::RESOURCES_THE_SERVER_REFUSED_THE_RESOURCE_SAMPLING_COMMAND)
    )]
    ExecRejected,
    #[error("{}", crate::trf!(i18n_key::RESOURCES_THE_RESOURCE_SAMPLING_COMMAND_EXITED_WITH_CODE_CODE, code = .0))]
    ExitStatus(u32),
    #[error(
        "{}",
        crate::i18n::tr(
            i18n_key::RESOURCES_THE_RESOURCE_SAMPLING_COMMAND_WAS_TERMINATED_BY_A_SIGNAL
        )
    )]
    RemoteSignal,
    #[error("{}", crate::trf!(i18n_key::RESOURCES_INVALID_RESOURCE_SAMPLE_ERROR, error = crate::i18n::tr(.0)))]
    InvalidOutput(&'static str),
    #[error(
        "{}",
        crate::i18n::tr(i18n_key::RESOURCES_RESOURCE_MONITORING_REQUIRES_LINUX_PROCFS)
    )]
    Unsupported,
    #[error("{}", crate::trf!(i18n_key::RESOURCES_RESOURCE_SAMPLING_FAILED_ERROR, error = .0))]
    Protocol(String),
}

pub(super) struct Request {
    pub include_disks: bool,
    pub deadline: Instant,
    pub cancel: CancellationToken,
    pub reply: oneshot::Sender<Result<(Vec<u8>, Instant), ResourceError>>,
}

struct Inner {
    requests: mpsc::Sender<Request>,
    stop: CancellationToken,
    runtime: tokio::runtime::Handle,
    sampling: Semaphore,
    sampler: Mutex<parse::Sampler>,
}

#[derive(Clone)]
pub struct ResourceClient(Arc<Inner>);

impl ResourceClient {
    pub(super) fn new(stop: CancellationToken) -> (Self, mpsc::Receiver<Request>) {
        let (requests, receive) = mpsc::channel(1);
        (
            Self(Arc::new(Inner {
                requests,
                stop,
                runtime: tokio::runtime::Handle::current(),
                sampling: Semaphore::new(1),
                sampler: Mutex::new(parse::Sampler::default()),
            })),
            receive,
        )
    }

    pub fn is_connected(&self) -> bool {
        !self.0.stop.is_cancelled() && !self.0.requests.is_closed()
    }

    pub async fn closed(&self) {
        tokio::select! {
            _ = self.0.stop.cancelled() => {},
            _ = self.0.requests.closed() => {},
        }
    }

    pub fn reset_rates(&self) {
        self.0
            .sampler
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .reset_rates();
    }

    /// This future may run on any executor; the SSH broker performs all I/O.
    /// Dropping it cancels only this sample, never the terminal connection.
    pub async fn sample(&self, include_disks: bool) -> Result<ResourceSnapshot, ResourceError> {
        if !self.is_connected() {
            return Err(ResourceError::Disconnected);
        }
        let _permit = self
            .0
            .sampling
            .try_acquire()
            .map_err(|_| ResourceError::Busy)?;
        let cancel = self.0.stop.child_token();
        let _cancel_on_drop = cancel.clone().drop_guard();
        let (reply, receive) = oneshot::channel();
        let deadline = Instant::now() + SAMPLE_TIMEOUT;
        let timeout = {
            let _runtime = self.0.runtime.enter();
            tokio::time::sleep_until(tokio::time::Instant::from_std(deadline))
        };
        self.0
            .requests
            .try_send(Request {
                include_disks,
                deadline,
                cancel,
                reply,
            })
            .map_err(|error| match error {
                mpsc::error::TrySendError::Full(_) => ResourceError::Busy,
                mpsc::error::TrySendError::Closed(_) => ResourceError::Disconnected,
            })?;
        let result = tokio::select! {
            biased;
            _ = self.0.stop.cancelled() => Err(ResourceError::Disconnected),
            _ = timeout => Err(ResourceError::Timeout),
            result = receive => result.unwrap_or(Err(ResourceError::Disconnected)),
        };
        if !self.is_connected() {
            self.reset_rates();
            return Err(ResourceError::Disconnected);
        }
        match result {
            Ok((output, sampled_at)) => self
                .0
                .sampler
                .lock()
                .unwrap_or_else(|error| error.into_inner())
                .sample(&output, sampled_at)
                .map_err(|error| match error {
                    parse::ParseError::UnsupportedPlatform => ResourceError::Unsupported,
                    parse::ParseError::Invalid(message) => ResourceError::InvalidOutput(message),
                }),
            Err(error) => {
                self.reset_rates();
                Err(error)
            }
        }
    }
}

pub(super) async fn collect<H: client::Handler>(handle: &client::Handle<H>, mut request: Request) {
    let deadline = tokio::time::Instant::from_std(request.deadline);
    let open = handle.channel_open_session();
    tokio::pin!(open);
    let mut channel = tokio::select! {
        result = &mut open => match result {
            Ok(channel) => channel,
            Err(error) => {
                let _ = request.reply.send(Err(ResourceError::Protocol(error.to_string())));
                return;
            }
        },
        _ = tokio::time::sleep_until(deadline) => {
            let _ = request.reply.send(Err(ResourceError::Timeout));
            if let Ok(channel) = open.await { drop(channel.into_stream()); }
            return;
        },
        _ = request.cancel.cancelled() => {
            let _ = request.reply.send(Err(ResourceError::Cancelled));
            if let Ok(channel) = open.await { drop(channel.into_stream()); }
            return;
        },
        _ = request.reply.closed() => {
            if let Ok(channel) = open.await { drop(channel.into_stream()); }
            return;
        },
    };
    let result = tokio::select! {
        biased;
        _ = request.cancel.cancelled() => Err(ResourceError::Cancelled),
        _ = tokio::time::sleep_until(deadline) => Err(ResourceError::Timeout),
        result = execute(&mut channel, request.include_disks) => result,
    };
    // Channel itself does not close on drop. ChannelStream does, including when
    // exec was rejected, the output limit was reached, or the caller cancelled.
    drop(channel.into_stream());
    let _ = request
        .reply
        .send(result.map(|output| (output, Instant::now())));
}

async fn execute(
    channel: &mut Channel<client::Msg>,
    include_disks: bool,
) -> Result<Vec<u8>, ResourceError> {
    let command = if include_disks {
        "sh -s -- 1"
    } else {
        "sh -s -- 0"
    };
    channel
        .exec(true, command)
        .await
        .map_err(|error| ResourceError::Protocol(error.to_string()))?;
    let mut writer = channel.make_writer();
    let script = async {
        writer.write_all(SCRIPT.as_bytes()).await?;
        writer.shutdown().await
    };
    tokio::pin!(script);
    let mut accepted = false;
    let mut sent = false;
    let mut eof = false;
    let mut exited = false;
    let mut received = 0usize;
    let mut output = Vec::new();
    loop {
        tokio::select! {
            result = &mut script, if accepted && !sent => {
                result.map_err(|error: std::io::Error| ResourceError::Protocol(error.to_string()))?;
                sent = true;
            },
            message = channel.wait() => match message {
                Some(ChannelMsg::Success) => accepted = true,
                Some(ChannelMsg::Failure) => return Err(ResourceError::ExecRejected),
                Some(ChannelMsg::Data { data }) => {
                    received = received.saturating_add(data.len());
                    if received > MAX_OUTPUT_BYTES { return Err(ResourceError::OutputLimit); }
                    output.extend_from_slice(&data);
                }
                Some(ChannelMsg::ExtendedData { data, .. }) => {
                    received = received.saturating_add(data.len());
                    if received > MAX_OUTPUT_BYTES { return Err(ResourceError::OutputLimit); }
                }
                Some(ChannelMsg::ExitStatus { exit_status: 0 }) => exited = true,
                Some(ChannelMsg::ExitStatus { exit_status }) => return Err(ResourceError::ExitStatus(exit_status)),
                Some(ChannelMsg::ExitSignal { .. }) => return Err(ResourceError::RemoteSignal),
                Some(ChannelMsg::Eof) => eof = true,
                Some(ChannelMsg::Close) | None => {
                    if accepted && exited { return Ok(output); }
                    return Err(ResourceError::InvalidOutput(i18n_key::RESOURCES_RESOURCE_COMMAND_CLOSED_WITHOUT_AN_EXIT_STATUS));
                }
                _ => {},
            }
        }
        if accepted && exited && eof {
            return Ok(output);
        }
    }
}
