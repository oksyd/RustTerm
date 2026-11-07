//! Engine-specific object inspection. All names come from quoted catalog values.
use super::*;

impl DatabaseClient {
    pub async fn table_details(&self, table: TableInfo) -> Result<QueryOutput> {
        check_table(&table)?;
        let engine = self.0.engine;
        let schema = quote_literal(engine, &table.schema);
        let name = quote_literal(engine, &table.name);
        let sql = match engine {
            SqlEngine::ClickHouse => format!(
                "SELECT engine, partition_key, sorting_key, primary_key, total_rows, total_bytes, comment FROM system.tables WHERE database={schema} AND name={name}"
            ),
            SqlEngine::Mysql => format!(
                "SELECT TABLE_TYPE, ENGINE, TABLE_ROWS AS estimated_rows, DATA_LENGTH, INDEX_LENGTH, TABLE_COLLATION, TABLE_COMMENT FROM information_schema.TABLES WHERE TABLE_SCHEMA={schema} AND TABLE_NAME={name}"
            ),
            SqlEngine::Postgres => format!(
                "SELECT pg_catalog.pg_get_userbyid(c.relowner) AS owner, c.relkind AS relation_kind, c.reltuples AS estimated_rows, c.relpersistence AS persistence, c.relrowsecurity AS row_security, pg_catalog.obj_description(c.oid, 'pg_class') AS comment FROM pg_catalog.pg_class c JOIN pg_catalog.pg_namespace n ON n.oid=c.relnamespace WHERE n.nspname={schema} AND c.relname={name}"
            ),
            SqlEngine::Sqlite => {
                check_sqlite_schema(&table.schema)?;
                format!(
                    "SELECT type, name, tbl_name, rootpage FROM {}.sqlite_schema WHERE name={name} AND type IN ('table', 'view')",
                    quote_identifier(engine, &table.schema)
                )
            }
        };
        self.execute(sql, 1).await
    }

    pub async fn constraints(&self, table: TableInfo) -> Result<QueryOutput> {
        check_table(&table)?;
        let engine = self.0.engine;
        let schema = quote_literal(engine, &table.schema);
        let name = quote_literal(engine, &table.name);
        let sql = match engine {
            SqlEngine::ClickHouse => return Err(DatabaseError::InvalidConfig),
            SqlEngine::Mysql => format!(
                "SELECT t.CONSTRAINT_NAME, t.CONSTRAINT_TYPE, k.COLUMN_NAME, k.REFERENCED_TABLE_SCHEMA, k.REFERENCED_TABLE_NAME, k.REFERENCED_COLUMN_NAME FROM information_schema.TABLE_CONSTRAINTS t LEFT JOIN information_schema.KEY_COLUMN_USAGE k ON k.CONSTRAINT_SCHEMA=t.CONSTRAINT_SCHEMA AND k.TABLE_NAME=t.TABLE_NAME AND k.CONSTRAINT_NAME=t.CONSTRAINT_NAME WHERE t.TABLE_SCHEMA={schema} AND t.TABLE_NAME={name} ORDER BY t.CONSTRAINT_NAME, k.ORDINAL_POSITION"
            ),
            SqlEngine::Postgres => format!(
                "SELECT co.conname AS name, co.contype AS type, pg_catalog.pg_get_constraintdef(co.oid, true) AS definition FROM pg_catalog.pg_constraint co JOIN pg_catalog.pg_class c ON c.oid=co.conrelid JOIN pg_catalog.pg_namespace n ON n.oid=c.relnamespace WHERE n.nspname={schema} AND c.relname={name} ORDER BY co.conname"
            ),
            SqlEngine::Sqlite => {
                check_sqlite_schema(&table.schema)?;
                format!("SELECT * FROM pragma_foreign_key_list({name}, {schema}) ORDER BY id, seq")
            }
        };
        self.execute(sql, MAX_ROWS).await
    }

    pub async fn parts(&self, table: TableInfo) -> Result<QueryOutput> {
        check_table(&table)?;
        if self.0.engine != SqlEngine::ClickHouse {
            return Err(DatabaseError::InvalidConfig);
        }
        let schema = quote_literal(self.0.engine, &table.schema);
        let name = quote_literal(self.0.engine, &table.name);
        self.execute(format!("SELECT partition, name, rows, bytes_on_disk, modification_time FROM system.parts WHERE active AND database={schema} AND table={name} ORDER BY partition, name"), MAX_ROWS).await
    }

    pub async fn definition(&self, table: TableInfo) -> Result<QueryOutput> {
        check_table(&table)?;
        let engine = self.0.engine;
        let schema = quote_literal(engine, &table.schema);
        let name = quote_literal(engine, &table.name);
        let sql = match engine {
            SqlEngine::ClickHouse | SqlEngine::Mysql => format!(
                "SHOW CREATE TABLE {}.{}",
                quote_identifier(engine, &table.schema),
                quote_identifier(engine, &table.name)
            ),
            SqlEngine::Sqlite => {
                check_sqlite_schema(&table.schema)?;
                format!(
                    "SELECT sql FROM {}.sqlite_schema WHERE name={name} AND type IN ('table', 'view')",
                    quote_identifier(engine, &table.schema)
                )
            }
            SqlEngine::Postgres => format!(
                "SELECT pg_catalog.pg_get_viewdef(c.oid, true) AS definition FROM pg_catalog.pg_class c JOIN pg_catalog.pg_namespace n ON n.oid=c.relnamespace WHERE n.nspname={schema} AND c.relname={name} AND c.relkind IN ('v', 'm')"
            ),
        };
        let mut output = self.execute(sql, 1).await?;
        // MySQL SHOW CREATE returns the object name before the definition (and
        // extra character-set fields for views). Expose one consistent SQL cell.
        if engine == SqlEngine::Mysql {
            for result in &mut output.results {
                result.columns = vec!["definition".into()];
                for row in &mut result.rows {
                    *row = vec![row.get(1).cloned().flatten()];
                }
            }
        }
        Ok(output)
    }
}
