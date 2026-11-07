use super::{ElasticsearchError, Result, runtime};
use crate::http_transport::{HttpConnector, RemoteResolver};
use crate::{config::ElasticsearchConfig, session::TunnelConnector};
use std::time::Duration;
use tokio_util::sync::CancellationToken;
use ureq::unversioned::transport::{Connector, RustlsConnector};

pub(super) fn agent(
    config: &ElasticsearchConfig,
    tunnel: Option<TunnelConnector>,
    stop: CancellationToken,
) -> Result<ureq::Agent> {
    let mut certs: Vec<_> = rustls_native_certs::load_native_certs()
        .certs
        .into_iter()
        .map(|cert| ureq::tls::Certificate::from_der(cert.as_ref()).to_owned())
        .collect();
    if let Some(path) = &config.ca_file {
        use std::io::Read;
        let mut pem = Vec::new();
        std::fs::File::open(path)
            .and_then(|file| file.take(1024 * 1024 + 1).read_to_end(&mut pem))
            .map_err(|_| ElasticsearchError::Certificate)?;
        if pem.len() > 1024 * 1024 {
            return Err(ElasticsearchError::Certificate);
        }
        let mut count = 0;
        for item in ureq::tls::parse_pem(&pem) {
            match item.map_err(|_| ElasticsearchError::Certificate)? {
                ureq::tls::PemItem::Certificate(cert) => {
                    certs.push(cert);
                    count += 1;
                }
                _ => return Err(ElasticsearchError::Certificate),
            }
        }
        if count == 0 {
            return Err(ElasticsearchError::Certificate);
        }
    }
    let roots = if certs.is_empty() {
        ureq::tls::RootCerts::WebPki
    } else {
        certs.into()
    };
    let config = ureq::Agent::config_builder()
        .proxy(None)
        .max_redirects(0)
        .http_status_as_error(false)
        .timeout_global(Some(Duration::from_secs(config.query_timeout_secs)))
        .timeout_connect(Some(Duration::from_secs(config.timeout_secs)))
        .tls_config(ureq::tls::TlsConfig::builder().root_certs(roots).build())
        .build();
    Ok(ureq::Agent::with_parts(
        config,
        HttpConnector {
            tunnel,
            stop,
            handle: runtime()?.handle().clone(),
        }
        .chain(RustlsConnector::default()),
        RemoteResolver,
    ))
}

pub(super) fn error(error: ureq::Error, stop: &CancellationToken) -> ElasticsearchError {
    if stop.is_cancelled() {
        ElasticsearchError::Cancelled
    } else if matches!(error, ureq::Error::Timeout(_)) {
        ElasticsearchError::Timeout
    } else if matches!(error, ureq::Error::BodyExceedsLimit(_)) {
        ElasticsearchError::TooLarge
    } else {
        ElasticsearchError::Network
    }
}
