use crate::i18n::messages as i18n_key;

use super::*;
use crate::database::{quote_identifier, quote_literal};

const MAX_COLUMNS: usize = 512;

pub(super) async fn load(client: &DatabaseClient, table: TableInfo) -> Result<TableSchema> {
    let engine = client.0.engine;
    let schema_name = quote_literal(engine, &table.schema);
    let table_name = quote_literal(engine, &table.name);
    let sql = match engine {
        SqlEngine::Mysql => format!(
            "SELECT c.COLUMN_NAME,c.COLUMN_TYPE,c.IS_NULLABLE,c.COLUMN_DEFAULT,c.EXTRA,COALESCE(k.ORDINAL_POSITION,0),t.TABLE_TYPE,t.ENGINE \
             FROM information_schema.COLUMNS c \
             JOIN information_schema.TABLES t ON t.TABLE_SCHEMA=c.TABLE_SCHEMA AND t.TABLE_NAME=c.TABLE_NAME \
             LEFT JOIN information_schema.KEY_COLUMN_USAGE k ON k.TABLE_SCHEMA=c.TABLE_SCHEMA AND k.TABLE_NAME=c.TABLE_NAME AND k.COLUMN_NAME=c.COLUMN_NAME AND k.CONSTRAINT_NAME='PRIMARY' \
             WHERE c.TABLE_SCHEMA={schema_name} AND c.TABLE_NAME={table_name} ORDER BY c.ORDINAL_POSITION"
        ),
        SqlEngine::Postgres => format!(
            "SELECT a.attname,pg_catalog.format_type(a.atttypid,a.atttypmod),CASE WHEN a.attnotnull THEN 'NO' ELSE 'YES' END,pg_catalog.pg_get_expr(d.adbin,d.adrelid), \
             CASE WHEN a.attgenerated<>'' OR a.attidentity='a' THEN '1' ELSE '0' END, \
             COALESCE((SELECT key.ordinality FROM pg_catalog.pg_index i CROSS JOIN LATERAL unnest(i.indkey) WITH ORDINALITY AS key(attnum,ordinality) WHERE i.indrelid=c.oid AND i.indisprimary AND key.ordinality<=i.indnkeyatts AND key.attnum=a.attnum),0),c.relkind::text, \
             CASE WHEN t.typtype='e' THEN 'text' WHEN t.typtype='d' THEN (SELECT base.typname FROM pg_catalog.pg_type base WHERE base.oid=t.typbasetype) ELSE t.typname END \
             FROM pg_catalog.pg_attribute a JOIN pg_catalog.pg_class c ON c.oid=a.attrelid \
             JOIN pg_catalog.pg_namespace n ON n.oid=c.relnamespace JOIN pg_catalog.pg_type t ON t.oid=a.atttypid \
             LEFT JOIN pg_catalog.pg_attrdef d ON d.adrelid=c.oid AND d.adnum=a.attnum \
             WHERE n.nspname={schema_name} AND c.relname={table_name} AND a.attnum>0 AND NOT a.attisdropped ORDER BY a.attnum"
        ),
        SqlEngine::Sqlite => format!(
            "SELECT p.name,p.type,CASE WHEN p.\"notnull\" THEN 'NO' ELSE 'YES' END,p.dflt_value,p.hidden,p.pk,s.type,'' \
             FROM pragma_table_xinfo({table_name},{schema_name}) p \
             JOIN {}.sqlite_schema s ON s.name={table_name} WHERE p.hidden<>1 ORDER BY p.cid",
            quote_identifier(engine, &table.schema)
        ),
    };
    let output = client.execute(sql, MAX_COLUMNS + 1).await?;
    super::super::ensure_complete(&output)?;
    let mut rows = output.results.into_iter().flat_map(|result| result.rows);
    let mut columns = Vec::new();
    let mut relation_kind = None;
    let mut mysql_engine = None;
    for row in &mut rows {
        if row.len() != 8 || columns.len() == MAX_COLUMNS {
            return Err(DatabaseError::Protocol);
        }
        let required = |index: usize| row[index].as_deref().ok_or(DatabaseError::Protocol);
        let name = required(0)?.to_owned();
        let data_type = required(1)?.to_owned();
        let extra = required(4)?.to_ascii_lowercase();
        let key_order = required(5)?
            .parse::<usize>()
            .map_err(|_| DatabaseError::Protocol)?;
        let generated = match engine {
            SqlEngine::Mysql => {
                extra.contains("virtual generated")
                    || extra.contains("stored generated")
                    || extra.contains("auto_increment")
            }
            SqlEngine::Postgres => extra == "1",
            SqlEngine::Sqlite => matches!(extra.as_str(), "2" | "3"),
        };
        relation_kind = Some(required(6)?.to_owned());
        if engine == SqlEngine::Mysql {
            mysql_engine = row[7].clone();
        }
        let kind = column_kind(engine, &data_type, row[7].as_deref().unwrap_or(""));
        columns.push(ColumnInfo {
            name,
            data_type,
            nullable: required(2)? == "YES",
            default: row[3].clone(),
            generated,
            primary_key: (key_order > 0).then_some(key_order),
            kind,
        });
    }
    if columns.is_empty() {
        return Err(DatabaseError::Protocol);
    }
    let table_relation = match engine {
        SqlEngine::Mysql => relation_kind.as_deref() == Some("BASE TABLE"),
        SqlEngine::Postgres => matches!(relation_kind.as_deref(), Some("r" | "p")),
        SqlEngine::Sqlite => relation_kind.as_deref() == Some("table"),
    };
    let primary_key: Vec<_> = columns
        .iter()
        .filter(|column| column.primary_key.is_some())
        .collect();
    let read_only_reason = if client.is_read_only() {
        Some(i18n_key::DATABASE_THIS_CONNECTION_IS_READ_ONLY)
    } else if !table_relation {
        Some(i18n_key::DATABASE_VIEWS_ARE_READ_ONLY_IN_THE_DATA_EDITOR)
    } else if primary_key.is_empty() {
        Some(i18n_key::DATABASE_A_PRIMARY_KEY_IS_REQUIRED_TO_EDIT_ROWS_SAFELY)
    } else if columns
        .iter()
        .any(|column| column.kind == ValueKind::Unsupported)
    {
        Some(i18n_key::DATABASE_THIS_TABLE_CONTAINS_A_TYPE_THAT_CANNOT_BE_EDITED_SAFELY)
    } else if engine == SqlEngine::Mysql
        && !mysql_engine
            .as_deref()
            .is_some_and(|engine| engine.eq_ignore_ascii_case("innodb"))
    {
        Some(i18n_key::DATABASE_EDITING_REQUIRES_A_TRANSACTIONAL_INNODB_TABLE)
    } else {
        None
    };
    Ok(TableSchema {
        table,
        columns,
        read_only_reason,
        engine,
    })
}

