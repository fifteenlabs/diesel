//! Ported from diesel's `test_correct_serialization_of_owned_strings` /
//! `_owned_bytes` (`diesel/src/sqlite/connection/mod.rs`). Verifies that a
//! `ToSql` impl producing an *owned* `String` / `Vec<u8>` inside the body
//! lives long enough for our bind collector to consume it.
//!
//! The binds are placed by the query builder from `table!` columns, which is
//! how the app reaches this code: the collector has to hold each owned value
//! until the whole statement is assembled and run, not just until the next
//! call returns.

use anyhow::Result;
use diesel::deserialize::{FromSql, FromSqlRow};
use diesel::expression::AsExpression;
use diesel::prelude::*;
use diesel::serialize::{self, IsNull, Output, ToSql};
use diesel::sql_types::{Binary, Text};
use diesel_async::{AsyncConnection, RunQueryDsl, SimpleAsyncConnection};
use diesel::turso::{Turso, TursoValue};
use diesel_async::turso::TursoConnection;

#[derive(Debug, AsExpression, FromSqlRow)]
#[diesel(sql_type = Text)]
struct OwnedText(String);

impl ToSql<Text, Turso> for OwnedText {
    fn to_sql<'b>(&'b self, out: &mut Output<'b, '_, Turso>) -> serialize::Result {
        // Construct the owned value inside this body (don't borrow from
        // self). The bind collector must take ownership before the
        // temporary dies.
        out.set_value(self.0.to_string());
        Ok(IsNull::No)
    }
}

impl FromSql<Text, Turso> for OwnedText {
    fn from_sql(v: TursoValue<'_>) -> diesel::deserialize::Result<Self> {
        <String as FromSql<Text, Turso>>::from_sql(v).map(OwnedText)
    }
}

#[derive(Debug, AsExpression, FromSqlRow)]
#[diesel(sql_type = Binary)]
struct OwnedBytes(Vec<u8>);

impl ToSql<Binary, Turso> for OwnedBytes {
    fn to_sql<'b>(&'b self, out: &mut Output<'b, '_, Turso>) -> serialize::Result {
        out.set_value(self.0.clone());
        Ok(IsNull::No)
    }
}

impl FromSql<Binary, Turso> for OwnedBytes {
    fn from_sql(v: TursoValue<'_>) -> diesel::deserialize::Result<Self> {
        <Vec<u8> as FromSql<Binary, Turso>>::from_sql(v).map(OwnedBytes)
    }
}

diesel::table! {
    stash (id) {
        id -> BigInt,
        t -> Text,
        b -> Binary,
    }
}

async fn setup() -> Result<TursoConnection> {
    let mut conn = TursoConnection::establish(":memory:").await?;
    conn.batch_execute(
        "CREATE TABLE stash(id INTEGER PRIMARY KEY, t TEXT NOT NULL, b BLOB NOT NULL) STRICT",
    )
    .await?;
    Ok(conn)
}

#[tokio::test(flavor = "current_thread")]
async fn owned_string_and_bytes_bind_and_decode() -> Result<()> {
    let mut conn = setup().await?;

    diesel::insert_into(stash::table)
        .values((
            stash::id.eq(1i64),
            stash::t.eq(OwnedText("hello".into())),
            stash::b.eq(OwnedBytes(vec![0xDE, 0xAD, 0xBE, 0xEF])),
        ))
        .execute(&mut conn)
        .await?;

    // Empty-string and empty-blob edge cases: diesel's original tests
    // pin this specifically.
    diesel::insert_into(stash::table)
        .values((
            stash::id.eq(2i64),
            stash::t.eq(OwnedText(String::new())),
            stash::b.eq(OwnedBytes(Vec::new())),
        ))
        .execute(&mut conn)
        .await?;

    // Two rows in one statement: the collector holds several owned values at
    // once, each dropped only after the batch has run.
    diesel::insert_into(stash::table)
        .values(&vec![
            (
                stash::id.eq(3i64),
                stash::t.eq(OwnedText("three".into())),
                stash::b.eq(OwnedBytes(vec![3])),
            ),
            (
                stash::id.eq(4i64),
                stash::t.eq(OwnedText("four".into())),
                stash::b.eq(OwnedBytes(vec![4, 4])),
            ),
        ])
        .execute(&mut conn)
        .await?;

    let (t, b): (OwnedText, OwnedBytes) = stash::table
        .filter(stash::id.eq(1i64))
        .select((stash::t, stash::b))
        .get_result(&mut conn)
        .await?;
    assert_eq!(t.0, "hello");
    assert_eq!(b.0, vec![0xDE, 0xAD, 0xBE, 0xEF]);

    let (t, b): (OwnedText, OwnedBytes) = stash::table
        .filter(stash::id.eq(2i64))
        .select((stash::t, stash::b))
        .get_result(&mut conn)
        .await?;
    assert!(t.0.is_empty());
    assert!(b.0.is_empty());

    let (t, b): (OwnedText, OwnedBytes) = stash::table
        .filter(stash::id.eq(4i64))
        .select((stash::t, stash::b))
        .get_result(&mut conn)
        .await?;
    assert_eq!(t.0, "four");
    assert_eq!(b.0, vec![4, 4]);
    Ok(())
}
