//! Backend contract and shared workflows for running migrations.

use crate::migrations::{Ledger, Migration, MigrationCatalog};
use std::error::Error;

/// A connected backend that executes SQL and tracks applied history.
///
/// Each operation must commit its SQL and ledger update together or leave both
/// unchanged. A backend that cannot guarantee this must reject the operation
/// before executing SQL. The workflow, not the backend, orders batches.
pub trait Driver: Ledger {
    /// Run the Up SQL and record the migration as applied.
    /// Reject an ID that is already applied.
    fn apply(&mut self, migration: &Migration) -> Result<(), Self::Error>;

    /// Run the Down SQL and remove the migration from applied history.
    /// Reject a missing Down artifact or an ID that isn't the latest applied.
    fn revert(&mut self, migration: &Migration) -> Result<(), Self::Error>;
}

/// Apply the pending catalog entries using the driver's per-migration transaction.
pub(crate) fn migrate(
    driver: &mut impl Driver<Error: 'static>,
    catalog: &MigrationCatalog,
) -> Result<Vec<u64>, Box<dyn Error>> {
    let (_, pending) = catalog.partition(&driver.list()?)?;
    let mut applied = Vec::new();
    for migration in pending {
        driver
            .apply(migration)
            .map_err(|error| format!("applying {}: {error}", migration.up.path.display()))?;
        applied.push(migration.id);
    }
    Ok(applied)
}

/// Revert the latest applied catalog entry, if any.
pub(crate) fn rollback(
    driver: &mut impl Driver<Error: 'static>,
    catalog: &MigrationCatalog,
) -> Result<Option<u64>, Box<dyn Error>> {
    let (applied, _) = catalog.partition(&driver.list()?)?;
    let Some(migration) = applied.last() else {
        return Ok(None);
    };
    Driver::revert(driver, migration)
        .map_err(|error| format!("rolling back migration {:06}: {error}", migration.id))?;
    Ok(Some(migration.id))
}
