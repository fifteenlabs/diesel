//! `CASE`, `coalesce`, and SQLite's two-argument `max`, against Turso.
//!
//! These exist so a statement that needs one of them can still be written in
//! the typed DSL instead of collapsing into a `sql_query` string. The tests
//! check both halves of that claim: the SQL text (an expression that renders
//! `WHEN` in the wrong order, or forgets its parentheses, is wrong in a way
//! only the text shows) and the answers (one that renders fine but binds its
//! values in the wrong order is wrong in a way only a run shows).
//!
//! `CASE` here is [`diesel::dsl::case_when`], diesel's own. It is tested even
//! though it is not our code, because *this backend* is: `QueryFragment` is
//! implemented per backend, the SQL it emits has to survive Turso's parser,
//! and Turso is not a backend diesel's own suite runs against. `coalesce` and
//! `max2` are ours, from `diesel::turso::expr`, because diesel has neither —
//! its `max` is the one-argument aggregate.

use anyhow::Result;
use diesel::prelude::*;
use diesel::sql_types::{BigInt, Integer};
use diesel_async::{AsyncConnection, RunQueryDsl, SimpleAsyncConnection};
use diesel::dsl::case_when;
use diesel::turso::expr::{coalesce, max2};
use diesel::turso::Turso;
use diesel_async::turso::TursoConnection;

diesel::table! {
    jobs(id) {
        id -> Integer,
        state -> Integer,
        retry_count -> Integer,
        updated_at -> BigInt,
        priority -> BigInt,
        last_error -> Nullable<Text>,
    }
}

async fn seeded() -> Result<TursoConnection> {
    let mut conn = TursoConnection::establish(":memory:").await?;
    conn.batch_execute(
        "CREATE TABLE jobs(
             id INTEGER PRIMARY KEY,
             state INTEGER NOT NULL,
             retry_count INTEGER NOT NULL,
             updated_at BIGINT NOT NULL,
             priority BIGINT NOT NULL,
             last_error TEXT
         ) STRICT;
         INSERT INTO jobs VALUES (1, 0, 0, 0,   5, NULL);
         INSERT INTO jobs VALUES (2, 3, 1, 100, 5, 'length mismatch: 3 != 4');
         INSERT INTO jobs VALUES (3, 3, 2, 100, 5, 'url expired');
         INSERT INTO jobs VALUES (4, 2, 0, 100, 5, NULL);",
    )
    .await?;
    Ok(conn)
}

/// The rendering, spelled out once. Every other test here leans on this
/// being the shape being run.
///
/// Diesel parenthesises each `WHEN`, `THEN` and `ELSE` operand and emits a
/// bare `CASE … END` rather than wrapping the whole expression. It needs no
/// wrapper: `CASE … END` is self-delimiting in the SQL grammar, so it takes
/// its precedence from its own keywords wherever it lands. The doubled
/// parentheses around each `WHEN` are diesel's `Grouped` applied to an
/// operand that is already `Grouped` — noise in the text, not in the parse.
#[test]
fn renders_arms_in_order_with_the_else_last() {
    let expr = case_when::<_, _, Integer>(jobs::state.eq(3), 0)
        .when(jobs::state.eq(4), 1)
        .otherwise(jobs::state);
    let query = jobs::table.select(expr);
    assert_eq!(
        diesel::debug_query::<Turso, _>(&query).to_string(),
        r#"SELECT CASE WHEN (("jobs"."state" = ?)) THEN (?) WHEN (("jobs"."state" = ?)) THEN (?) ELSE ("jobs"."state") END FROM "jobs" -- binds: [3, 0, 4, 1]"#
    );
}

/// A `CASE` in the `WHERE` clause: the retry-backoff ladder, which is the
/// shape the media-download queue claims work with.
#[tokio::test(flavor = "current_thread")]
async fn case_in_a_where_clause_picks_the_matching_arm() -> Result<()> {
    let mut conn = seeded().await?;

    // updated_at + backoff(retry_count) <= now, with the ladder as a CASE.
    let backoff = case_when::<_, _, BigInt>(jobs::retry_count.eq(0), 0i64)
        .when(jobs::retry_count.eq(1), 2_000i64)
        .when(jobs::retry_count.eq(2), 8_000i64)
        .otherwise(600_000i64);

    let now = 5_000i64;
    let ready: Vec<i32> = jobs::table
        .filter(jobs::state.eq(3))
        .filter((jobs::updated_at + backoff).le(now))
        .order(jobs::id.asc())
        .select(jobs::id)
        .load(&mut conn)
        .await?;

    // Job 2 waits 2s from t=100 → ready at 2100. Job 3 waits 8s → 8100,
    // which is past `now`, so only job 2 comes back.
    assert_eq!(ready, vec![2]);
    Ok(())
}

