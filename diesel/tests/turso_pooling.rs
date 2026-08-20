//! `TursoConnection` behind a connection pool.
//!
//! What a pool adds over a long-lived connection is a *second caller*. That
//! is the whole subject of this file. A connection returned in a state its
//! holder could live with — mid-transaction, or with the transaction manager
//! unable to say what state the database is in — becomes someone else's
//! problem the moment the pool hands it out again, and the way it becomes
//! their problem is silent: their perfectly ordinary autocommit writes join
//! the transaction nobody is going to commit, return `Ok`, read back
//! correctly for as long as that connection lives, and are gone when it
//! closes.
//!
//! So the assertions here are mostly of one shape. Break a connection, give
//! it back, take another one, write through it, close everything, and reopen
//! the file. The row is either there — the pool retired the broken
//! connection — or it is not, and the pool handed over the corpse. Checking
//! only the connection that did the write would pass either way, for the
//! reason `tests/turso/transaction_recovery.rs` opens with.
//!
//! Both pool crates the backend can be driven by are exercised, because the
//! bounds on `impl ManageConnection`/`impl Manager` are only checked where
//! they are instantiated: `cargo check` with the features on proves nothing
//! until something names `TursoConnection` as the pooled type.

use anyhow::Result;
use diesel::async_dsl::RunQueryDsl;
use diesel::connection::{AsyncConnection, AsyncTransactionManager, SimpleAsyncConnection};
use diesel::pooled_connection::{
    AsyncDieselConnectionManager, ManagerConfig, PoolableConnection, RecyclingMethod,
};
use diesel::prelude::*;
use diesel::result::Error;
use diesel::turso::TursoConnection;
use scoped_futures::ScopedFutureExt;
use std::path::Path;
use std::time::Duration;

diesel::table! {
    t (id) {
        id -> Integer,
    }
}

/// A plain connection, used both to lay the schema down before a pool opens
/// on the file and to read it back after every pooled connection is gone.
async fn plain(path: &Path) -> Result<TursoConnection> {
    let url = path.to_str().expect("temp path is utf-8");
    Ok(TursoConnection::establish(url).await?)
}

async fn schema(path: &Path) -> Result<()> {
    let mut conn = plain(path).await?;
    conn.batch_execute("CREATE TABLE IF NOT EXISTS t(id INTEGER PRIMARY KEY) STRICT")
        .await?;
    Ok(())
}

async fn insert(conn: &mut TursoConnection, id: i32) -> QueryResult<usize> {
    diesel::insert_into(t::table)
        .values(t::id.eq(id))
        .execute(conn)
        .await
}

/// What is actually in the file, read on a connection that had nothing to do
/// with writing it.
async fn ids_on_disk(path: &Path) -> Result<Vec<i32>> {
    let mut conn = plain(path).await?;
    Ok(t::table
        .order(t::id.asc())
        .select(t::id)
        .load(&mut conn)
        .await?)
}

fn manager(
    path: &Path,
    recycling_method: RecyclingMethod<TursoConnection>,
) -> AsyncDieselConnectionManager<TursoConnection> {
    let url = path.to_str().expect("temp path is utf-8");
    let mut config = ManagerConfig::default();
    config.recycling_method = recycling_method;
    AsyncDieselConnectionManager::new_with_config(url, config)
}

/// Leave a transaction open on the driver without telling diesel.
///
/// [`TursoConnection::raw`] is the documented escape hatch for the UNION and
/// STRUCT queries the DSL cannot express, and this is the state it can leave
/// behind: `auto_commit` is false, the transaction manager's depth is zero,
/// and the two disagree. Diesel's manager cannot see it, which is exactly why
/// `is_broken` asks the engine as well.
async fn begin_behind_diesels_back(conn: &TursoConnection) -> Result<()> {
    conn.raw().execute_batch("BEGIN").await?;
    Ok(())
}

mod deadpool_backed {
    use super::*;
    use diesel::pooled_connection::deadpool::Pool;

    /// The ordinary case, and the one that pins `ping`.
    ///
    /// `Verified` is the default recycling method, and on the second checkout
    /// it runs `SELECT 1` through `execute` — a statement that returns a row,
    /// run for its row count. `execute_returning_count` used to refuse that
    /// shape with `Misuse("unexpected row during execution")`, raised after
    /// the statement had already run, so a pool built on this backend before
    /// that was fixed would have reported every healthy connection dead and
    /// churned a fresh connection per checkout. Nothing else in the suite
    /// runs that shape through a pool.
    #[tokio::test(flavor = "current_thread")]
    async fn a_healthy_connection_is_recycled_rather_than_replaced() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let path = dir.path().join("healthy.db");
        schema(&path).await?;

        let pool: Pool<TursoConnection> = Pool::builder(manager(&path, RecyclingMethod::Verified))
            .max_size(1)
            .build()?;

        {
            let mut conn = pool.get().await?;
            assert_eq!(insert(&mut conn, 1).await?, 1);
        }
        {
            // Taking it back out is what runs `recycle`, so this checkout is
            // the ping.
            let mut conn = pool.get().await?;
            assert_eq!(insert(&mut conn, 2).await?, 1);
        }

