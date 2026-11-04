use super::*;
use std::{
    future::Future,
    io::{Read, Write},
    net::{TcpListener, TcpStream},
    sync::{Mutex, mpsc as std_mpsc},
    task::{Context, Poll, Wake, Waker},
    thread,
};

/// Deliberately no Tokio context: matches a GPUI background executor caller.
fn block_on<F: Future>(future: F) -> F::Output {
    struct ThreadWake(thread::Thread);
    impl Wake for ThreadWake {
        fn wake(self: Arc<Self>) {
            self.0.unpark();
        }
    }
    let waker = Waker::from(Arc::new(ThreadWake(thread::current())));
    let mut cx = Context::from_waker(&waker);
    let mut future = std::pin::pin!(future);
    loop {
        match future.as_mut().poll(&mut cx) {
            Poll::Ready(result) => return result,
            Poll::Pending => thread::park(),
        }
    }
}
fn args(values: &[&[u8]]) -> Vec<Vec<u8>> {
    values.iter().map(|value| value.to_vec()).collect()
}
fn read_command(parser: &mut redis::Parser, socket: &mut TcpStream) -> Option<Vec<Vec<u8>>> {
    let redis::Value::Array(values) = parser.parse_value(socket).ok()? else {
        return None;
    };
    values
        .into_iter()
        .map(|value| match value {
            redis::Value::BulkString(bytes) => Some(bytes),
            _ => None,
        })
        .collect()
}
fn mock_listener() -> (TcpListener, RedisConfig) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let config = RedisConfig {
        port: listener.local_addr().unwrap().port(),
        timeout_secs: 1,
        ..Default::default()
    };
    (listener, config)
}
fn accept(listener: TcpListener) -> TcpStream {
    let (socket, _) = listener.accept().unwrap();
    socket
        .set_read_timeout(Some(Duration::from_secs(3)))
        .unwrap();
    socket
        .set_write_timeout(Some(Duration::from_secs(3)))
        .unwrap();
    socket
}

#[test]
fn mock_auth_database_scan_and_binary_responses_use_one_connection() {
    let (listener, config) = mock_listener();
    let commands = Arc::new(Mutex::new(Vec::new()));
    let recorded = commands.clone();
    let server = thread::spawn(move || {
        let mut socket = accept(listener);
        let mut parser = redis::Parser::new();
        while let Some(command) = read_command(&mut parser, &mut socket) {
            let reply: &[u8] = match command[0].as_slice() {
                b"AUTH" | b"SELECT" => b"+OK\r\n",
                b"SCAN" => b"*2\r\n$1\r\n0\r\n*1\r\n$2\r\n\xff\0\r\n",
                b"GET" => b"$3\r\na\0b\r\n",
                _ => b"-ERR unexpected command\r\n",
            };
            recorded.lock().unwrap().push(command);
            if socket.write_all(reply).is_err() {
                break;
            }
        }
    });
    let client = block_on(RedisClient::connect(
        config,
        Some(Secret::new("test-only-password".into())),
    ))
    .unwrap();
    let page = block_on(client.scan(3, 0, b"*")).unwrap();
    assert_eq!(page.keys, [vec![255, 0]]);
    assert_eq!(
        block_on(client.command(0, args(&[b"GET", b"key"]))).unwrap(),
        RedisValue::Bytes(b"a\0b".to_vec())
    );
    client.close();
    server.join().unwrap();
    let commands = commands.lock().unwrap();
    assert_eq!(
        commands
            .iter()
            .filter(|command| command[0] == b"AUTH")
            .count(),
        1
    );
    assert_eq!(
        commands
            .iter()
            .filter(|command| command[0] == b"SELECT")
            .map(|command| command[1].clone())
            .collect::<Vec<_>>(),
        [b"3".to_vec(), b"0".to_vec()]
    );
}

#[test]
fn explicit_acl_user_without_secret_sends_empty_password_auth() {
    let (listener, mut config) = mock_listener();
    config.username = Some("nopass-user".into());
    let server = thread::spawn(move || {
        let mut socket = accept(listener);
        let mut parser = redis::Parser::new();
        assert_eq!(
            read_command(&mut parser, &mut socket).unwrap(),
            args(&[b"AUTH", b"nopass-user", b""])
        );
        socket.write_all(b"+OK\r\n").unwrap();
        let mut byte = [0];
        assert_eq!(socket.read(&mut byte).unwrap(), 0);
    });
    let client = block_on(RedisClient::connect(config, None)).unwrap();
    client.close();
    server.join().unwrap();
}

