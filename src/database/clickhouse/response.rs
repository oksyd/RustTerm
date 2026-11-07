use super::*;
use serde::Deserialize;

#[derive(Deserialize)]
struct Column {
    name: String,
}
#[derive(Deserialize)]
struct Response {
    meta: Vec<Column>,
    data: Vec<Vec<Box<serde_json::value::RawValue>>>,
    rows: usize,
}
pub(super) fn parse_response(body: &[u8], limit: usize, affected_rows: u64) -> Result<ResultSet> {
    if body.iter().all(u8::is_ascii_whitespace) {
        return Ok(ResultSet {
            affected_rows,
            ..Default::default()
        });
    }
    // Parse the entire body: ClickHouse can append an exception after HTTP 200.
    let decoded = serde_json::from_slice::<Response>(body);
    let mut decoded = match decoded {
        Ok(decoded) => decoded,
        Err(_) => {
            let text = String::from_utf8_lossy(body);
            if let Some(error) = text
                .lines()
                .find(|line| line.starts_with("Code: ") && line.contains("DB::Exception"))
            {
                let code = error
                    .strip_prefix("Code: ")
                    .and_then(|s| s.split('.').next())
                    .unwrap_or("ClickHouse");
                return Err(server_error(code.into(), error));
            }
            return Err(DatabaseError::ResultFormat);
        }
    };
    if decoded.meta.len() > MAX_COLUMNS
        || decoded.rows != decoded.data.len()
        || decoded
            .data
            .iter()
            .any(|row| row.len() != decoded.meta.len())
    {
        return Err(DatabaseError::Protocol);
    }
    let truncated = decoded.data.len() > limit;
    decoded.data.truncate(limit);
    Ok(ResultSet {
        columns: decoded.meta.into_iter().map(|column| column.name).collect(),
        rows: decoded
            .data
            .into_iter()
            .map(|row| {
                row.into_iter()
                    .map(|value| {
                        let raw = value.get();
                        if raw == "null" {
                            Ok(None)
                        } else if raw.starts_with('"') {
                            serde_json::from_str::<String>(raw)
                                .map(Some)
                                .map_err(|_| DatabaseError::Protocol)
                        } else {
                            Ok(Some(raw.to_owned()))
                        }
                    })
                    .collect::<Result<Vec<_>>>()
            })
            .collect::<Result<Vec<_>>>()?,
        affected_rows,
        truncated,
    })
}

/// TSV keeps NULL (\N), infinities, decimals and nested values distinct without
/// requiring a read-only user to change any server output settings.
pub(super) fn parse_tsv(body: &[u8], limit: usize, affected_rows: u64) -> Result<ResultSet> {
    if body.is_empty() {
        return Ok(ResultSet {
            affected_rows,
            ..Default::default()
        });
    }
    let mut lines = body
        .strip_suffix(b"\n")
        .unwrap_or(body)
        .split(|b| *b == b'\n');
    let columns = lines
        .next()
        .ok_or(DatabaseError::Protocol)?
        .split(|b| *b == b'\t')
        .map(tsv_string)
        .collect::<Result<Vec<_>>>()?;
    if columns.len() > MAX_COLUMNS
        || lines
            .next()
            .ok_or(DatabaseError::Protocol)?
            .split(|b| *b == b'\t')
            .count()
            != columns.len()
    {
        return Err(DatabaseError::Protocol);
    }
    let mut result = ResultSet {
        columns,
        affected_rows,
        ..Default::default()
    };
    for line in lines {
        let cells = line.split(|b| *b == b'\t').collect::<Vec<_>>();
        if cells.len() != result.columns.len()
            || line.starts_with(b"Code: ") && line.windows(13).any(|part| part == b"DB::Exception")
        {
            return Err(DatabaseError::ResultFormat);
        }
        if result.rows.len() == limit {
            result.truncated = true;
            continue;
        }
        result.rows.push(
            cells
                .into_iter()
                .map(|raw| {
                    if raw == b"\\N" {
                        Ok(None)
                    } else {
                        tsv_string(raw).map(Some)
                    }
                })
                .collect::<Result<Vec<_>>>()?,
        );
    }
    Ok(result)
}
fn tsv_string(raw: &[u8]) -> Result<String> {
    let mut decoded = Vec::with_capacity(raw.len());
    let mut bytes = raw.iter().copied();
    while let Some(byte) = bytes.next() {
        if byte != b'\\' {
            decoded.push(byte);
            continue;
        }
        decoded.push(match bytes.next().ok_or(DatabaseError::Protocol)? {
            b'0' => 0,
            b'a' => 7,
            b'b' => 8,
            b'e' => 27,
            b'f' => 12,
            b'n' => b'\n',
            b'r' => b'\r',
            b't' => b'\t',
            b'v' => 11,
            escaped => escaped,
        });
    }
    Ok(String::from_utf8(decoded).unwrap_or_else(|error| format!("0x{}", hex(error.as_bytes()))))
}
