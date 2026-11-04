use super::{MAX_RESPONSE_BYTES, RedisError, Result};
use crate::config::{RedisConfig, Secret};
use redis::{AsyncConnectionConfig, RedisConnectionInfo, aio::MultiplexedConnection};
use std::{
    io,
    pin::Pin,
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    },
    task::{Context, Poll},
};
use tokio::{
    io::{AsyncRead, AsyncWrite, ReadBuf},
    net::TcpStream,
    task::JoinHandle,
};

const MAX_RESPONSE_NODES: usize = 16_384;
trait Socket: AsyncRead + AsyncWrite + Unpin + Send {}
impl<T: AsyncRead + AsyncWrite + Unpin + Send> Socket for T {}

struct Budget {
    remaining: AtomicUsize,
    exceeded: AtomicBool,
    nodes: AtomicUsize,
}
impl Budget {
    fn new() -> Self {
        Self {
            remaining: AtomicUsize::new(MAX_RESPONSE_BYTES),
            exceeded: AtomicBool::new(false),
            nodes: AtomicUsize::new(0),
        }
    }
    fn reset(&self) {
        self.remaining.store(MAX_RESPONSE_BYTES, Ordering::Relaxed);
        self.nodes.store(0, Ordering::Relaxed);
    }
    fn limit(&self) -> io::Error {
        self.exceeded.store(true, Ordering::Relaxed);
        io::Error::other("Redis response limit")
    }
}

/// Allocation guard only: redis-rs still performs all RESP parsing. Its array
/// parser reserves the declared length, so a wire-byte limit alone is too late.
/// Inspect only RESP2 length headers, skipping bulk payload bytes verbatim.
#[derive(Default)]
struct LengthGuard {
    line: Vec<u8>,
    bulk_remaining: usize,
}
impl LengthGuard {
    fn check(&mut self, mut bytes: &[u8], budget: &Budget) -> io::Result<()> {
        while !bytes.is_empty() {
            if self.bulk_remaining > 0 {
                let skip = self.bulk_remaining.min(bytes.len());
                self.bulk_remaining -= skip;
                bytes = &bytes[skip..];
                continue;
            }
            let byte = bytes[0];
            bytes = &bytes[1..];
            self.line.push(byte);
            if self.line.len() > 4096 {
                return Err(budget.limit());
            }
            if !self.line.ends_with(b"\r\n") {
                continue;
            }
            match self.line.first() {
                Some(b'$' | b'*') => {
                    let length = std::str::from_utf8(&self.line[1..self.line.len() - 2])
                        .ok()
                        .and_then(|value| value.parse::<i64>().ok())
                        .ok_or_else(|| budget.limit())?;
                    if length < -1 {
                        return Err(budget.limit());
                    }
                    if length >= 0 {
                        let length = usize::try_from(length).map_err(|_| budget.limit())?;
                        if self.line[0] == b'$' {
                            if length > MAX_RESPONSE_BYTES {
                                return Err(budget.limit());
                            }
                            self.bulk_remaining = length + 2;
                        } else if length > MAX_RESPONSE_NODES
                            || budget.nodes.fetch_add(length, Ordering::Relaxed)
                                > MAX_RESPONSE_NODES - length
                        {
                            return Err(budget.limit());
                        }
                    }
                }
                Some(b'+' | b'-' | b':') => {}
                _ => return Err(budget.limit()),
            }
            self.line.clear();
        }
        Ok(())
    }
}

struct BoundedSocket {
    socket: Box<dyn Socket>,
    budget: Arc<Budget>,
    guard: LengthGuard,
}
impl AsyncRead for BoundedSocket {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        if buf.remaining() == 0 {
            return Poll::Ready(Ok(()));
        }
        let this = self.get_mut();
        let available = this
            .budget
            .remaining
            .load(Ordering::Relaxed)
            .min(buf.remaining())
            .min(16 * 1024);
        if available == 0 {
            // A complete response may exactly fill the budget. Keep the idle
            // connection open, rejecting only an actual byte beyond the limit.
            let mut byte = [0];
            let mut probe = ReadBuf::new(&mut byte);
            return match Pin::new(&mut this.socket).poll_read(cx, &mut probe) {
                Poll::Ready(Ok(())) if !probe.filled().is_empty() => {
                    Poll::Ready(Err(this.budget.limit()))
                }
                result => result,
            };
        }
        let mut limited = ReadBuf::new(buf.initialize_unfilled_to(available));
        match Pin::new(&mut this.socket).poll_read(cx, &mut limited) {
            Poll::Ready(Ok(())) => {
                let count = limited.filled().len();
                if let Err(error) = this.guard.check(limited.filled(), &this.budget) {
                    return Poll::Ready(Err(error));
                }
                this.budget.remaining.fetch_sub(count, Ordering::Relaxed);
                buf.advance(count);
                Poll::Ready(Ok(()))
            }
            other => other,
        }
    }
}
impl AsyncWrite for BoundedSocket {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        bytes: &[u8],
    ) -> Poll<io::Result<usize>> {
        Pin::new(&mut self.get_mut().socket).poll_write(cx, bytes)
    }
    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.get_mut().socket).poll_flush(cx)
    }
    fn poll_shutdown(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.get_mut().socket).poll_shutdown(cx)
    }
}

