use super::transport::TunnelBridge;
use super::{Collector, DatabaseError, Result, ResultSet, classify};
use crate::{
    config::{Secret, SqlConfig, SqlEngine},
    session::TunnelConnector,
};
use futures_util::TryStreamExt;
use sqlx::{
    Column, ConnectOptions, Connection as _, Decode, Either, MySql, Row, TypeInfo, ValueRef,
    mysql::{MySqlConnectOptions, MySqlSslMode},
};
use tokio_util::sync::CancellationToken;

pub(super) struct Connection {
    connection: sqlx::MySqlConnection,
    options: MySqlConnectOptions,
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
                Some(TunnelBridge::open(SqlEngine::Mysql, &config, tunnel, stop).await?)
            }
            None => None,
        };
        let mut options = MySqlConnectOptions::new()
            .host(&host)
            .port(config.port)
            .username(&config.username)
            .password(secret.expose())
            .ssl_mode(if config.tls {
                MySqlSslMode::VerifyIdentity
            } else {
                MySqlSslMode::Disabled
            })
            .statement_cache_capacity(0)
            .charset("utf8mb4")
            .timezone(None)
            .pipes_as_concat(false)
            .no_engine_substitution(false)
            .enable_cleartext_plugin(false)
            .disable_statement_logging();
        if !config.database.is_empty() {
            options = options.database(&config.database);
        }
        if let Some(bridge) = &bridge {
            options = options.socket(bridge.socket());
        }
        let connection = sqlx::MySqlConnection::connect_with(&options)
            .await
            .map_err(classify)?;
        Ok(Self {
            connection,
            options,
            _bridge: bridge,
        })
    }
    pub(super) fn cancellation(&self, marker: String) -> super::CancelHandle {
        super::CancelHandle::Mysql(self.options.clone(), marker)
    }
    pub(super) async fn synchronize(&mut self) -> Result<()> {
        self.connection.flush().await.map_err(classify)
    }
    pub async fn execute(&mut self, sql: &str, limit: usize) -> Result<Vec<ResultSet>> {
        // The editor intentionally accepts SQL. Generated metadata queries escape identifiers and values.
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
                            let bytes = <&[u8] as Decode<MySql>>::decode(value)
                                .map_err(|_| DatabaseError::Protocol)?;
                            let binary = matches!(
                                row.columns()[index].type_info().name(),
                                "BIT"
                                    | "BINARY"
                                    | "VARBINARY"
                                    | "TINYBLOB"
                                    | "BLOB"
                                    | "MEDIUMBLOB"
                                    | "LONGBLOB"
                                    | "GEOMETRY"
                            );
                            Ok(Some(if binary {
                                format!("0x{}", super::hex(bytes))
                            } else {
                                super::display_bytes(bytes)
                            }))
                        })
                        .collect::<Result<Vec<_>>>()?;
                    output.row(values);
                }
            }
        }
        Ok(output.finish())
    }
}
