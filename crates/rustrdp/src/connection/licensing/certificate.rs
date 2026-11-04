use crate::{Error, Result, wire::Reader};
use md5::{Digest, Md5};
use rsa::{BigUint, Pkcs1v15Sign, RsaPublicKey, pkcs1::DecodeRsaPublicKey, traits::PublicKeyParts};
use sha1::Sha1;
use sha2::{Sha256, Sha384, Sha512};
use subtle::ConstantTimeEq;
use x509_cert::{
    Certificate,
    der::{Decode, Encode},
};

const MAX_CHAIN: usize = 16;

pub(super) fn public_key(bytes: &[u8]) -> Result<RsaPublicKey> {
    let mut reader = Reader::new(bytes);
    match reader.u32_le()? & 0x7fff_ffff {
        1 => proprietary(bytes, reader),
        2 => x509(reader),
        _ => Err(Error::Unsupported("licensing certificate version")),
    }
}

fn proprietary(bytes: &[u8], mut reader: Reader<'_>) -> Result<RsaPublicKey> {
    if reader.u32_le()? != 1 || reader.u32_le()? != 1 {
        return Err(Error::Unsupported(
            "licensing proprietary certificate algorithm",
        ));
    }
    let key_bytes = super::blob(&mut reader, 6)?;
    let signed_length = reader.position();
    let signature = super::blob(&mut reader, 8)?;
    reader.finish()?;
    let mut key = Reader::new(key_bytes);
    if key.u32_le()? != 0x3141_5352 {
        return Err(Error::Invalid("licensing RSA magic"));
    }
    let key_length = key.u32_le()? as usize;
    let bits = key.u32_le()? as usize;
    let data_length = key.u32_le()? as usize;
    if !(512..=4096).contains(&bits)
        || !bits.is_multiple_of(8)
        || key_length != bits / 8 + 8
        || data_length != bits / 8 - 1
    {
        return Err(Error::Invalid("licensing RSA lengths"));
    }
    let exponent = key.u32_le()?;
    let modulus = BigUint::from_bytes_le(key.take(bits / 8)?);
    if key.take(8)? != [0; 8] {
        return Err(Error::Invalid("licensing RSA padding"));
    }
    key.finish()?;
    let key = RsaPublicKey::new(modulus, exponent.into())
        .map_err(|_| Error::Invalid("licensing RSA key"))?;
    if key.n().bits() != bits {
        return Err(Error::Invalid("licensing RSA bit length"));
    }
    validate_key(&key)?;

    // The well-known Terminal Services signing key authenticates the legacy
    // certificate format, not a remote identity. TLS supplies peer trust.
    let signing_modulus = [
        0x3d, 0x3a, 0x5e, 0xbd, 0x72, 0x43, 0x3e, 0xc9, 0x4d, 0xbb, 0xc1, 0x1e, 0x4a, 0xba, 0x5f,
        0xcb, 0x3e, 0x88, 0x20, 0x87, 0xef, 0xf5, 0xc1, 0xe2, 0xd7, 0xb7, 0x6b, 0x9a, 0xf2, 0x52,
        0x45, 0x95, 0xce, 0x63, 0x65, 0x6b, 0x58, 0x3a, 0xfe, 0xef, 0x7c, 0xe7, 0xbf, 0xfe, 0x3d,
        0xf6, 0x5c, 0x7d, 0x6c, 0x5e, 0x06, 0x09, 0x1a, 0xf5, 0x61, 0xbb, 0x20, 0x93, 0x09, 0x5f,
        0x05, 0x6d, 0xea, 0x87,
    ];
    let signing_key = RsaPublicKey::new(
        BigUint::from_bytes_le(&signing_modulus),
        0xc088_7b5b_u32.into(),
    )
    .map_err(|_| Error::Invalid("Terminal Services signing key"))?;
    if signature.len() != 72 || signature[64..] != [0; 8] {
        return Err(Error::Invalid("licensing proprietary signature length"));
    }
    let value = BigUint::from_bytes_le(&signature[..64]);
    if &value >= signing_key.n() {
        return Err(Error::Invalid("licensing proprietary signature"));
    }
    let decoded = rsa::hazmat::rsa_encrypt(&signing_key, &value)
        .map_err(|_| Error::Invalid("licensing proprietary signature"))?
        .to_bytes_le();
    let mut expected = [0xff; 63];
    expected[..16].copy_from_slice(&Md5::digest(&bytes[..signed_length]));
    expected[16] = 0;
    expected[62] = 1;
    if decoded.as_slice().ct_eq(&expected).unwrap_u8() != 1 {
        return Err(Error::Invalid("licensing proprietary signature"));
    }
    Ok(key)
}

