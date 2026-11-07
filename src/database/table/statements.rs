use super::*;
use crate::database::{MAX_RESULT_BYTES, MAX_SQL_BYTES, quote_identifier, quote_literal};
use std::collections::BTreeSet;

fn qualified(schema: &TableSchema) -> String {
    format!(
        "{}.{}",
        quote_identifier(schema.engine, &schema.table.schema),
        quote_identifier(schema.engine, &schema.table.name)
    )
}

pub(super) fn page(schema: &TableSchema, request: &PageRequest) -> Result<String> {
    if schema.columns.is_empty() || request.offset > i64::MAX as u64 {
        return Err(DatabaseError::InvalidTableEdit);
    }
    let names: Vec<_> = schema
        .columns
        .iter()
        .map(|column| quote_identifier(schema.engine, &column.name))
        .collect();
    let mut selected = names.clone();
    if schema.engine == SqlEngine::Sqlite {
        selected = names.iter().map(|name| {
            format!("CASE WHEN typeof({name})='text' THEN hex(CAST({name} AS BLOB)) ELSE {name} END AS {name}")
        }).collect();
        selected.extend(names.iter().map(|name| format!("typeof({name})")));
    }
    let mut sql = format!("SELECT {} FROM {}", selected.join(","), qualified(schema));
    if let Some(filter) = &request.filter {
        sql.push_str(" WHERE ");
        sql.push_str(&filter_sql(schema, filter)?);
    }
    let mut order = Vec::new();
    if let Some(sort) = request.order {
        let name = names
            .get(sort.column)
            .ok_or(DatabaseError::InvalidTableEdit)?;
        if schema.columns[sort.column].kind == ValueKind::Unsupported {
            return Err(DatabaseError::InvalidTableEdit);
        }
        let name = comparable(schema, &schema.columns[sort.column], name);
        order.push(format!(
            "{name} {}",
            if sort.descending { "DESC" } else { "ASC" }
        ));
    }
    let keys = schema.primary_key();
    for index in keys {
        if !request.order.is_some_and(|sort| sort.column == index) {
            order.push(format!("{} ASC", names[index]));
        }
    }
    if order.is_empty() {
        for (name, column) in names.iter().zip(&schema.columns) {
            if column.kind != ValueKind::Unsupported {
                order.push(format!("{} ASC", comparable(schema, column, name)));
            }
        }
    }
    if !order.is_empty() {
        sql.push_str(" ORDER BY ");
        sql.push_str(&order.join(","));
    }
    sql.push_str(&format!(
        " LIMIT {} OFFSET {}",
        request.limit + 1,
        request.offset
    ));
    Ok(sql)
}

fn filter_sql(schema: &TableSchema, filter: &ColumnFilter) -> Result<String> {
    let column = schema
        .columns
        .get(filter.column)
        .ok_or(DatabaseError::InvalidTableEdit)?;
    let name = quote_identifier(schema.engine, &column.name);
    if filter.operator == FilterOperator::IsNull {
        return Ok(format!("{name} IS NULL"));
    }
    if filter.operator == FilterOperator::IsNotNull {
        return Ok(format!("{name} IS NOT NULL"));
    }
    if filter.operator == FilterOperator::Contains {
        let CellValue::Text(value) = &filter.value else {
            return Err(DatabaseError::InvalidTableEdit);
        };
        if schema.engine == SqlEngine::ClickHouse {
            return Ok(format!(
                "positionCaseInsensitiveUTF8(toString({name}), {}) > 0",
                quote_literal(schema.engine, value)
            ));
        }
        let pattern = format!(
            "%{}%",
            value
                .replace('!', "!!")
                .replace('%', "!%")
                .replace('_', "!_")
        );
        let cast = if schema.engine == SqlEngine::Mysql {
            "CHAR"
        } else {
            "TEXT"
        };
        return Ok(format!(
            "LOWER(CAST({name} AS {cast})) LIKE LOWER({}) ESCAPE '!'",
            literal(schema.engine, &CellValue::Text(pattern))?
        ));
    }
    if filter.value == CellValue::Null {
        return match filter.operator {
            FilterOperator::Equals => Ok(format!("{name} IS NULL")),
            FilterOperator::NotEquals => Ok(format!("{name} IS NOT NULL")),
            _ => Err(DatabaseError::InvalidTableEdit),
        };
    }
    validate_value(schema.engine, column, &filter.value)?;
    let operator = match filter.operator {
        FilterOperator::Equals => "=",
        FilterOperator::NotEquals => "<>",
        FilterOperator::Greater => ">",
        FilterOperator::GreaterOrEqual => ">=",
        FilterOperator::Less => "<",
        FilterOperator::LessOrEqual => "<=",
        _ => return Err(DatabaseError::InvalidTableEdit),
    };
    let name = comparable(schema, column, &name);
    Ok(format!(
        "{name} {operator} {}",
        literal(schema.engine, &filter.value)?
    ))
}

