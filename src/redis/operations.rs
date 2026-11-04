use super::{
    KeyData, KeyInfo, KeyType, MAX_PAGE_ITEMS, Operation, PAGE_SIZE, PageRequest, RedisError,
    RedisValue, Response, Result, STRING_PAGE_BYTES, StreamEntry, router::Connection,
};
use redis::Value;

pub(super) async fn perform(
    connection: &mut Connection,
    database: u32,
    operation: Operation,
) -> Result<Response> {
    connection.select(database).await?;
    match operation {
        Operation::Subscribe(pattern) => {
            Ok(Response::Subscription(connection.subscribe(pattern).await?))
        }
        Operation::Command(args) => Ok(Response::Value(value(connection.query(args).await?)?)),
        Operation::Scan { cursor, pattern } => {
            Ok(Response::Keys(connection.scan(cursor, pattern).await?))
        }
        Operation::Inspect { key, page } => {
            Ok(Response::Key(inspect(connection, &key, page).await?))
        }
    }
}

async fn inspect(connection: &mut Connection, key: &[u8], page: PageRequest) -> Result<KeyInfo> {
    let kind = match bytes(connection.query(command("TYPE", key)).await?)?.as_slice() {
        b"none" => KeyType::Missing,
        b"string" => KeyType::String,
        b"hash" => KeyType::Hash,
        b"list" => KeyType::List,
        b"set" => KeyType::Set,
        b"zset" => KeyType::ZSet,
        b"stream" => KeyType::Stream,
        _ => KeyType::Unsupported,
    };
    let ttl_ms = if kind == KeyType::Missing {
        -2
    } else {
        integer(connection.query(command("PTTL", key)).await?)?
    };
    if ttl_ms == -2 {
        return Ok(KeyInfo {
            kind: KeyType::Missing,
            ttl_ms: -2,
            length: 0,
            data: KeyData::Missing,
            next: None,
        });
    }
    let length_command = match kind {
        KeyType::String => "STRLEN",
        KeyType::Hash => "HLEN",
        KeyType::List => "LLEN",
        KeyType::Set => "SCARD",
        KeyType::ZSet => "ZCARD",
        KeyType::Stream => "XLEN",
        _ => "",
    };
    let length = if length_command.is_empty() {
        0
    } else {
        u64::try_from(integer(
            connection.query(command(length_command, key)).await?,
        )?)
        .map_err(|_| RedisError::Protocol)?
    };
    let mut next = None;
    let data = match kind {
        KeyType::String | KeyType::List => {
            let count = if kind == KeyType::String {
                STRING_PAGE_BYTES
            } else {
                PAGE_SIZE
            } as u64;
            let end = page
                .offset
                .checked_add(count - 1)
                .filter(|n| *n <= i64::MAX as u64)
                .ok_or(RedisError::InvalidCommand)?;
            let mut args = command(
                if kind == KeyType::String {
                    "GETRANGE"
                } else {
                    "LRANGE"
                },
                key,
            );
            args.extend([decimal(page.offset), decimal(end)]);
            let response = connection.query(args).await?;
            let data = if kind == KeyType::String {
                KeyData::String(bytes(response)?)
            } else {
                KeyData::List(
                    array(response)?
                        .into_iter()
                        .map(bytes)
                        .collect::<Result<_>>()?,
                )
            };
            if end + 1 < length {
                next = Some(PageRequest {
                    offset: end + 1,
                    ..Default::default()
                });
            }
            data
        }
        KeyType::Hash | KeyType::Set | KeyType::ZSet => {
            let name = match kind {
                KeyType::Hash => "HSCAN",
                KeyType::Set => "SSCAN",
                _ => "ZSCAN",
            };
            let mut args = command(name, key);
            args.extend([
                decimal(page.cursor),
                b"COUNT".to_vec(),
                decimal(PAGE_SIZE as u64),
            ]);
            let (cursor, entries) = scan_result(connection.query(args).await?)?;
            if cursor != 0 {
                next = Some(PageRequest {
                    cursor,
                    ..Default::default()
                });
            }
            if kind == KeyType::Set {
                if entries.len() > MAX_PAGE_ITEMS {
                    return Err(RedisError::Limit);
                }
                KeyData::Set(entries.into_iter().map(bytes).collect::<Result<_>>()?)
            } else {
                let pairs = pairs(entries)?;
                if kind == KeyType::Hash {
                    KeyData::Hash(pairs)
                } else {
                    KeyData::ZSet(pairs)
                }
            }
        }
        KeyType::Stream => {
            let start = if let Some(after) = &page.after {
                // Validate before constructing Redis 6.2+'s exclusive range bound.
                if after.is_empty()
                    || !after
                        .iter()
                        .all(|byte| byte.is_ascii_digit() || *byte == b'-')
                {
                    return Err(RedisError::InvalidCommand);
                }
                let mut start = vec![b'('];
                start.extend(after);
                start
            } else {
                b"-".to_vec()
            };
            let mut args = command("XRANGE", key);
            args.extend([
                start,
                b"+".to_vec(),
                b"COUNT".to_vec(),
                decimal(PAGE_SIZE as u64),
            ]);
            let entries = array(connection.query(args).await?)?;
            if entries.len() > MAX_PAGE_ITEMS {
                return Err(RedisError::Limit);
            }
            let entries = entries
                .into_iter()
                .map(|entry| {
                    let mut fields = array(entry)?.into_iter();
                    let id = bytes(fields.next().ok_or(RedisError::Protocol)?)?;
                    let values = pairs(array(fields.next().ok_or(RedisError::Protocol)?)?)?;
                    if fields.next().is_some() {
                        return Err(RedisError::Protocol);
                    }
                    Ok(StreamEntry { id, fields: values })
                })
                .collect::<Result<Vec<_>>>()?;
            if entries.len() == PAGE_SIZE {
                next = entries.last().map(|entry| PageRequest {
                    after: Some(entry.id.clone()),
                    ..Default::default()
                });
            }
            KeyData::Stream(entries)
        }
        KeyType::Missing => KeyData::Missing,
        KeyType::Unsupported => KeyData::Unsupported,
    };
    Ok(KeyInfo {
        kind,
        ttl_ms,
        length,
        data,
        next,
    })
}

