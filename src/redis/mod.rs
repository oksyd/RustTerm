//! Independent, bounded Redis connections. Public futures work on any executor;
//! a private Tokio worker owns each connection and never retries a command.
use crate::i18n::messages as i18n_key;

mod console;
mod operations;
mod router;
#[cfg(test)]
mod tests;
mod transport;

use crate::config::{RedisConfig, Secret};
pub use console::{
    CREATE_SCRIPT, SAVE_LIST_SCRIPT, SAVE_STRING_SCRIPT, display_bytes, editable_text,
    parse_command,
};
use std::{
    sync::{Arc, OnceLock},
    time::Duration,
};
use tokio::{
    runtime::Runtime,
    sync::{mpsc, oneshot},
    time::Instant,
};
use tokio_util::sync::CancellationToken;

pub const PAGE_SIZE: usize = 100;
pub const STRING_PAGE_BYTES: usize = 64 * 1024;
pub const MAX_RESPONSE_BYTES: usize = 1024 * 1024;
pub const MAX_PAGE_ITEMS: usize = 1024;
const MAX_REQUEST_BYTES: usize = 1024 * 1024;
const MAX_ARGUMENTS: usize = 1024;
const QUEUE_CAPACITY: usize = 8;

#[derive(Clone, Debug, thiserror::Error, PartialEq, Eq)]
pub enum RedisError {
    #[error(
        "{}",
        crate::i18n::tr(i18n_key::REDIS_REDIS_CLUSTER_REDIRECTED_THE_REQUEST)
    )]
    Redirect {
        ask: bool,
        slot: u16,
        endpoint: crate::config::RedisEndpoint,
    },
    #[error(
        "{}",
        crate::i18n::tr(
            i18n_key::REDIS_REDIS_TOPOLOGY_DISCOVERY_FAILED_CHECK_THE_SEED_NODES_AND_PERMISSIONS
        )
    )]
    Topology,
    #[error(
        "{}",
        crate::i18n::tr(
            i18n_key::REDIS_THE_SELECTED_REDIS_NODE_IS_NO_LONGER_THE_PRIMARY_RECONNECT
        )
    )]
    ReadOnly,
    #[error(
        "{}",
        crate::i18n::tr(i18n_key::REDIS_THIS_COMMAND_USES_KEYS_IN_DIFFERENT_REDIS_CLUSTER_SLOTS)
    )]
    CrossSlot,
    #[error(
        "{}",
        crate::i18n::tr(i18n_key::REDIS_INVALID_REDIS_CONNECTION_SETTINGS)
    )]
    InvalidConfig,
    #[error("{}", crate::i18n::tr(i18n_key::REDIS_COULD_NOT_CONNECT_TO_REDIS))]
    Connection,
    #[error(
        "{}",
        crate::i18n::tr(i18n_key::REDIS_REDIS_TLS_VERIFICATION_OR_HANDSHAKE_FAILED)
    )]
    Tls,
    #[error("{}", crate::i18n::tr(i18n_key::REDIS_REDIS_AUTHENTICATION_FAILED))]
    Authentication,
    #[error("{}", crate::i18n::tr(i18n_key::REDIS_REDIS_PERMISSION_DENIED))]
    Permission,
    #[error(
        "{}",
        crate::i18n::tr(i18n_key::REDIS_THE_REDIS_KEY_HAS_A_DIFFERENT_TYPE_REFRESH_IT_BEFORE)
    )]
    WrongType,
    #[error(
        "{}",
        crate::i18n::tr(
            i18n_key::REDIS_REDIS_REJECTED_THE_COMMAND_CHECK_ITS_ARGUMENTS_AND_SERVER_VERSION
        )
    )]
    Server,
    #[error(
        "{}",
        crate::i18n::tr(
            i18n_key::REDIS_THIS_REDIS_COMMAND_IS_NOT_SUPPORTED_IN_THE_BOUNDED_CONSOLE
        )
    )]
    UnsupportedCommand,
    #[error(
        "{}",
        crate::i18n::tr(i18n_key::REDIS_INVALID_REDIS_COMMAND_QUOTING_OR_ARGUMENTS)
    )]
    InvalidCommand,
    #[error(
        "{}",
        crate::i18n::tr(i18n_key::REDIS_THE_REDIS_REQUEST_OR_RESPONSE_EXCEEDS_THE_SIZE_LIMIT_A)
    )]
    Limit,
    #[error(
        "{}",
        crate::i18n::tr(
            i18n_key::REDIS_REDIS_TIMED_OUT_AND_WAS_DISCONNECTED_A_SUBMITTED_WRITE_MAY
        )
    )]
    Timeout,
    #[error(
        "{}",
        crate::i18n::tr(
            i18n_key::REDIS_REDIS_IS_DISCONNECTED_A_SUBMITTED_WRITE_MAY_HAVE_COMPLETED_CHECK
        )
    )]
    Disconnected,
    #[error(
        "{}",
        crate::i18n::tr(i18n_key::REDIS_REDIS_IS_BUSY_RETRY_AFTER_THE_CURRENT_REQUESTS_FINISH)
    )]
    Busy,
    #[error(
        "{}",
        crate::i18n::tr(
            i18n_key::REDIS_REDIS_RETURNED_AN_INVALID_OR_UNSUPPORTED_RESPONSE_A_SUBMITTED_WRITE
        )
    )]
    Protocol,
    #[error(
        "{}",
        crate::i18n::tr(i18n_key::REDIS_THE_REDIS_BACKGROUND_RUNTIME_COULD_NOT_START)
    )]
    Runtime,
    #[error(
        "{}",
        crate::i18n::tr(i18n_key::REDIS_COULD_NOT_ESTABLISH_THE_REDIS_SSH_TUNNEL)
    )]
    SshConnection,
    #[error(
        "{}",
        crate::i18n::tr(i18n_key::REDIS_REDIS_SSH_TUNNEL_AUTHENTICATION_FAILED)
    )]
    SshAuthentication,
    #[error(
        "{}",
        crate::i18n::tr(i18n_key::REDIS_THE_REDIS_SSH_HOST_KEY_WAS_REJECTED)
    )]
    SshHostKeyRejected,
    #[error(
        "{}",
        crate::i18n::tr(i18n_key::REDIS_THE_REDIS_SSH_HOST_KEY_CHANGED_AUTHENTICATION_WAS_BLOCKED)
    )]
    SshHostKeyChanged,
    #[error(
        "{}",
        crate::i18n::tr(i18n_key::REDIS_COULD_NOT_READ_OR_SAVE_REDIS_SSH_HOST_TRUST)
    )]
    SshKnownHosts,
    #[error(
        "{}",
        crate::i18n::tr(i18n_key::REDIS_COULD_NOT_UNLOCK_OR_READ_THE_REDIS_SSH_PRIVATE_KEY)
    )]
    SshPrivateKey,
    #[error(
        "{}",
        crate::i18n::tr(i18n_key::REDIS_THE_REDIS_SSH_TUNNEL_CONNECTION_TIMED_OUT)
    )]
    SshTimeout,
}
pub type Result<T> = std::result::Result<T, RedisError>;

