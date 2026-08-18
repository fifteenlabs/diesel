//! `TursoTransactionManager` — diesel_async [`TransactionManager`] that
//! emits `BEGIN` / `COMMIT` / `ROLLBACK` and folds nested
//! `.transaction()` calls into the outer.
//!
//! Turso doesn't support SAVEPOINTs (see `docs/manual.md` — "no
//! savepoints"), so we can't use diesel's `AnsiTransactionManager`
//! verbatim — it would emit `SAVEPOINT diesel_savepoint_N` for nested
//! calls and blow up at runtime.
//!
//! Nested transactions are a no-op — depth is tracked internally but no
//! additional SQL is emitted. If an inner callback returns `Err` the
//! outer transaction gets "poisoned": its `commit` will emit `ROLLBACK`
//! instead of `COMMIT` and return
//! [`diesel::result::Error::RollbackTransaction`]. This trades
//! savepoint-style partial rollback (which Turso can't offer) for a
//! guarantee that silently-swallowed inner failures can never commit.
//!
//! `BEGIN CONCURRENT` (MVCC snapshot isolation) would be a better
//! top-level choice — writers with disjoint row sets could commit in
//! parallel — but `PRAGMA journal_mode = 'mvcc'` currently breaks
//! Turso's UNION-referencing-STRUCT custom-type resolution. Revisit
//! once upstream fixes that.

use std::num::NonZeroU32;

use diesel::QueryResult;
use diesel::connection::{
    InstrumentationEvent, TransactionDepthChange, TransactionManagerStatus,
    ValidTransactionManagerStatus,
};
use diesel::result::Error;
use crate::{AsyncConnection, TransactionManager};

/// Turso signals "you told me to commit/rollback but I had no transaction
/// open" via a specific error string. Happens when an op only runs reads
/// after `BEGIN` — Turso's deferred transaction never latches, so `COMMIT`
/// legitimately has nothing to commit. We treat it as a soft success so a
/// benign no-op doesn't poison the connection's transaction manager.
fn is_no_transaction_error(e: &Error) -> bool {
    let Error::DatabaseError(_, info) = e else {
        return false;
    };
    let msg = info.message();
    msg.contains("no transaction is active")
}

/// Shared bookkeeping for the "batch_execute returned a no-transaction
/// error" fallback path: clear `poisoned`, decrement depth, leave the
/// status `Valid` so the next `begin_transaction` succeeds.
fn reset_after_no_transaction<Conn>(conn: &mut Conn) -> QueryResult<()>
where
    Conn: AsyncConnection<TransactionManager = TursoTransactionManager>,
{
    let state = tm(conn);
    state.poisoned = false;
    state
        .valid()?
        .change_transaction_depth(TransactionDepthChange::DecreaseDepth)
}

#[derive(Default, Debug)]
pub struct TursoTransactionManager {
    status: TransactionManagerStatus,
    /// Set when a nested rollback requested abort without an actual
    /// SQL `ROLLBACK`. Top-level commit must then emit `ROLLBACK`.
    poisoned: bool,
    /// Set on the soft-success branches when Turso's deferred transaction
    /// never latched (read-only `BEGIN`...`COMMIT`). Reset at every
    /// `begin_transaction`. Higher layers (the meta-DB writer task) read
    /// this via [`last_commit_did_not_latch`] to decide whether the no-op
    /// is benign for the op they submitted.
    last_commit_did_not_latch: bool,
}

impl TursoTransactionManager {
    /// `true` if the most recent `commit_transaction` / `rollback_transaction`
    /// took the soft-success branch — i.e. Turso reported no transaction was
    /// latched. Cleared by the next `begin_transaction`.
    pub fn last_commit_did_not_latch<Conn>(conn: &mut Conn) -> bool
    where
        Conn: AsyncConnection<TransactionManager = Self>,
    {
        tm(conn).last_commit_did_not_latch
    }
}

impl TursoTransactionManager {
    fn valid(&mut self) -> QueryResult<&mut ValidTransactionManagerStatus> {
        match &mut self.status {
            TransactionManagerStatus::Valid(v) => Ok(v),
            TransactionManagerStatus::InError => Err(Error::BrokenTransactionManager),
        }
    }
}

