//! `RETURNING` on Turso, which the dialect claims support for in
//! `backend.rs` (`PgLikeReturningClause`).
//!
//! Turso's parser accepts `RETURNING` on INSERT, UPDATE and DELETE, but a
//! parser that accepts a clause is not the same as an engine that fills it
//! in, and the whole point of the clause here is to replace
//! `SELECT last_insert_rowid()` — a read that is only correct because
//! nothing else writes between the insert and it. So each statement kind is
//! exercised against real rows, and the last test pins the one behaviour
//! that would make the clause unsafe to use for a rowid: a statement that
//! returns more than one row must still apply to *every* row, not just the
//! ones the caller bothers to read off the stream.

use anyhow::Result;
use diesel::async_dsl::RunQueryDsl;
use diesel::connection::{AsyncConnection, SimpleAsyncConnection};
use diesel::prelude::*;
use diesel::turso::TursoConnection;

diesel::table! {
    notes(id) {
        id -> Integer,
        body -> Text,
        hits -> BigInt,
    }
}

async fn setup() -> Result<TursoConnection> {
    let mut conn = TursoConnection::establish(":memory:").await?;
    conn.batch_execute(
        "CREATE TABLE notes(id INTEGER PRIMARY KEY, body TEXT NOT NULL, hits BIGINT NOT NULL) STRICT",
    )
    .await?;
    Ok(conn)
}

/// The site this was added for: an INSERT that has to report the rowid the
/// database chose, without a second statement to ask for it.
#[tokio::test(flavor = "current_thread")]
async fn insert_returns_the_generated_rowid() -> Result<()> {
    let mut conn = setup().await?;

    let id: i32 = diesel::insert_into(notes::table)
        .values((notes::body.eq("first"), notes::hits.eq(0i64)))
        .returning(notes::id)
        .get_result(&mut conn)
        .await?;
    assert_eq!(id, 1);

    let id: i32 = diesel::insert_into(notes::table)
        .values((notes::body.eq("second"), notes::hits.eq(0i64)))
        .returning(notes::id)
        .get_result(&mut conn)
        .await?;
    assert_eq!(id, 2);

    // The insert really happened — a RETURNING row is not a dry run.
    let bodies: Vec<String> = notes::table
        .order(notes::id.asc())
        .select(notes::body)
        .load(&mut conn)
        .await?;
    assert_eq!(bodies, vec!["first", "second"]);
    Ok(())
}

/// Several columns at once, including one the statement did not set.
#[tokio::test(flavor = "current_thread")]
async fn insert_can_return_a_tuple() -> Result<()> {
    let mut conn = setup().await?;

    let (id, body, hits): (i32, String, i64) = diesel::insert_into(notes::table)
        .values((notes::body.eq("tuple"), notes::hits.eq(7i64)))
        .returning((notes::id, notes::body, notes::hits))
        .get_result(&mut conn)
        .await?;
    assert_eq!((id, body.as_str(), hits), (1, "tuple", 7));
    Ok(())
}

/// UPDATE returns the *post*-update values, which is what makes it usable as
/// a read-modify-write in one statement.
#[tokio::test(flavor = "current_thread")]
async fn update_returns_the_new_values() -> Result<()> {
    let mut conn = setup().await?;
    diesel::insert_into(notes::table)
        .values((notes::id.eq(1), notes::body.eq("a"), notes::hits.eq(4i64)))
        .execute(&mut conn)
        .await?;

    let hits: i64 = diesel::update(notes::table.filter(notes::id.eq(1)))
        .set(notes::hits.eq(notes::hits + 1))
        .returning(notes::hits)
        .get_result(&mut conn)
        .await?;
    assert_eq!(hits, 5);

    // A filter that matches nothing returns no row rather than erroring.
    let missed: Option<i64> = diesel::update(notes::table.filter(notes::id.eq(99)))
        .set(notes::hits.eq(notes::hits + 1))
        .returning(notes::hits)
        .get_result(&mut conn)
        .await
        .optional()?;
    assert_eq!(missed, None);
    Ok(())
}

/// DELETE returns the row as it was, which is the only chance to read it.
#[tokio::test(flavor = "current_thread")]
async fn delete_returns_the_removed_row() -> Result<()> {
    let mut conn = setup().await?;
    diesel::insert_into(notes::table)
        .values((notes::id.eq(1), notes::body.eq("doomed"), notes::hits.eq(1)))
        .execute(&mut conn)
        .await?;

    let body: String = diesel::delete(notes::table.filter(notes::id.eq(1)))
        .returning(notes::body)
        .get_result(&mut conn)
        .await?;
    assert_eq!(body, "doomed");

    let left: i64 = notes::table.count().get_result(&mut conn).await?;
    assert_eq!(left, 0);
    Ok(())
}

