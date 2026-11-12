use crate::i18n::messages as i18n_key;

use super::{connection::SshResult, forwarding::ForwardRequest};
use crate::session::{
    FailureKind,
    resources::{self, ResourceError},
    sftp::{OpenRequest, OpenedChannel, SftpError},
};
use futures_util::{StreamExt, stream::FuturesUnordered};
use russh::client;
use std::{collections::HashSet, time::Duration};
use tokio::sync::{mpsc, oneshot};
use tokio_util::sync::CancellationToken;

const MAX_SFTP_OPENS: usize = 8;
const MAX_FORWARD_OPENS: usize = 16;
const MAX_REMOTE_REQUESTS: usize = 8;
const MAX_RESOURCE_OPENS: usize = 2;

pub(super) struct TcpRequest {
    pub host: String,
    pub port: u16,
    pub reply: oneshot::Sender<SshResult<russh::ChannelStream<client::Msg>>>,
}

pub(super) async fn run_shell<H: client::Handler>(
    handle: &client::Handle<H>,
    mut sftp: mpsc::Receiver<OpenRequest>,
    mut resources: mpsc::Receiver<resources::Request>,
    mut exec: mpsc::Receiver<crate::session::exec::Request>,
    stop: &CancellationToken,
) {
    let mut sftp_pending = FuturesUnordered::new();
    let mut resource_pending = FuturesUnordered::new();
    let mut exec_pending = FuturesUnordered::new();
    loop {
        tokio::select! {
            biased;
            _ = stop.cancelled() => break,
            Some(()) = sftp_pending.next(), if !sftp_pending.is_empty() => {},
            Some(()) = resource_pending.next(), if !resource_pending.is_empty() => {},
            Some(()) = exec_pending.next(), if !exec_pending.is_empty() => {},
            Some(request) = exec.recv() => {
                if request.cancel.is_cancelled() || request.reply.is_closed() { continue; }
                if exec_pending.len() >= crate::session::exec::MAX_CHANNELS {
                    let _ = request.reply.send(Err(crate::docker::Error::Busy));
                } else {
                    exec_pending.push(crate::session::exec::open(handle, request));
                }
            },
            Some(request) = sftp.recv() => {
                if request.cancel.is_cancelled() || request.reply.is_closed() {
                    continue;
                }
                if sftp_pending.len() == MAX_SFTP_OPENS {
                    let _ = request.reply.send(Err(SftpError::Busy));
                } else {
                    sftp_pending.push(open_sftp(handle, request));
                }
            },
            Some(request) = resources.recv() => {
                if request.cancel.is_cancelled() || request.reply.is_closed() {
                    continue;
                }
                if resource_pending.len() == MAX_RESOURCE_OPENS {
                    let _ = request.reply.send(Err(ResourceError::Busy));
                } else {
                    resource_pending.push(resources::collect(handle, request));
                }
            },
        }
    }
}

pub(super) async fn run_forwarding<H: client::Handler>(
    handle: &client::Handle<H>,
    mut forwards: mpsc::Receiver<ForwardRequest>,
    closed: &CancellationToken,
    timeout: Duration,
) {
    let mut forward_pending = FuturesUnordered::new();
    let mut remote_pending = FuturesUnordered::new();
    let mut remote_endpoints = HashSet::new();
    let mut exec_pending = FuturesUnordered::new();
    loop {
        tokio::select! {
            biased;
            _ = closed.cancelled() => break,
            Some(()) = exec_pending.next(), if !exec_pending.is_empty() => {},
            Some(()) = forward_pending.next(), if !forward_pending.is_empty() => {},
            Some(endpoint) = remote_pending.next(), if !remote_pending.is_empty() => {
                remote_endpoints.remove(&endpoint);
            },
            Some(request) = forwards.recv() => match request {
                ForwardRequest::Exec(request) => {
                    if request.cancel.is_cancelled() || request.reply.is_closed() { continue; }
                    if exec_pending.len() >= crate::session::exec::MAX_CHANNELS {
                        let _ = request.reply.send(Err(crate::docker::Error::Busy));
                    } else {
                        exec_pending.push(crate::session::exec::open(handle, request));
                    }
                }
                ForwardRequest::Open(request) => {
                    if request.reply.is_closed() {
                        continue;
                    }
                    if forward_pending.len() == MAX_FORWARD_OPENS {
                        let _ = request.reply.send(Err((FailureKind::Backpressure, busy())));
                    } else {
                        forward_pending.push(open_forward(handle, request, timeout));
                    }
                }
                ForwardRequest::Remote { rule, enabled, reply } => {
                    if reply.is_closed() {
                        continue;
                    }
                    let endpoint = (rule.bind_host, rule.bind_port);
                    if remote_pending.len() == MAX_REMOTE_REQUESTS
                        || !remote_endpoints.insert(endpoint.clone())
                    {
                        let _ = reply.send(Err(busy()));
                    } else {
                        remote_pending.push(remote_forward(handle, endpoint, enabled, reply, timeout, closed));
                    }
                }
            },
        }
    }
    // Only whole-connection cancellation drops unconfirmed requests. A caller's
    // deadline or cancellation retains its slot until the peer responds.
}

