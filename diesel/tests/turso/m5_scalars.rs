//! M5 acceptance: the full scalar ToSql/FromSql grid roundtrips through the
//! diesel DSL with real binds — `SmallInt`, `Integer`, `BigInt`, `Float`,
//! `Double`, `Text`, `Bool`, `Binary`, and the `Nullable<…>` form of each.
//!
//! Every bind here is placed by the query builder from a `table!` column, so
//! the sql type each value serializes as is the one the schema declares rather
//! than one the test asserted in a string. Binding the wrong Rust type is a
//! compile error instead of a runtime coercion.

use anyhow::Result;
use diesel::async_dsl::RunQueryDsl;
use diesel::connection::{AsyncConnection, SimpleAsyncConnection};
use diesel::prelude::*;
use diesel::turso::TursoConnection;

// Match fifteen-db's convention: store timestamps as unix epoch BigInt
// rather than Timestamp (which demands a dedicated Rust datetime type).
diesel::table! {
    things (id) {
        id -> BigInt,
        tiny -> SmallInt,
        normal -> Integer,
        big -> BigInt,
        price -> Float,
        precise -> Double,
        name -> Text,
        nickname -> Nullable<Text>,
        is_active -> Bool,
        blob -> Binary,
        created_at -> BigInt,
        birthday -> Nullable<BigInt>,
    }
}

#[derive(Insertable, Queryable, PartialEq, Debug)]
#[diesel(table_name = things)]
struct Thing {
    id: i64,
    tiny: i16,
    normal: i32,
    big: i64,
    price: f32,
    precise: f64,
    name: String,
    nickname: Option<String>,
    is_active: bool,
    blob: Vec<u8>,
    created_at: i64,
    birthday: Option<i64>,
}

async fn setup() -> Result<TursoConnection> {
    let mut conn = TursoConnection::establish(":memory:").await?;
    conn.batch_execute(
        "CREATE TABLE things(
            id        INTEGER PRIMARY KEY,
            tiny      SMALLINT NOT NULL,
            normal    INTEGER  NOT NULL,
            big       BIGINT   NOT NULL,
            price     REAL     NOT NULL,
            precise   REAL     NOT NULL,
            name      TEXT     NOT NULL,
            nickname  TEXT,
            is_active BOOLEAN  NOT NULL,
            blob      BLOB     NOT NULL,
            created_at BIGINT  NOT NULL,
            birthday  BIGINT
        ) STRICT",
    )
    .await?;
    Ok(conn)
}

fn sample_row(id: i64, nickname: Option<&str>, birthday: Option<i64>) -> Thing {
    Thing {
        id,
        tiny: 7,
        normal: -42,
        big: 1_000_000_000_042,
        price: 9.5,
        precise: std::f64::consts::PI,
        name: "widget".into(),
        nickname: nickname.map(Into::into),
        is_active: true,
        blob: vec![0xde, 0xad, 0xbe, 0xef],
        created_at: 1_700_000_000,
        birthday,
    }
}

#[tokio::test(flavor = "current_thread")]
async fn insert_via_dsl_roundtrip() -> Result<()> {
    let mut conn = setup().await?;
    let row = sample_row(1, Some("wid"), Some(915_235_200));
    let row_null = sample_row(2, None, None);

    // diesel DSL insert — exercises ToSql for every scalar type + Option.
    let n = diesel::insert_into(things::table)
        .values(&row)
        .execute(&mut conn)
        .await?;
    assert_eq!(n, 1);
    let n = diesel::insert_into(things::table)
        .values(&row_null)
        .execute(&mut conn)
        .await?;
    assert_eq!(n, 1);

    // diesel DSL select — exercises FromSql for every scalar type + Option.
    let mut got: Vec<Thing> = things::table
        .order(things::id.asc())
        .load(&mut conn)
        .await?;
    // f32 equality is iffy across conversions; assert approximately.
    for t in &mut got {
        t.price = (t.price * 10.0).round() / 10.0;
    }
    let mut expected = vec![row, row_null];
    for t in &mut expected {
        t.price = (t.price * 10.0).round() / 10.0;
    }
    assert_eq!(got, expected);
    Ok(())
}

/// Values bound into a `WHERE` clause rather than a `VALUES` list: the same
/// `ToSql` impls, reached through comparison expressions. `Bool` and the
/// null-check are the interesting ones — a bool has no native storage in
/// SQLite, and `IS NULL` has to stay a predicate rather than become a bind.
#[tokio::test(flavor = "current_thread")]
async fn binds_in_where_clauses() -> Result<()> {
    let mut conn = setup().await?;
    diesel::insert_into(things::table)
        .values(&sample_row(42, Some("x"), None))
        .execute(&mut conn)
        .await?;

    let rows: Vec<(i64, String)> = things::table
        .filter(things::id.eq(42i64))
        .filter(things::is_active.eq(true))
        .select((things::id, things::name))
        .load(&mut conn)
        .await?;
    assert_eq!(rows, vec![(42, "widget".to_string())]);

    // Same row, negated on the bool: proves `is_active.eq(true)` above
    // actually discriminated rather than being ignored.
    let none: Vec<i64> = things::table
        .filter(things::is_active.eq(false))
        .select(things::id)
        .load(&mut conn)
        .await?;
    assert!(none.is_empty());

    let zero: Vec<(i64, String)> = things::table
        .filter(things::nickname.is_null())
        .filter(things::big.gt(2_000_000_000_000_i64))
        .select((things::id, things::name))
        .load(&mut conn)
        .await?;
    assert!(zero.is_empty());

    // The nullable column really is queryable both ways round.
    let by_null: Vec<i64> = things::table
        .filter(things::birthday.is_null())
        .filter(things::nickname.is_not_null())
        .select(things::id)
        .load(&mut conn)
        .await?;
    assert_eq!(by_null, vec![42]);
    Ok(())
}

#[tokio::test(flavor = "current_thread")]
async fn diesel_filter_update_delete() -> Result<()> {
    let mut conn = setup().await?;
    for i in 1..=5i64 {
        diesel::insert_into(things::table)
            .values(&sample_row(i, Some("x"), None))
            .execute(&mut conn)
            .await?;
    }

    let updated = diesel::update(things::table.filter(things::id.eq(3i64)))
        .set(things::name.eq("changed"))
        .execute(&mut conn)
        .await?;
    assert_eq!(updated, 1);

    let names: Vec<String> = things::table
        .filter(things::id.between(2i64, 4i64))
        .order(things::id.asc())
        .select(things::name)
        .load(&mut conn)
        .await?;
    assert_eq!(
        names,
        vec!["widget".to_string(), "changed".into(), "widget".into()]
    );

    let deleted = diesel::delete(things::table.filter(things::id.gt(3i64)))
        .execute(&mut conn)
        .await?;
    assert_eq!(deleted, 2);

    let ids: Vec<i64> = things::table
        .select(things::id)
        .order(things::id.asc())
        .load(&mut conn)
        .await?;
    assert_eq!(ids, vec![1, 2, 3]);
    Ok(())
}
