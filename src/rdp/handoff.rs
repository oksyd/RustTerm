//! GNOME remote-login handoffs. Tokens stay in memory and are never persisted.
use super::{RdpError, Result, tls};
use base64::{Engine, engine::general_purpose::STANDARD};
use zeroize::Zeroizing;

pub(super) struct Handoff {
    pub username: String,
    pub domain: Option<String>,
    pub routing: Zeroizing<Vec<u8>>,
    pub authentication: Zeroizing<Vec<u8>>,
    pub certificate_pin: String,
}

impl Handoff {
    pub fn parse(mut data: &[u8]) -> Result<Self> {
        take(&mut data, 2)?; // Enhanced-security padding.
        let length = data.len();
        if u16(&mut data)? != 0x400 || usize::from(u16(&mut data)?) != length {
            return Err(RdpError::Protocol("server redirection header"));
        }
        u32(&mut data)?; // Session ID; the routing token identifies the handoff.
        let flags = u32(&mut data)?;
        // Same-endpoint RDSTLS: load balance token, username, password token,
        // GUID and target certificate. Never forward the user's saved password
        // or silently follow a target address supplied by a server.
        const REQUIRED: u32 = 0x0001_c016;
        if flags & REQUIRED != REQUIRED || flags & !(REQUIRED | 8) != 0 {
            return Err(RdpError::Unsupported(
                "server redirection authentication or target",
            ));
        }
        let routing = Zeroizing::new(field(&mut data, 241)?.to_vec());
        rustrdp::connection::redirection_request(&routing)?;
        let username_bytes = field(&mut data, 512)?;
        let username = unicode(username_bytes)?;
        if username.is_empty() || username.chars().any(char::is_control) {
            return Err(RdpError::Protocol("redirection username"));
        }
        let domain_bytes = if flags & 8 != 0 {
            field(&mut data, 512)?
        } else {
            &[0, 0]
        };
        let domain = unicode(domain_bytes)?;
        if domain.chars().any(char::is_control) {
            return Err(RdpError::Protocol("redirection domain"));
        }
        let password = field(&mut data, 4096)?;
        let guid = field(&mut data, 512)?;
        if password.is_empty() || guid.is_empty() {
            return Err(RdpError::Protocol("empty handoff token"));
        }
        let encoded = field(&mut data, 32768)?;
        let encoded_certificate = unicode_body(encoded.strip_suffix(&[0, 0]).unwrap_or(encoded))?;
        let certificate_container = STANDARD
            .decode(encoded_certificate)
            .map_err(|_| RdpError::Protocol("redirection certificate encoding"))?;
        let certificate_pin = certificate_pin(&certificate_container)?;
        if data.len() != 8 {
            return Err(RdpError::Protocol("redirection padding"));
        }

        let mut authentication = Zeroizing::new(vec![1, 0, 2, 0, 1, 0]);
        for bytes in [guid, username_bytes, domain_bytes, password] {
            authentication.extend_from_slice(&(bytes.len() as u16).to_le_bytes());
            authentication.extend_from_slice(bytes);
        }
        Ok(Self {
            username,
            domain: (!domain.is_empty()).then_some(domain),
            routing,
            authentication,
            certificate_pin,
        })
    }
}

fn certificate_pin(mut data: &[u8]) -> Result<String> {
    let mut pin = None;
    while !data.is_empty() {
        let kind = u32(&mut data)?;
        let encoding = u32(&mut data)?;
        let certificate = field(&mut data, 16384)?;
        if kind == 32 && encoding == 1 {
            if pin.is_some() {
                return Err(RdpError::Protocol("duplicate redirection certificate"));
            }
            tls::public_key(certificate)?;
            pin = Some(tls::fingerprint(certificate));
        }
    }
    pin.ok_or(RdpError::Protocol("missing redirection certificate"))
}

