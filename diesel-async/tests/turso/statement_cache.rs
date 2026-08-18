//! SQL is compiled through Turso's statement cache, and doing so changes
//! nothing a caller can observe.
//!
//! Two things have to hold, and only one of them used to be tested. The
//! obvious one is that a cached program keeps answering for its own binds —
//! each test runs its query several times with different values, and after the
//! schema underneath it has moved, which is the one way a cached program could
//! go stale. The other is that the query took the cached path *at all*, and
//! nothing a caller can see distinguishes that: an uncached statement returns
//! exactly the right rows, having recompiled its program to get them. That is
//! how `cached_update_rebinds` passed for the whole time no `UPDATE` in this
//! app was ever cached. So every test here also asserts on
//! [`TursoConnection::statement_cache_stats`], and would fail if the statement
//! it runs stopped being admitted.

use anyhow::Result;
use diesel::connection::CacheSize;
use diesel::prelude::*;
use diesel_async::{AsyncConnection, RunQueryDsl, SimpleAsyncConnection};
use diesel_async::turso::TursoConnection;

diesel::table! {
    t (id) {
        id -> Integer,
        name -> Text,
    }
}

diesel::table! {
    wide (id) {
        id -> Integer,
        name -> Text,
        note -> Nullable<Text>,
    }
}

diesel::table! {
    counters (key) {
        key -> Text,
        hits -> BigInt,
        seen_at -> BigInt,
    }
}

async fn seeded() -> Result<TursoConnection> {
    let mut conn = TursoConnection::establish(":memory:").await?;
    conn.batch_execute(
        "CREATE TABLE t (id INTEGER PRIMARY KEY, name TEXT NOT NULL) STRICT;
         INSERT INTO t VALUES (1, 'a'), (2, 'b'), (3, 'c');",
    )
    .await?;
    Ok(conn)
}

/// The same query, bound differently each time, keeps answering for its own
/// binds once it is served from the cached program.
#[tokio::test(flavor = "current_thread")]
async fn cached_query_rebinds() -> Result<()> {
    let mut conn = seeded().await?;

    for (id, name) in [(1, "a"), (2, "b"), (3, "c"), (1, "a"), (3, "c")] {
        let rows: Vec<(i32, String)> = t::table
            .filter(t::id.eq(id))
            .select((t::id, t::name))
            .load(&mut conn)
            .await?;
        assert_eq!(rows, vec![(id, name.to_string())]);
    }
    Ok(())
}

/// Writes go through the same compile step as reads, so a cached INSERT has
/// to keep inserting the row it was handed rather than the one it was
/// compiled with.
#[tokio::test(flavor = "current_thread")]
async fn cached_insert_rebinds() -> Result<()> {
    let mut conn = seeded().await?;

    for id in 4..=8 {
        let affected = diesel::insert_into(t::table)
            .values((t::id.eq(id), t::name.eq(format!("row-{id}"))))
            .execute(&mut conn)
            .await?;
        assert_eq!(affected, 1);
    }

    let rows: Vec<(i32, String)> = t::table
        .filter(t::id.ge(4))
        .order(t::id.asc())
        .select((t::id, t::name))
        .load(&mut conn)
        .await?;
    assert_eq!(
        rows.iter().map(|(id, _)| *id).collect::<Vec<_>>(),
        vec![4, 5, 6, 7, 8]
    );
    assert_eq!(rows[0].1, "row-4");
    assert_eq!(rows[4].1, "row-8");
    Ok(())
}

/// An `UPDATE` with a fixed `SET` tuple, bound to different values each time.
///
/// Diesel reports every `UPDATE` unsafe to cache, so this is the statement
/// class the admission policy overrides on evidence of recurrence. The
/// assertions on `hits` are the point of the test: without them it passed
/// while no `UPDATE` in the app reached Turso's cache at all.
#[tokio::test(flavor = "current_thread")]
async fn cached_update_rebinds() -> Result<()> {
    let mut conn = seeded().await?;
    let before = conn.statement_cache_stats();

    for id in 1..=3 {
        let affected = diesel::update(t::table.filter(t::id.eq(id)))
            .set(t::name.eq(format!("updated-{id}")))
            .execute(&mut conn)
            .await?;
        assert_eq!(affected, 1);
    }

    let after = conn.statement_cache_stats();
    assert_eq!(
        after.admitted - before.admitted,
        1,
        "the UPDATE earned exactly one slot, on its second run"
    );
    assert_eq!(
        after.hits - before.hits,
        1,
        "and the third run was served from it"
    );
    assert_eq!(
        after.variadic_families, 0,
        "a fixed SET tuple is not a variadic call site"
    );

    let names: Vec<String> = t::table
        .order(t::id.asc())
        .select(t::name)
        .load(&mut conn)
        .await?;
    assert_eq!(names, vec!["updated-1", "updated-2", "updated-3"]);
    Ok(())
}

