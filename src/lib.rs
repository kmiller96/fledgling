//! Database schema inspection and migration workflows.

pub mod backends;
pub mod driver;
pub mod migrations;
pub mod planner;
pub mod schema;

use backends::sqlite::{
    driver::{OpenMode, SqliteDriver},
    planner::verified_plan,
};
use driver::Driver;
use migrations::{Ledger, MigrationFile};
use serde::{Deserialize, Serialize};
use std::{
    error::Error,
    fmt, fs, io,
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

/// Pulls the current schema definition from a live database in the backend's format.
///
/// Opens an existing database read-only and returns the user schema, excluding
/// the migration ledger and SQLite-owned objects. This is not a data dump.
/// Unlike [`preview`] and [`generate`], it does not read migration history.
pub fn pull(dialect: Dialect, database: &Path) -> Result<String, Box<dyn Error>> {
    match dialect {
        Dialect::SQLite => Ok(SqliteDriver::open(database, OpenMode::Read)?
            .schema_sql()?
            .into_iter()
            .map(|sql| format!("{sql};\n"))
            .collect()),
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
/// Each migration's commands and ledger entry commit atomically. On failure,
/// execution stops and earlier commits remain applied. Serialize calls to
/// `migrate` and [`rollback`] for the same database.
pub fn migrate(
    dialect: Dialect,
    database: &Path,
    migrations: &Path,
) -> Result<Vec<u64>, Box<dyn Error>> {
    let files = MigrationFile::list(migrations)?;
    let mut driver = match dialect {
        Dialect::SQLite => SqliteDriver::open(database, OpenMode::Write)?,
    };
    let applied = driver.ledger().list()?;
    let mut ids = Vec::new();
    for file in &files[applied_prefix(&files, &applied)?..] {
        let sql = read_sql(&file.up)?;
        let mut apply = || -> Result<(), backends::sqlite::SqliteError> {
            let mut transaction = driver.transaction()?;
            transaction.execute(&sql)?;
            transaction.ledger()?.commit(file.id)?;
            transaction.commit()
        };
        apply().map_err(|error| format!("applying {}: {error}", file.up.display()))?;
        ids.push(file.id);
    }
    Ok(ids)
}

// ----------------------------------------------------------------------------
// Preview
// ----------------------------------------------------------------------------

/// Previews the migration needed to reach the desired schema.
///
/// Replays the migration files in a disposable database to establish the
/// baseline, compares it with the desired schema, then renders and verifies
/// backend-specific commands in both directions.
///
/// Returns the same `(Up, Down)` command text that [`generate`] would write, or `None`
/// if no migration is needed. Does not write migration files or connect to a
/// live database.
pub fn preview(
    dialect: Dialect,
    migrations: &Path,
    schema: &Path,
) -> Result<Option<(String, String)>, Box<dyn Error>> {
    match dialect {
        Dialect::SQLite => verified_plan(&MigrationFile::list(migrations)?, schema),
    }
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
/// Writes both migration files to the existing `migrations` directory without
/// replacing existing files. The next ID is one greater than the highest ID
/// on disk, or `1` for an empty history. Returns the Up file's path, or `None`
/// without writing anything if no migration is needed. Serialize generation
/// per directory.
pub fn generate(
    dialect: Dialect,
    migrations: &Path,
    schema: &Path,
) -> Result<Option<PathBuf>, Box<dyn Error>> {
    let files = MigrationFile::list(migrations)?;
    let plan = match dialect {
        Dialect::SQLite => verified_plan(&files, schema)?,
    };
    let Some((up, down)) = plan else {
        return Ok(None);
    };
    let id = files
        .last()
        .map_or(0, |file| file.id)
        .checked_add(1)
        .ok_or("no migration IDs remain")?;
    Ok(Some(MigrationFile::publish(migrations, id, &up, &down)?))
}

// ----------------------------------------------------------------------------
// Rollback
// ----------------------------------------------------------------------------

/// Reverts the latest applied migration and returns its ID.
///
/// Requires the migration's Down commands and executes them atomically with removing
/// the migration from the database ledger. Returns `None` if no migrations
/// are applied. Reverting a schema change does not guarantee restoration of
/// deleted data. Serialize calls to `rollback` and [`migrate`] for the same
/// database.
pub fn rollback(
    dialect: Dialect,
    database: &Path,
    migrations: &Path,
) -> Result<Option<u64>, Box<dyn Error>> {
    let mut driver = match dialect {
        Dialect::SQLite => SqliteDriver::open(database, OpenMode::WriteExisting)?,
    };
    let mut transaction = driver.transaction()?;
    let applied = transaction.ledger()?.list()?;
    let Some(&id) = applied.last() else {
        return Ok(None);
    };
    let files = MigrationFile::list(migrations)?;
    let applied_count = applied_prefix(&files, &applied)?;
    let path = &files[applied_count - 1].down;
    let sql = read_sql(path).map_err(|error| {
        if error.kind() == io::ErrorKind::NotFound {
            io::Error::new(
                error.kind(),
                format!("migration {id:06} has no Down SQL: {error}"),
            )
        } else {
            error
        }
    })?;
    transaction
        .execute(&sql)
        .map_err(|error| format!("reverting {}: {error}", path.display()))?;
    transaction.ledger()?.uncommit(id)?;
    transaction.commit()?;
    Ok(Some(id))
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
        Dialect::SQLite => Ok(SqliteDriver::open(database, OpenMode::Read)?
            .ledger()
            .list()?),
    }
}

// ----------------------------------------------------------------------------
// Utils
// ----------------------------------------------------------------------------

fn applied_prefix(files: &[MigrationFile], applied: &[u64]) -> Result<usize, Box<dyn Error>> {
    for (index, &id) in applied.iter().enumerate() {
        if files.get(index).map(|file| file.id) != Some(id) {
            return Err(format!(
                "applied migration {id:06} does not match the migration files at position {}",
                index + 1
            )
            .into());
        }
    }
    Ok(applied.len())
}

fn read_sql(path: &Path) -> io::Result<String> {
    fs::read_to_string(path)
        .map_err(|error| io::Error::new(error.kind(), format!("{}: {error}", path.display())))
}
