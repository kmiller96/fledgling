//! SQL migration files and the backend contract for tracking applied migrations.

use std::{
    error::Error,
    fs,
    io::{self, Write},
    path::{Path, PathBuf},
};

fn parse_id(stem: &str) -> io::Result<u64> {
    stem.parse()
        .ok()
        .filter(|&id| id != 0 && stem.bytes().all(|byte| byte.is_ascii_digit()))
        .ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                "expected a positive numeric migration ID",
            )
        })
}

// ----------------------------------------------------------------------------
// Artifact
// ----------------------------------------------------------------------------

/// A migration's SQL file and its exact UTF-8 contents.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Artifact {
    /// Path to the SQL file.
    pub path: PathBuf,
    /// SQL contents.
    pub sql: String,
}

impl Artifact {
    /// Loads an artifact from the disk.
    fn load(path: PathBuf) -> io::Result<Self> {
        let sql = fs::read_to_string(&path)?;
        Ok(Self { path, sql })
    }

    fn create(&self) -> io::Result<()> {
        let contextualize = |error: io::Error| {
            io::Error::new(error.kind(), format!("{}: {error}", self.path.display()))
        };
        let mut file = fs::File::create_new(&self.path).map_err(contextualize)?;
        if let Err(error) = file.write_all(self.sql.as_bytes()) {
            drop(file);
            let _ = fs::remove_file(&self.path);
            return Err(contextualize(error));
        }
        Ok(())
    }
}

// ----------------------------------------------------------------------------
// Migrations
// ----------------------------------------------------------------------------

/// An executable migration with required Up SQL and optional Down SQL.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Migration {
    /// Database version ID, padded to at least six digits when written.
    // TODO: Allow an on-disk name separate from the database version.
    pub id: u64,

    /// Up migration artifact.
    pub up: Artifact,

    /// Down migration artifact.
    pub down: Option<Artifact>,
}

impl Migration {
    pub fn new(id: u64, directory: &Path, up: String, down: String) -> Self {
        Self {
            id,
            up: Artifact {
                path: directory.join(format!("{id:06}.up.sql")),
                sql: up,
            },
            down: Some(Artifact {
                path: directory.join(format!("{id:06}.down.sql")),
                sql: down,
            }),
        }
    }

    /// Read an Up file and its optional Down file with the same ID prefix.
    fn read(up: &Path) -> io::Result<Self> {
        let stem = up
            .file_name()
            .and_then(|name| name.to_str())
            .and_then(|name| name.strip_suffix(".up.sql"))
            .unwrap_or_default();
        let id = parse_id(stem)?;
        let down = up.with_file_name(format!("{stem}.down.sql"));
        let up = Artifact::load(up.to_owned())?;
        let down = match down.symlink_metadata() {
            Ok(_) => Some(Artifact::load(down)?),
            Err(error) if error.kind() == io::ErrorKind::NotFound => None,
            Err(error) => return Err(error),
        };
        Ok(Self { id, up, down })
    }

    /// Write Down before Up; Up is the file used to discover a migration.
    pub fn write(&self) -> io::Result<PathBuf> {
        if let Some(down) = &self.down {
            down.create()?;
        }
        if let Err(error) = self.up.create() {
            if let Some(down) = &self.down {
                let _ = fs::remove_file(&down.path);
            }
            return Err(error);
        }
        Ok(self.up.path.clone())
    }
}

// ----------------------------------------------------------------------------
// Migration catalog
// ----------------------------------------------------------------------------

/// On-disk migrations in numeric order with unique IDs.
pub(crate) struct MigrationCatalog(Vec<Migration>);

