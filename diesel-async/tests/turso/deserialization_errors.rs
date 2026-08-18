//! Decoding errors must surface as `DeserializationError` — never panics.
//! Mirrors diesel's own `errors_during_deserialization_do_not_panic`
//! (`diesel_tests/tests/types.rs`), adapted for turbo-diesel's actual
//! decode surface: integer narrowing and value-kind mismatches.
//!
//! The mismatch is declared, not written in SQL. Each test selects a column
//! through a second `table!` that claims a narrower or plainly wrong type than
//! the column actually holds — which is how this failure reaches us in
//! practice: a migration widens a column, or a hand-written `table!` says
//! `Integer` where the database stores an i64, and the SELECT that has been
//! fine for a year starts returning a value its declared type can't hold.
//! Both `table!` blocks below name the same physical table, so the only
//! difference between the query that decodes and the query that errors is the
//! sql type diesel was told to expect.

use anyhow::Result;
use diesel::prelude::*;
use diesel::result::Error;
use diesel_async::{AsyncConnection, RunQueryDsl, SimpleAsyncConnection};
use diesel_async::turso::TursoConnection;

// The truth: `v` is a BIGINT column.
diesel::table! {
    readings (id) {
        id -> BigInt,
        v -> BigInt,
    }
}

// The same table, under-declared three ways.
diesel::table! {
    #[sql_name = "readings"]
    readings_as_small (id) {
        id -> BigInt,
        v -> SmallInt,
    }
}

diesel::table! {
    #[sql_name = "readings"]
    readings_as_int (id) {
        id -> BigInt,
        v -> Integer,
    }
}

diesel::table! {
    #[sql_name = "readings"]
    readings_as_text (id) {
        id -> BigInt,
        v -> Text,
    }
}

async fn seeded(values: &[i64]) -> Result<TursoConnection> {
    let mut conn = TursoConnection::establish(":memory:").await?;
    conn.batch_execute("CREATE TABLE readings (id INTEGER PRIMARY KEY, v BIGINT NOT NULL) STRICT")
        .await?;
    let rows: Vec<_> = values
        .iter()
        .enumerate()
        .map(|(i, v)| (readings::id.eq(i as i64 + 1), readings::v.eq(*v)))
        .collect();
    diesel::insert_into(readings::table)
        .values(&rows)
        .execute(&mut conn)
        .await?;
    Ok(conn)
}

#[tokio::test(flavor = "current_thread")]
async fn integer_overflow_on_smallint_decode_is_error_not_panic() -> Result<()> {
    let mut conn = seeded(&[1_000_000, -40_000, 70_000]).await?;

    for id in 1..=3i64 {
        let res = readings_as_small::table
            .filter(readings_as_small::id.eq(id))
            .select(readings_as_small::v)
            .load::<i16>(&mut conn)
            .await;
        assert!(
            matches!(res, Err(Error::DeserializationError(_))),
            "expected DeserializationError for row {id}, got {res:?}"
        );
    }

    // The rows themselves are fine — read through the column's real type they
    // all decode. Only the declared type was wrong, and that is what errored.
    let ok: Vec<i64> = readings::table
        .order(readings::id.asc())
        .select(readings::v)
        .load(&mut conn)
        .await?;
    assert_eq!(ok, vec![1_000_000, -40_000, 70_000]);
    Ok(())
}

#[tokio::test(flavor = "current_thread")]
async fn unsigned_integer_overflow_on_integer_decode() -> Result<()> {
    let mut conn = seeded(&[9_999_999_999]).await?;
    let res = readings_as_int::table
        .select(readings_as_int::v)
        .load::<i32>(&mut conn)
        .await;
    assert!(
        matches!(res, Err(Error::DeserializationError(_))),
        "expected DeserializationError, got {res:?}"
    );

    // A value that does fit decodes through the very same query, so the error
    // above is the narrowing and not the `Integer` path being broken outright.
    let mut conn = seeded(&[7]).await?;
    let ok: Vec<i32> = readings_as_int::table
        .select(readings_as_int::v)
        .load(&mut conn)
        .await?;
    assert_eq!(ok, vec![7]);
    Ok(())
}

#[tokio::test(flavor = "current_thread")]
async fn value_kind_mismatch_is_error() -> Result<()> {
    // Column holds an INTEGER but we ask FromSql<Text> to decode it.
    let mut conn = seeded(&[42]).await?;
    let res = readings_as_text::table
        .select(readings_as_text::v)
        .load::<String>(&mut conn)
        .await;
    assert!(
        matches!(res, Err(Error::DeserializationError(_))),
        "expected DeserializationError, got {res:?}"
    );
    Ok(())
}
