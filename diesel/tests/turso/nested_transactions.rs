//! Nested `.transaction()` on Turso, which is ordinary ANSI savepoint
//! nesting — `BEGIN` at the top, `SAVEPOINT diesel_savepoint_N` beneath it.
//!
//! It was not always. The backend used to carry a hand-rolled transaction
//! manager that folded every nested call into the outer one, on the stated
//! grounds that "Turso doesn't support SAVEPOINTs". That is no longer true
//! — `SAVEPOINT`, `ROLLBACK TO SAVEPOINT` and `RELEASE SAVEPOINT` are all
//! implemented on the revision this crate pins — and the folding was not a
//! harmless simplification: an inner rollback poisoned the outer
//! transaction, so a caller who caught an inner failure and carried on lost
//! the outer transaction's work too, including the work it had done before
//! the inner block began. The first test below is the one that changed
//! meaning, and it is the meaning every other diesel backend has.
//!
//! The writes below go through the typed DSL, so what each transaction did or
//! undid is stated in terms of the rows the app would actually have written.

use anyhow::Result;
use diesel::async_dsl::RunQueryDsl;
use diesel::connection::{AsyncConnection, SimpleAsyncConnection};
use diesel::prelude::*;
use diesel::result::Error;
use diesel::turso::TursoConnection;
use scoped_futures::ScopedFutureExt;

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

/// An inner rollback undoes the inner block and nothing else.
///
/// Under the folding manager this test asserted the opposite: the outer
/// commit came back `Err(RollbackTransaction)` and *all three* rows were
/// gone, id 1 included — a row written before the inner transaction existed.
/// A caller that treats a failed sub-operation as recoverable, which is the
/// whole reason to open an inner transaction, silently lost everything
/// around it.
#[tokio::test(flavor = "current_thread")]
async fn inner_rollback_undoes_only_the_inner_block() -> Result<()> {
    let mut conn = connect().await?;

    conn.transaction::<_, Error, _>(|c| {
        async move {
            insert(c, 1).await?;

            // Inner rolls back to its savepoint: id 2 goes, id 1 stays, and
            // the outer transaction is still live and still committable.
            let inner = c
                .transaction::<(), Error, _>(|inner| {
                    async move {
                        insert(inner, 2).await?;
                        Err(Error::RollbackTransaction)
                    }
                    .scope_boxed()
                })
                .await;
            assert!(matches!(inner, Err(Error::RollbackTransaction)));

            insert(c, 3).await?;
            Ok(())
        }
        .scope_boxed()
    })
    .await?;

    assert_eq!(ids(&mut conn).await?, vec![1, 3]);
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

/// Three levels deep, with the middle one rolled back: the savepoint stack
/// has to unwind to the right level rather than to the nearest one.
#[tokio::test(flavor = "current_thread")]
async fn a_middle_level_rollback_keeps_the_levels_around_it() -> Result<()> {
    let mut conn = connect().await?;

    conn.transaction::<_, Error, _>(|c| {
        async move {
            insert(c, 1).await?;
            let middle = c
                .transaction::<(), Error, _>(|m| {
                    async move {
                        insert(m, 2).await?;
                        m.transaction::<_, Error, _>(|inner| {
                            async move {
                                insert(inner, 3).await?;
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
            assert!(matches!(middle, Err(Error::RollbackTransaction)));
            insert(c, 4).await?;
            Ok(())
        }
        .scope_boxed()
    })
    .await?;

    // 2 and 3 were both inside the rolled-back middle level; 1 preceded it
    // and 4 followed it.
    assert_eq!(ids(&mut conn).await?, vec![1, 4]);
    Ok(())
}
