use super::*;
use rsa::traits::PublicKeyParts;

// Public, published wire examples from MS-RDPELE 18.0 (2024-04-23),
// sections 4.1 and 4.2. No live credentials or private keys are included.
const REQUEST: &[u8] = include_bytes!("fixtures/ms-rdpele-server-request.bin");
const NEW_REQUEST: &[u8] = include_bytes!("fixtures/ms-rdpele-new-request.bin");
// The spec's examples are from different exchanges. Encrypting its section 4.2
// premaster with section 4.1's leaf key using OpenSSL pkeyutl (RSA_NO_PADDING)
// gives this independent value, rather than section 4.2's original ciphertext.
const ENCRYPTED_PREMASTER: &[u8] = include_bytes!("fixtures/openssl-encrypted-premaster.bin");
const CLIENT_RANDOM: &str = "dc73a0c869256b18af0b947aa9a520af8bbc0dcca395b7b9eb815dbe0a109cd8";
const PREMASTER: &str = "cf7adbcbfb0e1523871c8481ba9d4e15bbd256bdd8f7f316cc353be1934278dd929ae47ae299d473b1aa6f55943bc9bc";

// Independent vectors: Python hashlib's MD5/SHA1 and OpenSSL 3 `enc -rc4`
// using the published randoms/premaster above. No production crypto helper
// constructs these expected PDUs. The server challenge deliberately uses an
// undefined blob type, as allowed by MS-RDPELE 2.2.2.4 note 12.
const CHALLENGE: &str =
    "8000000002032600ffffffff50f70a00bf9720b954b30c27dc7be452a73830b396466bda2a6dcada4558";
const RESPONSE: &str = "800000001503420009001200eb96654604b35227887b84ebeaebc14be6ff09001400eb9764bd5de9027d86219bb1e3b1cf11bca51e52c66f955098edecd1bd3743d0b4573ae9";
const ISSUED: &str = "8000000003037d0009006500eb9763b909b35827b112a299d698fa2d92d127678bf83f02a31bfd4c5c6bbb0dd233d2a4ed3685dc9825cf31a4f2958d09fc56b0d3a5ed36d095bed2672d1940f408127a11b8991aa61b0c98e768d6a749a55f2e0b0511da9e25b12271d8176db5a689c13280a3fb02b2174db5ba53f70a864fdca8";

fn hex(value: &str) -> Vec<u8> {
    let digits: String = value.chars().filter(|v| !v.is_ascii_whitespace()).collect();
    digits
        .as_bytes()
        .as_chunks::<2>()
        .0
        .iter()
        .map(|pair| u8::from_str_radix(std::str::from_utf8(pair).unwrap(), 16).unwrap())
        .collect()
}

fn config() -> ClientConfig {
    ClientConfig {
        username: "Administrator".into(),
        client_name: "RODENT".into(),
        ..ClientConfig::default()
    }
}

fn identity() -> LicenseIdentity {
    LicenseIdentity {
        platform_id: 0x0401_0000,
        hardware_id: [0x5a; 16],
    }
}

fn entropy() -> LicenseEntropy {
    LicenseEntropy::new(
        hex(CLIENT_RANDOM).try_into().unwrap(),
        hex(PREMASTER).try_into().unwrap(),
    )
}

fn client(cached: Option<IssuedLicense>) -> LicenseClient {
    LicenseClient::new(&config(), identity(), entropy(), cached).unwrap()
}

fn request() -> Vec<u8> {
    [b"\x80\0\0\0".as_slice(), REQUEST].concat()
}

fn cached() -> IssuedLicense {
    IssuedLicense {
        version: 0x0006_0000,
        scope: b"microsoft.com".to_vec(),
        company_name: "Microsoft Corporation".into(),
        product_id: "A02".into(),
        data: b"opaque test CAL".to_vec(),
    }
}