/// `ON CONFLICT … DO UPDATE`, likewise vetoed by diesel and likewise admitted
/// on recurrence — and it has to be, despite rendering `VALUES (?, ?, ?)`,
/// which is indistinguishable from a variadic list by looking at one text.
#[tokio::test(flavor = "current_thread")]
async fn cached_upsert_rebinds() -> Result<()> {
    let mut conn = TursoConnection::establish(":memory:").await?;
    conn.batch_execute(
        "CREATE TABLE counters (
             key TEXT PRIMARY KEY,
             hits INTEGER NOT NULL,
             seen_at INTEGER NOT NULL
         ) STRICT;",
    )
    .await?;

    // Two keys, three rounds: every round after the first takes the DO UPDATE
    // branch, so the cached program has to keep both branches.
    for round in 1..=3i64 {
        for key in ["a", "b"] {
            diesel::insert_into(counters::table)
                .values((
                    counters::key.eq(key),
                    counters::hits.eq(1),
                    counters::seen_at.eq(round),
                ))
                .on_conflict(counters::key)
                .do_update()
                .set((
                    counters::hits.eq(counters::hits + 1),
                    counters::seen_at.eq(round),
                ))
                .execute(&mut conn)
                .await?;
        }
    }

    let stats = conn.statement_cache_stats();
    assert_eq!(stats.admitted, 1, "one upsert, one slot");
    assert_eq!(
        stats.hits, 4,
        "six executions: one on probation, one admitting, four served"
    );
    assert_eq!(
        stats.variadic_families, 0,
        "the VALUES tuple never varied, so the call site is not variadic"
    );

    let rows: Vec<(String, i64, i64)> = counters::table
        .order(counters::key.asc())
        .select((counters::key, counters::hits, counters::seen_at))
        .load(&mut conn)
        .await?;
    assert_eq!(
        rows,
        vec![
            ("a".to_string(), 3, 3),
            ("b".to_string(), 3, 3),
        ],
        "each key inserted once and updated twice, with the latest round"
    );
    Ok(())
}

/// A cached program is compiled against the schema of its day. After a
/// migration adds a column, the same query must not be replayed from the
/// program that predates it.
#[tokio::test(flavor = "current_thread")]
async fn cached_query_survives_a_schema_change() -> Result<()> {
    let mut conn = TursoConnection::establish(":memory:").await?;
    conn.batch_execute(
        "CREATE TABLE wide (id INTEGER PRIMARY KEY, name TEXT NOT NULL);
         INSERT INTO wide (id, name) VALUES (1, 'a'), (2, 'b'), (3, 'c');",
    )
    .await?;

    // Cached, against the two-column table.
    for _ in 0..3 {
        let rows: Vec<(i32, String)> = wide::table
            .order(wide::id.asc())
            .select((wide::id, wide::name))
            .load(&mut conn)
            .await?;
        assert_eq!(rows.len(), 3);
    }

    conn.batch_execute("ALTER TABLE wide ADD COLUMN note TEXT;")
        .await?;
    diesel::update(wide::table.filter(wide::id.eq(2)))
        .set(wide::note.eq("n"))
        .execute(&mut conn)
        .await?;

    // The same query, now over a wider table.
    let rows: Vec<(i32, String)> = wide::table
        .order(wide::id.asc())
        .select((wide::id, wide::name))
        .load(&mut conn)
        .await?;
    assert_eq!(rows.len(), 3);
    assert_eq!(rows[1].1, "b");

    let notes: Vec<Option<String>> = wide::table
        .order(wide::id.asc())
        .select(wide::note)
        .load(&mut conn)
        .await?;
    assert_eq!(notes, vec![None, Some("n".to_string()), None]);
    Ok(())
}