fn column_kind(engine: SqlEngine, data_type: &str, postgres_type: &str) -> ValueKind {
    let data_type = data_type.to_ascii_lowercase();
    match engine {
        SqlEngine::Postgres => match postgres_type {
            "int2" | "int4" | "int8" | "oid" => ValueKind::Integer,
            "numeric" => ValueKind::Decimal,
            "float4" | "float8" => ValueKind::Float,
            "bool" => ValueKind::Boolean,
            "bytea" => ValueKind::Binary,
            "text" | "varchar" | "bpchar" | "name" | "uuid" | "json" | "jsonb" | "date"
            | "time" | "timetz" | "timestamp" | "timestamptz" | "interval" | "inet" | "cidr"
            | "macaddr" | "macaddr8" | "xml" | "bit" | "varbit" => ValueKind::Text,
            name if name.starts_with('_') => ValueKind::Text,
            _ => ValueKind::Unsupported,
        },
        SqlEngine::Mysql => {
            let name = data_type.split(['(', ' ']).next().unwrap_or("");
            match name {
                "tinyint" | "smallint" | "mediumint" | "int" | "integer" | "bigint" | "year" => {
                    ValueKind::Integer
                }
                "decimal" | "numeric" => ValueKind::Decimal,
                "float" | "double" | "real" => ValueKind::Float,
                "binary" | "varbinary" | "tinyblob" | "blob" | "mediumblob" | "longblob" => {
                    ValueKind::Binary
                }
                "char" | "varchar" | "tinytext" | "text" | "mediumtext" | "longtext" | "enum"
                | "set" | "json" | "date" | "datetime" | "timestamp" | "time" => ValueKind::Text,
                _ => ValueKind::Unsupported,
            }
        }
        SqlEngine::Sqlite => {
            if data_type.contains("int") {
                ValueKind::Integer
            } else if ["char", "clob", "text"]
                .iter()
                .any(|part| data_type.contains(part))
            {
                ValueKind::Text
            } else if ["real", "floa", "doub"]
                .iter()
                .any(|part| data_type.contains(part))
            {
                ValueKind::Float
            } else if data_type == "blob" {
                ValueKind::Binary
            } else if data_type.contains("bool") {
                ValueKind::Boolean
            } else if data_type.contains("numeric") || data_type.contains("decimal") {
                ValueKind::Decimal
            } else {
                // SQLite still supplies the actual storage class for each loaded cell.
                ValueKind::Text
            }
        }
    }
}