pub(super) async fn run_tunnel<H: client::Handler>(
    handle: &client::Handle<H>,
    mut requests: mpsc::Receiver<TcpRequest>,
    closed: &CancellationToken,
    timeout: Duration,
) {
    let mut pending = FuturesUnordered::new();
    loop {
        tokio::select! {
            biased;
            _ = closed.cancelled() => break,
            Some(()) = pending.next(), if !pending.is_empty() => {},
            request = requests.recv() => {
                let Some(request) = request else { break };
                if request.reply.is_closed() {
                    continue;
                }
                if pending.len() == MAX_FORWARD_OPENS {
                    let _ = request.reply.send(Err((FailureKind::Backpressure, busy())));
                } else {
                    pending.push(open_forward(handle, request, timeout));
                }
            }
        }
    }
}

fn busy() -> String {
    crate::i18n::tr(i18n_key::CONNECTIONS_SSH_CHANNEL_REQUESTS_ARE_BUSY_PLEASE_RETRY_LATER).into()
}

fn timed_out() -> String {
    crate::i18n::tr(i18n_key::CONNECTIONS_THE_SSH_FORWARDING_REQUEST_TIMED_OUT).into()
}

async fn open_sftp<H: client::Handler>(handle: &client::Handle<H>, request: OpenRequest) {
    let result = handle
        .channel_open_session()
        .await
        .map(OpenedChannel::new)
        .map_err(|error| SftpError::Remote(error.to_string()));
    // OpenedChannel closes a late confirmation even when the receiver drops
    // between a successful send and receiving the value.
    let _ = request.reply.send(result);
}

async fn open_forward<H: client::Handler>(
    handle: &client::Handle<H>,
    request: TcpRequest,
    timeout: Duration,
) {
    let TcpRequest {
        host,
        port,
        mut reply,
    } = request;
    let open = handle.channel_open_direct_tcpip(host, u32::from(port), "127.0.0.1", 0);
    tokio::pin!(open);
    tokio::select! {
        result = &mut open => {
            let _ = reply.send(result.map(|channel| channel.into_stream()).map_err(|error| (FailureKind::Network, error.to_string())));
            return;
        }
        _ = tokio::time::sleep(timeout) => { let _ = reply.send(Err((FailureKind::Timeout, timed_out()))); }
        _ = reply.closed() => {},
    }
    if let Ok(channel) = open.await {
        drop(channel.into_stream());
    }
}

async fn remote_forward<H: client::Handler>(
    handle: &client::Handle<H>,
    endpoint: (String, u16),
    enabled: bool,
    mut reply: oneshot::Sender<Result<(), String>>,
    timeout: Duration,
    stop: &CancellationToken,
) -> (String, u16) {
    let cleanup = {
        let request = async {
            if enabled {
                handle
                    .tcpip_forward(&endpoint.0, u32::from(endpoint.1))
                    .await
                    .map(|_| ())
            } else {
                handle
                    .cancel_tcpip_forward(&endpoint.0, u32::from(endpoint.1))
                    .await
            }
        };
        tokio::pin!(request);
        tokio::select! {
            result = &mut request => {
                let established = enabled && result.is_ok();
                let delivered = reply.send(result.map_err(|error| error.to_string())).is_ok();
                established && !delivered
            }
            _ = tokio::time::sleep(timeout) => {
                let _ = reply.send(Err(timed_out()));
                let succeeded = request.await.is_ok();
                enabled && succeeded
            }
            _ = reply.closed() => {
                let succeeded = request.await.is_ok();
                enabled && succeeded
            }
        }
    };
    if cleanup {
        // Keep this endpoint reserved until the compensating request finishes:
        // cancelling it later must never remove a newly enabled listener.
        if handle
            .cancel_tcpip_forward(&endpoint.0, u32::from(endpoint.1))
            .await
            .is_err()
        {
            // A refused cancellation does not prove the listener is gone. Keep
            // its bounded slot and endpoint reserved for this connection.
            stop.cancelled().await;
        }
    }
    endpoint
}

#[cfg(test)]
mod tests;
