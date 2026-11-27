//! Errors returned by backend implementations.

use std::{error::Error, fmt};

// ----------------------------------------------------------------------------
// SQLite
// ----------------------------------------------------------------------------

/// An SQLite database or backend contract error.
#[derive(Debug)]
pub enum SqliteError {
    /// An error returned by SQLite or its Rust driver.
    Database(rusqlite::Error),
    /// Input or database state that violates a backend contract.
    Invalid(String),
}

impl fmt::Display for SqliteError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Database(error) => error.fmt(f),
            Self::Invalid(message) => f.write_str(message),
        }
    }
}

impl Error for SqliteError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Database(error) => Some(error),
            Self::Invalid(_) => None,
        }
    }
}

impl From<rusqlite::Error> for SqliteError {
    fn from(error: rusqlite::Error) -> Self {
        Self::Database(error)
    }
}

/// The SQL could not be parsed as one supported SQLite object in `main`.
#[derive(Debug)]
pub struct ObjectError(pub(super) String);

impl fmt::Display for ObjectError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl Error for ObjectError {}

/// A PostgreSQL database or backend contract error.
#[derive(Debug)]
pub enum PostgresError {
    /// An error returned by PostgreSQL or its Rust driver.
    Database(postgres::Error),
    /// Input or database state that violates a backend contract.
    Invalid(String),
}

impl fmt::Display for PostgresError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Database(error) => match error.as_db_error() {
                Some(error) => write!(f, "{}: {}", error.severity(), error.message()),
                None => error.fmt(f),
            },
            Self::Invalid(message) => f.write_str(message),
        }
    }
}

impl Error for PostgresError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Database(error) => Some(error),
            Self::Invalid(_) => None,
        }
    }
}

impl From<postgres::Error> for PostgresError {
    fn from(error: postgres::Error) -> Self {
        Self::Database(error)
    }
}
