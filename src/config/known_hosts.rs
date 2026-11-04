//! Application-owned SSH host trust records; trust changes are never implicit.

use crate::i18n::messages as i18n_key;

use super::{ConfigError, Result, invalid, storage, validate_text};
use serde::{Deserialize, Serialize};
use std::{collections::HashSet, net::IpAddr, path::Path, sync::Mutex};

static TRUST_WRITE: Mutex<()> = Mutex::new(());

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HostKey {
    pub algorithm: String,
    /// Full OpenSSH public-key encoding supplied by the SSH implementation.
    pub public_key: String,
}

impl HostKey {
    pub fn fingerprint(&self) -> Result<String> {
        let key = decode_key(&self.algorithm, &self.public_key)?;
        Ok(key.fingerprint(ssh_key::HashAlg::Sha256).to_string())
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum HostKeyStatus {
    Unknown,
    Trusted,
    Changed { trusted: Vec<HostKey> },
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TrustedHost {
    pub host: String,
    pub port: u16,
    pub keys: Vec<HostKey>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct KnownHosts {
    version: u32,
    hosts: Vec<TrustedHost>,
}

impl Default for KnownHosts {
    fn default() -> Self {
        Self {
            version: 1,
            hosts: Vec::new(),
        }
    }
}

impl KnownHosts {
    pub fn load(path: impl AsRef<Path>) -> Result<Self> {
        let Some(source) = storage::read_optional(path.as_ref())? else {
            return Ok(Self::default());
        };
        let records: Self = toml::from_str(&source)?;
        records.validate()?;
        Ok(records)
    }

    pub fn save(&self, path: impl AsRef<Path>) -> Result<()> {
        self.validate()?;
        storage::write_atomic(path.as_ref(), toml::to_string_pretty(self)?.as_bytes())
    }

    pub fn entries(&self) -> &[TrustedHost] {
        &self.hosts
    }

    /// Re-read and check under the same lock as the trust-management UI. A prompt may have
    /// remained open while another session saved a different key for this host.
    pub fn trust_and_save(
        path: impl AsRef<Path>,
        host: &str,
        port: u16,
        algorithm: &str,
        key: &str,
    ) -> Result<()> {
        let _guard = TRUST_WRITE.lock().map_err(|_| {
            invalid(i18n_key::CONNECTIONS_THE_HOST_TRUST_WRITER_FAILED_RESTART_THE_APPLICATION)
        })?;
        let path = path.as_ref();
        let mut hosts = Self::load(path)?;
        hosts.trust(host, port, algorithm, key)?;
        hosts.save(path)
    }

    pub fn remove_and_save(path: impl AsRef<Path>, host: &str, port: u16) -> Result<bool> {
        let _guard = TRUST_WRITE.lock().map_err(|_| {
            invalid(i18n_key::CONNECTIONS_THE_HOST_TRUST_WRITER_FAILED_RESTART_THE_APPLICATION)
        })?;
        let path = path.as_ref();
        let mut hosts = Self::load(path)?;
        let removed = hosts.remove(host, port)?;
        hosts.save(path)?;
        Ok(removed)
    }

    /// Delete only the fingerprints the user reviewed. A concurrent trust change requires
    /// another review rather than removing a newer record under an old confirmation.
    pub fn remove_if_unchanged_and_save(
        path: impl AsRef<Path>,
        host: &str,
        port: u16,
        expected_keys: &[HostKey],
    ) -> Result<bool> {
        let _guard = TRUST_WRITE.lock().map_err(|_| {
            invalid(i18n_key::CONNECTIONS_THE_HOST_TRUST_WRITER_FAILED_RESTART_THE_APPLICATION)
        })?;
        let host = normalize_host(host)?;
        let path = path.as_ref();
        let mut hosts = Self::load(path)?;
        let Some(record) = hosts
            .hosts
            .iter()
            .find(|record| record.host == host && record.port == port)
        else {
            return Ok(false);
        };
        if record.keys != expected_keys {
            return Err(invalid(
                i18n_key::CONNECTIONS_THE_HOST_TRUST_RECORD_HAS_CHANGED_REVIEW_IT_AGAIN_BEFORE,
            ));
        }
        let removed = hosts.remove(&host, port)?;
        hosts.save(path)?;
        Ok(removed)
    }

    /// Invalid target names cannot match an existing trusted key.
    pub fn check(&self, host: &str, port: u16, algorithm: &str, key: &str) -> HostKeyStatus {
        let Ok(host) = normalize_host(host) else {
            return HostKeyStatus::Changed {
                trusted: Vec::new(),
            };
        };
        let Some(record) = self
            .hosts
            .iter()
            .find(|record| record.host == host && record.port == port)
        else {
            return HostKeyStatus::Unknown;
        };
        if record
            .keys
            .iter()
            .any(|trusted| trusted.algorithm == algorithm && trusted.public_key == key)
        {
            HostKeyStatus::Trusted
        } else {
            // A new algorithm for a known host requires the same independent management flow.
            HostKeyStatus::Changed {
                trusted: record.keys.clone(),
            }
        }
    }

    /// Accept the first key only after an explicit first-connection confirmation.
    /// A different key or algorithm for an existing target is always rejected here.
    pub fn trust(&mut self, host: &str, port: u16, algorithm: &str, key: &str) -> Result<()> {
        validate_key(algorithm, key)?;
        match self.check(host, port, algorithm, key) {
            HostKeyStatus::Trusted => Ok(()),
            HostKeyStatus::Changed { .. } => Err(ConfigError::HostKeyChanged),
            HostKeyStatus::Unknown => self.trust_explicit(host, port, algorithm, key),
        }
    }

    /// Reserved for a separate trust-management action that displays old and new fingerprints.
    /// Replaces the key for this algorithm; preserves other independently confirmed algorithms.
    pub fn trust_explicit(
        &mut self,
        host: &str,
        port: u16,
        algorithm: &str,
        key: &str,
    ) -> Result<()> {
        let host = normalize_host(host)?;
        if port == 0 {
            return Err(invalid(
                i18n_key::CONNECTIONS_THE_TRUSTED_HOST_PORT_CANNOT_BE_ZERO,
            ));
        }
        validate_key(algorithm, key)?;
        let trusted = HostKey {
            algorithm: algorithm.into(),
            public_key: key.into(),
        };
        if let Some(record) = self
            .hosts
            .iter_mut()
            .find(|r| r.host == host && r.port == port)
        {
            record.keys.retain(|old| old.algorithm != algorithm);
            record.keys.push(trusted);
        } else {
            if self.hosts.len() >= 10_000 {
                return Err(invalid(
                    i18n_key::CONNECTIONS_THE_HOST_TRUST_RECORD_LIMIT_HAS_BEEN_REACHED,
                ));
            }
            self.hosts.push(TrustedHost {
                host,
                port,
                keys: vec![trusted],
            });
        }
        Ok(())
    }

    /// Call only from the dedicated trust-management screen, not an authentication error prompt.
    pub fn remove(&mut self, host: &str, port: u16) -> Result<bool> {
        let host = normalize_host(host)?;
        let before = self.hosts.len();
        self.hosts
            .retain(|record| record.host != host || record.port != port);
        Ok(before != self.hosts.len())
    }

    fn validate(&self) -> Result<()> {
        if self.version != 1 {
            return Err(ConfigError::FutureVersion {
                found: self.version,
                supported: 1,
            });
        }
        if self.hosts.len() > 10_000 {
            return Err(invalid(i18n_key::CONNECTIONS_TOO_MANY_HOST_TRUST_RECORDS));
        }
        let mut targets = HashSet::new();
        for record in &self.hosts {
            let host = normalize_host(&record.host)?;
            if host != record.host || record.port == 0 || !targets.insert((host, record.port)) {
                return Err(invalid(
                    i18n_key::CONNECTIONS_INVALID_OR_DUPLICATE_TRUSTED_HOST_IDENTITY,
                ));
            }
            if record.keys.is_empty() || record.keys.len() > 32 {
                return Err(invalid(
                    i18n_key::CONNECTIONS_INVALID_NUMBER_OF_TRUSTED_HOST_PUBLIC_KEYS,
                ));
            }
            let mut algorithms = HashSet::new();
            for key in &record.keys {
                validate_key(&key.algorithm, &key.public_key)?;
                if !algorithms.insert(&key.algorithm) {
                    return Err(invalid(i18n_key::CONNECTIONS_THE_SAME_HOST_HAS_DUPLICATE_RECORDS_FOR_ONE_KEY_ALGORITHM));
                }
            }
        }
        Ok(())
    }
}

pub fn normalize_host(host: &str) -> Result<String> {
    if host.trim() != host
        || host.is_empty()
        || host.len() > 253
        || host.chars().any(char::is_control)
    {
        return Err(invalid(
            i18n_key::CONNECTIONS_THE_HOSTNAME_IS_EMPTY_TOO_LONG_OR_CONTAINS_WHITESPACE_OR,
        ));
    }
    let unbracketed = host
        .strip_prefix('[')
        .and_then(|h| h.strip_suffix(']'))
        .unwrap_or(host);
    if let Ok(address) = unbracketed.parse::<IpAddr>() {
        return Ok(address.to_string());
    }
    let host = host.trim_end_matches('.').to_ascii_lowercase();
    if host.is_empty()
        || host.split('.').any(|part| {
            part.is_empty()
                || part.len() > 63
                || part.starts_with('-')
                || part.ends_with('-')
                || !part
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
        })
    {
        return Err(invalid(i18n_key::CONNECTIONS_INVALID_HOSTNAME_USE_THE_ASCII_FORM_OF_INTERNATIONALIZED_DOMAIN_NAMES));
    }
    Ok(host)
}

fn validate_key(algorithm: &str, key: &str) -> Result<()> {
    decode_key(algorithm, key).map(|_| ())
}

fn decode_key(algorithm: &str, key: &str) -> Result<ssh_key::PublicKey> {
    validate_text(algorithm, 128, i18n_key::CONNECTIONS_HOST_KEY_ALGORITHM)?;
    validate_text(key, 65536, i18n_key::CONNECTIONS_HOST_PUBLIC_KEY)?;
    if algorithm.chars().any(char::is_whitespace) {
        return Err(invalid(
            i18n_key::CONNECTIONS_THE_HOST_KEY_ALGORITHM_CANNOT_CONTAIN_WHITESPACE,
        ));
    }
    // Validate the SSH wire structure as well as Base64. from_openssh also checks that
    // the textual key type agrees with the algorithm inside the decoded key blob.
    let public_key = ssh_key::PublicKey::from_openssh(key).map_err(|_| {
        invalid(i18n_key::CONNECTIONS_THE_SAVED_HOST_PUBLIC_KEY_IS_NOT_A_VALID_OPENSSH)
    })?;
    if public_key.algorithm().as_str() != algorithm {
        return Err(invalid(
            i18n_key::CONNECTIONS_THE_TRUSTED_HOST_KEY_ALGORITHM_DOES_NOT_MATCH_THE_PUBLIC,
        ));
    }
    // RSA public keys are encoded as ssh-rsa, independently of the negotiated RSA
    // signature algorithm (e.g. rsa-sha2-512). Never silently rewrite trust identities.
    Ok(public_key)
}
