use super::{
    AuthMethod, CredentialRef, DatabaseConfig, LocalConfig, RdpConfig, RedisConfig, SqlConfig,
    SqlEngine, SqliteConfig, SshConfig, TelnetConfig,
};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Group {
    pub id: Uuid,
    pub name: String,
}

impl Group {
    pub fn new(name: impl Into<String>) -> Self {
        Self {
            id: Uuid::new_v4(),
            name: name.into(),
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Profile {
    pub id: Uuid,
    pub name: String,
    pub group: Option<Uuid>,
    pub connection: Connection,
}

impl Profile {
    pub fn local(name: impl Into<String>, config: LocalConfig) -> Self {
        Self {
            id: Uuid::new_v4(),
            name: name.into(),
            group: None,
            connection: Connection::Local(config),
        }
    }
    pub fn ssh(name: impl Into<String>, config: SshConfig) -> Self {
        Self {
            id: Uuid::new_v4(),
            name: name.into(),
            group: None,
            connection: Connection::Ssh(config),
        }
    }
    pub fn telnet(name: impl Into<String>, config: TelnetConfig) -> Self {
        Self {
            id: Uuid::new_v4(),
            name: name.into(),
            group: None,
            connection: Connection::Telnet(config),
        }
    }
    pub fn redis(name: impl Into<String>, config: RedisConfig) -> Self {
        Self {
            id: Uuid::new_v4(),
            name: name.into(),
            group: None,
            connection: Connection::Redis(config),
        }
    }
    pub fn rdp(name: impl Into<String>, config: RdpConfig) -> Self {
        Self {
            id: Uuid::new_v4(),
            name: name.into(),
            group: None,
            connection: Connection::Rdp(config),
        }
    }
    pub fn mysql(name: impl Into<String>, config: SqlConfig) -> Self {
        Self {
            id: Uuid::new_v4(),
            name: name.into(),
            group: None,
            connection: Connection::Mysql(config),
        }
    }
    pub fn postgres(name: impl Into<String>, config: SqlConfig) -> Self {
        Self {
            id: Uuid::new_v4(),
            name: name.into(),
            group: None,
            connection: Connection::Postgres(config),
        }
    }
    pub fn sqlite(name: impl Into<String>, config: SqliteConfig) -> Self {
        Self {
            id: Uuid::new_v4(),
            name: name.into(),
            group: None,
            connection: Connection::Sqlite(config),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(
    tag = "type",
    content = "options",
    rename_all = "snake_case",
    deny_unknown_fields
)]
pub enum Connection {
    Local(LocalConfig),
    Ssh(SshConfig),
    Telnet(TelnetConfig),
    Redis(RedisConfig),
    Mysql(SqlConfig),
    Postgres(SqlConfig),
    Sqlite(SqliteConfig),
    Rdp(RdpConfig),
}

impl Connection {
    pub fn credential_ref(&self) -> Option<CredentialRef> {
        match self {
            Self::Ssh(config) => match &config.auth {
                AuthMethod::Password { credential } => *credential,
                AuthMethod::PrivateKey { passphrase, .. } => *passphrase,
                AuthMethod::Agent { .. } | AuthMethod::KeyboardInteractive => None,
            },
            Self::Redis(config) => config.credential,
            Self::Mysql(config) | Self::Postgres(config) => config.credential,
            Self::Rdp(config) => config.credential,
            Self::Local(_) | Self::Telnet(_) | Self::Sqlite(_) => None,
        }
    }

    pub fn set_credential(&mut self, reference: Option<CredentialRef>) -> bool {
        let slot = match self {
            Self::Ssh(config) => match &mut config.auth {
                AuthMethod::Password { credential } => credential,
                AuthMethod::PrivateKey { passphrase, .. } => passphrase,
                AuthMethod::Agent { .. } | AuthMethod::KeyboardInteractive => return false,
            },
            Self::Redis(config) => &mut config.credential,
            Self::Mysql(config) | Self::Postgres(config) => &mut config.credential,
            Self::Rdp(config) => &mut config.credential,
            Self::Local(_) | Self::Telnet(_) | Self::Sqlite(_) => return false,
        };
        *slot = reference;
        true
    }

    pub fn sql(&self) -> Option<(SqlEngine, &SqlConfig)> {
        match self {
            Self::Mysql(config) => Some((SqlEngine::Mysql, config)),
            Self::Postgres(config) => Some((SqlEngine::Postgres, config)),
            _ => None,
        }
    }

    pub fn database(&self) -> Option<DatabaseConfig> {
        match self {
            Self::Mysql(config) => Some(DatabaseConfig::Mysql(config.clone())),
            Self::Postgres(config) => Some(DatabaseConfig::Postgres(config.clone())),
            Self::Sqlite(config) => Some(DatabaseConfig::Sqlite(config.clone())),
            _ => None,
        }
    }

    pub fn ssh_profile(&self) -> Option<Uuid> {
        match self {
            Self::Redis(config) => config.ssh_profile,
            Self::Mysql(config) | Self::Postgres(config) => config.ssh_profile,
            _ => None,
        }
    }
}
