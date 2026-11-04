use super::{MAX_ARGUMENTS, MAX_REQUEST_BYTES, RedisError, Result};

/// Atomic, non-overwriting collection creation. The command in ARGV[1] is
/// separately restricted below; arbitrary Lua is deliberately not supported.
pub const CREATE_SCRIPT: &str = "if redis.call('EXISTS', KEYS[1]) == 1 then return 0 end redis.call(ARGV[1], KEYS[1], unpack(ARGV, 2)) return 1";
/// Atomic compare-and-set for a complete string value. Never overwrites a
/// changed type/value or a key that expired while its editor was open.
pub const SAVE_STRING_SCRIPT: &str = "if redis.call('GET', KEYS[1]) ~= ARGV[1] then return 0 end redis.call('SET', KEYS[1], ARGV[2], 'KEEPTTL') return 1";
/// Compare the displayed list element before replacing its index. Reject a
/// changed value, missing index, expired key, or different key type.
pub const SAVE_LIST_SCRIPT: &str = "if redis.call('TYPE', KEYS[1]).ok ~= 'list' then return 0 end if redis.call('LINDEX', KEYS[1], ARGV[1]) ~= ARGV[2] then return 0 end redis.call('LSET', KEYS[1], ARGV[1], ARGV[3]) return 1";

pub(super) fn validate(args: &[Vec<u8>]) -> Result<()> {
    if args.is_empty() || args.len() > MAX_ARGUMENTS {
        return Err(RedisError::InvalidCommand);
    }
    if args
        .iter()
        .try_fold(0usize, |n, a| n.checked_add(a.len()))
        .is_none_or(|n| n > MAX_REQUEST_BYTES)
    {
        return Err(RedisError::Limit);
    }
    let name = std::str::from_utf8(&args[0])
        .map_err(|_| RedisError::InvalidCommand)?
        .to_ascii_uppercase();
    if name == "EVAL" {
        if args.len() == 6 && args[1] == SAVE_STRING_SCRIPT.as_bytes() && args[2] == b"1" {
            return Ok(());
        }
        if args.len() == 7 && args[1] == SAVE_LIST_SCRIPT.as_bytes() && args[2] == b"1" {
            return Ok(());
        }
        if args.len() >= 6
            && args[1] == CREATE_SCRIPT.as_bytes()
            && args[2] == b"1"
            && ["HSET", "LPUSH", "RPUSH", "SADD", "ZADD", "XADD"]
                .iter()
                .any(|name| args[4].eq_ignore_ascii_case(name.as_bytes()))
        {
            return Ok(());
        }
        return Err(RedisError::UnsupportedCommand);
    }
    // No transactions, subscriptions, blocking commands, arbitrary scripts,
    // server administration or connection-state changes. SELECT is actor-owned.
    if !matches!(
        name.as_str(),
        "PING"
            | "ECHO"
            | "INFO"
            | "TIME"
            | "DBSIZE"
            | "EXISTS"
            | "TYPE"
            | "GET"
            | "GETRANGE"
            | "STRLEN"
            | "MGET"
            | "SET"
            | "SETNX"
            | "SETEX"
            | "PSETEX"
            | "APPEND"
            | "SETRANGE"
            | "INCR"
            | "INCRBY"
            | "INCRBYFLOAT"
            | "DECR"
            | "DECRBY"
            | "DEL"
            | "UNLINK"
            | "RENAME"
            | "RENAMENX"
            | "TTL"
            | "PTTL"
            | "EXPIRE"
            | "PEXPIRE"
            | "EXPIREAT"
            | "PEXPIREAT"
            | "PERSIST"
            | "SCAN"
            | "HGET"
            | "HMGET"
            | "HLEN"
            | "HEXISTS"
            | "HSTRLEN"
            | "HSCAN"
            | "HSET"
            | "HSETNX"
            | "HDEL"
            | "HINCRBY"
            | "HINCRBYFLOAT"
            | "LLEN"
            | "LINDEX"
            | "LRANGE"
            | "LPUSH"
            | "RPUSH"
            | "LSET"
            | "LREM"
            | "LTRIM"
            | "LPOP"
            | "RPOP"
            | "SCARD"
            | "SISMEMBER"
            | "SSCAN"
            | "SADD"
            | "SREM"
            | "ZCARD"
            | "ZSCORE"
            | "ZRANK"
            | "ZREVRANK"
            | "ZSCAN"
            | "ZRANGE"
            | "ZREVRANGE"
            | "ZADD"
            | "ZREM"
            | "ZINCRBY"
            | "XLEN"
            | "XRANGE"
            | "XREVRANGE"
            | "XADD"
            | "XDEL"
            | "XTRIM"
    ) {
        return Err(RedisError::UnsupportedCommand);
    }
    Ok(())
}

