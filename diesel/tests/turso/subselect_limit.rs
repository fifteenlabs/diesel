//! A `LIMIT` inside a subselect is written into the SQL text, not bound.
//!
//! `.single_value()` is `Grouped(Subselect::new(self.limit(1)))` — the `1` is
//! a bind like any other. Turso's planner replaces the `LIMIT` expression of a
//! subquery it reads as a scalar with the literal `1` before emitting anything
//! (`core/translate/subquery.rs`), so the placeholder that expression held is
//! never compiled into the program and never gets a parameter slot. Diesel
//! counts placeholders by walking the AST and sends a value for that one
//! regardless.
//!
//! Whether that shows depends on nothing but where the dropped placeholder
//! sat, which is why this file tests both orders:
//!
//! - with a bind after it, the value lands in a slot that exists, and the
//!   only thing lost is the limit — which Turso had rewritten to 1 anyway, so
//!   the query answers correctly and the defect stays invisible;
//! - with the subselect last, there is no slot, and the statement fails with
//!   `bind index N is out of bounds`.
//!
//! See `diesel::turso::backend::LiteralSubselectLimit`.

use anyhow::Result;
use diesel::async_dsl::RunQueryDsl;
use diesel::connection::{AsyncConnection, SimpleAsyncConnection};
use diesel::prelude::*;
use diesel::turso::Turso;
use diesel::turso::TursoConnection;

diesel::table! {
    threads (id) {
        id -> BigInt,
        recipient -> Text,
        group_key -> Nullable<Text>,
    }
}

diesel::table! {
    pins (id) {
        id -> BigInt,
        thread_id -> BigInt,
        tag -> Text,
    }
}

diesel::allow_tables_to_appear_in_same_query!(threads, pins);

async fn seed() -> Result<TursoConnection> {
    let mut conn = TursoConnection::establish(":memory:").await?;
    conn.batch_execute(
        "CREATE TABLE threads(
             id INTEGER PRIMARY KEY,
             recipient TEXT NOT NULL UNIQUE,
             group_key TEXT UNIQUE
         ) STRICT;
         CREATE TABLE pins(
             id INTEGER PRIMARY KEY,
             thread_id INTEGER NOT NULL,
             tag TEXT NOT NULL
         ) STRICT;
         INSERT INTO threads VALUES (1,'r1',NULL),(2,'r2',NULL);
         INSERT INTO pins VALUES (1,1,'t'),(2,1,'other'),(3,2,'t');",
    )
    .await?;
    Ok(conn)
}

/// The subquery `.single_value()` builds: two binds of its own, so the
/// statement around it has placeholders on both sides of the `LIMIT`.
fn thread_id() -> diesel::dsl::AssumeNotNull<
    diesel::dsl::SingleValue<
        diesel::dsl::Select<
            diesel::dsl::Filter<
                threads::table,
                diesel::dsl::Or<
                    diesel::dsl::Eq<threads::recipient, &'static str>,
                    diesel::dsl::Eq<threads::group_key, Option<&'static str>>,
                >,
            >,
            threads::id,
        >,
    >,
> {
    threads::table
        .filter(
            threads::recipient
                .eq("r1")
                .or(threads::group_key.eq(None::<&str>)),
        )
        .select(threads::id)
        .single_value()
        .assume_not_null()
}