/// `eq_any` over a runtime list emits a placeholder per element, so each
/// length is its own SQL text. It still has to answer for the list it was
/// given — every length of it — and, more importantly, the lengths must not
/// each take a slot in a cache that never evicts.
///
/// This is the case recurrence alone could not have handled: a three-element
/// list recurs perfectly well. What closes the call site is the second
/// *distinct* length arriving under the same family key.
#[tokio::test(flavor = "current_thread")]
async fn an_in_list_does_not_fill_the_cache_with_its_lengths() -> Result<()> {
    let mut conn = seeded().await?;

    // Every length, many times over, in an order that repeats each one.
    for _ in 0..10 {
        for ids in [vec![1], vec![1, 2], vec![2, 3], vec![1, 2, 3], vec![3]] {
            let rows: Vec<i32> = t::table
                .filter(t::id.eq_any(ids.clone()))
                .order(t::id.asc())
                .select(t::id)
                .load(&mut conn)
                .await?;
            assert_eq!(rows, ids, "the list it was handed, not the one before");
        }
    }

    let stats = conn.statement_cache_stats();
    assert_eq!(
        stats.variadic_families, 1,
        "all the lengths were recognised as one call site, not one each"
    );
    assert!(
        stats.admitted <= 1,
        "at most the single length that recurred before the call site was \
         recognised — never a slot per length (got {})",
        stats.admitted
    );
    assert_eq!(
        stats.refused_at_cap, 0,
        "and nothing was pushed out of the cache to make room for them"
    );
    Ok(())
}

/// A statement whose text is built from runtime values is a fresh text every
/// call. It never recurs, so it never gets in — the half of the rule that
/// `eq_any` does not exercise, and the one that keeps a `format!`-built
/// `dsl::sql` fragment out while a static one goes in.
#[tokio::test(flavor = "current_thread")]
async fn a_freshly_built_text_never_gets_in_but_a_static_one_does() -> Result<()> {
    use diesel::sql_types::Bool;

    let mut conn = seeded().await?;

    // Static fragment text: diesel vetoes it (it is a `SqlLiteral`, and
    // diesel cannot know it wasn't interpolated), but it recurs, so it is
    // admitted like any other bounded statement.
    for _ in 0..4 {
        let rows: Vec<i32> = t::table
            .filter(diesel::dsl::sql::<diesel::sql_types::Bool>("id % 2 = 1"))
            .order(t::id.asc())
            .select(t::id)
            .load(&mut conn)
            .await?;
        assert_eq!(rows, vec![1, 3]);
    }
    let after_static = conn.statement_cache_stats();
    assert_eq!(
        after_static.admitted, 1,
        "a static fragment makes one text, so it is cacheable"
    );
    assert_eq!(after_static.hits, 2);

    // Interpolated fragment text: the same predicate every time, spelled a
    // different way every time, which is what a `format!`-built fragment does
    // in an app. No text ever recurs, so none is ever admitted — and the cache
    // does not grow by one entry per call.
    for n in 0..200 {
        let rows: Vec<i32> = t::table
            .filter(diesel::dsl::sql::<Bool>(&format!(
                "{n} < 1000 AND id % 2 = 1"
            )))
            .order(t::id.asc())
            .select(t::id)
            .load(&mut conn)
            .await?;
        assert_eq!(rows, vec![1, 3]);
    }

    let stats = conn.statement_cache_stats();
    assert_eq!(
        stats.admitted, after_static.admitted,
        "two hundred one-off texts, and not one of them took a slot"
    );
    assert_eq!(
        stats.refused_at_cap, 0,
        "they were turned away by the recurrence rule, not by the cap — the \
         cap never had to absorb them"
    );
    Ok(())
}

/// Turning the cache off leaves the results alone — the only difference it
/// may make is how the statement got compiled.
#[tokio::test(flavor = "current_thread")]
async fn disabled_cache_answers_the_same() -> Result<()> {
    let mut conn = seeded().await?;
    conn.set_prepared_statement_cache_size(CacheSize::Disabled);

    for _ in 0..3 {
        let rows: Vec<(i32, String)> = t::table
            .order(t::id.asc())
            .select((t::id, t::name))
            .load(&mut conn)
            .await?;
        assert_eq!(rows.len(), 3);
        assert_eq!(rows[2].1, "c");
    }
    Ok(())
}
