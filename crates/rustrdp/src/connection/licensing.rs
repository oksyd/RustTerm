//! MS-RDPELE licensing over an already authenticated TLS/CredSSP transport.
//!
//! The caller supplies fresh CSPRNG output and a stable client identity. This
//! module never invents a hardware identifier, accesses a license store, opens
//! a socket or treats a licensing certificate as a TLS trust decision.
mod certificate;
mod crypto;
#[cfg(test)]
mod tests;

use crate::{ClientConfig, Error, Result, wire::Reader};
use crypto::Keys;
use zeroize::{Zeroize, Zeroizing};

const MAX_LICENSE_PDU: usize = u16::MAX as usize + 4;
const MAX_SCOPES: usize = 256;
const MAX_CHALLENGE: usize = 1024;
const MAX_RESENDS: u8 = 3;

/// Stable licensing identity, reused with every cached CAL for this client.
/// The caller chooses its own platform/ISV identifier; the hardware bytes must
/// be stable across reconnections and distinct across client installations.
#[derive(Clone, PartialEq, Eq)]
pub struct LicenseIdentity {
    /// Stable platform/ISV identifier sent in licensing PDUs and the hardware ID.
    pub platform_id: u32,
    /// Stable identifier for this client installation, shared with its CAL cache.
    pub hardware_id: [u8; 16],
}

impl LicenseIdentity {
    fn encode(&self) -> [u8; 20] {
        let mut bytes = [0; 20];
        bytes[..4].copy_from_slice(&self.platform_id.to_le_bytes());
        bytes[4..].copy_from_slice(&self.hardware_id);
        bytes
    }
}

/// Fresh independent values from the caller's cryptographic random generator.
/// Never reuse these between exchanges, including after a failed connection.
pub struct LicenseEntropy {
    client_random: [u8; 32],
    premaster: [u8; 48],
}

impl LicenseEntropy {
    /// Takes fresh CSPRNG output for exactly one licensing exchange.
    ///
    /// Both values must be independently generated, including on reconnect.
    /// The owned values are zeroized when dropped; callers must also clear any
    /// copies they retain. This constructor does not generate or test randomness.
    pub fn new(client_random: [u8; 32], premaster: [u8; 48]) -> Self {
        Self {
            client_random,
            premaster,
        }
    }
}

impl Drop for LicenseEntropy {
    fn drop(&mut self) {
        self.client_random.zeroize();
        self.premaster.zeroize();
    }
}

/// An opaque CAL and its lookup metadata. Save it with the same client identity
/// and authenticated server context; the library performs no persistence.
/// `scope` excludes the terminating NUL and preserves the server's ANSI bytes.
#[derive(Clone, PartialEq, Eq)]
pub struct IssuedLicense {
    /// Product version used to index the license, not the protocol PDU version.
    pub version: u32,
    /// Server-provided scope bytes with the terminating NUL removed.
    pub scope: Vec<u8>,
    /// License issuer's company name decoded from UTF-16.
    pub company_name: String,
    /// Product identifier decoded from UTF-16.
    pub product_id: String,
    /// Opaque CAL bytes. Persist securely; never log or interpret as text.
    pub data: Vec<u8>,
}

impl std::fmt::Debug for IssuedLicense {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("IssuedLicense")
            .field("version", &self.version)
            .field("data_length", &self.data.len())
            .finish_non_exhaustive()
    }
}

impl Drop for IssuedLicense {
    fn drop(&mut self) {
        self.data.zeroize();
        self.scope.zeroize();
        self.company_name.zeroize();
        self.product_id.zeroize();
    }
}

#[derive(Default)]
pub(crate) struct LicenseOutput {
    /// Security header and licensing PDU; wrap in MCS SendData on the I/O channel.
    /// May contain an opaque CAL; do not log the reply or send it outside TLS.
    pub reply: Option<Vec<u8>>,
    pub complete: bool,
    pub issued: Option<IssuedLicense>,
}

impl std::fmt::Debug for LicenseOutput {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LicenseOutput")
            .field("reply_length", &self.reply.as_ref().map(Vec::len))
            .field("complete", &self.complete)
            .field("issued", &self.issued)
            .finish()
    }
}

#[derive(Clone, PartialEq, Eq)]
struct Product {
    version: u32,
    company: String,
    product: String,
}

enum State {
    AwaitRequest,
    AwaitChallenge,
    AwaitLicense,
    Complete,
    Failed,
}

