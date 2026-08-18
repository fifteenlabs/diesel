//! Batch INSERT via `insert_into().values(&vec)` — exercises diesel's
//! multi-row `VALUES (?,?),(?,?),...` path through
//! `SqliteLikeBatchInsertSupport`. Turso accepts the same SQL SQLite
//! does, so this should round-trip just like per-row inserts.

use anyhow::Result;
use diesel::async_dsl::RunQueryDsl;
use diesel::connection::{AsyncConnection, SimpleAsyncConnection};
use diesel::prelude::*;
use diesel::turso::TursoConnection;

diesel::table! {
    widgets (id) {
        id -> BigInt,
        name -> Text,
        qty -> Integer,
        tag -> Nullable<Text>,
    }
}

#[derive(Insertable, Queryable, PartialEq, Debug, Clone)]
#[diesel(table_name = widgets)]
struct Widget {
    id: i64,
    name: String,
    qty: i32,
    tag: Option<String>,
}

async fn setup() -> Result<TursoConnection> {
    let mut conn = TursoConnection::establish(":memory:").await?;
    conn.batch_execute(
        "CREATE TABLE widgets(
            id   INTEGER PRIMARY KEY,
            name TEXT    NOT NULL,
            qty  INTEGER NOT NULL,
            tag  TEXT
        ) STRICT",
    )
    .await?;
    Ok(conn)
}

#[tokio::test(flavor = "current_thread")]
async fn batch_insert_roundtrip() -> Result<()> {
    let mut conn = setup().await?;
    // All rows share the same column shape (no None in this test) so the
    // multi-row VALUES list is well-formed regardless of DEFAULT support.
    let rows = vec![
        Widget {
            id: 1,
            name: "a".into(),
            qty: 10,
            tag: Some("red".into()),
        },
        Widget {
            id: 2,
            name: "b".into(),
            qty: 20,
            tag: Some("green".into()),
        },
        Widget {
            id: 3,
            name: "c".into(),
            qty: 30,
            tag: Some("blue".into()),
        },
    ];

    let n = diesel::insert_into(widgets::table)
        .values(&rows)
        .execute(&mut conn)
        .await?;
    assert_eq!(n, 3);

    let got: Vec<Widget> = widgets::table
        .order(widgets::id.asc())
        .load(&mut conn)
        .await?;
    assert_eq!(got, rows);
    Ok(())
}

// Turso (like SQLite) doesn't accept `DEFAULT` inside a multi-row
// `VALUES` list. diesel's `Insertable` derive normally omits an
// `Option<T>` column when the value is `None`, which produces rows
// with different arities and breaks Turso's "all VALUES must have the
// same number of terms" check. Workaround: keep all `Option` values
// the same variant across a batch, or loop with single-row inserts.
#[tokio::test(flavor = "current_thread")]
async fn batch_insert_mixed_optional_columns_rejected() -> Result<()> {
    let mut conn = setup().await?;
    let rows = vec![
        Widget {
            id: 1,
            name: "a".into(),
            qty: 10,
            tag: Some("red".into()),
        },
        Widget {
            id: 2,
            name: "b".into(),
            qty: 20,
            tag: None,
        },
    ];
    let err = diesel::insert_into(widgets::table)
        .values(&rows)
        .execute(&mut conn)
        .await
        .expect_err("mixed Some/None Option column in batch should fail");
    assert!(
        err.to_string().contains("same number of terms"),
        "unexpected error: {err}"
    );
    Ok(())
}

#[tokio::test(flavor = "current_thread")]
async fn batch_insert_single_row_still_works() -> Result<()> {
    let mut conn = setup().await?;
    let rows = vec![Widget {
        id: 42,
        name: "solo".into(),
        qty: 1,
        tag: None,
    }];
    let n = diesel::insert_into(widgets::table)
        .values(&rows)
        .execute(&mut conn)
        .await?;
    assert_eq!(n, 1);
    Ok(())
}

#[tokio::test(flavor = "current_thread")]
async fn batch_insert_large() -> Result<()> {
    let mut conn = setup().await?;
    let rows: Vec<Widget> = (1..=500)
        .map(|i| Widget {
            id: i,
            name: format!("w{i}"),
            qty: i as i32,
            tag: Some(if i % 2 == 0 { "even" } else { "odd" }.into()),
        })
        .collect();

    let n = diesel::insert_into(widgets::table)
        .values(&rows)
        .execute(&mut conn)
        .await?;
    assert_eq!(n, 500);

    let count: i64 = widgets::table.count().get_result(&mut conn).await?;
    assert_eq!(count, 500);
    Ok(())
}
