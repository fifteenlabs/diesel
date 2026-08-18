//! The expressions in [`diesel::turso::expr`], run against a real Turso.
//!
//! Every one of these existed only as a render test before: the module's own
//! `#[cfg(test)]` block asserts the SQL text each function produces, and
//! nothing asserted that Turso would accept or agree with it. That is a
//! meaningful gap for this particular module, because its whole reason to
//! exist is that these are the expressions diesel has no node for — so the
//! spelling was chosen from SQLite's documentation rather than from anything
//! diesel had already validated. `bit_and` renders ` & `, which is not
//! portable spelling and which nothing had ever asked Turso to parse.
//!
//! These functions are also the fix for a requirement the consuming app has:
//! the four `define_sql_function!` declarations it kept in
//! `presage-store-diesel` are exactly `max2`/`min2`/`coalesce_opt`, and they
//! can only be deleted in favour of these if these are known to work.
//!
//! The tests therefore assert *results*, not SQL. Where a function has a
//! NULL-handling rule that the type signature encodes (scalar `min` returning
//! NULL if either argument is NULL, unlike the aggregate), the rule is
//! exercised rather than described.

use anyhow::Result;
use diesel::async_dsl::RunQueryDsl;
use diesel::connection::{AsyncConnection, SimpleAsyncConnection};
use diesel::prelude::*;
use diesel::turso::expr::{bit_and, coalesce, coalesce_opt, exists_over, max2, min2};
use diesel::turso::TursoConnection;

diesel::table! {
    readings (id) {
        id -> BigInt,
        label -> Nullable<Text>,
        seen_at -> BigInt,
        oldest -> Nullable<BigInt>,
        flags -> Integer,
    }
}

diesel::table! {
    owners (id) {
        id -> BigInt,
        reading_id -> BigInt,
    }
}

diesel::allow_tables_to_appear_in_same_query!(readings, owners);

/// Bit positions in `readings.flags`, so the mask tests read as something
/// other than magic numbers.
const HAS_AUDIO: i32 = 0b0001;
const HAS_VIDEO: i32 = 0b0010;

async fn seeded() -> Result<TursoConnection> {
    let mut conn = TursoConnection::establish(":memory:").await?;
    conn.batch_execute(
        "CREATE TABLE readings (
             id INTEGER PRIMARY KEY,
             label TEXT,
             seen_at INTEGER NOT NULL,
             oldest INTEGER,
             flags INTEGER NOT NULL
         ) STRICT;
         CREATE TABLE owners (
             id INTEGER PRIMARY KEY,
             reading_id INTEGER NOT NULL
         ) STRICT;
         INSERT INTO readings VALUES
             (1, 'first',  100, 50,   3),
             (2, NULL,     200, NULL, 1),
             (3, 'third',  300, 250,  0);
         INSERT INTO owners VALUES (10, 1);",
    )
    .await?;
    Ok(conn)
}

/// `max2` is the two-argument *scalar* `max`, which is a different function
/// from the aggregate diesel already has under that name.
///
/// The reason it matters is the monotonic write: `SET seen_at = max2(seen_at,
/// ?)` is how a column is kept from going backwards without reading the row
/// first and racing whoever else is writing it. If Turso dispatched `max` on
/// name rather than on argument count, this would silently be an aggregate
/// over one column and the update would write the table's maximum into every
/// row.
#[tokio::test(flavor = "current_thread")]
async fn max2_is_the_scalar_max_and_never_lets_a_column_go_backwards() -> Result<()> {
    let mut conn = seeded().await?;

    // An arriving value older than what is stored changes nothing.
    diesel::update(readings::table.filter(readings::id.eq(3i64)))
        .set(readings::seen_at.eq(max2(readings::seen_at, 250i64)))
        .execute(&mut conn)
        .await?;
    // An arriving value newer than what is stored wins.
    diesel::update(readings::table.filter(readings::id.eq(1i64)))
        .set(readings::seen_at.eq(max2(readings::seen_at, 900i64)))
        .execute(&mut conn)
        .await?;

    let seen: Vec<i64> = readings::table
        .order(readings::id.asc())
        .select(readings::seen_at)
        .load(&mut conn)
        .await?;
    assert_eq!(
        seen,
        vec![900, 200, 300],
        "row 3 kept its larger stored value; row 1 took the larger arriving one"
    );
    Ok(())
}

/// `min2`, and the NULL rule its signature is there to hold callers to.
///
/// SQLite's scalar `min` returns NULL if *either* argument is NULL, where the
/// aggregate of the same name skips NULLs. A caller who reasons from the
/// aggregate's behaviour writes `min2(col, ?)` over a nullable column and
/// silently nulls the column out on every row that had no value yet.
#[tokio::test(flavor = "current_thread")]
async fn min2_returns_null_when_either_side_is_null() -> Result<()> {
    let mut conn = seeded().await?;

    let got: Vec<Option<i64>> = readings::table
        .order(readings::id.asc())
        .select(min2(
            readings::oldest,
            100i64.into_sql::<diesel::sql_types::BigInt>().nullable(),
        ))
        .load(&mut conn)
        .await?;
    assert_eq!(
        got,
        vec![Some(50), None, Some(100)],
        "row 2 has a NULL `oldest`, and scalar min propagates that rather \
         than skipping it the way the aggregate would"
    );
    Ok(())
}

