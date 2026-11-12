use crate::i18n::messages as i18n_key;

mod openssh;
mod ports;
pub use openssh::import_openssh;
pub use ports::{AutoForwardConfig, AutoForwardRule, ForwardProtocol, ServiceIdentity};

use super::credentials::{CredentialRef, validate_reference};
use super::{Result, invalid, known_hosts, validate_path, validate_text};
use serde::{Deserialize, Serialize};
use std::{collections::HashSet, path::PathBuf};

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct SshConfig {
    pub host: String,
    pub port: u16,
    pub username: String,
    pub auth: AuthMethod,
    pub timeout_secs: u64,
    pub keepalive_secs: u64,
    pub keepalive_max: usize,
    pub shell_integration: bool,
    pub jump_hosts: Vec<SshJumpHost>,
    pub forwards: Vec<PortForward>,
    pub auto_forward: AutoForwardConfig,
}

impl Default for SshConfig {
    fn default() -> Self {
        Self {
            host: String::new(),
            port: 22,
            username: String::new(),
            auth: AuthMethod::default(),
            timeout_secs: 15,
            keepalive_secs: 30,
            keepalive_max: 3,
            shell_integration: true,
            jump_hosts: Vec::new(),
            forwards: Vec::new(),
            auto_forward: AutoForwardConfig::default(),
        }
    }
}

impl SshConfig {
    /// Identity of the destination and authentication method, independent of
    /// rotating vault references and connection timing preferences.
    pub fn same_auth_identity(&self, other: &Self) -> bool {
        self.host == other.host
            && self.port == other.port
            && self.username == other.username
            && self.jump_hosts == other.jump_hosts
            && match (&self.auth, &other.auth) {
                (AuthMethod::Password { .. }, AuthMethod::Password { .. }) => true,
                (AuthMethod::Agent { socket: left }, AuthMethod::Agent { socket: right }) => {
                    left == right
                }
                (AuthMethod::KeyboardInteractive, AuthMethod::KeyboardInteractive) => true,
                (
                    AuthMethod::PrivateKey { path: left, .. },
                    AuthMethod::PrivateKey { path: right, .. },
                ) => left == right,
                _ => false,
            }
    }

    pub fn validate(&self) -> Result<()> {
        known_hosts::normalize_host(&self.host)
            .map_err(|error| error.at_field(i18n_key::WORKSPACE_HOST))?;
        validate_text(&self.username, 256, i18n_key::CONNECTIONS_SSH_USERNAME)
            .map_err(|error| error.at_field(i18n_key::WORKSPACE_USERNAME))?;
        let invalid_number = if self.port == 0 {
            Some(i18n_key::WORKSPACE_PORT)
        } else if !(1..=600).contains(&self.timeout_secs) {
            Some(i18n_key::WORKSPACE_CONNECTION_TIMEOUT_SECONDS)
        } else if self.keepalive_secs > 3600 {
            Some(i18n_key::WORKSPACE_KEEPALIVE_INTERVAL_SECONDS_0_DISABLES)
        } else if !(1..=100).contains(&self.keepalive_max) {
            Some(i18n_key::WORKSPACE_KEEPALIVE_FAILURE_LIMIT)
        } else {
            None
        };
        if let Some(field) = invalid_number {
            return Err(invalid(
                i18n_key::CONNECTIONS_INVALID_SSH_PORT_TIMEOUT_OR_KEEPALIVE_SETTINGS,
            )
            .at_field(field));
        }
        match &self.auth {
            AuthMethod::Password { credential } => validate_reference(credential)?,
            AuthMethod::PrivateKey { path, passphrase } => {
                validate_path(path, i18n_key::CONNECTIONS_PRIVATE_KEY_PATH)
                    .map_err(|error| error.at_field(i18n_key::CONNECTIONS_PRIVATE_KEY_PATH))?;
                validate_reference(passphrase)?;
            }
            AuthMethod::Agent { socket } => {
                if let Some(path) = socket {
                    validate_path(path, i18n_key::CONNECTIONS_SSH_AGENT_SOCKET).map_err(
                        |error| error.at_field(i18n_key::CONNECTIONS_SSH_AGENT_SOCKET_OPTIONAL),
                    )?;
                }
            }
            AuthMethod::KeyboardInteractive => {}
        }
        if self.jump_hosts.len() > 8 {
            return Err(
                invalid(i18n_key::CONNECTIONS_TOO_MANY_SSH_JUMP_HOSTS_OR_PORT_FORWARDS)
                    .at_field(i18n_key::CONNECTIONS_JUMP_HOSTS_COMMA_SEPARATED),
            );
        }
        for hop in &self.jump_hosts {
            hop.validate().map_err(|error| {
                error.at_field(i18n_key::CONNECTIONS_JUMP_HOSTS_COMMA_SEPARATED)
            })?;
        }
        validate_forwards(&self.forwards)?;
        self.auto_forward.validate()
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "method", rename_all = "snake_case", deny_unknown_fields)]
pub enum AuthMethod {
    Password {
        credential: Option<CredentialRef>,
    },
    PrivateKey {
        path: PathBuf,
        passphrase: Option<CredentialRef>,
    },
    Agent {
        socket: Option<PathBuf>,
    },
    KeyboardInteractive,
}
impl Default for AuthMethod {
    fn default() -> Self {
        Self::Password { credential: None }
    }
}

