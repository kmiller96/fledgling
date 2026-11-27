//! PostgreSQL migration workflows.

use std::{
    error::Error,
    path::{Path, PathBuf},
};

use super::{
    PostgresError,
    driver::{PostgresDriver, PostgresTransaction},
    planner::verified_plan,
};
use crate::{
    driver::Driver,
    migrations::{Ledger, MigrationFile, read_sql},
};

pub(crate) fn pull(database: &Path) -> Result<String, PostgresError> {
    connect(database)?.schema_sql()
}

pub(crate) fn status(database: &Path) -> Result<Vec<u64>, PostgresError> {
    connect(database)?.read_ledger()?.ledger().list()
}

pub(crate) fn preview(
    migrations: &Path,
    schema: &Path,
) -> Result<Option<(String, String)>, Box<dyn Error>> {
    verified_plan(&MigrationFile::list(migrations)?, schema)
}

pub(crate) fn generate(
    migrations: &Path,
    schema: &Path,
) -> Result<Option<PathBuf>, Box<dyn Error>> {
    let files = MigrationFile::list(migrations)?;
    let Some((up, down)) = verified_plan(&files, schema)? else {
        return Ok(None);
    };
    Ok(Some(MigrationFile::publish_next(
        migrations, &files, &up, &down,
    )?))
}

pub(crate) fn migrate(database: &Path, migrations: &Path) -> Result<Vec<u64>, Box<dyn Error>> {
    let files = MigrationFile::list(migrations)?;
    let mut driver = connect(database)?;
    let applied = driver.read_ledger()?.ledger().list()?;
    let mut ids = Vec::new();
    for file in &files[MigrationFile::applied_prefix(&files, &applied)?..] {
        let sql = read_sql(&file.up)?;
        let mut apply = || {
            let mut tx = driver.transaction()?;
            tx.ledger().ensure()?;
            execute_preserving_history(&mut tx, &sql)?;
            tx.ledger().commit(file.id)?;
            tx.commit()
        };
        apply().map_err(|error| format!("applying {}: {error}", file.up.display()))?;
        ids.push(file.id);
    }
    Ok(ids)
}

pub(crate) fn rollback(database: &Path, migrations: &Path) -> Result<Option<u64>, Box<dyn Error>> {
    let mut driver = connect(database)?;
    let mut tx = driver.transaction()?;
    let applied = tx.ledger().list()?;
    let Some(&id) = applied.last() else {
        return Ok(None);
    };
    let files = MigrationFile::list(migrations)?;
    let count = MigrationFile::applied_prefix(&files, &applied)?;
    let file = &files[count - 1];
    let sql = file.read_down()?;
    execute_preserving_history(&mut tx, &sql)
        .map_err(|error| format!("reverting {}: {error}", file.down.display()))?;
    tx.ledger().uncommit(id)?;
    tx.commit()?;
    Ok(Some(id))
}

fn connect(database: &Path) -> Result<PostgresDriver, PostgresError> {
    let url = database
        .to_str()
        .filter(|url| url.starts_with("postgres://") || url.starts_with("postgresql://"))
        .ok_or_else(|| {
            PostgresError::Invalid(
                "PostgreSQL database must be a postgres:// or postgresql:// URL".into(),
            )
        })?;
    PostgresDriver::connect(url)
}

fn execute_preserving_history(
    tx: &mut PostgresTransaction<'_>,
    sql: &str,
) -> Result<(), PostgresError> {
    let before = tx.ledger().snapshot()?;
    tx.execute(sql)?;
    if tx.ledger().snapshot()? != before {
        return Err(PostgresError::Invalid(
            "migration modified the reserved PostgreSQL ledger".into(),
        ));
    }
    Ok(())
}