fn command(name: &str, key: &[u8]) -> Vec<Vec<u8>> {
    vec![name.as_bytes().to_vec(), key.to_vec()]
}
fn decimal(value: u64) -> Vec<u8> {
    value.to_string().into_bytes()
}
fn integer(value: Value) -> Result<i64> {
    match value {
        Value::Int(value) => Ok(value),
        _ => Err(RedisError::Protocol),
    }
}
fn bytes(value: Value) -> Result<Vec<u8>> {
    match value {
        Value::BulkString(value) => Ok(value),
        Value::SimpleString(value) => Ok(value.into_bytes()),
        Value::Okay => Ok(b"OK".to_vec()),
        _ => Err(RedisError::Protocol),
    }
}
fn array(value: Value) -> Result<Vec<Value>> {
    match value {
        Value::Array(value) => Ok(value),
        _ => Err(RedisError::Protocol),
    }
}
fn scan_result(value: Value) -> Result<(u64, Vec<Value>)> {
    let mut values = array(value)?.into_iter();
    let cursor = bytes(values.next().ok_or(RedisError::Protocol)?)?;
    let cursor = std::str::from_utf8(&cursor)
        .ok()
        .and_then(|value| value.parse().ok())
        .ok_or(RedisError::Protocol)?;
    let entries = array(values.next().ok_or(RedisError::Protocol)?)?;
    if values.next().is_some() {
        return Err(RedisError::Protocol);
    }
    Ok((cursor, entries))
}
fn pairs(values: Vec<Value>) -> Result<Vec<(Vec<u8>, Vec<u8>)>> {
    if !values.len().is_multiple_of(2) {
        return Err(RedisError::Protocol);
    }
    if values.len() / 2 > MAX_PAGE_ITEMS {
        return Err(RedisError::Limit);
    }
    let mut values = values.into_iter();
    let mut pairs = Vec::new();
    while let Some(first) = values.next() {
        pairs.push((
            bytes(first)?,
            bytes(values.next().ok_or(RedisError::Protocol)?)?,
        ));
    }
    Ok(pairs)
}
fn value(response: Value) -> Result<RedisValue> {
    Ok(match response {
        Value::Nil => RedisValue::Nil,
        Value::Int(value) => RedisValue::Integer(value),
        Value::BulkString(value) => RedisValue::Bytes(value),
        Value::SimpleString(value) => RedisValue::Bytes(value.into_bytes()),
        Value::Okay => RedisValue::Bytes(b"OK".to_vec()),
        Value::Array(values) => {
            RedisValue::Array(values.into_iter().map(value).collect::<Result<_>>()?)
        }
        Value::ServerError(_) => return Err(RedisError::Server),
        _ => return Err(RedisError::Protocol),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::RedisConfig;
    use std::time::Duration;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    #[tokio::test]
    async fn missing_key_inspection_does_not_query_ttl() {
        tokio::time::timeout(Duration::from_secs(2), async {
            let (client, mut server) = tokio::io::duplex(128);
            let server = tokio::spawn(async move {
                for (expected, reply) in [
                    (
                        redis::cmd("TYPE").arg("key").get_packed_command(),
                        b"+none\r\n".as_slice(),
                    ),
                    (
                        redis::cmd("PING").get_packed_command(),
                        b"+PONG\r\n".as_slice(),
                    ),
                ] {
                    let mut request = vec![0; expected.len()];
                    server.read_exact(&mut request).await.unwrap();
                    assert_eq!(request, expected);
                    server.write_all(reply).await.unwrap();
                }
            });
            let mut connection = Connection::over_stream(RedisConfig::default(), None, client)
                .await
                .unwrap();
            assert_eq!(
                inspect(&mut connection, b"key", PageRequest::default())
                    .await
                    .unwrap(),
                KeyInfo {
                    kind: KeyType::Missing,
                    ttl_ms: -2,
                    length: 0,
                    data: KeyData::Missing,
                    next: None,
                }
            );
            assert_eq!(
                connection.query(vec![b"PING".to_vec()]).await.unwrap(),
                Value::SimpleString("PONG".into())
            );
            server.await.unwrap();
        })
        .await
        .unwrap();
    }

    #[test]
    fn scan_keeps_empty_nonterminal_pages_and_binary_keys() {
        let (cursor, keys) = scan_result(Value::Array(vec![
            Value::BulkString(b"42".to_vec()),
            Value::Array(vec![]),
        ]))
        .unwrap();
        assert_eq!(cursor, 42);
        assert!(keys.is_empty());
        assert_eq!(bytes(Value::BulkString(vec![255, 0])).unwrap(), [255, 0]);
    }
    #[test]
    fn malformed_pairs_are_not_silently_dropped() {
        assert!(pairs(vec![Value::BulkString(b"field".to_vec())]).is_err());
    }
}
