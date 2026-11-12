use super::super::{Result, invalid};
use crate::i18n::messages as key;
use serde::{Deserialize, Serialize};
use std::{collections::HashSet, net::IpAddr};

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ForwardProtocol {
    #[default]
    Tcp,
    Http,
    Https,
}

/// Identity deliberately excludes PIDs and container IDs, which change on restart.
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(tag = "source", rename_all = "snake_case", deny_unknown_fields)]
pub enum ServiceIdentity {
    Process {
        executable: String,
        address: IpAddr,
        port: u16,
    },
    Compose {
        project: String,
        service: String,
        replica: String,
        port: u16,
    },
    Container {
        name: String,
        port: u16,
    },
}

impl ServiceIdentity {
    pub fn key(&self) -> String {
        serde_json::to_string(self).expect("service identity contains only scalar fields")
    }

    pub fn validate(&self) -> bool {
        let valid = |text: &str| {
            !text.is_empty() && text.len() <= 512 && !text.chars().any(char::is_control)
        };
        match self {
            Self::Process {
                executable, port, ..
            } => valid(executable) && *port != 0,
            Self::Compose {
                project,
                service,
                replica,
                port,
            } => valid(project) && valid(service) && valid(replica) && *port != 0,
            Self::Container { name, port } => valid(name) && *port != 0,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AutoForwardRule {
    pub service: ServiceIdentity,
    pub enabled: bool,
    /// A concrete local interface, never a wildcard. Loopback is the default.
    pub bind_address: IpAddr,
    /// None chooses the remote port, then an OS-assigned free port on conflict.
    pub local_port: Option<u16>,
    #[serde(default)]
    pub last_port: Option<u16>,
    #[serde(default)]
    pub protocol: ForwardProtocol,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct AutoForwardConfig {
    pub enabled: bool,
    pub projects: Vec<String>,
    pub services: Vec<AutoForwardRule>,
}

impl AutoForwardConfig {
    pub fn validate(&self) -> Result<()> {
        let invalid_rule = || invalid(key::CONNECTIONS_INVALID_SSH_PORT_FORWARDING_RULE);
        if self.projects.len() > 64 || self.services.len() > 128 {
            return Err(invalid_rule());
        }
        let mut projects = HashSet::new();
        for project in &self.projects {
            if project.is_empty()
                || project.len() > 256
                || project.chars().any(char::is_control)
                || !projects.insert(project)
            {
                return Err(invalid_rule());
            }
        }
        let mut services = HashSet::new();
        for rule in &self.services {
            if !rule.service.validate()
                || !services.insert(&rule.service)
                || rule.local_port == Some(0)
                || rule.last_port == Some(0)
                || rule.bind_address.is_unspecified()
                || rule.bind_address.is_multicast()
            {
                return Err(invalid_rule());
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::SshConfig;

    #[test]
    fn old_profiles_load_without_discovery_or_listener_identity() {
        let config: SshConfig = serde_json::from_value(serde_json::json!({
            "host":"server.example", "username":"developer", "forwards":[
                {"kind":"local", "bind_host":"127.0.0.1", "bind_port":3000, "destination_host":"127.0.0.1", "destination_port":3000, "enabled":true}
            ]
        })).unwrap();
        assert!(!config.auto_forward.enabled);
        assert!(!config.forwards[0].id.is_nil());
        let saved = toml::to_string(&config).unwrap();
        let restored: SshConfig = toml::from_str(&saved).unwrap();
        assert_eq!(config, restored);
    }

    #[test]
    fn service_policies_round_trip_and_reject_wildcards_duplicates_and_zero_ports() {
        let rule = AutoForwardRule {
            service: ServiceIdentity::Compose {
                project: "demo".into(),
                service: "web".into(),
                replica: "1".into(),
                port: 80,
            },
            enabled: true,
            bind_address: "192.0.2.1".parse().unwrap(),
            local_port: None,
            last_port: Some(3000),
            protocol: ForwardProtocol::Http,
        };
        let mut config = AutoForwardConfig {
            enabled: true,
            projects: vec!["demo".into()],
            services: vec![rule.clone()],
        };
        config.validate().unwrap();
        assert_eq!(
            toml::from_str::<AutoForwardConfig>(&toml::to_string(&config).unwrap()).unwrap(),
            config
        );
        for address in ["0.0.0.0", "::", "224.0.0.1", "ff02::1"] {
            config.services[0].bind_address = address.parse().unwrap();
            assert!(config.validate().is_err());
        }
        config.services[0] = rule.clone();
        config.services[0].local_port = Some(0);
        assert!(config.validate().is_err());
        config.services = vec![rule.clone(), rule];
        assert!(config.validate().is_err());
    }
}
