use super::{KeyPage, MAX_PAGE_ITEMS, PAGE_SIZE, RedisError, Result, transport};
use crate::config::{RedisConfig, RedisEndpoint, RedisTopology, Secret};
use crate::session::TunnelConnector;
use redis::Value;
use std::collections::{BTreeMap, HashMap};

const MAX_NODES: usize = 256;
const MAX_REDIRECTS: usize = 5;

pub(super) struct Connection {
    config: RedisConfig,
    secret: Option<Secret>,
    sentinel_secret: Option<Secret>,
    tunnel: Option<TunnelConnector>,
    nodes: HashMap<RedisEndpoint, transport::Connection>,
    primary: RedisEndpoint,
    slots: Vec<RedisEndpoint>,
    masters: Vec<RedisEndpoint>,
    scans: BTreeMap<u64, (usize, u64)>,
    next_scan: u64,
    subscriptions_stop: tokio_util::sync::CancellationToken,
    subscriptions: std::sync::Arc<tokio::sync::Semaphore>,
}

impl Drop for Connection {
    fn drop(&mut self) {
        self.subscriptions_stop.cancel();
    }
}

impl Connection {
    pub async fn open(
        config: RedisConfig,
        secret: Option<Secret>,
        sentinel_secret: Option<Secret>,
        tunnel: Option<TunnelConnector>,
    ) -> Result<Self> {
        let primary = RedisEndpoint {
            host: config.host.clone(),
            port: config.port,
        };
        let mut this = Self {
            config,
            secret,
            sentinel_secret,
            tunnel,
            nodes: HashMap::new(),
            primary,
            slots: Vec::new(),
            masters: Vec::new(),
            scans: BTreeMap::new(),
            next_scan: 1,
            subscriptions_stop: tokio_util::sync::CancellationToken::new(),
            subscriptions: std::sync::Arc::new(tokio::sync::Semaphore::new(8)),
        };
        match this.config.topology {
            RedisTopology::Standalone => {
                this.ensure(this.primary.clone()).await?;
            }
            RedisTopology::Cluster => {
                this.discover_cluster().await?;
            }
            RedisTopology::Sentinel => {
                this.discover_primary().await?;
            }
        }
        Ok(this)
    }

    #[cfg(test)]
    pub async fn over_stream<
        S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin + Send + 'static,
    >(
        config: RedisConfig,
        secret: Option<Secret>,
        stream: S,
    ) -> Result<Self> {
        let primary = RedisEndpoint {
            host: config.host.clone(),
            port: config.port,
        };
        let node = transport::Connection::over_stream(config.clone(), secret, stream).await?;
        Ok(Self {
            config,
            secret: None,
            sentinel_secret: None,
            tunnel: None,
            nodes: HashMap::from([(primary.clone(), node)]),
            primary,
            slots: Vec::new(),
            masters: Vec::new(),
            scans: BTreeMap::new(),
            next_scan: 1,
            subscriptions_stop: tokio_util::sync::CancellationToken::new(),
            subscriptions: std::sync::Arc::new(tokio::sync::Semaphore::new(8)),
        })
    }

    fn seeds(&self) -> Vec<RedisEndpoint> {
        let mut seeds = vec![RedisEndpoint {
            host: self.config.host.clone(),
            port: self.config.port,
        }];
        for seed in &self.config.nodes {
            if !seeds.contains(seed) {
                seeds.push(seed.clone());
            }
        }
        seeds
    }

    pub async fn subscribe(&mut self, pattern: String) -> Result<super::RedisSubscription> {
        let permit = self
            .subscriptions
            .clone()
            .try_acquire_owned()
            .map_err(|_| RedisError::Busy)?;
        let mut config = self.config.clone();
        config.host = self.primary.host.clone();
        config.port = self.primary.port;
        let mut subscription = if let Some(tunnel) = &self.tunnel {
            let stream = tunnel
                .open(config.host.clone(), config.port)
                .await
                .map_err(|(kind, _)| super::ssh_error(kind))?;
            transport::Subscription::open(&config, self.secret.as_ref(), stream, &pattern).await?
        } else {
            let stream = tokio::net::TcpStream::connect((config.host.as_str(), config.port))
                .await
                .map_err(|_| RedisError::Connection)?;
            transport::Subscription::open(&config, self.secret.as_ref(), stream, &pattern).await?
        };
        let stop = self.subscriptions_stop.child_token();
        let worker_stop = stop.clone();
        let (sender, receiver) = tokio::sync::mpsc::channel(32);
        tokio::spawn(async move {
            let _permit = permit;
            loop {
                let result = tokio::select! { _ = worker_stop.cancelled() => break, result = subscription.receive() => result };
                let failed = result.is_err();
                tokio::select! { _ = worker_stop.cancelled() => break, sent = sender.send(result) => { if sent.is_err() || failed { break; } } }
            }
        });
        Ok(super::RedisSubscription { receiver, stop })
    }