/// One bounded licensing exchange. A processing error permanently fails it.
/// Enable only after the transport peer and CredSSP public-key binding have been
/// verified. Licensing certificates are authenticated inside that TLS channel;
/// their signatures are checked separately and do not establish network trust.
pub(crate) struct LicenseClient {
    identity: LicenseIdentity,
    entropy: Option<LicenseEntropy>,
    cached: Option<IssuedLicense>,
    username: String,
    machine: String,
    limit: usize,
    state: State,
    version: u8,
    keys: Option<Keys>,
    product: Option<Product>,
    scopes: Vec<Vec<u8>>,
    last_reply: Option<Zeroizing<Vec<u8>>>,
    resends: u8,
}

impl LicenseClient {
    pub fn new(
        config: &ClientConfig,
        identity: LicenseIdentity,
        entropy: LicenseEntropy,
        cached: Option<IssuedLicense>,
    ) -> Result<Self> {
        config.validate()?;
        let limit = config.limits.max_pdu.min(MAX_LICENSE_PDU);
        if let Some(license) = &cached {
            license.validate(limit)?;
        }
        Ok(Self {
            identity,
            entropy: Some(entropy),
            cached,
            username: config.username.clone(),
            machine: config.client_name.clone(),
            limit,
            state: State::AwaitRequest,
            version: 3,
            keys: None,
            product: None,
            scopes: Vec::new(),
            last_reply: None,
            resends: 0,
        })
    }

    pub fn process(&mut self, packet: &[u8]) -> Result<LicenseOutput> {
        let result = self.process_packet(packet);
        if result.is_err() {
            self.state = State::Failed;
            self.clear_keys();
        }
        result
    }

    fn process_packet(&mut self, packet: &[u8]) -> Result<LicenseOutput> {
        if matches!(self.state, State::Complete | State::Failed) {
            return Err(Error::Invalid("closed licensing exchange"));
        }
        if packet.len() > self.limit {
            return Err(Error::Limit("licensing PDU"));
        }
        let mut reader = Reader::new(packet);
        let security = reader.u16_le()?;
        reader.skip(2)?;
        if security & 0x80 == 0 || security & 0x08 != 0 {
            return Err(Error::Invalid("licensing security header"));
        }
        let kind = reader.u8()?;
        let version = reader.u8()? & 15;
        if !matches!(version, 2 | 3) || usize::from(reader.u16_le()?) != packet.len() - 4 {
            return Err(Error::Invalid("licensing preamble"));
        }
        self.version = version;
        if kind == 0xff {
            return self.error_message(reader);
        }
        match (&self.state, kind) {
            (State::AwaitRequest, 1) => self.request(reader),
            (State::AwaitChallenge, 2) => self.challenge(reader),
            (State::AwaitLicense, 3 | 4) => self.issued(reader),
            _ => Err(Error::Invalid("licensing message sequence")),
        }
    }

    fn request(&mut self, mut reader: Reader<'_>) -> Result<LicenseOutput> {
        let server_random: [u8; 32] = reader.take(32)?.try_into().map_err(|_| Error::Truncated)?;
        let product = product(&mut reader)?;
        let algorithms = blob(&mut reader, 13)?;
        if !algorithms.len().is_multiple_of(4)
            || !algorithms
                .as_chunks::<4>()
                .0
                .iter()
                .any(|id| u32::from_le_bytes(*id) == 1)
        {
            return Err(Error::Unsupported("licensing key exchange algorithm"));
        }
        let certificate = blob(&mut reader, 3)?;
        // In NLA mode GCC has no legacy server certificate. An omitted licensing
        // key is unusable; the unrelated TLS certificate must not be substituted.
        if certificate.is_empty() {
            return Err(Error::Unsupported("omitted licensing certificate"));
        }
        let public_key = certificate::public_key(certificate)?;
        let count = reader.u32_le()? as usize;
        if count == 0 || count > MAX_SCOPES {
            return Err(Error::Limit("licensing scope count"));
        }
        let mut scopes = Vec::with_capacity(count);
        for _ in 0..count {
            scopes.push(ansi_z(blob(&mut reader, 14)?)?.to_vec());
        }
        reader.finish()?;

        let cached = self.cached.as_ref().filter(|license| {
            license.version == product.version
                && license.company_name == product.company
                && license.product_id == product.product
                && scopes.contains(&license.scope)
        });
        let entropy = self
            .entropy
            .take()
            .ok_or(Error::Invalid("missing licensing entropy"))?;
        let keys = Keys::derive(&entropy.premaster, &entropy.client_random, &server_random);
        let encrypted = crypto::encrypt_premaster(&public_key, &entropy.premaster)?;
        let mut body = Zeroizing::new(Vec::new());
        body.extend_from_slice(&1_u32.to_le_bytes());
        body.extend_from_slice(&self.identity.platform_id.to_le_bytes());
        body.extend_from_slice(&entropy.client_random);
        put_blob(&mut body, 2, &encrypted)?;
        let kind = if let Some(license) = cached {
            put_blob(&mut body, 1, &license.data)?;
            let hardware = Zeroizing::new(self.identity.encode());
            put_blob(&mut body, 9, &keys.crypt(hardware.as_slice()))?;
            body.extend_from_slice(&keys.mac(hardware.as_slice())?);
            0x12
        } else {
            put_blob(&mut body, 15, &encode_ansi(&self.username)?)?;
            put_blob(&mut body, 16, &encode_ansi(&self.machine)?)?;
            0x13
        };
        let reply = self.reply(kind, &body)?;
        self.product = Some(product);
        self.scopes = scopes;
        self.keys = Some(keys);
        self.cached = None;
        self.state = State::AwaitChallenge;
        Ok(LicenseOutput {
            reply: Some(reply),
            ..LicenseOutput::default()
        })
    }

