//! SQLite backend.
//!
//! The backend provides schema objects, SQL rendering and execution, and
//! migration history storage. Planning and workflow orchestration are separate.

pub mod driver;
pub mod ledger;
pub mod objects;
pub mod operations;
mod planner;
pub(crate) mod workflow;

pub use super::errors::SqliteError;
