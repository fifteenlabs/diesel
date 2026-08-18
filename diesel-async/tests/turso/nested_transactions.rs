//! Turso doesn't support SAVEPOINT, so `TursoTransactionManager` folds
//! nested `.transaction()` calls into the outer. If an inner rollback
//! happens, the outer is "poisoned" — its commit emits `ROLLBACK`
//! instead of `COMMIT` and returns [`Error::RollbackTransaction`]. We
//! can't provide savepoint-style partial rollback, so we fail safe:
//! silently-swallowed inner failures can never commit.
//!
//! The writes below go through the typed DSL, so what each transaction did or
//! undid is stated in terms of the rows the app would actually have written.

use anyhow::Result;
use diesel::prelude::*;
use diesel::result::Error;
use diesel_async::{AsyncConnection, RunQueryDsl, SimpleAsyncConnection};
use scoped_futures::ScopedFutureExt;
use diesel_async::turso::TursoConnection;

diesel::table! {
    t (id) {
        id -> Integer,
    }
}

async fn connect() -> Result<TursoConnection> {
    let mut conn = TursoConnection::establish(":memory:").await?;
    conn.batch_execute("CREATE TABLE t(id INTEGER PRIMARY KEY) STRICT")
        .await?;
    Ok(conn)
}

/// The ids that survived, in order — the shape of a rollback is which rows
/// are left, not just how many.
async fn ids(conn: &mut TursoConnection) -> Result<Vec<i32>> {
    Ok(t::table.order(t::id.asc()).select(t::id).load(conn).await?)
}

async fn insert(conn: &mut TursoConnection, id: i32) -> QueryResult<usize> {
    diesel::insert_into(t::table)
        .values(t::id.eq(id))
        .execute(conn)
        .await
}

#[tokio::test(flavor = "current_thread")]
async fn inner_rollback_poisons_outer_commit() -> Result<()> {
    let mut conn = connect().await?;

    let outer = conn
        .transaction::<_, Error, _>(|c| {
            async move {
                insert(c, 1).await?;

                // Inner rolls back. Under savepoint semantics id=1 would
                // survive and id=2 would be gone; here the outer is
                // poisoned and *everything* will roll back on commit.
                let _ = c
                    .transaction::<(), Error, _>(|inner| {
                        async move {
                            insert(inner, 2).await?;
                            Err(Error::RollbackTransaction)
                        }
                        .scope_boxed()
                    })
                    .await;

                insert(c, 3).await?;
                Ok(())
            }
            .scope_boxed()
        })
        .await;

    assert!(matches!(outer, Err(Error::RollbackTransaction)));
    assert_eq!(ids(&mut conn).await?, Vec::<i32>::new());
    Ok(())
}

#[tokio::test(flavor = "current_thread")]
async fn outer_rollback_undoes_nested_work() -> Result<()> {
    let mut conn = connect().await?;

    let _ = conn
        .transaction::<(), Error, _>(|c| {
            async move {
                c.transaction::<_, Error, _>(|inner| {
                    async move {
                        insert(inner, 1).await?;
                        Ok(())
                    }
                    .scope_boxed()
                })
                .await?;
                Err(Error::RollbackTransaction)
            }
            .scope_boxed()
        })
        .await;

    assert_eq!(ids(&mut conn).await?, Vec::<i32>::new());
    Ok(())
}

#[tokio::test(flavor = "current_thread")]
async fn nested_all_success_commits() -> Result<()> {
    let mut conn = connect().await?;

    conn.transaction::<_, Error, _>(|c| {
        async move {
            insert(c, 1).await?;
            c.transaction::<_, Error, _>(|inner| {
                async move {
                    insert(inner, 2).await?;
                    Ok(())
                }
                .scope_boxed()
            })
            .await?;
            insert(c, 3).await?;
            Ok(())
        }
        .scope_boxed()
    })
    .await?;

    assert_eq!(ids(&mut conn).await?, vec![1, 2, 3]);
    Ok(())
}
