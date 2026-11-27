//! Connection and transaction-scoped PostgreSQL command execution.

use native_tls::TlsConnector;
use pg_query::{NodeEnum, NodeRef};
use postgres::{Client, IsolationLevel, Transaction};
use postgres_native_tls::MakeTlsConnector;

use super::{
    PostgresError,
    ledger::{LEDGER_NAME, PostgresLedger},
    pull,
};
use crate::driver::Driver;

/// A PostgreSQL connection.
pub struct PostgresDriver(Client);

impl PostgresDriver {
    /// Connect to an existing database using libpq-style URL options.
    pub fn connect(url: &str) -> Result<Self, PostgresError> {
        let tls = TlsConnector::new()
            .map_err(|error| PostgresError::Invalid(format!("PostgreSQL TLS setup: {error}")))?;
        Ok(Self(Client::connect(url, MakeTlsConnector::new(tls))?))
    }

    /// Inspect the schema in one read-only, consistent snapshot.
    pub fn schema_sql(&mut self) -> Result<String, PostgresError> {
        let mut tx = self
            .0
            .build_transaction()
            .read_only(true)
            .isolation_level(IsolationLevel::RepeatableRead)
            .start()?;
        tx.batch_execute("SET LOCAL search_path = pg_catalog")?;
        let sql = pull::schema_sql(&mut tx)?;
        tx.commit()?;
        Ok(sql)
    }

    /// Borrow the connection for a read-only ledger snapshot.
    pub(crate) fn read_ledger(&mut self) -> Result<PostgresTransaction<'_>, PostgresError> {
        Ok(PostgresTransaction(
            self.0
                .build_transaction()
                .read_only(true)
                .isolation_level(IsolationLevel::RepeatableRead)
                .start()?,
        ))
    }

    /// Start a write transaction for commands and their ledger update.
    pub fn transaction(&mut self) -> Result<PostgresTransaction<'_>, PostgresError> {
        let mut tx = self.0.transaction()?;
        tx.batch_execute(
            "SET LOCAL search_path = public; SET LOCAL standard_conforming_strings = on",
        )?;
        Ok(PostgresTransaction(tx))
    }
}

impl Driver for PostgresDriver {
    type Error = PostgresError;

    fn execute(&mut self, commands: &str) -> Result<(), Self::Error> {
        let mut tx = self.transaction()?;
        tx.execute(commands)?;
        tx.commit()
    }
}

/// A migration transaction; dropping it without committing rolls it back.
pub struct PostgresTransaction<'a>(Transaction<'a>);

impl<'a> PostgresTransaction<'a> {
    /// Borrow the transaction for an independent ledger operation.
    pub fn ledger(&mut self) -> PostgresLedger<'_, Transaction<'a>> {
        PostgresLedger::new(&mut self.0)
    }

    /// Commit the commands and any ledger updates together.
    pub fn commit(self) -> Result<(), PostgresError> {
        Ok(self.0.commit()?)
    }
}

impl Driver for PostgresTransaction<'_> {
    type Error = PostgresError;

    fn execute(&mut self, commands: &str) -> Result<(), Self::Error> {
        validate_batch(commands)?;
        Ok(self.0.batch_execute(commands)?)
    }
}

fn validate_batch(sql: &str) -> Result<(), PostgresError> {
    let parsed = pg_query::parse(sql)
        .map_err(|e| PostgresError::Invalid(format!("invalid migration SQL: {e}")))?;
    for (node, ..) in parsed.protobuf.nodes() {
        if matches!(node, NodeRef::TransactionStmt(_)) {
            return Err(PostgresError::Invalid(
                "explicit transaction control is not allowed in migrations".into(),
            ));
        }
        let reserved = match node {
            NodeRef::RangeVar(r) => r.relname == LEDGER_NAME,
            NodeRef::RenameStmt(r) => r.newname == LEDGER_NAME || r.subname == LEDGER_NAME,
            NodeRef::DropStmt(s) => s.objects.iter().any(|target| {
                match &target.node {
                Some(NodeEnum::String(name)) => name.sval == LEDGER_NAME,
                Some(NodeEnum::List(list)) => list.items.iter().any(|part| {
                    matches!(&part.node, Some(NodeEnum::String(name)) if name.sval == LEDGER_NAME)
                }),
                _ => false,
            }
            }),
            _ => false,
        };
        if reserved {
            return Err(PostgresError::Invalid(format!(
                "migration may not access the reserved PostgreSQL ledger {LEDGER_NAME}"
            )));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::validate_batch;

    #[test]
    fn rejects_transaction_control_and_direct_ledger_access() {
        for sql in [
            "CREATE TABLE t (id int); COMMIT; BEGIN",
            "PREPARE TRANSACTION 'prepared'",
            "DROP TABLE public._fledgling_migrations",
            "WITH d AS (DELETE FROM public._fledgling_migrations RETURNING *) SELECT * FROM d",
            "ALTER TABLE t RENAME TO _fledgling_migrations",
        ] {
            assert!(validate_batch(sql).is_err(), "{sql}");
        }
        validate_batch("SELECT 'COMMIT; DROP TABLE _fledgling_migrations'").unwrap();
    }
}
