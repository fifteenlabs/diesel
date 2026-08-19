//! `PRAGMA foreign_keys`, from every door that establishes a connection.
//!
//! This is the one pragma the app's stores depend on for correctness —
//! whatsapp.db and slack.db both declare `REFERENCES` constraints that are
//! *only* enforced if the connection turned it on — so the test worth
//! having is a write that must fail, not a string comparison.
//!
//! Enforcement is part of establishing a connection rather than something
//! each store remembers, and it is not a choice any of them get to make:
//! both doors enforce, which is what these tests pin down. There used to be
//! a third that left foreign keys off for schema migrations; a migration
//! that needs them off now says so in its own SQL, so which state it runs
//! under no longer depends on how its connection was opened. A misspelling
//! of the pragma text in `TursoConnection::open` — `foreign_key` singular is
//! a silent no-op, not an error — lands here as well.

use anyhow::Result;
use diesel::async_dsl::RunQueryDsl;
use diesel::connection::{AsyncConnection, SimpleAsyncConnection};
use diesel::prelude::*;
use diesel::turso::TursoConnection;

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

const SCHEMA: &str = "CREATE TABLE owners(id INTEGER PRIMARY KEY, name TEXT NOT NULL) STRICT;
     CREATE TABLE pets(id INTEGER PRIMARY KEY,
                       owner_id INTEGER NOT NULL REFERENCES owners(id)) STRICT;";

async fn setup() -> Result<TursoConnection> {
    let mut conn = TursoConnection::establish(":memory:").await?;
    conn.batch_execute(SCHEMA).await?;
    Ok(conn)
}

/// The single-process door enforces too. It is the one signal.db and
/// whatsapp.db open through, and it differs from `establish` only in the WAL
/// mode — a difference that must not quietly become a difference in what the
/// database accepts.
#[tokio::test(flavor = "current_thread")]
async fn the_single_process_door_enforces_references() -> Result<()> {
    let mut conn = TursoConnection::establish_single_process(":memory:").await?;
    conn.batch_execute(SCHEMA).await?;

    let err = diesel::insert_into(pets::table)
        .values((pets::id.eq(1), pets::owner_id.eq(404)))
        .execute(&mut conn)
        .await
        .expect_err("owner 404 does not exist");
    assert!(
        format!("{err}").to_lowercase().contains("foreign key"),
        "expected a foreign-key violation, got: {err}"
    );
    Ok(())
}

/// A migration that needs enforcement off turns it off itself, and gets it —
/// which is the whole reason the connection no longer has to.
#[tokio::test(flavor = "current_thread")]
async fn a_connection_can_still_turn_enforcement_off_for_itself() -> Result<()> {
    let mut conn = setup().await?;
    conn.batch_execute("PRAGMA foreign_keys = OFF").await?;

    let n = diesel::insert_into(pets::table)
        .values((pets::id.eq(1), pets::owner_id.eq(404)))
        .execute(&mut conn)
        .await?;
    assert_eq!(n, 1, "no owner 404 exists, and nothing objects");
    Ok(())
}

/// Establishing a connection enforces, without the caller asking. This is
/// the invariant every store depends on and none of them spells out.
#[tokio::test(flavor = "current_thread")]
async fn establishing_a_connection_enforces_references() -> Result<()> {
    let mut conn = setup().await?;

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
