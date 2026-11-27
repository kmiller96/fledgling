//! Backend operations, migration commands, and applied history.
//!
//! A backend turns a logical [`crate::planner::ChangeSet`] into ordered
//! [`Operation`]s and renders them into Up and Down command text. A
//! [`Migration`] holds that text for writing to disk or passing to a
//! [`crate::driver::Driver`]. A [`Ledger`] tracks applied IDs independently of
//! command execution.

use std::{
    error::Error,
    fs::{self, File},
    io::{self, Write},
    path::{Path, PathBuf},
    sync::atomic::{AtomicU64, Ordering},
};

// ----------------------------------------------------------------------------
// Operation
// ----------------------------------------------------------------------------

/// A backend-defined execution step, such as adding a column or creating a view.
///
/// Unlike a logical schema change, an operation describes how to make the
/// change in a particular backend.
pub trait Operation {
    /// Error returned when the backend cannot render the operation.
    type Error: Error;

    /// Render this operation as a command in the backend's language.
    fn render(&self) -> Result<String, Self::Error>;
}

// ----------------------------------------------------------------------------
// Migration
// ----------------------------------------------------------------------------

/// UTF-8 Encoding of a backend command.
///
/// Typically this would be SQL but in reality it could be anything e.g. Cypher
/// for Neo4j.
type Command = String;

/// An "up" command, and optionally an inverting "down" command, representing a migration.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Migration {
    /// Command text used to apply this migration.
    pub up: Command,
    /// Command text used to revert this migration, if provided.
    pub down: Option<Command>,
}

/// A migration with an identity in the migration history.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct VersionedMigration {
    /// Numeric ID shared by the filenames and the ledger entry.
    pub id: u64,
    /// Commands to apply or revert this migration.
    pub migration: Migration,
}

// ----------------------------------------------------------------------------
// Migration files
// ----------------------------------------------------------------------------
// This section was AI-generated and requires human review.

/// An on-disk migration identified by its Up file, without loading its SQL.
pub(crate) struct MigrationFile {
    /// Positive numeric ID parsed from the Up filename.
    pub id: u64,
    /// Path to the Up SQL file.
    pub up: PathBuf,
    /// Path to the matching Down SQL file, whether or not it exists.
    pub down: PathBuf,
}

