//! SQLite command execution and main-schema inspection.

use std::path::Path;

use rusqlite::{
    Batch, Connection, OpenFlags, Transaction, TransactionBehavior,
    fallible_iterator::FallibleIterator,
    hooks::{AuthAction, AuthContext, Authorization},
};

use super::{
    SqliteError,
    ledger::{LEDGER_NAME, SqliteLedger},
    objects::SqliteObject,
};
use crate::{driver::Driver, schema::Schema};

// ----------------------------------------------------------------------------
// Driver
// ----------------------------------------------------------------------------

/// How to open an SQLite database file.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum OpenMode {
    /// Open an existing database without write access.
    Read,
    /// Open for reading and writing, creating the database if needed.
    Write,
    /// Open an existing database for reading and writing.
    WriteExisting,
}

/// A connected SQLite driver. Executing commands never updates applied history.
///
/// The driver expects trusted migration SQL. Its authorizer protects the
/// transaction and ledger; it is not a general-purpose SQL sandbox.
pub struct SqliteDriver {
    connection: Connection,
}

impl SqliteDriver {
    /// Open a database file with the requested access mode.
    pub fn open(path: &Path, mode: OpenMode) -> Result<Self, SqliteError> {
        let flags = match mode {
            OpenMode::Read => OpenFlags::SQLITE_OPEN_READ_ONLY,
            OpenMode::Write => OpenFlags::SQLITE_OPEN_READ_WRITE | OpenFlags::SQLITE_OPEN_CREATE,
            OpenMode::WriteExisting => OpenFlags::SQLITE_OPEN_READ_WRITE,
        };
        Self::new(Connection::open_with_flags(path, flags)?)
    }

    /// Open a disposable database that lasts as long as this driver.
    pub fn open_in_memory() -> Result<Self, SqliteError> {
        Self::new(Connection::open_in_memory()?)
    }

    fn new(connection: Connection) -> Result<Self, SqliteError> {
        connection.pragma_update(None, "foreign_keys", true)?;
        Ok(Self { connection })
    }

    /// Borrow history on this connection without starting a transaction.
    pub fn ledger(&self) -> SqliteLedger<'_> {
        SqliteLedger::new(&self.connection)
    }

    /// Read replayable main-schema DDL, excluding the ledger, automatic
    /// indexes, and other SQLite-owned objects (including shadow tables).
    pub fn schema_sql(&self) -> Result<Vec<String>, SqliteError> {
        let mut statement = self.connection.prepare(
            "SELECT s.sql FROM main.sqlite_schema AS s
             WHERE s.sql IS NOT NULL
               AND substr(s.name, 1, 7) <> 'sqlite_' COLLATE NOCASE
               AND s.name <> ?1 COLLATE NOCASE
               AND NOT EXISTS (
                   SELECT 1 FROM pragma_table_list AS t
                   WHERE t.schema = 'main' AND t.type = 'shadow' AND t.name = s.tbl_name
               )
             ORDER BY CASE s.type WHEN 'table' THEN 0 WHEN 'index' THEN 1
                                  WHEN 'view' THEN 2 ELSE 3 END, s.rowid",
        )?;
        Ok(statement
            .query_map([LEDGER_NAME], |row| row.get(0))?
            .collect::<rusqlite::Result<_>>()?)
    }

    /// Read the live schema as backend objects. The parser may lag SQLite's grammar.
    pub fn schema(&self) -> Result<Schema<SqliteObject>, SqliteError> {
        self.schema_sql()?
            .iter()
            .map(|sql| SqliteObject::from_sql(sql).map_err(|e| SqliteError::Invalid(e.to_string())))
            .collect::<Result<Vec<_>, _>>()
            .map(Schema::new)
    }

    pub(super) fn validate_views(&self, schema: &Schema<SqliteObject>) -> Result<(), SqliteError> {
        use super::{objects::SqliteObjectKind, operations::quote};

        // CREATE VIEW succeeds even when its query cannot resolve.
        for view in schema
            .objects()
            .iter()
            .filter(|o| o.kind() == SqliteObjectKind::View)
        {
            self.connection.prepare(&format!(
                "SELECT * FROM main.{} LIMIT 0",
                quote(view.name())
            ))?;
        }
        Ok(())
    }

    /// Start a transaction for a caller coordinating commands and history.
    pub fn transaction(&mut self) -> Result<SqliteTransaction<'_>, SqliteError> {
        Ok(SqliteTransaction {
            transaction: self
                .connection
                .transaction_with_behavior(TransactionBehavior::Immediate)?,
            failed: false,
        })
    }
}

impl Driver for SqliteDriver {
    type Error = SqliteError;

    /// Execute the entire batch in one transaction.
    fn execute(&mut self, commands: &str) -> Result<(), Self::Error> {
        let mut transaction = self.transaction()?;
        transaction.execute(commands)?;
        transaction.commit()
    }
}

// ----------------------------------------------------------------------------
// Shared transaction
// ----------------------------------------------------------------------------