fn take<'a>(data: &mut &'a [u8], length: usize) -> Result<&'a [u8]> {
    let (value, rest) = data
        .split_at_checked(length)
        .ok_or(RdpError::Protocol("truncated server redirection"))?;
    *data = rest;
    Ok(value)
}
fn u16(data: &mut &[u8]) -> Result<u16> {
    Ok(u16::from_le_bytes(take(data, 2)?.try_into().unwrap()))
}
fn u32(data: &mut &[u8]) -> Result<u32> {
    Ok(u32::from_le_bytes(take(data, 4)?.try_into().unwrap()))
}
fn field<'a>(data: &mut &'a [u8], limit: usize) -> Result<&'a [u8]> {
    let length = u32(data)? as usize;
    if length > limit {
        return Err(RdpError::Limit("server redirection field"));
    }
    take(data, length)
}
fn unicode(data: &[u8]) -> Result<String> {
    let body = data
        .strip_suffix(&[0, 0])
        .ok_or(RdpError::Protocol("unterminated redirection string"))?;
    unicode_body(body)
}
fn unicode_body(body: &[u8]) -> Result<String> {
    if !body.len().is_multiple_of(2) {
        return Err(RdpError::Protocol("redirection string length"));
    }
    let units: Vec<_> = body
        .as_chunks::<2>()
        .0
        .iter()
        .map(|v| u16::from_le_bytes([v[0], v[1]]))
        .collect();
    let text = String::from_utf16(&units).map_err(|_| RdpError::Protocol("redirection Unicode"))?;
    if text.contains('\0') {
        return Err(RdpError::Protocol("redirection string terminator"));
    }
    Ok(text)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn utf16(text: &str) -> Vec<u8> {
        text.encode_utf16().flat_map(u16::to_le_bytes).collect()
    }

    fn packet(domain: bool, terminated_certificate: bool) -> (Vec<u8>, Vec<u8>) {
        let pem = include_str!("../../tests/fixtures/elasticsearch/server.pem");
        let cert = STANDARD
            .decode(
                pem.lines()
                    .filter(|l| !l.starts_with("---"))
                    .collect::<String>(),
            )
            .unwrap();
        let mut container = vec![];
        for value in [32u32, 1, cert.len() as u32] {
            container.extend(value.to_le_bytes());
        }
        container.extend(&cert);
        let encoded = STANDARD.encode(container);
        let mut certificate = utf16(&encoded);
        if terminated_certificate {
            certificate.extend([0, 0]);
        }
        let mut packet = vec![0; 14];
        packet[2..4].copy_from_slice(&0x400u16.to_le_bytes());
        packet[10..14]
            .copy_from_slice(&(0x0001_c016u32 | if domain { 8 } else { 0 }).to_le_bytes());
        let mut fields = vec![b"12345".to_vec(), utf16("handoff-user\0")];
        if domain {
            fields.push(utf16("example\0"));
        }
        fields.extend([vec![0xff, 0, 1], utf16("guid\0"), certificate]);
        for field in fields {
            packet.extend((field.len() as u32).to_le_bytes());
            packet.extend(field);
        }
        packet.extend([0; 8]);
        let length = (packet.len() - 2) as u16;
        packet[4..6].copy_from_slice(&length.to_le_bytes());
        (packet, cert)
    }

    #[test]
    fn handoff_uses_the_server_token_and_delegated_certificate() {
        for domain in [false, true] {
            for terminated in [false, true] {
                let (packet, cert) = packet(domain, terminated);
                let handoff = Handoff::parse(&packet).unwrap();
                assert_eq!(handoff.username, "handoff-user");
                assert_eq!(handoff.domain.as_deref(), domain.then_some("example"));
                assert_eq!(&*handoff.routing, b"12345");
                assert_eq!(handoff.certificate_pin, tls::fingerprint(&cert));
                assert!(handoff.authentication.starts_with(&[1, 0, 2, 0, 1, 0]));
                assert!(handoff.authentication.ends_with(&[3, 0, 0xff, 0, 1]));
            }
        }
    }

    #[test]
    fn malformed_or_unsupported_handoffs_fail_without_exposing_tokens() {
        let (packet, _) = packet(false, false);
        for length in 0..packet.len() {
            assert!(Handoff::parse(&packet[..length]).is_err());
        }
        for flags in [0x0001_c017u32, 0x0001_8016, 0x0001_c096] {
            let mut changed = packet.clone();
            changed[10..14].copy_from_slice(&flags.to_le_bytes());
            assert!(matches!(
                Handoff::parse(&changed),
                Err(RdpError::Unsupported(_))
            ));
        }
        let mut oversized = packet;
        oversized[14..18].copy_from_slice(&u32::MAX.to_le_bytes());
        assert!(matches!(
            Handoff::parse(&oversized),
            Err(RdpError::Limit(_))
        ));
        assert!(certificate_pin(&[0; 12]).is_err());
    }
}