#[test]
fn timed_out_write_disconnects_without_replay() {
    let (listener, config) = mock_listener();
    let server = thread::spawn(move || {
        let mut socket = accept(listener);
        let mut parser = redis::Parser::new();
        let command = read_command(&mut parser, &mut socket).unwrap();
        assert_eq!(command[0], b"SET");
        // Read after receiving the write but never acknowledge it.
        let mut byte = [0];
        assert_eq!(socket.read(&mut byte).unwrap(), 0);
    });
    let client = block_on(RedisClient::connect(config, None)).unwrap();
    assert_eq!(
        block_on(client.command(0, args(&[b"SET", b"key", b"value"]))),
        Err(RedisError::Timeout)
    );
    assert_eq!(
        block_on(client.command(0, args(&[b"PING"]))),
        Err(RedisError::Disconnected)
    );
    server.join().unwrap();
}

#[test]
fn dropping_an_in_flight_future_closes_socket_and_all_clones() {
    let (listener, config) = mock_listener();
    let (received, wait_received) = std_mpsc::channel();
    let server = thread::spawn(move || {
        let mut socket = accept(listener);
        let mut parser = redis::Parser::new();
        assert_eq!(read_command(&mut parser, &mut socket).unwrap()[0], b"GET");
        received.send(()).unwrap();
        let mut byte = [0];
        assert_eq!(socket.read(&mut byte).unwrap(), 0);
    });
    let client = block_on(RedisClient::connect(config, None)).unwrap();
    let clone = client.clone();
    let task = runtime()
        .unwrap()
        .spawn(async move { clone.command(0, args(&[b"GET", b"key"])).await });
    wait_received.recv_timeout(Duration::from_secs(2)).unwrap();
    task.abort();
    assert!(block_on(task).unwrap_err().is_cancelled());
    assert!(client.is_closed());
    server.join().unwrap();
}

#[test]
fn oversized_array_is_rejected_before_redis_parser_allocation() {
    let (listener, config) = mock_listener();
    let server = thread::spawn(move || {
        let mut socket = accept(listener);
        let mut parser = redis::Parser::new();
        assert_eq!(read_command(&mut parser, &mut socket).unwrap()[0], b"SCAN");
        socket.write_all(b"*9223372036854775807\r\n").unwrap();
        let mut byte = [0];
        assert_eq!(socket.read(&mut byte).unwrap(), 0);
    });
    let client = block_on(RedisClient::connect(config, None)).unwrap();
    assert_eq!(block_on(client.scan(0, 0, b"*")), Err(RedisError::Limit));
    server.join().unwrap();
}