/// A batch executor and ledger handle sharing one SQLite transaction.
///
/// Propagate both execution and ledger errors before calling [`Self::commit`].
/// Dropping the transaction without committing rolls back both kinds of writes.
///
/// ```
/// use fledgling::{backends::sqlite::driver::SqliteDriver, driver::Driver, migrations::Ledger};
///
/// # fn main() -> Result<(), Box<dyn std::error::Error>> {
/// let mut driver = SqliteDriver::open_in_memory()?;
/// let mut tx = driver.transaction()?;
/// tx.execute("CREATE TABLE books (id INTEGER PRIMARY KEY);")?;
/// tx.ledger()?.commit(1)?;
/// tx.commit()?;
/// assert_eq!(driver.ledger().list()?, [1]);
/// # Ok(())
/// # }
/// ```
pub struct SqliteTransaction<'a> {
    transaction: Transaction<'a>,
    failed: bool,
}

impl SqliteTransaction<'_> {
    fn check_active(&self) -> Result<(), SqliteError> {
        if self.failed || self.transaction.is_autocommit() {
            Err(SqliteError::Invalid(
                "migration transaction is no longer usable".into(),
            ))
        } else {
            Ok(())
        }
    }

    /// Borrow history inside this transaction, so changes roll back with SQL.
    pub fn ledger(&self) -> Result<SqliteLedger<'_>, SqliteError> {
        self.check_active()?;
        Ok(SqliteLedger::new(&self.transaction))
    }

    /// Commit SQL and history together. A failed SQL batch prevents commit.
    pub fn commit(self) -> Result<(), SqliteError> {
        self.check_active()?;
        Ok(self.transaction.commit()?)
    }
}

// ----------------------------------------------------------------------------
// Batch execution and authorization
// ----------------------------------------------------------------------------

impl Driver for SqliteTransaction<'_> {
    type Error = SqliteError;

    fn execute(&mut self, commands: &str) -> Result<(), Self::Error> {
        self.check_active()?;
        let result = execute_batch(&self.transaction, commands);
        self.failed = result.is_err();
        result
    }
}

