//! Read-only MySQL session monitoring on a separate, lazily opened connection.
use super::*;
use sqlx::mysql::MySqlConnectOptions;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Visibility {
    All,
    Own,
    Unknown,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Process {
    pub id: u64,
    pub user: String,
    pub host: String,
    pub database: Option<String>,
    pub command: String,
    /// MySQL reports seconds spent in the current state, not total query time.
    pub seconds: Option<u64>,
    pub state: Option<String>,
    pub sql: Option<String>,
}

pub struct Snapshot {
    pub processes: Vec<Process>,
    pub visibility: Visibility,
    pub truncated: bool,
    pub elapsed: Duration,
}

struct MonitorInner {
    options: MySqlConnectOptions,
    stop: CancellationToken,
    timeout: Duration,
    connection: tokio::sync::Mutex<Option<mysql::Connection>>,
}
impl Drop for MonitorInner {
    fn drop(&mut self) {
        self.stop.cancel();
    }
}
#[derive(Clone)]
pub struct MysqlMonitor(Arc<MonitorInner>);

impl DatabaseClient {
    pub fn mysql_monitor(&self) -> Result<MysqlMonitor> {
        if !self.is_connected() {
            return Err(DatabaseError::Disconnected);
        }
        let options = self
            .0
            .mysql_options
            .clone()
            .ok_or(DatabaseError::InvalidConfig)?;
        let inner = Arc::new(MonitorInner {
            options,
            stop: self.0.stop.child_token(),
            timeout: self.0.monitor_timeout,
            connection: tokio::sync::Mutex::new(None),
        });
        let weak = Arc::downgrade(&inner);
        let stop = inner.stop.clone();
        runtime()?.spawn(async move {
            stop.cancelled().await;
            if let Some(inner) = weak.upgrade() {
                inner.connection.lock().await.take();
            }
        });
        Ok(MysqlMonitor(inner))
    }
}

impl MysqlMonitor {
    pub fn close(&self) {
        self.0.stop.cancel();
    }
    pub async fn snapshot(&self) -> Result<Snapshot> {
        if self.0.stop.is_cancelled() {
            return Err(DatabaseError::Disconnected);
        }
        let inner = self.0.clone();
        let cancel = CancellationToken::new();
        let _guard = CancelOnDrop(Some(cancel.clone()));
        let (reply, response) = oneshot::channel();
        runtime()?.spawn(async move {
            let operation = async {
                let mut slot = inner.connection.try_lock().map_err(|_| DatabaseError::Busy)?;
                // Taking ownership ensures a cancelled/failed read never leaves
                // a partially consumed protocol stream in the connection cache.
                let mut connection = match slot.take() {
                    Some(connection) => connection,
                    None => tokio::time::timeout(inner.timeout, mysql::Connection::open_monitor(inner.options.clone()))
                        .await.map_err(|_| DatabaseError::Timeout)??,
                };
                let started = Instant::now();
                let output = connection.execute("SHOW FULL PROCESSLIST", MAX_ROWS).await?;
                let mut snapshot = decode(output)?;
                snapshot.visibility = inspect_visibility(&mut connection).await;
                snapshot.elapsed = started.elapsed();
                *slot = Some(connection);
                Ok(snapshot)
            };
            let result = tokio::select! {
                biased;
                _ = inner.stop.cancelled() => Err(DatabaseError::Disconnected),
                _ = cancel.cancelled() => Err(DatabaseError::Cancelled),
                result = tokio::time::timeout(inner.timeout + Duration::from_secs(5), operation) => result.unwrap_or(Err(DatabaseError::Timeout)),
            };
            let _ = reply.send(result);
        });
        response.await.map_err(|_| DatabaseError::Disconnected)?
    }
}

async fn inspect_visibility(connection: &mut mysql::Connection) -> Visibility {
    let Ok(grants) = connection.execute("SHOW GRANTS", 256).await else {
        return Visibility::Unknown;
    };
    if grants.iter().any(|r| r.truncated) || grants.iter().all(|r| r.rows.is_empty()) {
        return Visibility::Unknown;
    }
    if has_process_grant(&grants) {
        return Visibility::All;
    }
    // Role grants have no ON clause. Inspect only active roles, never activate
    // additional privileges on behalf of the user. Older MySQL needs no role query.
    let has_roles = grants
        .iter()
        .flat_map(|r| &r.rows)
        .filter_map(|r| r.first().and_then(Option::as_deref))
        .any(|g| {
            g.strip_prefix("GRANT ")
                .is_some_and(|g| g.starts_with(['`', '\'', '"']))
        });
    if !has_roles {
        return Visibility::Own;
    }
    let Ok(roles) = connection
        .execute(
            "SELECT ROLE_NAME, ROLE_HOST FROM information_schema.ENABLED_ROLES",
            64,
        )
        .await
    else {
        return Visibility::Unknown;
    };
    let Some(using) = role_clause(&roles) else {
        return Visibility::Unknown;
    };
    if using.is_empty() {
        return Visibility::Own;
    }
    match connection
        .execute(&format!("SHOW GRANTS FOR CURRENT_USER USING {using}"), 256)
        .await
    {
        Ok(grants) if grants.iter().all(|r| !r.truncated) => {
            if has_process_grant(&grants) {
                Visibility::All
            } else {
                Visibility::Own
            }
        }
        _ => Visibility::Unknown,
    }
}

fn has_process_grant(grants: &[ResultSet]) -> bool {
    grants
        .iter()
        .flat_map(|r| &r.rows)
        .filter_map(|r| r.first().and_then(Option::as_deref))
        .any(|grant| {
            let Some(privileges) = grant
                .strip_prefix("GRANT ")
                .and_then(|g| g.split_once(" ON *.* TO "))
                .map(|(p, _)| p)
            else {
                return false;
            };
            privileges
                .split(',')
                .any(|p| matches!(p.trim(), "PROCESS" | "ALL PRIVILEGES"))
        })
}
fn role_clause(results: &[ResultSet]) -> Option<String> {
    if results.iter().any(|r| r.truncated) {
        return None;
    }
    let mut roles = Vec::new();
    for row in results.iter().flat_map(|r| &r.rows) {
        let name = row.first()?.as_ref()?;
        let host = row.get(1)?.as_ref()?;
        if name.len() > 256 || host.len() > 256 || name.contains('\0') || host.contains('\0') {
            return None;
        }
        roles.push(format!(
            "{}@{}",
            quote_identifier(SqlEngine::Mysql, name),
            quote_identifier(SqlEngine::Mysql, host)
        ));
    }
    Some(roles.join(", "))
}
fn decode(mut results: Vec<ResultSet>) -> Result<Snapshot> {
    if results.len() != 1 {
        return Err(DatabaseError::Protocol);
    }
    let result = results.remove(0);
    let column = |name: &str| {
        result
            .columns
            .iter()
            .position(|c| c.eq_ignore_ascii_case(name))
            .ok_or(DatabaseError::Protocol)
    };
    let [id, user, host, database, command, seconds, state, sql] = [
        column("Id")?,
        column("User")?,
        column("Host")?,
        column("db")?,
        column("Command")?,
        column("Time")?,
        column("State")?,
        column("Info")?,
    ];
    let processes = result
        .rows
        .iter()
        .map(|row| {
            let field = |i: usize| row.get(i).cloned().ok_or(DatabaseError::Protocol);
            let number = |i: usize| -> Result<Option<u64>> {
                field(i)?
                    .map(|v| v.parse().map_err(|_| DatabaseError::Protocol))
                    .transpose()
            };
            Ok(Process {
                id: number(id)?.ok_or(DatabaseError::Protocol)?,
                user: field(user)?.unwrap_or_default(),
                host: field(host)?.unwrap_or_default(),
                database: field(database)?,
                command: field(command)?.unwrap_or_default(),
                seconds: number(seconds)?,
                state: field(state)?,
                sql: field(sql)?,
            })
        })
        .collect::<Result<Vec<_>>>()?;
    Ok(Snapshot {
        processes,
        visibility: Visibility::Unknown,
        truncated: result.truncated,
        elapsed: Duration::ZERO,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    fn grants(values: &[&str]) -> Vec<ResultSet> {
        vec![ResultSet {
            rows: values.iter().map(|v| vec![Some((*v).into())]).collect(),
            ..Default::default()
        }]
    }
    #[test]
    fn permission_detection_requires_a_global_privilege_not_a_name() {
        assert!(has_process_grant(&grants(&[
            "GRANT SELECT, PROCESS ON *.* TO `u`@`%`"
        ])));
        assert!(has_process_grant(&grants(&[
            "GRANT ALL PRIVILEGES ON *.* TO `u`@`%`"
        ])));
        assert!(!has_process_grant(&grants(&[
            "GRANT ALL PRIVILEGES ON `db`.* TO `PROCESS`@`%`",
            "GRANT `PROCESS`@`%` TO `u`@`%`"
        ])));
    }
    #[test]
    fn roles_are_quoted_as_identifiers_and_truncation_is_not_authoritative() {
        let mut roles = vec![ResultSet {
            rows: vec![vec![Some("r`1".into()), Some("%".into())]],
            ..Default::default()
        }];
        assert_eq!(role_clause(&roles).unwrap(), "`r``1`@`%`");
        roles[0].truncated = true;
        assert!(role_clause(&roles).is_none());
    }
    #[test]
    fn processlist_preserves_nulls_full_sql_and_unsigned_ids() {
        let sql = "SELECT ".to_owned() + &"long_query".repeat(40);
        let snapshot = decode(vec![ResultSet {
            columns: [
                "Id", "User", "Host", "db", "Command", "Time", "State", "Info",
            ]
            .map(str::to_owned)
            .into(),
            rows: vec![vec![
                Some(u64::MAX.to_string()),
                Some("app".into()),
                Some("localhost".into()),
                None,
                Some("Query".into()),
                Some("100".into()),
                None,
                Some(sql.clone()),
            ]],
            ..Default::default()
        }])
        .unwrap();
        assert_eq!(snapshot.processes[0].id, u64::MAX);
        assert_eq!(snapshot.processes[0].sql, Some(sql));
        assert_eq!(snapshot.processes[0].database, None);
        assert!(decode(vec![ResultSet::default()]).is_err());
    }
}
