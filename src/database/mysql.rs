use super::{
    Collector, DatabaseError, Result, ResultSet,
    transport::{self, Forward, Route},
};
use crate::{
    config::{Secret, SqlConfig},
    session::TunnelConnector,
};
use futures_util::TryStreamExt;
use mysql_async::{
    Conn, Opts, OptsBuilder, Row, SslOpts, Value, consts::ColumnType, prelude::Queryable,
};
use tokio_util::sync::CancellationToken;

#[derive(Clone)]
pub(super) struct Options {
    pub(super) driver: Opts,
    route: Route,
}

pub(super) struct Connection {
    connection: Option<Conn>,
    options: Options,
    _forward: Option<Forward>,
}

impl Options {
    pub(super) async fn connect(&self) -> Result<Connection> {
        self.connect_forwarded(self.route.forward().await?).await
    }

    pub(super) async fn connect_forwarded(&self, forward: Option<Forward>) -> Result<Connection> {
        let mut options = OptsBuilder::from_opts(self.driver.clone());
        if let Some(forward) = &forward {
            options = options
                .tcp_port(forward.address.port())
                .resolved_ips(Some(vec![forward.address.ip()]));
        }
        let connection = Conn::new(options).await.map_err(classify)?;
        Ok(Connection {
            connection: Some(connection),
            options: self.clone(),
            _forward: forward,
        })
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
        let mut options = OptsBuilder::default()
            .ip_or_hostname(&host)
            .tcp_port(config.port)
            .user(Some(config.username))
            .pass(Some(secret.expose().to_owned()))
            .client_found_rows(true)
            .prefer_socket(false)
            .stmt_cache_size(Some(0))
            .setup(vec!["SET NAMES utf8mb4"]);
        if !config.database.is_empty() {
            options = options.db_name(Some(config.database));
        }
        if config.tls {
            options = options.ssl_opts(Some(
                SslOpts::default()
                    .with_disable_built_in_roots(true)
                    .with_root_certs(
                        transport::native_roots()
                            .into_iter()
                            .map(|cert| cert.to_vec().into())
                            .collect(),
                    ),
            ));
        }
        Ok(Options {
            driver: options.into(),
            route: Route::new(tunnel, host, config.port, stop),
        })
    }

    pub(super) fn monitor_options(&self) -> Options {
        self.options.clone()
    }

    pub(super) async fn open_monitor(options: Options) -> Result<Self> {
        options.connect().await
    }

    pub(super) fn cancellation(&self, marker: String) -> super::CancelHandle {
        super::CancelHandle::Mysql(self.options.clone(), marker)
    }

    pub(super) async fn synchronize(&mut self) -> Result<()> {
        self.connection
            .as_mut()
            .ok_or(DatabaseError::Disconnected)?
            .ping()
            .await
            .map_err(classify)
    }

    pub(super) async fn close(mut self) {
        if let Some(connection) = self.connection.take() {
            disconnect(connection).await;
        }
    }

    pub async fn execute(&mut self, sql: &str, limit: usize) -> Result<Vec<ResultSet>> {
        let mut query = self
            .connection
            .as_mut()
            .ok_or(DatabaseError::Disconnected)?
            .query_iter(sql)
            .await
            .map_err(classify)?;
        let mut output = Collector::new(limit);
        while let Some(mut set) = query.stream::<Row>().await.map_err(classify)? {
            output.columns(
                set.columns_ref()
                    .iter()
                    .map(|column| column.name_str().into_owned())
                    .collect(),
            )?;
            while let Some(row) = set.try_next().await.map_err(classify)? {
                if !output.retaining_rows() {
                    continue;
                }
                let values = row
                    .columns_ref()
                    .iter()
                    .enumerate()
                    .map(|(index, column)| {
                        display_value(column, row.as_ref(index).ok_or(DatabaseError::Protocol)?)
                    })
                    .collect::<Result<Vec<_>>>()?;
                output.row(values);
            }
            output.complete(set.affected_rows())?;
        }
        Ok(output.finish())
    }
}

impl Drop for Connection {
    fn drop(&mut self) {
        if let Some(connection) = self.connection.take()
            && let Ok(runtime) = tokio::runtime::Handle::try_current()
        {
            runtime.spawn(disconnect(connection));
        }
    }
}

async fn disconnect(connection: Conn) {
    let _ = tokio::time::timeout(std::time::Duration::from_secs(5), connection.disconnect()).await;
}

fn display_value(column: &mysql_async::Column, value: &Value) -> Result<Option<String>> {
    match value {
        Value::NULL => Ok(None),
        Value::Bytes(bytes) => Ok(Some(
            if matches!(
                column.column_type(),
                ColumnType::MYSQL_TYPE_BIT | ColumnType::MYSQL_TYPE_GEOMETRY
            ) {
                format!("0x{}", super::hex(bytes))
            } else if binary_column(column) {
                super::display_blob(bytes)
            } else {
                super::display_bytes(bytes)
            },
        )),
        _ => Err(DatabaseError::Protocol),
    }
}

fn binary_column(column: &mysql_async::Column) -> bool {
    // The binary character set distinguishes BLOB from TEXT with a binary collation.
    column.character_set() == 63
        && matches!(
            column.column_type(),
            ColumnType::MYSQL_TYPE_STRING
                | ColumnType::MYSQL_TYPE_VAR_STRING
                | ColumnType::MYSQL_TYPE_VARCHAR
                | ColumnType::MYSQL_TYPE_TINY_BLOB
                | ColumnType::MYSQL_TYPE_BLOB
                | ColumnType::MYSQL_TYPE_MEDIUM_BLOB
                | ColumnType::MYSQL_TYPE_LONG_BLOB
        )
}

fn classify(error: mysql_async::Error) -> DatabaseError {
    use mysql_async::{DriverError, Error, IoError};
    if transport::is_tls_error(&error) {
        return DatabaseError::Tls;
    }
    match error {
        Error::Server(error) if error.state.starts_with("28") => DatabaseError::Authentication,
        Error::Server(error) => super::server_error(error.state, &error.message),
        Error::Io(IoError::Tls(_)) | Error::Driver(DriverError::NoClientSslFlagFromServer) => {
            DatabaseError::Tls
        }
        Error::Io(_) | Error::Driver(DriverError::ConnectionClosed) => DatabaseError::Disconnected,
        Error::Url(_) => DatabaseError::InvalidConfig,
        _ => DatabaseError::Protocol,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use mysql_async::{Column, consts::ColumnFlags};

    #[test]
    fn empty_binary_strings_and_typed_binary_values_keep_their_identity() {
        use ColumnType::*;
        for (kind, charset, bytes, expected) in [
            (MYSQL_TYPE_BLOB, 63, b"".as_slice(), "0x"),
            (MYSQL_TYPE_BLOB, 46, b"", ""),
            (MYSQL_TYPE_BLOB, 46, b"0x41", "0x41"),
            (MYSQL_TYPE_BIT, 63, b"A", "0x41"),
            (MYSQL_TYPE_GEOMETRY, 63, b"A", "0x41"),
        ] {
            let column = Column::new(kind)
                .with_character_set(charset)
                .with_flags(ColumnFlags::BINARY_FLAG);
            assert_eq!(
                display_value(&column, &Value::Bytes(bytes.to_vec()))
                    .unwrap()
                    .as_deref(),
                Some(expected)
            );
        }
    }
}