        assert_eq!(
            pool.status().size,
            1,
            "a healthy connection was thrown away and replaced"
        );
        drop(pool);
        assert_eq!(ids_on_disk(&path).await?, vec![1, 2]);
        Ok(())
    }

    /// A connection whose transaction future was cancelled must not reach the
    /// next caller.
    ///
    /// `BEGIN` ran, the callback never finished, and nothing left running
    /// knows whether the work was wanted — `Drop` cannot issue the `ROLLBACK`
    /// that would settle it, because that is an `await`. The transaction
    /// manager's job is to disclose that, and the pool's job is to act on the
    /// disclosure. See
    /// `tests/turso/transaction_recovery.rs`'s
    /// `a_cancelled_transaction_leaves_the_connection_reporting_broken`
    /// for the disclosure half on its own.
    ///
    /// The final assertion is the one with teeth. Had the pool handed the
    /// same connection back, the `1` would have gone into the abandoned
    /// transaction, and the file would be empty.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_cancelled_transaction_is_not_handed_to_the_next_caller() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let path = dir.path().join("cancelled.db");
        schema(&path).await?;

        let pool: Pool<TursoConnection> = Pool::builder(manager(&path, RecyclingMethod::Verified))
            .max_size(1)
            .build()?;

        {
            let mut conn = pool.get().await?;
            let cancelled = tokio::time::timeout(
                Duration::from_millis(50),
                conn.transaction::<(), Error, _>(|c| {
                    async move {
                        insert(c, 100).await?;
                        tokio::time::sleep(Duration::from_secs(30)).await;
                        Ok(())
                    }
                    .scope_boxed()
                }),
            )
            .await;
            assert!(cancelled.is_err(), "the timeout was supposed to fire");
            assert!(
                conn.is_broken(),
                "a connection holding an abandoned transaction must report broken"
            );
        }

        let mut conn = pool.get().await?;
        assert_eq!(insert(&mut conn, 1).await?, 1);
        drop(conn);
        drop(pool);

        assert_eq!(
            ids_on_disk(&path).await?,
            vec![1],
            "the write after the cancellation did not reach the file, so the pool \
             handed the cancelled connection to the next caller"
        );
        Ok(())
    }

    /// The half of `is_broken` that belongs to Turso rather than to diesel.
    ///
    /// The transaction manager is asked first and says the connection is
    /// fine, because by its own accounting it is: depth zero, no error, no
    /// cancelled critical block. Only the engine knows it is mid-transaction.
    /// The first assertion pins that the manager really does miss this, so
    /// that if the `is_autocommit` check is ever dropped the test fails for
    /// the reason it was written for rather than by coincidence.
    #[tokio::test(flavor = "current_thread")]
    async fn a_transaction_opened_behind_diesels_back_retires_the_connection() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let path = dir.path().join("orphan.db");
        schema(&path).await?;

        let pool: Pool<TursoConnection> = Pool::builder(manager(&path, RecyclingMethod::Verified))
            .max_size(1)
            .build()?;

        {
            let mut conn = pool.get().await?;
            begin_behind_diesels_back(&conn).await?;

            // Through the connection itself, not through the pool's wrapper:
            // an `Object` is an `AsyncConnection` in its own right, with its
            // own transaction manager type.
            let inner: &mut TursoConnection = &mut conn;
            assert!(
                !<TursoConnection as AsyncConnection>::TransactionManager
                    ::is_broken_transaction_manager(inner),
                "the transaction manager is not supposed to be able to see this one"
            );
            assert!(
                conn.is_broken(),
                "an open transaction the manager cannot see still makes the \
                 connection unfit to hand out"
            );
        }

        let mut conn = pool.get().await?;
        assert_eq!(insert(&mut conn, 1).await?, 1);
        drop(conn);
        drop(pool);

        assert_eq!(ids_on_disk(&path).await?, vec![1]);
        Ok(())
    }

    /// `Fast` skips the ping entirely, which leaves `is_broken` as the only
    /// check between a returned connection and the next caller.
    ///
    /// For a database that is a file in this process that is the recycling
    /// method that makes sense — there is no socket to have closed and no
    /// session for a server to have timed out — so it needs to be safe, and
    /// what makes it safe is that the check `Fast` keeps is the one that
    /// catches the failure that matters.
    #[tokio::test(flavor = "current_thread")]
    async fn fast_recycling_still_retires_a_broken_connection() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let path = dir.path().join("fast.db");
        schema(&path).await?;

        let pool: Pool<TursoConnection> = Pool::builder(manager(&path, RecyclingMethod::Fast))
            .max_size(1)
            .build()?;

        {
            let conn = pool.get().await?;
            begin_behind_diesels_back(&conn).await?;
        }

        let mut conn = pool.get().await?;
        assert_eq!(insert(&mut conn, 1).await?, 1);
        drop(conn);
        drop(pool);

        assert_eq!(ids_on_disk(&path).await?, vec![1]);
        Ok(())
    }
}

mod bb8_backed {
    use super::*;
    use diesel::pooled_connection::bb8::Pool;

    /// bb8 asks the same two questions deadpool does, just in different
    /// places — `has_broken` when the connection comes back, `is_valid`
    /// before it goes out again — so the same `PoolableConnection` impl has
    /// to serve it. This is here as much to instantiate
    /// `ManageConnection for AsyncDieselConnectionManager<TursoConnection>`
    /// as to assert anything: those bounds are checked at the use site and
    /// nowhere else.
    #[tokio::test(flavor = "current_thread")]
    async fn a_broken_connection_is_replaced_rather_than_reused() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let path = dir.path().join("bb8.db");
        schema(&path).await?;

        let pool: Pool<TursoConnection> = Pool::builder()
            .max_size(1)
            .build(manager(&path, RecyclingMethod::Verified))
            .await?;

        {
            let mut conn = pool.get().await?;
            assert_eq!(insert(&mut conn, 1).await?, 1);
            begin_behind_diesels_back(&conn).await?;
        }

        let mut conn = pool.get().await?;
        assert_eq!(insert(&mut conn, 2).await?, 1);
        drop(conn);
        drop(pool);

        // `1` was written before the stray `BEGIN`, so it was an autocommit
        // write and is durable; `2` proves the replacement connection is a
        // real one.
        assert_eq!(ids_on_disk(&path).await?, vec![1, 2]);
        Ok(())
    }
}