/// `coalesce` in the arity whose fallback cannot be NULL, so the result
/// cannot be either — which is how a `Nullable` column reaches a place that
/// needs a value.
#[tokio::test(flavor = "current_thread")]
async fn coalesce_makes_a_nullable_column_total() -> Result<()> {
    let mut conn = seeded().await?;

    let labels: Vec<String> = readings::table
        .order(readings::id.asc())
        .select(coalesce(readings::label, "unknown"))
        .load(&mut conn)
        .await?;
    assert_eq!(
        labels,
        vec![
            "first".to_string(),
            "unknown".to_string(),
            "third".to_string()
        ],
        "the type says the result is non-null, and it is"
    );
    Ok(())
}

/// `coalesce_opt` — the arity an upsert wants, where both sides may be NULL.
///
/// This is the "don't clobber what we know with the NULL that means we don't
/// know" shape: an incoming row whose label is NULL because the roster has
/// not synced that contact yet must not overwrite a label already learned.
#[tokio::test(flavor = "current_thread")]
async fn coalesce_opt_keeps_a_known_value_when_the_incoming_one_is_null() -> Result<()> {
    let mut conn = seeded().await?;

    diesel::insert_into(readings::table)
        .values((
            readings::id.eq(1i64),
            readings::label.eq(None::<String>),
            readings::seen_at.eq(100i64),
            readings::flags.eq(0),
        ))
        .on_conflict(readings::id)
        .do_update()
        .set(readings::label.eq(coalesce_opt(
            diesel::upsert::excluded(readings::label),
            readings::label,
        )))
        .execute(&mut conn)
        .await?;

    let label: Option<String> = readings::table
        .filter(readings::id.eq(1i64))
        .select(readings::label)
        .first(&mut conn)
        .await?;
    assert_eq!(
        label,
        Some("first".to_string()),
        "the NULL arriving in `excluded` fell through to the stored value"
    );
    Ok(())
}

/// Bitwise `&`, which is the one thing in this module that is an operator
/// rather than a function, and the one whose spelling is not portable.
///
/// The point of having it as an expression rather than a `dsl::sql`
/// fragment is the right-hand side: written this way the mask *binds*, so a
/// bitset test is one cached statement however many masks it is asked about.
/// A `SqlLiteral` is reported unsafe to cache and takes the whole statement
/// around it out of the cache with it.
#[tokio::test(flavor = "current_thread")]
async fn bit_and_masks_and_turso_parses_the_operator() -> Result<()> {
    let mut conn = seeded().await?;

    let with_audio: Vec<i64> = readings::table
        .filter(bit_and(readings::flags, HAS_AUDIO).ne(0))
        .order(readings::id.asc())
        .select(readings::id)
        .load(&mut conn)
        .await?;
    assert_eq!(with_audio, vec![1, 2], "rows with flags 3 and 1");

    let with_video: Vec<i64> = readings::table
        .filter(bit_and(readings::flags, HAS_VIDEO).ne(0))
        .order(readings::id.asc())
        .select(readings::id)
        .load(&mut conn)
        .await?;
    assert_eq!(with_video, vec![1], "only flags 3 has the second bit");

    let stats_before = conn.statement_cache_stats();
    for mask in [HAS_AUDIO, HAS_VIDEO, HAS_AUDIO | HAS_VIDEO] {
        let _: Vec<i64> = readings::table
            .filter(bit_and(readings::flags, mask).ne(0))
            .select(readings::id)
            .load(&mut conn)
            .await?;
    }
    let stats_after = conn.statement_cache_stats();
    assert_eq!(
        stats_after.admitted - stats_before.admitted,
        1,
        "three masks are one statement, because the mask is a bind — that is \
         the whole reason this is an expression and not a `dsl::sql` fragment"
    );
    Ok(())
}

/// `exists_over`, which is sugar rather than a missing node: it renders
/// exactly what `exists(q.select(1.into_sql::<Integer>()))` renders. The test
/// is that the sugar and the long spelling agree, both in SQL and in answer.
#[tokio::test(flavor = "current_thread")]
async fn exists_over_is_the_long_spelling() -> Result<()> {
    use diesel::sql_types::Integer;

    let mut conn = seeded().await?;

    let sugar = readings::table
        .filter(exists_over(
            owners::table.filter(owners::reading_id.eq(readings::id)),
        ))
        .order(readings::id.asc())
        .select(readings::id);
    let long = readings::table
        .filter(diesel::dsl::exists(
            owners::table
                .filter(owners::reading_id.eq(readings::id))
                .select(1.into_sql::<Integer>()),
        ))
        .order(readings::id.asc())
        .select(readings::id);

    assert_eq!(
        diesel::debug_query::<diesel::turso::Turso, _>(&sugar).to_string(),
        diesel::debug_query::<diesel::turso::Turso, _>(&long).to_string(),
        "the sugar has to render identically or it is a second node, not sugar"
    );

    let ids: Vec<i64> = sugar.load(&mut conn).await?;
    assert_eq!(ids, vec![1], "only reading 1 has an owner");
    Ok(())
}
