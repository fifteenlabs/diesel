//! The SQL functions and operators diesel's DSL has no name for.
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
//! What is left is what diesel genuinely has no equivalent for: the
//! two-argument scalar `max` and `min` (diesel's `max` and `min` are the
//! one-argument aggregates), `coalesce` in both its arities (diesel has none
//! at all — the name appears nowhere in the tree), and bitwise `&` (diesel
//! declares one, `AndNet`, and it is typed to Pg's `Inet`). The functions are
//! `define_sql_function!` declarations rather than hand-written nodes, so they
//! are backend-generic and carry no impls of their own — the SQLite-family
//! spelling is the only thing tying them to [`crate::turso::Turso`]. The
//! operator is a plain [`crate::infix_operator!`], the same mechanism the Pg
//! backend declares its own operators with, and is backend-pinned because `&`
//! is not portable spelling.
//!
//! [`exists_over`] is the one thing here that is not a missing piece: it is
//! sugar over [`crate::dsl::exists`], and it lives here because the callers do.

use crate::expression::grouped::Grouped;
use crate::expression::{AsExpression, Expression, TypedExpressionType};
use crate::sql_types::{Nullable, SingleValue, SqlType};

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
    /// SQLite's two-argument `min(a, b)` — the *scalar* one, not the
    /// aggregate `MIN(col)` diesel already has.
    ///
    /// The mirror of [`max2`], and named the same way for the same reason.
    /// Used where a column must never move forwards: the earlier of a
    /// stored timestamp and an arriving one, the oldest message either side
    /// of an upsert has seen.
    ///
    /// Note that SQLite's scalar `min` returns NULL if *either* argument is
    /// NULL, unlike the aggregate, which skips them. Pin `T` to a
    /// `Nullable` type and the signature says so; pin it to a bare one and
    /// the type system holds you to arguments that cannot be NULL.
    #[sql_name = "min"]
    fn min2<T: SingleValue>(a: T, b: T) -> T;
}

crate::define_sql_function! {
    /// `coalesce(a, b)` — `a` unless it is NULL, in which case `b`.
    ///
    /// The two-argument form, which is the only one the app uses. Typed so
    /// that the fallback is non-nullable and the result therefore is too,
    /// which is the point: it is how a `Nullable` column reaches a place
    /// that needs a value. When the fallback is itself nullable — one
    /// nullable column standing in for another, which is what an upsert's
    /// "don't clobber what we know with the NULL that means we don't know"
    /// looks like — reach for [`coalesce_opt`].
    fn coalesce<T: SingleValue>(a: Nullable<T>, b: T) -> T;
}

crate::define_sql_function! {
    /// `coalesce(a, b)` where the fallback may itself be NULL, so the
    /// result may be too.
    ///
    /// Renders identically to [`coalesce`]; the two differ only in type,
    /// and both arities are needed because diesel resolves a function's
    /// nullability from its declaration rather than its arguments. This is
    /// the arity an `ON CONFLICT DO UPDATE` wants: `coalesce_opt(excluded.x,
    /// table.x)` keeps a value already learned when the incoming row has
    /// none.
    #[sql_name = "coalesce"]
    fn coalesce_opt<T: SingleValue>(a: Nullable<T>, b: Nullable<T>) -> Nullable<T>;
}

crate::__diesel_infix_operator!(
    BitAnd,
    " & ",
    __diesel_internal_SameResultAsInput,
    backend: crate::turso::Turso
);

/// `a & b` — bitwise AND, for the columns that are bitsets.
///
/// The result takes the left operand's type, so a mask over an `Integer`
/// column is an `Integer` and can be compared with `.eq`/`.ne` like any
/// other expression: `bit_and(messages::kind, AUDIO_BITS).ne(0)`.
///
/// The point of having it is the right-hand side: written as an expression
/// the mask *binds*, so a bitset test is one cached statement however many
/// masks it is asked about, where the `sql()` fragment it replaces was a
/// `SqlLiteral` — which diesel reports as unsafe to cache, taking the whole
/// statement around it out of the cache with it.
pub fn bit_and<T, U>(a: T, b: U) -> Grouped<BitAnd<T, U::Expression>>
where
    T: Expression,
    T::SqlType: SqlType + TypedExpressionType,
    U: AsExpression<T::SqlType>,
{
    Grouped(BitAnd::new(a, b.as_expression()))
}

/// `EXISTS (SELECT 1 FROM …)` — an existence test over a query, without
/// having to name the projection nobody reads.
///
/// The only thing separating `exists(q)` from what an existence test wants
/// is that `q` still projects its table's columns, so every caller writes
/// `exists(q.select(1.into_sql::<Integer>()))` and imports two more names to
/// say it. This says it once. It is sugar over
/// [`exists`](crate::dsl::exists), not a missing node — it renders exactly
/// what the long spelling does, on every backend.
pub fn exists_over<T>(
    query: T,
) -> crate::helper_types::exists<
    crate::helper_types::Select<T, crate::helper_types::AsExprOf<i32, crate::sql_types::Integer>>,
