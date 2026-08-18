//! Error plumbing between turso, diesel, and anyhow consumers.

use diesel::result::{ConnectionError, DatabaseErrorKind, Error as DieselError};

/// Translate a `turso::Error` into the closest diesel `Error`.
///
/// Turso doesn't yet surface structured error kinds (unique violation,
/// check constraint, etc.), so everything funnels through
/// `DatabaseErrorKind::Unknown`. Refine this mapping as turso's error
/// taxonomy grows.
pub fn turso_to_diesel(e: turso::Error) -> DieselError {
    DieselError::DatabaseError(
        DatabaseErrorKind::Unknown,
        Box::new(TursoDbError(e.to_string())),
    )
}

/// Same as [`turso_to_diesel`] but wrapped as a `ConnectionError` for the
/// `establish` path.
pub fn turso_to_connection(e: turso::Error) -> ConnectionError {
    ConnectionError::BadConnection(e.to_string())
}

/// Minimal `DatabaseErrorInformation` impl that only exposes a message.
#[derive(Debug)]
struct TursoDbError(String);

impl diesel::result::DatabaseErrorInformation for TursoDbError {
    fn message(&self) -> &str {
        &self.0
    }
    fn details(&self) -> Option<&str> {
        None
    }
    fn hint(&self) -> Option<&str> {
        None
    }
    fn table_name(&self) -> Option<&str> {
        None
    }
    fn column_name(&self) -> Option<&str> {
        None
    }
    fn constraint_name(&self) -> Option<&str> {
        None
    }
    fn statement_position(&self) -> Option<i32> {
        None
    }
}
