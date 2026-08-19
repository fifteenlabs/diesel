//! What the connection is left holding when a transaction goes wrong, and
//! whether anything written afterwards actually reaches the file.
//!
//! Nesting is [`super::nested_transactions`]'s subject. This one is about
//! the two ways a transaction can end other than by committing — the
//! database refusing the `COMMIT`, and the caller dropping the future
//! mid-flight — because on Turso both of them leave a real, open write
//! transaction behind, and the interesting question is what the *next*
//! statement on that connection does.
//!
//! Every assertion here is made twice: once against the connection that did
//! the work, and once against a second connection opened on the same file
//! afterwards. That is deliberate. Writes stranded in an abandoned
//! transaction read back perfectly on the connection holding it — they
//! really are there — and vanish when it closes. A test that only checked
//! the first connection would have passed against every bug this file
//! exists for.

use anyhow::Result;
use diesel::async_dsl::RunQueryDsl;
use diesel::connection::{AsyncConnection, AsyncTransactionManager, SimpleAsyncConnection};
use diesel::prelude::*;
use diesel::result::Error;
use diesel::turso::TursoConnection;
use scoped_futures::ScopedFutureExt;
use std::time::Duration;

diesel::table! {
    t (id) {
        id -> Integer,
    }
}

/// A file-backed database, because the whole point is to close a connection
/// and open another one on the same bytes.
async fn connect(path: &std::path::Path) -> Result<TursoConnection> {
    let url = path.to_str().expect("temp path is utf-8");
    let mut conn = TursoConnection::establish(url).await?;
    conn.batch_execute(
        "CREATE TABLE IF NOT EXISTS parent(id INTEGER PRIMARY KEY) STRICT;\
         CREATE TABLE IF NOT EXISTS child(\
             id INTEGER PRIMARY KEY,\
             p INTEGER REFERENCES parent(id) DEFERRABLE INITIALLY DEFERRED\
         ) STRICT;\
         CREATE TABLE IF NOT EXISTS t(id INTEGER PRIMARY KEY) STRICT;",
    )
    .await?;
    conn.batch_execute("PRAGMA foreign_keys = ON").await?;
    Ok(conn)
}

async fn insert(conn: &mut TursoConnection, id: i32) -> QueryResult<usize> {
    diesel::insert_into(t::table)
        .values(t::id.eq(id))
        .execute(conn)
        .await
}

async fn ids(conn: &mut TursoConnection) -> Result<Vec<i32>> {
    Ok(t::table.order(t::id.asc()).select(t::id).load(conn).await?)
}

/// A `COMMIT` the database refuses must still end the transaction, so the
/// connection can go on being used.
///
/// Turso leaves the transaction open when a deferred foreign key fails at
/// commit time — `TxOp::Commit` returns before clearing `auto_commit`, on
/// purpose, so the caller can choose. The transaction manager has to make
/// that choice, and the only correct one is `ROLLBACK`: the commit was
/// refused, so there is nothing to keep, and leaving it open means the next
/// statement joins a transaction that will never be committed.
///
/// This is what the failure looked like before: three plain inserts after
/// the failed commit each returned `Ok(1)`, read back as `[1, 2, 3]` on that
/// connection, and were `[]` on the next one. Nothing anywhere returned an
/// error. For a process holding one long-lived connection — how the app uses
/// its meta database — one deferred-FK violation was enough to make every
/// subsequent write disappear at shutdown.
#[tokio::test(flavor = "current_thread")]
async fn a_refused_commit_ends_the_transaction_and_the_connection_still_works() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let path = dir.path().join("refused-commit.db");
    let mut conn = connect(&path).await?;

    let failed = conn
        .transaction::<(), Error, _>(|c| {
            async move {
                // Passes the immediate check; fails the deferred one at COMMIT.
                c.batch_execute("INSERT INTO child(id, p) VALUES (1, 999)")
                    .await?;
                Ok(())
            }
            .scope_boxed()
        })
        .await;
    let Err(Error::DatabaseError(_, info)) = &failed else {
        anyhow::bail!("expected the deferred FK to refuse the commit, got {failed:?}");
    };
    assert!(
        info.message().contains("deferred foreign key"),
        "unexpected commit error: {}",
        info.message()
    );

    // The transaction is over as far as both sides are concerned, so these
    // are ordinary autocommit writes.
    for id in 1..=3 {
        assert_eq!(insert(&mut conn, id).await?, 1);
    }
    assert_eq!(ids(&mut conn).await?, vec![1, 2, 3]);
    drop(conn);

    let mut reopened = connect(&path).await?;
    assert_eq!(
        ids(&mut reopened).await?,
        vec![1, 2, 3],
        "writes made after a refused commit did not reach the file"
    );
    Ok(())
}

