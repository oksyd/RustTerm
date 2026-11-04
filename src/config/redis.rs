use crate::i18n::messages as i18n_key;

use super::credentials::{CredentialRef, validate_reference};
use super::{ConfigError, Result, invalid, known_hosts, validate_text};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

/// A connection to one Redis server, directly or through a saved SSH profile.
/// Passwords live only in the
/// credential store; TLS always validates the server certificate and hostname.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct RedisConfig {
    pub host: String,
    pub port: u16,
    pub username: Option<String>,
    pub credential: Option<CredentialRef>,
    pub database: u32,
    pub tls: bool,
    pub timeout_secs: u64,
    /// When set, Redis host/port are resolved and reached from this SSH server.
    pub ssh_profile: Option<Uuid>,
    pub topology: RedisTopology,
    pub nodes: Vec<RedisEndpoint>,
    pub sentinel_service: Option<String>,
    pub sentinel_username: Option<String>,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RedisTopology {
    #[default]
    Standalone,
    Cluster,
    Sentinel,
}

#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RedisEndpoint {
    pub host: String,
    pub port: u16,
}

impl std::str::FromStr for RedisEndpoint {
    type Err = ConfigError;
    fn from_str(value: &str) -> Result<Self> {
        let (host, port) = value
            .trim()
            .rsplit_once(':')
            .ok_or_else(|| invalid(i18n_key::REDIS_USE_HOST_PORT_FOR_EACH_REDIS_NODE))?;
        let host = known_hosts::normalize_host(host)?;
        let port = port
            .parse::<u16>()
            .ok()
            .filter(|port| *port > 0)
            .ok_or_else(|| invalid(i18n_key::REDIS_USE_HOST_PORT_FOR_EACH_REDIS_NODE))?;
        Ok(Self { host, port })
    }
}
impl std::fmt::Display for RedisEndpoint {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        if self.host.contains(':') {
            write!(f, "[{}]:{}", self.host, self.port)
        } else {
            write!(f, "{}:{}", self.host, self.port)
        }
    }
}

impl Default for RedisConfig {
    fn default() -> Self {
        Self {
            host: "127.0.0.1".into(),
            port: 6379,
            username: None,
            credential: None,
            database: 0,
            tls: false,
            timeout_secs: 10,
            ssh_profile: None,
            topology: RedisTopology::Standalone,
            nodes: Vec::new(),
            sentinel_service: None,
            sentinel_username: None,
        }
    }
}

impl RedisConfig {
    pub fn validate(&self) -> Result<()> {
        if self.nodes.len() > 32 || (self.topology == RedisTopology::Cluster && self.database != 0)
        {
            return Err(invalid(
                i18n_key::REDIS_REDIS_CLUSTER_USES_DATABASE_0_AT_MOST_32_SEED_NODES,
            )
            .at_field(if self.nodes.len() > 32 {
                i18n_key::REDIS_ADDITIONAL_REDIS_NODES
            } else {
                i18n_key::REDIS_DATABASE_INDEX
            }));
        }
        for node in &self.nodes {
            known_hosts::normalize_host(&node.host)
                .map_err(|error| error.at_field(i18n_key::REDIS_ADDITIONAL_REDIS_NODES))?;
            if node.port == 0 {
                return Err(invalid(i18n_key::REDIS_USE_HOST_PORT_FOR_EACH_REDIS_NODE)
                    .at_field(i18n_key::REDIS_ADDITIONAL_REDIS_NODES));
            }
        }
        if self.topology == RedisTopology::Sentinel {
            validate_text(
                self.sentinel_service.as_deref().unwrap_or_default(),
                256,
                i18n_key::REDIS_SENTINEL_SERVICE_NAME,
            )
            .map_err(|error| error.at_field(i18n_key::REDIS_SENTINEL_SERVICE_NAME))?;
        }
        if let Some(username) = &self.sentinel_username {
            validate_text(username, 256, i18n_key::REDIS_SENTINEL_USERNAME)
                .map_err(|error| error.at_field(i18n_key::REDIS_SENTINEL_USERNAME))?;
        }
        known_hosts::normalize_host(&self.host)
            .map_err(|error| error.at_field(i18n_key::WORKSPACE_HOST))?;
        if self.port == 0 {
            return Err(
                invalid(i18n_key::REDIS_REDIS_PORT_MUST_BE_BETWEEN_1_AND_65535)
                    .at_field(i18n_key::WORKSPACE_PORT),
            );
        }
        if self.database > i32::MAX as u32 {
            return Err(invalid(
                i18n_key::REDIS_REDIS_DATABASE_INDEX_MUST_BE_BETWEEN_0_AND_2147483647,
            )
            .at_field(i18n_key::REDIS_DATABASE_INDEX));
        }
        if !(1..=600).contains(&self.timeout_secs) {
            return Err(invalid(
                i18n_key::REDIS_REDIS_CONNECTION_TIMEOUT_MUST_BE_BETWEEN_1_AND_600_SECONDS,
            )
            .at_field(i18n_key::WORKSPACE_CONNECTION_TIMEOUT_SECONDS));
        }
        if let Some(username) = &self.username {
            validate_text(username, 256, i18n_key::REDIS_REDIS_ACL_USERNAME)
                .map_err(|error| error.at_field(i18n_key::REDIS_ACL_USERNAME_OPTIONAL))?;
        }
        if self.ssh_profile.is_some_and(|id| id.is_nil()) {
            return Err(invalid(
                i18n_key::REDIS_THE_SSH_TUNNEL_PROFILE_ID_CANNOT_BE_NIL,
            ));
        }
        validate_reference(&self.credential)
    }
}

#[cfg(test)]
mod tests {
    use super::RedisConfig;
    use crate::config::CredentialRef;
    use uuid::Uuid;

    #[test]
    fn redis_validation_rejects_invalid_targets_and_credentials() {
        let defaults = RedisConfig::default();
        for invalid in [
            RedisConfig {
                host: "redis://user:password@host".into(),
                ..defaults.clone()
            },
            RedisConfig {
                host: "host/path".into(),
                ..defaults.clone()
            },
            RedisConfig {
                port: 0,
                ..defaults.clone()
            },
            RedisConfig {
                database: u32::MAX,
                ..defaults.clone()
            },
            RedisConfig {
                timeout_secs: 0,
                ..defaults.clone()
            },
            RedisConfig {
                timeout_secs: 601,
                ..defaults.clone()
            },
            RedisConfig {
                username: Some(String::new()),
                ..defaults.clone()
            },
            RedisConfig {
                username: Some("bad\nuser".into()),
                ..defaults.clone()
            },
            RedisConfig {
                credential: Some(CredentialRef(Uuid::nil())),
                ..defaults.clone()
            },
        ] {
            assert!(invalid.validate().is_err());
        }
        RedisConfig {
            host: "[::1]".into(),
            database: 31,
            tls: true,
            ..defaults
        }
        .validate()
        .unwrap();
    }
}
