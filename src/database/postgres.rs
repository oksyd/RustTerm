use super::{
    Collector, DatabaseError, Result, ResultSet,
    transport::{self, Forward, Route},
};
use crate::{
    config::{Secret, SqlConfig},
    session::TunnelConnector,
};
use futures_util::TryStreamExt;
use std::{error::Error as _, sync::Arc};
use tokio::task::JoinHandle;
use tokio_postgres::{Config, SimpleQueryMessage, config::SslMode};
use tokio_postgres_rustls::MakeRustlsConnect;
use tokio_util::sync::CancellationToken;

#[derive(Clone)]
pub(super) struct Options {
    driver: Arc<Config>,
    pub(super) tls: MakeRustlsConnect,
    route: Route,
}

pub(super) struct Connection {
    client: tokio_postgres::Client,
    worker: JoinHandle<()>,
    options: Options,
    _forward: Option<Forward>,
}

impl Options {
    pub(super) async fn connect(&self) -> Result<Connection> {
        self.connect_forwarded(self.route.forward().await?).await
    }

    pub(super) async fn connect_forwarded(&self, forward: Option<Forward>) -> Result<Connection> {
        let mut options = self.driver.as_ref().clone();
        options.port(
            forward
                .as_ref()
                .map_or(self.route.port(), |forward| forward.address.port()),
        );
        if let Some(forward) = &forward {
            options.hostaddr(forward.address.ip());
        }
        let (client, connection) = options.connect(self.tls.clone()).await.map_err(classify)?;
        let worker = tokio::spawn(async move {
            let _ = connection.await;
        });
        Ok(Connection {
            client,
            worker,
            options: self.clone(),
            _forward: forward,
        })
    }
}

impl Drop for Connection {
    fn drop(&mut self) {
        self.worker.abort();
    }
}

impl Connection {
    pub async fn open(
        config: SqlConfig,
        secret: Secret,
        tunnel: Option<TunnelConnector>,
        stop: CancellationToken,
    ) -> Result<Self> {
        Self::options(config, secret, tunnel, stop)?.connect().await
    }

    pub(super) fn options(
        config: SqlConfig,
        secret: Secret,
        tunnel: Option<TunnelConnector>,
        stop: CancellationToken,
    ) -> Result<Options> {
        let host = crate::config::known_hosts::normalize_host(&config.host)
            .map_err(|_| DatabaseError::InvalidConfig)?;
        let mut driver = Config::new();
        driver
            .host(&host)
            .user(&config.username)
            .password(secret.expose())
            .dbname(if config.database.is_empty() {
                &config.username
            } else {
                &config.database
            })
            .ssl_mode(if config.tls {
                SslMode::Require
            } else {
                SslMode::Disable
            })
            .application_name("RustTerm");
        let mut roots = rustls::RootCertStore::empty();
        if config.tls {
            roots.add_parsable_certificates(transport::native_roots());
        }
        let tls = MakeRustlsConnect::new(
            rustls::ClientConfig::builder()
                .with_root_certificates(roots)
                .with_no_client_auth(),
        );
        Ok(Options {
            driver: Arc::new(driver),
            tls,
            route: Route::new(tunnel, host, config.port, stop),
        })
    }

    pub(super) fn cancellation(&self, marker: String) -> super::CancelHandle {
        super::CancelHandle::Postgres(self.options.clone(), marker)
    }

    pub(super) async fn synchronize(&mut self) -> Result<()> {
        self.client.batch_execute("").await.map_err(classify)
    }

    pub async fn execute(&mut self, sql: &str, limit: usize) -> Result<Vec<ResultSet>> {
        let stream = self.client.simple_query_raw(sql).await.map_err(classify)?;
        tokio::pin!(stream);
        let mut output = Collector::new(limit);
        while let Some(message) = stream.try_next().await.map_err(classify)? {
            match message {
                SimpleQueryMessage::RowDescription(columns) => output.columns(
                    columns
                        .iter()
                        .map(|column| column.name().to_owned())
                        .collect(),
                )?,
                SimpleQueryMessage::Row(row) if output.retaining_rows() => {
                    let values = (0..row.len())
                        .map(|index| {
                            row.try_get(index)
                                .map(|value| value.map(str::to_owned))
                                .map_err(classify)
                        })
                        .collect::<Result<Vec<_>>>()?;
                    output.row(values);
                }
                SimpleQueryMessage::Row(_) => {}
                SimpleQueryMessage::CommandComplete(count) => output.complete(count)?,
                _ => return Err(DatabaseError::Protocol),
            }
        }
        Ok(output.finish())
    }
}

fn classify(error: tokio_postgres::Error) -> DatabaseError {
    if transport::is_tls_error(&error) {
        return DatabaseError::Tls;
    }
    if let Some(error) = error.as_db_error() {
        return if error.code().code().starts_with("28") {
            DatabaseError::Authentication
        } else {
            super::server_error(error.code().code().into(), error.message())
        };
    }
    let mut source = error.source();
    let mut io = false;
    while let Some(error) = source {
        io |= error.is::<std::io::Error>();
        source = error.source();
    }
    if error.is_closed() || io {
        DatabaseError::Disconnected
    } else {
        DatabaseError::Protocol
    }
}
