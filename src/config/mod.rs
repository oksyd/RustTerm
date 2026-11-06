//! Persistent, validated application settings. Authentication secrets never enter this model.

use crate::i18n::{Message, messages as i18n_key};

mod credential_process;
pub mod credentials;
mod elasticsearch;
mod groups;
pub mod known_hosts;
mod local;
mod profiles;
mod rdp;
mod redis;
mod settings;
mod sql;
pub mod ssh;
mod storage;
mod telnet;
mod workspace;

pub use credential_process::{ProcessCredentialStore, run_credential_worker};
pub use credentials::{CredentialRef, CredentialStore, Secret, SystemCredentialStore};
pub use elasticsearch::{ElasticsearchAuth, ElasticsearchConfig};
pub use groups::GroupNode;
pub use known_hosts::{HostKey, HostKeyStatus, KnownHosts};
pub use local::{LocalConfig, LocalProfile};
pub use profiles::{Connection, Group, Profile};
pub use rdp::{RdpConfig, RdpQuality, RdpSharedFolder};
pub use redis::{RedisConfig, RedisEndpoint, RedisTopology};
pub use settings::{AppLockConfig, AppTheme, Settings, Theme};
pub use sql::{DatabaseConfig, SqlConfig, SqlEngine, SqliteConfig};
pub use ssh::{AuthMethod, ForwardKind, PortForward, SshConfig, SshJumpHost, SshProfile};
pub use storage::{ConfigLock, config_dir};
pub use telnet::TelnetConfig;
pub use uuid::Uuid;
pub use workspace::{
    PanePosition, QuickCommand, SavedLayout, SavedTab, SavedWorkspace, SshSessionPosition,
    WorkspaceSnapshot,
};

use serde::{Deserialize, Serialize};
use std::{
    collections::HashSet,
    path::{Path, PathBuf},
};

pub const SCHEMA_VERSION: u32 = 1;

#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    #[error("{}", crate::trf!(i18n_key::CONNECTIONS_CONFIGURATION_READ_WRITE_FAILED_ERROR, error = .0.to_string()))]
    Io(#[from] std::io::Error),
    // TOML errors retain the source document and can include it in both Display and Debug.
    // Keep only a byte location: malformed files may contain accidentally pasted passwords.
    #[error("{}", crate::trf!(i18n_key::CONNECTIONS_INVALID_CONFIGURATION_FORMAT_AT_BYTE_OFFSET_OFFSET, offset = .offset))]
    Decode { offset: usize },
    #[error("{}", crate::trf!(i18n_key::CONNECTIONS_CONFIGURATION_ENCODING_FAILED_ERROR, error = .0.to_string()))]
    Encode(#[from] toml::ser::Error),
    #[error("{}", crate::trf!(i18n_key::CONNECTIONS_UNSUPPORTED_CONFIGURATION_VERSION_FOUND_LATEST_SUPPORTED_SUPPORTED_THE, found = .found, supported = .supported))]
    FutureVersion { found: u32, supported: u32 },
    #[error("{}", crate::trf!(i18n_key::CONNECTIONS_CONFIGURATION_VALIDATION_FAILED_ERROR, error = .0))]
    Validation(Message),
    #[error("{}", crate::trf!(i18n_key::CONNECTIONS_CONFIGURATION_VALIDATION_FAILED_ERROR, error = .message))]
    FieldValidation {
        field: &'static str,
        message: Message,
    },
    #[error(
        "{}",
        crate::i18n::tr(
            i18n_key::CONNECTIONS_CANNOT_DETERMINE_THE_CONFIGURATION_DIRECTORY_SET_HOME_OR_XDG_CONFIG
        )
    )]
    MissingConfigDirectory,
    #[error("{}", crate::trf!(i18n_key::CONNECTIONS_UNSAFE_FILE_OR_DIRECTORY_PERMISSIONS_PATH, path = .0.display().to_string()))]
    Permissions(PathBuf),
    #[error(
        "{}",
        crate::i18n::tr(
            i18n_key::CONNECTIONS_THE_HOST_ALREADY_HAS_A_DIFFERENT_TRUST_RECORD_REPLACE_IT
        )
    )]
    HostKeyChanged,
    #[error(
        "{}",
        crate::i18n::tr(
            i18n_key::CONNECTIONS_ANOTHER_RUSTTERM_INSTANCE_IS_USING_THIS_CONFIGURATION_DIRECTORY
        )
    )]
    AlreadyRunning,
    #[error("{}", crate::trf!(i18n_key::CONNECTIONS_THE_CONFIGURATION_FILE_WAS_REPLACED_BUT_DIRECTORY_SYNCHRONIZATION_FAILED, error = .0.to_string()))]
    CommitUncertain(std::io::Error),
}