    fn challenge(&mut self, mut reader: Reader<'_>) -> Result<LicenseOutput> {
        reader.skip(4)?; // Reserved ConnectFlags.
        reader.skip(2)?; // MS-RDPELE 2.2.2.4 note 12: this blob type is unused.
        let length = usize::from(reader.u16_le()?);
        let encrypted = reader.take(length)?;
        if encrypted.is_empty() || encrypted.len() > MAX_CHALLENGE {
            return Err(Error::Limit("licensing platform challenge"));
        }
        let signature = reader.take(16)?;
        reader.finish()?;
        let keys = self
            .keys
            .as_ref()
            .ok_or(Error::Invalid("missing licensing keys"))?;
        let challenge = keys.decrypt_checked(encrypted, signature)?;
        let mut response = Zeroizing::new(Vec::with_capacity(challenge.len() + 8));
        for value in [0x0100_u16, 0xff00, 3, challenge.len() as u16] {
            response.extend_from_slice(&value.to_le_bytes());
        }
        response.extend_from_slice(&challenge);
        let hardware = Zeroizing::new(self.identity.encode());
        let mut authenticated = Zeroizing::new(response.to_vec());
        authenticated.extend_from_slice(hardware.as_slice());
        let mut body = Vec::new();
        put_blob(&mut body, 9, &keys.crypt(&response))?;
        put_blob(&mut body, 9, &keys.crypt(hardware.as_slice()))?;
        body.extend_from_slice(&keys.mac(&authenticated)?);
        let reply = self.reply(0x15, &body)?;
        self.state = State::AwaitLicense;
        Ok(LicenseOutput {
            reply: Some(reply),
            ..LicenseOutput::default()
        })
    }

    fn issued(&mut self, mut reader: Reader<'_>) -> Result<LicenseOutput> {
        let encrypted = blob(&mut reader, 9)?;
        let signature = reader.take(16)?;
        reader.finish()?;
        let decoded = self
            .keys
            .as_ref()
            .ok_or(Error::Invalid("missing licensing keys"))?
            .decrypt_checked(encrypted, signature)?;
        let mut reader = Reader::new(&decoded);
        let version = reader.u32_le()?;
        let scope_length = reader.u32_le()? as usize;
        let scope = ansi_z(reader.take(scope_length)?)?.to_vec();
        let company_name = unicode_field(&mut reader)?;
        let product_id = unicode_field(&mut reader)?;
        let length = reader.u32_le()? as usize;
        let data = reader.take(length)?.to_vec();
        reader.finish()?;
        let license = IssuedLicense {
            version,
            scope,
            company_name,
            product_id,
            data,
        };
        license.validate(self.limit)?;
        let expected = self
            .product
            .as_ref()
            .ok_or(Error::Invalid("missing license product"))?;
        if license.version != expected.version
            || license.company_name != expected.company
            || license.product_id != expected.product
            || !self.scopes.contains(&license.scope)
        {
            return Err(Error::Invalid("issued license product or scope"));
        }
        self.state = State::Complete;
        self.clear_keys();
        Ok(LicenseOutput {
            complete: true,
            issued: Some(license),
            ..LicenseOutput::default()
        })
    }