/// And the transaction the refused commit was carrying is gone, not
/// half-applied: the row that violated the constraint must not be in the
/// file either.
#[tokio::test(flavor = "current_thread")]
async fn a_refused_commit_keeps_none_of_its_own_work() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let path = dir.path().join("refused-commit-atomic.db");
    let mut conn = connect(&path).await?;

    let failed = conn
        .transaction::<(), Error, _>(|c| {
            async move {
                insert(c, 1).await?;
                c.batch_execute("INSERT INTO child(id, p) VALUES (1, 999)")
                    .await?;
                Ok(())
            }
            .scope_boxed()
        })
        .await;
    assert!(failed.is_err());

    assert_eq!(ids(&mut conn).await?, Vec::<i32>::new());
    drop(conn);

    let mut reopened = connect(&path).await?;
    assert_eq!(ids(&mut reopened).await?, Vec::<i32>::new());
    Ok(())
}

/// A connection whose transaction future was dropped mid-flight reports
/// itself broken, which is the signal a pool retires it on.
///
/// There is no way to rescue such a connection from inside the transaction
/// manager. `BEGIN` reached the database, the callback did not finish, and
/// nothing left running knows whether the caller wanted the work kept — and
/// `Drop` cannot run the `ROLLBACK` that would settle it, because that is an
/// `await`. What the manager owes the caller is therefore not recovery but
/// *disclosure*: say the connection is not fit to hand out again.
///
/// So this test pins the disclosure. Note what it does not claim: the rows
/// written after the cancellation are not in the file at the end, and cannot
/// be, because they went into the transaction the cancelled future left
/// open. That is the residue of an abandoned transaction, not a bug in the
/// manager, and the answer to it is to drop the connection — which is what
/// `is_broken_transaction_manager` is telling the caller to do.
///
/// The cancellation here lands in the callback, so what reports it is the
/// leftover depth. The ANSI manager also covers the case a depth count
/// cannot see — a drop while `BEGIN` or `COMMIT` is itself in flight, which
/// leaves the depth exactly where it was — by setting a flag before each of
/// its own awaits and clearing it after, so a drop in between leaves it set
/// for good. That case has no test because it has no repro on a local file:
/// Turso answers every statement without yielding, so a transaction against
/// one either completes on the first poll or does not start.
#[tokio::test(flavor = "multi_thread")]
async fn a_cancelled_transaction_leaves_the_connection_reporting_broken() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let path = dir.path().join("cancelled.db");
    let mut conn = connect(&path).await?;

    let cancelled = tokio::time::timeout(
        Duration::from_millis(50),
        conn.transaction::<(), Error, _>(|c| {
            async move {
                insert(c, 100).await?;
                // Outlives the timeout, so the transaction future is dropped
                // between `BEGIN` and any `COMMIT`/`ROLLBACK`.
                tokio::time::sleep(Duration::from_secs(30)).await;
                Ok(())
            }
            .scope_boxed()
        }),
    )
    .await;
    assert!(cancelled.is_err(), "the timeout was supposed to fire");

    assert!(
        <TursoConnection as AsyncConnection>::TransactionManager::is_broken_transaction_manager(
            &mut conn
        ),
        "a connection left holding an abandoned transaction must report broken"
    );
    Ok(())
}

/// A read-only transaction commits cleanly.
///
/// Worth pinning because the manager this replaced carried a whole
/// soft-success branch for the belief that Turso's deferred transaction
/// would not latch for a block that never wrote, and that `COMMIT` would
/// come back with "cannot commit - no transaction is active". `BEGIN` clears
/// `auto_commit` unconditionally, so it does latch and the branch was dead
/// code — but it was dead code that a consumer read a diagnostic out of, so
/// the fact that there is nothing to diagnose belongs in the suite rather
/// than in a commit message.
#[tokio::test(flavor = "current_thread")]
async fn a_transaction_that_only_reads_commits() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let path = dir.path().join("read-only.db");
    let mut conn = connect(&path).await?;
    insert(&mut conn, 1).await?;

    let seen = conn
        .transaction::<Vec<i32>, Error, _>(|c| {
            async move { t::table.select(t::id).load(c).await }.scope_boxed()
        })
        .await?;
    assert_eq!(seen, vec![1]);

    // And the connection is still in a state that can open the next one.
    conn.transaction::<_, Error, _>(|c| async move { insert(c, 2).await }.scope_boxed())
        .await?;
    assert_eq!(ids(&mut conn).await?, vec![1, 2]);
    Ok(())
}
