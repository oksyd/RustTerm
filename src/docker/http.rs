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
        let response = async {
            // Consuming a response body can finish before the driver makes its
            // dispatcher reusable. Wait for readiness before sending, including
            // on a freshly opened connection; never retry a submitted mutation.
            self.send.ready().await?;
            self.send.send_request(request).await
        }
        .await;
        response.map_err(|error| {
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

    #[tokio::test]
    async fn reused_connection_waits_for_the_previous_response_to_finish() {
        tokio::time::timeout(Duration::from_secs(5), async {
            let (io, peer) = tokio::io::duplex(4096);
            let (send, connection) = hyper::client::conn::http1::handshake(TokioIo::new(io))
                .await
                .unwrap();
            let mut connection = Connection {
                send,
                driver: tokio::spawn(async move {
                    connection.with_upgrades().await.unwrap();
                }),
                diagnostic: Arc::new(Mutex::new(Vec::new())),
            };
            let (release_body, body_ready) = tokio::sync::oneshot::channel();
            let peer = tokio::spawn(async move {
                let mut peer = BufReader::new(peer);
                let mut body_ready = Some(body_ready);
                for path in ["/first", "/second"] {
                    let mut line = String::new();
                    peer.read_line(&mut line).await.unwrap();
                    assert_eq!(line, format!("POST {path} HTTP/1.1\r\n"));
                    loop {
                        line.clear();
                        assert_ne!(peer.read_line(&mut line).await.unwrap(), 0);
                        if line == "\r\n" {
                            break;
                        }
                    }
                    peer.write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\n")
                        .await
                        .unwrap();
                    if let Some(body_ready) = body_ready.take() {
                        body_ready.await.unwrap();
                    }
                    peer.write_all(b"ok").await.unwrap();
                }
            });
            let first = connection
                .send("POST", "/first", None, false)
                .await
                .unwrap();
            let second = connection.send("POST", "/second", None, false);
            tokio::pin!(second);
            // Pause the peer mid-response so the next request must wait for
            // readiness, independent of how the runtime schedules the driver.
            let polled = futures_util::poll!(&mut second);
            assert!(
                polled.is_pending(),
                "request was sent before readiness: {polled:?}"
            );
            release_body.send(()).unwrap();
            assert_eq!(collect(first, 16).await.unwrap(), b"ok");
            assert_eq!(collect(second.await.unwrap(), 16).await.unwrap(), b"ok");
            peer.await.unwrap();
        })
        .await
        .expect("HTTP connection did not become reusable");
    }
}
