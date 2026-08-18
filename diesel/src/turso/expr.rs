//! The two SQL functions diesel's DSL has no name for.
//!
//! Diesel models every expression as a type, which is why a typo in a column
//! name is a compile error — but the flip side is that an expression diesel
//! has no type for cannot be written at all, and the way out is
//! `crate::sql_query` for the whole statement. That trade is a bad one: one
//! missing function costs the checking on every column, bind and table name in
//! the statement around it, and a `SqlLiteral` is unsafe to cache, so the
//! statement stops being statement-cacheable into the bargain.
//!
//! So the missing pieces live here. They are ordinary diesel expressions: they
//! compose with `filter`, `select`, `set` and each other, their operand types
//! are checked, and their binds go through the normal bind collector in the
//! normal order.
//!
//! **`CASE` is not among them.** Diesel ships [`crate::dsl::case_when`], and
//! this module used to carry a second, parallel `CASE` node under the same
//! name — 276 lines of it — while `presage-store-diesel` used diesel's. Two
//! nodes spelled `case_when`, on one backend, in one workspace. The
//! justification recorded here was a compile-time explosion: a five-arm ladder
//! was said to have pinned rustc at 100% CPU for 46 minutes. That was measured
//! against an earlier design that inferred an SQL type per arm, *not* against
//! diesel's function with the type pinned by turbofish, which was never tried.
//! It is free: checking the app's real five-arm ladder (whatsappdb's
//! media-download backoff, in its actual query) costs 1.0s either way, against
//! a 1.0s baseline with no ladder at all — diesel's `.when` resolves each arm's
//! type from the immediately preceding one through `CaseWhenTypesExtractor`,
//! deliberately non-recursively, for exactly this reason. So write
//! `crate::dsl::case_when::<_, _, SqlType>(…)`; the turbofish pins the type
//! the same way naming it up front did.
//!
//! What is left is what diesel genuinely has no equivalent for: a two-argument
//! scalar `max` (diesel's `max` is the one-argument aggregate) and `coalesce`
//! (diesel has none at all). Both are `define_sql_function!` declarations
//! rather than hand-written nodes, so they are backend-generic and carry no
//! impls of their own — the SQLite-family spelling is the only thing tying
//! them to [`Turso`].

use crate::sql_types::SingleValue;

crate::define_sql_function! {
    /// SQLite's two-argument `max(a, b)` — the *scalar* one, not the
    /// aggregate `MAX(col)` diesel already has.
    ///
    /// Written as `max2` because `max` is taken by the aggregate; it renders
    /// as `max`, which is what SQLite dispatches on the argument count.
    /// Used for monotonic writes ("never let this column go backwards"),
    /// where the alternative is reading the row first and racing.
    #[sql_name = "max"]
    fn max2<T: SingleValue>(a: T, b: T) -> T;
}

crate::define_sql_function! {
    /// `coalesce(a, b)` — `a` unless it is NULL, in which case `b`.
    ///
    /// The two-argument form, which is the only one the app uses. Typed so
    /// that the fallback is non-nullable and the result therefore is too,
    /// which is the point: it is how a `Nullable` column reaches a place
    /// that needs a value.
    fn coalesce<T: SingleValue>(a: crate::sql_types::Nullable<T>, b: T) -> T;
}