impl ConfigError {
    /// The stable field message ID is independent of the translated error.
    pub fn field(&self) -> Option<&'static str> {
        match self {
            Self::FieldValidation { field, .. } => Some(field),
            _ => None,
        }
    }

    fn at_field(self, field: &'static str) -> Self {
        match self {
            Self::Validation(message) | Self::FieldValidation { message, .. } => {
                Self::FieldValidation { field, message }
            }
            error => error,
        }
    }

    /// The destination was replaced, but crash durability could not be confirmed.
    /// Callers must retain the new state and its credential references in this case.
    pub fn was_committed(&self) -> bool {
        matches!(self, Self::CommitUncertain(_))
    }
}

impl From<toml::de::Error> for ConfigError {
    fn from(error: toml::de::Error) -> Self {
        Self::Decode {
            offset: error.span().map_or(0, |span| span.start),
        }
    }
}

pub type Result<T> = std::result::Result<T, ConfigError>;
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct AppConfig {
    pub schema_version: u32,
    pub profiles: Vec<Profile>,
    pub groups: Vec<Group>,
    pub settings: Settings,
    pub app_lock: AppLockConfig,
    pub workspace: WorkspaceSnapshot,
    pub quick_commands: Vec<QuickCommand>,
}

impl Default for AppConfig {
    fn default() -> Self {
        Self {
            schema_version: SCHEMA_VERSION,
            profiles: vec![Profile::local(
                crate::i18n::tr(i18n_key::COMMON_LOCAL_TERMINAL),
                LocalConfig::default(),
            )],
            groups: Vec::new(),
            settings: Settings::default(),
            app_lock: AppLockConfig::default(),
            workspace: WorkspaceSnapshot::default(),
            quick_commands: Vec::new(),
        }
    }
}

impl AppConfig {
    /// Missing files produce defaults; malformed/future files are never overwritten.
    /// Legacy version 0 used the same field layout, without an explicit schema version.
    pub fn load(path: impl AsRef<Path>) -> Result<Self> {
        let path = path.as_ref();
        let Some(source) = storage::read_optional(path)? else {
            return Ok(Self::default());
        };
        let value: toml::Value = toml::from_str(&source)?;
        let version = match value.get("schema_version") {
            None => 0,
            Some(value) => value
                .as_integer()
                .and_then(|v| u32::try_from(v).ok())
                .ok_or_else(|| {
                    invalid(i18n_key::CONNECTIONS_SCHEMA_VERSION_MUST_BE_A_NONNEGATIVE_INTEGER)
                })?,
        };
        if version > SCHEMA_VERSION {
            return Err(ConfigError::FutureVersion {
                found: version,
                supported: SCHEMA_VERSION,
            });
        }
        let mut config: Self = toml::from_str(&source)?;
        config.schema_version = SCHEMA_VERSION;
        config.validate()?;
        if version == 0 {
            let mut backup = path.as_os_str().to_owned();
            backup.push(".v0.bak");
            // Only validated, secret-free configuration fields can reach a migration backup.
            storage::write_atomic(Path::new(&backup), source.as_bytes())?;
            config.save(path)?;
        }
        Ok(config)
    }

    pub fn save(&self, path: impl AsRef<Path>) -> Result<()> {
        self.validate()?;
        storage::write_atomic(path.as_ref(), toml::to_string_pretty(self)?.as_bytes())
    }

    /// A credential can be shared by imported profiles. Delete it from the OS vault only
    /// after committing a configuration in which no connection references it anymore.
    pub fn uses_credential(&self, reference: CredentialRef) -> bool {
        self.profiles
            .iter()
            .any(|profile| profile.connection.credential_ref() == Some(reference))
    }

    /// Referenced SSH profiles must remain available until their database routes are changed.
    pub fn uses_ssh_profile(&self, id: Uuid) -> bool {
        self.profiles
            .iter()
            .any(|profile| profile.connection.ssh_profile() == Some(id))
    }

