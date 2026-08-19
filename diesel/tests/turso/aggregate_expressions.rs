//! What diesel's aggregate-function syntax does on Turso, and the one half
//! of it the engine does not have.
//!
//! `SqlDialect::AggregateFunctionExpressions` is a single marker that gates
//! two unrelated SQL features: `FILTER (WHERE …)` after an aggregate, and
//! `ORDER BY` *inside* an aggregate's argument list. Turso has the first and
//! not the second, so the obvious choice — `PostgresLikeAggregate…`, the
//! marker every SQLite-shaped backend takes — was a claim the engine does not
//! honour: `max(a ORDER BY a)` compiled and came back at run time with
//!
//! ```text
//! Parse error: ORDER BY clause is not supported yet in aggregate functions
//! ```
//!
//! This backend therefore selects `FilterOnlyAggregateFunctionExpressions`,
//! which keeps `aggregate_filter` and leaves `aggregate_order` with no
//! `QueryFragment` impl at all — so
//!
//! ```ignore
//! samples::table.select(max(samples::score).aggregate_order(samples::score.asc()))
//! ```
//!
//! no longer builds — it is refused with diesel's own dialect diagnostic,
//! "`Order<…, false>` is no valid SQL fragment for the `Turso` backend …
//! this usually means that the `Turso` database system does not support this
//! SQL syntax", which is what a dialect marker is for.
//!
//! That is not something a runtime test can assert, so the
//! tests here pin the two facts the choice rests on instead: `FILTER` works,
//! and the engine still rejects the `ORDER BY` form. If a future Turso learns
//! it, [`the_engine_still_rejects_order_by_inside_an_aggregate`] fails and
//! says to move the marker back.

use anyhow::Result;
use diesel::async_dsl::RunQueryDsl;
use diesel::connection::{AsyncConnection, SimpleAsyncConnection};
use diesel::dsl::{count, max};
use diesel::prelude::*;
use diesel::turso::TursoConnection;

use crate::sql_text::rendered;

diesel::table! {
    samples (id) {
        id -> BigInt,
        kind -> Text,
        score -> BigInt,
    }
}

/// Three `a`s and two `b`s, with a repeated score so `DISTINCT` has
/// something to remove.
async fn setup() -> Result<TursoConnection> {
    let mut conn = TursoConnection::establish(":memory:").await?;
    conn.batch_execute(
        "CREATE TABLE samples(
             id INTEGER PRIMARY KEY,
             kind TEXT NOT NULL,
             score INTEGER NOT NULL
         ) STRICT;",
    )
    .await?;
    for (id, kind, score) in [
        (1i64, "a", 10i64),
        (2, "a", 20),
        (3, "a", 20),
        (4, "b", 30),
        (5, "b", 40),
    ] {
        diesel::insert_into(samples::table)
            .values((
                samples::id.eq(id),
                samples::kind.eq(kind),
                samples::score.eq(score),
            ))
            .execute(&mut conn)
            .await?;
    }
    Ok(conn)
}

/// The half of the marker Turso does have. This is what the dialect choice is
/// *for*: dropping to `NoAggregateFunctionExpressions` would have taken
/// `FILTER` away too, and `count(*) FILTER (WHERE …)` is the reason anyone
/// reaches for aggregate expressions here.
#[tokio::test(flavor = "current_thread")]
async fn a_filter_clause_renders_and_runs() -> Result<()> {
    let mut conn = setup().await?;

    let query = samples::table.select(count(samples::id).aggregate_filter(samples::kind.eq("a")));
    let sql = rendered(&query);
    assert!(
        sql.contains("FILTER ( WHERE"),
        "the FILTER clause has to render. Rendered:\n  {sql}"
    );

    let counted: i64 = query.get_result(&mut conn).await?;
    assert_eq!(counted, 3, "three rows have kind = 'a'");
    Ok(())
}

/// `DISTINCT` inside an aggregate is not gated by this marker at all, and is
/// here so that a future change to the dialect that took it away would be
/// noticed rather than assumed.
#[tokio::test(flavor = "current_thread")]
async fn aggregate_distinct_still_works() -> Result<()> {
    let mut conn = setup().await?;

    let all: i64 = samples::table
        .select(count(samples::score))
        .get_result(&mut conn)
        .await?;
    let distinct: i64 = samples::table
        .select(count(samples::score).aggregate_distinct())
        .get_result(&mut conn)
        .await?;
    assert_eq!((all, distinct), (5, 4), "20 appears twice");
    Ok(())
}

/// A window function's own `ORDER BY` is a different clause in a different
/// place — `OVER (ORDER BY …)` — and Turso runs it. Pinned because the two
/// are easy to conflate: the marker this backend declines is about the order
/// inside the *argument list*, not this one.
#[tokio::test(flavor = "current_thread")]
async fn a_window_order_by_is_unaffected() -> Result<()> {
    let mut conn = setup().await?;

    let ranked: Vec<i64> = samples::table
        .select(
            diesel::dsl::row_number()
                .over()
                .window_order(samples::score.desc()),
        )
        .order(samples::score.desc())
        .load(&mut conn)
        .await?;
    assert_eq!(ranked, vec![1, 2, 3, 4, 5]);
    Ok(())
}

/// The fact the dialect choice rests on, asserted against the engine rather
/// than taken from a changelog.
///
/// Written as raw SQL because the typed DSL can no longer express it: with
/// `FilterOnlyAggregateFunctionExpressions` selected, `aggregate_order` on a
/// Turso query has no `QueryFragment` impl and does not compile. The SQL below
/// is exactly what it used to render.
///
/// If this test starts failing because the statement *succeeded*, Turso has
/// grown the clause: swap `SqlDialect::AggregateFunctionExpressions` in
/// `src/turso/backend.rs` back to `PostgresLikeAggregateFunctionExpressions`,
/// which restores `aggregate_order`, and delete this test.
#[tokio::test(flavor = "current_thread")]
async fn the_engine_still_rejects_order_by_inside_an_aggregate() -> Result<()> {
    let mut conn = setup().await?;

    // The `FILTER` control, through the same raw path, so a failure below is
    // about `ORDER BY` and not about the statement or the connection.
    diesel::sql_query(r#"SELECT count(*) FILTER (WHERE "kind" = 'a') FROM "samples""#)
        .execute(&mut conn)
        .await?;

    let err = diesel::sql_query(r#"SELECT max("score" ORDER BY "score" ASC) FROM "samples""#)
        .execute(&mut conn)
        .await
        .expect_err(
            "Turso used to reject ORDER BY inside an aggregate; if it no longer does, \
             see this test's doc comment",
        );
    let message = err.to_string();
    assert!(
        message.contains("ORDER BY clause is not supported yet in aggregate functions"),
        "expected Turso's aggregate-ORDER BY parse error, got: {message}"
    );
    Ok(())
}

/// `max(x)` on its own — the function `aggregate_order` would have been
/// attached to — is unaffected, so the dialect choice costs the ordering
/// clause and nothing else.
#[tokio::test(flavor = "current_thread")]
async fn the_bare_aggregate_is_untouched() -> Result<()> {
    let mut conn = setup().await?;

    let highest: Option<i64> = samples::table
        .select(max(samples::score))
        .get_result(&mut conn)
        .await?;
    assert_eq!(highest, Some(40));
    Ok(())
}
