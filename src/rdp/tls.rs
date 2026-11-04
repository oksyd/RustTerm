//! The trust callback rejects first, then the UI can authorize a fresh pinned handshake.
//! Credentials are constructed only after this gate, and TLS signatures are always checked.
use super::{RdpError, Result};
use rustls::{
    DigitallySignedStruct, SignatureScheme,
    client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier},
    pki_types::{CertificateDer, ServerName, UnixTime},
};
use sha2::{Digest, Sha256};
use std::sync::{Arc, Mutex};

pub(super) type Challenge = Arc<Mutex<Option<String>>>;

#[derive(Debug)]
struct Verifier {
    roots: Option<Arc<rustls::client::WebPkiServerVerifier>>,
    provider: Arc<rustls::crypto::CryptoProvider>,
    pin: Option<String>,
    challenge: Challenge,
}
impl ServerCertVerifier for Verifier {
    fn verify_server_cert(
        &self,
        end: &CertificateDer<'_>,
        intermediates: &[CertificateDer<'_>],
        name: &ServerName<'_>,
        ocsp: &[u8],
        now: UnixTime,
    ) -> std::result::Result<ServerCertVerified, rustls::Error> {
        let fingerprint = fingerprint(end.as_ref());
        let result = match &self.pin {
            Some(pin) if pin.eq_ignore_ascii_case(&fingerprint) => {
                Ok(ServerCertVerified::assertion())
            }
            Some(_) => Err(rustls::Error::InvalidCertificate(
                rustls::CertificateError::ApplicationVerificationFailure,
            )),
            None => match &self.roots {
                Some(roots) => roots.verify_server_cert(end, intermediates, name, ocsp, now),
                None => Err(rustls::Error::InvalidCertificate(
                    rustls::CertificateError::UnknownIssuer,
                )),
            },
        };
        if result.is_err() {
            *self.challenge.lock().unwrap_or_else(|e| e.into_inner()) = Some(fingerprint);
        }
        result
    }
    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> std::result::Result<HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls12_signature(
            message,
            cert,
            dss,
            &self.provider.signature_verification_algorithms,
        )
    }
    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> std::result::Result<HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls13_signature(
            message,
            cert,
            dss,
            &self.provider.signature_verification_algorithms,
        )
    }
    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        self.provider
            .signature_verification_algorithms
            .supported_schemes()
    }
}

pub(super) fn config(
    pin: Option<String>,
    challenge: Challenge,
) -> Result<Arc<rustls::ClientConfig>> {
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let mut roots = rustls::RootCertStore::empty();
    for cert in rustls_native_certs::load_native_certs().certs {
        let _ = roots.add(cert);
    }
    let roots = rustls::client::WebPkiServerVerifier::builder_with_provider(
        Arc::new(roots),
        provider.clone(),
    )
    .build()
    .ok();
    let verifier = Verifier {
        roots,
        provider: provider.clone(),
        pin,
        challenge,
    };
    let mut config = rustls::ClientConfig::builder_with_provider(provider)
        .with_safe_default_protocol_versions()
        .map_err(|_| RdpError::Tls)?
        .dangerous()
        .with_custom_certificate_verifier(Arc::new(verifier))
        .with_no_client_auth();
    // CredSSP does not support TLS resumption. Never install a TLS key logger.
    config.resumption = rustls::client::Resumption::disabled();
    Ok(Arc::new(config))
}

pub(super) fn fingerprint(der: &[u8]) -> String {
    let hash = Sha256::digest(der);
    hash.iter().map(|byte| format!("{byte:02x}")).collect()
}

pub(super) fn public_key(der: &[u8]) -> Result<Vec<u8>> {
    use x509_cert::der::Decode;
    let cert = x509_cert::Certificate::from_der(der).map_err(|_| RdpError::Tls)?;
    cert.tbs_certificate
        .subject_public_key_info
        .subject_public_key
        .as_bytes()
        .map(<[u8]>::to_vec)
        .ok_or(RdpError::Tls)
}

#[cfg(test)]
mod tests {
    use super::*;
    fn verifier(pin: Option<String>) -> Verifier {
        Verifier {
            roots: None,
            provider: Arc::new(rustls::crypto::ring::default_provider()),
            pin,
            challenge: Arc::new(Mutex::new(None)),
        }
    }
    #[test]
    fn unknown_certificate_is_rejected_and_challenged() {
        let verifier = verifier(None);
        let certificate = CertificateDer::from(b"unknown leaf".to_vec());
        assert!(
            verifier
                .verify_server_cert(
                    &certificate,
                    &[],
                    &ServerName::try_from("example.test").unwrap(),
                    &[],
                    UnixTime::now()
                )
                .is_err()
        );
        assert_eq!(
            verifier.challenge.lock().unwrap().as_deref(),
            Some(fingerprint(certificate.as_ref()).as_str())
        );
    }
    #[test]
    fn changed_pin_is_rejected_even_before_chain_fallback() {
        let verifier = verifier(Some(fingerprint(b"old leaf")));
        let certificate = CertificateDer::from(b"new leaf".to_vec());
        assert!(
            verifier
                .verify_server_cert(
                    &certificate,
                    &[],
                    &ServerName::try_from("example.test").unwrap(),
                    &[],
                    UnixTime::now()
                )
                .is_err()
        );
        assert_eq!(
            verifier.challenge.lock().unwrap().as_deref(),
            Some(fingerprint(b"new leaf").as_str())
        );
    }
}
