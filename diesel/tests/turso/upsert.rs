//! Upsert acceptance: `ON CONFLICT ... DO UPDATE` and `ON CONFLICT DO NOTHING`
//! through the diesel DSL, exercising the `OnConflictSelectWrapper` impls in
//! `query_fragments.rs` that required `pub` exposure in the diesel fork.

use anyhow::Result;
use diesel::async_dsl::RunQueryDsl;
use diesel::connection::{AsyncConnection, SimpleAsyncConnection};
use diesel::prelude::*;
use diesel::turso::TursoConnection;
use diesel::upsert::excluded;

diesel::table! {
    counters(id) {
        id -> BigInt,
        label -> Text,
        hits -> BigInt,
    }
}

#[derive(Insertable, Queryable, Debug, PartialEq)]
#[diesel(table_name = counters)]
struct Counter {
    id: i64,
    label: String,
    hits: i64,
}

async fn setup() -> Result<TursoConnection> {
    let mut conn = TursoConnection::establish(":memory:").await?;
    conn.batch_execute(
        "CREATE TABLE counters(id INTEGER PRIMARY KEY, label TEXT NOT NULL, hits BIGINT NOT NULL) STRICT",
    )
    .await?;
    Ok(conn)
}

#[tokio::test(flavor = "current_thread")]
async fn on_conflict_do_nothing() -> Result<()> {
    let mut conn = setup().await?;
    let row = Counter {
        id: 1,
        label: "a".into(),
        hits: 5,
    };

    let n = diesel::insert_into(counters::table)
        .values(&row)
        .on_conflict_do_nothing()
        .execute(&mut conn)
        .await?;
    assert_eq!(n, 1);

    // Second insert with same PK — conflict → no-op.
    let n = diesel::insert_into(counters::table)
        .values(&Counter {
            id: 1,
            label: "b".into(),
            hits: 999,
        })
        .on_conflict_do_nothing()
        .execute(&mut conn)
        .await?;
    assert_eq!(n, 0);

    let stored: Counter = counters::table
        .filter(counters::id.eq(1i64))
        .first(&mut conn)
        .await?;
    assert_eq!(stored, row);
    Ok(())
}

#[tokio::test(flavor = "current_thread")]
async fn on_conflict_do_update_with_excluded() -> Result<()> {
    let mut conn = setup().await?;

    diesel::insert_into(counters::table)
        .values(Counter {
            id: 1,
            label: "a".into(),
            hits: 1,
        })
        .execute(&mut conn)
        .await?;

    // Upsert: on conflict, add to hits and replace label.
    diesel::insert_into(counters::table)
        .values(Counter {
            id: 1,
            label: "b".into(),
            hits: 10,
        })
        .on_conflict(counters::id)
        .do_update()
        .set((
            counters::label.eq(excluded(counters::label)),
            counters::hits.eq(counters::hits + excluded(counters::hits)),
        ))
        .execute(&mut conn)
        .await?;

    // Second conflict: +7 more, label back to "c".
    diesel::insert_into(counters::table)
        .values(Counter {
            id: 1,
            label: "c".into(),
            hits: 7,
        })
        .on_conflict(counters::id)
        .do_update()
        .set((
            counters::label.eq(excluded(counters::label)),
            counters::hits.eq(counters::hits + excluded(counters::hits)),
        ))
        .execute(&mut conn)
        .await?;

    let stored: Counter = counters::table
        .filter(counters::id.eq(1i64))
        .first(&mut conn)
        .await?;
    assert_eq!(
        stored,
        Counter {
            id: 1,
            label: "c".into(),
            hits: 18
        }
    );
    Ok(())
}