pub(super) struct Connection {
    connection: MultiplexedConnection,
    driver: JoinHandle<()>,
    budget: Arc<Budget>,
    database: u32,
}
impl Drop for Connection {
    fn drop(&mut self) {
        self.driver.abort();
    }
}
impl Connection {
    pub fn is_closed(&self) -> bool {
        self.driver.is_finished()
    }
    pub async fn closed(&mut self) {
        let _ = (&mut self.driver).await;
    }
    pub async fn open(config: RedisConfig, secret: Option<Secret>) -> Result<Self> {
        let host = crate::config::known_hosts::normalize_host(&config.host)
            .map_err(|_| RedisError::InvalidConfig)?;
        let socket = TcpStream::connect((host.as_str(), config.port))
            .await
            .map_err(|_| RedisError::Connection)?;
        socket
            .set_nodelay(true)
            .map_err(|_| RedisError::Connection)?;
        Self::over_stream(config, secret, socket).await
    }
    /// Applies the same TLS identity and RESP limits to TCP and SSH channels.
    /// The certificate identity is always the original Redis host, never SSH.
    pub async fn over_stream<S>(
        config: RedisConfig,
        secret: Option<Secret>,
        socket: S,
    ) -> Result<Self>
    where
        S: AsyncRead + AsyncWrite + Unpin + Send + 'static,
    {
        let socket = secure_stream(&config, Box::new(socket)).await?;
        let info = connection_info(&config, secret.as_ref());
        let budget = Arc::new(Budget::new());
        let stream = BoundedSocket {
            socket,
            budget: budget.clone(),
            guard: LengthGuard::default(),
        };
        let settings = AsyncConnectionConfig::new()
            // The actor deadline is the sole timeout, including queueing and
            // all commands of one inspect. redis-rs defaults to a shorter 500ms.
            .set_response_timeout(None)
            .set_pipeline_buffer_size(1)
            .set_concurrency_limit(1);
        let (connection, driver) = MultiplexedConnection::new_with_config(&info, stream, settings)
            .await
            .map_err(|error| {
                if budget.exceeded.load(Ordering::Relaxed) {
                    RedisError::Limit
                } else {
                    classify(&error)
                }
            })?;
        // Credentials only live during connection establishment, never in the actor.
        drop(info);
        drop(secret);
        Ok(Self {
            connection,
            driver: tokio::spawn(driver),
            budget,
            database: config.database,
        })
    }
    pub async fn select(&mut self, database: u32) -> Result<()> {
        if database != self.database {
            self.query(vec![b"SELECT".to_vec(), database.to_string().into_bytes()])
                .await?;
            self.database = database;
        }
        Ok(())
    }
    pub async fn query(&mut self, args: Vec<Vec<u8>>) -> Result<redis::Value> {
        if self.driver.is_finished() {
            return Err(RedisError::Disconnected);
        }
        self.budget.reset();
        let mut command = redis::Cmd::new();
        for argument in args {
            command.arg(argument);
        }
        command
            .query_async(&mut self.connection)
            .await
            .map_err(|error| {
                if self.budget.exceeded.load(Ordering::Relaxed) {
                    RedisError::Limit
                } else {
                    classify(&error)
                }
            })
    }
}