    async fn connect_node(
        &self,
        endpoint: &RedisEndpoint,
        sentinel: bool,
    ) -> Result<transport::Connection> {
        let mut config = self.config.clone();
        config.host = endpoint.host.clone();
        config.port = endpoint.port;
        let secret = if sentinel {
            config.username = self.config.sentinel_username.clone();
            config.database = 0;
            &self.sentinel_secret
        } else {
            &self.secret
        };
        let secret = secret
            .as_ref()
            .map(|secret| Secret::new(secret.expose().to_owned()));
        if let Some(tunnel) = &self.tunnel {
            let stream = tunnel
                .open(endpoint.host.clone(), endpoint.port)
                .await
                .map_err(|(kind, _)| super::ssh_error(kind))?;
            transport::Connection::over_stream(config, secret, stream).await
        } else {
            transport::Connection::open(config, secret).await
        }
    }

    async fn ensure(&mut self, endpoint: RedisEndpoint) -> Result<()> {
        if self
            .nodes
            .get(&endpoint)
            .is_some_and(|node| !node.is_closed())
        {
            return Ok(());
        }
        if self.nodes.len() >= MAX_NODES && !self.nodes.contains_key(&endpoint) {
            return Err(RedisError::Limit);
        }
        let connection = self.connect_node(&endpoint, false).await?;
        self.nodes.insert(endpoint, connection);
        Ok(())
    }

    async fn discover_cluster(&mut self) -> Result<()> {
        let mut last = RedisError::Topology;
        let mut candidates = self.seeds();
        for node in self.masters.iter().chain(self.nodes.keys()) {
            if !candidates.contains(node) && candidates.len() < MAX_NODES {
                candidates.push(node.clone());
            }
        }
        for seed in candidates {
            if let Err(error) = self.ensure(seed.clone()).await {
                last = error;
                continue;
            }
            match self
                .nodes
                .get_mut(&seed)
                .unwrap()
                .query(vec![b"CLUSTER".to_vec(), b"SLOTS".to_vec()])
                .await
            {
                Ok(value) => {
                    let (slots, masters) = parse_slots(value, &seed.host)?;
                    self.slots = slots;
                    self.masters = masters;
                    self.primary = seed;
                    return Ok(());
                }
                Err(error) => last = error,
            }
        }
        Err(last)
    }

    async fn discover_primary(&mut self) -> Result<()> {
        let service = self
            .config
            .sentinel_service
            .clone()
            .ok_or(RedisError::InvalidConfig)?;
        let mut last = RedisError::Topology;
        for seed in self.seeds() {
            let mut sentinel = match self.connect_node(&seed, true).await {
                Ok(node) => node,
                Err(error) => {
                    last = error;
                    continue;
                }
            };
            let response = sentinel
                .query(vec![
                    b"SENTINEL".to_vec(),
                    b"get-master-addr-by-name".to_vec(),
                    service.as_bytes().to_vec(),
                ])
                .await;
            let endpoint = match response.and_then(parse_primary) {
                Ok(endpoint) => endpoint,
                Err(error) => {
                    last = error;
                    continue;
                }
            };
            if let Err(error) = self.ensure(endpoint.clone()).await {
                last = error;
                continue;
            }
            match self
                .nodes
                .get_mut(&endpoint)
                .unwrap()
                .query(vec![b"ROLE".to_vec()])
                .await
            {
                Ok(role) if is_primary(&role) => {}
                Ok(_) => {
                    last = RedisError::Topology;
                    continue;
                }
                Err(error) => {
                    last = error;
                    continue;
                }
            }
            self.primary = endpoint.clone();
            self.nodes.retain(|address, _| *address == endpoint);
            return Ok(());
        }
        Err(last)
    }

