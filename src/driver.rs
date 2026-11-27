//! Execution boundary for Up or Down migration commands.
//!
//! A future workflow will select migrations, coordinate history updates with
//! execution, and handle failures. A driver does not implement or own a ledger.
//! A shared transaction is possible when both use the same database transaction;
//! external stores need a separate coordination and recovery policy.

use std::error::Error;

/// A connected backend that executes migration commands in its own language.
///
/// No drivers are implemented yet.
pub trait Driver {
    type Error: Error;

    /// Execute the commands for one migration direction against the target database.
    /// Does not read or update applied history.
    fn execute(&mut self, commands: &str) -> Result<(), Self::Error>;
}