fn x509(mut reader: Reader<'_>) -> Result<RsaPublicKey> {
    let count = reader.u32_le()? as usize;
    if !(2..=MAX_CHAIN).contains(&count) {
        return Err(Error::Limit("licensing X509 chain"));
    }
    let mut issuer: Option<Certificate> = None;
    let mut issuer_key = None;
    for _ in 0..count {
        let length = reader.u32_le()? as usize;
        let certificate = Certificate::from_der(reader.take(length)?)
            .map_err(|_| Error::Invalid("licensing X509 certificate"))?;
        let public_key = x509_key(&certificate)?;
        let signing_key = if let Some(previous) = &issuer {
            if previous.tbs_certificate.subject != certificate.tbs_certificate.issuer {
                return Err(Error::Invalid("licensing X509 issuer"));
            }
            issuer_key
                .as_ref()
                .ok_or(Error::Invalid("licensing X509 issuer key"))?
        } else {
            if certificate.tbs_certificate.subject != certificate.tbs_certificate.issuer {
                return Err(Error::Invalid("licensing X509 root"));
            }
            &public_key
        };
        verify_x509(&certificate, signing_key)?;
        issuer = Some(certificate);
        issuer_key = Some(public_key);
    }
    // MS-RDPELE 2.2.1.4.2: chain padding is present but its value is ignored.
    reader.skip(8 + 4 * count)?;
    reader.finish()?;
    issuer_key.ok_or(Error::Invalid("licensing X509 chain"))
}

fn x509_key(certificate: &Certificate) -> Result<RsaPublicKey> {
    let info = &certificate.tbs_certificate.subject_public_key_info;
    // Old RDS licensing certificates use the OIW RSA signature OID in SPKI.
    match info.algorithm.oid.to_string().as_str() {
        "1.2.840.113549.1.1.1" | "1.3.14.3.2.15" => (),
        _ => return Err(Error::Unsupported("licensing X509 public key algorithm")),
    }
    let bytes = info
        .subject_public_key
        .as_bytes()
        .ok_or(Error::Invalid("licensing X509 public key bits"))?;
    let key = RsaPublicKey::from_pkcs1_der(bytes)
        .map_err(|_| Error::Invalid("licensing X509 RSA key"))?;
    validate_key(&key)?;
    Ok(key)
}

fn validate_key(key: &RsaPublicKey) -> Result<()> {
    if !(512..=4096).contains(&key.n().bits()) || key.e().bits() > 32 {
        return Err(Error::Limit("licensing RSA key size"));
    }
    Ok(())
}

fn verify_x509(certificate: &Certificate, key: &RsaPublicKey) -> Result<()> {
    if certificate.signature_algorithm != certificate.tbs_certificate.signature {
        return Err(Error::Invalid("licensing X509 signature algorithms"));
    }
    let bytes = certificate
        .tbs_certificate
        .to_der()
        .map_err(|_| Error::Invalid("licensing X509 signed data"))?;
    // DigestInfo contains a DER AlgorithmIdentifier followed by the hash. Using
    // an unprefixed RSA verifier bridges RustCrypto digest versions; RSA padding
    // and all hash implementations remain library operations.
    let digest_info = match certificate.signature_algorithm.oid.to_string().as_str() {
        "1.2.840.113549.1.1.4" => {
            let mut value =
                b"\x30\x20\x30\x0c\x06\x08\x2a\x86\x48\x86\xf7\x0d\x02\x05\x05\x00\x04\x10"
                    .to_vec();
            value.extend_from_slice(&Md5::digest(&bytes));
            value
        }
        "1.2.840.113549.1.1.5" | "1.3.14.3.2.29" => {
            let mut value =
                b"\x30\x21\x30\x09\x06\x05\x2b\x0e\x03\x02\x1a\x05\x00\x04\x14".to_vec();
            value.extend_from_slice(&Sha1::digest(&bytes));
            value
        }
        "1.2.840.113549.1.1.11" => {
            let mut value =
                b"\x30\x31\x30\x0d\x06\x09\x60\x86\x48\x01\x65\x03\x04\x02\x01\x05\x00\x04\x20"
                    .to_vec();
            value.extend_from_slice(&Sha256::digest(&bytes));
            value
        }
        "1.2.840.113549.1.1.12" => {
            let mut value =
                b"\x30\x41\x30\x0d\x06\x09\x60\x86\x48\x01\x65\x03\x04\x02\x02\x05\x00\x04\x30"
                    .to_vec();
            value.extend_from_slice(&Sha384::digest(&bytes));
            value
        }
        "1.2.840.113549.1.1.13" => {
            let mut value =
                b"\x30\x51\x30\x0d\x06\x09\x60\x86\x48\x01\x65\x03\x04\x02\x03\x05\x00\x04\x40"
                    .to_vec();
            value.extend_from_slice(&Sha512::digest(&bytes));
            value
        }
        _ => return Err(Error::Unsupported("licensing X509 signature algorithm")),
    };
    let signature = certificate
        .signature
        .as_bytes()
        .ok_or(Error::Invalid("licensing X509 signature bits"))?;
    key.verify(Pkcs1v15Sign::new_unprefixed(), &digest_info, signature)
        .map_err(|_| Error::Invalid("licensing X509 signature"))
}
