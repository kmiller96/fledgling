//! SQLite storage for applied migration IDs.
//!
use rusqlite::{Connection, OptionalExtension};

use super::SqliteError;
use crate::migrations::Ledger;

// ----------------------------------------------------------------------------
// Storage format
// ----------------------------------------------------------------------------

pub(super) const LEDGER_NAME: &str = "__migrations";

// Separate position from ID: order is application order, and text IDs retain
// the entire u64 range, which does not fit SQLite's signed INTEGER type.
const DEFINITION: &str = "(position INTEGER PRIMARY KEY, id TEXT NOT NULL UNIQUE)";

// ----------------------------------------------------------------------------
// Ledger
// ----------------------------------------------------------------------------

/// Migration history stored in SQLite's main schema. Can borrow a transaction
/// so that the history update commits together with the migration's SQL.
///
/// Reading an uninitialized ledger returns an empty history without creating
/// the table. The first commit initializes it; incompatible formats are rejected.
pub struct SqliteLedger<'a> {
    connection: &'a Connection,
}

impl<'a> SqliteLedger<'a> {
    pub(super) fn new(connection: &'a Connection) -> Self {
        Self { connection }
    }

    fn exists(&self) -> Result<bool, SqliteError> {
        let sql: Option<Option<String>> = self.connection.query_row(
            "SELECT sql FROM main.sqlite_schema WHERE name = ?1 COLLATE NOCASE AND type <> 'trigger'",
            [LEDGER_NAME],
            |row| row.get(0),
        ).optional()?;
        match sql {
            None => Ok(false),
            Some(Some(sql)) if sql == format!("CREATE TABLE {LEDGER_NAME} {DEFINITION}") => {
                Ok(true)
            }
            _ => Err(SqliteError::Invalid(format!(
                "incompatible {LEDGER_NAME} ledger format; explicit migration required"
            ))),
        }
    }
}

impl Ledger for SqliteLedger<'_> {
    type Error = SqliteError;

    fn list(&self) -> Result<Vec<u64>, Self::Error> {
        if !self.exists()? {
            return Ok(Vec::new());
        }
        let mut statement = self.connection.prepare(&format!(
            "SELECT id FROM main.{LEDGER_NAME} ORDER BY position"
        ))?;
        statement
            .query_map([], |row| row.get::<_, String>(0))?
            .map(|value| {
                let value = value?;
                value
                    .parse::<u64>()
                    .ok()
                    .filter(|id| id.to_string() == value)
                    .ok_or_else(|| SqliteError::Invalid("invalid ID in migration ledger".into()))
            })
            .collect()
    }

    fn commit(&mut self, id: u64) -> Result<(), Self::Error> {
        if !self.exists()? {
            self.connection.execute(
                &format!("CREATE TABLE IF NOT EXISTS main.{LEDGER_NAME} {DEFINITION}"),
                [],
            )?;
            self.exists()?;
        }
        self.connection.execute(
            &format!("INSERT INTO main.{LEDGER_NAME} (id) VALUES (?1)"),
            [id.to_string()],
        )?;
        Ok(())
    }

    fn uncommit(&mut self, id: u64) -> Result<(), Self::Error> {
        if self.exists()?
            && self.connection.execute(
                &format!(
                    "DELETE FROM main.{LEDGER_NAME}
                     WHERE position = (SELECT max(position) FROM main.{LEDGER_NAME}) AND id = ?1"
                ),
                [id.to_string()],
            )? == 1
        {
            Ok(())
        } else {
            Err(SqliteError::Invalid(format!(
                "migration {id} is not the latest applied"
            )))
        }
    }
}

// ----------------------------------------------------------------------------
// Tests
// ----------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stores_full_ids_in_application_order_and_only_removes_the_latest() {
        let database = Connection::open_in_memory().unwrap();
        let mut ledger = SqliteLedger::new(&database);
        assert!(ledger.list().unwrap().is_empty());
        assert!(ledger.uncommit(1).is_err());
        assert!(!ledger.exists().unwrap());
        ledger.commit(u64::MAX).unwrap();
        ledger.commit(1).unwrap();
        assert!(ledger.commit(u64::MAX).is_err());
        assert!(ledger.uncommit(u64::MAX).is_err());
        assert_eq!(ledger.list().unwrap(), [u64::MAX, 1]);
        ledger.uncommit(1).unwrap();
        ledger.uncommit(u64::MAX).unwrap();
        assert!(ledger.list().unwrap().is_empty());
    }

    #[test]
    fn refuses_to_overwrite_a_legacy_ledger() {
        let database = Connection::open_in_memory().unwrap();
        database
            .execute("CREATE TABLE __migrations (id INTEGER PRIMARY KEY)", [])
            .unwrap();
        let mut ledger = SqliteLedger::new(&database);
        assert!(
            ledger
                .list()
                .unwrap_err()
                .to_string()
                .contains("incompatible")
        );
        assert!(ledger.commit(1).is_err());
    }
}