pub type SshProfile = SshConfig;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SshJumpHost {
    pub host: String,
    pub port: u16,
    pub username: String,
    pub identity_file: Option<PathBuf>,
    pub agent_socket: Option<PathBuf>,
}
impl SshJumpHost {
    pub fn validate(&self) -> Result<()> {
        known_hosts::normalize_host(&self.host)
            .map_err(|error| error.at_field(i18n_key::WORKSPACE_HOST))?;
        validate_text(&self.username, 256, i18n_key::CONNECTIONS_SSH_USERNAME)
            .map_err(|error| error.at_field(i18n_key::WORKSPACE_USERNAME))?;
        if self.port == 0 {
            return Err(invalid(i18n_key::CONNECTIONS_INVALID_SSH_JUMP_HOST));
        }
        for path in [&self.identity_file, &self.agent_socket]
            .into_iter()
            .flatten()
        {
            validate_path(path, i18n_key::CONNECTIONS_PRIVATE_KEY_PATH)
                .map_err(|error| error.at_field(i18n_key::CONNECTIONS_PRIVATE_KEY_PATH))?;
        }
        Ok(())
    }
    pub fn address(&self) -> String {
        format!("{}@{}", self.username, endpoint(&self.host, self.port))
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ForwardKind {
    #[default]
    Local,
    Remote,
    Dynamic,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PortForward {
    #[serde(default = "super::Uuid::new_v4")]
    pub id: super::Uuid,
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub protocol: ForwardProtocol,
    pub kind: ForwardKind,
    pub bind_host: String,
    pub bind_port: u16,
    pub destination_host: String,
    pub destination_port: u16,
    #[serde(default = "enabled")]
    pub enabled: bool,
}
fn enabled() -> bool {
    true
}
impl PortForward {
    pub fn validate(&self) -> Result<()> {
        if self.name.len() > 256 || self.name.chars().any(char::is_control) {
            return Err(invalid(
                i18n_key::CONNECTIONS_INVALID_SSH_PORT_FORWARDING_RULE,
            ));
        }
        known_hosts::normalize_host(&self.bind_host)?;
        if self.bind_port == 0 {
            return Err(invalid(
                i18n_key::CONNECTIONS_A_FORWARDING_PORT_MUST_BE_BETWEEN_1_AND_65535,
            ));
        }
        if self.kind != ForwardKind::Dynamic {
            known_hosts::normalize_host(&self.destination_host)?;
            if self.destination_port == 0 {
                return Err(invalid(
                    i18n_key::CONNECTIONS_A_FORWARDING_PORT_MUST_BE_BETWEEN_1_AND_65535,
                ));
            }
        }
        Ok(())
    }
    pub fn description(&self) -> String {
        let kind = match self.kind {
            ForwardKind::Local => "L",
            ForwardKind::Remote => "R",
            ForwardKind::Dynamic => "D",
        };
        if self.kind == ForwardKind::Dynamic {
            format!("{kind} {}", endpoint(&self.bind_host, self.bind_port))
        } else {
            format!(
                "{kind} {} {}",
                endpoint(&self.bind_host, self.bind_port),
                endpoint(&self.destination_host, self.destination_port)
            )
        }
    }
}

pub(crate) fn validate_forwards(forwards: &[PortForward]) -> Result<()> {
    const FIELD: &str = i18n_key::CONNECTIONS_PORT_FORWARDS_SEMICOLON_SEPARATED;
    if forwards.len() > 32 {
        return Err(
            invalid(i18n_key::CONNECTIONS_TOO_MANY_SSH_JUMP_HOSTS_OR_PORT_FORWARDS).at_field(FIELD),
        );
    }
    let mut listeners = HashSet::with_capacity(forwards.len());
    let mut identities = HashSet::new();
    for forward in forwards {
        forward.validate().map_err(|error| error.at_field(FIELD))?;
        let host = known_hosts::normalize_host(&forward.bind_host)
            .map_err(|error| error.at_field(FIELD))?;
        let endpoint = (forward.kind == ForwardKind::Remote, host, forward.bind_port);
        if !listeners.insert(endpoint) {
            return Err(
                invalid(i18n_key::CONNECTIONS_DUPLICATE_SSH_FORWARDING_LISTENER).at_field(FIELD),
            );
        }
        if forward.id.is_nil() || !identities.insert(forward.id) {
            return Err(invalid(
                i18n_key::CONNECTIONS_INVALID_SSH_PORT_FORWARDING_RULE,
            ));
        }
    }
    Ok(())
}

pub fn endpoint(host: &str, port: u16) -> String {
    if host.contains(':') {
        format!("[{host}]:{port}")
    } else {
        format!("{host}:{port}")
    }
}
fn parse_endpoint(
    text: &str,
    default_host: Option<&str>,
    default_port: Option<u16>,
) -> Result<(String, u16)> {
    let (host, port) = if let Some(rest) = text.strip_prefix('[') {
        let (host, suffix) = rest
            .split_once(']')
            .ok_or_else(|| invalid(i18n_key::CONNECTIONS_INVALID_SSH_ENDPOINT))?;
        (host, suffix.strip_prefix(':'))
    } else if let Some((host, port)) = text.rsplit_once(':') {
        (host, Some(port))
    } else if default_host.is_some() {
        (default_host.unwrap_or("127.0.0.1"), Some(text))
    } else {
        (text, None)
    };
    let port = match port {
        Some(port) => port.parse::<u16>().ok(),
        None => default_port,
    }
    .filter(|p| *p != 0)
    .ok_or_else(|| invalid(i18n_key::CONNECTIONS_INVALID_SSH_ENDPOINT))?;
    let host = known_hosts::normalize_host(host)?;
    Ok((host, port))
}
pub fn parse_forwards(text: &str) -> Result<Vec<PortForward>> {
    let mut forwards = Vec::new();
    for line in text
        .split(['\n', ';'])
        .map(str::trim)
        .filter(|line| !line.is_empty())
    {
        let parts: Vec<_> = line.split_whitespace().collect();
        let kind = match parts.first().copied() {
            Some("L") => ForwardKind::Local,
            Some("R") => ForwardKind::Remote,
            Some("D") => ForwardKind::Dynamic,
            _ => {
                return Err(invalid(
                    i18n_key::CONNECTIONS_USE_L_BIND_PORT_TARGET_PORT_R_BIND_PORT_TARGET,
                ));
            }
        };
        if parts.len() != if kind == ForwardKind::Dynamic { 2 } else { 3 } {
            return Err(invalid(
                i18n_key::CONNECTIONS_INVALID_SSH_PORT_FORWARDING_RULE,
            ));
        }
        let (bind_host, bind_port) = parse_endpoint(parts[1], Some("127.0.0.1"), None)?;
        let (destination_host, destination_port) = if kind == ForwardKind::Dynamic {
            (String::new(), 0)
        } else {
            parse_endpoint(parts[2], None, None)?
        };
        let forward = PortForward {
            id: uuid::Uuid::new_v4(),
            name: String::new(),
            protocol: Default::default(),
            kind,
            bind_host,
            bind_port,
            destination_host,
            destination_port,
            enabled: true,
        };
        forwards.push(forward);
        if forwards.len() > 32 {
            return Err(
                invalid(i18n_key::CONNECTIONS_TOO_MANY_SSH_JUMP_HOSTS_OR_PORT_FORWARDS)
                    .at_field(i18n_key::CONNECTIONS_PORT_FORWARDS_SEMICOLON_SEPARATED),
            );
        }
    }
    validate_forwards(&forwards)?;
    Ok(forwards)
}
pub fn parse_jump_hosts(text: &str, username: &str) -> Result<Vec<SshJumpHost>> {
    let mut hops = Vec::new();
    for value in text.split(',').map(str::trim).filter(|v| !v.is_empty()) {
        let (user, address) = value.rsplit_once('@').unwrap_or((username, value));
        let (host, port) = parse_endpoint(address, None, Some(22))?;
        let hop = SshJumpHost {
            host,
            port,
            username: user.into(),
            identity_file: None,
            agent_socket: None,
        };
        hop.validate()
            .map_err(|error| error.at_field(i18n_key::CONNECTIONS_JUMP_HOSTS_COMMA_SEPARATED))?;
        hops.push(hop);
        if hops.len() > 8 {
            return Err(
                invalid(i18n_key::CONNECTIONS_TOO_MANY_SSH_JUMP_HOSTS_OR_PORT_FORWARDS)
                    .at_field(i18n_key::CONNECTIONS_JUMP_HOSTS_COMMA_SEPARATED),
            );
        }
    }
    Ok(hops)
}

#[cfg(test)]
mod tests {
    use super::{AuthMethod, ForwardKind, SshConfig, parse_forwards, parse_jump_hosts};
    use crate::config::CredentialRef;
    use crate::i18n::messages as i18n_key;
    use uuid::Uuid;

    #[test]
    fn forwarding_and_jump_endpoints_preserve_ipv6() {
        let forwards = parse_forwards("L [::1]:8080 [2001:db8::1]:80; D 127.0.0.1:1080").unwrap();
        assert_eq!(forwards[0].destination_host, "2001:db8::1");
        assert_eq!(forwards[1].kind, ForwardKind::Dynamic);
        assert!(parse_forwards("L 127.0.0.1:0 host:22").is_err());
        let hops = parse_jump_hosts("alice@[::1]:2222,bob@host", "user").unwrap();
        assert_eq!(hops[0].port, 2222);
        assert_eq!(hops[1].username, "bob");
        let too_many_hops = ["user@host"; 9].join(",");
        assert_eq!(
            parse_jump_hosts(&too_many_hops, "user")
                .unwrap_err()
                .field(),
            Some(i18n_key::CONNECTIONS_JUMP_HOSTS_COMMA_SEPARATED),
        );
        let too_many_forwards = (10000..10033)
            .map(|port| format!("D 127.0.0.1:{port}"))
            .collect::<Vec<_>>()
            .join(";");
        assert_eq!(
            parse_forwards(&too_many_forwards).unwrap_err().field(),
            Some(i18n_key::CONNECTIONS_PORT_FORWARDS_SEMICOLON_SEPARATED),
        );
    }

    #[test]
    fn forwarding_validation_rejects_collisions_in_direct_config_and_text() {
        for (first_kind, second_kind, first_host, second_host) in [
            (
                ForwardKind::Remote,
                ForwardKind::Remote,
                "LOCALHOST.",
                "localhost",
            ),
            (
                ForwardKind::Remote,
                ForwardKind::Remote,
                "::1",
                "0:0:0:0:0:0:0:1",
            ),
            (
                ForwardKind::Local,
                ForwardKind::Dynamic,
                "127.0.0.1",
                "127.0.0.1",
            ),
            (
                ForwardKind::Dynamic,
                ForwardKind::Local,
                "127.0.0.1",
                "127.0.0.1",
            ),
        ] {
            let mut first = parse_forwards("L 127.0.0.1:2222 host:22")
                .unwrap()
                .remove(0);
            first.kind = first_kind;
            first.bind_host = first_host.into();
            let mut second = first.clone();
            second.kind = second_kind;
            second.bind_host = second_host.into();
            second.enabled = false;
            let text = format!("{}; {}", first.description(), second.description());
            let config = SshConfig {
                host: "server.example".into(),
                username: "operator".into(),
                forwards: vec![first, second],
                ..SshConfig::default()
            };
            for error in [
                config.validate().unwrap_err(),
                parse_forwards(&text).unwrap_err(),
            ] {
                assert_eq!(
                    error.field(),
                    Some(i18n_key::CONNECTIONS_PORT_FORWARDS_SEMICOLON_SEPARATED)
                );
                assert!(error.to_string().contains(crate::i18n::tr(
                    i18n_key::CONNECTIONS_DUPLICATE_SSH_FORWARDING_LISTENER
                )));
            }
        }
    }

    #[test]
    fn forwarding_validation_keeps_local_and_remote_namespaces_separate() {
        for kind in ["L 127.0.0.1:2222 host:22", "D 127.0.0.1:2222"] {
            let forwards = parse_forwards(&format!("{kind}; R 127.0.0.1:2222 host:22")).unwrap();
            SshConfig {
                host: "server.example".into(),
                username: "operator".into(),
                forwards,
                ..SshConfig::default()
            }
            .validate()
            .unwrap();
        }
    }

    #[test]
    fn ssh_auth_identity_ignores_rotating_vault_references_and_timing() {
        let left = SshConfig {
            host: "gateway.example.invalid".into(),
            username: "operator".into(),
            auth: AuthMethod::Password {
                credential: Some(CredentialRef(Uuid::from_u128(1))),
            },
            ..SshConfig::default()
        };
        let mut right = SshConfig {
            auth: AuthMethod::Password {
                credential: Some(CredentialRef(Uuid::from_u128(2))),
            },
            timeout_secs: 30,
            keepalive_secs: 60,
            keepalive_max: 4,
            shell_integration: false,
            ..left.clone()
        };
        assert!(left.same_auth_identity(&right));
        right.host = "another.example.invalid".into();
        assert!(!left.same_auth_identity(&right));
        right = left.clone();
        right.port = 2222;
        assert!(!left.same_auth_identity(&right));
        right = left.clone();
        right.username = "another-user".into();
        assert!(!left.same_auth_identity(&right));
        right = left.clone();
        right.auth = AuthMethod::PrivateKey {
            path: "/keys/id_ed25519".into(),
            passphrase: None,
        };
        assert!(!left.same_auth_identity(&right));
        let mut same_key = right.clone();
        same_key.auth = AuthMethod::PrivateKey {
            path: "/keys/id_ed25519".into(),
            passphrase: Some(CredentialRef(Uuid::from_u128(3))),
        };
        assert!(right.same_auth_identity(&same_key));
        same_key.auth = AuthMethod::PrivateKey {
            path: "/keys/other".into(),
            passphrase: None,
        };
        assert!(!right.same_auth_identity(&same_key));
    }

    #[test]
    fn shell_integration_defaults_on_and_preserves_explicit_opt_out() {
        let previous: SshConfig =
            toml::from_str("host = 'example.invalid'\nusername = 'operator'").unwrap();
        assert!(previous.shell_integration);
        let disabled = SshConfig {
            shell_integration: false,
            ..previous
        };
        let restored: SshConfig = toml::from_str(&toml::to_string(&disabled).unwrap()).unwrap();
        assert_eq!(restored, disabled);
    }
}
