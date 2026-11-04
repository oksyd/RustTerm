use super::transport::TunnelBridge;
use super::{Collector, DatabaseError, Result, ResultSet, classify};
use crate::{
    config::{Secret, SqlConfig, SqlEngine},
    session::TunnelConnector,
};
use futures_util::TryStreamExt;
use sqlx::{
    Column, ConnectOptions, Connection as _, Either, Row, ValueRef,
    postgres::{PgConnectOptions, PgSslMode},
};
use tokio_util::sync::CancellationToken;

pub(super) struct Connection {
    connection: sqlx::PgConnection,
    options: PgConnectOptions,
    _bridge: Option<TunnelBridge>,
}
impl Connection {
    pub async fn open(
        config: SqlConfig,
        secret: Secret,
        tunnel: Option<TunnelConnector>,
        stop: CancellationToken,
    ) -> Result<Self> {
        let host = crate::config::known_hosts::normalize_host(&config.host)
            .map_err(|_| DatabaseError::InvalidConfig)?;
        let bridge = match tunnel {
            Some(tunnel) => {
                Some(TunnelBridge::open(SqlEngine::Postgres, &config, tunnel, stop).await?)
            }
            None => None,
        };
        let mut options = PgConnectOptions::new_without_pgpass()
            .host(&host)
            .port(config.port)
            .username(&config.username)
            .password(secret.expose())
            .database(if config.database.is_empty() {
                &config.username
            } else {
                &config.database
            })
            .ssl_mode(if config.tls {
                PgSslMode::VerifyFull
            } else {
                PgSslMode::Disable
            })
            .statement_cache_capacity(0)
            .application_name("RustTerm")
            .disable_statement_logging();
        if let Some(bridge) = &bridge {
            options = options.socket(bridge.directory());
        }
        let connection = sqlx::PgConnection::connect_with(&options)
            .await
            .map_err(classify)?;
        Ok(Self {
            connection,
            options,
            _bridge: bridge,
        })
    }
    pub(super) fn cancellation(&self, marker: String) -> super::CancelHandle {
        super::CancelHandle::Postgres(self.options.clone(), marker)
    }
    pub(super) async fn synchronize(&mut self) -> Result<()> {
        self.connection.flush().await.map_err(classify)
    }
    pub async fn execute(&mut self, sql: &str, limit: usize) -> Result<Vec<ResultSet>> {
        let mut stream = sqlx::raw_sql(sqlx::AssertSqlSafe(sql)).fetch_many(&mut self.connection);
        let mut output = Collector::new(limit);
        while let Some(message) = stream.try_next().await.map_err(classify)? {
            match message {
                Either::Left(done) => output.complete(done.rows_affected())?,
                Either::Right(row) => {
                    if !output.retaining_rows() {
                        continue;
                    }
                    if output.needs_columns() {
                        output.columns(
                            row.columns()
                                .iter()
                                .map(|column| column.name().to_owned())
                                .collect(),
                        )?;
                    }
                    let values = (0..row.len())
                        .map(|index| {
                            let value = row.try_get_raw(index).map_err(classify)?;
                            if value.is_null() {
                                return Ok(None);
                            }
                            Ok(Some(
                                value
                                    .as_str()
                                    .map_err(|_| DatabaseError::Protocol)?
                                    .to_owned(),
                            ))
                        })
                        .collect::<Result<Vec<_>>>()?;
                    output.row(values);
                }
            }
        }
        Ok(output.finish())
    }
}
