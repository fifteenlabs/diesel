//! The startup probe: does it actually refuse the databases it should?
//!
//! The check it performs is only worth its place in the connect path if it
//! catches the case that motivates it — a file whose `CREATE TYPE` ran
//! under a different build of the same enum. That case cannot be produced
//! by editing Rust in a test, so it is produced from the other side: the
//! database is declared with a deliberately wrong layout and the same
//! enum is asked to open it.

use anyhow::Result;
use diesel::connection::{AsyncConnection, SimpleAsyncConnection};
use diesel::deserialize::FromSqlRow;
use diesel::expression::AsExpression;
use diesel::turso::probe::verify_declared_types;
use diesel::turso::union::{TaggedUnion, UnionSchema};
use diesel::turso::TursoConnection;
use diesel::UnionSchema as DeriveUnionSchema;

#[derive(
    Debug,
    PartialEq,
    Clone,
    FromSqlRow,
    AsExpression,
    diesel::query_builder::QueryId,
    DeriveUnionSchema,
)]
#[diesel(sql_type = TaggedUnion<Person>)]
#[union(name = "person")]
pub enum Person {
    #[union(struct_type = "person_user")]
    User {
        first_name: String,
        last_name: String,
    },
    #[union(struct_type = "person_bot")]
    Bot { handle: String },
}

async fn open_with(ddl: &str) -> Result<TursoConnection> {
    let mut conn = TursoConnection::establish(":memory:").await?;
    conn.batch_execute(ddl).await?;
    Ok(conn)
}

fn types() -> &'static [diesel::turso::probe::DeclaredType] {
    diesel::declared_types![Person]
}

#[tokio::test(flavor = "current_thread")]
async fn a_database_the_derive_wrote_passes() -> Result<()> {
    let conn = open_with(&Person::create_type_sql()).await?;
    verify_declared_types(&conn, types(), &[])
        .await
        .map_err(|e| anyhow::anyhow!("{e}"))?;
    Ok(())
}

/// Superseded versions of a type stay in the file forever — Turso refuses
/// `DROP TYPE` while any column names it — so extra declarations must not
/// be treated as drift.
#[tokio::test(flavor = "current_thread")]
async fn unrelated_and_superseded_declarations_are_ignored() -> Result<()> {
    let ddl = format!(
        "CREATE TYPE person_v0 AS UNION(user person_user_v0); \
         CREATE TYPE person_user_v0 AS STRUCT(name TEXT); {}",
        Person::create_type_sql()
    );
    let conn = open_with(&ddl).await?;
    verify_declared_types(&conn, types(), &[])
        .await
        .map_err(|e| anyhow::anyhow!("{e}"))?;
    Ok(())
}

/// The case the probe exists for, and the one no compiler and no golden
/// test can see: two same-typed fields swapped in a file that already
/// exists. Every row decodes, with the two values exchanged.
#[tokio::test(flavor = "current_thread")]
async fn swapped_same_typed_fields_are_refused() -> Result<()> {
    let conn = open_with(
        "CREATE TYPE person_user AS STRUCT(last_name TEXT, first_name TEXT);
         CREATE TYPE person_bot AS STRUCT(handle TEXT);
         CREATE TYPE person AS UNION(user person_user, bot person_bot);",
    )
    .await?;
    let err = verify_declared_types(&conn, types(), &[])
        .await
        .expect_err("a swapped field layout must not open");
    let msg = err.to_string();
    assert!(msg.contains("person_user"), "{msg}");
    // The message shows both layouts, so the reader can see which two
    // fields moved rather than only that something did.
    assert!(msg.contains("first_name text, last_name text"), "{msg}");
    assert!(msg.contains("last_name text, first_name text"), "{msg}");
    Ok(())
}

/// The other half of the same coupling: variant order, where the payload
/// shapes are compatible enough that nothing downstream would notice.
#[tokio::test(flavor = "current_thread")]
async fn swapped_variant_order_is_refused() -> Result<()> {
    let conn = open_with(
        "CREATE TYPE person_user AS STRUCT(first_name TEXT, last_name TEXT);
         CREATE TYPE person_bot AS STRUCT(handle TEXT);
         CREATE TYPE person AS UNION(bot person_bot, user person_user);",
    )
    .await?;
    let err = verify_declared_types(&conn, types(), &[])
        .await
        .expect_err("a swapped variant order must not open");
    assert!(err.to_string().contains("person"), "{err}");
    Ok(())
}

#[tokio::test(flavor = "current_thread")]
async fn a_database_missing_the_type_is_refused() -> Result<()> {
    let conn = open_with("CREATE TABLE t(a INT)").await?;
    let err = verify_declared_types(&conn, types(), &[])
        .await
        .expect_err("a database without the type must not open");
    assert!(err.to_string().contains("not declared"), "{err}");
    Ok(())
}

/// A waiver silences exactly the type it names and nothing else.
#[tokio::test(flavor = "current_thread")]
async fn a_waiver_covers_only_the_type_it_names() -> Result<()> {
    let conn = open_with(
        "CREATE TYPE person_user AS STRUCT(first_name TEXT, last_name TEXT, extra INT);
         CREATE TYPE person_bot AS STRUCT(handle TEXT);
         CREATE TYPE person AS UNION(user person_user, bot person_bot);",
    )
    .await?;
    verify_declared_types(&conn, types(), &["person_user"])
        .await
        .map_err(|e| anyhow::anyhow!("waived type still reported: {e}"))?;
    verify_declared_types(&conn, types(), &["person_bot"])
        .await
        .expect_err("waiving a different type must not silence this one");
    Ok(())
}

/// The probe runs on every connection in release builds, so its cost is
/// part of startup. Measured against a type registry the size of the meta
/// DB's — every superseded union version is still in there, plus Turso's
/// fifteen built-in domains.
#[tokio::test(flavor = "current_thread")]
async fn cost_at_meta_db_scale() -> Result<()> {
    let mut ddl = Person::create_type_sql();
    for i in 0..60 {
        ddl.push_str(&format!(
            "; CREATE TYPE filler_{i} AS STRUCT(a INT, b TEXT, c BLOB, d REAL)"
        ));
    }
    let conn = open_with(&ddl).await?;

    // One warm pass so the measurement is the steady-state cost, not the
    // first compile of the vtab query.
    verify_declared_types(&conn, types(), &[])
        .await
        .map_err(|e| anyhow::anyhow!("{e}"))?;

    const RUNS: u32 = 50;
    let started = std::time::Instant::now();
    for _ in 0..RUNS {
        verify_declared_types(&conn, types(), &[])
            .await
            .map_err(|e| anyhow::anyhow!("{e}"))?;
    }
    let per_run = started.elapsed() / RUNS;
    println!("probe over 62 declared types: {per_run:?} per connection");
    assert!(
        per_run < std::time::Duration::from_millis(5),
        "the probe is on the connect path; {per_run:?} is too much to hide"
    );
    Ok(())
}
