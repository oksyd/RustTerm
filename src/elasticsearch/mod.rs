//! Bounded Elasticsearch requests on a worker runtime. Credentials and HTTP
//! responses stay out of diagnostic errors; closing a client cancels network I/O.
#[cfg(test)]
mod tests;
#[cfg(test)]
mod tls_tests;
mod transport;

use crate::{
    config::{ElasticsearchAuth, ElasticsearchConfig, Secret},
    i18n::messages as msg,
    session::{HostKeyChallenge, SshOptions, TunnelConnector},
};
use base64::Engine;
use serde::Deserialize;
use serde_json::Value;
use std::{
    sync::{Arc, OnceLock},
    time::{Duration, Instant},
};
use tokio::{
    runtime::Runtime,
    sync::{Semaphore, mpsc},
};
use tokio_util::sync::CancellationToken;

pub const MAX_RESPONSE_BYTES: usize = 4 * 1024 * 1024;
pub const MAX_QUERY_BYTES: usize = 64 * 1024;
pub const MAX_INDICES: usize = 2000;
pub const DEFAULT_QUERY: &str =
    "{\n  \"size\": 100,\n  \"query\": {\n    \"match_all\": {}\n  }\n}";

#[derive(Clone, Copy, Debug, thiserror::Error)]
pub enum ElasticsearchError {
    #[error("{}", crate::i18n::tr(msg::ES_INVALID_CONFIG))]
    InvalidConfig,
    #[error("{}", crate::i18n::tr(msg::ES_NETWORK))]
    Network,
    #[error("{}", crate::i18n::tr(msg::ES_UNAUTHORIZED))]
    Unauthorized,
    #[error("{}", crate::i18n::tr(msg::ES_PROTOCOL))]
    Protocol,
    #[error("{}", crate::trf!(msg::ES_HTTP_STATUS, status = .0))]
    HttpStatus(u16),
    #[error("{}", crate::i18n::tr(msg::ES_INVALID_CA))]
    Certificate,
    #[error("{}", crate::i18n::tr(msg::ES_TIMEOUT))]
    Timeout,
    #[error("{}", crate::i18n::tr(msg::ES_CANCELLED))]
    Cancelled,
    #[error("{}", crate::i18n::tr(msg::ES_TOO_LARGE))]
    TooLarge,
    #[error("{}", crate::i18n::tr(msg::ES_INVALID_QUERY))]
    InvalidQuery,
}
pub type Result<T> = std::result::Result<T, ElasticsearchError>;

#[derive(Clone, Debug, Deserialize)]
pub struct Index {
    #[serde(rename = "name")]
    pub index: String,
    #[serde(default)]
    pub attributes: Vec<String>,
}
pub struct IndexList {
    pub indices: Vec<Index>,
    pub truncated: bool,
}
pub struct Response {
    pub status: u16,
    pub elapsed: Duration,
    pub json: Value,
}
#[derive(Clone)]
pub enum Request {
    Info,
    Indices,
    IndexSettings,
    Mapping(Option<String>),
    Search { index: Option<String>, body: String },
}
#[derive(Clone)]
pub struct ElasticsearchClient {
    inner: Arc<Inner>,
    indices_api: IndicesApi,
    server_version: Arc<str>,
}
#[derive(Clone, Copy)]
enum IndicesApi {
    Resolve,
    Settings,
}
impl IndicesApi {
    fn for_version(version: &str) -> Self {
        // Resolve Index was added in 7.9. Older or unrecognized versions use
        // the structured settings API, without requiring cluster-monitor access.
        if semver::Version::parse(version)
            .is_ok_and(|version| (version.major, version.minor) >= (7, 9))
        {
            Self::Resolve
        } else {
            Self::Settings
        }
    }
}
struct Inner {
    config: ElasticsearchConfig,
    authorization: Secret,
    tunnel: Option<TunnelConnector>,
    stop: CancellationToken,
    busy: Arc<Semaphore>,
}
impl Drop for Inner {
    fn drop(&mut self) {
        self.stop.cancel();
    }
}
struct CancelOnDrop(Option<CancellationToken>);
impl Drop for CancelOnDrop {
    fn drop(&mut self) {
        if let Some(stop) = &self.0 {
            stop.cancel();
        }
    }
}

pub fn validate_query(body: &str) -> Result<()> {
    if body.len() > MAX_QUERY_BYTES
        || !serde_json::from_str::<Value>(body).is_ok_and(|value| value.is_object())
    {
        return Err(ElasticsearchError::InvalidQuery);
    }
    Ok(())
}

