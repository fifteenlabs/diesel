//! Provides types and functions related to working with Turso.
//!
//! Turso speaks SQLite's SQL but is a different engine with a different
//! client, so it gets its own [`Backend`](crate::backend::Backend) rather
//! than riding on diesel's `Sqlite`: the raw value type is
//! `turso::Value` rather than a byte buffer, the bind collector hands the
//! driver a `Vec<turso::Value>` rather than a statement to bind onto, and
//! and the SQL differs in the two places noted on [`Turso`]'s `SqlDialect`
//! impl — an unparenthesized join `FROM` clause, and a `LIMIT` inside a
//! subselect, which is written into the text rather than bound because
//! Turso's planner discards a placeholder there. Everything else about the
//! dialect mirrors SQLite's.
//!
//! # What is here
//!
//! Everything: the backend marker and its dialect, the bind collector, the
//! row and value types, the SQL type impls, the query fragments, the
//! UNION/STRUCT support, and [`TursoConnection`] itself.
//!
//! That last one is recent. Turso's client is async, so the connection
//! implements [`AsyncConnection`](crate::connection::AsyncConnection), and
//! while those traits lived in a separate `diesel-async` crate — which
//! depends on `diesel` — a `TursoConnection` in this module would have been
//! a dependency cycle. The backend was split across two crates for that
//! reason alone, and it showed: a consumer named the `turso` feature twice,
//! once on each crate, and could get a build where half the backend was
//! compiled and the other half was not. Folding `diesel-async` in removed
//! the cycle by removing the second crate, so the whole backend is now one
//! module behind one feature.
//!
//! A handful of items are `pub` that would otherwise be `pub(crate)` —
//! [`row::TursoRow::new`] and [`bind::TursoBindCollector::into_values`].
//! They were the seam the connection reached through from the other crate.
//! They can be narrowed now that there is no other crate; they are left
//! alone here so that this change is a move and not also an API edit.

pub mod array_comparison;
pub(crate) mod backend;
pub mod bind;
mod connection;
// Gated on `chrono` rather than on a `turso-chrono` of its own. Requirement
// one is that a single feature turns the backend on, and a separate
// `turso-chrono` was the second switch a consumer had to remember — while
// diesel's other backends have always taken the plain `chrono` feature for
// exactly this (see `sqlite/types/date_and_time/chrono.rs`).
#[cfg(feature = "chrono")]
mod chrono;
mod error;
pub mod expr;
pub mod pragma;
pub mod probe;
mod query_fragments;
pub mod row;
pub mod string_list;
pub(crate) mod types;
pub mod union;
mod uuid;
pub mod value;

pub use self::array_comparison::JsonList;
pub use self::backend::{Turso, TursoJsonArrayComparison, TursoQueryBuilder, TursoType};
pub use self::bind::{TursoBindBuffer, TursoBindCollector};
pub use self::connection::{StatementCacheStats, TursoConnection};
pub use self::error::{turso_to_connection, turso_to_diesel};
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
