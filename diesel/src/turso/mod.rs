//! Provides types and functions related to working with Turso.
//!
//! Turso speaks SQLite's SQL but is a different engine with a different
//! client, so it gets its own [`Backend`](crate::backend::Backend) rather
//! than riding on diesel's `Sqlite`: the raw value type is
//! `turso::Value` rather than a byte buffer, the bind collector hands the
//! driver a `Vec<turso::Value>` rather than a statement to bind onto, and
//! the SQL differs in the one place noted on
//! [`Turso`]'s `SqlDialect` impl. Everything else about the
//! dialect mirrors SQLite's.
//!
//! # Why the connection is not here
//!
//! There is no `TursoConnection` in this module. Turso's client is async,
//! so the connection implements `diesel_async::AsyncConnection`, and
//! `diesel-async` depends on `diesel` — a crate cannot depend on something
//! that depends on it. The connection therefore lives one crate out, in
//! `diesel-async`'s own `turso` module, alongside its `pg` and `mysql`
//! siblings. This module is everything a connection is not: the backend
//! marker and its dialect, the bind collector, the row and value types, the
//! SQL type impls, the query fragments, and the UNION/STRUCT support.
//!
//! That split is why a handful of items here are `pub` that would otherwise
//! be `pub(crate)` — [`row::TursoRow::new`] and
//! [`bind::TursoBindCollector::into_values`] are the seam the connection
//! reaches through.

pub(crate) mod backend;
pub mod bind;
#[cfg(feature = "turso-chrono")]
mod chrono;
pub mod expr;
pub mod pragma;
mod query_fragments;
pub mod row;
pub mod string_list;
pub(crate) mod types;
pub mod union;
mod uuid;
pub mod value;

pub use self::backend::{Turso, TursoQueryBuilder, TursoType};
pub use self::bind::{TursoBindBuffer, TursoBindCollector};
pub use self::row::{TursoField, TursoRow};
pub use self::value::TursoValue;

#[doc(hidden)]
pub mod driver {
    //! Names `#[derive(UnionSchema)]` reaches for in the code it emits.
    //!
    //! A UNION's wire form is built out of `turso::Value`s, so the derive's
    //! output has to name that type. Re-exporting it here means a crate
    //! using the derive needs `diesel` in scope and nothing else — before
    //! the fold, every such crate carried a `turso` dependency it never
    //! called, purely so the generated code would resolve.
    #[doc(hidden)]
    pub use ::turso::Value;
}

/// Turso specific sql types
pub mod sql_types {
    #[doc(inline)]
    pub use super::types::Timestamptz;
    #[doc(inline)]
    pub use super::union::TaggedUnion;
    #[doc(inline)]
    pub use super::uuid::sql_types::Uuid;
}