impl MigrationCatalog {
    /// Load Up files and their optional Down partners.
    pub(crate) fn read(directory: &Path) -> io::Result<Self> {
        let mut migrations = Vec::new();
        for entry in fs::read_dir(directory)? {
            let path = entry?.path();
            if path.as_os_str().as_encoded_bytes().ends_with(b".up.sql") {
                migrations.push(Migration::read(&path).map_err(|error| {
                    io::Error::new(error.kind(), format!("{}: {error}", path.display()))
                })?);
            }
        }
        migrations.sort_by_key(|migration| migration.id);
        for pair in migrations.windows(2) {
            if pair[0].id == pair[1].id {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!("duplicate migration ID {:06}", pair[0].id),
                ));
            }
        }
        Ok(Self(migrations))
    }

    pub(crate) fn iter(&self) -> impl Iterator<Item = &Migration> {
        self.0.iter()
    }

    /// Use one more than the highest ID on disk, or 1 for an empty catalog.
    pub(crate) fn next_id(&self) -> io::Result<u64> {
        self.0
            .last()
            .map_or(Some(1), |migration| migration.id.checked_add(1))
            .ok_or_else(|| io::Error::other("no migration IDs remain"))
    }

    /// Match ledger IDs to an exact prefix, then return (applied, pending).
    pub(crate) fn partition(
        &self,
        applied: &[AppliedMigration],
    ) -> io::Result<(&[Migration], &[Migration])> {
        for (index, record) in applied.iter().enumerate() {
            if self.0.get(index).map(|migration| migration.id) != Some(record.id) {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!(
                        "applied migration {:06} does not match the migration files at position {}",
                        record.id,
                        index + 1
                    ),
                ));
            }
        }
        Ok(self.0.split_at(applied.len()))
    }
}

// ----------------------------------------------------------------------------
// Ledger
// ----------------------------------------------------------------------------

/// An applied migration record, separate from its executable SQL files.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct AppliedMigration {
    pub id: u64,
}

/// Records of currently applied migrations. The storage format is backend-specific.
pub trait Ledger {
    type Error: Error;

    /// List currently applied migrations in application order without changing storage.
    /// Return an empty list if no migrations are applied.
    fn list(&self) -> Result<Vec<AppliedMigration>, Self::Error>;

    /// Append an applied migration. Reject an ID already in the ledger.
    ///
    /// When applying SQL, this update must be part of the same atomic operation.
    #[allow(dead_code)]
    fn commit(&mut self, migration: AppliedMigration) -> Result<(), Self::Error>;

    /// Remove the latest applied record. Reject an empty ledger or a mismatched ID.
    ///
    /// When reverting SQL, this update must be part of the same atomic operation.
    #[allow(dead_code)]
    fn revert(&mut self, id: u64) -> Result<(), Self::Error>;
}

#[cfg(test)]
pub(crate) mod tests {
    use super::{AppliedMigration, Ledger};
    use std::io;

    /// Check the commit, list, and revert contract against a fresh ledger.
    pub(crate) fn assert_commit_list_revert(mut ledger: impl Ledger) {
        let first = AppliedMigration { id: 20 };
        let second = AppliedMigration { id: 10 };

        assert!(ledger.list().unwrap().is_empty());

        ledger.commit(first).unwrap();
        assert_eq!(ledger.list().unwrap(), [first]);

        ledger.commit(second).unwrap();
        assert_eq!(ledger.list().unwrap(), [first, second]);

        ledger.revert(second.id).unwrap();
        assert_eq!(ledger.list().unwrap(), [first]);

        ledger.revert(first.id).unwrap();
        assert!(ledger.list().unwrap().is_empty());
    }

    #[derive(Default)]
    struct MemoryLedger(Vec<AppliedMigration>);

    impl Ledger for MemoryLedger {
        type Error = io::Error;

        fn list(&self) -> io::Result<Vec<AppliedMigration>> {
            Ok(self.0.clone())
        }

        fn commit(&mut self, migration: AppliedMigration) -> io::Result<()> {
            if self.0.contains(&migration) {
                return Err(io::Error::other("migration already applied"));
            }
            self.0.push(migration);
            Ok(())
        }

        fn revert(&mut self, id: u64) -> io::Result<()> {
            if self.0.last().map(|migration| migration.id) != Some(id) {
                return Err(io::Error::other("migration is not the latest applied"));
            }
            self.0.pop();
            Ok(())
        }
    }

    #[test]
    fn lists_committed_migrations_until_reverted() {
        assert_commit_list_revert(MemoryLedger::default());
    }
}