/// Redis CLI arguments: double quotes decode escapes (including binary `\xHH`),
/// single quotes only decode `\'`, and unquoted backslashes remain literal.
pub fn parse_command(input: &str) -> Result<Vec<Vec<u8>>> {
    if input.len() > MAX_REQUEST_BYTES {
        return Err(RedisError::Limit);
    }
    let bytes = input.as_bytes();
    let mut args = Vec::new();
    let mut current = Vec::new();
    let mut quote = None;
    let mut started = false;
    let mut index = 0;
    while index < bytes.len() {
        let byte = bytes[index];
        index += 1;
        if let Some(delimiter) = quote {
            if byte == delimiter {
                if bytes
                    .get(index)
                    .is_some_and(|byte| !byte.is_ascii_whitespace())
                {
                    return Err(RedisError::InvalidCommand);
                }
                quote = None;
            } else if byte == b'\\' && delimiter == b'"' {
                let escaped = *bytes.get(index).ok_or(RedisError::InvalidCommand)?;
                index += 1;
                if escaped == b'x'
                    && let Some(high) = bytes.get(index).copied().and_then(hex)
                    && let Some(low) = bytes.get(index + 1).copied().and_then(hex)
                {
                    current.push(high * 16 + low);
                    index += 2;
                } else {
                    current.push(match escaped {
                        b'n' => b'\n',
                        b'r' => b'\r',
                        b't' => b'\t',
                        b'b' => 8,
                        b'a' => 7,
                        other => other,
                    });
                }
            } else if byte == b'\\' && delimiter == b'\'' && bytes.get(index) == Some(&b'\'') {
                current.push(b'\'');
                index += 1;
            } else {
                current.push(byte);
            }
        } else if byte == b'\'' || byte == b'"' {
            quote = Some(byte);
            started = true;
        } else if byte.is_ascii_whitespace() {
            if started {
                args.push(std::mem::take(&mut current));
                if args.len() > MAX_ARGUMENTS {
                    return Err(RedisError::Limit);
                }
                started = false;
            }
        } else {
            started = true;
            current.push(byte);
        }
    }
    if quote.is_some() {
        return Err(RedisError::InvalidCommand);
    }
    if started {
        args.push(current);
    }
    validate(&args)?;
    Ok(args)
}
fn hex(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        b'A'..=b'F' => Some(byte - b'A' + 10),
        _ => None,
    }
}

