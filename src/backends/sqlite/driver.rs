//! Connected SQLite migration execution.

use super::{
    ledger::{self, LEDGER_NAME, SqliteError},
    operators::quote_identifier,
};
use crate::{driver::Driver, migrations::Migration};
use rusqlite::{Batch, Connection, OpenFlags, Transaction, TransactionBehavior};
use std::path::Path;

/// SQLite migration backend. Owns a fresh connection so migration SQL cannot
/// inherit caller-installed hooks, TEMP objects, or attached databases.
///
/// Migration SQL cannot manage transactions, attach databases, modify the
/// ledger, change SQLite PRAGMAs, or create TEMP schema objects.
pub struct SqliteDriver {
    pub(super) connection: Connection,
}

impl SqliteDriver {
    pub fn open(path: impl AsRef<Path>) -> Result<Self, SqliteError> {
        Ok(Self {
            connection: Connection::open(path)?,
        })
    }

    pub fn open_read_only(path: &Path) -> Result<Self, SqliteError> {
        Self::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_ONLY)
    }

    pub fn open_existing(path: &Path) -> Result<Self, SqliteError> {
        Self::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_WRITE)
    }

    fn open_with_flags(path: &Path, flags: OpenFlags) -> Result<Self, SqliteError> {
        Ok(Self {
            connection: Connection::open_with_flags(path, flags)?,
        })
    }

    pub fn open_in_memory() -> Result<Self, SqliteError> {
        Ok(Self {
            connection: Connection::open_in_memory()?,
        })
    }

    /// Read replayable main-schema DDL, without the ledger or SQLite-owned objects.
    pub(super) fn schema_sql(&self) -> Result<Vec<String>, SqliteError> {
        let mut statement = self.connection.prepare(
            "SELECT s.sql FROM main.sqlite_schema AS s
             WHERE s.sql IS NOT NULL
               AND substr(s.name, 1, 7) <> 'sqlite_' COLLATE NOCASE
               AND s.name <> ?1 COLLATE NOCASE
               AND NOT EXISTS (
                   SELECT 1 FROM pragma_table_list AS t
                   WHERE t.schema = 'main' AND t.type = 'shadow'
                     AND t.name = s.tbl_name AND s.type <> 'view'
               )
             ORDER BY CASE s.type WHEN 'table' THEN 0 WHEN 'index' THEN 1
                                  WHEN 'view' THEN 2 ELSE 3 END, s.rowid",
        )?;
        Ok(statement
            .query_map([LEDGER_NAME], |row| row.get(0))?
            .collect::<rusqlite::Result<_>>()?)
    }

    pub fn pull(&self) -> Result<String, SqliteError> {
        Ok(self
            .schema_sql()?
            .into_iter()
            .map(|sql| format!("{sql};\n"))
            .collect())
    }

    pub(super) fn load_desired_schema(&mut self, sql: &str) -> Result<(), SqliteError> {
        let tx = self.connection.transaction()?;
        guarded_batch(&tx, sql)?;
        {
            // SQLite accepts CREATE VIEW even if the query cannot resolve.
            let mut views =
                tx.prepare("SELECT name FROM main.sqlite_schema WHERE type = 'view'")?;
            for name in views.query_map([], |row| row.get::<_, String>(0))? {
                let name = quote_identifier(&name?);
                tx.prepare(&format!("SELECT * FROM main.{name} LIMIT 0"))?;
            }
        }
        tx.commit()?;
        Ok(())
    }

    fn run(&mut self, migration: &Migration, up: bool) -> Result<(), SqliteError> {
        let artifact = if up {
            &migration.up
        } else {
            migration
                .down
                .as_ref()
                .ok_or_else(|| SqliteError::Invalid("migration has no Down SQL".into()))?
        };
        let tx = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        if up {
            ledger::ensure(&tx)?;
            ledger::check_unapplied(&tx, migration.id)?;
        } else {
            ledger::check_latest(&tx, migration.id)?;
        }
        guarded_batch(&tx, &artifact.sql)?;
        if up {
            ledger::append(&tx, migration.id)?;
        } else {
            ledger::remove(&tx, migration.id)?;
        }
        tx.commit()?;
        Ok(())
    }
}

