//! SQLite schema inspection, planning, and versioned migration execution.
//!
//! [`pull`] exports stored `main` definitions; [`plan`] instead builds and
//! checks an additive migration on a disposable database. [`migrate`] and
//! [`rollback`] run against a caller-owned connection and update the ledger in
//! the same transaction as the migration SQL.

mod ddl;
pub(super) mod runtime {
    //! SQLite ledger storage, migration preflight, and transaction outcomes.

    use rusqlite::{Connection, Transaction, TransactionBehavior, types::ValueRef};
    use sqlite3_parser::{
        Bump, FallibleIterator,
        ast::{Cmd, Stmt},
        lexer::sql::Parser,
    };

    use super::{SqliteAdaptor, SqliteExecutor};
    use crate::{
        FailureOutcome,
        migrations::{
            AppliedMigration, CatalogError, Migration, MigrationCatalog, MigrationExecutor,
            MigrationRunError, MigrationSql, Planner,
        },
    };
    use std::error::Error;

    impl From<CatalogError> for rusqlite::Error {
        fn from(error: CatalogError) -> Self {
            Self::InvalidParameterName(error.to_string())
        }
    }

    // ----------------------------------------------------------------------------
    // Ledger storage
    // ----------------------------------------------------------------------------

    /// Recheck the rollback target before executing Down SQL in the same transaction.
    fn check_rollback(connection: &Connection, migration: &Migration) -> rusqlite::Result<()> {
        let latest_id: Option<i64> = connection.query_row(
            "SELECT max(id) FROM main._fledgling_migrations",
            [],
            |row| row.get(0),
        )?;
        if latest_id.map(decode_id).transpose()? != Some(migration.id) {
            return Err(invalid(
                "only the latest applied migration can be rolled back",
            ));
        }
        let digest = connection.query_row(
            "SELECT digest FROM main._fledgling_migrations WHERE id = ?1",
            [encode_id(migration.id)?],
            |row| decode_digest(row.get_ref(0)?),
        )?;
        if digest
            != crate::migrations::digest(
                migration.up.sql.as_bytes(),
                migration
                    .down
                    .as_ref()
                    .map(|artifact| artifact.sql.as_bytes()),
            )
        {
            return Err(invalid("migration contents differ from applied history"));
        }
        Ok(())
    }

    impl Planner for SqliteAdaptor {
        fn plan(
            &self,
            catalog: &MigrationCatalog,
            desired_schema_sql: &str,
        ) -> Result<Option<MigrationSql>, Box<dyn Error>> {
            let connection = Connection::open_in_memory()?;
            apply(&connection, catalog)?;
            super::schema::plan(&connection, desired_schema_sql)
        }
    }