    fn error_message(&mut self, mut reader: Reader<'_>) -> Result<LicenseOutput> {
        let code = reader.u32_le()?;
        let transition = reader.u32_le()?;
        let _extended_error = blob(&mut reader, 4)?;
        reader.finish()?;
        if code == 7 && transition == 2 {
            self.state = State::Complete;
            self.clear_keys();
            return Ok(LicenseOutput {
                complete: true,
                ..LicenseOutput::default()
            });
        }
        if transition == 4 {
            if self.resends == MAX_RESENDS {
                return Err(Error::Limit("licensing retransmissions"));
            }
            let reply = self
                .last_reply
                .as_ref()
                .ok_or(Error::Invalid("licensing retransmission before response"))?
                .to_vec();
            self.resends += 1;
            return Ok(LicenseOutput {
                reply: Some(reply),
                ..LicenseOutput::default()
            });
        }
        // A reset requires fresh entropy and a new exchange. The caller closes
        // this connection rather than reusing the previous premaster secret.
        if transition == 3 {
            return Err(Error::Unsupported("licensing phase reset"));
        }
        if transition != 1 {
            return Err(Error::Invalid("licensing error transition"));
        }
        Err(Error::Server(code))
    }

    fn reply(&mut self, kind: u8, body: &[u8]) -> Result<Vec<u8>> {
        let length = u16::try_from(body.len().saturating_add(4))
            .map_err(|_| Error::Limit("licensing response"))?;
        if usize::from(length) + 4 > self.limit {
            return Err(Error::Limit("licensing response"));
        }
        let mut reply = vec![0x80, 0, 0, 0, kind, self.version];
        reply.extend_from_slice(&length.to_le_bytes());
        reply.extend_from_slice(body);
        self.last_reply = Some(Zeroizing::new(reply.clone()));
        Ok(reply)
    }

    fn clear_keys(&mut self) {
        self.entropy = None;
        self.keys = None;
        self.cached = None;
        self.last_reply = None;
    }
}

impl IssuedLicense {
    fn validate(&self, limit: usize) -> Result<()> {
        let size = self
            .data
            .len()
            .saturating_add(self.scope.len())
            .saturating_add(self.company_name.len())
            .saturating_add(self.product_id.len());
        if self.data.is_empty() || size > limit.saturating_sub(128) {
            return Err(Error::Limit("cached license"));
        }
        if self.scope.contains(&0)
            || self.company_name.contains('\0')
            || self.product_id.contains('\0')
        {
            return Err(Error::Invalid("license metadata"));
        }
        Ok(())
    }
}

fn product(reader: &mut Reader<'_>) -> Result<Product> {
    Ok(Product {
        version: reader.u32_le()?,
        company: unicode_field(reader)?,
        product: unicode_field(reader)?,
    })
}

fn blob<'a>(reader: &mut Reader<'a>, kind: u16) -> Result<&'a [u8]> {
    if reader.u16_le()? != kind {
        return Err(Error::Invalid("licensing blob type"));
    }
    let length = usize::from(reader.u16_le()?);
    reader.take(length)
}

fn put_blob(output: &mut Vec<u8>, kind: u16, bytes: &[u8]) -> Result<()> {
    let length = u16::try_from(bytes.len()).map_err(|_| Error::Limit("licensing blob"))?;
    output.extend_from_slice(&kind.to_le_bytes());
    output.extend_from_slice(&length.to_le_bytes());
    output.extend_from_slice(bytes);
    Ok(())
}

fn unicode_field(reader: &mut Reader<'_>) -> Result<String> {
    let length = reader.u32_le()? as usize;
    let data = reader.take(length)?;
    if length < 2 || !length.is_multiple_of(2) || data[length - 2..] != [0, 0] {
        return Err(Error::Invalid("licensing Unicode string"));
    }
    let units: Vec<_> = data[..length - 2]
        .as_chunks::<2>()
        .0
        .iter()
        .map(|v| u16::from_le_bytes(*v))
        .collect();
    let text =
        String::from_utf16(&units).map_err(|_| Error::Invalid("licensing Unicode string"))?;
    if text.contains('\0') {
        return Err(Error::Invalid("licensing Unicode string"));
    }
    Ok(text)
}

fn ansi_z(bytes: &[u8]) -> Result<&[u8]> {
    let Some((&0, value)) = bytes.split_last() else {
        return Err(Error::Invalid("licensing ANSI string"));
    };
    if value.contains(&0) {
        return Err(Error::Invalid("licensing ANSI string"));
    }
    Ok(value)
}

fn encode_ansi(text: &str) -> Result<Vec<u8>> {
    let mut bytes = Vec::with_capacity(text.len() + 1);
    for character in text.chars() {
        let value = u8::try_from(u32::from(character))
            .map_err(|_| Error::Unsupported("licensing identity outside ISO-8859-1"))?;
        if value == 0 {
            return Err(Error::Invalid("licensing identity"));
        }
        bytes.push(value);
    }
    bytes.push(0);
    Ok(bytes)
}
