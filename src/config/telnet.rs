use crate::i18n::messages as i18n_key;

use super::{Result, invalid, known_hosts};
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct TelnetConfig {
    pub host: String,
    pub port: u16,
    pub timeout_secs: u64,
}

impl Default for TelnetConfig {
    fn default() -> Self {
        Self {
            host: String::new(),
            port: 23,
            timeout_secs: 15,
        }
    }
}

impl TelnetConfig {
    pub fn validate(&self) -> Result<()> {
        known_hosts::normalize_host(&self.host)
            .map_err(|error| error.at_field(i18n_key::WORKSPACE_HOST))?;
        if self.port == 0 || !(1..=600).contains(&self.timeout_secs) {
            return Err(
                invalid(i18n_key::CONNECTIONS_INVALID_TELNET_PORT_OR_TIMEOUT).at_field(
                    if self.port == 0 {
                        i18n_key::WORKSPACE_PORT
                    } else {
                        i18n_key::WORKSPACE_CONNECTION_TIMEOUT_SECONDS
                    },
                ),
            );
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::TelnetConfig;

    #[test]
    fn telnet_validation_rejects_invalid_hosts_ports_and_timeouts() {
        for host in ["console.example.invalid", "127.0.0.1", "::1", "[::1]"] {
            TelnetConfig {
                host: host.into(),
                ..TelnetConfig::default()
            }
            .validate()
            .unwrap();
        }
        for host in ["", "telnet://localhost", "host:23", "host\nname", " host"] {
            assert!(
                TelnetConfig {
                    host: host.into(),
                    ..TelnetConfig::default()
                }
                .validate()
                .is_err()
            );
        }
        for (port, timeout_secs) in [(0, 15), (23, 0), (23, 601)] {
            assert!(
                TelnetConfig {
                    host: "localhost".into(),
                    port,
                    timeout_secs,
                }
                .validate()
                .is_err()
            );
        }
    }
}
