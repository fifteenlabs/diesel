//! M4 acceptance: `load()` produces a stream of rows that decode via
//! `#[derive(Queryable)]`. Uses only the BigInt/Text FromSql impls that
//! shipped with M4; fuller coverage in M5.
//!
//! The rows are selected through the typed DSL, so the decode under test is
//! the one the app performs: columns matched to struct fields by position at
//! compile time, against a `table!` the query builder also emitted the SELECT
//! from.

use anyhow::Result;
use diesel::async_dsl::RunQueryDsl;
use diesel::connection::{AsyncConnection, SimpleAsyncConnection};
use diesel::prelude::*;
use diesel::turso::TursoConnection;

diesel::table! {
    friends (id) {
        id -> BigInt,
        name -> Text,
    }
}

#[derive(Queryable, Debug, PartialEq)]
#[diesel(table_name = friends)]
struct Friend {
    id: i64,
    name: String,
}

const DDL: &str = "CREATE TABLE friends (id INTEGER PRIMARY KEY, name TEXT NOT NULL) STRICT";

#[tokio::test(flavor = "current_thread")]
async fn load_roundtrip() -> Result<()> {
    let mut conn = TursoConnection::establish(":memory:").await?;
    conn.batch_execute(DDL).await?;
    diesel::insert_into(friends::table)
        .values(&vec![
            (friends::id.eq(1i64), friends::name.eq("alice")),
            (friends::id.eq(2), friends::name.eq("bob")),
            (friends::id.eq(3), friends::name.eq("carol")),
        ])
        .execute(&mut conn)
        .await?;

    let rows: Vec<Friend> = friends::table
        .order(friends::id.asc())
        .load(&mut conn)
        .await?;

    assert_eq!(
        rows,
        vec![
            Friend {
                id: 1,
                name: "alice".into()
            },
            Friend {
                id: 2,
                name: "bob".into()
            },
            Friend {
                id: 3,
                name: "carol".into()
            },
        ]
    );
    Ok(())
}

#[tokio::test(flavor = "current_thread")]
async fn empty_result_set() -> Result<()> {
    let mut conn = TursoConnection::establish(":memory:").await?;
    conn.batch_execute(DDL).await?;
    let rows: Vec<Friend> = friends::table.load(&mut conn).await?;
    assert!(rows.is_empty());
    Ok(())
}

#[tokio::test(flavor = "current_thread")]
async fn filter_and_order() -> Result<()> {
    let mut conn = TursoConnection::establish(":memory:").await?;
    conn.batch_execute(DDL).await?;
    // Names deliberately run opposite to ids, so ordering by name descending
    // is not also ordering by id descending: a query that lost the ORDER BY,
    // or ordered by the wrong column, answers differently.
    diesel::insert_into(friends::table)
        .values(&vec![
            (friends::id.eq(5i64), friends::name.eq("b")),
            (friends::id.eq(2), friends::name.eq("c")),
            (friends::id.eq(10), friends::name.eq("a")),
        ])
        .execute(&mut conn)
        .await?;

    let rows: Vec<Friend> = friends::table
        .filter(friends::id.ge(5))
        .order(friends::name.desc())
        .load(&mut conn)
        .await?;
    let names: Vec<&str> = rows.iter().map(|r| r.name.as_str()).collect();
    assert_eq!(names, vec!["b", "a"]);
    // The filter dropped id=2, and the surviving pair came back in name order
    // rather than id order.
    assert_eq!(rows.iter().map(|r| r.id).collect::<Vec<_>>(), vec![5, 10]);
    Ok(())
}