fn comparable(schema: &TableSchema, column: &ColumnInfo, name: &str) -> String {
    if schema.engine == SqlEngine::Postgres && matches!(column.data_type.as_str(), "json" | "xml") {
        format!("CAST({name} AS TEXT)")
    } else {
        name.to_owned()
    }
}

pub(super) fn decode_rows(
    schema: &TableSchema,
    result: &mut ResultSet,
) -> Result<Vec<Vec<CellValue>>> {
    let count = schema.columns.len();
    let expected = if schema.engine == SqlEngine::Sqlite {
        count * 2
    } else {
        count
    };
    let mut originals = Vec::with_capacity(result.rows.len());
    for row in &mut result.rows {
        if row.len() != expected {
            return Err(DatabaseError::Protocol);
        }
        let mut values = Vec::with_capacity(count);
        for (index, column) in schema.columns.iter().enumerate() {
            let value = match &row[index] {
                None => CellValue::Null,
                Some(text) if schema.engine == SqlEngine::Sqlite => {
                    match row[index + count].as_deref() {
                        Some("text") => match String::from_utf8(decode_hex(text)?) {
                            Ok(value) => CellValue::Text(value),
                            Err(error) => CellValue::InvalidText(error.into_bytes()),
                        },
                        Some("integer") => CellValue::Integer(text.clone()),
                        Some("real") => CellValue::Real(text.clone()),
                        Some("blob") => CellValue::Binary(decode_hex(text)?),
                        _ => return Err(DatabaseError::Protocol),
                    }
                }
                Some(text) => match CellValue::parse(column, text) {
                    Ok(value) => value,
                    // An unsupported type or special numeric value can still be inspected.
                    // Its representation cannot be assigned through the cell editor.
                    Err(_) => CellValue::Text(text.clone()),
                },
            };
            row[index] = value.display();
            values.push(value);
        }
        row.truncate(count);
        originals.push(values);
    }
    Ok(originals)
}

pub(super) fn changes(
    schema: &TableSchema,
    changes: &[RowChange],
) -> Result<Vec<GuardedStatement>> {
    if !schema.editable() {
        return Err(DatabaseError::TableReadOnly);
    }
    if changes.len() > MAX_ROWS {
        return Err(DatabaseError::Limit);
    }
    let table = qualified(schema);
    let mut statements = Vec::new();
    let mut total_bytes = 0;
    for change in changes {
        let sql = match change {
            RowChange::Insert { values } => {
                validate_assignments(schema, values)?;
                if values.is_empty() {
                    match schema.engine {
                        SqlEngine::Mysql => format!("INSERT INTO {table} () VALUES ()"),
                        _ => format!("INSERT INTO {table} DEFAULT VALUES"),
                    }
                } else {
                    let names = values
                        .iter()
                        .map(|(index, _)| {
                            quote_identifier(schema.engine, &schema.columns[*index].name)
                        })
                        .collect::<Vec<_>>();
                    let values = values
                        .iter()
                        .map(|(_, value)| literal(schema.engine, value))
                        .collect::<Result<Vec<_>>>()?;
                    format!(
                        "INSERT INTO {table} ({}) VALUES ({})",
                        names.join(","),
                        values.join(",")
                    )
                }
            }
            RowChange::Update { original, values } => {
                validate_original(schema, original)?;
                validate_assignments(schema, values)?;
                let values: Vec<_> = values
                    .iter()
                    .filter(|(index, value)| original[*index] != *value)
                    .collect();
                if values.is_empty() {
                    continue;
                }
                let assignments = values
                    .into_iter()
                    .map(|(index, value)| {
                        Ok(format!(
                            "{}={}",
                            quote_identifier(schema.engine, &schema.columns[*index].name),
                            literal(schema.engine, value)?
                        ))
                    })
                    .collect::<Result<Vec<_>>>()?;
                format!(
                    "UPDATE {table} SET {} WHERE {}",
                    assignments.join(","),
                    original_predicate(schema, original)?
                )
            }
            RowChange::Delete { original } => {
                validate_original(schema, original)?;
                format!(
                    "DELETE FROM {table} WHERE {}",
                    original_predicate(schema, original)?
                )
            }
        };
        total_bytes += sql.len();
        if total_bytes > MAX_SQL_BYTES {
            return Err(DatabaseError::Limit);
        }
        statements.push(GuardedStatement {
            sql,
            expected_rows: 1,
        });
    }
    Ok(statements)
}