fn tm<Conn>(conn: &mut Conn) -> &mut TursoTransactionManager
where
    Conn: AsyncConnection<TransactionManager = TursoTransactionManager>,
{
    conn.transaction_state()
}

impl<Conn> TransactionManager<Conn> for TursoTransactionManager
where
    Conn: AsyncConnection<TransactionManager = Self>,
{
    type TransactionStateData = Self;

    async fn begin_transaction(conn: &mut Conn) -> QueryResult<()> {
        let depth_before = tm(conn).valid()?.transaction_depth();
        let new_depth = NonZeroU32::new(depth_before.map_or(0, NonZeroU32::get).saturating_add(1))
            .expect("new depth is at least 1");
        conn.instrumentation()
            .on_connection_event(InstrumentationEvent::begin_transaction(new_depth));
        if depth_before.is_none() {
            tm(conn).last_commit_did_not_latch = false;
            conn.batch_execute("BEGIN").await?;
        }
        tm(conn)
            .valid()?
            .change_transaction_depth(TransactionDepthChange::IncreaseDepth)?;
        Ok(())
    }

    async fn rollback_transaction(conn: &mut Conn) -> QueryResult<()> {
        let depth = tm(conn)
            .valid()?
            .transaction_depth()
            .ok_or(Error::NotInTransaction)?;
        conn.instrumentation()
            .on_connection_event(InstrumentationEvent::rollback_transaction(depth));
        if depth.get() == 1 {
            match conn.batch_execute("ROLLBACK").await {
                Ok(()) => {
                    let state = tm(conn);
                    state.poisoned = false;
                    state
                        .valid()?
                        .change_transaction_depth(TransactionDepthChange::DecreaseDepth)?;
                    Ok(())
                }
                Err(e) if is_no_transaction_error(&e) => {
                    tracing::debug!(
                        error = %e,
                        "Turso reports no active transaction at rollback — treating as no-op"
                    );
                    tm(conn).last_commit_did_not_latch = true;
                    reset_after_no_transaction(conn)
                }
                Err(e) => {
                    tm(conn).status.set_in_error();
                    Err(e)
                }
            }
        } else {
            let state = tm(conn);
            state.poisoned = true;
            state
                .valid()?
                .change_transaction_depth(TransactionDepthChange::DecreaseDepth)?;
            Ok(())
        }
    }

    async fn commit_transaction(conn: &mut Conn) -> QueryResult<()> {
        let state = tm(conn);
        let depth = state
            .valid()?
            .transaction_depth()
            .ok_or(Error::NotInTransaction)?;
        let poisoned = state.poisoned;
        conn.instrumentation()
            .on_connection_event(InstrumentationEvent::commit_transaction(depth));
        if depth.get() == 1 {
            let sql = if poisoned { "ROLLBACK" } else { "COMMIT" };
            match conn.batch_execute(sql).await {
                Ok(()) => {
                    let state = tm(conn);
                    state.poisoned = false;
                    state
                        .valid()?
                        .change_transaction_depth(TransactionDepthChange::DecreaseDepth)?;
                    if poisoned {
                        Err(Error::RollbackTransaction)
                    } else {
                        Ok(())
                    }
                }
                Err(e) if is_no_transaction_error(&e) => {
                    tracing::debug!(
                        error = %e,
                        sql,
                        "Turso did not latch a transaction for this op; treating {sql} as no-op",
                    );
                    tm(conn).last_commit_did_not_latch = true;
                    reset_after_no_transaction(conn)?;
                    if poisoned {
                        Err(Error::RollbackTransaction)
                    } else {
                        Ok(())
                    }
                }
                Err(e) => {
                    tm(conn).status.set_in_error();
                    Err(e)
                }
            }
        } else {
            tm(conn)
                .valid()?
                .change_transaction_depth(TransactionDepthChange::DecreaseDepth)?;
            Ok(())
        }
    }

    fn transaction_manager_status_mut(conn: &mut Conn) -> &mut TransactionManagerStatus {
        &mut tm(conn).status
    }
}
