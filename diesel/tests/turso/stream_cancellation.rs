//! Dropping a `load` stream mid-iteration must cleanly abandon the
//! in-flight turso statement and leave the connection usable. Pins the
//! cancellation-safety invariant of the streaming `load` path in
//! `src/connection.rs`.

use anyhow::Result;
use diesel::async_dsl::RunQueryDsl;
use diesel::connection::{AsyncConnection, SimpleAsyncConnection};
use diesel::prelude::*;
use diesel::turso::TursoConnection;
use futures_util::StreamExt;

diesel::table! {
    items (id) {
        id -> BigInt,
        name -> Text,
    }
}

#[derive(Queryable, Debug)]
#[allow(dead_code)]
struct Item {
    id: i64,
    name: String,
}

async fn seed(n: i64) -> Result<TursoConnection> {
    let mut conn = TursoConnection::establish(":memory:").await?;
    conn.batch_execute("CREATE TABLE items(id INTEGER PRIMARY KEY, name TEXT NOT NULL) STRICT")
        .await?;
    for i in 1..=n {
        diesel::insert_into(items::table)
            .values((items::id.eq(i), items::name.eq(format!("row-{i}"))))
            .execute(&mut conn)
            .await?;
    }
    Ok(conn)
}

#[tokio::test(flavor = "current_thread")]
async fn drop_stream_midway_then_reuse_connection() -> Result<()> {
    let mut conn = seed(50).await?;

    // Open the stream, take two rows, then drop it.
    {
        let mut stream = items::table
            .order(items::id.asc())
            .load_stream::<Item>(&mut conn)
            .await?;
        let first = stream.next().await.unwrap()?;
        let second = stream.next().await.unwrap()?;
        assert_eq!(first.id, 1);
        assert_eq!(second.id, 2);
        // Stream drops here with 48 rows still unyielded.
    }

    // Connection is usable: a fresh full scan comes back in full.
    let all: Vec<Item> = items::table.order(items::id.asc()).load(&mut conn).await?;
    assert_eq!(all.len(), 50);
    assert_eq!(all[0].id, 1);
    assert_eq!(all[49].id, 50);

    // And a point query still works for good measure.
    let one: Item = items::table
        .filter(items::id.eq(25i64))
        .get_result(&mut conn)
        .await?;
    assert_eq!(one.id, 25);
    Ok(())
}

#[tokio::test(flavor = "current_thread")]
async fn stream_yields_exactly_all_rows() -> Result<()> {
    let mut conn = seed(3).await?;
    let mut stream = items::table
        .order(items::id.asc())
        .load_stream::<Item>(&mut conn)
        .await?;
    let mut seen = 0;
    while let Some(r) = stream.next().await {
        let item = r?;
        seen += 1;
        assert_eq!(item.id, seen);
    }
    assert_eq!(seen, 3);
    Ok(())
}