    pub async fn closed(&mut self) {
        if self.config.topology == RedisTopology::Standalone {
            if let Some(node) = self.nodes.get_mut(&self.primary) {
                node.closed().await;
            }
        } else {
            // A cluster node closing does not close unrelated nodes. Each next
            // operation reconnects before submission; uncertain writes never retry.
            std::future::pending::<()>().await;
        }
    }

    pub async fn select(&mut self, database: u32) -> Result<()> {
        if self.config.topology == RedisTopology::Cluster {
            return if database == 0 {
                Ok(())
            } else {
                Err(RedisError::InvalidConfig)
            };
        }
        if self.config.topology == RedisTopology::Sentinel {
            let live = self
                .nodes
                .get(&self.primary)
                .is_some_and(|node| !node.is_closed());
            if !live {
                self.discover_primary().await?;
            }
            let role = self
                .nodes
                .get_mut(&self.primary)
                .unwrap()
                .query(vec![b"ROLE".to_vec()])
                .await;
            if !matches!(role,Ok(role) if is_primary(&role)) {
                self.discover_primary().await?;
            }
        }
        self.nodes
            .get_mut(&self.primary)
            .ok_or(RedisError::Disconnected)?
            .select(database)
            .await?;
        self.config.database = database;
        Ok(())
    }

    pub async fn query(&mut self, args: Vec<Vec<u8>>) -> Result<Value> {
        let slot = if self.config.topology == RedisTopology::Cluster {
            command_key(&args).map(hash_slot)
        } else {
            None
        };
        let mut target = slot
            .and_then(|slot| self.slots.get(usize::from(slot)))
            .cloned()
            .unwrap_or_else(|| self.primary.clone());
        let mut asking = false;
        for _ in 0..MAX_REDIRECTS {
            if let Err(error) = self.ensure(target.clone()).await {
                if self.config.topology != RedisTopology::Cluster
                    || !matches!(
                        error,
                        RedisError::Connection
                            | RedisError::Disconnected
                            | RedisError::Timeout
                            | RedisError::SshConnection
                            | RedisError::SshTimeout
                    )
                {
                    return Err(error);
                }
                self.discover_cluster().await?;
                target = slot
                    .and_then(|slot| self.slots.get(usize::from(slot)))
                    .cloned()
                    .unwrap_or_else(|| self.primary.clone());
                asking = false;
                continue;
            }
            let node = self.nodes.get_mut(&target).unwrap();
            if asking {
                node.query(vec![b"ASKING".to_vec()]).await?;
            }
            match node.query(args.clone()).await {
                Err(RedisError::Redirect {
                    ask,
                    slot,
                    endpoint,
                }) if self.config.topology == RedisTopology::Cluster => {
                    if !ask {
                        self.slots[usize::from(slot)] = endpoint.clone();
                    }
                    target = endpoint;
                    asking = ask;
                }
                Err(RedisError::ReadOnly) if self.config.topology == RedisTopology::Sentinel => {
                    // READONLY explicitly rejects execution, unlike a lost reply.
                    self.discover_primary().await?;
                    target = self.primary.clone();
                }
                result => return result,
            }
        }
        Err(RedisError::Topology)
    }

    pub async fn scan(&mut self, cursor: u64, pattern: Vec<u8>) -> Result<KeyPage> {
        if self.config.topology != RedisTopology::Cluster {
            return parse_scan(self.query(scan_command(cursor, &pattern)).await?);
        }
        if cursor == 0 {
            self.discover_cluster().await?;
            self.scans.clear();
        }
        let (mut index, node_cursor) = if cursor == 0 {
            (0, 0)
        } else {
            *self.scans.get(&cursor).ok_or(RedisError::InvalidCommand)?
        };
        let endpoint = self
            .masters
            .get(index)
            .cloned()
            .ok_or(RedisError::Topology)?;
        self.ensure(endpoint.clone()).await?;
        let mut page = parse_scan(
            self.nodes
                .get_mut(&endpoint)
                .unwrap()
                .query(scan_command(node_cursor, &pattern))
                .await?,
        )?;
        if page.next_cursor == 0 {
            index += 1;
        }
        if index < self.masters.len() {
            let token = self.next_scan;
            self.next_scan = self.next_scan.checked_add(1).ok_or(RedisError::Limit)?;
            self.scans.insert(token, (index, page.next_cursor));
            if self.scans.len() > 1024 {
                self.scans.pop_first();
            }
            page.next_cursor = token;
        }
        Ok(page)
    }
}

