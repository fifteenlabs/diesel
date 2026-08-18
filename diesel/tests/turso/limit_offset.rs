//! Exercises the four `LimitOffsetClause` shapes plus the boxed variant
//! (`src/query_fragments.rs`). Mirrors diesel's own
//! `diesel_tests/tests/limit_offset.rs`, adapted for the async API.

use anyhow::Result;
use diesel::async_dsl::RunQueryDsl;
use diesel::connection::{AsyncConnection, SimpleAsyncConnection};
use diesel::prelude::*;
use diesel::turso::TursoConnection;

diesel::table! {
    items (id) {
        id -> BigInt,
        name -> Text,
    }
}

#[derive(Insertable, Queryable, PartialEq, Debug)]
#[diesel(table_name = items)]
struct Item {
    id: i64,
    name: String,
}

async fn seed() -> Result<TursoConnection> {
    let mut conn = TursoConnection::establish(":memory:").await?;
    conn.batch_execute(
        "CREATE TABLE items(id INTEGER PRIMARY KEY, name TEXT NOT NULL) STRICT;
         INSERT INTO items VALUES (1,'a'),(2,'b'),(3,'c'),(4,'d'),(5,'e');",
    )
    .await?;
    Ok(conn)
}

#[tokio::test(flavor = "current_thread")]
async fn no_limit_no_offset() -> Result<()> {
    let mut conn = seed().await?;
    let got: Vec<Item> = items::table.order(items::id.asc()).load(&mut conn).await?;
    assert_eq!(got.len(), 5);
    Ok(())
}

#[tokio::test(flavor = "current_thread")]
async fn limit_only() -> Result<()> {
    let mut conn = seed().await?;
    let got: Vec<Item> = items::table
        .order(items::id.asc())
        .limit(2)
        .load(&mut conn)
        .await?;
    assert_eq!(got.iter().map(|i| i.id).collect::<Vec<_>>(), vec![1, 2]);
    Ok(())
}

#[tokio::test(flavor = "current_thread")]
async fn offset_only_injects_limit_negative_one() -> Result<()> {
    let mut conn = seed().await?;
    // No LIMIT + OFFSET 2: our impl injects `LIMIT -1` so the OFFSET is
    // legal in sqlite/turso. Expect rows 3, 4, 5.
    let got: Vec<Item> = items::table
        .order(items::id.asc())
        .offset(2)
        .load(&mut conn)
        .await?;
    assert_eq!(got.iter().map(|i| i.id).collect::<Vec<_>>(), vec![3, 4, 5]);
    Ok(())
}

#[tokio::test(flavor = "current_thread")]
async fn limit_and_offset() -> Result<()> {
    let mut conn = seed().await?;
    let got: Vec<Item> = items::table
        .order(items::id.asc())
        .limit(2)
        .offset(1)
        .load(&mut conn)
        .await?;
    assert_eq!(got.iter().map(|i| i.id).collect::<Vec<_>>(), vec![2, 3]);
    Ok(())
}

#[tokio::test(flavor = "current_thread")]
async fn boxed_no_clauses() -> Result<()> {
    let mut conn = seed().await?;
    let got: Vec<Item> = items::table
        .order(items::id.asc())
        .into_boxed()
        .load(&mut conn)
        .await?;
    assert_eq!(got.len(), 5);
    Ok(())
}

#[tokio::test(flavor = "current_thread")]
async fn boxed_limit_only() -> Result<()> {
    let mut conn = seed().await?;
    let got: Vec<Item> = items::table
        .order(items::id.asc())
        .into_boxed()
        .limit(3)
        .load(&mut conn)
        .await?;
    assert_eq!(got.iter().map(|i| i.id).collect::<Vec<_>>(), vec![1, 2, 3]);
    Ok(())
}

#[tokio::test(flavor = "current_thread")]
async fn boxed_offset_only() -> Result<()> {
    let mut conn = seed().await?;
    let got: Vec<Item> = items::table
        .order(items::id.asc())
        .into_boxed()
        .offset(4)
        .load(&mut conn)
        .await?;
    assert_eq!(got.iter().map(|i| i.id).collect::<Vec<_>>(), vec![5]);
    Ok(())
}

#[tokio::test(flavor = "current_thread")]
async fn boxed_limit_and_offset() -> Result<()> {
    let mut conn = seed().await?;
    let got: Vec<Item> = items::table
        .order(items::id.asc())
        .into_boxed()
        .limit(2)
        .offset(2)
        .load(&mut conn)
        .await?;
    assert_eq!(got.iter().map(|i| i.id).collect::<Vec<_>>(), vec![3, 4]);
    Ok(())
}
