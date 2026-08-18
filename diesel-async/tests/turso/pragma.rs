//! `PRAGMA` through the typed facility in `diesel::turso::pragma`.
//!
//! The unit test beside the module checks the text; this checks that the
//! text does something. `PRAGMA foreign_keys` is the one pragma the app's
//! stores depend on for correctness — whatsapp.db and slack.db both declare
//! `REFERENCES` constraints that are *only* enforced if the connection
//! turned this on — so the test worth having is a write that must fail
//! afterwards, not a string comparison.

use anyhow::Result;
use diesel::prelude::*;
use diesel_async::{AsyncConnection, RunQueryDsl, SimpleAsyncConnection};
use diesel_async::turso::TursoConnection;

diesel::table! {
    owners(id) {
        id -> Integer,
        name -> Text,
    }
}

diesel::table! {
    pets(id) {
        id -> Integer,
        owner_id -> Integer,
    }
}

async fn setup() -> Result<TursoConnection> {
    let mut conn = TursoConnection::establish(":memory:").await?;
    conn.batch_execute(
        "CREATE TABLE owners(id INTEGER PRIMARY KEY, name TEXT NOT NULL) STRICT;
         CREATE TABLE pets(id INTEGER PRIMARY KEY,
                           owner_id INTEGER NOT NULL REFERENCES owners(id)) STRICT;",
    )
    .await?;
    Ok(conn)
}

/// Without the pragma the `REFERENCES` is decoration — this is the state a
/// store lands in if the call is forgotten or misspelled.
#[tokio::test(flavor = "current_thread")]
async fn a_connection_without_the_pragma_accepts_an_orphan() -> Result<()> {
    let mut conn = setup().await?;

    let n = diesel::insert_into(pets::table)
        .values((pets::id.eq(1), pets::owner_id.eq(404)))
        .execute(&mut conn)
        .await?;
    assert_eq!(n, 1, "no owner 404 exists, and nothing objects");
    Ok(())
}

#[tokio::test(flavor = "current_thread")]
async fn the_pragma_makes_the_reference_enforced() -> Result<()> {
    let mut conn = setup().await?;
    diesel::turso::pragma::foreign_keys(true)
        .execute(&mut conn)
        .await?;

    let err = diesel::insert_into(pets::table)
        .values((pets::id.eq(1), pets::owner_id.eq(404)))
        .execute(&mut conn)
        .await
        .expect_err("owner 404 does not exist");
    assert!(
        format!("{err}").to_lowercase().contains("foreign key"),
        "expected a foreign-key violation, got: {err}"
    );

    // And a row with a real parent still goes in.
    diesel::insert_into(owners::table)
        .values((owners::id.eq(1), owners::name.eq("k")))
        .execute(&mut conn)
        .await?;
    diesel::insert_into(pets::table)
        .values((pets::id.eq(1), pets::owner_id.eq(1)))
        .execute(&mut conn)
        .await?;
    Ok(())
}

/// The `false` arm is not decoration either: it is what a migration that
/// rewrites a referenced table needs, and a facility that only spelled `ON`
/// would send that caller back to a raw string.
#[tokio::test(flavor = "current_thread")]
async fn the_pragma_can_be_turned_back_off() -> Result<()> {
    let mut conn = setup().await?;
    diesel::turso::pragma::foreign_keys(true)
        .execute(&mut conn)
        .await?;
    diesel::turso::pragma::foreign_keys(false)
        .execute(&mut conn)
        .await?;

    diesel::insert_into(pets::table)
        .values((pets::id.eq(1), pets::owner_id.eq(404)))
        .execute(&mut conn)
        .await?;
    Ok(())
}