fn error(code: u32, transition: u32) -> Vec<u8> {
    let mut value = vec![0x80, 0, 0, 0, 0xff, 3, 16, 0];
    value.extend_from_slice(&code.to_le_bytes());
    value.extend_from_slice(&transition.to_le_bytes());
    value.extend_from_slice(&[4, 0, 0, 0]);
    value
}

#[test]
fn microsoft_request_fields_and_openssl_rsa_encryption() {
    let output = client(None).process(&request()).unwrap();
    let mut expected = [b"\x80\0\0\0".as_slice(), NEW_REQUEST].concat();
    expected[5] = 3; // We do not advertise extended licensing errors.
    expected[0x34..0x13c].copy_from_slice(ENCRYPTED_PREMASTER);
    assert_eq!(output.reply.unwrap(), expected);
    assert!(!output.complete);
}

#[test]
fn full_new_license_exchange_matches_independent_vectors() {
    let mut client = client(None);
    client.process(&request()).unwrap();
    let output = client.process(&hex(CHALLENGE)).unwrap();
    assert_eq!(output.reply.unwrap(), hex(RESPONSE));
    assert!(!output.complete);
    let output = client.process(&hex(ISSUED)).unwrap();
    assert!(output.complete);
    assert!(output.reply.is_none());
    assert_eq!(output.issued.unwrap(), cached());
    assert!(client.process(&error(7, 2)).is_err());
}

#[test]
fn cached_license_is_sent_with_authenticated_hardware_then_accepted() {
    let mut client = client(Some(cached()));
    let output = client.process(&request()).unwrap();
    let reply = output.reply.unwrap();
    assert_eq!(reply[4], 0x12);
    assert_eq!(&reply[8..0x34], &NEW_REQUEST[4..0x30]);
    assert_eq!(&reply[0x34..316], ENCRYPTED_PREMASTER);
    let mut reader = Reader::new(&reply[316..]);
    assert_eq!(blob(&mut reader, 1).unwrap(), b"opaque test CAL");
    assert_eq!(
        blob(&mut reader, 9).unwrap(),
        hex("eb9764bd5de9027d86219bb1e3b1cf11bca51e52")
    );
    assert_eq!(
        reader.take(16).unwrap(),
        hex("4e486e0e0e8eb2b5a7e2b292bb1b890c")
    );
    reader.finish().unwrap();
    let accepted = client.process(&error(7, 2)).unwrap();
    assert!(accepted.complete);
    assert!(accepted.issued.is_none());
}

#[test]
fn cached_license_can_be_upgraded_after_challenge() {
    let mut client = client(Some(cached()));
    client.process(&request()).unwrap();
    assert_eq!(
        client.process(&hex(CHALLENGE)).unwrap().reply.unwrap(),
        hex(RESPONSE)
    );
    let mut upgraded = hex(ISSUED);
    upgraded[4] = 4;
    let output = client.process(&upgraded).unwrap();
    assert!(output.complete);
    assert_eq!(output.issued.unwrap(), cached());
}

#[test]
fn cached_license_must_match_product_and_scope() {
    for mutate in [0, 1, 2, 3] {
        let mut license = cached();
        match mutate {
            0 => license.version += 1,
            1 => license.product_id.push('x'),
            2 => license.company_name.push('x'),
            _ => license.scope.push(b'x'),
        }
        assert_eq!(
            client(Some(license))
                .process(&request())
                .unwrap()
                .reply
                .unwrap()[4],
            0x13
        );
    }
}

#[test]
fn tampered_macs_fail_permanently_without_issuing_cal() {
    let mut client = client(None);
    client.process(&request()).unwrap();
    let mut challenge = hex(CHALLENGE);
    *challenge.last_mut().unwrap() ^= 1;
    assert_eq!(
        client.process(&challenge).unwrap_err(),
        Error::Invalid("licensing message authentication")
    );
    assert!(client.process(&hex(CHALLENGE)).is_err());

    let mut client = self::client(None);
    client.process(&request()).unwrap();
    client.process(&hex(CHALLENGE)).unwrap();
    let mut issued = hex(ISSUED);
    *issued.last_mut().unwrap() ^= 1;
    assert_eq!(
        client.process(&issued).unwrap_err(),
        Error::Invalid("licensing message authentication")
    );
    assert!(client.process(&hex(ISSUED)).is_err());
}

