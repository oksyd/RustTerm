use super::{DatabaseError, Result};
use crate::{
    config::{SqlConfig, SqlEngine},
    session::TunnelConnector,
};
use std::path::{Path, PathBuf};
use tokio::{
    net::UnixListener,
    task::{JoinHandle, JoinSet},
};
use tokio_util::sync::CancellationToken;

/// A private Unix socket preserves the database hostname for TLS verification.
/// The session and its short-lived cancellation connection use the same route.
pub(super) struct TunnelBridge {
    directory: tempfile::TempDir,
    socket: PathBuf,
    task: JoinHandle<()>,
}
impl TunnelBridge {
    pub async fn open(
        engine: SqlEngine,
        config: &SqlConfig,
        tunnel: TunnelConnector,
        stop: CancellationToken,
    ) -> Result<Self> {
        let host = crate::config::known_hosts::normalize_host(&config.host)
            .map_err(|_| DatabaseError::InvalidConfig)?;
        let directory = tempfile::Builder::new()
            .prefix("rustterm-sql-")
            .tempdir()
            .map_err(|_| DatabaseError::SshConnection)?;
        let name = match engine {
            SqlEngine::Mysql => "mysql.sock".to_owned(),
            SqlEngine::Postgres => format!(".s.PGSQL.{}", config.port),
            SqlEngine::Sqlite => return Err(DatabaseError::InvalidConfig),
        };
        let socket = directory.path().join(name);
        let listener = UnixListener::bind(&socket).map_err(|_| DatabaseError::SshConnection)?;
        let port = config.port;
        let task = tokio::spawn(async move {
            let mut streams = JoinSet::new();
            loop {
                tokio::select! {
                    biased;
                    _ = stop.cancelled() => break,
                    _ = streams.join_next(), if !streams.is_empty() => {},
                    accepted = listener.accept(), if streams.len() < 2 => {
                        let Ok((mut local, _)) = accepted else { break };
                        let tunnel = tunnel.clone();
                        let host = host.clone();
                        streams.spawn(async move {
                            let Ok(mut remote) = tunnel.open(host, port).await else { return };
                            let _ = tokio::io::copy_bidirectional(&mut local, &mut remote).await;
                        });
                    }
                }
            }
        });
        Ok(Self {
            directory,
            socket,
            task,
        })
    }
    pub fn directory(&self) -> &Path {
        self.directory.path()
    }
    pub fn socket(&self) -> &Path {
        &self.socket
    }
}
impl Drop for TunnelBridge {
    fn drop(&mut self) {
        self.task.abort();
    }
}