    impl SqliteExecutor<'_> {
        /// Read table existence and rows in one snapshot. The connection
        /// must not already be in a transaction.
        fn load(&self) -> rusqlite::Result<Option<Vec<AppliedMigration>>> {
            let transaction = self.connection.unchecked_transaction()?;
            let ledger = load_ledger(&transaction)?;
            transaction.commit()?;
            Ok(ledger)
        }
    }

    impl MigrationExecutor for SqliteExecutor<'_> {
        type Error = rusqlite::Error;

        fn status(&self) -> rusqlite::Result<Option<Vec<u64>>> {
            Ok(self
                .load()?
                .map(|records| records.into_iter().map(|record| record.id).collect()))
        }

        fn apply(&self, ledger: &MigrationCatalog) -> Result<Vec<u64>, VersionedRunError> {
            apply(self.connection, ledger)
        }

        fn revert(&self, ledger: &MigrationCatalog) -> Result<(), VersionedRunError> {
            revert(self.connection, ledger)
        }
    }

    fn load_ledger(connection: &Connection) -> rusqlite::Result<Option<Vec<AppliedMigration>>> {
        let exists = connection.query_row(
            "SELECT EXISTS(SELECT 1 FROM main.sqlite_schema \
             WHERE type = 'table' AND name = '_fledgling_migrations')",
            [],
            |row| row.get::<_, bool>(0),
        )?;
        if !exists {
            return Ok(None);
        }

        let mut statement =
            connection.prepare("SELECT id, digest FROM main._fledgling_migrations ORDER BY id")?;
        let migrations = statement
            .query_map([], |row| {
                let id = decode_id(row.get("id")?)?;
                let digest = decode_digest(row.get_ref("digest")?)?;
                Ok(AppliedMigration::new(id, digest))
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(Some(migrations))
    }

    fn initialize_ledger(connection: &Connection) -> rusqlite::Result<()> {
        let transaction = Transaction::new_unchecked(connection, TransactionBehavior::Immediate)?;
        if load_ledger(&transaction)?.is_some() {
            return Err(invalid("migration ledger changed since it was loaded"));
        }
        transaction.execute_batch(
            "CREATE TABLE main._fledgling_migrations (\
                id INTEGER PRIMARY KEY CHECK (id > 0),\
                digest BLOB NOT NULL CHECK (typeof(digest) = 'blob' AND length(digest) = 32),\
                applied_at TEXT\
            )",
        )?;
        transaction.commit()
    }

    fn insert_applied_record(
        transaction: &Transaction<'_>,
        migration: &AppliedMigration,
    ) -> rusqlite::Result<()> {
        let inserted = transaction.execute(
            "INSERT INTO main._fledgling_migrations (id, digest, applied_at) \
             VALUES (?1, ?2, strftime('%Y-%m-%dT%H:%M:%fZ', 'now'))",
            rusqlite::params![encode_id(migration.id)?, migration.digest.as_slice()],
        )?;
        if inserted != 1 {
            return Err(invalid("failed to record applied migration"));
        }
        Ok(())
    }

    fn delete_record(transaction: &Transaction<'_>, id: u64) -> rusqlite::Result<()> {
        let deleted = transaction.execute(
            "DELETE FROM main._fledgling_migrations WHERE id = ?1",
            [encode_id(id)?],
        )?;
        if deleted != 1 {
            return Err(invalid("failed to remove applied migration"));
        }
        Ok(())
    }

    /// Convert a catalog ID to SQLite's signed integer representation.
    fn encode_id(id: u64) -> rusqlite::Result<i64> {
        i64::try_from(id).map_err(|error| rusqlite::Error::ToSqlConversionFailure(Box::new(error)))
    }

    fn decode_id(id: i64) -> rusqlite::Result<u64> {
        u64::try_from(id).map_err(|_| {
            rusqlite::Error::InvalidParameterName(
                "migration ledger contains a non-positive ID".into(),
            )
        })
    }

    /// Reject corrupt representations rather than coercing text or other types.
    fn decode_digest(value: ValueRef<'_>) -> rusqlite::Result<[u8; 32]> {
        match value {
            ValueRef::Blob(bytes) if bytes.len() == 32 => {
                let mut digest = [0; 32];
                digest.copy_from_slice(bytes);
                Ok(digest)
            }
            _ => Err(invalid_stored_digest()),
        }
    }

    fn invalid_stored_digest() -> rusqlite::Error {
        rusqlite::Error::InvalidParameterName(
            "migration ledger contains a malformed digest (expected 32-byte BLOB)".into(),
        )
    }

    // ----------------------------------------------------------------------------
    // Versioned migration and rollback
    // ----------------------------------------------------------------------------

    /// Failure from a versioned SQLite migration run.
    pub type VersionedRunError = MigrationRunError<rusqlite::Error>;

    /// Apply pending migrations and return the IDs committed by this call.
    ///
    /// Check the applied catalog prefix by ID and digest. Preflight all pending
    /// Up and present Down files before bootstrapping a missing ledger or
    /// applying any migration. Each Up and ledger insert commits separately;
    /// a later failure does not undo earlier commits or an initialized ledger.
    ///
    /// Use an idle connection without TEMP objects or attached databases.
    /// Serialize apply and rollback runs for this database. Supply trusted SQL:
    /// preflight checks runner compatibility, not the semantics of the batch.
    fn apply(
        connection: &Connection,
        catalog: &MigrationCatalog,
    ) -> Result<Vec<u64>, VersionedRunError> {
        ensure_dedicated_connection(connection).map_err(not_started)?;
        let adaptor = SqliteExecutor::new(connection);
        let history = adaptor.load().map_err(not_started)?;
        let pending = catalog
            .pending_from_history(history.as_deref().unwrap_or_default())
            .map_err(|error| not_started(error.into()))?;
        for migration in pending {
            preflight(migration).map_err(|source| VersionedRunError {
                failed_migration_id: Some(migration.id),
                ..not_started(source)
            })?;
        }
        if history.is_none() {
            initialize_ledger(connection).map_err(not_started)?;
        }
        let mut committed = Vec::new();
        for migration in pending {
            if let Err(mut failure) = run_transaction(connection, |transaction| {
                let up = &migration.up;
                transaction.execute_batch(&up.sql)?;
                ensure_dedicated_connection(transaction)?;
                insert_applied_record(
                    transaction,
                    &AppliedMigration::new(
                        migration.id,
                        crate::migrations::digest(
                            migration.up.sql.as_bytes(),
                            migration
                                .down
                                .as_ref()
                                .map(|artifact| artifact.sql.as_bytes()),
                        ),
                    ),
                )
            }) {
                failure.failed_migration_id = Some(migration.id);
                failure.committed_migration_ids = committed;
                return Err(failure);
            }
            committed.push(migration.id);
        }
        Ok(committed)
    }

    /// Run the latest applied migration's Down SQL and remove its ledger entry.
    ///
    /// Require a valid applied catalog prefix and a present Down artifact.
    /// Recheck the target's ID and digest inside the same immediate transaction
    /// that executes Down and deletes the ledger row; never bootstrap here.
    ///
    /// Use an idle connection without TEMP objects or attached databases.
    /// Serialize against other apply and rollback runs for this database, and
    /// only execute trusted migration artifacts.
    fn revert(
        connection: &Connection,
        catalog: &MigrationCatalog,
    ) -> Result<(), VersionedRunError> {
        let history = SqliteExecutor::new(connection).load();
        // Preserve the recorded ID on failures, including connection checks.
        let latest_id = history
            .as_ref()
            .ok()
            .and_then(|history| history.as_ref()?.last().map(|migration| migration.id));
        let before_execution = |source| VersionedRunError {
            failed_migration_id: latest_id,
            ..not_started(source)
        };
        ensure_dedicated_connection(connection).map_err(before_execution)?;
        let history = history
            .map_err(before_execution)?
            .ok_or_else(|| before_execution(invalid("migration ledger is not initialized")))?;
        let migration = catalog
            .latest_applied(&history)
            .map_err(|error| before_execution(error.into()))?
            .ok_or_else(|| before_execution(invalid("no migration is applied")))?;
        let down_sql = migration
            .down
            .as_ref()
            .map(|artifact| artifact.sql.as_str())
            .ok_or_else(|| before_execution(invalid("migration has no Down SQL")))?;
        validate_sql(migration.id, "Down", down_sql).map_err(before_execution)?;
        run_transaction(connection, |transaction| {
            check_rollback(transaction, migration)?;
            transaction.execute_batch(down_sql)?;
            ensure_dedicated_connection(transaction)?;
            delete_record(transaction, migration.id)
        })
        .map_err(|mut failure| {
            failure.failed_migration_id = latest_id;
            failure
        })
    }

    // ----------------------------------------------------------------------------
    // Preflight and connection isolation
    // ----------------------------------------------------------------------------

    /// Check SQLite's ID range and both SQL directions before applying Up.
    fn preflight(migration: &Migration) -> rusqlite::Result<()> {
        let id = migration.id;
        i64::try_from(id).map_err(|_| invalid("migration ID exceeds SQLite integer range"))?;
        let up = migration.up.sql.as_str();
        let down = migration
            .down
            .as_ref()
            .map(|artifact| artifact.sql.as_str());
        if up.trim().is_empty() {
            return Err(invalid(format!("Up migration {id} must not be empty")));
        }
        if down.is_some_and(|sql| sql.trim().is_empty()) {
            return Err(invalid(format!("Down migration {id} must not be empty")));
        }
        validate_sql(id, "Up", up)?;
        if let Some(sql) = down {
            validate_sql(id, "Down", sql)?;
        }
        Ok(())
    }

    fn validate_sql(id: u64, direction: &str, sql: &str) -> rusqlite::Result<()> {
        validate_migration_sql(sql)
            .map_err(|error| invalid(format!("{direction} migration {id} is invalid: {error}")))
    }

    fn invalid(message: impl Into<String>) -> rusqlite::Error {
        rusqlite::Error::InvalidParameterName(message.into())
    }

    fn not_started(source: rusqlite::Error) -> VersionedRunError {
        MigrationRunError {
            source,
            failed_migration_id: None,
            committed_migration_ids: Vec::new(),
            outcome: FailureOutcome::NotStarted,
            cleanup_error: None,
        }
    }

    /// Parse a non-empty batch and reject transaction control, ATTACH, DETACH,
    /// and VACUUM as statements. Does not execute SQL or guarantee it will run.
    pub(super) fn validate_migration_sql(sql: &str) -> Result<(), String> {
        let arena = Bump::new();
        let commands: Vec<_> = Parser::new(&arena, sql.as_bytes())
            .collect()
            .map_err(|error| format!("{error:?}"))?;
        if commands.is_empty() {
            return Err("migration contains no SQL statements".to_owned());
        }
        if commands.iter().any(|command| {
            matches!(
                command,
                Cmd::Stmt(
                    Stmt::Begin(..)
                        | Stmt::Commit(..)
                        | Stmt::Rollback { .. }
                        | Stmt::Savepoint(..)
                        | Stmt::Release(..)
                        | Stmt::Attach { .. }
                        | Stmt::Detach(..)
                        | Stmt::Vacuum(..)
                )
            )
        }) {
            return Err(
                "migration contains an operation incompatible with runner transactions".to_owned(),
            );
        }
        Ok(())
    }

    /// Check before execution and again before the ledger update that TEMP
    /// objects cannot shadow `main` and no other databases are attached.
    fn ensure_dedicated_connection(connection: &Connection) -> rusqlite::Result<()> {
        // Unqualified SQLite DDL can resolve TEMP objects before main objects.
        let temp_objects: i64 =
            connection.query_row("SELECT count(*) FROM sqlite_temp_master", [], |row| {
                row.get(0)
            })?;
        if temp_objects != 0 {
            return Err(rusqlite::Error::InvalidParameterName(
                "versioned migrations require a connection without TEMP objects".into(),
            ));
        }
        let mut statement = connection.prepare("PRAGMA database_list")?;
        let mut rows = statement.query([])?;
        while let Some(row) = rows.next()? {
            let name: String = row.get(1)?;
            if name != "main" && name != "temp" {
                return Err(rusqlite::Error::InvalidParameterName(
                    "versioned migrations require a connection without attached databases".into(),
                ));
            }
        }
        Ok(())
    }

    // ----------------------------------------------------------------------------
    // Transaction lifecycle and failure classification
    // ----------------------------------------------------------------------------

    /// Acquire an immediate write transaction for one migration. The callback
    /// must leave transaction control to the runner. Preserve the primary
    /// error and distinguish confirmed rollback from an uncertain outcome.
    fn run_transaction(
        connection: &Connection,
        execute: impl FnOnce(&Transaction<'_>) -> rusqlite::Result<()>,
    ) -> Result<(), VersionedRunError> {
        let transaction = Transaction::new_unchecked(connection, TransactionBehavior::Immediate)
            .map_err(not_started)?;
        execute(&transaction).map_err(|source| rollback_transaction(&transaction, source))?;
        if let Err(source) = transaction.execute_batch("COMMIT") {
            // If COMMIT ended the transaction despite the error, we cannot confirm rollback.
            if transaction.is_autocommit() {
                return Err(VersionedRunError {
                    outcome: FailureOutcome::Uncertain,
                    ..not_started(source)
                });
            }
            return Err(rollback_transaction(&transaction, source));
        }
        Ok(())
    }

    /// Roll back a still-active transaction and retain any separate cleanup
    /// error. On the execution-error path, autocommit can mean SQLite already
    /// rolled back; commit errors in autocommit are handled separately.
    fn rollback_transaction(
        transaction: &Transaction<'_>,
        source: rusqlite::Error,
    ) -> VersionedRunError {
        let cleanup_error = if transaction.is_autocommit() {
            None
        } else {
            transaction.execute_batch("ROLLBACK").err().map(Box::new)
        };
        let outcome = if cleanup_error.is_some() {
            FailureOutcome::Uncertain
        } else {
            FailureOutcome::KnownRollback
        };
        VersionedRunError {
            outcome,
            cleanup_error,
            ..not_started(source)
        }
    }

    #[cfg(test)]
    mod ledger_tests {
        use super::*;
        #[test]
        fn initializing_rejects_an_existing_ledger() {
            let connection = Connection::open_in_memory().unwrap();
            let adaptor = SqliteExecutor::new(&connection);
            assert_eq!(adaptor.load().unwrap(), None);
            initialize_ledger(&connection).unwrap();
            let error = initialize_ledger(&connection).unwrap_err();
            assert!(error.to_string().contains("migration ledger changed"));
            assert_eq!(adaptor.load().unwrap(), Some(vec![]));
        }

        #[test]
        fn adaptor_reads_current_history_after_apply_and_rollback() {
            let connection = Connection::open_in_memory().unwrap();
            let adaptor = SqliteExecutor::new(&connection);
            let migration = Migration {
                id: 1,
                up: crate::migrations::Artifact {
                    path: "000001.up.sql".into(),
                    sql: "CREATE TABLE example (id INTEGER);".into(),
                },
                down: Some(crate::migrations::Artifact {
                    path: "000001.down.sql".into(),
                    sql: "DROP TABLE example;".into(),
                }),
            };

            assert_eq!(adaptor.status().unwrap(), None);

            let catalog = MigrationCatalog::from_migrations(vec![migration]).unwrap();
            adaptor.apply(&catalog).unwrap();
            assert_eq!(adaptor.status().unwrap(), Some(vec![1]));
            let has_utc_timestamp: bool = connection
                .query_row(
                    "SELECT applied_at LIKE '%Z' AND julianday(applied_at) IS NOT NULL \
                 FROM main._fledgling_migrations WHERE id = 1",
                    [],
                    |row| row.get(0),
                )
                .unwrap();
            assert!(has_utc_timestamp);
            let applied = adaptor.load().unwrap().unwrap();
            assert_eq!(
                applied[0].digest,
                crate::migrations::digest(
                    catalog[0].up.sql.as_bytes(),
                    catalog[0]
                        .down
                        .as_ref()
                        .map(|artifact| artifact.sql.as_bytes())
                )
            );
            assert_eq!(applied[0].id, catalog[0].id);

            adaptor.revert(&catalog).unwrap();
            assert_eq!(adaptor.status().unwrap(), Some(vec![]));
        }
    }
    #[cfg(test)]
    mod runner_tests {
        use super::validate_migration_sql;

        #[test]
        fn rejects_vacuum_in_transactional_migrations() {
            assert_eq!(
                validate_migration_sql("VACUUM"),
                Err(
                    "migration contains an operation incompatible with runner transactions"
                        .to_owned()
                )
            );
            assert!(validate_migration_sql("CREATE TABLE items (id INTEGER)").is_ok());
        }

        #[test]
        fn rejects_attach_and_detach_in_single_database_migrations() {
            for sql in [
                "ATTACH DATABASE 'other.db' AS other",
                "DETACH DATABASE other",
            ] {
                assert_eq!(
                    validate_migration_sql(sql),
                    Err(
                        "migration contains an operation incompatible with runner transactions"
                            .to_owned()
                    )
                );
            }
            assert!(validate_migration_sql("CREATE TABLE items (id INTEGER)").is_ok());
        }
    }
}

mod schema;

use crate::migrations::{MigrationCatalog, MigrationExecutor, MigrationSql, Planner};
use rusqlite::Connection;
use std::error::Error;

// ----------------------------------------------------------------------------
// Public inspection and planning facade
// ----------------------------------------------------------------------------

pub use runtime::VersionedRunError;
pub use schema::PreviewError;
/// Open a SQLite connection with read-only flags; a missing database file
/// is an error, not a request to create it.
pub fn open_read_only(path: impl AsRef<std::path::Path>) -> rusqlite::Result<rusqlite::Connection> {
    schema::open_read_only(path)
}
pub use schema::pull;

/// SQLite offline schema planner. Does not hold a project database connection.
#[derive(Clone, Copy, Debug, Default)]
pub struct SqliteAdaptor;

/// Connection-bound SQLite ledger reader and migration executor.
pub struct SqliteExecutor<'connection> {
    connection: &'connection Connection,
}

impl<'connection> SqliteExecutor<'connection> {
    /// Bind migration execution and ledger operations to an idle connection.
    pub fn new(connection: &'connection Connection) -> Self {
        Self { connection }
    }
}

/// Replay the catalog's Up migrations in memory and render the next Up and Down
/// SQL without opening the project database or writing either file.
///
/// Replay executes trusted Up SQL using the versioned runner, including
/// preflight of present Down files. Verify generated Up against the desired
/// schema and Down against the baseline. Returns `Ok(None)` when the modeled
/// schemas match; unsupported changes are errors.
pub fn plan(
    catalog: &MigrationCatalog,
    desired_schema_sql: &str,
) -> Result<Option<MigrationSql>, Box<dyn Error>> {
    SqliteAdaptor.plan(catalog, desired_schema_sql)
}

/// Read applied migration IDs in ascending order without initializing the ledger.
///
/// Returns `None` if the ledger does not exist or `Some(vec![])` if it is empty.
/// Use a connection with no active transaction. This does not compare the
/// ledger with a catalog.
pub fn status(connection: &Connection) -> rusqlite::Result<Option<Vec<u64>>> {
    SqliteExecutor::new(connection).status()
}

/// Apply pending migrations using a dedicated, idle connection.
pub fn migrate(
    connection: &Connection,
    ledger: &MigrationCatalog,
) -> Result<Vec<u64>, VersionedRunError> {
    SqliteExecutor::new(connection).apply(ledger)
}

/// Revert the latest applied migration using a dedicated, idle connection.
pub fn rollback(
    connection: &Connection,
    ledger: &MigrationCatalog,
) -> Result<(), VersionedRunError> {
    SqliteExecutor::new(connection).revert(ledger)
}