#[test]
fn invalid_lengths_sequence_and_unbounded_retries_are_rejected() {
    let mut invalid = request();
    invalid[6] ^= 1;
    assert!(client(None).process(&invalid).is_err());
    assert!(client(None).process(&hex(CHALLENGE)).is_err());
    assert!(client(None).process(&hex(ISSUED)).is_err());
    assert!(client(None).process(&error(0, 4)).is_err());
    let mut client = client(None);
    let original = client.process(&request()).unwrap().reply.unwrap();
    for _ in 0..MAX_RESENDS {
        assert_eq!(
            client.process(&error(0, 4)).unwrap().reply.unwrap(),
            original
        );
    }
    assert_eq!(
        client.process(&error(0, 4)).unwrap_err(),
        Error::Limit("licensing retransmissions")
    );
    assert!(client.process(&error(7, 2)).is_err());
}

#[test]
fn non_ansi_usernames_are_never_silently_corrupted() {
    let mut config = config();
    config.username = "user\u{100}".into();
    let mut client = LicenseClient::new(&config, identity(), entropy(), None).unwrap();
    assert_eq!(
        client.process(&request()).unwrap_err(),
        Error::Unsupported("licensing identity outside ISO-8859-1")
    );
    // Reusing an existing CAL needs no ANSI username field.
    let mut client = LicenseClient::new(&config, identity(), entropy(), Some(cached())).unwrap();
    assert_eq!(client.process(&request()).unwrap().reply.unwrap()[4], 0x12);
}

#[test]
fn debug_never_exposes_cal_or_wire_contents() {
    assert!(!format!("{:?}", cached()).contains("opaque"));
    let output = client(Some(cached())).process(&request()).unwrap();
    let debug = format!("{output:?}");
    assert!(!debug.contains("opaque"));
    assert!(debug.contains("reply_length"));
}

#[test]
fn published_proprietary_certificate_and_corruption() {
    // MS-RDPBCGR 62.0 section 4.1.4, public 512-bit server certificate.
    let bytes = hex("01000000010000000100000006005c00
        5253413148000000000200003f00000001000100cb81feba6d61c35505d55f2e
        87f87194d6f1a5cbf15f0c3df8700296c4fb9bc83c2d55aee8ff3275ea6879e5
        a201fd31a0b11f55a61fc1f6d1838863265612bc000000000000000008004800
        e9e1d628468b4ef50adffdee2199acb4e18f5f815782ef9d965263271829dbb3
        4afd9ada42adb56921890e1dc04c1aa8aa713e0f54b99ae499683f6cd6768461
        0000000000000000");
    assert_eq!(certificate::public_key(&bytes).unwrap().n().bits(), 512);
    for index in [20, 28, 40, 108, 120, 183] {
        let mut corrupted = bytes.clone();
        corrupted[index] ^= 1;
        assert!(certificate::public_key(&corrupted).is_err());
    }
}

#[test]
fn x509_chain_signatures_lengths_and_order_are_checked() {
    let certificate = &REQUEST[0x70..0x882];
    assert_eq!(certificate::public_key(certificate).unwrap().size(), 256);
    let mut corrupted = certificate.to_vec();
    corrupted[0x300] ^= 1; // Root signature, not a DER framing byte.
    assert!(certificate::public_key(&corrupted).is_err());
    let mut corrupted = certificate.to_vec();
    corrupted[4..8].copy_from_slice(&17_u32.to_le_bytes());
    assert_eq!(
        certificate::public_key(&corrupted).unwrap_err(),
        Error::Limit("licensing X509 chain")
    );
    assert!(certificate::public_key(&certificate[..certificate.len() - 1]).is_err());
    let mut with_trailing_data = certificate.to_vec();
    with_trailing_data.push(0);
    assert!(certificate::public_key(&with_trailing_data).is_err());
}