#[test]
#[ignore = "manual local Redis smoke; set RUSTTERM_TEST_REDIS_ADDR and RUSTTERM_TEST_REDIS_PASSWORD"]
fn redis_live_smoke() {
    let address =
        std::env::var("RUSTTERM_TEST_REDIS_ADDR").expect("explicit Redis address required");
    let (host, port) = address.rsplit_once(':').expect("host:port");
    let password =
        std::env::var("RUSTTERM_TEST_REDIS_PASSWORD").expect("password-protected fixture required");
    let config = RedisConfig {
        host: host.into(),
        port: port.parse().unwrap(),
        timeout_secs: 3,
        ..Default::default()
    };
    assert!(matches!(
        block_on(RedisClient::connect(
            config.clone(),
            Some(Secret::new("wrong-fixture-password".into()))
        )),
        Err(RedisError::Authentication)
    ));
    let client = block_on(RedisClient::connect(config, Some(Secret::new(password)))).unwrap();
    let prefix = format!("rustterm-smoke-{}:", crate::config::Uuid::new_v4());
    let keys: Vec<Vec<u8>> = ["string", "hash", "list", "set", "zset", "stream", "renamed"]
        .iter()
        .map(|name| format!("{prefix}{name}").into_bytes())
        .collect();
    struct Cleanup(RedisClient, Vec<Vec<u8>>);
    impl Drop for Cleanup {
        fn drop(&mut self) {
            for db in [0, 1] {
                for key in &self.1 {
                    let _ = block_on(self.0.command(db, args(&[b"DEL", key])));
                }
            }
            self.0.close();
        }
    }
    let _cleanup = Cleanup(client.clone(), keys.clone());
    let command = |db, arguments| block_on(client.command(db, arguments)).unwrap();
    assert_eq!(
        command(0, args(&[b"SET", &keys[0], b"hello\0binary", b"NX"])),
        RedisValue::Bytes(b"OK".to_vec())
    );
    for (index, mutation) in [
        args(&[b"HSET", b"field", b"value"]),
        args(&[b"RPUSH", b"one", b"two"]),
        args(&[b"SADD", b"member"]),
        args(&[b"ZADD", b"1.5", b"member"]),
        args(&[b"XADD", b"*", b"field", b"value"]),
    ]
    .into_iter()
    .enumerate()
    {
        let mut create = args(&[b"EVAL", CREATE_SCRIPT.as_bytes(), b"1", &keys[index + 1]]);
        create.extend(mutation);
        assert_eq!(command(0, create.clone()), RedisValue::Integer(1));
        assert_eq!(command(0, create), RedisValue::Integer(0));
    }
    for (key, kind) in keys.iter().zip([
        KeyType::String,
        KeyType::Hash,
        KeyType::List,
        KeyType::Set,
        KeyType::ZSet,
        KeyType::Stream,
    ]) {
        let info = block_on(client.inspect(0, key, PageRequest::default())).unwrap();
        assert_eq!(info.kind, kind);
        assert!(info.length > 0);
        assert_eq!(info.ttl_ms, -1);
    }
    let mut cursor = 0;
    let mut found = Vec::new();
    for _ in 0..100 {
        let page = block_on(client.scan(0, cursor, format!("{prefix}*").as_bytes())).unwrap();
        found.extend(page.keys);
        cursor = page.next_cursor;
        if cursor == 0 {
            break;
        }
    }
    assert_eq!(cursor, 0);
    found.sort();
    found.dedup();
    assert_eq!(found.len(), 6);
    command(1, args(&[b"SET", &keys[0], b"other database", b"NX"]));
    assert_eq!(
        command(0, args(&[b"GET", &keys[0]])),
        RedisValue::Bytes(b"hello\0binary".to_vec())
    );
    assert_eq!(
        command(1, args(&[b"GET", &keys[0]])),
        RedisValue::Bytes(b"other database".to_vec())
    );
    assert_eq!(
        command(0, args(&[b"PEXPIRE", &keys[0], b"5000"])),
        RedisValue::Integer(1)
    );
    assert_eq!(
        command(
            0,
            args(&[
                b"EVAL",
                SAVE_STRING_SCRIPT.as_bytes(),
                b"1",
                &keys[0],
                b"stale value",
                b"edited"
            ])
        ),
        RedisValue::Integer(0)
    );
    assert_eq!(
        command(
            0,
            args(&[
                b"EVAL",
                SAVE_STRING_SCRIPT.as_bytes(),
                b"1",
                &keys[0],
                b"hello\0binary",
                b"edited"
            ])
        ),
        RedisValue::Integer(1)
    );
    let ttl = block_on(client.inspect(0, &keys[0], PageRequest::default()))
        .unwrap()
        .ttl_ms;
    assert!(ttl > 0 && ttl <= 5000);
    command(0, args(&[b"HSET", &keys[1], b"field", b"edited"]));
    let save_list = |key: &[u8], index: &[u8], original: &[u8]| {
        args(&[
            b"EVAL",
            SAVE_LIST_SCRIPT.as_bytes(),
            b"1",
            key,
            index,
            original,
            b"edited",
        ])
    };
    // A concurrent push shifts the editor's old index. Reject it atomically,
    // without modifying the newly inserted element or creating missing keys.
    command(0, args(&[b"LPUSH", &keys[2], b"new head"]));
    assert_eq!(
        command(0, save_list(&keys[2], b"0", b"one")),
        RedisValue::Integer(0)
    );
    assert_eq!(
        command(0, args(&[b"LINDEX", &keys[2], b"0"])),
        RedisValue::Bytes(b"new head".to_vec())
    );
    assert_eq!(
        command(0, save_list(&keys[2], b"1", b"one")),
        RedisValue::Integer(1)
    );
    assert_eq!(
        command(0, args(&[b"LINDEX", &keys[2], b"1"])),
        RedisValue::Bytes(b"edited".to_vec())
    );
    assert_eq!(
        command(0, save_list(&keys[2], b"99", b"")),
        RedisValue::Integer(0)
    );
    assert_eq!(
        command(0, save_list(&keys[0], b"0", b"edited")),
        RedisValue::Integer(0)
    );
    assert_eq!(
        command(0, save_list(&keys[6], b"0", b"")),
        RedisValue::Integer(0)
    );
    assert_eq!(
        command(0, args(&[b"EXISTS", &keys[6]])),
        RedisValue::Integer(0)
    );
    command(0, args(&[b"SADD", &keys[3], b"edited"]));
    command(0, args(&[b"ZADD", &keys[4], b"2.5", b"edited"]));
    command(0, args(&[b"XADD", &keys[5], b"*", b"field", b"edited"]));
    assert_eq!(
        command(0, args(&[b"RENAMENX", &keys[0], &keys[1]])),
        RedisValue::Integer(0)
    );
    assert_eq!(
        command(0, args(&[b"RENAMENX", &keys[0], &keys[6]])),
        RedisValue::Integer(1)
    );
    assert_eq!(
        command(0, args(&[b"DEL", &keys[6]])),
        RedisValue::Integer(1)
    );
}