impl ElasticsearchClient {
    pub async fn connect(
        config: ElasticsearchConfig,
        secret: Secret,
        ssh: Option<(SshOptions, mpsc::Sender<HostKeyChallenge>)>,
    ) -> Result<Self> {
        config
            .validate()
            .map_err(|_| ElasticsearchError::InvalidConfig)?;
        if config.ssh_profile.is_some() != ssh.is_some() {
            return Err(ElasticsearchError::InvalidConfig);
        }
        let stop = CancellationToken::new();
        let mut guard = CancelOnDrop(Some(stop.clone()));
        let result = runtime()?
            .spawn(async move {
                let tunnel = if let Some((options, trust)) = ssh {
                    Some(
                        crate::session::connect_tunnel(options, trust, stop.child_token())
                            .await
                            .map_err(|_| ElasticsearchError::Network)?,
                    )
                } else {
                    None
                };
                let authorization = authorization(&config, &secret)?;
                let mut client = Self {
                    inner: Arc::new(Inner {
                        config,
                        authorization,
                        tunnel,
                        stop,
                        busy: Arc::new(Semaphore::new(1)),
                    }),
                    indices_api: IndicesApi::Settings,
                    server_version: Arc::from(""),
                };
                let response = client.request(Request::Info).await?;
                if matches!(response.status, 401 | 403) {
                    return Err(ElasticsearchError::Unauthorized);
                }
                if response.status != 200 {
                    return Err(ElasticsearchError::HttpStatus(response.status));
                }
                let Some(version) = response.json["version"]["number"].as_str() else {
                    return Err(ElasticsearchError::Protocol);
                };
                if response.json["cluster_name"].as_str().is_none() {
                    return Err(ElasticsearchError::Protocol);
                }
                client.indices_api = IndicesApi::for_version(version);
                client.server_version = Arc::from(version);
                Ok(client)
            })
            .await
            .map_err(|_| ElasticsearchError::Network)?;
        // The guard must remain armed on cancellation, but not after a successful handshake.
        if result.is_ok() {
            guard.0.take();
        }
        result
    }

    pub fn close(&self) {
        self.inner.stop.cancel();
    }

    pub fn server_version(&self) -> &str {
        &self.server_version
    }

    pub async fn request(&self, request: Request) -> Result<Response> {
        if let Request::Search { body, .. } = &request {
            validate_query(body)?;
        }
        if self.inner.stop.is_cancelled() {
            return Err(ElasticsearchError::Cancelled);
        }
        // A cancelled worker may still be releasing its socket. Wait for that
        // cleanup so an immediate retry does not spuriously fail as cancelled.
        let permit = tokio::select! {
            biased;
            _ = self.inner.stop.cancelled() => return Err(ElasticsearchError::Cancelled),
            permit = self.inner.busy.clone().acquire_owned() =>
                permit.map_err(|_| ElasticsearchError::Cancelled)?,
        };
        let cancel = self.inner.stop.child_token();
        let _guard = CancelOnDrop(Some(cancel.clone()));
        let inner = self.inner.clone();
        runtime()?
            .spawn_blocking(move || {
                let _permit = permit;
                execute(&inner, request, cancel)
            })
            .await
            .map_err(|_| ElasticsearchError::Network)?
    }

    pub async fn indices(&self) -> Result<IndexList> {
        let request = match self.indices_api {
            IndicesApi::Resolve => Request::Indices,
            IndicesApi::Settings => Request::IndexSettings,
        };
        let response = self.request(request).await?;
        if matches!(response.status, 401 | 403) {
            return Err(ElasticsearchError::Unauthorized);
        }
        if response.status != 200 {
            return Err(ElasticsearchError::HttpStatus(response.status));
        }
        let mut json = response.json;
        let object = json.as_object_mut().ok_or(ElasticsearchError::Protocol)?;
        let mut indices: Vec<Index> = match self.indices_api {
            IndicesApi::Resolve => {
                // filter_path omits empty arrays, so an empty cluster can return {}.
                let value = object.remove("indices").unwrap_or(Value::Array(Vec::new()));
                serde_json::from_value(value).map_err(|_| ElasticsearchError::Protocol)?
            }
            IndicesApi::Settings => {
                let mut indices = Vec::with_capacity(object.len());
                for (name, settings) in object {
                    if settings["settings"]["index"]["uuid"].as_str().is_none() {
                        return Err(ElasticsearchError::Protocol);
                    }
                    indices.push(Index {
                        index: name.clone(),
                        attributes: Vec::new(),
                    });
                }
                indices
            }
        };
        indices.sort_unstable_by(|a, b| a.index.cmp(&b.index));
        let truncated = indices.len() > MAX_INDICES;
        indices.truncate(MAX_INDICES);
        Ok(IndexList { indices, truncated })
    }
}

