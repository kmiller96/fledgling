//! Database schema inspection and SQL migration workflows.
//!
//! A project keeps its desired schema in SQL and its history in versioned
//! Up and Down files. [`preview`] and [`generate`] work offline: they compare
//! the desired schema with the schema obtained by replaying those files, not
//! with a live database. [`pull`] inspects a live database instead.
//!
//! [`migrate`] and [`rollback`] commit each migration and its ledger update
//! together. Callers must still serialize those workflows per database.

mod backends;
mod driver;
mod migrations;
mod planner;
mod schema;

use backends::sqlite::driver::SqliteDriver;
use migrations::{Ledger, Migration, MigrationCatalog};
use serde::{Deserialize, Serialize};
use std::{
    error::Error,
    fmt,
    path::{Path, PathBuf},
};

// ----------------------------------------------------------------------------
// Dialect
// ----------------------------------------------------------------------------

/// Database dialect selected by the caller or project configuration.
#[derive(Clone, Copy, Debug, Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Dialect {
    /// SQLite, the only supported backend (`sqlite` in TOML).
    SQLite,
}

impl fmt::Display for Dialect {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::SQLite => f.write_str("SQLite"),
        }
    }
}

// ----------------------------------------------------------------------------
// Pull
// ----------------------------------------------------------------------------

/// Pulls the current schema definition from a live database as SQL.
///
/// Opens an existing database read-only and returns the user schema, excluding
/// the migration ledger and SQLite-owned objects. This is not a data dump.
/// Unlike [`preview`] and [`generate`], it does not read migration history.
pub fn pull(dialect: Dialect, database: &Path) -> Result<String, Box<dyn Error>> {
    match dialect {
        Dialect::SQLite => Ok(SqliteDriver::open_read_only(database)?.pull()?),
    }
}

// ----------------------------------------------------------------------------
// Migrate
// ----------------------------------------------------------------------------

/// Applies pending migrations to the configured database.
///
/// Compares the database ledger with the migration files in `migrations`.
/// The applied IDs must match the start of the numeric file order. Runs the
/// remaining Up files in that order and returns the IDs applied by this call.
///
/// Each migration's SQL and ledger entry commit atomically. On failure,
/// execution stops and earlier commits remain applied. Serialize calls to
/// `migrate` and [`rollback`] for the same database.
pub fn migrate(
    dialect: Dialect,
    database: &Path,
    migrations: &Path,
) -> Result<Vec<u64>, Box<dyn Error>> {
    let catalog = MigrationCatalog::read(migrations)?;
    match dialect {
        Dialect::SQLite => {
            let mut driver = SqliteDriver::open(database)?;
            driver::migrate(&mut driver, &catalog)
        }
    }
}

// ----------------------------------------------------------------------------
// Preview
// ----------------------------------------------------------------------------

/// Previews the migration needed to reach the desired schema.
///
/// Replays the migration files in a disposable database to establish the
/// baseline, compares it with the desired schema, then renders and verifies
/// backend-specific SQL in both directions.
///
/// Returns the same `(Up, Down)` SQL that [`generate`] would write, or `None`
/// if no migration is needed. Does not write migration files or connect to a
/// live database.
pub fn preview(
    dialect: Dialect,
    migrations: &Path,
    schema: &Path,
) -> Result<Option<(String, String)>, Box<dyn Error>> {
    Ok(planner::plan_files(dialect, migrations, schema)?.map(|(_, plan)| (plan.up, plan.down)))
}

// ----------------------------------------------------------------------------
// Generate
// ----------------------------------------------------------------------------

/// Generates a migration to reach the desired schema.
///
/// Uses the same offline planning path as [`preview`]: replays the existing
/// Up files, translates the schema diff into backend-specific operations, and
/// verifies that Up reaches the desired schema and Down restores the baseline.
///
/// Writes both SQL files to the existing `migrations` directory without
/// replacing existing files. The next ID is one greater than the highest ID
/// on disk, or `1` for an empty history. Returns the Up file's path, or `None`
/// without writing anything if no migration is needed. Serialize generation
/// per directory.
pub fn generate(
    dialect: Dialect,
    migrations: &Path,
    schema: &Path,
) -> Result<Option<PathBuf>, Box<dyn Error>> {
    let Some((id, plan)) = planner::plan_files(dialect, migrations, schema)? else {
        return Ok(None);
    };
    Ok(Some(
        Migration::new(id, migrations, plan.up, plan.down).write()?,
    ))
}

// ----------------------------------------------------------------------------
// Rollback
// ----------------------------------------------------------------------------

/// Reverts the latest applied migration and returns its ID.
///
/// Requires the migration's Down SQL and executes it atomically with removing
/// the migration from the database ledger. Returns `None` if no migrations
/// are applied. Reverting a schema change does not guarantee restoration of
/// deleted data. Serialize calls to `rollback` and [`migrate`] for the same
/// database.
pub fn rollback(
    dialect: Dialect,
    database: &Path,
    migrations: &Path,
) -> Result<Option<u64>, Box<dyn Error>> {
    match dialect {
        Dialect::SQLite => {
            let mut driver = SqliteDriver::open_existing(database)?;
            let catalog = MigrationCatalog::read(migrations)?;
            driver::rollback(&mut driver, &catalog)
        }
    }
}

// ----------------------------------------------------------------------------
// Status
// ----------------------------------------------------------------------------

/// Reports migration IDs recorded in the database ledger, in application order.
///
/// Opens an existing database read-only. If the ledger has not been initialized,
/// returns an empty list without creating it. Does not inspect migration files
/// or report pending migrations.
pub fn status(dialect: Dialect, database: &Path) -> Result<Vec<u64>, Box<dyn Error>> {
    match dialect {
        Dialect::SQLite => Ok(SqliteDriver::open_read_only(database)?
            .list()?
            .into_iter()
            .map(|record| record.id)
            .collect()),
    }
}
