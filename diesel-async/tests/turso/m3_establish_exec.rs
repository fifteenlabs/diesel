//! M3 acceptance: establish, batch_execute, execute_returning_count, and
//! the transaction manager round-trip real statements.
//!
//! `batch_execute` still gets the DDL — that is the interface for it. The DML
//! goes through the typed DSL, so `execute()`'s row count is checked against a
//! statement the compiler agreed matches the schema `batch_execute` just
//! created.

use anyhow::Result;
use diesel::prelude::*;
use diesel_async::{AsyncConnection, RunQueryDsl, SimpleAsyncConnection};
use scoped_futures::ScopedFutureExt;
use diesel_async::turso::TursoConnection;

diesel::table! {
    t (id) {
        id -> Integer,
        name -> Text,
    }
}

diesel::table! {
    tv (id) {
        id -> Integer,
        v -> Nullable<Text>,
    }
}

async fn count_t(conn: &mut TursoConnection) -> Result<i64> {
    Ok(t::table.count().get_result(conn).await?)
}

async fn count_tv(conn: &mut TursoConnection) -> Result<i64> {
    Ok(tv::table.count().get_result(conn).await?)
}

#[tokio::test(flavor = "current_thread")]
async fn establish_memory_and_ddl() -> Result<()> {
    let mut conn = TursoConnection::establish(":memory:").await?;
    conn.batch_execute(
        "CREATE TABLE t (id INTEGER PRIMARY KEY, name TEXT NOT NULL) STRICT;
         CREATE INDEX t_name ON t(name);",
    )
    .await?;
    assert_eq!(count_t(&mut conn).await?, 0);

    // Multi-row INSERT: one statement, three rows, one count back.
    let affected = diesel::insert_into(t::table)
        .values(&vec![
            (t::id.eq(1), t::name.eq("a")),
            (t::id.eq(2), t::name.eq("b")),
            (t::id.eq(3), t::name.eq("c")),
        ])
        .execute(&mut conn)
        .await?;
    assert_eq!(affected, 3);
    assert_eq!(count_t(&mut conn).await?, 3);

    // UPDATE / DELETE row counts.
    let updated = diesel::update(t::table.filter(t::id.eq(1)))
        .set(t::name.eq("A"))
        .execute(&mut conn)
        .await?;
    assert_eq!(updated, 1);

    let deleted = diesel::delete(t::table.filter(t::id.ge(2)))
        .execute(&mut conn)
        .await?;
    assert_eq!(deleted, 2);
    assert_eq!(count_t(&mut conn).await?, 1);

    // The one row left is the updated one — a count alone would not say so.
    let rows: Vec<(i32, String)> = t::table.select((t::id, t::name)).load(&mut conn).await?;
    assert_eq!(rows, vec![(1, "A".to_string())]);

    conn.batch_execute("DROP TABLE t").await?;
    Ok(())
}

#[tokio::test(flavor = "current_thread")]
async fn transaction_commit_and_rollback() -> Result<()> {
    let mut conn = TursoConnection::establish(":memory:").await?;
    conn.batch_execute("CREATE TABLE tv(id INTEGER PRIMARY KEY, v TEXT) STRICT")
        .await?;

    // Commit path: closure returns Ok.
    conn.transaction::<_, diesel::result::Error, _>(|c| {
        async move {
            diesel::insert_into(tv::table)
                .values(&vec![
                    (tv::id.eq(1), tv::v.eq("one")),
                    (tv::id.eq(2), tv::v.eq("two")),
                ])
                .execute(c)
                .await?;
            Ok(())
        }
        .scope_boxed()
    })
    .await?;
    assert_eq!(count_tv(&mut conn).await?, 2);

    // Rollback path: closure returns Err.
    let err = conn
        .transaction::<(), diesel::result::Error, _>(|c| {
            async move {
                diesel::insert_into(tv::table)
                    .values((tv::id.eq(3), tv::v.eq("three")))
                    .execute(c)
                    .await?;
                Err(diesel::result::Error::RollbackTransaction)
            }
            .scope_boxed()
        })
        .await
        .unwrap_err();
    assert!(matches!(err, diesel::result::Error::RollbackTransaction));
    assert_eq!(count_tv(&mut conn).await?, 2, "rollback should undo insert");
    let ids: Vec<i32> = tv::table
        .order(tv::id.asc())
        .select(tv::id)
        .load(&mut conn)
        .await?;
    assert_eq!(ids, vec![1, 2], "the rolled-back id must not be among them");

    Ok(())
}
