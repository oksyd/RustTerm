//! Table metadata, bounded server-side browsing and optimistic row changes.

use crate::i18n::messages as i18n_key;

mod metadata;
mod statements;
#[cfg(test)]
mod tests;

use super::{DatabaseClient, DatabaseError, MAX_ROWS, Result, ResultSet, TableInfo};
use crate::config::SqlEngine;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ValueKind {
    Text,
    Integer,
    Decimal,
    Float,
    Boolean,
    Binary,
    Unsupported,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ColumnInfo {
    pub name: String,
    pub data_type: String,
    pub nullable: bool,
    pub default: Option<String>,
    pub generated: bool,
    pub primary_key: Option<usize>,
    pub kind: ValueKind,
}

impl ColumnInfo {
    pub fn editable(&self) -> bool {
        !self.generated && self.kind != ValueKind::Unsupported
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TableSchema {
    pub table: TableInfo,
    pub columns: Vec<ColumnInfo>,
    pub read_only_reason: Option<&'static str>,
    engine: SqlEngine,
}

impl TableSchema {
    pub fn editable(&self) -> bool {
        self.read_only_reason.is_none()
    }

    pub fn row_editable(&self, original: &[CellValue]) -> bool {
        if !self.editable()
            || original.len() != self.columns.len()
            || original
                .iter()
                .any(|value| matches!(value, CellValue::InvalidText(_)))
        {
            return false;
        }
        let key = self.primary_key();
        !key.is_empty() && key.iter().all(|index| original[*index] != CellValue::Null)
    }

    pub fn primary_key(&self) -> Vec<usize> {
        let mut columns: Vec<_> = self
            .columns
            .iter()
            .enumerate()
            .filter_map(|(index, column)| column.primary_key.map(|order| (order, index)))
            .collect();
        columns.sort_unstable();
        columns.into_iter().map(|(_, index)| index).collect()
    }

    pub fn parse_cell(
        &self,
        column: usize,
        original: Option<&CellValue>,
        text: &str,
    ) -> Result<CellValue> {
        let column = self
            .columns
            .get(column)
            .ok_or(DatabaseError::InvalidTableEdit)?;
        let kind = if self.engine == SqlEngine::Sqlite {
            match original {
                Some(CellValue::Text(_)) => ValueKind::Text,
                Some(CellValue::Integer(_)) => ValueKind::Integer,
                Some(CellValue::Real(_)) => ValueKind::Float,
                Some(CellValue::Boolean(_)) => ValueKind::Boolean,
                Some(CellValue::Binary(_)) => ValueKind::Binary,
                Some(CellValue::InvalidText(_)) => return Err(DatabaseError::InvalidTableEdit),
                _ => column.kind,
            }
        } else {
            column.kind
        };
        CellValue::parse_kind(kind, text)
    }
}

/// Numeric text remains exact; decimal values never pass through an f64.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CellValue {
    Null,
    Text(String),
    Integer(String),
    Real(String),
    Boolean(bool),
    Binary(Vec<u8>),
    /// SQLite can store non-UTF-8 TEXT. It remains viewable but cannot be edited.
    InvalidText(Vec<u8>),
}

impl CellValue {
    pub fn parse(column: &ColumnInfo, text: &str) -> Result<Self> {
        Self::parse_kind(column.kind, text)
    }

    fn parse_kind(kind: ValueKind, text: &str) -> Result<Self> {
        let value = match kind {
            ValueKind::Text => Self::Text(text.to_owned()),
            ValueKind::Integer if statements::valid_number(text, true) => {
                Self::Integer(text.to_owned())
            }
            ValueKind::Decimal if statements::valid_number(text, false) => {
                Self::Real(text.to_owned())
            }
            ValueKind::Float
                if statements::valid_number(text, false)
                    && text.parse::<f64>().is_ok_and(f64::is_finite) =>
            {
                Self::Real(text.to_owned())
            }
            ValueKind::Boolean => match text.to_ascii_lowercase().as_str() {
                "true" | "t" | "1" => Self::Boolean(true),
                "false" | "f" | "0" => Self::Boolean(false),
                _ => return Err(DatabaseError::InvalidTableEdit),
            },
            ValueKind::Binary => Self::Binary(statements::decode_hex(text)?),
            _ => return Err(DatabaseError::InvalidTableEdit),
        };
        Ok(value)
    }

    pub fn display(&self) -> Option<String> {
        match self {
            Self::Null => None,
            Self::Text(value) | Self::Integer(value) | Self::Real(value) => Some(value.clone()),
            Self::Boolean(value) => Some(value.to_string()),
            Self::Binary(value) | Self::InvalidText(value) => {
                Some(format!("0x{}", super::hex(value)))
            }
        }
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum FilterOperator {
    #[default]
    Equals,
    NotEquals,
    Contains,
    Greater,
    GreaterOrEqual,
    Less,
    LessOrEqual,
    IsNull,
    IsNotNull,
}

impl FilterOperator {
    pub const ALL: [Self; 9] = [
        Self::Equals,
        Self::NotEquals,
        Self::Contains,
        Self::Greater,
        Self::GreaterOrEqual,
        Self::Less,
        Self::LessOrEqual,
        Self::IsNull,
        Self::IsNotNull,
    ];

    pub fn label(self) -> &'static str {
        match self {
            Self::Equals => "=",
            Self::NotEquals => "≠",
            Self::Contains => crate::i18n::tr(i18n_key::DATABASE_CONTAINS),
            Self::Greater => ">",
            Self::GreaterOrEqual => "≥",
            Self::Less => "<",
            Self::LessOrEqual => "≤",
            Self::IsNull => "IS NULL",
            Self::IsNotNull => "IS NOT NULL",
        }
    }

    pub const fn needs_value(self) -> bool {
        !matches!(self, Self::IsNull | Self::IsNotNull)
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ColumnFilter {
    pub column: usize,
    pub operator: FilterOperator,
    pub value: CellValue,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SortOrder {
    pub column: usize,
    pub descending: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PageRequest {
    pub offset: u64,
    pub limit: usize,
    pub filter: Option<ColumnFilter>,
    pub order: Option<SortOrder>,
}

impl Default for PageRequest {
    fn default() -> Self {
        Self {
            offset: 0,
            limit: 200,
            filter: None,
            order: None,
        }
    }
}

#[derive(Clone, Debug)]
pub struct TablePage {
    pub result: ResultSet,
    pub originals: Vec<Vec<CellValue>>,
    pub has_next: bool,
    pub offset: u64,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RowChange {
    Insert {
        values: Vec<(usize, CellValue)>,
    },
    Update {
        original: Vec<CellValue>,
        values: Vec<(usize, CellValue)>,
    },
    Delete {
        original: Vec<CellValue>,
    },
}

/// Generated mutations are executed atomically by the session actor.
pub struct GuardedStatement {
    pub sql: String,
    pub expected_rows: u64,
}

impl DatabaseClient {
    pub async fn table_schema(&self, table: TableInfo) -> Result<TableSchema> {
        super::check_table(&table)?;
        if self.0.engine == SqlEngine::Sqlite {
            super::check_sqlite_schema(&table.schema)?;
        }
        metadata::load(self, table).await
    }

    pub async fn table_page(
        &self,
        schema: &TableSchema,
        request: PageRequest,
    ) -> Result<TablePage> {
        if schema.engine != self.0.engine || !(1..=MAX_ROWS).contains(&request.limit) {
            return Err(DatabaseError::Limit);
        }
        let sql = statements::page(schema, &request)?;
        // One look-ahead row is internal; visible pages still obey MAX_ROWS.
        let mut output = self
            .request(super::Command::Sql {
                sql,
                row_limit: request.limit + 1,
            })
            .await?;
        super::ensure_complete(&output)?;
        if output.results.len() != 1 {
            return Err(DatabaseError::Protocol);
        }
        let mut result = output.results.remove(0);
        let has_next = result.rows.len() > request.limit;
        result.rows.truncate(request.limit);
        let originals = statements::decode_rows(schema, &mut result)?;
        result.columns = schema
            .columns
            .iter()
            .map(|column| column.name.clone())
            .collect();
        Ok(TablePage {
            result,
            originals,
            has_next,
            offset: request.offset,
        })
    }

    /// Returns the exact mutation statements shown in the pending-changes preview.
    pub fn preview_changes(
        &self,
        schema: &TableSchema,
        changes: &[RowChange],
    ) -> Result<Vec<String>> {
        if schema.engine != self.0.engine {
            return Err(DatabaseError::InvalidTableEdit);
        }
        Ok(statements::changes(schema, changes)?
            .into_iter()
            .map(|statement| statement.sql)
            .collect())
    }

    pub async fn apply_changes(&self, schema: &TableSchema, changes: Vec<RowChange>) -> Result<()> {
        if schema.engine != self.0.engine || self.is_read_only() {
            return Err(DatabaseError::TableReadOnly);
        }
        let statements = statements::changes(schema, &changes)?;
        if statements.is_empty() {
            return Ok(());
        }
        self.apply_batch(statements).await
    }
}
