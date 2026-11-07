use super::*;
use ui::Icon;

pub(super) fn is_view(table: &TableInfo) -> bool {
    table.kind.to_ascii_lowercase().contains("view")
}

impl TableAction {
    pub(super) fn label(self, engine: SqlEngine) -> &'static str {
        match self {
            Self::Details => i18n_key::DATABASE_OBJECT_DETAILS,
            Self::Data => "Data",
            Self::Columns => "Columns",
            Self::Indexes if engine == SqlEngine::ClickHouse => i18n_key::DATABASE_SKIP_INDEXES,
            Self::Indexes => "Indexes",
            Self::Constraints if engine == SqlEngine::Sqlite => i18n_key::DATABASE_FOREIGN_KEYS,
            Self::Constraints => i18n_key::DATABASE_CONSTRAINTS,
            Self::Parts => i18n_key::DATABASE_PARTS,
            Self::Definition => i18n_key::DATABASE_TABLE_DEFINITION,
        }
    }
    pub(super) fn icon(self) -> Icon {
        match self {
            Self::Data => Icon::Table,
            Self::Definition => Icon::File,
            Self::Columns => Icon::Sidebar,
            Self::Indexes | Self::Constraints => Icon::Key,
            Self::Parts => Icon::Folder,
            Self::Details => Icon::Settings,
        }
    }
}

pub(super) fn sections(engine: SqlEngine, table: &TableInfo) -> Vec<TableAction> {
    use TableAction::*;
    let mut sections = vec![Data, Details, Columns];
    if !is_view(table) || table.kind == "MATERIALIZED VIEW" {
        sections.push(Indexes);
        if engine != SqlEngine::ClickHouse {
            sections.push(Constraints);
        }
    }
    if engine == SqlEngine::ClickHouse && table.kind.contains("MergeTree") {
        sections.push(Parts);
    }
    if engine != SqlEngine::Postgres || is_view(table) {
        sections.push(Definition);
    }
    sections
}

pub(super) struct Template {
    pub label: &'static str,
    pub hint: &'static str,
    pub sql: &'static str,
}

pub(super) fn templates(engine: SqlEngine) -> [Template; 2] {
    use SqlEngine::*;
    match engine {
        ClickHouse => [
            Template {
                label: i18n_key::DATABASE_LIVE_QUERIES,
                hint: i18n_key::DATABASE_CH_QUERIES_HINT,
                sql: "SELECT query_id, user, elapsed, read_rows, memory_usage, query\nFROM system.processes\nWHERE query_id != currentQueryID()\nORDER BY elapsed DESC;",
            },
            Template {
                label: i18n_key::DATABASE_STORAGE,
                hint: i18n_key::DATABASE_CH_STORAGE_HINT,
                sql: "SELECT table, partition, sum(rows) AS rows,\n       formatReadableSize(sum(bytes_on_disk)) AS size\nFROM system.parts\nWHERE active AND database = currentDatabase()\nGROUP BY table, partition\nORDER BY table, partition;",
            },
        ],
        Postgres => [
            Template {
                label: i18n_key::DATABASE_LIVE_QUERIES,
                hint: i18n_key::DATABASE_PG_QUERIES_HINT,
                sql: "SELECT pid, usename, state, query_start, wait_event_type, query\nFROM pg_catalog.pg_stat_activity\nWHERE datname = current_database() AND pid <> pg_backend_pid()\nORDER BY query_start;",
            },
            Template {
                label: i18n_key::DATABASE_STORAGE,
                hint: i18n_key::DATABASE_PG_STORAGE_HINT,
                sql: "SELECT schemaname, relname, n_live_tup AS estimated_rows,\n       pg_size_pretty(pg_total_relation_size(relid)) AS total_size\nFROM pg_catalog.pg_stat_user_tables\nORDER BY pg_total_relation_size(relid) DESC;",
            },
        ],
        Mysql => [
            Template {
                label: i18n_key::DATABASE_LIVE_QUERIES,
                hint: i18n_key::DATABASE_MY_QUERIES_HINT,
                sql: "SHOW FULL PROCESSLIST;",
            },
            Template {
                label: i18n_key::DATABASE_STORAGE,
                hint: i18n_key::DATABASE_MY_STORAGE_HINT,
                sql: "SELECT TABLE_NAME, ENGINE, TABLE_ROWS AS estimated_rows,\n       DATA_LENGTH, INDEX_LENGTH\nFROM information_schema.TABLES\nWHERE TABLE_SCHEMA = DATABASE()\nORDER BY DATA_LENGTH DESC;",
            },
        ],
        Sqlite => [
            Template {
                label: i18n_key::DATABASE_FILE_INFO,
                hint: i18n_key::DATABASE_SQLITE_FILE_HINT,
                sql: "PRAGMA database_list;\nPRAGMA main.page_count;\nPRAGMA main.page_size;\nPRAGMA main.journal_mode;",
            },
            Template {
                label: i18n_key::DATABASE_INTEGRITY,
                hint: i18n_key::DATABASE_SQLITE_CHECK_HINT,
                sql: "PRAGMA main.quick_check;",
            },
        ],
    }
}

impl DatabaseView {
    pub(super) fn supports_transactions(&self) -> bool {
        self.engine() != SqlEngine::ClickHouse
            && !matches!(&self.config, DatabaseConfig::Sqlite(c) if c.read_only)
    }
    pub(super) fn engine_color(&self) -> u32 {
        match self.engine() {
            SqlEngine::Mysql => ui::colors().teal,
            SqlEngine::Postgres => ui::colors().blue,
            SqlEngine::ClickHouse => ui::colors().warning,
            SqlEngine::Sqlite => ui::colors().accent,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn object_sections_follow_engine_and_object_capabilities() {
        let mut table = TableInfo {
            schema: "public".into(),
            name: "items".into(),
            kind: "TABLE".into(),
        };
        assert!(!sections(SqlEngine::Postgres, &table).contains(&TableAction::Definition));
        assert!(sections(SqlEngine::Sqlite, &table).contains(&TableAction::Constraints));
        table.kind = "VIEW".into();
        assert!(sections(SqlEngine::Postgres, &table).contains(&TableAction::Definition));
        assert!(!sections(SqlEngine::Mysql, &table).contains(&TableAction::Indexes));
        table.kind = "ReplicatedMergeTree".into();
        let ch = sections(SqlEngine::ClickHouse, &table);
        assert!(ch.contains(&TableAction::Parts));
        assert!(!ch.contains(&TableAction::Constraints));
        table.kind = "Memory".into();
        assert!(!sections(SqlEngine::ClickHouse, &table).contains(&TableAction::Parts));
    }
}