fn scan_command(cursor: u64, pattern: &[u8]) -> Vec<Vec<u8>> {
    vec![
        b"SCAN".to_vec(),
        cursor.to_string().into_bytes(),
        b"MATCH".to_vec(),
        if pattern.is_empty() {
            b"*".to_vec()
        } else {
            pattern.to_vec()
        },
        b"COUNT".to_vec(),
        PAGE_SIZE.to_string().into_bytes(),
    ]
}
fn parse_scan(value: Value) -> Result<KeyPage> {
    let Value::Array(mut values) = value else {
        return Err(RedisError::Protocol);
    };
    if values.len() != 2 {
        return Err(RedisError::Protocol);
    }
    let Value::Array(keys) = values.pop().unwrap() else {
        return Err(RedisError::Protocol);
    };
    if keys.len() > MAX_PAGE_ITEMS {
        return Err(RedisError::Limit);
    }
    let next_cursor = string(&values[0])?
        .parse()
        .map_err(|_| RedisError::Protocol)?;
    let keys = keys
        .into_iter()
        .map(|key| match key {
            Value::BulkString(bytes) => Ok(bytes),
            _ => Err(RedisError::Protocol),
        })
        .collect::<Result<_>>()?;
    Ok(KeyPage { keys, next_cursor })
}
fn string(value: &Value) -> Result<&str> {
    match value {
        Value::BulkString(bytes) => std::str::from_utf8(bytes).map_err(|_| RedisError::Protocol),
        Value::SimpleString(text) => Ok(text),
        _ => Err(RedisError::Protocol),
    }
}
fn parse_primary(value: Value) -> Result<RedisEndpoint> {
    let Value::Array(values) = value else {
        return Err(RedisError::Topology);
    };
    if values.len() != 2 {
        return Err(RedisError::Protocol);
    }
    let host = crate::config::known_hosts::normalize_host(string(&values[0])?)
        .map_err(|_| RedisError::Protocol)?;
    let port = string(&values[1])?
        .parse::<u16>()
        .ok()
        .filter(|port| *port != 0)
        .ok_or(RedisError::Protocol)?;
    Ok(RedisEndpoint { host, port })
}
fn is_primary(value: &Value) -> bool {
    matches!(value, Value::Array(values) if values.first().is_some_and(|value| string(value) == Ok("master")))
}
fn parse_slots(value: Value, seed_host: &str) -> Result<(Vec<RedisEndpoint>, Vec<RedisEndpoint>)> {
    let Value::Array(ranges) = value else {
        return Err(RedisError::Protocol);
    };
    let mut slots = vec![None; 16384];
    let mut masters = Vec::new();
    for range in ranges {
        let Value::Array(fields) = range else {
            return Err(RedisError::Protocol);
        };
        let [Value::Int(start), Value::Int(end), Value::Array(node), ..] = fields.as_slice() else {
            return Err(RedisError::Protocol);
        };
        if *start < 0 || *end >= 16384 || end < start {
            return Err(RedisError::Protocol);
        }
        let [address, Value::Int(port), ..] = node.as_slice() else {
            return Err(RedisError::Protocol);
        };
        let host = if matches!(address, Value::Nil) || string(address)?.is_empty() {
            seed_host.to_owned()
        } else {
            crate::config::known_hosts::normalize_host(string(address)?)
                .map_err(|_| RedisError::Protocol)?
        };
        let port = u16::try_from(*port)
            .ok()
            .filter(|port| *port != 0)
            .ok_or(RedisError::Protocol)?;
        let endpoint = RedisEndpoint { host, port };
        if !masters.contains(&endpoint) {
            masters.push(endpoint.clone());
        }
        if masters.len() > MAX_NODES {
            return Err(RedisError::Limit);
        }
        for slot in &mut slots[*start as usize..=*end as usize] {
            if slot.replace(endpoint.clone()).is_some() {
                return Err(RedisError::Protocol);
            }
        }
    }
    let slots = slots
        .into_iter()
        .collect::<Option<Vec<_>>>()
        .ok_or(RedisError::Topology)?;
    Ok((slots, masters))
}
fn command_key(args: &[Vec<u8>]) -> Option<&[u8]> {
    let name = args.first()?;
    if [
        b"PING".as_slice(),
        b"ECHO",
        b"INFO",
        b"TIME",
        b"DBSIZE",
        b"SCAN",
        b"SLOWLOG",
        b"PUBLISH",
    ]
    .iter()
    .any(|command| name.eq_ignore_ascii_case(command))
    {
        return None;
    }
    if name.eq_ignore_ascii_case(b"EVAL") {
        args.get(3).map(Vec::as_slice)
    } else {
        args.get(1).map(Vec::as_slice)
    }
}
fn hash_slot(key: &[u8]) -> u16 {
    let key = key
        .iter()
        .position(|byte| *byte == b'{')
        .and_then(|start| {
            key[start + 1..]
                .iter()
                .position(|byte| *byte == b'}')
                .filter(|end| *end > 0)
                .map(|end| &key[start + 1..start + 1 + end])
        })
        .unwrap_or(key);
    let mut crc = 0_u16;
    for byte in key {
        crc ^= u16::from(*byte) << 8;
        for _ in 0..8 {
            crc = if crc & 0x8000 != 0 {
                (crc << 1) ^ 0x1021
            } else {
                crc << 1
            };
        }
    }
    crc % 16384
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn redis_hash_tags_and_crc_match_the_specification() {
        assert_eq!(hash_slot(b"123456789"), 0x31c3);
        assert_eq!(hash_slot(b"{user}:name"), hash_slot(b"{user}:email"));
        assert_eq!(hash_slot(b"{}x{user}"), hash_slot(b"{}x{user}"));
        assert_ne!(hash_slot(b"user:one"), hash_slot(b"user:two"));
    }
    #[test]
    fn slot_ranges_must_cover_cluster_without_overlap() {
        let node = || {
            Value::Array(vec![
                Value::BulkString(b"localhost".to_vec()),
                Value::Int(6379),
            ])
        };
        let valid = Value::Array(vec![Value::Array(vec![
            Value::Int(0),
            Value::Int(16383),
            node(),
        ])]);
        assert_eq!(parse_slots(valid, "seed").unwrap().0.len(), 16384);
        assert!(
            parse_slots(
                Value::Array(vec![Value::Array(vec![
                    Value::Int(0),
                    Value::Int(100),
                    node()
                ])]),
                "seed"
            )
            .is_err()
        );
    }
    struct Node {
        endpoint: RedisEndpoint,
        task: tokio::task::JoinHandle<()>,
    }
    impl Drop for Node {
        fn drop(&mut self) {
            self.task.abort();
        }
    }
    async fn node(handler: impl Fn(Vec<Vec<u8>>) -> Vec<u8> + Send + Sync + 'static) -> Node {
        use futures_util::{SinkExt, StreamExt};
        let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0))
            .await
            .unwrap();
        let endpoint = RedisEndpoint {
            host: "127.0.0.1".into(),
            port: listener.local_addr().unwrap().port(),
        };
        let handler = std::sync::Arc::new(handler);
        let task = tokio::spawn(async move {
            let mut clients = tokio::task::JoinSet::new();
            loop {
                tokio::select! {
                    incoming=listener.accept()=>{
                        let (stream,_)=incoming.unwrap();let handler=handler.clone();
                        clients.spawn(async move {
                            let mut framed=tokio_util::codec::Framed::new(stream,redis::ValueCodec::default());
                            while let Some(Ok(Value::Array(args)))=framed.next().await {
                                let args=args.into_iter().map(|value|match value {Value::BulkString(value)=>value,_=>panic!("command arguments must be strings")}).collect();
                                if framed.send(handler(args)).await.is_err(){break;}
                            }
                        });
                    },
                    _=clients.join_next(),if !clients.is_empty()=>{},
                }
            }
        });
        Node { endpoint, task }
    }
    fn slots(port: u16) -> Vec<u8> {
        format!("*1\r\n*3\r\n:0\r\n:16383\r\n*2\r\n$9\r\n127.0.0.1\r\n:{port}\r\n").into_bytes()
    }
    #[tokio::test]
    async fn moved_updates_slot_but_ask_remains_temporary_and_precedes_command() {
        tokio::time::timeout(std::time::Duration::from_secs(3), async {
            for ask in [false, true] {
                let commands = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
                let recorded = commands.clone();
                let target = node(move |args| {
                    recorded.lock().unwrap().push(args[0].clone());
                    if args[0] == b"ASKING" {
                        b"+OK\r\n".to_vec()
                    } else {
                        b"$5\r\nvalue\r\n".to_vec()
                    }
                })
                .await;
                let target_port = target.endpoint.port;
                let own_port = std::sync::Arc::new(std::sync::atomic::AtomicU16::new(0));
                let own = own_port.clone();
                let calls = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
                let redirects = calls.clone();
                let seed = node(move |args| {
                    if args[0] == b"CLUSTER" {
                        slots(own.load(std::sync::atomic::Ordering::Relaxed))
                    } else {
                        redirects.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                        format!(
                            "-{} {} 127.0.0.1:{target_port}\r\n",
                            if ask { "ASK" } else { "MOVED" },
                            hash_slot(b"key")
                        )
                        .into_bytes()
                    }
                })
                .await;
                own_port.store(seed.endpoint.port, std::sync::atomic::Ordering::Relaxed);
                let config = RedisConfig {
                    host: seed.endpoint.host.clone(),
                    port: seed.endpoint.port,
                    topology: RedisTopology::Cluster,
                    ..Default::default()
                };
                let mut connection = Connection::open(config, None, None, None).await.unwrap();
                for _ in 0..2 {
                    assert_eq!(
                        connection
                            .query(vec![b"GET".to_vec(), b"key".to_vec()])
                            .await
                            .unwrap(),
                        Value::BulkString(b"value".to_vec())
                    );
                }
                assert_eq!(
                    calls.load(std::sync::atomic::Ordering::Relaxed),
                    if ask { 2 } else { 1 }
                );
                let expected = if ask {
                    vec![
                        b"ASKING".to_vec(),
                        b"GET".to_vec(),
                        b"ASKING".to_vec(),
                        b"GET".to_vec(),
                    ]
                } else {
                    vec![b"GET".to_vec(), b"GET".to_vec()]
                };
                assert_eq!(*commands.lock().unwrap(), expected);
            }
        })
        .await
        .unwrap();
    }
    #[tokio::test]
    async fn sentinel_rediscovers_primary_before_submitting_next_write() {
        use std::sync::{
            Arc,
            atomic::{AtomicBool, AtomicUsize, Ordering},
        };
        tokio::time::timeout(std::time::Duration::from_secs(3), async {
            let failed = Arc::new(AtomicBool::new(false));
            let old_failed = failed.clone();
            let old_writes = Arc::new(AtomicUsize::new(0));
            let writes = old_writes.clone();
            let old = node(move |args| {
                if args[0] == b"ROLE" {
                    if old_failed.load(Ordering::Relaxed) {
                        b"-ERR primary unavailable\r\n".to_vec()
                    } else {
                        b"*1\r\n$6\r\nmaster\r\n".to_vec()
                    }
                } else {
                    writes.fetch_add(1, Ordering::Relaxed);
                    b"+OK\r\n".to_vec()
                }
            })
            .await;
            let new_writes = Arc::new(AtomicUsize::new(0));
            let writes = new_writes.clone();
            let new = node(move |args| {
                if args[0] == b"ROLE" {
                    b"*1\r\n$6\r\nmaster\r\n".to_vec()
                } else {
                    writes.fetch_add(1, Ordering::Relaxed);
                    b"+OK\r\n".to_vec()
                }
            })
            .await;
            let old_port = old.endpoint.port;
            let new_port = new.endpoint.port;
            let switched = failed.clone();
            let sentinel = node(move |args| {
                assert_eq!(args[0], b"SENTINEL");
                let port = if switched.load(Ordering::Relaxed) {
                    new_port
                } else {
                    old_port
                }
                .to_string();
                format!("*2\r\n$9\r\n127.0.0.1\r\n${}\r\n{port}\r\n", port.len()).into_bytes()
            })
            .await;
            let config = RedisConfig {
                host: sentinel.endpoint.host.clone(),
                port: sentinel.endpoint.port,
                topology: RedisTopology::Sentinel,
                sentinel_service: Some("service".into()),
                ..Default::default()
            };
            let mut connection = Connection::open(config, None, None, None).await.unwrap();
            connection.select(0).await.unwrap();
            connection
                .query(vec![b"SET".to_vec(), b"key".to_vec(), b"first".to_vec()])
                .await
                .unwrap();
            failed.store(true, Ordering::Relaxed);
            connection.select(0).await.unwrap();
            connection
                .query(vec![b"SET".to_vec(), b"key".to_vec(), b"second".to_vec()])
                .await
                .unwrap();
            assert_eq!(old_writes.load(Ordering::Relaxed), 1);
            assert_eq!(new_writes.load(Ordering::Relaxed), 1);
        })
        .await
        .unwrap();
    }
}