async fn secure_stream(config: &RedisConfig, socket: Box<dyn Socket>) -> Result<Box<dyn Socket>> {
    let host = crate::config::known_hosts::normalize_host(&config.host)
        .map_err(|_| RedisError::InvalidConfig)?;
    let socket: Box<dyn Socket> = if config.tls {
        let mut roots = rustls::RootCertStore::empty();
        for certificate in rustls_native_certs::load_native_certs().certs {
            roots.add(certificate).map_err(|_| RedisError::Tls)?;
        }
        if roots.is_empty() {
            return Err(RedisError::Tls);
        }
        let tls = rustls::ClientConfig::builder_with_provider(Arc::new(
            rustls::crypto::ring::default_provider(),
        ))
        .with_safe_default_protocol_versions()
        .map_err(|_| RedisError::Tls)?
        .with_root_certificates(roots)
        .with_no_client_auth();
        let server = rustls::pki_types::ServerName::try_from(host).map_err(|_| RedisError::Tls)?;
        Box::new(
            tokio_rustls::TlsConnector::from(Arc::new(tls))
                .connect(server, socket)
                .await
                .map_err(|_| RedisError::Tls)?,
        )
    } else {
        socket
    };
    Ok(socket)
}
fn connection_info(config: &RedisConfig, secret: Option<&Secret>) -> RedisConnectionInfo {
    let mut info = RedisConnectionInfo::default()
        .set_protocol(redis::ProtocolVersion::RESP2)
        .set_db(i64::from(config.database))
        .set_skip_set_lib_name();
    let has_username = config.username.is_some();
    if let Some(username) = &config.username {
        info = info.set_username(username.clone());
    }
    if let Some(secret) = secret {
        info = info.set_password(secret.expose());
    } else if has_username {
        // RESP2 redis-rs emits AUTH only when password is Some. Preserve an
        // explicit ACL identity even for nopass users; never fall back to default.
        info = info.set_password("");
    }
    info
}

pub(super) struct Subscription {
    stream: tokio_util::codec::Framed<BoundedSocket, redis::ValueCodec>,
    budget: Arc<Budget>,
}
impl Subscription {
    pub async fn open<S: AsyncRead + AsyncWrite + Unpin + Send + 'static>(
        config: &RedisConfig,
        secret: Option<&Secret>,
        socket: S,
        pattern: &str,
    ) -> Result<Self> {
        use futures_util::SinkExt;
        let socket = secure_stream(config, Box::new(socket)).await?;
        let budget = Arc::new(Budget::new());
        let socket = BoundedSocket {
            socket,
            budget: budget.clone(),
            guard: LengthGuard::default(),
        };
        let mut subscription = Self {
            stream: tokio_util::codec::Framed::new(socket, redis::ValueCodec::default()),
            budget,
        };
        if secret.is_some() || config.username.is_some() {
            let mut command = redis::cmd("AUTH");
            if let Some(username) = &config.username {
                command.arg(username);
            }
            command.arg(secret.map(Secret::expose).unwrap_or(""));
            subscription
                .stream
                .send(command.get_packed_command())
                .await
                .map_err(|e| classify(&e))?;
            if subscription.next_value().await? != redis::Value::Okay {
                return Err(RedisError::Protocol);
            }
        }
        // Pub/Sub is server-wide, so no SELECT is sent on this dedicated connection.
        subscription
            .stream
            .send(redis::cmd("PSUBSCRIBE").arg(pattern).get_packed_command())
            .await
            .map_err(|e| classify(&e))?;
        let reply = subscription.next_value().await?;
        let redis::Value::Array(values) = reply else {
            return Err(RedisError::Protocol);
        };
        if !matches!(values.as_slice(),[redis::Value::BulkString(kind),redis::Value::BulkString(channel),redis::Value::Int(1)] if kind==b"psubscribe" && channel==pattern.as_bytes())
        {
            return Err(RedisError::Protocol);
        }
        Ok(subscription)
    }
    async fn next_value(&mut self) -> Result<redis::Value> {
        use futures_util::StreamExt;
        self.budget.reset();
        let value = self
            .stream
            .next()
            .await
            .ok_or(RedisError::Disconnected)?
            .map_err(|error| {
                if self.budget.exceeded.load(Ordering::Relaxed) {
                    RedisError::Limit
                } else {
                    classify(&error)
                }
            })?;
        value.extract_error().map_err(|error| classify(&error))
    }
    pub async fn receive(&mut self) -> Result<super::PubSubMessage> {
        let redis::Value::Array(mut fields) = self.next_value().await? else {
            return Err(RedisError::Protocol);
        };
        if fields.len() != 4
            || !matches!(&fields[0],redis::Value::BulkString(kind) if kind==b"pmessage")
        {
            return Err(RedisError::Protocol);
        }
        let redis::Value::BulkString(payload) = fields.pop().unwrap() else {
            return Err(RedisError::Protocol);
        };
        let redis::Value::BulkString(channel) = fields.pop().unwrap() else {
            return Err(RedisError::Protocol);
        };
        if channel.len() > 1024 || payload.len() > 65536 {
            return Err(RedisError::Limit);
        }
        Ok(super::PubSubMessage { channel, payload })
    }
}