fn validate_assignments(schema: &TableSchema, values: &[(usize, CellValue)]) -> Result<()> {
    let mut used = BTreeSet::new();
    for (index, value) in values {
        let column = schema
            .columns
            .get(*index)
            .ok_or(DatabaseError::InvalidTableEdit)?;
        if !column.editable() || !used.insert(*index) {
            return Err(DatabaseError::InvalidTableEdit);
        }
        validate_value(schema.engine, column, value)?;
    }
    Ok(())
}

fn validate_value(engine: SqlEngine, column: &ColumnInfo, value: &CellValue) -> Result<()> {
    let dynamic = engine == SqlEngine::Sqlite;
    let valid = match value {
        CellValue::Null => column.nullable,
        CellValue::Text(value) => {
            (dynamic || column.kind == ValueKind::Text) && value.len() <= MAX_RESULT_BYTES
        }
        CellValue::Integer(value) => {
            (dynamic
                || matches!(
                    column.kind,
                    ValueKind::Integer | ValueKind::Decimal | ValueKind::Float
                ))
                && valid_number(value, true)
        }
        CellValue::Real(value) => {
            (dynamic || matches!(column.kind, ValueKind::Decimal | ValueKind::Float))
                && valid_number(value, false)
                && (!dynamic && column.kind != ValueKind::Float
                    || value.parse::<f64>().is_ok_and(f64::is_finite))
        }
        CellValue::Boolean(_) => dynamic || column.kind == ValueKind::Boolean,
        CellValue::Binary(value) => {
            (dynamic || column.kind == ValueKind::Binary) && value.len() <= MAX_RESULT_BYTES / 2
        }
        CellValue::InvalidText(_) => false,
    };
    if valid {
        Ok(())
    } else {
        Err(DatabaseError::InvalidTableEdit)
    }
}

fn validate_original(schema: &TableSchema, original: &[CellValue]) -> Result<()> {
    if !schema.row_editable(original) {
        return Err(DatabaseError::InvalidTableEdit);
    }
    Ok(())
}