    pub fn validate(&self) -> Result<()> {
        if self.schema_version != SCHEMA_VERSION {
            return Err(ConfigError::FutureVersion {
                found: self.schema_version,
                supported: SCHEMA_VERSION,
            });
        }
        if self.profiles.len() > 10_000 || self.groups.len() > 1_000 {
            return Err(invalid(
                i18n_key::CONNECTIONS_THE_PROFILE_OR_GROUP_COUNT_EXCEEDS_THE_LIMIT,
            ));
        }
        let mut group_ids = HashSet::new();
        for group in &self.groups {
            validate_name(&group.name)?;
            if group.id.is_nil() || !group_ids.insert(group.id) {
                return Err(invalid(
                    i18n_key::CONNECTIONS_GROUP_IDS_MUST_BE_UNIQUE_AND_NONEMPTY,
                ));
            }
        }
        self.validate_group_tree()?;
        let mut ids = HashSet::new();
        let ssh_ids: HashSet<_> = self
            .profiles
            .iter()
            .filter(|profile| matches!(profile.connection, Connection::Ssh(_)))
            .map(|profile| profile.id)
            .collect();
        for profile in &self.profiles {
            validate_name(&profile.name)?;
            if profile.id.is_nil() || !ids.insert(profile.id) {
                return Err(invalid(
                    i18n_key::CONNECTIONS_PROFILE_IDS_MUST_BE_UNIQUE_AND_NONEMPTY,
                ));
            }
            if profile.group.is_some_and(|id| !group_ids.contains(&id)) {
                return Err(invalid(
                    i18n_key::CONNECTIONS_A_PROFILE_REFERENCES_A_GROUP_THAT_DOES_NOT_EXIST,
                ));
            }
            match &profile.connection {
                Connection::Local(local) => local.validate()?,
                Connection::Ssh(ssh) => ssh.validate()?,
                Connection::Telnet(telnet) => telnet.validate()?,
                Connection::Rdp(rdp) => rdp.validate()?,
                Connection::Sqlite(sqlite) => sqlite.validate()?,
                Connection::Elasticsearch(config) => {
                    config.validate()?;
                    if config
                        .ssh_profile
                        .is_some_and(|id| id == profile.id || !ssh_ids.contains(&id))
                    {
                        return Err(invalid(i18n_key::DATABASE_A_DATABASE_SSH_TUNNEL_MUST_REFERENCE_AN_EXISTING_SSH_PROFILE));
                    }
                }
                Connection::Mysql(sql) | Connection::Postgres(sql) => {
                    sql.validate()?;
                    if sql
                        .ssh_profile
                        .is_some_and(|id| id == profile.id || !ssh_ids.contains(&id))
                    {
                        return Err(invalid(i18n_key::DATABASE_A_DATABASE_SSH_TUNNEL_MUST_REFERENCE_AN_EXISTING_SSH_PROFILE));
                    }
                }
                Connection::Redis(redis) => {
                    redis.validate()?;
                    if redis
                        .ssh_profile
                        .is_some_and(|id| id == profile.id || !ssh_ids.contains(&id))
                    {
                        return Err(invalid(i18n_key::REDIS_A_REDIS_SSH_TUNNEL_MUST_REFERENCE_AN_EXISTING_SSH_PROFILE));
                    }
                }
            }
        }
        self.workspace.validate()?;
        if self.quick_commands.len() > 256 {
            return Err(invalid(i18n_key::WORKSPACE_TOO_MANY_SAVED_COMMANDS));
        }
        if self
            .quick_commands
            .iter()
            .map(|command| command.name.len() + command.command.len())
            .sum::<usize>()
            > 1024 * 1024
        {
            return Err(invalid(
                i18n_key::WORKSPACE_SAVED_COMMANDS_EXCEED_THE_1_MIB_LIMIT,
            ));
        }
        let mut command_ids = std::collections::HashSet::new();
        for command in &self.quick_commands {
            validate_text(&command.name, 256, i18n_key::WORKSPACE_COMMAND_NAME)?;
            if command.id.is_nil()
                || !command_ids.insert(command.id)
                || command.command.len() > 65536
                || command.command.contains('\0')
            {
                return Err(invalid(i18n_key::WORKSPACE_INVALID_SAVED_COMMAND));
            }
        }
        self.settings.validate()?;
        self.app_lock.validate()
    }
}

fn invalid(message: impl Into<Message>) -> ConfigError {
    ConfigError::Validation(message.into())
}
fn validate_name(value: &str) -> Result<()> {
    validate_text(value, 128, i18n_key::CONNECTIONS_DISPLAY_NAME)
        .map_err(|error| error.at_field(i18n_key::COMMON_NAME))
}
fn validate_text(value: &str, max: usize, label: &'static str) -> Result<()> {
    if value.trim().is_empty() || value.len() > max || value.chars().any(char::is_control) {
        return Err(invalid(crate::msg!(
            i18n_key::CONNECTIONS_LABEL_IS_EMPTY_TOO_LONG_OR_CONTAINS_CONTROL_CHARACTERS,
            label = Message::new(label)
        )));
    }
    Ok(())
}
fn validate_path(value: &Path, label: &'static str) -> Result<()> {
    validate_text(&value.to_string_lossy(), 4096, label)
}

#[cfg(test)]
mod tests;