fn authorization(config: &ElasticsearchConfig, secret: &Secret) -> Result<Secret> {
    let value = match config.auth {
        ElasticsearchAuth::None => String::new(),
        ElasticsearchAuth::Basic => {
            let pair = Secret::new(format!("{}:{}", config.username, secret.expose()));
            format!(
                "Basic {}",
                base64::engine::general_purpose::STANDARD.encode(pair.expose())
            )
        }
        ElasticsearchAuth::ApiKey => {
            if secret.expose().is_empty()
                || !secret
                    .expose()
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b"+/=_-".contains(&b))
            {
                return Err(ElasticsearchError::Unauthorized);
            }
            format!("ApiKey {}", secret.expose())
        }
    };
    Ok(Secret::new(value))
}

fn request_url(config: &ElasticsearchConfig, request: &Request) -> Result<url::Url> {
    let mut url = config
        .url()
        .map_err(|_| ElasticsearchError::InvalidConfig)?;
    if matches!(request, Request::Info) {
        return Ok(url);
    }
    {
        let mut path = url
            .path_segments_mut()
            .map_err(|_| ElasticsearchError::InvalidConfig)?;
        path.pop_if_empty();
        match request {
            Request::Info => {}
            Request::Indices => {
                path.push("_resolve").push("index").push("*");
            }
            Request::IndexSettings => {
                path.push("_settings");
            }
            Request::Mapping(index) | Request::Search { index, .. } => {
                if let Some(index) = index {
                    if index.is_empty()
                        || index == "."
                        || index == ".."
                        || index
                            .chars()
                            .any(|c| c.is_control() || "/\\?#%".contains(c))
                    {
                        return Err(ElasticsearchError::InvalidConfig);
                    }
                    path.push(index);
                }
                path.push(if matches!(request, Request::Mapping(_)) {
                    "_mapping"
                } else {
                    "_search"
                });
            }
        }
    }
    if matches!(request, Request::Indices) {
        url.query_pairs_mut()
            .append_pair("expand_wildcards", "all")
            .append_pair("filter_path", "indices.name,indices.attributes");
    } else if matches!(request, Request::IndexSettings) {
        url.query_pairs_mut()
            .append_pair("expand_wildcards", "all")
            .append_pair("filter_path", "*.settings.index.uuid");
    }
    Ok(url)
}

pub fn format_json(value: &Value) -> Result<String> {
    struct Bounded(Vec<u8>);
    impl std::io::Write for Bounded {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            if bytes.len() > MAX_RESPONSE_BYTES.saturating_sub(self.0.len()) {
                return Err(std::io::Error::other("JSON display limit"));
            }
            self.0.extend_from_slice(bytes);
            Ok(bytes.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }
    let mut buffer = Bounded(Vec::new());
    serde_json::to_writer_pretty(&mut buffer, value).map_err(|_| ElasticsearchError::TooLarge)?;
    String::from_utf8(buffer.0).map_err(|_| ElasticsearchError::Protocol)
}

fn execute(inner: &Inner, request: Request, cancel: CancellationToken) -> Result<Response> {
    let url = request_url(&inner.config, &request)?;
    let started = Instant::now();
    let agent = transport::agent(&inner.config, inner.tunnel.clone(), cancel.clone())?;
    let (method, body) = match &request {
        Request::Search { body, .. } => ("POST", body.as_bytes()),
        _ => ("GET", &[][..]),
    };
    let mut builder = ureq::http::Request::builder()
        .method(method)
        .uri(url.as_str())
        .header("Accept", "application/json")
        .header("Content-Type", "application/json");
    if !inner.authorization.expose().is_empty() {
        let mut header = ureq::http::HeaderValue::from_str(inner.authorization.expose())
            .map_err(|_| ElasticsearchError::Unauthorized)?;
        header.set_sensitive(true);
        builder = builder.header("Authorization", header);
    }
    let outgoing = builder
        .body(body)
        .map_err(|_| ElasticsearchError::InvalidConfig)?;
    let mut response = agent
        .run(outgoing)
        .map_err(|error| transport::error(error, &cancel))?;
    let status = response.status().as_u16();
    if matches!(status, 401 | 403) {
        return Err(ElasticsearchError::Unauthorized);
    }
    let bytes = response
        .body_mut()
        .with_config()
        .limit(MAX_RESPONSE_BYTES as u64)
        .read_to_vec()
        .map_err(|error| transport::error(error, &cancel))?;
    let json = serde_json::from_slice(&bytes).map_err(|_| ElasticsearchError::Protocol)?;
    Ok(Response {
        status,
        elapsed: started.elapsed(),
        json,
    })
}

fn runtime() -> Result<&'static Runtime> {
    static RUNTIME: OnceLock<std::io::Result<Runtime>> = OnceLock::new();
    RUNTIME
        .get_or_init(|| {
            tokio::runtime::Builder::new_multi_thread()
                .worker_threads(2)
                .enable_all()
                .build()
        })
        .as_ref()
        .map_err(|_| ElasticsearchError::Network)
}
