//! The legacy algorithms here are mandated by MS-RDPELE 5.1 inside verified TLS.
//! They never select or replace the transport's security protocol.
use crate::{Error, Result};
use md5::{Digest, Md5};
use rc4::{KeyInit, Rc4, StreamCipher, consts::U16};
use rsa::{BigUint, RsaPublicKey, traits::PublicKeyParts};
use sha1::Sha1;
use subtle::ConstantTimeEq;
use zeroize::{Zeroize, Zeroizing};

pub(super) struct Keys {
    salt: [u8; 16],
    encryption: [u8; 16],
}

impl Keys {
    pub fn derive(premaster: &[u8; 48], client: &[u8; 32], server: &[u8; 32]) -> Self {
        let master = salted(premaster, client, server);
        let session = salted(master.as_slice(), server, client);
        let mut salt = [0; 16];
        salt.copy_from_slice(&session[..16]);
        let mut digest = Md5::new();
        digest.update(&session[16..32]);
        digest.update(client);
        digest.update(server);
        let encryption = digest.finalize().into();
        Self { salt, encryption }
    }

    pub fn crypt(&self, bytes: &[u8]) -> Zeroizing<Vec<u8>> {
        let mut output = Zeroizing::new(bytes.to_vec());
        // Each independently encrypted BLOB starts a fresh RC4 state.
        Rc4::<U16>::new((&self.encryption).into()).apply_keystream(&mut output);
        output
    }

    pub fn mac(&self, bytes: &[u8]) -> Result<[u8; 16]> {
        let length = u32::try_from(bytes.len()).map_err(|_| Error::Limit("licensing MAC input"))?;
        let mut sha = Sha1::new();
        sha.update(self.salt);
        sha.update([0x36; 40]);
        sha.update(length.to_le_bytes());
        sha.update(bytes);
        let hash = Zeroizing::new(<[u8; 20]>::from(sha.finalize()));
        let mut md5 = Md5::new();
        md5.update(self.salt);
        md5.update([0x5c; 48]);
        md5.update(hash.as_slice());
        Ok(md5.finalize().into())
    }

    pub fn decrypt_checked(
        &self,
        encrypted: &[u8],
        signature: &[u8],
    ) -> Result<Zeroizing<Vec<u8>>> {
        let plain = self.crypt(encrypted);
        let expected = self.mac(&plain)?;
        if expected.as_slice().ct_eq(signature).unwrap_u8() != 1 {
            return Err(Error::Invalid("licensing message authentication"));
        }
        Ok(plain)
    }
}

impl Drop for Keys {
    fn drop(&mut self) {
        self.salt.zeroize();
        self.encryption.zeroize();
    }
}

fn salted(secret: &[u8], first: &[u8; 32], second: &[u8; 32]) -> Zeroizing<[u8; 48]> {
    let mut output = Zeroizing::new([0; 48]);
    for (index, label) in [b"A".as_slice(), b"BB", b"CCC"].iter().enumerate() {
        let mut sha = Sha1::new();
        sha.update(label);
        sha.update(secret);
        sha.update(first);
        sha.update(second);
        let hash = Zeroizing::new(<[u8; 20]>::from(sha.finalize()));
        let mut md5 = Md5::new();
        md5.update(secret);
        md5.update(hash.as_slice());
        output[index * 16..(index + 1) * 16].copy_from_slice(&md5.finalize());
    }
    output
}

pub(super) fn encrypt_premaster(key: &RsaPublicKey, premaster: &[u8; 48]) -> Result<Vec<u8>> {
    // MS-RDPBCGR 5.3.4 uses little-endian raw RSA followed by eight zero bytes,
    // not PKCS#1 encryption padding. The input is a fresh 384-bit random secret.
    let message = Zeroizing::new(BigUint::from_bytes_le(premaster));
    if &*message >= key.n() {
        return Err(Error::Invalid("licensing RSA modulus"));
    }
    let encrypted = rsa::hazmat::rsa_encrypt(key, &message)
        .map_err(|_| Error::Invalid("licensing RSA encryption"))?;
    let mut bytes = encrypted.to_bytes_le();
    bytes.resize(key.size() + 8, 0);
    Ok(bytes)
}
