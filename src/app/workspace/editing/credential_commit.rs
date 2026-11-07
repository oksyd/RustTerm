use crate::app::forms::CredentialChange;
use rustterm::config::{
    AppConfig, AppPaths, ConfigError, CredentialRef, CredentialStore, ProcessCredentialStore, Uuid,
    credentials::CredentialError,
};
use rustterm::i18n::messages as i18n_key;

#[derive(Debug, thiserror::Error)]
pub(super) enum SaveError {
    #[error("{0}")]
    Configuration(#[from] ConfigError),
    #[error("{}", rustterm::trf!(i18n_key::CONNECTIONS_COULD_NOT_SAVE_THE_CONNECTION_PASSWORD_ERROR, error = .0.to_string()))]
    Credential(#[from] CredentialError),
    #[error(
        "{}",
        rustterm::i18n::tr(
            i18n_key::CONNECTIONS_THIS_CONNECTION_NO_LONGER_SUPPORTS_THE_REQUESTED_PASSWORD_CHANGE
        )
    )]
    InvalidTarget,
}

impl SaveError {
    pub fn was_committed(&self) -> bool {
        matches!(self, Self::Configuration(error) if error.was_committed())
    }
}

pub(super) struct SaveOutcome {
    pub config: AppConfig,
    pub result: Result<(), SaveError>,
    pub unused_credential: Option<CredentialRef>,
}

pub(super) fn save(
    config: AppConfig,
    credential: Option<(Uuid, CredentialChange)>,
    paths: &AppPaths,
) -> SaveOutcome {
    commit(
        config,
        credential,
        &ProcessCredentialStore::new(),
        |config| paths.save_config(config),
    )
}

fn commit(
    mut config: AppConfig,
    credential: Option<(Uuid, CredentialChange)>,
    store: &impl CredentialStore,
    save_config: impl FnOnce(&AppConfig) -> Result<(), ConfigError>,
) -> SaveOutcome {
    let mut created = None;
    let result = (|| {
        if let Some((id, change)) = credential {
            let profile = config
                .profiles
                .iter_mut()
                .find(|profile| profile.id == id)
                .ok_or(SaveError::InvalidTarget)?;
            match change {
                CredentialChange::Keep => {}
                CredentialChange::Remove => {
                    if !profile.connection.set_credential(None) {
                        return Err(SaveError::InvalidTarget);
                    }
                }
                CredentialChange::Store(secret) => {
                    let reference = CredentialRef::new();
                    let mut connection = profile.connection.clone();
                    if !connection.set_credential(Some(reference)) {
                        return Err(SaveError::InvalidTarget);
                    }
                    created = Some(reference);
                    // Never overwrite the old entry: a failed config commit must leave it usable.
                    store.set(reference, &secret)?;
                    profile.connection = connection;
                }
            }
        }
        save_config(&config).map_err(SaveError::from)
    })();
    let committed = result
        .as_ref()
        .map_or_else(|error| error.was_committed(), |_| true);
    SaveOutcome {
        config,
        result,
        unused_credential: (!committed).then_some(created).flatten(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rustterm::config::{AuthMethod, Profile, Secret, SshConfig};
    use std::{cell::RefCell, collections::HashMap};

    #[derive(Default)]
    struct Vault {
        entries: RefCell<HashMap<CredentialRef, String>>,
        fail_write: bool,
    }

    impl CredentialStore for Vault {
        fn get(&self, reference: CredentialRef) -> Result<Secret, CredentialError> {
            self.entries
                .borrow()
                .get(&reference)
                .cloned()
                .map(Secret::new)
                .ok_or(CredentialError::Missing)
        }

        fn set(&self, reference: CredentialRef, secret: &Secret) -> Result<(), CredentialError> {
            self.entries
                .borrow_mut()
                .insert(reference, secret.expose().to_owned());
            if self.fail_write {
                Err(CredentialError::OutcomeUnknown)
            } else {
                Ok(())
            }
        }

        fn delete(&self, reference: CredentialRef) -> Result<(), CredentialError> {
            self.entries.borrow_mut().remove(&reference);
            Ok(())
        }
    }

    fn fixture() -> (AppConfig, Uuid, CredentialRef, Vault) {
        let previous = CredentialRef::new();
        let profile = Profile::ssh(
            "Server",
            SshConfig {
                auth: AuthMethod::Password {
                    credential: Some(previous),
                },
                ..Default::default()
            },
        );
        let id = profile.id;
        let vault = Vault::default();
        vault
            .entries
            .borrow_mut()
            .insert(previous, "previous password".into());
        (
            AppConfig {
                profiles: vec![profile],
                ..Default::default()
            },
            id,
            previous,
            vault,
        )
    }

    #[test]
    fn new_password_exists_before_config_commit_without_overwriting_previous_entry() {
        let (config, id, previous, vault) = fixture();
        let outcome = commit(
            config,
            Some((
                id,
                CredentialChange::Store(Secret::new("new password".into())),
            )),
            &vault,
            |candidate| {
                let reference = candidate.profiles[0].connection.credential_ref().unwrap();
                assert_ne!(reference, previous);
                assert_eq!(vault.get(reference).unwrap().expose(), "new password");
                assert_eq!(vault.get(previous).unwrap().expose(), "previous password");
                Ok(())
            },
        );
        assert!(outcome.result.is_ok());
        assert!(outcome.unused_credential.is_none());
        assert_eq!(vault.entries.borrow().len(), 2);
    }

    #[test]
    fn failed_or_uncertain_writes_keep_old_secret_and_classify_new_reference_for_cleanup() {
        for stage in 0..3 {
            let (config, id, previous, mut vault) = fixture();
            vault.fail_write = stage == 0;
            let outcome = commit(
                config,
                Some((
                    id,
                    CredentialChange::Store(Secret::new("new password".into())),
                )),
                &vault,
                |_| {
                    assert_ne!(
                        stage, 0,
                        "unconfirmed vault writes must not commit a reference"
                    );
                    let error = std::io::Error::other("injected failure");
                    Err(if stage == 1 {
                        ConfigError::Io(error)
                    } else {
                        ConfigError::CommitUncertain(error)
                    })
                },
            );
            assert!(outcome.result.is_err());
            assert_eq!(vault.get(previous).unwrap().expose(), "previous password");
            if stage == 2 {
                assert!(outcome.result.unwrap_err().was_committed());
                assert!(outcome.unused_credential.is_none());
                let reference = outcome.config.profiles[0]
                    .connection
                    .credential_ref()
                    .unwrap();
                assert_eq!(vault.get(reference).unwrap().expose(), "new password");
            } else {
                let reference = outcome.unused_credential.unwrap();
                assert_ne!(reference, previous);
                vault.delete(reference).unwrap();
                assert_eq!(vault.entries.borrow().len(), 1);
            }
        }
    }

    #[test]
    fn keep_and_remove_change_only_configuration_references() {
        for remove in [false, true] {
            let (config, id, previous, vault) = fixture();
            let change = if remove {
                CredentialChange::Remove
            } else {
                CredentialChange::Keep
            };
            let outcome = commit(config, Some((id, change)), &vault, |candidate| {
                assert_eq!(
                    candidate.profiles[0].connection.credential_ref(),
                    (!remove).then_some(previous)
                );
                Ok(())
            });
            assert!(outcome.result.is_ok());
            assert!(outcome.unused_credential.is_none());
            assert_eq!(vault.entries.borrow().len(), 1);
        }
    }
}