/// A `CASE` on the right of a `SET`, which is how one UPDATE can reset some
/// rows' columns and leave others alone.
#[tokio::test(flavor = "current_thread")]
async fn case_in_a_set_clause_rewrites_only_the_matching_rows() -> Result<()> {
    let mut conn = seeded().await?;

    diesel::update(jobs::table)
        .set((
            jobs::state
                .eq(case_when::<_, _, Integer>(jobs::state.eq_any([3, 4, 5]), 0)
                    .otherwise(jobs::state)),
            jobs::retry_count.eq(case_when::<_, _, Integer>(jobs::state.eq_any([3, 4, 5]), 0)
                .otherwise(jobs::retry_count)),
        ))
        .execute(&mut conn)
        .await?;

    let rows: Vec<(i32, i32, i32)> = jobs::table
        .order(jobs::id.asc())
        .select((jobs::id, jobs::state, jobs::retry_count))
        .load(&mut conn)
        .await?;
    assert_eq!(
        rows,
        vec![
            (1, 0, 0), // was already pending
            (2, 0, 0), // failed → pending, retries cleared
            (3, 0, 0), // failed → pending, retries cleared
            (4, 2, 0), // done → untouched
        ]
    );
    Ok(())
}

/// `SUM(CASE WHEN … THEN 1 ELSE 0 END)`, several times over, in one pass —
/// the monitor-panel shape. The point of the node is that this stays *one*
/// scan instead of one query per bucket.
#[tokio::test(flavor = "current_thread")]
async fn several_case_aggregates_share_one_scan() -> Result<()> {
    use diesel::dsl::{count_star, sum};

    let mut conn = seeded().await?;

    let (total, corrupt, expired): (i64, Option<i64>, Option<i64>) = jobs::table
        .filter(jobs::state.eq(3))
        .select((
            count_star(),
            sum(
                case_when::<_, _, Integer>(jobs::last_error.like("length mismatch%"), 1)
                    .otherwise(0),
            ),
            sum(case_when::<_, _, Integer>(jobs::last_error.like("%expired%"), 1).otherwise(0)),
        ))
        .get_result(&mut conn)
        .await?;

    assert_eq!((total, corrupt, expired), (2, Some(1), Some(1)));
    Ok(())
}

/// `coalesce` in a comparison, where a NULL has to read as a floor rather
/// than swallowing the whole predicate.
#[tokio::test(flavor = "current_thread")]
async fn coalesce_supplies_a_floor_for_null() -> Result<()> {
    let mut conn = seeded().await?;

    let ids: Vec<i32> = jobs::table
        .filter(coalesce(jobs::last_error, "").eq(""))
        .order(jobs::id.asc())
        .select(jobs::id)
        .load(&mut conn)
        .await?;
    // The two rows with no error — a plain `= ''` would have matched none of
    // them, because NULL = '' is NULL.
    assert_eq!(ids, vec![1, 4]);
    Ok(())
}

/// `max(a, b)`, the two-argument scalar — the monotonic write.
#[tokio::test(flavor = "current_thread")]
async fn max2_never_lets_a_column_go_backwards() -> Result<()> {
    let mut conn = seeded().await?;

    diesel::update(jobs::table.filter(jobs::id.eq(1)))
        .set(jobs::priority.eq(max2(jobs::priority, 9i64)))
        .execute(&mut conn)
        .await?;
    diesel::update(jobs::table.filter(jobs::id.eq(2)))
        .set(jobs::priority.eq(max2(jobs::priority, 1i64)))
        .execute(&mut conn)
        .await?;

    let priorities: Vec<i64> = jobs::table
        .filter(jobs::id.le(2))
        .order(jobs::id.asc())
        .select(jobs::priority)
        .load(&mut conn)
        .await?;
    assert_eq!(priorities, vec![9, 5], "raised, then left alone");
    Ok(())
}
