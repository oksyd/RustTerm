use super::{Endpoint, Error, Result};
use crate::session::exec::ExecClient;
use http_body_util::{BodyExt, Full};
use hyper::{
    Request, Response,
    body::{Bytes, Incoming},
    client::conn::http1::SendRequest,
};
use hyper_util::rt::TokioIo;
use serde_json::Value;
use std::sync::{Arc, Mutex};

pub(super) struct Connection {
    send: SendRequest<Full<Bytes>>,
    driver: tokio::task::JoinHandle<()>,
    diagnostic: Arc<Mutex<Vec<u8>>>,
}
impl Drop for Connection {
    fn drop(&mut self) {
        self.driver.abort();
    }
}
impl Connection {
    pub(super) async fn open(exec: &ExecClient, endpoint: &Endpoint) -> Result<Self> {
        let io = exec.open(endpoint.command()).await?;
        let diagnostic = io.diagnostic();
        let (send, connection) = hyper::client::conn::http1::Builder::new()
            .max_buf_size(64 * 1024)
            .handshake(TokioIo::new(io))
            .await
            .map_err(|e| Error::Transport(e.to_string()))?;
        let driver = tokio::spawn(async move {
            let _ = connection.with_upgrades().await;
        });
        Ok(Self {
            send,
            driver,
            diagnostic,
        })
    }
    pub(super) fn is_closed(&self) -> bool {
        self.send.is_closed()
    }
    pub(super) async fn send(
        &mut self,
        method: &str,
        path: &str,
        body: Option<Value>,
        upgrade: bool,
    ) -> Result<Response<Incoming>> {
        let mut request = Request::builder()
            .method(method)
            .uri(path)
            .header("Host", "docker")
            .header("Content-Type", "application/json");
        if upgrade {
            request = request
                .header("Connection", "Upgrade")
                .header("Upgrade", "tcp");
        }
        let body = body.map(|v| v.to_string()).unwrap_or_default();
        let request = request
            .body(Full::new(Bytes::from(body)))
            .map_err(|_| Error::InvalidResponse)?;
        self.send.send_request(request).await.map_err(|error| {
            let stderr = self.diagnostic.lock().unwrap_or_else(|e| e.into_inner());
            let diagnostic = String::from_utf8_lossy(&stderr);
            Error::Transport(if diagnostic.trim().is_empty() {
                error.to_string()
            } else {
                diagnostic.trim().to_owned()
            })
        })
    }
}
pub(super) async fn collect(response: Response<Incoming>, limit: usize) -> Result<Vec<u8>> {
    let status = response.status();
    let mut body = response.into_body();
    let limit = if status.is_success() || status.as_u16() == 304 {
        limit
    } else {
        limit.min(64 * 1024)
    };
    let mut bytes = Vec::new();
    while let Some(frame) = body.frame().await {
        let frame = frame.map_err(|e| Error::Transport(e.to_string()))?;
        if let Ok(data) = frame.into_data() {
            if bytes.len().saturating_add(data.len()) > limit {
                return Err(Error::Limit);
            }
            bytes.extend_from_slice(&data);
        }
    }
    if status.is_success() || status.as_u16() == 304 {
        return Ok(bytes);
    }
    let message = serde_json::from_slice::<Value>(&bytes)
        .ok()
        .and_then(|v| v["message"].as_str().map(str::to_owned))
        .unwrap_or_else(|| String::from_utf8_lossy(&bytes).into_owned());
    Err(Error::Api {
        status: status.as_u16(),
        message,
    })
}
