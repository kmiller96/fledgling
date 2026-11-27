//! Migration planning types and the shared offline workflow.

use crate::{
    Dialect,
    backends::sqlite::planner::SqlitePlanner,
    migrations::MigrationCatalog,
    schema::{Change, DatabaseObject},
};
use std::{error::Error, fs, path::Path};

/// A rendered migration pair.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Plan {
    /// SQL that moves the database to the target definition.
    pub up: String,
    /// SQL that restores the previous definition.
    pub down: String,
}

/// Translate logical schema changes into backend SQL.
pub trait Planner {
    /// Backend schema object type.
    type Object: DatabaseObject;
    /// Planning error.
    type Error;

    /// Plan the requested changes. An empty change list has no plan.
    fn plan(&self, changes: &[Change<'_, Self::Object>]) -> Result<Option<Plan>, Self::Error>;
}

/// Load the offline inputs and ask the selected backend for a verified plan.
pub(crate) fn plan_files(
    dialect: Dialect,
    migrations: &Path,
    schema: &Path,
) -> Result<Option<(u64, Plan)>, Box<dyn Error>> {
    let catalog = MigrationCatalog::read(migrations)?;
    let desired = fs::read_to_string(schema)
        .map_err(|error| format!("reading {}: {error}", schema.display()))?;
    match dialect {
        Dialect::SQLite => SqlitePlanner::verified_plan(&catalog, &desired),
    }
}