impl RedisError {
    pub fn diagnostic_code(&self) -> &'static str {
        match self {
            Self::Redirect { .. } | Self::Topology => "redis-error-topology",
            Self::ReadOnly => "redis-error-read-only",
            Self::CrossSlot => "redis-error-cross-slot",
            Self::InvalidConfig => "redis-error-invalid-config",
            Self::Connection => "redis-error-connection",
            Self::Tls => "redis-error-tls",
            Self::Authentication => "redis-error-authentication",
            Self::Permission => "redis-error-permission",
            Self::WrongType => "redis-error-wrong-type",
            Self::Server => "redis-error-server",
            Self::UnsupportedCommand => "redis-error-unsupported-command",
            Self::InvalidCommand => "redis-error-invalid-command",
            Self::Limit => "redis-error-limit",
            Self::Timeout => "redis-error-timeout",
            Self::Disconnected => "redis-error-disconnected",
            Self::Busy => "redis-error-busy",
            Self::Protocol => "redis-error-protocol",
            Self::Runtime => "redis-error-runtime",
            Self::SshConnection => "redis-error-ssh-connection",
            Self::SshAuthentication => "redis-error-ssh-authentication",
            Self::SshHostKeyRejected => "redis-error-ssh-host-key-rejected",
            Self::SshHostKeyChanged => "redis-error-ssh-host-key-changed",
            Self::SshKnownHosts => "redis-error-ssh-known-hosts",
            Self::SshPrivateKey => "redis-error-ssh-private-key",
            Self::SshTimeout => "redis-error-ssh-timeout",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RedisValue {
    Nil,
    Integer(i64),
    Bytes(Vec<u8>),
    Array(Vec<RedisValue>),
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum KeyType {
    Missing,
    String,
    Hash,
    List,
    Set,
    ZSet,
    Stream,
    Unsupported,
}
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct PageRequest {
    pub cursor: u64,
    pub offset: u64,
    pub after: Option<Vec<u8>>,
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct KeyPage {
    pub keys: Vec<Vec<u8>>,
    pub next_cursor: u64,
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StreamEntry {
    pub id: Vec<u8>,
    pub fields: Vec<(Vec<u8>, Vec<u8>)>,
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum KeyData {
    Missing,
    String(Vec<u8>),
    Hash(Vec<(Vec<u8>, Vec<u8>)>),
    List(Vec<Vec<u8>>),
    Set(Vec<Vec<u8>>),
    ZSet(Vec<(Vec<u8>, Vec<u8>)>),
    Stream(Vec<StreamEntry>),
    Unsupported,
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct KeyInfo {
    pub kind: KeyType,
    pub ttl_ms: i64,
    pub length: u64,
    pub data: KeyData,
    pub next: Option<PageRequest>,
}

struct Inner {
    queue: mpsc::Sender<Request>,
    stop: CancellationToken,
    timeout: Duration,
}

pub struct PubSubMessage {
    pub channel: Vec<u8>,
    pub payload: Vec<u8>,
}
pub struct RedisSubscription {
    receiver: mpsc::Receiver<Result<PubSubMessage>>,
    stop: CancellationToken,
}
impl RedisSubscription {
    pub async fn receive(&mut self) -> Option<Result<PubSubMessage>> {
        self.receiver.recv().await
    }
}
impl Drop for RedisSubscription {
    fn drop(&mut self) {
        self.stop.cancel();
    }
}
impl Drop for Inner {
    fn drop(&mut self) {
        self.stop.cancel();
    }
}
#[derive(Clone)]
pub struct RedisClient(Arc<Inner>);
enum Operation {
    Subscribe(String),
    Scan { cursor: u64, pattern: Vec<u8> },
    Inspect { key: Vec<u8>, page: PageRequest },
    Command(Vec<Vec<u8>>),
}
enum Response {
    Subscription(RedisSubscription),
    Keys(KeyPage),
    Key(KeyInfo),
    Value(RedisValue),
}
struct Request {
    database: u32,
    operation: Operation,
    deadline: Instant,
    reply: oneshot::Sender<Result<Response>>,
}
enum Route {
    Direct,
    Ssh {
        options: Box<crate::session::SshOptions>,
        trust: mpsc::Sender<crate::session::HostKeyChallenge>,
    },
}

struct CancelOnDrop(Option<CancellationToken>);
impl Drop for CancelOnDrop {
    fn drop(&mut self) {
        if let Some(stop) = &self.0 {
            stop.cancel();
        }
    }
}

fn runtime() -> Result<&'static Runtime> {
    static RUNTIME: OnceLock<std::result::Result<Runtime, std::io::Error>> = OnceLock::new();
    RUNTIME
        .get_or_init(|| {
            tokio::runtime::Builder::new_multi_thread()
                .worker_threads(1)
                .thread_name("rustterm-redis")
                .enable_all()
                .build()
        })
        .as_ref()
        .map_err(|_| RedisError::Runtime)
}

impl RedisClient {
    pub async fn subscribe(&self, pattern: String) -> Result<RedisSubscription> {
        if pattern.is_empty() || pattern.len() > 1024 {
            return Err(RedisError::InvalidCommand);
        }
        match self.request(0, Operation::Subscribe(pattern)).await? {
            Response::Subscription(subscription) => Ok(subscription),
            _ => unreachable!(),
        }
    }
    pub async fn slow_log(&self) -> Result<RedisValue> {
        match self
            .request(
                0,
                Operation::Command(vec![b"SLOWLOG".to_vec(), b"GET".to_vec(), b"100".to_vec()]),
            )
            .await?
        {
            Response::Value(value) => Ok(value),
            _ => unreachable!(),
        }
    }
    pub async fn publish(&self, channel: Vec<u8>, payload: Vec<u8>) -> Result<RedisValue> {
        if channel.is_empty() || channel.len() > 1024 || payload.len() > 65536 {
            return Err(RedisError::InvalidCommand);
        }
        match self
            .request(
                0,
                Operation::Command(vec![b"PUBLISH".to_vec(), channel, payload]),
            )
            .await?
        {
            Response::Value(value) => Ok(value),
            _ => unreachable!(),
        }
    }
    pub async fn connect(config: RedisConfig, secret: Option<Secret>) -> Result<Self> {
        // A missing launch resolver must never silently bypass a configured tunnel.
        if config.ssh_profile.is_some() {
            return Err(RedisError::InvalidConfig);
        }
        Self::connect_route(config, secret, None, Route::Direct).await
    }
    pub async fn connect_via_ssh(
        config: RedisConfig,
        secret: Option<Secret>,
        ssh: crate::session::SshOptions,
        trust: mpsc::Sender<crate::session::HostKeyChallenge>,
    ) -> Result<Self> {
        Self::connect_route(
            config,
            secret,
            None,
            Route::Ssh {
                options: Box::new(ssh),
                trust,
            },
        )
        .await
    }
    pub async fn connect_authenticated(
        config: RedisConfig,
        secret: Option<Secret>,
        sentinel_secret: Option<Secret>,
        ssh: Option<(
            crate::session::SshOptions,
            mpsc::Sender<crate::session::HostKeyChallenge>,
        )>,
    ) -> Result<Self> {
        let route = match ssh {
            Some((options, trust)) => Route::Ssh {
                options: Box::new(options),
                trust,
            },
            None => Route::Direct,
        };
        if config.ssh_profile.is_some() && matches!(route, Route::Direct) {
            return Err(RedisError::InvalidConfig);
        }
        Self::connect_route(config, secret, sentinel_secret, route).await
    }
    async fn connect_route(
        config: RedisConfig,
        secret: Option<Secret>,
        sentinel_secret: Option<Secret>,
        route: Route,
    ) -> Result<Self> {
        config.validate().map_err(|_| RedisError::InvalidConfig)?;
        if [secret.as_ref(), sentinel_secret.as_ref()]
            .into_iter()
            .flatten()
            .any(|secret| secret.expose().len() > MAX_REQUEST_BYTES)
        {
            return Err(RedisError::Limit);
        }
        let timeout = Duration::from_secs(config.timeout_secs);
        let stop = CancellationToken::new();
        let mut cancel = CancelOnDrop(Some(stop.clone()));
        let (queue, receive) = mpsc::channel(QUEUE_CAPACITY);
        let (send, connected) = oneshot::channel();
        let worker_stop = stop.clone();
        runtime()?.spawn(async move {
            let open = async {
                let tunnel = match route {
                    Route::Direct => None,
                    Route::Ssh { options, trust } => Some(
                        crate::session::connect_tunnel(*options, trust, worker_stop.child_token())
                            .await
                            .map_err(|(kind, _)| ssh_error(kind))?,
                    ),
                };
                tokio::time::timeout(
                    timeout,
                    router::Connection::open(config, secret, sentinel_secret, tunnel),
                )
                .await
                .unwrap_or(Err(RedisError::Timeout))
            };
            let opened = tokio::select! {
                biased;
                _ = worker_stop.cancelled() => Err(RedisError::Disconnected),
                result = open => result,
            };
            match opened {
                Ok(connection) => {
                    if send.send(Ok(())).is_ok() {
                        actor(connection, receive, worker_stop).await;
                    }
                }
                Err(error) => {
                    let _ = send.send(Err(error));
                    worker_stop.cancel();
                }
            }
        });
        connected.await.map_err(|_| RedisError::Disconnected)??;
        cancel.0 = None;
        Ok(Self(Arc::new(Inner {
            queue,
            stop,
            timeout,
        })))
    }
    /// Closes all clones. Pending operations are cancelled, with no write retry.
    pub fn close(&self) {
        self.0.stop.cancel();
    }
    pub fn is_closed(&self) -> bool {
        self.0.stop.is_cancelled() || self.0.queue.is_closed()
    }
    /// Event-driven notification for idle remote disconnects or explicit close.
    /// Dropping this observer never cancels the connection.
    pub async fn closed(&self) {
        self.0.stop.cancelled().await;
    }
    pub async fn scan(&self, database: u32, cursor: u64, pattern: &[u8]) -> Result<KeyPage> {
        check_bytes(pattern)?;
        match self
            .request(
                database,
                Operation::Scan {
                    cursor,
                    pattern: pattern.to_vec(),
                },
            )
            .await?
        {
            Response::Keys(page) => Ok(page),
            _ => unreachable!(),
        }
    }
    pub async fn inspect(&self, database: u32, key: &[u8], page: PageRequest) -> Result<KeyInfo> {
        check_bytes(key)?;
        if page.offset > i64::MAX as u64 || page.after.as_ref().is_some_and(|v| v.len() > 128) {
            return Err(RedisError::InvalidCommand);
        }
        match self
            .request(
                database,
                Operation::Inspect {
                    key: key.to_vec(),
                    page,
                },
            )
            .await?
        {
            Response::Key(info) => Ok(info),
            _ => unreachable!(),
        }
    }
    pub async fn command(&self, database: u32, args: Vec<Vec<u8>>) -> Result<RedisValue> {
        console::validate(&args)?;
        match self.request(database, Operation::Command(args)).await? {
            Response::Value(value) => Ok(value),
            _ => unreachable!(),
        }
    }
    async fn request(&self, database: u32, operation: Operation) -> Result<Response> {
        if database > i32::MAX as u32 {
            return Err(RedisError::InvalidConfig);
        }
        if self.is_closed() {
            return Err(RedisError::Disconnected);
        }
        let (reply, response) = oneshot::channel();
        let request = Request {
            database,
            operation,
            deadline: Instant::now() + self.0.timeout,
            reply,
        };
        self.0
            .queue
            .try_send(request)
            .map_err(|error| match error {
                mpsc::error::TrySendError::Full(_) => RedisError::Busy,
                mpsc::error::TrySendError::Closed(_) => RedisError::Disconnected,
            })?;
        // Cancelling the caller closes the underlying socket, including an in-flight write.
        let mut cancel = CancelOnDrop(Some(self.0.stop.clone()));
        let result = response.await.map_err(|_| RedisError::Disconnected)?;
        cancel.0 = None;
        result
    }
}

fn ssh_error(kind: crate::session::FailureKind) -> RedisError {
    use crate::session::FailureKind;
    match kind {
        FailureKind::Authentication => RedisError::SshAuthentication,
        FailureKind::HostKeyRejected => RedisError::SshHostKeyRejected,
        FailureKind::HostKeyMismatch => RedisError::SshHostKeyChanged,
        FailureKind::KnownHosts => RedisError::SshKnownHosts,
        FailureKind::PrivateKey => RedisError::SshPrivateKey,
        FailureKind::Timeout => RedisError::SshTimeout,
        _ => RedisError::SshConnection,
    }
}

async fn actor(
    mut connection: router::Connection,
    mut requests: mpsc::Receiver<Request>,
    stop: CancellationToken,
) {
    loop {
        let request = tokio::select! { biased; _ = stop.cancelled() => break, _ = connection.closed() => break, request = requests.recv() => {
            let Some(request) = request else { break }; request
        }};
        if request.reply.is_closed() {
            stop.cancel();
            break;
        }
        let result = tokio::select! {
            biased;
            _ = stop.cancelled() => Err(RedisError::Disconnected),
            result = tokio::time::timeout_at(request.deadline, operations::perform(&mut connection, request.database, request.operation)) =>
                result.unwrap_or(Err(RedisError::Timeout)),
        };
        let fatal = matches!(
            &result,
            Err(RedisError::Timeout
                | RedisError::Disconnected
                | RedisError::Protocol
                | RedisError::Limit)
        );
        let _ = request.reply.send(result);
        if fatal {
            break;
        }
    }
    stop.cancel();
    // Drop aborts the redis-rs driver immediately, even if a reply never arrives.
    drop(connection);
    requests.close();
    while let Some(request) = requests.recv().await {
        let _ = request.reply.send(Err(RedisError::Disconnected));
    }
}

fn check_bytes(bytes: &[u8]) -> Result<()> {
    if bytes.len() > MAX_REQUEST_BYTES {
        Err(RedisError::Limit)
    } else {
        Ok(())
    }
}
