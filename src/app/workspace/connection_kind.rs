use crate::app::ui::Icon;
use rustterm::config::Connection;
use rustterm::i18n::{messages as key, tr};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum ConnectionKind {
    Local,
    Ssh,
    Telnet,
    Rdp,
    Redis,
    Elasticsearch,
    Mysql,
    ClickHouse,
    Postgres,
    Sqlite,
}
impl ConnectionKind {
    pub const ALL: [Self; 10] = [
        Self::Ssh,
        Self::Rdp,
        Self::Mysql,
        Self::Postgres,
        Self::Redis,
        Self::Elasticsearch,
        Self::ClickHouse,
        Self::Sqlite,
        Self::Telnet,
        Self::Local,
    ];
    pub fn of(connection: &Connection) -> Self {
        match connection {
            Connection::Local(_) => Self::Local,
            Connection::Ssh(_) => Self::Ssh,
            Connection::Telnet(_) => Self::Telnet,
            Connection::Rdp(_) => Self::Rdp,
            Connection::Redis(_) => Self::Redis,
            Connection::Elasticsearch(_) => Self::Elasticsearch,
            Connection::Mysql(_) => Self::Mysql,
            Connection::ClickHouse(_) => Self::ClickHouse,
            Connection::Postgres(_) => Self::Postgres,
            Connection::Sqlite(_) => Self::Sqlite,
        }
    }
    pub fn label(self) -> &'static str {
        match self {
            Self::Local => tr(key::WORKSPACE_LOCAL),
            Self::Ssh => "SSH",
            Self::Telnet => "Telnet",
            Self::Rdp => "RDP",
            Self::Redis => "Redis",
            Self::Elasticsearch => "Elasticsearch",
            Self::Mysql => "MySQL",
            Self::ClickHouse => "ClickHouse",
            Self::Postgres => "PostgreSQL",
            Self::Sqlite => "SQLite",
        }
    }
    pub fn profile_kind(self) -> crate::app::forms::ProfileKind {
        use crate::app::forms::ProfileKind;
        match self {
            Self::Local => ProfileKind::Local,
            Self::Ssh => ProfileKind::Ssh,
            Self::Telnet => ProfileKind::Telnet,
            Self::Rdp => ProfileKind::Rdp,
            Self::Redis => ProfileKind::Redis,
            Self::Elasticsearch => ProfileKind::Elasticsearch,
            Self::Mysql => ProfileKind::Mysql,
            Self::ClickHouse => ProfileKind::ClickHouse,
            Self::Postgres => ProfileKind::Postgres,
            Self::Sqlite => ProfileKind::Sqlite,
        }
    }
    pub fn icon(self) -> Icon {
        match self {
            Self::Local => Icon::Local,
            Self::Ssh => Icon::Ssh,
            Self::Telnet => Icon::Telnet,
            Self::Rdp => Icon::Desktop,
            Self::Redis => Icon::Redis,
            Self::Elasticsearch => Icon::Elasticsearch,
            Self::Mysql => Icon::Mysql,
            Self::ClickHouse => Icon::ClickHouse,
            Self::Postgres => Icon::Postgres,
            Self::Sqlite => Icon::Sqlite,
        }
    }
}

/// Display destinations only; credentials and URL query strings never belong in the library.
pub(super) fn target(connection: &Connection) -> String {
    fn host(host: &str, port: u16) -> String {
        if host.contains(':') && !host.starts_with('[') {
            format!("[{host}]:{port}")
        } else {
            format!("{host}:{port}")
        }
    }
    match connection {
        Connection::Local(c) => c.shell.clone(),
        Connection::Ssh(c) => host(&c.host, c.port),
        Connection::Telnet(c) => host(&c.host, c.port),
        Connection::Rdp(c) => host(&c.host, c.port),
        Connection::Redis(c) => host(&c.host, c.port),
        Connection::Mysql(c) | Connection::ClickHouse(c) | Connection::Postgres(c) => {
            host(&c.host, c.port)
        }
        Connection::Sqlite(c) => c.path.display().to_string(),
        Connection::Elasticsearch(c) => c
            .endpoint
            .split("://")
            .nth(1)
            .unwrap_or(&c.endpoint)
            .split(['/', '?', '#'])
            .next()
            .unwrap_or_default()
            .rsplit('@')
            .next()
            .unwrap_or_default()
            .to_owned(),
    }
}
