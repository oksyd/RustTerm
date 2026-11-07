//! ClickHouse HTTP sessions. Requests are never retried or redirected.
use super::*;
use crate::{
    config::SqlConfig,
    http_transport::{HttpConnector, RemoteResolver},
    session::TunnelConnector,
};
use base64::Engine as _;
mod response;
use response::{parse_response, parse_tsv};
use ureq::unversioned::transport::{Connector, RustlsConnector};

#[derive(Clone)]
pub(super) struct Connection {
    config: SqlConfig,
    authorization: Arc<Secret>,
    tunnel: Option<TunnelConnector>,
    stop: CancellationToken,
    session: String,
    server_limits: bool,
    active: Arc<Mutex<Option<CancellationToken>>>,
}
impl Connection {
    pub(super) async fn open(
        config: SqlConfig,
        secret: Secret,
        tunnel: Option<TunnelConnector>,
        stop: CancellationToken,
    ) -> Result<Self> {
        // Basic auth is encoded in a sensitive header, never in the URL.
        if config.username.contains(':') {
            return Err(DatabaseError::InvalidConfig);
        }
        let pair = Secret::new(format!("{}:{}", config.username, secret.expose()));
        let authorization = Secret::new(format!(
            "Basic {}",
            base64::engine::general_purpose::STANDARD.encode(pair.expose())
        ));
        let mut connection = Self {
            config,
            authorization: Arc::new(authorization),
            tunnel,
            stop,
            session: uuid::Uuid::new_v4().to_string(),
            server_limits: true,
            active: Arc::new(Mutex::new(None)),
        };
        let opening = connection
            .request(
                "SELECT version()".into(),
                1,
                uuid::Uuid::new_v4().to_string(),
                RequestMode::Open,
            )
            .await;
        let result = match opening {
            // Only retry the read-only handshake, never submitted user SQL.
            // Strict read-only profiles prohibit changing output/limit settings.
            Err(DatabaseError::Server { ref code, .. }) if code == "164" => {
                connection.server_limits = false;
                connection
                    .request(
                        "SELECT version()".into(),
                        1,
                        uuid::Uuid::new_v4().to_string(),
                        RequestMode::Open,
                    )
                    .await?
            }
            result => result?,
        };
        if !result
            .rows
            .first()
            .and_then(|row| row.first())
            .and_then(Option::as_ref)
            .is_some_and(|v| !v.is_empty())
        {
            return Err(DatabaseError::Protocol);
        }
        if !connection.config.database.is_empty() {
            connection
                .request(
                    format!(
                        "USE {}",
                        quote_identifier(SqlEngine::ClickHouse, &connection.config.database)
                    ),
                    1,
                    uuid::Uuid::new_v4().to_string(),
                    RequestMode::Query,
                )
                .await?;
        }
        Ok(connection)
    }
    pub(super) async fn execute(
        &self,
        sql: &str,
        limit: usize,
        cancel: &CancellationToken,
        marker: &str,
    ) -> Result<Vec<ResultSet>> {
        let statements = sql::statements(sql, SqlEngine::ClickHouse)
            .map_err(|_| DatabaseError::InvalidConfig)?;
        if statements.len() > MAX_RESULT_SETS {
            return Err(DatabaseError::Limit);
        }
        if statements.iter().any(|statement| {
            matches!(
                sql::leading_words(&sql[statement.range.clone()], SqlEngine::ClickHouse)
                    .first()
                    .map(String::as_str),
                Some("BEGIN" | "START" | "COMMIT" | "ROLLBACK" | "SAVEPOINT" | "RELEASE")
            )
        }) {
            return Err(DatabaseError::TransactionsUnsupported);
        }

        let mut results = Vec::new();
        let mut bytes = 0;
        for statement in statements {
            if cancel.is_cancelled() {
                return Err(DatabaseError::Cancelled);
            }
            let result = self
                .request(
                    sql[statement.range].to_owned(),
                    limit,
                    marker.to_owned(),
                    RequestMode::Query,
                )
                .await;
            let result = match result {
                Err(DatabaseError::Limit) => {
                    // The HTTP body was stopped at the local byte cap. Confirm
                    // server cancellation before reusing this session.
                    match tokio::time::timeout(Duration::from_secs(5), self.kill(marker)).await {
                        Ok(Ok(_)) => return Err(DatabaseError::Limit),
                        _ => return Err(DatabaseError::OutcomeUnknown),
                    }
                }
                result => result?,
            };
            bytes += result.columns.iter().map(String::len).sum::<usize>()
                + result
                    .rows
                    .iter()
                    .flatten()
                    .flatten()
                    .map(String::len)
                    .sum::<usize>();
            if bytes > MAX_RESULT_BYTES {
                return Err(DatabaseError::Limit);
            }
            results.push(result);
        }
        Ok(results)
    }
    pub(super) fn cancellation(&self, marker: String) -> CancelHandle {
        CancelHandle::ClickHouse(self.clone(), marker)
    }
    pub(super) async fn cancel(&self, marker: &str) -> Result<bool> {
        let Some(done) = self.active.lock().unwrap().clone() else {
            return Ok(false);
        };
        // A cancel can arrive while the HTTP request is still being submitted.
        // Keep looking until that exact query is visible, or its request finishes.
        loop {
            if done.is_cancelled() {
                return Ok(false);
            }
            let output = self.kill(marker).await?;
            if !output.rows.is_empty() {
                return Ok(true);
            }
            tokio::select! {
                _ = done.cancelled() => return Ok(false),
                _ = tokio::time::sleep(Duration::from_millis(50)) => {},
            }
        }
    }

