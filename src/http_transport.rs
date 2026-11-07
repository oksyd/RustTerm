use crate::session::TunnelConnector;
use std::{fmt, future::Future, io, time::Duration};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio_util::sync::CancellationToken;
use ureq::unversioned::{
    resolver::{ResolvedSocketAddrs, Resolver},
    transport::{Buffers, ConnectionDetails, Connector, LazyBuffers, NextTimeout, Transport},
};

/// DNS is performed by the connector (or SSH server), never by the HTTP layer.
#[derive(Debug)]
pub(crate) struct RemoteResolver;
impl Resolver for RemoteResolver {
    fn resolve(
        &self,
        _: &ureq::http::Uri,
        _: &ureq::config::Config,
        _: NextTimeout,
    ) -> std::result::Result<ResolvedSocketAddrs, ureq::Error> {
        let mut addresses = self.empty();
        addresses.push(([127, 0, 0, 1], 0).into());
        Ok(addresses)
    }
}

pub(crate) struct HttpConnector {
    pub(crate) handle: tokio::runtime::Handle,
    pub(crate) tunnel: Option<TunnelConnector>,
    pub(crate) stop: CancellationToken,
}
impl fmt::Debug for HttpConnector {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("HttpConnector")
    }
}
trait Socket: AsyncRead + AsyncWrite + Unpin + Send + Sync {}
impl<T: AsyncRead + AsyncWrite + Unpin + Send + Sync> Socket for T {}

impl Connector for HttpConnector {
    type Out = HttpTransport;
    fn connect(
        &self,
        details: &ConnectionDetails,
        _: Option<()>,
    ) -> std::result::Result<Option<HttpTransport>, ureq::Error> {
        let url =
            url::Url::parse(&details.uri.to_string()).map_err(|_| ureq::Error::ConnectionFailed)?;
        let host = url
            .host_str()
            .ok_or(ureq::Error::ConnectionFailed)?
            .trim_matches(['[', ']'])
            .to_owned();
        let port = url
            .port_or_known_default()
            .ok_or(ureq::Error::ConnectionFailed)?;
        let stream: Box<dyn Socket> = wait(&self.handle, details.timeout, &self.stop, async {
            if let Some(tunnel) = &self.tunnel {
                Ok(Box::new(
                    tunnel
                        .open(host, port)
                        .await
                        .map_err(|_| io::Error::other("SSH channel failed"))?,
                ) as Box<dyn Socket>)
            } else {
                Ok(
                    Box::new(tokio::net::TcpStream::connect((host.as_str(), port)).await?)
                        as Box<dyn Socket>,
                )
            }
        })?;
        Ok(Some(HttpTransport {
            stream,
            handle: self.handle.clone(),
            buffers: LazyBuffers::new(16384, 16384),
            stop: self.stop.clone(),
        }))
    }
}

pub(crate) struct HttpTransport {
    handle: tokio::runtime::Handle,
    stream: Box<dyn Socket>,
    buffers: LazyBuffers,
    pub(crate) stop: CancellationToken,
}
impl fmt::Debug for HttpTransport {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("HttpTransport")
    }
}
impl Transport for HttpTransport {
    fn buffers(&mut self) -> &mut dyn Buffers {
        &mut self.buffers
    }
    fn transmit_output(
        &mut self,
        amount: usize,
        timeout: NextTimeout,
    ) -> std::result::Result<(), ureq::Error> {
        wait(&self.handle, timeout, &self.stop, async {
            self.stream
                .write_all(&self.buffers.output()[..amount])
                .await?;
            self.stream.flush().await
        })
    }
    fn await_input(&mut self, timeout: NextTimeout) -> std::result::Result<bool, ureq::Error> {
        let count = wait(
            &self.handle,
            timeout,
            &self.stop,
            self.stream.read(self.buffers.input_append_buf()),
        )?;
        self.buffers.input_appended(count);
        Ok(count > 0)
    }
    // Requests own their transports; a cancelled/stale connection is never reused.
    fn is_open(&mut self) -> bool {
        false
    }
}

fn wait<T>(
    handle: &tokio::runtime::Handle,
    timeout: NextTimeout,
    stop: &CancellationToken,
    operation: impl Future<Output = io::Result<T>>,
) -> std::result::Result<T, ureq::Error> {
    let duration = timeout
        .not_zero()
        .map(|duration| *duration)
        .unwrap_or(Duration::from_secs(30));
    handle.block_on(async {
        tokio::select! {
            biased;
            _ = stop.cancelled() => Err(ureq::Error::Io(io::Error::new(io::ErrorKind::Interrupted, "Request cancelled"))),
            result = tokio::time::timeout(duration, operation) => match result {
                Ok(result) => result.map_err(ureq::Error::Io),
                Err(_) => Err(ureq::Error::Timeout(timeout.reason)),
            },
        }
    })
}
