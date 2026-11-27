//! SQLite storage for applied migration records.

use super::driver::SqliteDriver;
use crate::migrations::{AppliedMigration, Ledger};
use rusqlite::{Connection, OptionalExtension, TransactionBehavior};
use std::{error::Error, fmt};

pub(super) const LEDGER_NAME: &str = "_fledgling_migrations";

// SQLite omits the main qualifier in sqlite_schema.sql. An explicit integer
// primary key preserves application order even when the database is vacuumed.
const CREATE: &str =
    "CREATE TABLE _fledgling_migrations (position INTEGER PRIMARY KEY, id TEXT NOT NULL UNIQUE)";

/// An SQLite operation or migration contract failed.
#[derive(Debug)]
pub enum SqliteError {
    Sqlite(rusqlite::Error),
    Invalid(String),
}

impl fmt::Display for SqliteError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Sqlite(error) => error.fmt(f),
            Self::Invalid(message) => f.write_str(message),
        }
    }
}

impl Error for SqliteError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Sqlite(error) => Some(error),
            Self::Invalid(_) => None,
        }
    }
}

impl From<rusqlite::Error> for SqliteError {
    fn from(error: rusqlite::Error) -> Self {
        Self::Sqlite(error)
    }
}

fn exists(conn: &Connection) -> Result<bool, SqliteError> {
    let object = conn
        .query_row(
            "SELECT type, sql FROM main.sqlite_schema
             WHERE name = ?1 COLLATE NOCASE AND type <> 'trigger'",
            [LEDGER_NAME],
            |row| Ok((row.get::<_, String>(0)?, row.get::<_, Option<String>>(1)?)),
        )
        .optional()?;
    match object {
        None => Ok(false),
        Some((kind, Some(sql))) if kind == "table" && sql == CREATE => Ok(true),
        _ => Err(SqliteError::Invalid(
            "incompatible _fledgling_migrations ledger format; explicit migration required".into(),
        )),
    }
}

pub(super) fn ensure(conn: &Connection) -> Result<(), SqliteError> {
    if !exists(conn)? {
        conn.execute(CREATE, [])?;
    }
    Ok(())
}

pub(super) fn check_unapplied(conn: &Connection, id: u64) -> Result<(), SqliteError> {
    let applied: bool = conn.query_row(
        "SELECT EXISTS(SELECT 1 FROM main._fledgling_migrations WHERE id = ?1)",
        [id.to_string()],
        |row| row.get(0),
    )?;
    if applied {
        return Err(SqliteError::Invalid("migration already applied".into()));
    }
    Ok(())
}

pub(super) fn check_latest(conn: &Connection, id: u64) -> Result<(), SqliteError> {
    let latest = if exists(conn)? {
        conn.query_row(
            "SELECT id FROM main._fledgling_migrations ORDER BY position DESC LIMIT 1",
            [],
            |row| row.get::<_, String>(0),
        )
        .optional()?
    } else {
        None
    };
    if latest.as_deref() != Some(id.to_string().as_str()) {
        return Err(SqliteError::Invalid(
            "migration is not the latest applied".into(),
        ));
    }
    Ok(())
}

pub(super) fn append(conn: &Connection, id: u64) -> Result<(), SqliteError> {
    conn.execute(
        "INSERT INTO main._fledgling_migrations (id) VALUES (?1)",
        [id.to_string()],
    )?;
    Ok(())
}

pub(super) fn remove(conn: &Connection, id: u64) -> Result<(), SqliteError> {
    conn.execute(
        "DELETE FROM main._fledgling_migrations WHERE id = ?1",
        [id.to_string()],
    )?;
    Ok(())
}

impl Ledger for SqliteDriver {
    type Error = SqliteError;

    fn list(&self) -> Result<Vec<AppliedMigration>, Self::Error> {
        // An uninitialized ledger is empty; do not create it on this path.
        if !exists(&self.connection)? {
            return Ok(vec![]);
        }
        let mut stmt = self
            .connection
            .prepare("SELECT id FROM main._fledgling_migrations ORDER BY position")?;
        let ids = stmt.query_map([], |row| row.get::<_, String>(0))?;
        ids.map(|value| {
            let value = value?;
            let id = value
                .parse::<u64>()
                .ok()
                .filter(|id| id.to_string() == value)
                .ok_or_else(|| SqliteError::Invalid("invalid ID in migration ledger".into()))?;
            Ok(AppliedMigration { id })
        })
        .collect()
    }

    fn commit(&mut self, migration: AppliedMigration) -> Result<(), Self::Error> {
        let tx = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        ensure(&tx)?;
        check_unapplied(&tx, migration.id)?;
        append(&tx, migration.id)?;
        tx.commit()?;
        Ok(())
    }

    fn revert(&mut self, id: u64) -> Result<(), Self::Error> {
        let tx = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        check_latest(&tx, id)?;
        remove(&tx, id)?;
        tx.commit()?;
        Ok(())
    }
}
