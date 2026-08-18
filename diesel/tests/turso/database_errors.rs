//! Constraint violations must surface as `Error::DatabaseError` — never
//! panic, never swallow into `QueryBuilderError`. Mirrors diesel's own
//! `diesel_tests/tests/errors.rs`.
//!
//! Note: turbo-diesel currently funnels every turso error through
//! `DatabaseErrorKind::Unknown` (`src/error.rs`) — once turso exposes
//! a structured error taxonomy, tighten these matches.
//!
//! All three violations are provoked through the typed DSL. The NOT NULL one
//! needs a `table!` that calls `users.name` nullable while the schema says it
//! is not; that disagreement is the realistic way an app hits this error, and
//! it lets the test bind a real NULL without writing the statement out by
//! hand.

use anyhow::Result;
use diesel::async_dsl::RunQueryDsl;
use diesel::connection::{AsyncConnection, SimpleAsyncConnection};
use diesel::prelude::*;
use diesel::result::Error;
use diesel::turso::TursoConnection;

diesel::table! {
    users (id) {
        id -> BigInt,
        name -> Text,
    }
}

// The same table with `name` declared nullable, so a NULL can be bound into a
// column the schema declares NOT NULL.
diesel::table! {
    #[sql_name = "users"]
    users_lax (id) {
        id -> BigInt,
        name -> Nullable<Text>,
    }
}

diesel::table! {
    fk_tests (id) {
        id -> BigInt,
        user_id -> BigInt,
    }
}

async fn setup() -> Result<TursoConnection> {
    let mut conn = TursoConnection::establish(":memory:").await?;
    conn.batch_execute(
        "CREATE TABLE users(id INTEGER PRIMARY KEY, name TEXT NOT NULL) STRICT;
         CREATE TABLE fk_tests(
             id INTEGER PRIMARY KEY,
             user_id INTEGER NOT NULL REFERENCES users(id)
         ) STRICT;
         PRAGMA foreign_keys = ON;",
    )
    .await?;
    Ok(conn)
}

#[tokio::test(flavor = "current_thread")]
async fn unique_violation_is_database_error() -> Result<()> {
    let mut conn = setup().await?;
    diesel::insert_into(users::table)
        .values((users::id.eq(1i64), users::name.eq("sean")))
        .execute(&mut conn)
        .await?;

    let err = diesel::insert_into(users::table)
        .values((users::id.eq(1i64), users::name.eq("jim")))
        .execute(&mut conn)
        .await
        .unwrap_err();

    assert!(
        matches!(err, Error::DatabaseError(_, _)),
        "expected DatabaseError, got {err:?}"
    );

    // The rejected insert left the original row alone — the error was raised
    // instead of the write, not alongside it.
    let names: Vec<String> = users::table.select(users::name).load(&mut conn).await?;
    assert_eq!(names, vec!["sean".to_string()]);
    Ok(())
}

#[tokio::test(flavor = "current_thread")]
async fn not_null_violation_is_database_error() -> Result<()> {
    let mut conn = setup().await?;
    let err = diesel::insert_into(users_lax::table)
        .values((users_lax::id.eq(1i64), users_lax::name.eq(None::<String>)))
        .execute(&mut conn)
        .await
        .unwrap_err();
    assert!(
        matches!(err, Error::DatabaseError(_, _)),
        "expected DatabaseError, got {err:?}"
    );

    // Same statement, a non-NULL name: the insert goes through, so what
    // failed above was the NULL and not the lax `table!` itself.
    diesel::insert_into(users_lax::table)
        .values((
            users_lax::id.eq(1i64),
            users_lax::name.eq(Some("sean".to_string())),
        ))
        .execute(&mut conn)
        .await?;
    Ok(())
}

#[tokio::test(flavor = "current_thread")]
async fn foreign_key_violation_is_database_error() -> Result<()> {
    let mut conn = setup().await?;
    let err = diesel::insert_into(fk_tests::table)
        .values((fk_tests::id.eq(1i64), fk_tests::user_id.eq(999i64)))
        .execute(&mut conn)
        .await
        .unwrap_err();
    assert!(
        matches!(err, Error::DatabaseError(_, _)),
        "expected DatabaseError, got {err:?}"
    );
    Ok(())
}