fn original_predicate(schema: &TableSchema, original: &[CellValue]) -> Result<String> {
    schema
        .columns
        .iter()
        .zip(original)
        .map(|(column, value)| {
            let name = quote_identifier(schema.engine, &column.name);
            if *value == CellValue::Null {
                return Ok(format!("{name} IS NULL"));
            }
            let value_sql = literal(schema.engine, value)?;
            Ok(match schema.engine {
                SqlEngine::Postgres
                    if matches!(column.data_type.as_str(), "json" | "xml")
                        || column.kind == ValueKind::Unsupported =>
                {
                    format!(
                        "CAST({name} AS TEXT) IS NOT DISTINCT FROM {}",
                        quote_literal(schema.engine, &value.display().unwrap_or_default())
                    )
                }
                SqlEngine::Postgres => format!("{name} IS NOT DISTINCT FROM {value_sql}"),
                SqlEngine::Mysql
                    if matches!(value, CellValue::Text(_)) || column.kind == ValueKind::Float =>
                {
                    let original_text =
                        quote_literal(schema.engine, &value.display().unwrap_or_default());
                    format!(
                        "CAST(CAST({name} AS CHAR) AS BINARY) <=> CAST({original_text} AS BINARY)"
                    )
                }
                SqlEngine::Mysql => format!("{name} <=> {value_sql}"),
                SqlEngine::ClickHouse => return Err(DatabaseError::TableReadOnly),
                SqlEngine::Sqlite => {
                    let storage = match value {
                        CellValue::Null => "null",
                        CellValue::Text(_) | CellValue::InvalidText(_) => "text",
                        CellValue::Integer(_) | CellValue::Boolean(_) => "integer",
                        CellValue::Real(_) => "real",
                        CellValue::Binary(_) => "blob",
                    };
                    format!("(typeof({name})='{storage}' AND {name} COLLATE BINARY IS {value_sql})")
                }
            })
        })
        .collect::<Result<Vec<_>>>()
        .map(|terms| terms.join(" AND "))
}

fn literal(engine: SqlEngine, value: &CellValue) -> Result<String> {
    Ok(match value {
        CellValue::Null => "NULL".into(),
        CellValue::Text(value) if engine == SqlEngine::Sqlite && value.contains('\0') => {
            format!("CAST(X'{}' AS TEXT)", super::super::hex(value.as_bytes()))
        }
        CellValue::Text(value) if engine != SqlEngine::Postgres || !value.contains('\0') => {
            quote_literal(engine, value)
        }
        CellValue::Text(_) => return Err(DatabaseError::InvalidTableEdit),
        CellValue::InvalidText(_) => return Err(DatabaseError::InvalidTableEdit),
        CellValue::Integer(value) if valid_number(value, true) => value.clone(),
        CellValue::Real(value) if valid_number(value, false) => value.clone(),
        CellValue::Integer(_) | CellValue::Real(_) => return Err(DatabaseError::InvalidTableEdit),
        CellValue::Boolean(value) => if *value { "TRUE" } else { "FALSE" }.into(),
        CellValue::Binary(value) => match engine {
            SqlEngine::Postgres => format!("decode('{}','hex')", super::super::hex(value)),
            _ => format!("X'{}'", super::super::hex(value)),
        },
    })
}

pub(super) fn valid_number(text: &str, integer: bool) -> bool {
    let bytes = text.as_bytes();
    let mut at = usize::from(matches!(bytes.first(), Some(b'+' | b'-')));
    let begin = at;
    while bytes.get(at).is_some_and(u8::is_ascii_digit) {
        at += 1;
    }
    let mut digits = at - begin;
    if !integer && bytes.get(at) == Some(&b'.') {
        at += 1;
        let begin = at;
        while bytes.get(at).is_some_and(u8::is_ascii_digit) {
            at += 1;
        }
        digits += at - begin;
    }
    if digits == 0 {
        return false;
    }
    if !integer && matches!(bytes.get(at), Some(b'e' | b'E')) {
        at += 1;
        if matches!(bytes.get(at), Some(b'+' | b'-')) {
            at += 1;
        }
        let begin = at;
        while bytes.get(at).is_some_and(u8::is_ascii_digit) {
            at += 1;
        }
        if at == begin {
            return false;
        }
    }
    at == bytes.len()
}

pub(super) fn decode_hex(text: &str) -> Result<Vec<u8>> {
    let text = text
        .strip_prefix("0x")
        .or_else(|| text.strip_prefix("\\x"))
        .unwrap_or(text);
    if !text.len().is_multiple_of(2)
        || text.len() > MAX_RESULT_BYTES
        || !text.bytes().all(|byte| byte.is_ascii_hexdigit())
    {
        return Err(DatabaseError::InvalidTableEdit);
    }
    text.as_bytes()
        .as_chunks::<2>()
        .0
        .iter()
        .map(|pair| {
            let digit = |byte: u8| {
                (byte as char)
                    .to_digit(16)
                    .ok_or(DatabaseError::InvalidTableEdit)
            };
            Ok(((digit(pair[0])? << 4) | digit(pair[1])?) as u8)
        })
        .collect()
}
