//! SQLite migration workflows.

use std::{
    error::Error,
    path::{Path, PathBuf},
};

use super::{
    SqliteError,
    driver::{OpenMode, SqliteDriver},
    planner::verified_plan,
};
use crate::{
    driver::Driver,
    migrations::{Ledger, MigrationFile, read_sql},
};

pub(crate) fn pull(database: &Path) -> Result<String, SqliteError> {
    Ok(SqliteDriver::open(database, OpenMode::Read)?
        .schema_sql()?
        .into_iter()
        .map(|sql| format!("{sql};\n"))
        .collect())
}

pub(crate) fn status(database: &Path) -> Result<Vec<u64>, SqliteError> {
    SqliteDriver::open(database, OpenMode::Read)?
        .ledger()
        .list()
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
    let mut driver = SqliteDriver::open(database, OpenMode::Write)?;
    let applied = driver.ledger().list()?;
    let mut ids = Vec::new();
    for file in &files[MigrationFile::applied_prefix(&files, &applied)?..] {
        let sql = read_sql(&file.up)?;
        let mut apply = || {
            let mut tx = driver.transaction()?;
            tx.execute(&sql)?;
            tx.ledger()?.commit(file.id)?;
            tx.commit()
        };
        apply().map_err(|error| format!("applying {}: {error}", file.up.display()))?;
        ids.push(file.id);
    }
    Ok(ids)
}

pub(crate) fn rollback(database: &Path, migrations: &Path) -> Result<Option<u64>, Box<dyn Error>> {
    let mut driver = SqliteDriver::open(database, OpenMode::WriteExisting)?;
    let mut tx = driver.transaction()?;
    let applied = tx.ledger()?.list()?;
    let Some(&id) = applied.last() else {
        return Ok(None);
    };
    let files = MigrationFile::list(migrations)?;
    let count = MigrationFile::applied_prefix(&files, &applied)?;
    let file = &files[count - 1];
    let sql = file.read_down()?;
    tx.execute(&sql)
        .map_err(|error| format!("reverting {}: {error}", file.down.display()))?;
    tx.ledger()?.uncommit(id)?;
    tx.commit()?;
    Ok(Some(id))
}
