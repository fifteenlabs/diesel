//! An async connection to a Turso database.
//!
//! The backend itself — `Turso`, its `SqlDialect`, the bind collector, the
//! row and value types, the SQL type impls and the UNION/STRUCT support —
//! lives in [`diesel::turso`]. Only the parts that need a live connection
//! are here: the [`AsyncConnection`](crate::AsyncConnection) impl, the
//! transaction manager, the `turso::Error` → `diesel::result::Error`
//! mapping, and the establish-time check that the database's stored type
//! declarations still match the derives compiled into this binary.
//!
//! The split is not a preference. Turso's client is async, so the
//! connection implements a `diesel-async` trait, and `diesel-async` depends
//! on `diesel`; a `TursoConnection` inside `diesel` would be a dependency
//! cycle. Everything that does *not* need the driver's connection stays in
//! `diesel`, where implementing `ToSql`/`FromSql` for foreign types is
//! diesel's own business rather than an orphan-rule accident.

mod connection;
mod error;
pub mod probe;
mod transaction;

pub use self::connection::{StatementCacheStats, TursoConnection};
pub use self::error::{turso_to_connection, turso_to_diesel};
pub use self::transaction::TursoTransactionManager;
