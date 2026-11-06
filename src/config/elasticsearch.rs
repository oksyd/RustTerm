use super::{CredentialRef, Result, invalid, validate_text};
use crate::i18n::messages as msg;
use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use uuid::Uuid;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ElasticsearchAuth {
    #[default]
    None,
    Basic,
    ApiKey,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ElasticsearchConfig {
    pub endpoint: String,
    pub auth: ElasticsearchAuth,
    pub username: String,
    pub ca_file: Option<PathBuf>,
    pub timeout_secs: u64,
    pub query_timeout_secs: u64,
    pub credential: Option<CredentialRef>,
    pub ssh_profile: Option<Uuid>,
}

impl Default for ElasticsearchConfig {
    fn default() -> Self {
        Self {
            endpoint: "http://127.0.0.1:9200".into(),
            auth: ElasticsearchAuth::None,
            username: String::new(),
            ca_file: None,
            timeout_secs: 10,
            query_timeout_secs: 30,
            credential: None,
            ssh_profile: None,
        }
    }
}

impl ElasticsearchConfig {
    pub fn url(&self) -> Result<url::Url> {
        let fail = || invalid(msg::ES_INVALID_ENDPOINT).at_field(msg::ES_ENDPOINT);
        let mut url = url::Url::parse(&self.endpoint).map_err(|_| fail())?;
        if !matches!(url.scheme(), "http" | "https")
            || url.host_str().is_none()
            || url.port() == Some(0)
            || !url.username().is_empty()
            || url.password().is_some()
            || url.query().is_some()
            || url.fragment().is_some()
            || self.endpoint.trim() != self.endpoint
            || self.endpoint.chars().any(char::is_control)
        {
            return Err(fail());
        }
        if !url.path().ends_with('/') {
            url.set_path(&format!("{}/", url.path()));
        }
        Ok(url)
    }

    pub fn validate(&self) -> Result<()> {
        self.url()?;
        if self.auth == ElasticsearchAuth::None && self.credential.is_some() {
            return Err(invalid(msg::ES_INVALID_CONFIG));
        }
        if self.auth == ElasticsearchAuth::Basic {
            validate_text(&self.username, 256, msg::WORKSPACE_USERNAME)
                .map_err(|error| error.at_field(msg::WORKSPACE_USERNAME))?;
            if self.username.contains(':') {
                return Err(invalid(msg::ES_INVALID_USERNAME).at_field(msg::WORKSPACE_USERNAME));
            }
        }
        if !(1..=600).contains(&self.timeout_secs) || !(1..=3600).contains(&self.query_timeout_secs)
        {
            return Err(
                invalid(msg::DATABASE_USE_A_CONNECTION_TIMEOUT_OF_1_600_SECONDS_AND_A).at_field(
                    if !(1..=600).contains(&self.timeout_secs) {
                        msg::WORKSPACE_CONNECTION_TIMEOUT_SECONDS
                    } else {
                        msg::DATABASE_QUERY_TIMEOUT_SECONDS
                    },
                ),
            );
        }
        if let Some(path) = &self.ca_file
            && (path.as_os_str().is_empty() || path.to_string_lossy().chars().any(char::is_control))
        {
            return Err(invalid(msg::ES_INVALID_CA).at_field(msg::ES_CA_FILE));
        }
        super::credentials::validate_reference(&self.credential)?;
        Ok(())
    }

    pub fn same_auth_identity(&self, other: &Self) -> bool {
        matches!((self.url(),other.url()),(Ok(first),Ok(second)) if first == second)
            && self.auth == other.auth
            && self.username == other.username
            && self.ca_file == other.ca_file
            && self.ssh_profile == other.ssh_profile
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn endpoints_separate_authentication_from_urls_and_validate_timeouts() {
        for endpoint in [
            "http://127.0.0.1:9200",
            "https://example.invalid/es",
            "https://[::1]:9200/",
        ] {
            let config = ElasticsearchConfig {
                endpoint: endpoint.into(),
                ..Default::default()
            };
            assert!(config.validate().is_ok());
            assert!(config.url().unwrap().path().ends_with('/'));
        }
        for endpoint in [
            "",
            "file:///tmp/es",
            "ftp://host",
            "http://host:0",
            "http://user:secret@host",
            "http://host?token=secret",
            "http://host#fragment",
            " http://host",
            "http://host\n",
        ] {
            let config = ElasticsearchConfig {
                endpoint: endpoint.into(),
                ..Default::default()
            };
            assert!(config.validate().is_err(), "{endpoint}");
        }
        let mut config = ElasticsearchConfig {
            auth: ElasticsearchAuth::Basic,
            ..Default::default()
        };
        assert!(config.validate().is_err());
        config.username = "name:password".into();
        assert!(config.validate().is_err());
        config.username = "operator".into();
        assert!(config.validate().is_ok());
        config.timeout_secs = 0;
        assert!(config.validate().is_err());
        config.timeout_secs = 600;
        config.query_timeout_secs = 3601;
        assert!(config.validate().is_err());
    }

    #[test]
    fn credentials_follow_the_endpoint_authentication_and_tunnel_identity() {
        let original = ElasticsearchConfig {
            endpoint: "https://example.invalid/es".into(),
            auth: ElasticsearchAuth::Basic,
            username: "operator".into(),
            ..Default::default()
        };
        let mut edited = original.clone();
        edited.endpoint.push('/');
        edited.timeout_secs = 60;
        assert!(original.same_auth_identity(&edited));
        for edited in [
            ElasticsearchConfig {
                endpoint: "https://other.invalid/es".into(),
                ..original.clone()
            },
            ElasticsearchConfig {
                endpoint: "https://example.invalid/other".into(),
                ..original.clone()
            },
            ElasticsearchConfig {
                auth: ElasticsearchAuth::ApiKey,
                ..original.clone()
            },
            ElasticsearchConfig {
                username: "another".into(),
                ..original.clone()
            },
            ElasticsearchConfig {
                ssh_profile: Some(Uuid::new_v4()),
                ..original.clone()
            },
            ElasticsearchConfig {
                ca_file: Some("different-ca.pem".into()),
                ..original.clone()
            },
        ] {
            assert!(!original.same_auth_identity(&edited));
        }
        let invalid = ElasticsearchConfig {
            endpoint: "invalid".into(),
            ..original
        };
        assert!(!invalid.same_auth_identity(&invalid));
    }
}
