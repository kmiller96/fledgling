//! Database schema previews, migration generation, and migration execution.
//!
//! Projects describe their desired schema in SQL and keep versioned Up/Down
//! files in a disk ledger. Planning compares the desired schema to the
//! replayed migrations, not to the live database. The internal schema model is not
//! a public DSL. [`sqlite`] is the supported backend.
//!
//! Callers must serialize apply and rollback runs against the same database:
//! atomicity of individual migrations does not coordinate overlapping runs.

pub mod config;
pub mod migrations;
pub mod schema;

pub mod sqlite;

pub use migrations::{
    AppliedMigration, CatalogError, FailureOutcome, Migration, MigrationCatalog, MigrationExecutor,
    MigrationSql, Planner,
};
pub use sqlite::SqliteAdaptor;

/// Plan and publish the next Up/Down pair.
///
/// Returns `None` without writing files for a no-op. The destination directory
/// must exist; callers coordinate catalog freshness and concurrent publication.
pub fn plan_and_publish(
    planner: &impl Planner,
    directory: impl AsRef<std::path::Path>,
    catalog: &MigrationCatalog,
    desired_schema_sql: &str,
) -> Result<Option<std::path::PathBuf>, Box<dyn std::error::Error>> {
    let Some(MigrationSql { up, down }) = planner.plan(catalog, desired_schema_sql)? else {
        return Ok(None);
    };
    let id = catalog.next_id()?;
    let directory = directory.as_ref();
    let stem = format!("{id:06}");
    let migration = Migration {
        id,
        up: migrations::Artifact {
            path: directory.join(format!("{stem}.up.sql")),
            sql: up,
        },
        down: Some(migrations::Artifact {
            path: directory.join(format!("{stem}.down.sql")),
            sql: down,
        }),
    };
    Ok(Some(migration.write(directory)?))
}