/// A UI text editor may only replace complete values for which this succeeds.
/// Keep original bytes for every key and binary value, including display escapes.
pub fn editable_text(bytes: &[u8]) -> Option<&str> {
    let text = std::str::from_utf8(bytes).ok()?;
    (!text.chars().any(|c| {
        (c.is_control() && !matches!(c, '\n' | '\r' | '\t'))
            || matches!(c, '\u{202a}'..='\u{202e}' | '\u{2066}'..='\u{2069}')
    }))
    .then_some(text)
}
pub fn display_bytes(bytes: &[u8]) -> String {
    use std::fmt::Write;
    let mut result = String::new();
    if let Ok(text) = std::str::from_utf8(bytes) {
        for character in text.chars() {
            match character {
                '\\' => result.push_str("\\\\"),
                '\n' => result.push_str("\\n"),
                '\r' => result.push_str("\\r"),
                '\t' => result.push_str("\\t"),
                c if c.is_control()
                    || matches!(c, '\u{202a}'..='\u{202e}' | '\u{2066}'..='\u{2069}') =>
                {
                    let _ = write!(result, "\\u{{{:x}}}", c as u32);
                }
                c => result.push(c),
            }
        }
    } else {
        for byte in bytes {
            let _ = write!(result, "\\x{byte:02x}");
        }
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn quoted_arguments_preserve_empty_unicode_and_binary() {
        assert_eq!(
            parse_command(r#"SET 'a b' """#).unwrap(),
            vec![b"SET".to_vec(), b"a b".to_vec(), vec![]]
        );
        assert_eq!(
            parse_command(r#"SET 中文 "\xff\x00\n""#).unwrap()[2],
            [255, 0, 10]
        );
        assert!(parse_command("SET a 'unfinished").is_err());
    }
    #[test]
    fn single_quotes_and_unquoted_backslashes_follow_redis_cli() {
        for command in [r"ECHO C:\new\temp\x41\0", r"ECHO 'C:\new\temp\x41\0'"] {
            assert_eq!(parse_command(command).unwrap()[1], br"C:\new\temp\x41\0");
        }
        assert_eq!(parse_command(r"ECHO 'it\'s \n'").unwrap()[1], br"it's \n");
        assert_eq!(parse_command(r"ECHO path\").unwrap()[1], br"path\");
    }
    #[test]
    fn double_quotes_decode_redis_escapes_and_preserve_empty_arguments() {
        assert_eq!(
            parse_command(r#"SET "" "\a\b\n\r\t\x00\xFf\"\\""#).unwrap(),
            vec![
                b"SET".to_vec(),
                vec![],
                b"\x07\x08\n\r\t\0\xff\"\\".to_vec()
            ]
        );
        assert_eq!(
            parse_command(r#"MGET "" ''"#).unwrap(),
            vec![b"MGET".to_vec(), vec![], vec![]]
        );
        // Redis treats unknown and incomplete hex escapes as escaped literals.
        assert_eq!(parse_command(r#"ECHO "\0\q\x4g\x""#).unwrap()[1], b"0qx4gx");
    }
    #[test]
    fn closing_quotes_require_a_separator() {
        for command in [r#"GET "a"b"#, "GET 'a'b", r#"GET "a"'b'"#, r#"GET 'a'"b""#] {
            assert_eq!(parse_command(command), Err(RedisError::InvalidCommand));
        }
        for command in [r#"ECHO pre"fix""#, "ECHO pre'fix'"] {
            assert_eq!(parse_command(command).unwrap()[1], b"prefix");
        }
    }
    #[test]
    fn console_cannot_change_connection_or_bypass_scan_limits() {
        for command in [
            "KEYS *",
            "SELECT 1",
            "AUTH secret",
            "MULTI",
            "MONITOR",
            "SUBSCRIBE x",
            "EVAL 'return redis.call(\"KEYS\",\"*\")' 0",
        ] {
            assert_eq!(parse_command(command), Err(RedisError::UnsupportedCommand));
        }
    }
    #[test]
    fn binary_display_is_not_silently_editable() {
        assert_eq!(display_bytes(&[255, 0]), "\\xff\\x00");
        assert_eq!(editable_text(&[255]), None);
        assert_eq!(editable_text(b"a\0b"), None);
        assert_eq!(editable_text("中文\n".as_bytes()), Some("中文\n"));
    }
    #[test]
    fn string_compare_and_set_accepts_only_the_fixed_script_shape() {
        let command = vec![
            b"EVAL".to_vec(),
            SAVE_STRING_SCRIPT.as_bytes().to_vec(),
            b"1".to_vec(),
            b"key".to_vec(),
            b"old".to_vec(),
            b"new".to_vec(),
        ];
        assert!(validate(&command).is_ok());
        assert_eq!(validate(&command[..5]), Err(RedisError::UnsupportedCommand));
        let mut changed = command;
        changed[1].push(b' ');
        assert_eq!(validate(&changed), Err(RedisError::UnsupportedCommand));
    }

    #[test]
    fn list_compare_and_set_cannot_escape_its_single_key_and_fixed_script() {
        let command = vec![
            b"EVAL".to_vec(),
            SAVE_LIST_SCRIPT.as_bytes().to_vec(),
            b"1".to_vec(),
            b"key\0binary".to_vec(),
            b"42".to_vec(),
            b"old\0value".to_vec(),
            b"new\0value".to_vec(),
        ];
        assert!(validate(&command).is_ok());
        assert_eq!(validate(&command[..6]), Err(RedisError::UnsupportedCommand));
        let mut changed = command.clone();
        changed.push(b"extra".to_vec());
        assert_eq!(validate(&changed), Err(RedisError::UnsupportedCommand));
        changed = command.clone();
        changed[2] = b"2".to_vec();
        assert_eq!(validate(&changed), Err(RedisError::UnsupportedCommand));
        changed = command;
        changed[1].extend_from_slice(b" return redis.call('FLUSHALL')");
        assert_eq!(validate(&changed), Err(RedisError::UnsupportedCommand));
    }
}