/// The failure mode that would make `RETURNING` a trap.
///
/// A `RETURNING` statement is served through `conn.query()` and read as a
/// stream, and in SQLite the statement advances as rows are stepped — so a
/// multi-row statement whose stream is dropped after the first row could
/// leave the rest of its work undone. `get_result` reads exactly one row and
/// drops the rest, which is precisely that shape.
///
/// Turso does not behave that way: the statement is applied in full. This
/// test is the licence for `get_result` on a single-row insert (the
/// `last_insert_rowid` replacement) — if it ever starts failing, every
/// `.returning(...)` call site has to be re-read as a partial write.
#[tokio::test(flavor = "current_thread")]
async fn a_multi_row_statement_applies_to_every_row_even_if_one_row_is_read() -> Result<()> {
    let mut conn = setup().await?;
    for id in 1..=5 {
        diesel::insert_into(notes::table)
            .values((notes::id.eq(id), notes::body.eq("x"), notes::hits.eq(0i64)))
            .execute(&mut conn)
            .await?;
    }

    // Reads one row off a five-row RETURNING stream and drops the rest.
    let first: i64 = diesel::update(notes::table)
        .set(notes::hits.eq(notes::hits + 1))
        .returning(notes::hits)
        .get_result(&mut conn)
        .await?;
    assert_eq!(first, 1);

    let hits: Vec<i64> = notes::table
        .order(notes::id.asc())
        .select(notes::hits)
        .load(&mut conn)
        .await?;
    assert_eq!(
        hits,
        vec![1, 1, 1, 1, 1],
        "every row must have been updated"
    );

    // And the whole stream reads back, for the callers that want every row.
    let all: Vec<i64> = diesel::update(notes::table)
        .set(notes::hits.eq(notes::hits + 1))
        .returning(notes::hits)
        .load(&mut conn)
        .await?;
    assert_eq!(all, vec![2, 2, 2, 2, 2]);
    Ok(())
}

/// `.returning(…)` followed by `.execute()` — the caller who wants the
/// clause's effect but not its rows — reports the row count and applies the
/// write exactly once.
///
/// This used to be the worst answer a query can give: `Err` *and* done.
/// `execute_returning_count` ran the statement through
/// `turso::Statement::execute`, which steps with `columns: None`, and a step
/// that yields a row in that mode is
/// `Misuse("unexpected row during execution")` — raised after the DML had
/// already taken effect. So the insert happened, the caller was told it had
/// not, and a caller that retries on error (which is the sane reading of
/// "this returned an error") applied it twice.
///
/// Diesel's SQLite and PostgreSQL backends both answer this with a count, so
/// nothing in the calling code marks it as a shape to avoid.
#[tokio::test(flavor = "current_thread")]
async fn returning_execute_reports_a_count_and_applies_once() -> Result<()> {
    let mut conn = setup().await?;

    let affected = diesel::insert_into(notes::table)
        .values((notes::body.eq("only once"), notes::hits.eq(0i64)))
        .returning(notes::id)
        .execute(&mut conn)
        .await?;
    assert_eq!(affected, 1);

    let bodies: Vec<String> = notes::table.select(notes::body).load(&mut conn).await?;
    assert_eq!(bodies, vec!["only once".to_string()]);

    // Multi-row too: the count is the number of rows changed, not the number
    // of rows anybody read off the RETURNING stream.
    for id in 2..=4 {
        diesel::insert_into(notes::table)
            .values((notes::id.eq(id), notes::body.eq("x"), notes::hits.eq(0i64)))
            .execute(&mut conn)
            .await?;
    }
    let affected = diesel::update(notes::table)
        .set(notes::hits.eq(notes::hits + 1))
        .returning(notes::hits)
        .execute(&mut conn)
        .await?;
    assert_eq!(affected, 4);

    let affected = diesel::delete(notes::table)
        .returning(notes::id)
        .execute(&mut conn)
        .await?;
    assert_eq!(affected, 4);
    Ok(())
}

/// The same root cause on a statement with no `RETURNING` in sight: a plain
/// `SELECT` run through `.execute()`, which every other diesel backend
/// answers with a row count of zero rather than an error.
///
/// It reads as a pointless thing to write until you notice it is what
/// `ExecuteDsl` does for any query a caller runs for its side effects and
/// does not want the rows of — and it is what a generic helper over
/// `QueryFragment` ends up emitting.
#[tokio::test(flavor = "current_thread")]
async fn a_select_run_for_its_side_effects_reports_zero_changes() -> Result<()> {
    let mut conn = setup().await?;
    diesel::insert_into(notes::table)
        .values((notes::body.eq("a"), notes::hits.eq(0i64)))
        .execute(&mut conn)
        .await?;

    assert_eq!(notes::table.select(notes::id).execute(&mut conn).await?, 0);
    Ok(())
}