fn guarded_batch(tx: &Transaction<'_>, sql: &str) -> Result<(), SqliteError> {
    use rusqlite::fallible_iterator::FallibleIterator;
    use rusqlite::hooks::{AuthAction, AuthContext, Authorization};
    tx.authorizer(Some(|ctx: AuthContext<'_>| match ctx.action {
        AuthAction::Transaction { .. }
        | AuthAction::Savepoint { .. }
        | AuthAction::Attach { .. }
        | AuthAction::Detach { .. } => Authorization::Deny,
        // Bundled virtual-table modules consult the database page size.
        AuthAction::Pragma {
            pragma_name,
            pragma_value: None,
        } if pragma_name.eq_ignore_ascii_case("page_size") => Authorization::Allow,
        AuthAction::Pragma { .. } => Authorization::Deny,
        AuthAction::CreateTempTable { .. }
        | AuthAction::CreateTempIndex { .. }
        | AuthAction::CreateTempTrigger { .. }
        | AuthAction::CreateTempView { .. }
        | AuthAction::DropTempTable { .. }
        | AuthAction::DropTempIndex { .. }
        | AuthAction::DropTempTrigger { .. }
        | AuthAction::DropTempView { .. } => Authorization::Deny,
        AuthAction::Insert { table_name }
        | AuthAction::Update { table_name, .. }
        | AuthAction::Delete { table_name }
        | AuthAction::CreateIndex { table_name, .. }
        | AuthAction::DropIndex { table_name, .. }
        | AuthAction::CreateTrigger { table_name, .. }
        | AuthAction::DropTrigger { table_name, .. }
        | AuthAction::DropTable { table_name }
        | AuthAction::AlterTable { table_name, .. }
            if table_name.eq_ignore_ascii_case(LEDGER_NAME) =>
        {
            Authorization::Deny
        }
        _ => Authorization::Allow,
    }))?;
    let result = (|| {
        let mut batch = Batch::new(tx, sql);
        while let Some(mut statement) = batch.next()? {
            let mut rows = statement.raw_query();
            while rows.next()?.is_some() {}
            drop(rows);
            drop(statement);
            if tx.is_autocommit() {
                return Err(SqliteError::Invalid(
                    "migration transaction ended unexpectedly".into(),
                ));
            }
        }
        Ok(())
    })();
    let cleanup = tx.authorizer(None::<fn(rusqlite::hooks::AuthContext<'_>) -> Authorization>);
    cleanup?;
    result
}

impl Driver for SqliteDriver {
    fn apply(&mut self, migration: &Migration) -> Result<(), Self::Error> {
        self.run(migration, true)
    }
    fn revert(&mut self, migration: &Migration) -> Result<(), Self::Error> {
        self.run(migration, false)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::backends::sqlite::{
        operators::{Operator, QualifiedName},
        schema::{SchemaObject, SchemaObjectKind},
    };
    use crate::migrations::{AppliedMigration, Artifact, Ledger, tests::assert_commit_list_revert};

    fn migration(id: u64, up: &str, down: Option<&str>) -> Migration {
        Migration {
            id,
            up: Artifact {
                path: "up.sql".into(),
                sql: up.into(),
            },
            down: down.map(|sql| Artifact {
                path: "down.sql".into(),
                sql: sql.into(),
            }),
        }
    }

    #[test]
    fn implements_shared_ledger_contract() {
        assert_commit_list_revert(SqliteDriver::open_in_memory().unwrap());
    }

    #[test]
    fn preserves_u64_and_rolls_back_failed_batch() {
        let mut driver = SqliteDriver::open_in_memory().unwrap();
        let id = u64::MAX;
        driver.commit(AppliedMigration { id }).unwrap();
        assert_eq!(driver.list().unwrap()[0].id, id);
        let error = driver.apply(&migration(
            3,
            "CREATE TABLE transient(x); SELECT * FROM missing;",
            Some("DROP TABLE transient;"),
        ));
        assert!(error.is_err());
        let exists: bool = driver
            .connection
            .query_row(
                "SELECT EXISTS(SELECT 1 FROM main.sqlite_schema WHERE name='transient')",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert!(!exists);
        assert_eq!(driver.list().unwrap()[0].id, id);
    }

    #[test]
    fn rejects_transaction_control_without_breaking_atomicity() {
        let mut driver = SqliteDriver::open_in_memory().unwrap();
        let error = driver.apply(&migration(
            1,
            "CREATE TABLE x(a); COMMIT;",
            Some("DROP TABLE x;"),
        ));
        assert!(error.is_err());
        assert!(driver.list().unwrap().is_empty());
        let exists: bool = driver
            .connection
            .query_row(
                "SELECT EXISTS(SELECT 1 FROM main.sqlite_schema WHERE name='x')",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert!(!exists);
    }

    #[test]
    fn applies_and_reverts_rendered_schema_operations() {
        let mut driver = SqliteDriver::open_in_memory().unwrap();
        let create = Operator::Create(
            SchemaObject::from_sql("CREATE TABLE items (id INTEGER) -- table").unwrap(),
        );
        let add = Operator::AddColumn {
            table: QualifiedName::new("items"),
            column_definition: "name TEXT".into(),
        };
        let drop = Operator::Drop {
            kind: SchemaObjectKind::Table,
            name: QualifiedName::new("items"),
            if_exists: false,
        };
        let up = format!("{}\n{}", create.to_sql(), add.to_sql());
        let migration = migration(1, &up, Some(&drop.to_sql()));

        driver.apply(&migration).unwrap();
        assert_eq!(driver.list().unwrap(), [AppliedMigration { id: 1 }]);
        assert!(driver.connection.prepare("SELECT name FROM items").is_ok());
        Driver::revert(&mut driver, &migration).unwrap();
        assert!(driver.list().unwrap().is_empty());
        assert!(driver.connection.prepare("SELECT name FROM items").is_err());
    }

    #[test]
    fn reads_without_initializing_and_rejects_legacy_history() {
        let mut driver = SqliteDriver::open_in_memory().unwrap();
        assert!(driver.list().unwrap().is_empty());
        driver
            .connection
            .execute_batch(
                "CREATE TABLE _fledgling_migrations (id INTEGER PRIMARY KEY, digest BLOB);",
            )
            .unwrap();
        assert!(
            driver
                .list()
                .unwrap_err()
                .to_string()
                .contains("incompatible")
        );
        assert!(
            driver
                .apply(&migration(1, "CREATE TABLE items (id);", None))
                .is_err()
        );
    }

    #[test]
    fn failed_down_keeps_schema_and_history() {
        let mut driver = SqliteDriver::open_in_memory().unwrap();
        let migration = migration(
            1,
            "CREATE TABLE items (id);",
            Some("DROP TABLE items; DELETE FROM main._fledgling_migrations;"),
        );
        driver.apply(&migration).unwrap();
        assert!(Driver::revert(&mut driver, &migration).is_err());
        assert_eq!(driver.list().unwrap(), [AppliedMigration { id: 1 }]);
        assert!(driver.connection.prepare("SELECT id FROM items").is_ok());
    }

    #[test]
    fn applies_virtual_table_ddl() {
        let mut driver = SqliteDriver::open_in_memory().unwrap();
        let migration = migration(
            1,
            "CREATE VIRTUAL TABLE docs USING fts5(body); INSERT INTO docs VALUES ('example');",
            Some("DROP TABLE docs;"),
        );
        driver.apply(&migration).unwrap();
        Driver::revert(&mut driver, &migration).unwrap();
    }
}