pub(super) fn classify(error: &redis::RedisError) -> RedisError {
    if let Some((address, slot)) = error.redirect_node() {
        return match address.parse() {
            Ok(endpoint) if slot < 16384 => RedisError::Redirect {
                ask: error.code() == Some("ASK"),
                slot,
                endpoint,
            },
            _ => RedisError::Protocol,
        };
    }
    // Never display server-provided details: they may echo passwords, keys or values.
    match error.code() {
        Some("WRONGPASS" | "NOAUTH") => RedisError::Authentication,
        Some("NOPERM") => RedisError::Permission,
        Some("WRONGTYPE") => RedisError::WrongType,
        Some("READONLY") => RedisError::ReadOnly,
        Some("CROSSSLOT") => RedisError::CrossSlot,
        _ => match error.kind() {
            redis::ErrorKind::AuthenticationFailed => RedisError::Authentication,
            redis::ErrorKind::Io => RedisError::Disconnected,
            redis::ErrorKind::Parse | redis::ErrorKind::UnexpectedReturnType => {
                RedisError::Protocol
            }
            _ => RedisError::Server,
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    #[tokio::test]
    async fn exact_limit_response_stays_connected_while_idle_and_accepts_next_command() {
        tokio::time::timeout(Duration::from_secs(2), async {
            let (client, mut server) = tokio::io::duplex(16 * 1024);
            let (resume, idle) = tokio::sync::oneshot::channel();
            let server = tokio::spawn(async move {
                let expected = redis::cmd("GET").arg("key").get_packed_command();
                let mut request = vec![0; expected.len()];
                server.read_exact(&mut request).await.unwrap();
                assert_eq!(request, expected);
                // Seven length digits plus RESP's framing consume twelve bytes.
                let payload = vec![b'x'; MAX_RESPONSE_BYTES - 12];
                let header = format!("${}\r\n", payload.len());
                assert_eq!(header.len() + payload.len() + 2, MAX_RESPONSE_BYTES);
                server.write_all(header.as_bytes()).await.unwrap();
                server.write_all(&payload).await.unwrap();
                server.write_all(b"\r\n").await.unwrap();
                idle.await.unwrap();
                let expected = redis::cmd("PING").get_packed_command();
                request.resize(expected.len(), 0);
                server.read_exact(&mut request).await.unwrap();
                assert_eq!(request, expected);
                server.write_all(b"+PONG\r\n").await.unwrap();
            });
            let mut connection = Connection::over_stream(RedisConfig::default(), None, client)
                .await
                .unwrap();
            let reply = connection
                .query(vec![b"GET".to_vec(), b"key".to_vec()])
                .await
                .unwrap();
            let redis::Value::BulkString(payload) = reply else {
                panic!("expected a bulk string");
            };
            assert_eq!(payload.len(), MAX_RESPONSE_BYTES - 12);
            assert!(payload.iter().all(|byte| *byte == b'x'));
            tokio::task::yield_now().await;
            assert!(!connection.driver.is_finished());
            assert!(!connection.budget.exceeded.load(Ordering::Relaxed));
            resume.send(()).unwrap();
            assert_eq!(
                connection.query(vec![b"PING".to_vec()]).await.unwrap(),
                redis::Value::SimpleString("PONG".into())
            );
            server.await.unwrap();
        })
        .await
        .unwrap();
    }

    #[tokio::test]
    async fn response_one_byte_over_limit_is_rejected() {
        tokio::time::timeout(Duration::from_secs(2), async {
            let (client, mut server) = tokio::io::duplex(MAX_RESPONSE_BYTES + 1);
            let server = tokio::spawn(async move {
                let expected = redis::cmd("GET").arg("key").get_packed_command();
                let mut request = vec![0; expected.len()];
                server.read_exact(&mut request).await.unwrap();
                assert_eq!(request, expected);
                let payload = vec![b'x'; MAX_RESPONSE_BYTES - 11];
                let header = format!("${}\r\n", payload.len());
                assert_eq!(header.len() + payload.len() + 2, MAX_RESPONSE_BYTES + 1);
                server.write_all(header.as_bytes()).await.unwrap();
                server.write_all(&payload).await.unwrap();
                server.write_all(b"\r\n").await.unwrap();
            });
            let mut connection = Connection::over_stream(RedisConfig::default(), None, client)
                .await
                .unwrap();
            assert_eq!(
                connection
                    .query(vec![b"GET".to_vec(), b"key".to_vec()])
                    .await,
                Err(RedisError::Limit)
            );
            server.await.unwrap();
        })
        .await
        .unwrap();
    }

    #[tokio::test]
    async fn exhausted_budget_preserves_idle_and_eof_semantics() {
        let (client, server) = tokio::io::duplex(1);
        let budget = Arc::new(Budget::new());
        budget.remaining.store(0, Ordering::Relaxed);
        let mut socket = BoundedSocket {
            socket: Box::new(client),
            budget: budget.clone(),
            guard: LengthGuard::default(),
        };
        let mut bytes = [0];
        let mut context = Context::from_waker(std::task::Waker::noop());
        assert!(
            Pin::new(&mut socket)
                .poll_read(&mut context, &mut ReadBuf::new(&mut bytes))
                .is_pending()
        );
        assert!(!budget.exceeded.load(Ordering::Relaxed));
        drop(server);
        assert_eq!(socket.read(&mut bytes).await.unwrap(), 0);
        assert!(!budget.exceeded.load(Ordering::Relaxed));
    }

    #[test]
    fn length_guard_rejects_declared_array_before_client_allocation() {
        let budget = Budget::new();
        assert!(
            LengthGuard::default()
                .check(b"*999999999999\r\n", &budget)
                .is_err()
        );
        assert!(
            LengthGuard::default()
                .check(b"$999999999999\r\n", &Budget::new())
                .is_err()
        );
    }
    #[test]
    fn length_guard_skips_binary_payload_and_handles_split_headers() {
        let budget = Budget::new();
        let mut guard = LengthGuard::default();
        for byte in b"*2\r\n$5\r\n*xxx\0\r\n$-1\r\n" {
            guard.check(&[*byte], &budget).unwrap();
        }
        assert!(!budget.exceeded.load(Ordering::Relaxed));
    }
    #[test]
    fn server_errors_do_not_echo_details() {
        let error = redis::RedisError::from((
            redis::ErrorKind::AuthenticationFailed,
            "secret=not-for-the-ui",
        ));
        assert!(!classify(&error).to_string().contains("not-for-the-ui"));
    }
    #[tokio::test]
    async fn subscription_applies_backpressure_and_delivers_binary_messages() {
        tokio::time::timeout(Duration::from_secs(3), async {
            let (client, mut server) = tokio::io::duplex(1024);
            let server = tokio::spawn(async move {
                let expected = redis::cmd("PSUBSCRIBE").arg("*").get_packed_command();
                let mut request = vec![0; expected.len()];
                server.read_exact(&mut request).await.unwrap();
                assert_eq!(request, expected);
                server
                    .write_all(b"*3\r\n$10\r\npsubscribe\r\n$1\r\n*\r\n:1\r\n")
                    .await
                    .unwrap();
                for _ in 0..20 {
                    server
                        .write_all(b"*4\r\n$8\r\npmessage\r\n$1\r\n*\r\n$7\r\nchannel\r\n$4096\r\n")
                        .await
                        .unwrap();
                    server.write_all(&vec![0; 4096]).await.unwrap();
                    server.write_all(b"\r\n").await.unwrap();
                }
            });
            let mut subscription = Subscription::open(&RedisConfig::default(), None, client, "*")
                .await
                .unwrap();
            // No background parser drains the socket while the consumer is idle.
            for _ in 0..20 {
                tokio::task::yield_now().await;
            }
            assert!(!server.is_finished());
            for _ in 0..20 {
                let message = subscription.receive().await.unwrap();
                assert_eq!(message.channel, b"channel");
                assert_eq!(message.payload, vec![0; 4096]);
            }
            server.await.unwrap();
            assert!(matches!(
                subscription.receive().await,
                Err(RedisError::Disconnected)
            ));
        })
        .await
        .unwrap();
    }
}