impl MigrationFile {
    /// Check that the applied IDs match the start of the on-disk history.
    pub fn applied_prefix(files: &[Self], applied: &[u64]) -> io::Result<usize> {
        for (index, &id) in applied.iter().enumerate() {
            if files.get(index).map(|file| file.id) != Some(id) {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!(
                        "applied migration {id:06} does not match the migration files at position {}",
                        index + 1
                    ),
                ));
            }
        }
        Ok(applied.len())
    }

    /// Load Down SQL, with a specific error when the migration has no Down file.
    pub fn read_down(&self) -> io::Result<String> {
        read_sql(&self.down).map_err(|error| {
            if error.kind() == io::ErrorKind::NotFound {
                io::Error::new(
                    error.kind(),
                    format!("migration {:06} has no Down SQL: {error}", self.id),
                )
            } else {
                error
            }
        })
    }

    /// Finds `*.up.sql` entries in numeric ID order.
    ///
    /// Rejects invalid IDs and duplicate numeric IDs, including differently
    /// padded filenames for the same ID. Does not read file contents or
    /// require matching Down files.
    pub fn list(directory: &Path) -> io::Result<Vec<Self>> {
        let mut files = Vec::new();
        for entry in fs::read_dir(directory)? {
            if let Some(file) = Self::from_path(entry?.path())? {
                files.push(file);
            }
        }
        files.sort_by_key(|file| file.id);
        if let Some(pair) = files.windows(2).find(|pair| pair[0].id == pair[1].id) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("duplicate migration ID {:06}", pair[0].id),
            ));
        }
        Ok(files)
    }

    /// Ignores unrelated names and rejects malformed Up filenames.
    fn from_path(up: PathBuf) -> io::Result<Option<Self>> {
        let Some(stem) = up
            .file_name()
            .and_then(|name| name.as_encoded_bytes().strip_suffix(b".up.sql"))
        else {
            return Ok(None);
        };

        // Check the bytes so non-UTF-8 names and signs aren't accepted as IDs.
        if let Ok(stem) = str::from_utf8(stem)
            && stem.bytes().all(|byte| byte.is_ascii_digit())
            && let Ok(id @ 1..) = stem.parse::<u64>()
        {
            let down = up.with_file_name(format!("{stem}.down.sql"));
            return Ok(Some(Self { id, up, down }));
        }
        Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("{}: expected a positive numeric migration ID", up.display()),
        ))
    }

    /// Writes an Up/Down pair at the next numeric ID and returns the Up path.
    ///
    /// Expects sorted file history. Uses six-digit minimum padding and never
    /// overwrites an existing file. Publishes Down first, so discovery cannot
    /// see Up before its partner exists. If publishing Up fails, attempts to
    /// remove the new Down file. An interruption can still leave a Down file behind.
    pub fn publish_next(
        directory: &Path,
        files: &[Self],
        up: &str,
        down: &str,
    ) -> io::Result<PathBuf> {
        let id = files
            .last()
            .map_or(0, |file| file.id)
            .checked_add(1)
            .ok_or_else(|| io::Error::other("no migration IDs remain"))?;
        let up_path = directory.join(format!("{id:06}.up.sql"));
        let down_path = directory.join(format!("{id:06}.down.sql"));
        Self::publish_sql(directory, &down_path, down)?;
        if let Err(error) = Self::publish_sql(directory, &up_path, up) {
            let _ = fs::remove_file(&down_path);
            return Err(error);
        }
        Ok(up_path)
    }
    /// Stages SQL next to its destination, then links it without replacing it.
    fn publish_sql(directory: &Path, path: &Path, sql: &str) -> io::Result<()> {
        static NEXT_TEMP_ID: AtomicU64 = AtomicU64::new(0);

        let contextualize =
            |error: io::Error| io::Error::new(error.kind(), format!("{}: {error}", path.display()));
        let (temp, mut file) = loop {
            let id = NEXT_TEMP_ID.fetch_add(1, Ordering::Relaxed);
            let temp = directory.join(format!(".fledgling-{}-{id}.tmp", std::process::id()));
            match File::create_new(&temp) {
                Ok(file) => break (temp, file),
                Err(error) if error.kind() == io::ErrorKind::AlreadyExists => continue,
                Err(error) => return Err(contextualize(error)),
            }
        };

        let written = file.write_all(sql.as_bytes());
        drop(file);
        let published = written.and_then(|()| fs::hard_link(&temp, path));
        let _ = fs::remove_file(temp);
        published.map_err(contextualize)
    }
}

pub(crate) fn read_sql(path: &Path) -> io::Result<String> {
    fs::read_to_string(path)
        .map_err(|error| io::Error::new(error.kind(), format!("{}: {error}", path.display())))
}

// ----------------------------------------------------------------------------
// Ledger
// ----------------------------------------------------------------------------

/// Ordered history of IDs for [`VersionedMigration`]s currently applied to a target database.
///
/// The ledger can live in the target database or in an independent store,
/// such as a local file or Redis. It does not execute commands or make history
/// updates atomic with execution.
pub trait Ledger {
    /// Error returned when a history operation fails.
    type Error: Error;

    /// List applied migration IDs.
    ///
    /// Return an empty list if no migrations are applied.
    fn list(&self) -> Result<Vec<u64>, Self::Error>;

    /// Commit a new migration to the ledger.
    ///
    /// Returns an error if the ID is already applied.
    fn commit(&mut self, id: u64) -> Result<(), Self::Error>;

    /// Removes the latest committed ID.
    ///
    /// Returns an error if the ledger is empty or `id` is not the latest entry.
    fn uncommit(&mut self, id: u64) -> Result<(), Self::Error>;
}
