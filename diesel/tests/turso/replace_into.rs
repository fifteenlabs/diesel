//! `replace_into` (INSERT OR REPLACE) and `insert_or_ignore_into`
//! (INSERT OR IGNORE) through the diesel DSL, exercising the
//! `QueryFragment<Turso>` impls for the `Replace` / `InsertOrIgnore` insert
//! operators in `query_fragments.rs`.

use anyhow::Result;
use diesel::async_dsl::RunQueryDsl;
use diesel::connection::{AsyncConnection, SimpleAsyncConnection};
use diesel::prelude::*;
use diesel::turso::TursoConnection;

diesel::table! {
    kv(id) {
        id -> BigInt,
        label -> Text,
        hits -> BigInt,
    }
}

#[derive(Insertable, Queryable, Debug, PartialEq)]
#[diesel(table_name = kv)]
struct Kv {
    id: i64,
    label: String,
    hits: i64,
}

async fn setup() -> Result<TursoConnection> {
    let mut conn = TursoConnection::establish(":memory:").await?;
    conn.batch_execute(
        "CREATE TABLE kv(id INTEGER PRIMARY KEY, label TEXT NOT NULL, hits BIGINT NOT NULL) STRICT",
    )
    .await?;
    Ok(conn)
}

#[tokio::test(flavor = "current_thread")]
async fn replace_into_overwrites_conflicting_row() -> Result<()> {
    let mut conn = setup().await?;

    let n = diesel::replace_into(kv::table)
        .values(Kv {
            id: 1,
            label: "a".into(),
            hits: 1,
        })
        .execute(&mut conn)
        .await?;
    assert_eq!(n, 1);

    // Same PK → row is deleted and re-inserted with the new values.
    diesel::replace_into(kv::table)
        .values(Kv {
            id: 1,
            label: "b".into(),
            hits: 99,
        })
        .execute(&mut conn)
        .await?;

    let stored: Kv = kv::table.filter(kv::id.eq(1i64)).first(&mut conn).await?;
    assert_eq!(
        stored,
        Kv {
            id: 1,
            label: "b".into(),
            hits: 99
        }
    );
    let count: i64 = kv::table.count().get_result(&mut conn).await?;
    assert_eq!(count, 1);
    Ok(())
}

#[tokio::test(flavor = "current_thread")]
async fn insert_or_ignore_skips_conflicting_row() -> Result<()> {
    let mut conn = setup().await?;

    diesel::insert_or_ignore_into(kv::table)
        .values(Kv {
            id: 1,
            label: "a".into(),
            hits: 1,
        })
        .execute(&mut conn)
        .await?;

    // Same PK → ignored, original row kept.
    let n = diesel::insert_or_ignore_into(kv::table)
        .values(Kv {
            id: 1,
            label: "b".into(),
            hits: 99,
        })
        .execute(&mut conn)
        .await?;
    assert_eq!(n, 0);

    let stored: Kv = kv::table.filter(kv::id.eq(1i64)).first(&mut conn).await?;
    assert_eq!(
        stored,
        Kv {
            id: 1,
            label: "a".into(),
            hits: 1
        }
    );
    Ok(())
}
