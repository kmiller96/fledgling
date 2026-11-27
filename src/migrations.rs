//! Backend operations, SQL migration files, and applied history.
//!
//! A backend turns a logical [`crate::planner::ChangeSet`] into ordered
//! [`Operation`]s and renders them into Up and Down SQL. A [`Migration`] holds
//! those scripts for writing to disk or passing to a [`crate::driver::Driver`].
//! A [`Ledger`] records what was applied, independently of where SQL runs.

use std::{
    error::Error,
    io,
    path::{Path, PathBuf},
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

    /// Render this operation as one or more SQL statements without executing them.
    /// Down SQL must be planned separately; rendering does not infer a reversal.
    fn render_sql(&self) -> Result<String, Self::Error>;
}

// ----------------------------------------------------------------------------
// Migration
// ----------------------------------------------------------------------------

/// A numbered migration containing Up SQL and, when available, Down SQL.
///
/// Down SQL can reverse a schema change without restoring deleted data.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Migration {
    id: u64,
    up: String,
    down: Option<String>,
}

impl Migration {
    /// Create a migration from already-rendered SQL without validating the ID
    /// or SQL, or touching the filesystem.
    pub fn new(id: u64, up: String, down: Option<String>) -> Self {
        Self { id, up, down }
    }

    /// Return the numeric ID shared by the filenames and the ledger record.
    pub fn id(&self) -> u64 {
        self.id
    }

    /// Return the SQL script used to apply this migration.
    pub fn up(&self) -> &str {
        &self.up
    }

    /// Return the SQL script used to revert this migration, if provided.
    pub fn down(&self) -> Option<&str> {
        self.down.as_deref()
    }

    /// Persist the migration's SQL files to an existing directory.
    ///
    /// The intended format uses a positive numeric ID padded to at least six
    /// digits, as in `000001.up.sql` and, if Down SQL is present, `000001.down.sql`.
    /// Writing must not replace existing files and returns the Up file's path.
    ///
    /// # Errors
    ///
    /// Currently always returns [`io::ErrorKind::Unsupported`] without writing
    /// any files.
    pub fn write(&self, _directory: &Path) -> io::Result<PathBuf> {
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "writing migration files is not implemented",
        ))
    }
}

// ----------------------------------------------------------------------------
// Ledger
// ----------------------------------------------------------------------------

/// The identity of a currently applied migration, without its SQL contents.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct AppliedMigration {
    /// Numeric ID of the corresponding [`Migration`].
    pub id: u64,
}

/// Ordered history of migrations currently applied to a target database.
///
/// The ledger can live in the target database or in an independent store,
/// such as a local file or Redis. It records history; it does not execute SQL
/// or make history updates atomic with SQL execution.
pub trait Ledger {
    /// Error returned when a history operation fails.
    type Error: Error;

    /// List currently applied migrations in application order without changing
    /// storage. Return an empty list if none are applied.
    fn list(&self) -> Result<Vec<AppliedMigration>, Self::Error>;

    /// Append a record to the end of the applied history. Reject duplicate IDs.
    fn record(&mut self, migration: AppliedMigration) -> Result<(), Self::Error>;

    /// Remove the most recently recorded migration, not the highest numeric ID.
    /// Reject an empty ledger or an ID that does not match the latest record.
    fn remove_latest(&mut self, id: u64) -> Result<(), Self::Error>;
}
