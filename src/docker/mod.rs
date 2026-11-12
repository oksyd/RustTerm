//! Docker Engine over independent channels of an existing SSH connection.
//! No daemon TCP listener, local Docker installation, or remote agent is required.
mod http;
mod logs;
mod model;
#[cfg(test)]
mod tests;
use crate::{
    i18n::{messages as key, tr},
    session::exec::ExecClient,
};
pub use logs::{LogBuffer, LogLine, LogStream};
pub use model::{Container, ContainerDetails, EngineVersion, Stats};
use serde::de::DeserializeOwned;
use serde_json::{Value, json};
use std::{future::Future, sync::Arc, time::Duration};
use tokio::sync::{Mutex, OnceCell, Semaphore};
use tokio_util::sync::CancellationToken;

pub type Result<T> = std::result::Result<T, Error>;
#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("{}", tr(key::DOCKER_DISCONNECTED))]
    Disconnected,
    #[error("{}", tr(key::DOCKER_BUSY))]
    Busy,
    #[error("{}", tr(key::DOCKER_CANCELLED))]
    Cancelled,
    #[error("{}", tr(key::DOCKER_TIMEOUT))]
    Timeout,
    #[error("{}", tr(key::DOCKER_RESPONSE_LIMIT))]
    Limit,
    #[error("{}", tr(key::DOCKER_INVALID_RESPONSE))]
    InvalidResponse,
    #[error("{}", tr(key::DOCKER_INVALID_ENDPOINT))]
    InvalidEndpoint,
    #[error("{}", tr(key::DOCKER_INVALID_CONTAINER))]
    InvalidContainer,
    #[error("{}", tr(key::DOCKER_UNSUPPORTED_VERSION))]
    Version,
    #[error("{label}: {0}", label = tr(key::DOCKER_TRANSPORT_ERROR))]
    Transport(String),
    #[error("Docker HTTP {status}: {message}")]
    Api { status: u16, message: String },
    #[error("{label}: {0}", label = tr(key::DOCKER_UNCERTAIN_ACTION))]
    Uncertain(String),
}
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub enum Endpoint {
    #[default]
    System,
    Rootless,
    Socket(String),
}
impl Endpoint {
    pub fn parse(value: &str) -> Result<Self> {
        let value = value.trim();
        if value.is_empty() {
            return Ok(Self::System);
        }
        if value == "rootless" {
            return Ok(Self::Rootless);
        }
        if !value.starts_with('/') || value.len() > 4096 || value.chars().any(char::is_control) {
            return Err(Error::InvalidEndpoint);
        }
        Ok(Self::Socket(value.to_owned()))
    }
    fn command(&self) -> String {
        let host = match self {
            Self::System => "'unix:///var/run/docker.sock'".to_owned(),
            Self::Rootless => {
                "\"unix://${XDG_RUNTIME_DIR:-/run/user/$(id -u)}/docker.sock\"".to_owned()
            }
            Self::Socket(path) => format!("'unix://{}'", path.replace('\'', "'\\''")),
        };
        format!(
            "DOCKER_CONTEXT= DOCKER_HOST= DOCKER_TLS= DOCKER_TLS_VERIFY= DOCKER_CERT_PATH= DOCKER_API_VERSION= docker --host {host} system dial-stdio"
        )
    }
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Action {
    Start,
    Stop,
    Restart,
}
impl Action {
    fn path(self) -> &'static str {
        match self {
            Self::Start => "start",
            Self::Stop => "stop?t=10",
            Self::Restart => "restart?t=10",
        }
    }
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Shell {
    Sh,
    Bash,
}
impl Shell {
    pub fn path(self) -> &'static str {
        match self {
            Self::Sh => "/bin/sh",
            Self::Bash => "/bin/bash",
        }
    }
}
struct Inner {
    id: uuid::Uuid,
    exec: ExecClient,
    endpoint: Endpoint,
    version: OnceCell<EngineVersion>,
    connection: Mutex<Option<http::Connection>>,
    requests: Semaphore,
}
#[derive(Clone)]
pub struct DockerClient(Arc<Inner>);
impl DockerClient {
    pub(crate) fn new(exec: ExecClient) -> Self {
        Self::create(exec, Endpoint::System)
    }
    fn create(exec: ExecClient, endpoint: Endpoint) -> Self {
        Self(Arc::new(Inner {
            id: uuid::Uuid::new_v4(),
            exec,
            endpoint,
            version: OnceCell::new(),
            connection: Mutex::new(None),
            requests: Semaphore::new(4),
        }))
    }
    /// Identity shared by clones, distinct for each SSH session and endpoint selection.
    pub fn id(&self) -> uuid::Uuid {
        self.0.id
    }

