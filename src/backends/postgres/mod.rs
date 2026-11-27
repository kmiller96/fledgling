//! PostgreSQL schema inspection, planning, execution, and applied history.

pub mod driver;
pub mod ledger;
mod objects;
mod operations;
mod planner;
mod pull;
pub(crate) mod workflow;

pub use super::errors::PostgresError;

fn quote(identifier: &str) -> String {
    format!("\"{}\"", identifier.replace('"', "\"\""))
}
