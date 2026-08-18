//! Joins through the DSL.
//!
//! Turso's planner rejects a parenthesized join group in a `FROM` clause
//! (`Parenthesized FROM clause subqueries are not supported`), which is
//! exactly what diesel emits by default:
//!
//! ```sql
//! SELECT … FROM ("books" INNER JOIN "authors" ON ("books"."author_id" = "authors"."id"))
//! ```
//!
//! `Turso::JoinFromClauseSyntax` selects diesel's unparenthesized rendering
//! instead, so every one of these queries is a *runtime* failure without
//! that one line in `backend.rs` — the types compile either way. Hence the
//! coverage below: inferred (`joinable!`) and explicit-`ON` variants of both
//! `inner_join` and `left_join`, plus the shapes that wrap the join in more
//! SQL (WHERE, ORDER BY, aggregates, chained joins), because the parens sit
//! in the middle of the statement and a bad splice shows up as a syntax
//! error further along.

use anyhow::Result;
use diesel::prelude::*;
use diesel_async::{AsyncConnection, RunQueryDsl, SimpleAsyncConnection};
use diesel_async::turso::TursoConnection;

diesel::table! {
    authors (id) {
        id -> BigInt,
        name -> Text,
    }
}

diesel::table! {
    books (id) {
        id -> BigInt,
        author_id -> BigInt,
        title -> Text,
    }
}

diesel::table! {
    reviews (id) {
        id -> BigInt,
        book_id -> BigInt,
        stars -> BigInt,
    }
}

diesel::joinable!(books -> authors (author_id));
diesel::joinable!(reviews -> books (book_id));
diesel::allow_tables_to_appear_in_same_query!(authors, books, reviews);

/// `orwell` wrote two books, `austen` one, `nobody` none — so a left join
/// from `authors` has a row whose right half is all NULL, and an inner join
/// from `books` never sees `nobody`. The lone review hangs off book 1, so
/// `books LEFT JOIN reviews` covers both the matched and the unmatched side
/// in one query.
async fn seeded() -> Result<TursoConnection> {
    let mut conn = TursoConnection::establish(":memory:").await?;
    conn.batch_execute(
        "CREATE TABLE authors (id INTEGER PRIMARY KEY, name TEXT NOT NULL) STRICT;
         CREATE TABLE books (
             id INTEGER PRIMARY KEY,
             author_id INTEGER NOT NULL,
             title TEXT NOT NULL
         ) STRICT;
         CREATE TABLE reviews (
             id INTEGER PRIMARY KEY,
             book_id INTEGER NOT NULL,
             stars INTEGER NOT NULL
         ) STRICT;
         INSERT INTO authors VALUES (1, 'orwell'), (2, 'austen'), (3, 'nobody');
         INSERT INTO books VALUES (1, 1, '1984'), (2, 1, 'animal farm'), (3, 2, 'emma');
         INSERT INTO reviews VALUES (1, 1, 5);",
    )
    .await?;
    Ok(conn)
}

#[tokio::test(flavor = "current_thread")]
async fn inner_join_with_inferred_on_clause() -> Result<()> {
    let mut conn = seeded().await?;

    let rows: Vec<(String, String)> = books::table
        .inner_join(authors::table)
        .select((authors::name, books::title))
        .order_by(books::id)
        .load(&mut conn)
        .await?;

    assert_eq!(
        rows,
        vec![
            ("orwell".to_string(), "1984".to_string()),
            ("orwell".to_string(), "animal farm".to_string()),
            ("austen".to_string(), "emma".to_string()),
        ],
        "author 3 has no books, so an inner join must drop it"
    );
    Ok(())
}

#[tokio::test(flavor = "current_thread")]
async fn inner_join_with_explicit_on_clause() -> Result<()> {
    let mut conn = seeded().await?;

    // The same join written by hand, and narrowed by the `ON` rather than by
    // a `WHERE` — an explicit `.on()` renders through the same fragment as
    // the `joinable!`-inferred one, but it's the shape the two raw-SQL
    // workarounds in fifteen-db were written for, so it gets its own case.
    let rows: Vec<(String, String)> = authors::table
        .inner_join(
            books::table.on(books::author_id
                .eq(authors::id)
                .and(books::title.ne("animal farm"))),
        )
        .select((authors::name, books::title))
        .order_by(books::id)
        .load(&mut conn)
        .await?;

    assert_eq!(
        rows,
        vec![
            ("orwell".to_string(), "1984".to_string()),
            ("austen".to_string(), "emma".to_string()),
        ]
    );
    Ok(())
}

#[tokio::test(flavor = "current_thread")]
async fn left_join_with_inferred_on_clause() -> Result<()> {
    let mut conn = seeded().await?;

    let rows: Vec<(String, Option<String>)> = authors::table
        .left_join(books::table)
        .select((authors::name, books::title.nullable()))
        .order_by((authors::id, books::id))
        .load(&mut conn)
        .await?;

    assert_eq!(
        rows,
        vec![
            ("orwell".to_string(), Some("1984".to_string())),
            ("orwell".to_string(), Some("animal farm".to_string())),
            ("austen".to_string(), Some("emma".to_string())),
            ("nobody".to_string(), None),
        ],
        "the bookless author survives a left join with a NULL right half"
    );
    Ok(())
}

#[tokio::test(flavor = "current_thread")]
async fn left_join_with_explicit_on_clause_and_is_null_filter() -> Result<()> {
    let mut conn = seeded().await?;

    // "rows on the left with no match on the right" — the anti-join every
    // pending-work query in fifteen-db is built out of.
    let unreviewed: Vec<String> = books::table
        .left_join(reviews::table.on(reviews::book_id.eq(books::id)))
        .filter(reviews::id.is_null())
        .select(books::title)
        .order_by(books::id)
        .load(&mut conn)
        .await?;

    assert_eq!(unreviewed, vec!["animal farm", "emma"]);
    Ok(())
}

#[tokio::test(flavor = "current_thread")]
async fn chained_joins_nest_the_from_clause() -> Result<()> {
    let mut conn = seeded().await?;

    // Two joins on one query source nest the fragment inside itself, which
    // is where the parenthesized rendering doubled up:
    // `FROM (("authors" JOIN "books" ON …) LEFT JOIN "reviews" ON …)`.
    let rows: Vec<(String, String, Option<i64>)> = authors::table
        .inner_join(books::table)
        .left_join(reviews::table.on(reviews::book_id.eq(books::id)))
        .select((authors::name, books::title, reviews::stars.nullable()))
        .order_by(books::id)
        .load(&mut conn)
        .await?;

    assert_eq!(
        rows,
        vec![
            ("orwell".to_string(), "1984".to_string(), Some(5)),
            ("orwell".to_string(), "animal farm".to_string(), None),
            ("austen".to_string(), "emma".to_string(), None),
        ]
    );
    Ok(())
}

#[tokio::test(flavor = "current_thread")]
async fn join_survives_boxing_and_binds() -> Result<()> {
    let mut conn = seeded().await?;

    // `into_boxed` swaps the whole query for the dynamic fragment chain, so
    // it reaches the join through a different code path than the typed
    // queries above. Binds also have to land in the right order relative to
    // the join, which the `ON` + `WHERE` pair here checks.
    let mut query = authors::table
        .inner_join(books::table)
        .select((authors::name, books::title))
        .into_boxed();
    query = query.filter(authors::name.eq("orwell"));
    query = query.filter(books::title.like("1%"));

    let rows: Vec<(String, String)> = query.load(&mut conn).await?;
    assert_eq!(rows, vec![("orwell".to_string(), "1984".to_string())]);
    Ok(())
}
