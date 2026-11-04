//! OS Secret Service adapter. Errors never trigger plaintext persistence.

use crate::i18n::messages as i18n_key;

use serde::{Deserialize, Serialize};
use uuid::Uuid;
use zeroize::Zeroizing;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct CredentialRef(pub Uuid);

impl CredentialRef {
    pub fn new() -> Self {
        Self(Uuid::new_v4())
    }
}
impl Default for CredentialRef {
    fn default() -> Self {
        Self::new()
    }
}

/// Deliberately does not implement Debug, Display, Serialize, or Clone.
pub struct Secret(Zeroizing<String>);
impl Secret {
    pub fn new(value: String) -> Self {
        Self(Zeroizing::new(value))
    }
    pub fn expose(&self) -> &str {
        &self.0
    }
}

#[derive(Debug, thiserror::Error)]
pub enum CredentialError {
    #[error(
        "{}",
        crate::i18n::tr(
            i18n_key::CONNECTIONS_CREDENTIAL_NOT_FOUND_IN_THE_SYSTEM_VAULT_PLEASE_ENTER_IT
        )
    )]
    Missing,
    #[error(
        "{}",
        crate::i18n::tr(
            i18n_key::CONNECTIONS_THE_SYSTEM_CREDENTIAL_VAULT_IS_UNAVAILABLE_LOCKED_OR_DENIED_ACCESS
        )
    )]
    Unavailable,
    #[error(
        "{}",
        crate::i18n::tr(i18n_key::CONNECTIONS_INVALID_CREDENTIAL_REFERENCE)
    )]
    InvalidReference,
    #[error(
        "{}",
        crate::i18n::tr(
            i18n_key::CONNECTIONS_THE_CREDENTIAL_OPERATION_WAS_NOT_CONFIRMED_AND_THE_SYSTEM_VAULT
        )
    )]
    OutcomeUnknown,
}

impl CredentialError {
    pub fn may_have_committed(&self) -> bool {
        matches!(self, Self::OutcomeUnknown)
    }
}

pub trait CredentialStore {
    fn get(&self, reference: CredentialRef) -> Result<Secret, CredentialError>;
    fn set(&self, reference: CredentialRef, secret: &Secret) -> Result<(), CredentialError>;
    fn delete(&self, reference: CredentialRef) -> Result<(), CredentialError>;
}

#[derive(Default)]
pub struct SystemCredentialStore;

impl SystemCredentialStore {
    pub fn new() -> Self {
        Self
    }
    fn entry(reference: CredentialRef) -> Result<keyring::Entry, CredentialError> {
        if reference.0.is_nil() {
            return Err(CredentialError::InvalidReference);
        }
        keyring::Entry::new("io.rustterm.credentials.v1", &reference.0.to_string())
            .map_err(map_error)
    }
}

impl CredentialStore for SystemCredentialStore {
    fn get(&self, reference: CredentialRef) -> Result<Secret, CredentialError> {
        Self::entry(reference)?
            .get_password()
            .map(Secret::new)
            .map_err(map_error)
    }
    fn set(&self, reference: CredentialRef, secret: &Secret) -> Result<(), CredentialError> {
        Self::entry(reference)?
            .set_password(secret.expose())
            .map_err(map_error)
    }
    fn delete(&self, reference: CredentialRef) -> Result<(), CredentialError> {
        match Self::entry(reference)?.delete_credential() {
            Ok(()) | Err(keyring::Error::NoEntry) => Ok(()),
            Err(error) => Err(map_error(error)),
        }
    }
}

fn map_error(error: keyring::Error) -> CredentialError {
    // Do not include backend error strings: some providers expose entry attributes there.
    match error {
        keyring::Error::NoEntry => CredentialError::Missing,
        _ => CredentialError::Unavailable,
    }
}

pub(super) fn validate_reference(value: &Option<CredentialRef>) -> super::Result<()> {
    if value.as_ref().is_some_and(|r| r.0.is_nil()) {
        return Err(super::invalid(
            i18n_key::CONNECTIONS_A_CREDENTIAL_REFERENCE_CANNOT_HAVE_A_NIL_ID,
        ));
    }
    Ok(())
}
