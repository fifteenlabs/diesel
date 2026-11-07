//! What diesel's aggregate-function syntax does on Turso.
//!
//! `SqlDialect::AggregateFunctionExpressions` is a single marker that gates
//! two unrelated SQL features: `FILTER (WHERE …)` after an aggregate, and
//! `ORDER BY` *inside* an aggregate's argument list. Turso used to have only
//! the first — `max(a ORDER BY a)` came back with "Parse error: ORDER BY
//! clause is not supported yet in aggregate functions" — so this backend
//! selected a FILTER-only marker that made `aggregate_order` a compile error.
//! The engine has since grown the clause, and the backend takes
//! `PostgresLikeAggregateFunctionExpressions` like every other SQLite-shaped
//! backend.
//!
//! The tests pin all three facts that choice rests on: `FILTER` works,
//! `aggregate_order` renders and runs through the typed DSL, and the engine
//! honours the order rather than parsing it and dropping it.

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

/// The `FILTER` half of the marker. Dropping to
/// `NoAggregateFunctionExpressions` would take it away, and
/// `count(*) FILTER (WHERE …)` is the reason anyone reaches for aggregate
/// expressions here.
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
/// are easy to conflate: the marker's `ORDER BY` is the one inside the
/// *argument list*, not this one.
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

/// `aggregate_order` through the typed DSL: it renders the `ORDER BY` inside
/// the argument list and the engine runs it.
///
/// This replaces the test that pinned Turso *rejecting* the clause. That test
/// said what to do when the engine grew it — select
/// `PostgresLikeAggregateFunctionExpressions` again — and this is the other
/// half of that instruction: the DSL form compiles again, so it is asserted
/// end to end rather than as raw SQL.
#[tokio::test(flavor = "current_thread")]
async fn aggregate_order_renders_and_runs() -> Result<()> {
    let mut conn = setup().await?;

    let query = samples::table.select(max(samples::score).aggregate_order(samples::score.asc()));
    let sql = rendered(&query);
    assert!(
        sql.contains("ORDER BY"),
        "the ORDER BY has to render inside the aggregate. Rendered:\n  {sql}"
    );

    let highest: Option<i64> = query.get_result(&mut conn).await?;
    assert_eq!(highest, Some(40));
    Ok(())
}

/// That the engine *honours* the order, not merely parses it. `max` cannot
/// tell — its answer is the same in any order — so this asks an
/// order-sensitive aggregate, through raw SQL because diesel has no typed
/// `group_concat`. If Turso ever parsed the clause and dropped it, the
/// marker would be claiming a capability again, just a quieter one.
#[tokio::test(flavor = "current_thread")]
async fn the_engine_honours_order_by_inside_an_aggregate() -> Result<()> {
    #[derive(QueryableByName)]
    struct Joined {
        #[diesel(sql_type = diesel::sql_types::Text)]
        joined: String,
    }

    let mut conn = setup().await?;

    let descending: Joined = diesel::sql_query(
        r#"SELECT group_concat("id", ',' ORDER BY "score" DESC, "id" DESC) AS joined FROM "samples""#,
    )
    .get_result(&mut conn)
    .await?;
    let ascending: Joined = diesel::sql_query(
        r#"SELECT group_concat("id", ',' ORDER BY "score" ASC, "id" ASC) AS joined FROM "samples""#,
    )
    .get_result(&mut conn)
    .await?;
    assert_eq!(descending.joined, "5,4,3,2,1");
    assert_eq!(ascending.joined, "1,2,3,4,5");
    Ok(())
}

/// `max(x)` on its own — the function `aggregate_order` is attached to above —
/// renders and runs without any aggregate clause at all.
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
