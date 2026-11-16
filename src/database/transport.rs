use super::{DatabaseError, Result};
use crate::session::TunnelConnector;
use std::net::{Ipv4Addr, SocketAddr};
use tokio::{
    io::{AsyncRead, AsyncWrite, copy_bidirectional},
    net::TcpListener,
    task::JoinHandle,
};
use tokio_util::sync::CancellationToken;

#[derive(Clone)]
pub(super) struct Route {
    tunnel: Option<TunnelConnector>,
    host: String,
    port: u16,
    stop: CancellationToken,
}

impl Route {
    pub fn port(&self) -> u16 {
        self.port
    }

    pub fn new(
        tunnel: Option<TunnelConnector>,
        host: String,
        port: u16,
        stop: CancellationToken,
    ) -> Self {
        Self {
            tunnel,
            host,
            port,
            stop,
        }
    }

    pub async fn forward(&self) -> Result<Option<Forward>> {
        if self.stop.is_cancelled() {
            return Err(DatabaseError::Disconnected);
        }
        let Some(tunnel) = &self.tunnel else {
            return Ok(None);
        };
        let stream = tokio::select! {
            biased;
            _ = self.stop.cancelled() => return Err(DatabaseError::Disconnected),
            stream = tunnel.open(self.host.clone(), self.port) => stream.map_err(|(kind, _)| super::ssh_error(kind))?,
        };
        Forward::start(stream, self.stop.clone()).await.map(Some)
    }
}

pub(super) struct Forward {
    pub address: SocketAddr,
    task: JoinHandle<()>,
}

impl Forward {
    async fn start<S>(mut remote: S, stop: CancellationToken) -> Result<Self>
    where
        S: AsyncRead + AsyncWrite + Unpin + Send + 'static,
    {
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0))
            .await
            .map_err(|_| DatabaseError::Connection)?;
        let address = listener
            .local_addr()
            .map_err(|_| DatabaseError::Connection)?;
        let task = tokio::spawn(async move {
            let forward = async move {
                let (mut local, _) = listener.accept().await?;
                drop(listener);
                local.set_nodelay(true)?;
                copy_bidirectional(&mut local, &mut remote).await?;
                Ok::<_, std::io::Error>(())
            };
            tokio::select! {
                biased;
                _ = stop.cancelled() => {},
                _ = forward => {},
            }
        });
        Ok(Self { address, task })
    }
}

impl Drop for Forward {
    fn drop(&mut self) {
        self.task.abort();
    }
}

pub(super) fn native_roots() -> Vec<rustls::pki_types::CertificateDer<'static>> {
    rustls_native_certs::load_native_certs().certs
}

pub(super) fn is_tls_error(error: &(dyn std::error::Error + 'static)) -> bool {
    let mut current = Some(error);
    while let Some(error) = current {
        if error.is::<rustls::Error>() || error.is::<rustls::pki_types::InvalidDnsNameError>() {
            return true;
        }
        current = error
            .downcast_ref::<std::io::Error>()
            .and_then(|io| {
                io.get_ref()
                    .map(|inner| inner as &(dyn std::error::Error + 'static))
            })
            .or_else(|| error.source());
    }
    false
}

#[cfg(test)]
mod tests;
