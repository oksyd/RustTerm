//! Bounded non-PTY channels on the already authenticated SSH transport.
use crate::docker::{Error, Result};
use russh::{Channel, ChannelMsg, client};
use std::{
    io,
    pin::Pin,
    sync::{Arc, Mutex},
    task::{Context, Poll},
    time::Duration,
};
use tokio::{
    io::{AsyncRead, AsyncWrite, AsyncWriteExt, DuplexStream, ReadBuf},
    sync::{OwnedSemaphorePermit, Semaphore, mpsc, oneshot},
};
use tokio_util::sync::CancellationToken;

pub(crate) const MAX_CHANNELS: usize = 6;
pub(crate) struct Request {
    command: String,
    pub(crate) cancel: CancellationToken,
    pub(crate) reply: oneshot::Sender<Result<ExecStream>>,
    permit: OwnedSemaphorePermit,
}
#[derive(Clone)]
pub(crate) struct ExecClient {
    requests: mpsc::Sender<Request>,
    pub(crate) stop: CancellationToken,
    permits: Arc<Semaphore>,
    pub(crate) runtime: tokio::runtime::Handle,
}
impl ExecClient {
    pub(super) fn new(stop: CancellationToken) -> (Self, mpsc::Receiver<Request>) {
        let (requests, receive) = mpsc::channel(MAX_CHANNELS);
        (
            Self {
                requests,
                stop,
                permits: Arc::new(Semaphore::new(MAX_CHANNELS)),
                runtime: tokio::runtime::Handle::current(),
            },
            receive,
        )
    }
    pub(crate) fn is_connected(&self) -> bool {
        !self.stop.is_cancelled() && !self.requests.is_closed()
    }
    pub(crate) async fn open(&self, command: String) -> Result<ExecStream> {
        let permit = self
            .permits
            .clone()
            .try_acquire_owned()
            .map_err(|_| Error::Busy)?;
        let cancel = self.stop.child_token();
        let guard = cancel.clone().drop_guard();
        let (reply, receive) = oneshot::channel();
        self.requests
            .try_send(Request {
                command,
                cancel,
                reply,
                permit,
            })
            .map_err(|e| match e {
                mpsc::error::TrySendError::Full(_) => Error::Busy,
                mpsc::error::TrySendError::Closed(_) => Error::Disconnected,
            })?;
        let stream = tokio::select! {
            _ = self.stop.cancelled() => return Err(Error::Disconnected),
            result = tokio::time::timeout(Duration::from_secs(10), receive) => result.map_err(|_| Error::Timeout)?.map_err(|_| Error::Disconnected)??,
        };
        guard.disarm(); // The stream now owns cancellation and its channel permit.
        Ok(stream)
    }
}
struct Opened(Option<Channel<client::Msg>>);
impl Drop for Opened {
    fn drop(&mut self) {
        if let Some(channel) = self.0.take() {
            drop(channel.into_stream());
        }
    }
}
pub(super) async fn open<H: client::Handler>(handle: &client::Handle<H>, request: Request) {
    // Keep an unconfirmed open borrowed until the reply, even after caller timeout.
    let result = async {
        let mut opened = Opened(Some(
            handle
                .channel_open_session()
                .await
                .map_err(|e| Error::Transport(e.to_string()))?,
        ));
        if request.cancel.is_cancelled() || request.reply.is_closed() {
            return Err(Error::Cancelled);
        }
        let channel = opened.0.as_mut().expect("opened channel");
        channel
            .exec(true, request.command.as_bytes())
            .await
            .map_err(|e| Error::Transport(e.to_string()))?;
        let mut stderr = Vec::new();
        loop {
            let msg = tokio::select! {
                _ = request.cancel.cancelled() => return Err(Error::Cancelled),
                msg = channel.wait() => msg,
            };
            match msg {
                Some(ChannelMsg::Success) => break,
                Some(ChannelMsg::ExtendedData { data, .. }) => append_error(&mut stderr, &data),
                Some(ChannelMsg::WindowAdjusted { .. } | ChannelMsg::XonXoff { .. }) => {}
                _ => {
                    return Err(Error::Transport(
                        String::from_utf8_lossy(&stderr).into_owned(),
                    ));
                }
            }
        }
        Ok(ExecStream::new(
            opened.0.take().expect("opened channel"),
            request.cancel,
            request.permit,
        ))
    }
    .await;
    let _ = request.reply.send(result);
}
fn append_error(out: &mut Vec<u8>, data: &[u8]) {
    out.extend_from_slice(&data[..data.len().min(8192usize.saturating_sub(out.len()))]);
}

pub(crate) struct ExecStream {
    io: DuplexStream,
    cancel: CancellationToken,
    diagnostic: Arc<Mutex<Vec<u8>>>,
}
impl ExecStream {
    fn new(
        channel: Channel<client::Msg>,
        cancel: CancellationToken,
        permit: OwnedSemaphorePermit,
    ) -> Self {
        let (io, bridge) = tokio::io::duplex(64 * 1024);
        let diagnostic = Arc::new(Mutex::new(Vec::new()));
        let errors = diagnostic.clone();
        let stop = cancel.clone();
        tokio::spawn(async move {
            let _permit = permit;
            let (mut receive, send) = channel.split();
            let (mut local_read, mut local_write) = tokio::io::split(bridge);
            let mut remote_write = send.make_writer();
            let read = async {
                while let Some(msg) = receive.wait().await {
                    match msg {
                        ChannelMsg::Data { data } => {
                            if local_write.write_all(&data).await.is_err() {
                                break;
                            }
                        }
                        ChannelMsg::ExtendedData { data, .. } => append_error(
                            &mut errors.lock().unwrap_or_else(|e| e.into_inner()),
                            &data,
                        ),
                        ChannelMsg::ExitStatus { exit_status } if exit_status != 0 => append_error(
                            &mut errors.lock().unwrap_or_else(|e| e.into_inner()),
                            format!(" (exit {exit_status})").as_bytes(),
                        ),
                        ChannelMsg::Eof | ChannelMsg::Close => break,
                        _ => {}
                    }
                }
            };
            let write = async {
                let _ = tokio::io::copy(&mut local_read, &mut remote_write).await;
            };
            tokio::select! {
                _ = stop.cancelled() => {},
                _ = read => {},
                _ = write => {},
            }
            let _ = tokio::time::timeout(Duration::from_secs(2), send.close()).await;
        });
        Self {
            io,
            cancel,
            diagnostic,
        }
    }
    pub(crate) fn diagnostic(&self) -> Arc<Mutex<Vec<u8>>> {
        self.diagnostic.clone()
    }
}
impl Drop for ExecStream {
    fn drop(&mut self) {
        self.cancel.cancel();
    }
}
impl AsyncRead for ExecStream {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        Pin::new(&mut self.io).poll_read(cx, buf)
    }
}
impl AsyncWrite for ExecStream {
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        Pin::new(&mut self.io).poll_write(cx, buf)
    }
    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.io).poll_flush(cx)
    }
    fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.io).poll_shutdown(cx)
    }
}
