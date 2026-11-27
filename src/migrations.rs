//! Backend operations, migration commands, and applied history.
//!
//! A backend turns a logical [`crate::planner::ChangeSet`] into ordered
//! [`Operation`]s and renders them into Up and Down command text. A
//! [`Migration`] holds that text for writing to disk or passing to a
//! [`crate::driver::Driver`]. A [`Ledger`] tracks applied IDs independently of
//! command execution.

use std::error::Error;

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