// Guard the caller's transaction and reserved ledger while stepping every
// statement to completion (including statements with RETURNING rows).
fn execute_batch(transaction: &Transaction<'_>, sql: &str) -> Result<(), SqliteError> {
    if sql.contains('\0') {
        return Err(SqliteError::Invalid("SQL contains a NUL byte".into()));
    }
    let reserved_objects = || -> rusqlite::Result<i64> {
        transaction.query_row(
            "SELECT count(*) FROM main.sqlite_schema WHERE name = ?1 COLLATE NOCASE",
            [LEDGER_NAME],
            |row| row.get(0),
        )
    };
    let before = reserved_objects()?;
    transaction.authorizer(Some(authorize))?;
    let result = (|| {
        let mut batch = Batch::new(transaction, sql);
        while let Some(mut statement) = batch.next()? {
            if statement.parameter_count() != 0 {
                return Err(SqliteError::Invalid(
                    "migration SQL cannot use unbound parameters".into(),
                ));
            }
            let mut rows = statement.raw_query();
            while rows.next()?.is_some() {}
            if transaction.is_autocommit() {
                return Err(SqliteError::Invalid(
                    "migration transaction ended unexpectedly".into(),
                ));
            }
        }
        Ok(())
    })();
    transaction.authorizer(None::<fn(AuthContext<'_>) -> Authorization>)?;
    result?;
    // SQLite's ALTER TABLE authorizer reports the old name, not the new name.
    if reserved_objects()? != before {
        return Err(SqliteError::Invalid(format!(
            "{LEDGER_NAME} is reserved for migration history"
        )));
    }
    Ok(())
}

fn authorize(context: AuthContext<'_>) -> Authorization {
    use AuthAction::*;
    let reserved = |name: &str| name.eq_ignore_ascii_case(LEDGER_NAME);
    let denied = match context.action {
        Transaction { .. }
        | Savepoint { .. }
        | Attach { .. }
        | Detach { .. }
        | CreateTempTable { .. }
        | CreateTempIndex { .. }
        | CreateTempTrigger { .. }
        | CreateTempView { .. }
        | DropTempTable { .. }
        | DropTempIndex { .. }
        | DropTempTrigger { .. }
        | DropTempView { .. } => true,
        // Virtual tables don't have a separate temp action.
        CreateVtable { .. } | DropVtable { .. } if context.database_name == Some("temp") => true,
        // The bundled FTS module reads the page size when opening a virtual table.
        Pragma {
            pragma_name,
            pragma_value,
        } => !pragma_name.eq_ignore_ascii_case("page_size") || pragma_value.is_some(),
        Insert { table_name }
        | Delete { table_name }
        | Update { table_name, .. }
        | AlterTable { table_name, .. }
        | CreateTable { table_name }
        | DropTable { table_name }
        | CreateVtable { table_name, .. }
        | DropVtable { table_name, .. } => reserved(table_name),
        CreateIndex {
            index_name,
            table_name,
        }
        | DropIndex {
            index_name,
            table_name,
        } => reserved(index_name) || reserved(table_name),
        CreateTrigger {
            trigger_name,
            table_name,
        }
        | DropTrigger {
            trigger_name,
            table_name,
        } => reserved(trigger_name) || reserved(table_name),
        CreateView { view_name } | DropView { view_name } => reserved(view_name),
        _ => false,
    };
    if denied {
        Authorization::Deny
    } else {
        Authorization::Allow
    }
}

// ----------------------------------------------------------------------------
// Tests
// ----------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        backends::sqlite::{objects::SqliteObjectKind, operations::SqliteOperation},
        migrations::{Ledger, Operation},
    };

    #[test]
    fn executes_rendered_ddl_with_quoted_names() {
        use SqliteOperation::*;

        let mut driver = SqliteDriver::open_in_memory().unwrap();
        let table = "items.\"";
        for operation in [
            Create(
                SqliteObject::from_sql("CREATE TABLE [items.\"](id INTEGER) -- comment").unwrap(),
            ),
            AddColumn {
                table: table.into(),
                definition: "label TEXT".into(),
            },
            RenameColumn {
                table: table.into(),
                column: "label".into(),
                new_name: "title".into(),
            },
            AddCheck {
                table: table.into(),
                name: "id check".into(),
                expression: "id > 0".into(),
            },
            SetNotNull {
                table: table.into(),
                column: "title".into(),
                not_null: true,
            },
        ] {
            driver.execute(&operation.render().unwrap()).unwrap();
        }
        assert!(
            driver
                .execute("INSERT INTO [items.\"] VALUES (1, NULL)")
                .is_err()
        );
        assert!(
            driver
                .execute("INSERT INTO [items.\"] VALUES (0, 'title')")
                .is_err()
        );
        for operation in [
            DropCheck {
                table: table.into(),
                name: "id check".into(),
            },
            SetNotNull {
                table: table.into(),
                column: "title".into(),
                not_null: false,
            },
            DropColumn {
                table: table.into(),
                column: "title".into(),
            },
            RenameTable {
                table: table.into(),
                new_name: "renamed".into(),
            },
            Drop {
                kind: SqliteObjectKind::Table,
                name: "renamed".into(),
            },
        ] {
            driver.execute(&operation.render().unwrap()).unwrap();
        }
        assert!(driver.schema().unwrap().objects().is_empty());
    }

    #[test]
    fn inspects_user_objects_without_ledger_or_internal_tables() {
        let mut driver = SqliteDriver::open_in_memory().unwrap();
        driver
            .execute(
                "CREATE TABLE items(id INTEGER PRIMARY KEY AUTOINCREMENT, label UNIQUE);
             CREATE INDEX items_label ON items(label);
             CREATE VIEW labels AS SELECT label FROM items;
             CREATE TRIGGER items AFTER INSERT ON items BEGIN SELECT 1; SELECT 2; END;
             CREATE VIRTUAL TABLE docs USING fts5(body);
             INSERT INTO docs VALUES ('example');",
            )
            .unwrap();
        driver.ledger().commit(1).unwrap();
        let schema = driver.schema().unwrap();
        let names: Vec<_> = schema.objects().iter().map(SqliteObject::name).collect();
        assert_eq!(names, ["items", "docs", "items_label", "labels", "items"]);
    }

    #[test]
    fn shares_a_transaction_without_execution_owning_history() {
        let mut driver = SqliteDriver::open_in_memory().unwrap();
        let mut tx = driver.transaction().unwrap();
        tx.execute("CREATE TABLE items(id);").unwrap();
        tx.ledger().unwrap().commit(8).unwrap();
        tx.commit().unwrap();

        let mut tx = driver.transaction().unwrap();
        tx.ledger().unwrap().uncommit(8).unwrap();
        assert!(
            tx.execute("DROP TABLE items; SELECT * FROM missing;")
                .is_err()
        );
        assert!(tx.commit().is_err());
        assert_eq!(driver.ledger().list().unwrap(), [8]);
        assert_eq!(driver.schema().unwrap().objects()[0].name(), "items");
    }

    #[test]
    fn rejects_batch_commands_that_can_break_history_or_atomicity() {
        let mut driver = SqliteDriver::open_in_memory().unwrap();
        assert!(
            driver
                .execute("CREATE TABLE t(id); ALTER TABLE t RENAME TO __migrations;")
                .is_err()
        );
        driver.ledger().commit(1).unwrap();
        for sql in [
            "COMMIT",
            "CREATE TEMP TABLE temp_items(id)",
            "DELETE FROM main.__migrations",
            "PRAGMA foreign_keys = OFF",
            "INSERT INTO items VALUES (?)",
        ] {
            assert!(
                driver
                    .execute(&format!("CREATE TABLE items(id); {sql}"))
                    .is_err(),
                "{sql}"
            );
            assert!(driver.schema().unwrap().objects().is_empty());
        }
        assert_eq!(driver.ledger().list().unwrap(), [1]);
    }
}
