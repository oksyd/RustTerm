use crate::i18n::messages as i18n_key;

use super::credentials::{CredentialRef, validate_reference};
use super::{Result, invalid, known_hosts, validate_text};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use uuid::Uuid;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SqlEngine {
    Mysql,
    Postgres,
    Sqlite,
}

impl SqlEngine {
    pub const fn label(self) -> &'static str {
        match self {
            Self::Mysql => "MySQL",
            Self::Postgres => "PostgreSQL",
            Self::Sqlite => "SQLite",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SqlConfig {
    pub host: String,
    pub port: u16,
    pub username: String,
    #[serde(default)]
    pub database: String,
    #[serde(default)]
    pub tls: bool,
    #[serde(default = "connection_timeout")]
    pub timeout_secs: u64,
    #[serde(default = "query_timeout")]
    pub query_timeout_secs: u64,
    pub credential: Option<CredentialRef>,
    pub ssh_profile: Option<Uuid>,
}

impl SqlConfig {
    pub fn mysql() -> Self {
        Self::server_defaults(3306, "root", "")
    }

    pub fn postgres() -> Self {
        Self::server_defaults(5432, "postgres", "postgres")
    }

    fn server_defaults(port: u16, username: &str, database: &str) -> Self {
        Self {
            host: "127.0.0.1".into(),
            port,
            username: username.into(),
            database: database.into(),
            tls: false,
            timeout_secs: connection_timeout(),
            query_timeout_secs: query_timeout(),
            credential: None,
            ssh_profile: None,
        }
    }

    pub fn validate(&self) -> Result<()> {
        known_hosts::normalize_host(&self.host)
            .map_err(|error| error.at_field(i18n_key::WORKSPACE_HOST))?;
        validate_text(&self.username, 256, i18n_key::DATABASE_DATABASE_USERNAME)
            .map_err(|error| error.at_field(i18n_key::DATABASE_DATABASE_USERNAME))?;
        if !self.database.is_empty() {
            validate_text(&self.database, 256, i18n_key::DATABASE_DATABASE_NAME)
                .map_err(|error| error.at_field(i18n_key::DATABASE_DATABASE_NAME))?;
        }
        if self.port == 0 {
            return Err(
                invalid(i18n_key::DATABASE_DATABASE_PORT_MUST_BE_BETWEEN_1_AND_65535)
                    .at_field(i18n_key::WORKSPACE_PORT),
            );
        }
        if !(1..=600).contains(&self.timeout_secs) || !(1..=3600).contains(&self.query_timeout_secs)
        {
            return Err(invalid(
                i18n_key::DATABASE_USE_A_CONNECTION_TIMEOUT_OF_1_600_SECONDS_AND_A,
            )
            .at_field(if !(1..=600).contains(&self.timeout_secs) {
                i18n_key::WORKSPACE_CONNECTION_TIMEOUT_SECONDS
            } else {
                i18n_key::DATABASE_QUERY_TIMEOUT_SECONDS
            }));
        }
        if self.ssh_profile.is_some_and(|id| id.is_nil()) {
            return Err(invalid(
                i18n_key::REDIS_THE_SSH_TUNNEL_PROFILE_ID_CANNOT_BE_NIL,
            ));
        }
        validate_reference(&self.credential)
    }

    pub fn same_auth_identity(&self, other: &Self) -> bool {
        self.host == other.host
            && self.port == other.port
            && self.username == other.username
            && self.database == other.database
            && self.tls == other.tls
            && self.ssh_profile == other.ssh_profile
    }
}

/// A local database file. Opening never creates the file or interprets a URI.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SqliteConfig {
    pub path: PathBuf,
    #[serde(default = "default_read_only")]
    pub read_only: bool,
    #[serde(default = "query_timeout")]
    pub query_timeout_secs: u64,
}

fn default_read_only() -> bool {
    true
}

impl Default for SqliteConfig {
    fn default() -> Self {
        Self {
            path: PathBuf::new(),
            read_only: true,
            query_timeout_secs: query_timeout(),
        }
    }
}

impl SqliteConfig {
    pub fn validate(&self) -> Result<()> {
        let valid_path = self.path.is_absolute()
            && self.path.file_name().is_some()
            && self
                .path
                .to_str()
                .is_some_and(|path| path.len() <= 16_384 && !path.chars().any(char::is_control));
        if !valid_path {
            return Err(
                invalid(i18n_key::DATABASE_CHOOSE_AN_ABSOLUTE_LOCAL_SQLITE_FILE_PATH)
                    .at_field(i18n_key::DATABASE_DATABASE_FILE),
            );
        }
        if !(1..=3600).contains(&self.query_timeout_secs) {
            return Err(
                invalid(i18n_key::DATABASE_USE_A_QUERY_TIMEOUT_OF_1_3600_SECONDS)
                    .at_field(i18n_key::DATABASE_QUERY_TIMEOUT_SECONDS),
            );
        }
        Ok(())
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum DatabaseConfig {
    Mysql(SqlConfig),
    Postgres(SqlConfig),
    Sqlite(SqliteConfig),
}

impl DatabaseConfig {
    pub fn engine(&self) -> SqlEngine {
        match self {
            Self::Mysql(_) => SqlEngine::Mysql,
            Self::Postgres(_) => SqlEngine::Postgres,
            Self::Sqlite(_) => SqlEngine::Sqlite,
        }
    }

    pub fn query_timeout_secs(&self) -> u64 {
        match self {
            Self::Mysql(config) | Self::Postgres(config) => config.query_timeout_secs,
            Self::Sqlite(config) => config.query_timeout_secs,
        }
    }

    pub fn server(&self) -> Option<&SqlConfig> {
        match self {
            Self::Mysql(config) | Self::Postgres(config) => Some(config),
            Self::Sqlite(_) => None,
        }
    }

    pub fn validate(&self) -> Result<()> {
        match self {
            Self::Mysql(config) | Self::Postgres(config) => config.validate(),
            Self::Sqlite(config) => config.validate(),
        }
    }
}

fn connection_timeout() -> u64 {
    10
}

fn query_timeout() -> u64 {
    60
}

#[cfg(test)]
mod tests {
    use super::{SqlConfig, SqliteConfig};
    use crate::i18n::messages as i18n_key;
    use uuid::Uuid;

    #[test]
    fn sql_validation_rejects_unsafe_targets_and_plaintext_options() {
        let base = SqlConfig::postgres();
        for field in 0..7 {
            let mut config = base.clone();
            match field {
                0 => config.host = "postgres://user:secret@localhost".into(),
                1 => config.port = 0,
                2 => config.username = String::new(),
                3 => config.database = "database\nname".into(),
                4 => config.timeout_secs = 0,
                5 => config.query_timeout_secs = 3601,
                _ => config.ssh_profile = Some(Uuid::nil()),
            }
            let expected = [
                Some(i18n_key::WORKSPACE_HOST),
                Some(i18n_key::WORKSPACE_PORT),
                Some(i18n_key::DATABASE_DATABASE_USERNAME),
                Some(i18n_key::DATABASE_DATABASE_NAME),
                Some(i18n_key::WORKSPACE_CONNECTION_TIMEOUT_SECONDS),
                Some(i18n_key::DATABASE_QUERY_TIMEOUT_SECONDS),
                None,
            ][field];
            assert_eq!(config.validate().unwrap_err().field(), expected);
        }
        let encoded = toml::to_string(&base).unwrap();
        for option in ["password = 'must-not-be-stored'", "tls_insecure = true"] {
            assert!(toml::from_str::<SqlConfig>(&format!("{encoded}\n{option}")).is_err());
        }
    }

    #[test]
    fn sqlite_defaults_and_validation_are_file_specific() {
        assert!(SqliteConfig::default().read_only);
        for path in [
            "relative.db",
            ":memory:",
            "file:/tmp/database.db",
            "/",
            "/tmp/bad\0.db",
        ] {
            let config = SqliteConfig {
                path: path.into(),
                ..Default::default()
            };
            assert_eq!(
                config.validate().unwrap_err().field(),
                Some(i18n_key::DATABASE_DATABASE_FILE),
                "{path:?}"
            );
        }
        let config = SqliteConfig {
            path: std::env::temp_dir().join("future.sqlite"),
            ..Default::default()
        };
        assert!(config.validate().is_ok());
        let profile = crate::config::Profile::sqlite("Local SQLite", config.clone());
        let encoded = toml::to_string(&profile).unwrap();
        let decoded: crate::config::Profile = toml::from_str(&encoded).unwrap();
        assert_eq!(decoded.id, profile.id);
        assert_eq!(
            decoded.connection,
            crate::config::Connection::Sqlite(config)
        );
        assert!(decoded.connection.credential_ref().is_none());
        assert!(decoded.connection.ssh_profile().is_none());
        assert!(decoded.connection.sql().is_none());
        assert!(!encoded.contains("credential"));
        assert!(!encoded.contains("ssh_profile"));
    }
}
