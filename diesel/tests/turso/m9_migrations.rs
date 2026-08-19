//! M9 acceptance: diesel_migrations works over `TursoConnection` via
//! `diesel-async::AsyncMigrationHarness`. Two ordered migrations apply
//! in order, the second run is a no-op, and the resulting schema
//! accepts diesel-DSL CRUD.
//!
//! Requires the multi-threaded Tokio runtime: the async migration
//! harness wraps the sync diesel migration machinery via
//! `tokio::task::block_in_place`, which panics on `current_thread`.
//!
//! Nothing gates this module. It used to carry `#![cfg(feature =
//! "migrations")]`, and `diesel` has no `migrations` feature — so the cfg
//! was false in every build there has ever been and the module never
//! compiled, which is exactly the sort of thing a `cfg` on a whole file
//! hides. `diesel_migrations` is an unconditional dev-dependency (see
//! `Cargo.toml`), and it is here for this file, so there is nothing to gate
//! on.

use anyhow::Result;
use diesel::async_connection_wrapper::AsyncConnectionWrapper;
use diesel::async_dsl::RunQueryDsl;
use diesel::connection::AsyncConnection;
use diesel::deserialize::FromSqlRow;
use diesel::expression::AsExpression;
use diesel::prelude::*;
use diesel::turso::union::TaggedUnion;
use diesel::turso::TursoConnection;
use diesel::UnionSchema as DeriveUnionSchema;
use diesel_migrations::{embed_migrations, EmbeddedMigrations, MigrationHarness};

const MIGRATIONS: EmbeddedMigrations = embed_migrations!("tests/turso/m9_migrations");

#[derive(
    Debug,
    PartialEq,
    Clone,
    FromSqlRow,
    AsExpression,
    diesel::query_builder::QueryId,
    DeriveUnionSchema,
)]
#[diesel(sql_type = TaggedUnion<MessageData>)]
pub enum MessageData {
    Telegram {
        chat_id: i64,
        text: String,
    },
    Slack {
        channel_id_hash: i64,
        text: Option<String>,
    },
}

diesel::table! {
    use diesel::sql_types::*;
    use diesel::turso::union::TaggedUnion;
    use super::MessageData;

    messages(id) {
        id -> BigInt,
        data -> TaggedUnion<MessageData>,
    }
}

/// Run the migrations against a fresh in-memory DB and return the live
/// async connection. Uses `spawn_blocking` + `AsyncConnectionWrapper` so
/// the sync `MigrationHarness` can drive our async connection.
async fn fresh_migrated() -> Result<TursoConnection> {
    let conn = TursoConnection::establish(":memory:").await?;
    let mut sync_wrapper = AsyncConnectionWrapper::<TursoConnection>::from(conn);
    tokio::task::spawn_blocking(move || -> Result<TursoConnection> {
        let applied = sync_wrapper
            .run_pending_migrations(MIGRATIONS)
            .map_err(|e| anyhow::anyhow!("migration error: {e}"))?;
        assert_eq!(
            applied.len(),
            2,
            "both migrations should apply on first run"
        );

        // Second invocation is a no-op.
        let applied_again = sync_wrapper
            .run_pending_migrations(MIGRATIONS)
            .map_err(|e| anyhow::anyhow!("second migration error: {e}"))?;
        assert!(applied_again.is_empty(), "re-run should apply nothing");

        Ok(AsyncConnectionWrapper::into_inner(sync_wrapper))
    })
    .await?
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn migrations_apply_in_order_and_are_idempotent() -> Result<()> {
    let mut conn = fresh_migrated().await?;

    // Schema is live: insert + select via the DSL.
    for (id, data) in [
        (
            1i64,
            MessageData::Telegram {
                chat_id: -100,
                text: "hi".into(),
            },
        ),
        (
            2,
            MessageData::Slack {
                channel_id_hash: 777,
                text: Some("yo".into()),
            },
        ),
    ] {
        diesel::insert_into(messages::table)
            .values((messages::id.eq(id), messages::data.eq(data)))
            .execute(&mut conn)
            .await?;
    }
    let rows: Vec<(i64, MessageData)> = messages::table
        .order(messages::id.asc())
        .load(&mut conn)
        .await?;
    assert_eq!(
        rows,
        vec![
            (
                1,
                MessageData::Telegram {
                    chat_id: -100,
                    text: "hi".into()
                }
            ),
            (
                2,
                MessageData::Slack {
                    channel_id_hash: 777,
                    text: Some("yo".into())
                }
            ),
        ]
    );
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn migration_tracking_table_is_populated() -> Result<()> {
    let conn = fresh_migrated().await?;
    let mut rows = conn
        .raw()
        .query(
            "SELECT version FROM __diesel_schema_migrations ORDER BY version",
            (),
        )
        .await?;
    let mut versions = Vec::new();
    while let Some(r) = rows.next().await? {
        if let turso::Value::Text(v) = r.get_value(0)? {
            versions.push(v);
        }
    }
    assert_eq!(
        versions,
        vec!["20260413000000".to_string(), "20260413000001".to_string(),]
    );
    Ok(())
}