    async fn kill(&self, marker: &str) -> Result<ResultSet> {
        let sql = format!(
            "KILL QUERY WHERE query_id = {} SYNC",
            quote_literal(SqlEngine::ClickHouse, marker)
        );
        self.request(
            sql,
            1,
            uuid::Uuid::new_v4().to_string(),
            RequestMode::Control,
        )
        .await
    }
    async fn request(
        &self,
        sql: String,
        limit: usize,
        query_id: String,
        mode: RequestMode,
    ) -> Result<ResultSet> {
        let completion = if matches!(mode, RequestMode::Query) {
            let done = CancellationToken::new();
            *self.active.lock().unwrap() = Some(done.clone());
            Some(CancelOnDrop(Some(done)))
        } else {
            None
        };
        let this = self.clone();
        let stop = self.stop.child_token();
        let _guard = CancelOnDrop(Some(stop.clone()));
        tokio::task::spawn_blocking(move || {
            let _completion = completion;
            this.blocking_request(sql, limit, query_id, mode, stop)
        })
        .await
        .map_err(|_| DatabaseError::Runtime)?
    }
    fn blocking_request(
        &self,
        sql: String,
        limit: usize,
        query_id: String,
        mode: RequestMode,
        stop: CancellationToken,
    ) -> Result<ResultSet> {
        let mut url = url::Url::parse(if self.config.tls {
            "https://localhost/"
        } else {
            "http://localhost/"
        })
        .map_err(|_| DatabaseError::InvalidConfig)?;
        let host = crate::config::known_hosts::normalize_host(&self.config.host)
            .map_err(|_| DatabaseError::InvalidConfig)?;
        let host = if host.contains(':') {
            format!("[{host}]")
        } else {
            host
        };
        url.set_host(Some(&host))
            .map_err(|_| DatabaseError::InvalidConfig)?;
        url.set_port(Some(self.config.port))
            .map_err(|_| DatabaseError::InvalidConfig)?;
        {
            let mut params = url.query_pairs_mut();
            params
                .append_pair("query_id", &query_id)
                .append_pair(
                    "default_format",
                    if self.server_limits {
                        "JSONCompact"
                    } else {
                        "TabSeparatedWithNamesAndTypes"
                    },
                )
                .append_pair("wait_end_of_query", "1");
            if self.server_limits {
                params
                    .append_pair("output_format_json_quote_denormals", "1")
                    .append_pair("max_result_rows", &(limit + 1).to_string())
                    .append_pair("max_result_bytes", &MAX_RESULT_BYTES.to_string())
                    .append_pair("result_overflow_mode", "break")
                    .append_pair(
                        "max_execution_time",
                        &self.config.query_timeout_secs.to_string(),
                    );
            }
            if !matches!(mode, RequestMode::Control) {
                params
                    .append_pair("session_id", &self.session)
                    .append_pair("session_timeout", "3600");
                if matches!(mode, RequestMode::Open) {
                    if !self.config.database.is_empty() {
                        params.append_pair("database", &self.config.database);
                    }
                } else {
                    params.append_pair("session_check", "1");
                }
            }
        }
        let certs: Vec<_> = rustls_native_certs::load_native_certs()
            .certs
            .into_iter()
            .map(|cert| ureq::tls::Certificate::from_der(cert.as_ref()).to_owned())
            .collect();
        let roots = if certs.is_empty() {
            ureq::tls::RootCerts::WebPki
        } else {
            certs.into()
        };
        let config = ureq::Agent::config_builder()
            .proxy(None)
            .max_redirects(0)
            .http_status_as_error(false)
            .timeout_global(Some(Duration::from_secs(
                if matches!(mode, RequestMode::Control) {
                    4
                } else {
                    self.config.query_timeout_secs.saturating_add(5)
                },
            )))
            .timeout_connect(Some(Duration::from_secs(self.config.timeout_secs)))
            .tls_config(ureq::tls::TlsConfig::builder().root_certs(roots).build())
            .build();
        let agent = ureq::Agent::with_parts(
            config,
            HttpConnector {
                tunnel: self.tunnel.clone(),
                stop: stop.clone(),
                handle: runtime()?.handle().clone(),
            }
            .chain(RustlsConnector::default()),
            RemoteResolver,
        );
        let mut auth = ureq::http::HeaderValue::from_str(self.authorization.expose())
            .map_err(|_| DatabaseError::InvalidConfig)?;
        auth.set_sensitive(true);
        let mut response = agent
            .post(url.as_str())
            .header("Authorization", auth)
            .header("Content-Type", "text/plain; charset=utf-8")
            .send(sql.as_bytes())
            .map_err(|e| transport_error(e, &stop))?;
        let status = response.status().as_u16();
        let code = response
            .headers()
            .get("X-ClickHouse-Exception-Code")
            .and_then(|v| v.to_str().ok())
            .filter(|v| *v != "0")
            .map(str::to_owned);
        let format = response
            .headers()
            .get("X-ClickHouse-Format")
            .and_then(|v| v.to_str().ok())
            .unwrap_or(if self.server_limits {
                "JSONCompact"
            } else {
                "TabSeparatedWithNamesAndTypes"
            })
            .to_owned();
        let summary = response
            .headers()
            .get("X-ClickHouse-Summary")
            .and_then(|v| v.to_str().ok())
            .and_then(|v| serde_json::from_str::<serde_json::Value>(v).ok());
        let counter = |name: &str| {
            summary
                .as_ref()
                .and_then(|v| v.get(name))
                .and_then(|value| {
                    value
                        .as_str()
                        .and_then(|v| v.parse::<u64>().ok())
                        .or_else(|| value.as_u64())
                })
        };
        let affected_rows = counter("written_rows").unwrap_or(0);
        // ClickHouse's byte limit counts internal blocks, not JSON bytes. A
        // byte-limited partial result may have fewer rows than the requested cap.
        let byte_limit_reached =
            counter("result_bytes").is_some_and(|bytes| bytes >= MAX_RESULT_BYTES as u64);
        let body = response
            .body_mut()
            .with_config()
            .limit(MAX_RESULT_BYTES as u64)
            .read_to_vec()
            .map_err(|e| transport_error(e, &stop))?;
        if status == 401 || code.as_deref() == Some("516") {
            return Err(DatabaseError::Authentication);
        }
        match code.as_deref() {
            Some("159") => return Err(DatabaseError::Timeout),
            Some("394") => return Err(DatabaseError::Cancelled),
            Some("372") => return Err(DatabaseError::Disconnected),
            _ => {}
        }
        if !(200..300).contains(&status) || code.is_some() {
            return Err(server_error(
                code.unwrap_or_else(|| status.to_string()),
                &String::from_utf8_lossy(&body),
            ));
        }
        let mut result = match format.as_str() {
            "JSONCompact" => parse_response(&body, limit, affected_rows)?,
            "TabSeparatedWithNamesAndTypes" | "TSVWithNamesAndTypes" => {
                parse_tsv(&body, limit, affected_rows)?
            }
            _ if body.is_empty() => ResultSet {
                affected_rows,
                ..Default::default()
            },
            _ => return Err(DatabaseError::ResultFormat),
        };
        result.truncated |= byte_limit_reached;
        Ok(result)
    }
}
#[derive(Clone, Copy)]
enum RequestMode {
    Open,
    Query,
    Control,
}

fn transport_error(error: ureq::Error, stop: &CancellationToken) -> DatabaseError {
    if stop.is_cancelled() {
        DatabaseError::Cancelled
    } else {
        match error {
            ureq::Error::Timeout(_) => DatabaseError::Timeout,
            ureq::Error::BodyExceedsLimit(_) => DatabaseError::Limit,
            ureq::Error::Rustls(_) | ureq::Error::Tls(_) => DatabaseError::Tls,
            ureq::Error::Io(ref error)
                if error
                    .get_ref()
                    .is_some_and(|source| source.is::<rustls::Error>()) =>
            {
                DatabaseError::Tls
            }
            _ => DatabaseError::Disconnected,
        }
    }
}

#[cfg(test)]
mod tests;