>
where
    T: crate::query_dsl::methods::SelectDsl<
        crate::helper_types::AsExprOf<i32, crate::sql_types::Integer>,
    >,
{
    use crate::expression::IntoSql;
    crate::dsl::exists(query.select(1.into_sql::<crate::sql_types::Integer>()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::prelude::*;
    use crate::turso::Turso;
    use crate::upsert::excluded;

    crate::table! {
        watermarks (chat) {
            chat -> Text,
            oldest_seen -> BigInt,
            newest_seen -> BigInt,
            title -> Nullable<Text>,
            kind -> Integer,
        }
    }

    fn sql_of<Q>(q: &Q) -> String
    where
        Q: crate::query_builder::QueryFragment<Turso> + crate::query_builder::QueryId,
    {
        crate::debug_query::<Turso, _>(q).to_string()
    }

    /// The scalar `min`/`max` render as the two-argument SQLite functions,
    /// with the second operand bound rather than interpolated.
    #[test]
    fn scalar_min_and_max() {
        assert_eq!(
            sql_of(&watermarks::table.select(min2(watermarks::oldest_seen, 5_i64))),
            r#"SELECT min("watermarks"."oldest_seen", ?) FROM "watermarks" -- binds: [5]"#
        );
        assert_eq!(
            sql_of(&watermarks::table.select(max2(watermarks::newest_seen, 5_i64))),
            r#"SELECT max("watermarks"."newest_seen", ?) FROM "watermarks" -- binds: [5]"#
        );
    }

    /// Both coalesce arities render the same SQL; only their types differ.
    #[test]
    fn both_coalesce_arities() {
        assert_eq!(
            sql_of(&watermarks::table.select(coalesce(watermarks::title, "x"))),
            r#"SELECT coalesce("watermarks"."title", ?) FROM "watermarks" -- binds: ["x"]"#
        );
        assert_eq!(
            sql_of(&watermarks::table.select(coalesce_opt(watermarks::title, watermarks::title))),
            r#"SELECT coalesce("watermarks"."title", "watermarks"."title") FROM "watermarks" -- binds: []"#
        );
    }

    /// The bitmask binds, and the operand grouping survives the comparison
    /// it is wrapped in — `(kind & ?) != ?`, not `kind & (? != ?)`.
    #[test]
    fn bitwise_and_binds_its_mask() {
        assert_eq!(
            sql_of(
                &watermarks::table
                    .select(watermarks::chat)
                    .filter(bit_and(watermarks::kind, 4_i32).ne(0))
            ),
            r#"SELECT "watermarks"."chat" FROM "watermarks" WHERE (("watermarks"."kind" & ?) != ?) -- binds: [4, 0]"#
        );
    }

    /// The shape they exist for: an upsert whose `DO UPDATE` merges the
    /// stored row with the incoming one instead of overwriting it.
    #[test]
    fn merging_upsert() {
        let q = crate::insert_into(watermarks::table)
            .values((
                watermarks::chat.eq("c"),
                watermarks::oldest_seen.eq(1_i64),
                watermarks::newest_seen.eq(2_i64),
            ))
            .on_conflict(watermarks::chat)
            .do_update()
            .set((
                watermarks::oldest_seen.eq(min2(
                    watermarks::oldest_seen,
                    excluded(watermarks::oldest_seen),
                )),
                watermarks::newest_seen.eq(max2(
                    watermarks::newest_seen,
                    excluded(watermarks::newest_seen),
                )),
                watermarks::title.eq(coalesce_opt(excluded(watermarks::title), watermarks::title)),
            ));
        assert_eq!(
            sql_of(&q),
            concat!(
                r#"INSERT INTO "watermarks" ("chat", "oldest_seen", "newest_seen") VALUES (?, ?, ?) "#,
                r#"ON CONFLICT ("chat") DO UPDATE SET "#,
                r#""oldest_seen" = min("watermarks"."oldest_seen", excluded."oldest_seen"), "#,
                r#""newest_seen" = max("watermarks"."newest_seen", excluded."newest_seen"), "#,
                r#""title" = coalesce(excluded."title", "watermarks"."title") "#,
                r#"-- binds: ["c", 1, 2]"#
            )
        );
    }

    /// `exists_over` over an alias, which is the correlated shape the app
    /// uses it in.
    #[test]
    fn exists_over_an_alias() {
        let w2 = crate::alias!(watermarks as w2);
        let q = watermarks::table
            .select(watermarks::chat)
            .filter(exists_over(
                w2.filter(w2.field(watermarks::chat).eq(watermarks::chat)),
            ));
        assert_eq!(
            sql_of(&q),
            concat!(
                r#"SELECT "watermarks"."chat" FROM "watermarks" WHERE EXISTS "#,
                r#"(SELECT ? FROM "watermarks" AS "w2" WHERE ("w2"."chat" = "watermarks"."chat")) "#,
                r#"-- binds: [1]"#
            )
        );
    }
}
