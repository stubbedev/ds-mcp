//! Classifies SQL as read-only with a real parser, not string prefixes.
//! Policy (same as the Go sqlguard): exactly one statement, and it must be a
//! SELECT / SHOW / DESCRIBE / EXPLAIN. Anything that fails to parse is
//! rejected — the safe direction; execute is the escape hatch.

use sqlparser::ast::Statement;
use sqlparser::dialect::{
    ClickHouseDialect, Dialect, DuckDbDialect, GenericDialect, MsSqlDialect, MySqlDialect,
    PostgreSqlDialect, SQLiteDialect,
};
use sqlparser::parser::Parser;

use crate::config::EngineKind;

fn dialect(kind: EngineKind) -> Box<dyn Dialect> {
    match kind {
        EngineKind::MySql | EngineKind::MariaDb => Box::new(MySqlDialect {}),
        EngineKind::Postgres => Box::new(PostgreSqlDialect {}),
        EngineKind::Sqlite => Box::new(SQLiteDialect {}),
        EngineKind::DuckDb => Box::new(DuckDbDialect {}),
        EngineKind::Mssql => Box::new(MsSqlDialect {}),
        EngineKind::ClickHouse => Box::new(ClickHouseDialect {}),
        // unreachable in practice (non-SQL engines never parse SQL)
        EngineKind::Redis
        | EngineKind::Valkey
        | EngineKind::MongoDb
        | EngineKind::Elasticsearch
        | EngineKind::OpenSearch
        | EngineKind::Qdrant => Box::new(GenericDialect {}),
    }
}

pub fn ensure_read_only(kind: EngineKind, sql: &str) -> Result<(), String> {
    let statements = Parser::parse_sql(dialect(kind).as_ref(), sql)
        .map_err(|e| format!("query is not parseable as read-only SQL ({e}); use execute on a writable source if this is intentional"))?;
    match statements.as_slice() {
        [] => Err("empty query".into()),
        [stmt] => ensure_stmt_read_only(stmt),
        _ => Err("multiple statements are not allowed in read_query".into()),
    }
}

fn ensure_stmt_read_only(stmt: &Statement) -> Result<(), String> {
    match stmt {
        Statement::Query(q) => {
            // SELECT ... INTO OUTFILE / INTO table writes despite being a Query.
            if let sqlparser::ast::SetExpr::Select(s) = q.body.as_ref()
                && s.into.is_some()
            {
                return Err("SELECT INTO writes; use execute".into());
            }
            Ok(())
        }
        Statement::Explain { .. } | Statement::ExplainTable { .. } => Ok(()),
        Statement::ShowColumns { .. }
        | Statement::ShowTables { .. }
        | Statement::ShowDatabases { .. }
        | Statement::ShowSchemas { .. }
        | Statement::ShowCreate { .. }
        | Statement::ShowFunctions { .. }
        | Statement::ShowVariable { .. }
        | Statement::ShowVariables { .. }
        | Statement::ShowStatus { .. }
        | Statement::ShowCollation { .. }
        | Statement::ShowViews { .. }
        | Statement::ShowObjects { .. } => Ok(()),
        other => Err(format!(
            "statement is not read-only ({}); use execute",
            stmt_kind(other)
        )),
    }
}

fn stmt_kind(stmt: &Statement) -> &'static str {
    match stmt {
        Statement::Insert { .. } => "INSERT",
        Statement::Update { .. } => "UPDATE",
        Statement::Delete { .. } => "DELETE",
        Statement::CreateTable { .. } => "CREATE TABLE",
        Statement::AlterTable { .. } => "ALTER TABLE",
        Statement::Drop { .. } => "DROP",
        Statement::Truncate { .. } => "TRUNCATE",
        _ => "not a SELECT/SHOW/DESCRIBE/EXPLAIN",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use EngineKind::*;

    #[test]
    fn allows_reads() {
        for (kind, sql) in [
            (MySql, "SELECT * FROM t WHERE id = 1"),
            (MySql, "SELECT a FROM t UNION SELECT b FROM u"),
            (MySql, "SHOW TABLES"),
            (MySql, "SHOW DATABASES"),
            (MySql, "DESCRIBE t"),
            (MySql, "EXPLAIN SELECT 1"),
            (Postgres, "SELECT * FROM t LIMIT 5"),
            (Postgres, "EXPLAIN ANALYZE SELECT 1"),
            (Sqlite, "SELECT count(*) FROM sqlite_master"),
            (Mssql, "SELECT TOP 10 * FROM t"),
            (MariaDb, "WITH x AS (SELECT 1) SELECT * FROM x"),
        ] {
            assert!(
                ensure_read_only(kind, sql).is_ok(),
                "{kind:?}: {sql} should be allowed"
            );
        }
    }

    #[test]
    fn rejects_writes() {
        for (kind, sql) in [
            (MySql, "INSERT INTO t VALUES (1)"),
            (MySql, "UPDATE t SET a = 1"),
            (MySql, "DELETE FROM t"),
            (MySql, "DROP TABLE t"),
            (MySql, "CREATE TABLE t (id INT)"),
            (MySql, "TRUNCATE TABLE t"),
            (Postgres, "ALTER TABLE t ADD COLUMN c INT"),
            (Sqlite, "REPLACE INTO t VALUES (1)"),
        ] {
            assert!(
                ensure_read_only(kind, sql).is_err(),
                "{kind:?}: {sql} should be rejected"
            );
        }
    }

    #[test]
    fn rejects_stacked_statements() {
        assert!(ensure_read_only(MySql, "SELECT 1; DROP TABLE t").is_err());
        assert!(ensure_read_only(MySql, "SELECT 1; SELECT 2").is_err());
    }

    #[test]
    fn rejects_unparseable() {
        assert!(ensure_read_only(MySql, "FLUSH PRIVILEGES WAT").is_err());
        assert!(ensure_read_only(MySql, "").is_err());
    }
}
