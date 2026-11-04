//! Local application-lock password hashes. Call creation and verification off the UI thread.

use crate::i18n::messages as i18n_key;

use argon2::{
    Algorithm, Argon2, Params, PasswordHash, PasswordHasher, PasswordVerifier, Version,
    password_hash,
};

pub const MIN_PASSWORD_CHARACTERS: usize = 8;
pub const MAX_PASSWORD_BYTES: usize = 1024;
const MEMORY_KIB: u32 = 19 * 1024;
const ITERATIONS: u32 = 2;
const LANES: u32 = 1;
const SALT_BYTES: usize = 16;
const HASH_BYTES: usize = 32;
const MAX_HASH_BYTES: usize = 256;

#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
pub enum Error {
    #[error(
        "{}",
        crate::i18n::tr(i18n_key::SETTINGS_USE_AT_LEAST_8_CHARACTERS_AND_AT_MOST_1024_BYTES)
    )]
    PasswordLength,
    #[error(
        "{}",
        crate::i18n::tr(i18n_key::SETTINGS_THE_SAVED_LOCK_PASSWORD_IS_INVALID_OR_UNSUPPORTED)
    )]
    InvalidHash,
    #[error(
        "{}",
        crate::i18n::tr(i18n_key::SETTINGS_COULD_NOT_PROCESS_THE_LOCK_PASSWORD_TRY_AGAIN)
    )]
    Unavailable,
}

/// Hashes a new password with a fresh OS-generated salt; the returned PHC string is persistable.
pub fn create(password: &str) -> Result<String, Error> {
    if password.len() > MAX_PASSWORD_BYTES || password.chars().count() < MIN_PASSWORD_CHARACTERS {
        return Err(Error::PasswordLength);
    }
    let mut salt = [0u8; SALT_BYTES];
    rustls::crypto::ring::default_provider()
        .secure_random
        .fill(&mut salt)
        .map_err(|_| Error::Unavailable)?;
    hasher()?
        .hash_password_with_salt(password.as_bytes(), &salt)
        .map(|hash| hash.to_string())
        .map_err(|_| Error::Unavailable)
}

/// Returns false only for a password mismatch; a malformed saved hash remains an error.
pub fn verify(password: &str, encoded: &str) -> Result<bool, Error> {
    let hash = parse(encoded)?;
    if password.len() > MAX_PASSWORD_BYTES {
        return Err(Error::PasswordLength);
    }
    match hasher()?.verify_password(password.as_bytes(), &hash) {
        Ok(()) => Ok(true),
        Err(password_hash::Error::PasswordInvalid) => Ok(false),
        Err(_) => Err(Error::Unavailable),
    }
}

/// Checks encoding and resource costs without running Argon2 or allocating its memory buffer.
pub fn validate(encoded: &str) -> Result<(), Error> {
    parse(encoded).map(|_| ())
}

fn hasher() -> Result<Argon2<'static>, Error> {
    let params = Params::new(MEMORY_KIB, ITERATIONS, LANES, Some(HASH_BYTES))
        .map_err(|_| Error::Unavailable)?;
    Ok(Argon2::new(Algorithm::Argon2id, Version::V0x13, params))
}

fn parse(encoded: &str) -> Result<PasswordHash, Error> {
    if encoded.len() > MAX_HASH_BYTES {
        return Err(Error::InvalidHash);
    }
    let hash = PasswordHash::new(encoded).map_err(|_| Error::InvalidHash)?;
    let params = Params::try_from(&hash).map_err(|_| Error::InvalidHash)?;
    // Verification honors the encoded parameters. Accept only our supported
    // profile, so edited configuration cannot request unbounded memory or CPU.
    if hash.algorithm != argon2::ARGON2ID_IDENT
        || hash.version != Some(19)
        || params.m_cost() != MEMORY_KIB
        || params.t_cost() != ITERATIONS
        || params.p_cost() != LANES
        || !params.keyid().is_empty()
        || !params.data().is_empty()
        || hash
            .salt
            .as_ref()
            .is_none_or(|salt| salt.len() != SALT_BYTES)
        || hash
            .hash
            .as_ref()
            .is_none_or(|output| output.len() != HASH_BYTES)
    {
        return Err(Error::InvalidHash);
    }
    Ok(hash)
}

#[cfg(test)]
mod tests {
    use super::*;

    const FORMATTED: &str = "$argon2id$v=19$m=19456,t=2,p=1$MDEyMzQ1Njc4OWFiY2RlZg$AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA";

    #[test]
    fn refuses_unbounded_or_incomplete_hashes_without_hashing() {
        assert_eq!(validate(FORMATTED), Ok(()));
        for hash in [
            String::new(),
            FORMATTED.replace("argon2id", "argon2i"),
            FORMATTED.replace("v=19", "v=16"),
            FORMATTED.replace("m=19456", "m=4294967295"),
            FORMATTED.replace("t=2", "t=4294967295"),
            FORMATTED.replace("p=1", "p=32"),
            FORMATTED.replace(",p=1", ",p=1,data=YQ"),
            FORMATTED.rsplit_once('$').unwrap().0.to_owned(),
            "x".repeat(MAX_HASH_BYTES + 1),
        ] {
            assert_eq!(validate(&hash), Err(Error::InvalidHash));
        }
        assert_eq!(verify("anything", "invalid"), Err(Error::InvalidHash));
        assert_eq!(create("short"), Err(Error::PasswordLength));
        assert_eq!(
            create(&"a".repeat(MAX_PASSWORD_BYTES + 1)),
            Err(Error::PasswordLength)
        );
    }

    #[test]
    fn password_hash_round_trip_and_mismatch() {
        let password = "lock 中文 🔐 password";
        let encoded = create(password).unwrap();
        assert_eq!(validate(&encoded), Ok(()));
        assert!(!encoded.contains(password));
        assert_eq!(verify(password, &encoded), Ok(true));
        assert_eq!(verify("wrong password", &encoded), Ok(false));
    }
}
