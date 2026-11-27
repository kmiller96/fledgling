//! Database schema inspection and SQL migration workflows.
//!
//! Workflows are temporarily unimplemented. The documentation below describes
//! the public contract to restore.

pub mod driver;
pub mod migrations;
pub mod planner;
pub mod schema;

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
    let _ = (dialect, database);
    Err(not_implemented("pull"))
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
    let _ = (dialect, database, migrations);
    Err(not_implemented("migrate"))
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
    let _ = (dialect, migrations, schema);
    Err(not_implemented("preview"))
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
    let _ = (dialect, migrations, schema);
    Err(not_implemented("generate"))
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
    let _ = (dialect, database, migrations);
    Err(not_implemented("rollback"))
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
    let _ = (dialect, database);
    Err(not_implemented("status"))
}

// ----------------------------------------------------------------------------
// Utils
// ----------------------------------------------------------------------------

fn not_implemented(operation: &str) -> Box<dyn Error> {
    format!("{operation} is not implemented").into()
}
