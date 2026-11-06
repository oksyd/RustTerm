use super::{ElasticsearchError, Result, runtime};
use crate::{config::ElasticsearchConfig, session::TunnelConnector};
use std::{fmt, future::Future, io, time::Duration};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio_util::sync::CancellationToken;
use ureq::unversioned::{
    resolver::{ResolvedSocketAddrs, Resolver},
    transport::{
        Buffers, ConnectionDetails, Connector, LazyBuffers, NextTimeout, RustlsConnector, Transport,
    },
};

pub(super) fn agent(
    config: &ElasticsearchConfig,
    tunnel: Option<TunnelConnector>,
    stop: CancellationToken,
) -> Result<ureq::Agent> {
    let mut certs: Vec<_> = rustls_native_certs::load_native_certs()
        .certs
        .into_iter()
        .map(|cert| ureq::tls::Certificate::from_der(cert.as_ref()).to_owned())
        .collect();
    if let Some(path) = &config.ca_file {
        use std::io::Read;
        let mut pem = Vec::new();
        std::fs::File::open(path)
            .and_then(|file| file.take(1024 * 1024 + 1).read_to_end(&mut pem))
            .map_err(|_| ElasticsearchError::Certificate)?;
        if pem.len() > 1024 * 1024 {
            return Err(ElasticsearchError::Certificate);
        }
        let mut count = 0;
        for item in ureq::tls::parse_pem(&pem) {
            match item.map_err(|_| ElasticsearchError::Certificate)? {
                ureq::tls::PemItem::Certificate(cert) => {
                    certs.push(cert);
                    count += 1;
                }
                _ => return Err(ElasticsearchError::Certificate),
            }
        }
        if count == 0 {
            return Err(ElasticsearchError::Certificate);
        }
    }
    let roots = if certs.is_empty() {
        ureq::tls::RootCerts::WebPki
    } else {
        certs.into()
    };
    let config = ureq::Agent::config_builder()
        .proxy(None)
        .max_redirects(0)
        .http_status_as_error(false)
        .timeout_global(Some(Duration::from_secs(config.query_timeout_secs)))
        .timeout_connect(Some(Duration::from_secs(config.timeout_secs)))
        .tls_config(ureq::tls::TlsConfig::builder().root_certs(roots).build())
        .build();
    Ok(ureq::Agent::with_parts(
        config,
        HttpConnector { tunnel, stop }.chain(RustlsConnector::default()),
        RemoteResolver,
    ))
}

/// DNS is performed by the connector (or SSH server), never by the HTTP layer.
#[derive(Debug)]
struct RemoteResolver;
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

struct HttpConnector {
    tunnel: Option<TunnelConnector>,
    stop: CancellationToken,
}
impl fmt::Debug for HttpConnector {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("ElasticsearchConnector")
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
        let stream: Box<dyn Socket> = wait(details.timeout, &self.stop, async {
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
            buffers: LazyBuffers::new(16384, 16384),
            stop: self.stop.clone(),
        }))
    }
}

struct HttpTransport {
    stream: Box<dyn Socket>,
    buffers: LazyBuffers,
    stop: CancellationToken,
}
impl fmt::Debug for HttpTransport {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("ElasticsearchTransport")
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
        wait(timeout, &self.stop, async {
            self.stream
                .write_all(&self.buffers.output()[..amount])
                .await?;
            self.stream.flush().await
        })
    }
    fn await_input(&mut self, timeout: NextTimeout) -> std::result::Result<bool, ureq::Error> {
        let count = wait(
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
    timeout: NextTimeout,
    stop: &CancellationToken,
    operation: impl Future<Output = io::Result<T>>,
) -> std::result::Result<T, ureq::Error> {
    let duration = timeout
        .not_zero()
        .map(|duration| *duration)
        .unwrap_or(Duration::from_secs(30));
    runtime().map_err(|_| ureq::Error::ConnectionFailed)?.block_on(async {
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

pub(super) fn error(error: ureq::Error, stop: &CancellationToken) -> ElasticsearchError {
    if stop.is_cancelled() {
        ElasticsearchError::Cancelled
    } else if matches!(error, ureq::Error::Timeout(_)) {
        ElasticsearchError::Timeout
    } else if matches!(error, ureq::Error::BodyExceedsLimit(_)) {
        ElasticsearchError::TooLarge
    } else {
        ElasticsearchError::Network
    }
}