#[test]
fn subselect_limit_renders_as_a_literal() {
    let query = pins::table
        .filter(pins::thread_id.eq(thread_id()))
        .select(pins::id);
    let sql = diesel::debug_query::<Turso, _>(&query).to_string();
    assert!(
        sql.contains("LIMIT 1)"),
        "subselect LIMIT should be a literal, got: {sql}"
    );
    assert!(
        sql.ends_with(r#"-- binds: ["r1", None]"#),
        "the limit should not be bound, got: {sql}"
    );
}

/// A top-level `LIMIT` keeps its bind: one text serves every page, which is
/// what keeps a paged query in the statement cache.
#[test]
fn top_level_limit_still_binds() {
    let query = pins::table.select(pins::id).limit(50).offset(100);
    let sql = diesel::debug_query::<Turso, _>(&query).to_string();
    assert!(
        sql.contains("LIMIT ? OFFSET ?"),
        "top-level LIMIT should stay bound, got: {sql}"
    );
}

#[tokio::test(flavor = "current_thread")]
async fn scalar_subselect_with_binds_on_both_sides() -> Result<()> {
    let mut conn = seed().await?;
    let got: Vec<i64> = pins::table
        .filter(pins::thread_id.eq(thread_id()))
        .filter(pins::tag.eq("t"))
        .select(pins::id)
        .load(&mut conn)
        .await?;
    assert_eq!(got, vec![1]);
    Ok(())
}

/// The order that fails without the fix: nothing binds after the subselect,
/// so its discarded `LIMIT` placeholder is the last index in the statement
/// and there is no slot for the value diesel sends.
#[tokio::test(flavor = "current_thread")]
async fn scalar_subselect_as_the_last_bind() -> Result<()> {
    let mut conn = seed().await?;
    let got: Vec<i64> = pins::table
        .filter(pins::tag.eq("t"))
        .filter(pins::thread_id.eq(thread_id()))
        .select(pins::id)
        .order(pins::id.desc())
        .load(&mut conn)
        .await?;
    assert_eq!(got, vec![1]);
    Ok(())
}

/// The shape the app hit: a scalar subselect and a `GROUP BY`.
#[tokio::test(flavor = "current_thread")]
async fn scalar_subselect_with_group_by() -> Result<()> {
    let mut conn = seed().await?;
    let got: Vec<(String, i64)> = pins::table
        .filter(pins::thread_id.eq(thread_id()))
        .group_by(pins::tag)
        .select((pins::tag, diesel::dsl::count_star()))
        .order(pins::tag.asc())
        .load(&mut conn)
        .await?;
    assert_eq!(
        got,
        vec![("other".to_string(), 1), ("t".to_string(), 1)],
        "one row per tag in thread 1"
    );
    Ok(())
}

/// A `LIMIT` in an `IN (SELECT …)` is not one Turso rewrites, so the literal
/// has to carry the value the caller asked for rather than a fixed 1.
#[tokio::test(flavor = "current_thread")]
async fn in_subselect_keeps_its_own_limit() -> Result<()> {
    let mut conn = seed().await?;
    let got: Vec<i64> = pins::table
        .filter(
            pins::thread_id.eq_any(
                threads::table
                    .select(threads::id)
                    .order(threads::id.asc())
                    .limit(1),
            ),
        )
        .select(pins::id)
        .order(pins::id.asc())
        .load(&mut conn)
        .await?;
    assert_eq!(got, vec![1, 2], "only thread 1 is in the limited subquery");
    Ok(())
}

/// The subselect mark is scoped to the subselect: a statement with both an
/// inner and an outer limit binds the outer one and writes the inner one.
#[tokio::test(flavor = "current_thread")]
async fn outer_limit_binds_while_inner_limit_is_literal() -> Result<()> {
    let mut conn = seed().await?;
    let query = pins::table
        .filter(pins::thread_id.eq(thread_id()))
        .select(pins::id)
        .order(pins::id.asc())
        .limit(1);
    let sql = diesel::debug_query::<Turso, _>(&query).to_string();
    assert!(sql.contains("LIMIT 1)"), "inner limit is a literal: {sql}");
    assert!(
        sql.contains(r#"ORDER BY "pins"."id" ASC LIMIT ?"#),
        "outer limit is bound: {sql}"
    );
    let got: Vec<i64> = query.load(&mut conn).await?;
    assert_eq!(got, vec![1]);
    Ok(())
}

/// Writing the limit into the text makes the statement's SQL a function of a
/// runtime value, and the fragment says so by calling
/// `unsafe_to_cache_prepared`. This is what that costs and what it does not.
///
/// The cost has to be bounded, because Turso's own statement cache never
/// evicts: if every distinct inner limit were admitted, a call site that
/// varied its limit would grow that cache without end. The `unsafe` verdict
/// is what stops that — but on its own it would also keep out
/// `.single_value()`, whose inner limit is the constant `1` and which is
/// nearly every subselect anyone writes. The connection's admission policy is
/// what separates the two: a vetoed statement is admitted on its *second*
/// sighting of the same text, so a constant limit gets in and a varying one
/// never produces the same text twice.
///
/// Neither half is visible from the answers, which is the reason to assert on
/// the counters here. A statement that recompiles on every execution returns
/// exactly the right rows.
#[tokio::test(flavor = "current_thread")]
async fn a_constant_inner_limit_is_cached_and_a_varying_one_is_not() -> Result<()> {
    let mut conn = seed().await?;

    // `.single_value()` — one text, however often it runs.
    let before = conn.statement_cache_stats();
    for _ in 0..4 {
        let _: Vec<i64> = threads::table
            .filter(
                threads::id
                    .nullable()
                    .eq(pins::table.select(pins::thread_id).single_value()),
            )
            .select(threads::id)
            .load(&mut conn)
            .await?;
    }
    let after_constant = conn.statement_cache_stats();
    assert_eq!(
        after_constant.admitted - before.admitted,
        1,
        "the constant `1` renders one text, so the second sighting admits it \
         despite diesel's veto"
    );
    assert!(
        after_constant.hits > before.hits,
        "and the runs after that are served from the compiled program"
    );

    // A limit that comes from runtime data mints a text per value.
    //
    // Note it has to be an `eq_any` subquery rather than another
    // `.single_value()`: `single_value()` *is* `.limit(1)`, and it appends
    // that limit on top of whatever the inner query already had, so
    // `.limit(n).single_value()` renders `LIMIT 1` for every `n` and would
    // have made this half of the test assert nothing at all.
    let before_varying = conn.statement_cache_stats();
    for limit in 1..=4i64 {
        let _: Vec<i64> = pins::table
            .filter(
                pins::thread_id.eq_any(
                    threads::table
                        .select(threads::id)
                        .order(threads::id.asc())
                        .limit(limit),
                ),
            )
            .select(pins::id)
            .load(&mut conn)
            .await?;
    }
    let after_varying = conn.statement_cache_stats();
    assert_eq!(
        after_varying.admitted - before_varying.admitted,
        0,
        "four different limits are four different SQL texts, and none of them \
         is ever seen twice — which is exactly the growth the veto prevents"
    );
    assert_eq!(
        after_varying.uncached - before_varying.uncached,
        4,
        "each one compiled one-shot, and the counter is the only place that \
         shows it"
    );
    Ok(())
}