    pub fn with_endpoint(&self, endpoint: Endpoint) -> Self {
        Self::create(self.0.exec.clone(), endpoint)
    }
    pub fn is_connected(&self) -> bool {
        self.0.exec.is_connected()
    }
    /// Only idle management transport is released. Log/console channels own their lifetimes.
    pub async fn release_idle(&self) {
        self.0.connection.lock().await.take();
    }
    async fn run<T: Send + 'static>(
        &self,
        work: impl Future<Output = Result<T>> + Send + 'static,
    ) -> Result<T> {
        if !self.is_connected() {
            return Err(Error::Disconnected);
        }
        let permit = self.0.requests.try_acquire().map_err(|_| Error::Busy)?;
        let cancel = CancellationToken::new();
        let _guard = cancel.clone().drop_guard();
        let stop = self.0.exec.stop.clone();
        let task = self.0.exec.runtime.spawn(async move {
            tokio::select! {
                biased;
                _ = stop.cancelled() => Err(Error::Disconnected),
                _ = cancel.cancelled() => Err(Error::Cancelled),
                result = tokio::time::timeout(Duration::from_secs(30), work) => result.map_err(|_| Error::Timeout)?,
            }
        });
        let result = task.await.map_err(|_| Error::Disconnected)?;
        drop(permit);
        result
    }
    async fn version_inner(&self) -> Result<&EngineVersion> {
        self.0
            .version
            .get_or_try_init(|| async {
                let bytes = self.request("GET", "/version", None).await?;
                let mut version: EngineVersion = decode(&bytes)?;
                version.negotiate()?;
                Ok(version)
            })
            .await
    }
    pub async fn version(&self) -> Result<EngineVersion> {
        let client = self.clone();
        self.run(async move { client.version_inner().await.cloned() })
            .await
    }
    async fn path(&self, resource: &str) -> Result<String> {
        Ok(format!("/v{}{resource}", self.version_inner().await?.api))
    }
    async fn request(&self, method: &str, path: &str, body: Option<Value>) -> Result<Vec<u8>> {
        let mut slot = self.0.connection.lock().await;
        // Taking ownership makes dropping a cancelled request close its transport.
        let mut connection = match slot.take() {
            Some(connection) if !connection.is_closed() => connection,
            _ => http::Connection::open(&self.0.exec, &self.0.endpoint).await?,
        };
        let response = connection.send(method, path, body, false).await?;
        let result = http::collect(response, 8 * 1024 * 1024).await;
        if result.is_ok() {
            *slot = Some(connection);
        }
        result
    }
    async fn get<T: DeserializeOwned>(&self, resource: &str) -> Result<T> {
        decode(
            &self
                .request("GET", &self.path(resource).await?, None)
                .await?,
        )
    }
    pub async fn containers(&self) -> Result<Vec<Container>> {
        let client = self.clone();
        self.run(async move {
            let mut containers: Vec<Container> = client.get("/containers/json?all=true").await?;
            if containers.len() > 20_000 {
                return Err(Error::Limit);
            }
            containers.sort_by(|a, b| (a.project(), a.name()).cmp(&(b.project(), b.name())));
            Ok(containers)
        })
        .await
    }
    pub async fn inspect(&self, id: &str) -> Result<ContainerDetails> {
        validate_id(id)?;
        let id = id.to_owned();
        let client = self.clone();
        self.run(async move {
            let details: ContainerDetails = client.get(&format!("/containers/{id}/json")).await?;
            if details.id != id {
                return Err(Error::InvalidResponse);
            }
            Ok(details)
        })
        .await
    }
    pub async fn stats(&self, id: &str) -> Result<Stats> {
        validate_id(id)?;
        let id = id.to_owned();
        let client = self.clone();
        self.run(async move {
            client
                .get(&format!(
                    "/containers/{id}/stats?stream=false&one-shot=true"
                ))
                .await
        })
        .await
    }
    pub async fn action(&self, id: &str, action: Action) -> Result<()> {
        validate_id(id)?;
        let id = id.to_owned();
        let client = self.clone();
        // Mutations are never automatically retried, including ambiguous disconnects.
        let result = self
            .run(async move {
                let path = client
                    .path(&format!("/containers/{id}/{}", action.path()))
                    .await?;
                client.request("POST", &path, None).await?;
                Ok(())
            })
            .await;
        match result {
            Err(e @ (Error::Timeout | Error::Transport(_) | Error::Disconnected)) => {
                Err(Error::Uncertain(e.to_string()))
            }
            result => result,
        }
    }
    pub async fn logs(&self, id: &str, tty: bool) -> Result<LogStream> {
        validate_id(id)?;
        let id = id.to_owned();
        let client = self.clone();
        self.run(async move {
            let path = client.path(&format!("/containers/{id}/logs?stdout=true&stderr=true&timestamps=true&follow=true&tail=200")).await?;
            let mut connection = http::Connection::open(&client.0.exec, &client.0.endpoint).await?;
            let response = connection.send("GET", &path, None, false).await?;
            if !response.status().is_success() { http::collect(response, 64 * 1024).await?; return Err(Error::InvalidResponse); }
            Ok(LogStream::start(response.into_body(), connection, tty, client.0.exec.stop.clone()))
        }).await
    }
    /// A bounded raw JSON event stream, on its own SSH channel so idle event
    /// waits never hold the management request connection or block the shell.
    pub(crate) async fn container_events(&self) -> Result<LogStream> {
        let client = self.clone();
        self.run(async move {
            let filters = url::form_urlencoded::Serializer::new(String::new())
                .append_pair(
                    "filters",
                    r#"{"type":["container"],"event":["start","die","destroy","rename"]}"#,
                )
                .finish();
            let path = client.path(&format!("/events?{filters}")).await?;
            let mut connection = http::Connection::open(&client.0.exec, &client.0.endpoint).await?;
            let response = connection.send("GET", &path, None, false).await?;
            if !response.status().is_success() {
                http::collect(response, 64 * 1024).await?;
                return Err(Error::InvalidResponse);
            }
            Ok(LogStream::start(
                response.into_body(),
                connection,
                true,
                client.0.exec.stop.clone(),
            ))
        })
        .await
    }
    pub(crate) async fn console(
        &self,
        id: String,
        shell: Shell,
        columns: u16,
        rows: u16,
    ) -> Result<Console> {
        validate_id(&id)?;
        let client = self.clone();
        self.run(async move {
            // Reserve the interactive channel before creating a remote exec instance.
            let mut connection = http::Connection::open(&client.0.exec, &client.0.endpoint).await?;
            let path = client.path(&format!("/containers/{id}/exec")).await?;
            let result: Value = decode(&client.request("POST", &path, Some(json!({"AttachStdin":true,"AttachStdout":true,"AttachStderr":true,"Tty":true,"Cmd":[shell.path()],"Env":["TERM=xterm-256color"],"ConsoleSize":[rows,columns]}))).await?)?;
            let id = result["Id"].as_str().ok_or(Error::InvalidResponse)?.to_owned();
            validate_id(&id)?;
            let path = client.path(&format!("/exec/{id}/start")).await?;
            let mut response = connection.send("POST", &path, Some(json!({"Detach":false,"Tty":true,"ConsoleSize":[rows,columns]})), true).await?;
            if response.status() != hyper::StatusCode::SWITCHING_PROTOCOLS {
                http::collect(response, 64 * 1024).await?;
                return Err(Error::InvalidResponse);
            }
            let io = hyper_util::rt::TokioIo::new(hyper::upgrade::on(&mut response).await.map_err(|e| Error::Transport(e.to_string()))?);
            Ok(Console { id, io, _connection: connection })
        }).await
    }
    pub(crate) async fn resize_exec(&self, id: &str, columns: u16, rows: u16) -> Result<()> {
        validate_id(id)?;
        let id = id.to_owned();
        let client = self.clone();
        self.run(async move {
            let path = client
                .path(&format!("/exec/{id}/resize?h={rows}&w={columns}"))
                .await?;
            client.request("POST", &path, None).await.map(|_| ())
        })
        .await
    }
    pub(crate) async fn exec_exit(&self, id: &str) -> Result<Option<u32>> {
        validate_id(id)?;
        let id = id.to_owned();
        let client = self.clone();
        self.run(async move {
            let value: Value = client.get(&format!("/exec/{id}/json")).await?;
            Ok((value["Running"] == false)
                .then(|| {
                    value["ExitCode"]
                        .as_u64()
                        .and_then(|n| u32::try_from(n).ok())
                })
                .flatten())
        })
        .await
    }
}
pub(crate) struct Console {
    pub(crate) id: String,
    pub(crate) io: hyper_util::rt::TokioIo<hyper::upgrade::Upgraded>,
    _connection: http::Connection,
}
fn decode<T: DeserializeOwned>(bytes: &[u8]) -> Result<T> {
    serde_json::from_slice(bytes).map_err(|_| Error::InvalidResponse)
}
fn validate_id(id: &str) -> Result<()> {
    if id.len() != 64 || !id.bytes().all(|b| b.is_ascii_hexdigit()) {
        Err(Error::InvalidContainer)
    } else {
        Ok(())
    }
}
impl tokio::io::AsyncRead for Console {
    fn poll_read(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        buffer: &mut tokio::io::ReadBuf<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        std::pin::Pin::new(&mut self.io).poll_read(cx, buffer)
    }
}
impl tokio::io::AsyncWrite for Console {
    fn poll_write(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        buffer: &[u8],
    ) -> std::task::Poll<std::io::Result<usize>> {
        std::pin::Pin::new(&mut self.io).poll_write(cx, buffer)
    }
    fn poll_flush(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        std::pin::Pin::new(&mut self.io).poll_flush(cx)
    }
    fn poll_shutdown(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        std::pin::Pin::new(&mut self.io).poll_shutdown(cx)
    }
}
