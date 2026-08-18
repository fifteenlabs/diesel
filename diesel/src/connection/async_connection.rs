//! The async half of [`Connection`](super::Connection).
//!
//! These are the traits a driver implements when its io is async: they
//! mirror [`SimpleConnection`](super::SimpleConnection) and
//! [`Connection`](super::Connection) method for method, and differ only in
//! returning futures. They live beside their sync counterparts rather than
//! under an `async` module because they *are* the same concept — a
//! connection — and a reader looking for "how do I open a connection" should
//! find both in one place.
//!
//! Everything here was `diesel-async`'s crate root before that crate was
//! folded in. Two names changed on the way, and both changes were the point
//! of moving rather than incidental to it:
//!
//! - `TransactionManager` became [`AsyncTransactionManager`] and
//!   `AnsiTransactionManager` became
//!   [`AnsiAsyncTransactionManager`](super::AnsiAsyncTransactionManager),
//!   because the sync traits of those names are declared two modules up and
//!   two traits cannot share a path.
//! - `RunQueryDsl` became
//!   [`AsyncRunQueryDsl`](crate::async_dsl::RunQueryDsl). That one was
//!   not forced — the traits were in different crates and could have kept the
//!   name — and it is the more valuable of the two. While both existed, every
//!   file that used the async trait wrote `use diesel::prelude::*;`
//!   *under* a `use diesel::prelude::*;` and relied on the explicit import
//!   shadowing the glob. That works, silently, until someone writes the glob
//!   after the import or forgets the import in a new file, at which point the
//!   sync trait resolves, the call is not awaited, and the query is built and
//!   dropped. Renaming means both can sit in [`crate::prelude`] and the
//!   compiler picks the one whose name you wrote.

use std::fmt::Debug;
use std::future::Future;

use futures_core::future::BoxFuture;
use futures_core::Stream;
use futures_util::FutureExt;
use scoped_futures::{ScopedBoxFuture, ScopedFutureExt};

use super::{AsyncTransactionManager, CacheSize, Instrumentation};
use crate::backend::Backend;
use crate::query_builder::{AsQuery, QueryFragment, QueryId};
use crate::row::Row;
use crate::{ConnectionResult, QueryResult};

/// Perform simple operations on a backend.
///
/// You should likely use [`AsyncConnection`] instead.
pub trait SimpleAsyncConnection {
    /// Execute multiple SQL statements within the same string.
    ///
    /// This function is used to execute migrations,
    /// which may contain more than one SQL statement.
    fn batch_execute(&mut self, query: &str) -> impl Future<Output = QueryResult<()>> + Send;
}

/// Core trait for an async database connection
pub trait AsyncConnectionCore: SimpleAsyncConnection + Send {
    /// The future returned by `AsyncConnection::execute`
    type ExecuteFuture<'conn, 'query>: Future<Output = QueryResult<usize>> + Send;
    /// The future returned by `AsyncConnection::load`
    type LoadFuture<'conn, 'query>: Future<Output = QueryResult<Self::Stream<'conn, 'query>>> + Send;
    /// The inner stream returned by `AsyncConnection::load`
    type Stream<'conn, 'query>: Stream<Item = QueryResult<Self::Row<'conn, 'query>>> + Send;
    /// The row type used by the stream returned by `AsyncConnection::load`
    type Row<'conn, 'query>: Row<'conn, Self::Backend>;

    /// The backend this type connects to
    type Backend: Backend;

    #[doc(hidden)]
    fn load<'conn, 'query, T>(&'conn mut self, source: T) -> Self::LoadFuture<'conn, 'query>
    where
        T: AsQuery + 'query,
        T::Query: QueryFragment<Self::Backend> + QueryId + 'query;

    #[doc(hidden)]
    fn execute_returning_count<'conn, 'query, T>(
        &'conn mut self,
        source: T,
    ) -> Self::ExecuteFuture<'conn, 'query>
    where
        T: QueryFragment<Self::Backend> + QueryId + 'query;

    // These functions allow the associated types (`ExecuteFuture`, `LoadFuture`, etc.) to
    // compile without a `where Self: '_` clause. This is needed the because bound causes
    // lifetime issues when using `transaction()` with generic `AsyncConnection`s.
    //
    // See: https://github.com/rust-lang/rust/issues/87479
    #[doc(hidden)]
    fn _silence_lint_on_execute_future(_: Self::ExecuteFuture<'_, '_>) {}
    #[doc(hidden)]
    fn _silence_lint_on_load_future(_: Self::LoadFuture<'_, '_>) {}
}

/// An async connection to a database
///
/// This trait represents an async database connection. It can be used to query the database through
/// the query dsl provided by diesel, custom extensions or raw sql queries. It essentially mirrors
/// the sync diesel [`Connection`](diesel::connection::Connection) implementation
pub trait AsyncConnection: AsyncConnectionCore + Sized {
    #[doc(hidden)]
    type TransactionManager: AsyncTransactionManager<Self>;

    /// Establishes a new connection to the database
    ///
    /// The argument to this method and the method's behavior varies by backend.
    /// See the documentation for that backend's connection class
    /// for details about what it accepts and how it behaves.
    fn establish(database_url: &str) -> impl Future<Output = ConnectionResult<Self>> + Send;

    /// Executes the given function inside of a database transaction
    ///
    /// This function executes the provided closure `f` inside a database
    /// transaction. If there is already an open transaction for the current
    /// connection savepoints will be used instead. The connection is committed if
    /// the closure returns `Ok(_)`, it will be rolled back if it returns `Err(_)`.
    /// For both cases the original result value will be returned from this function.
    ///
    /// If the transaction fails to commit due to a `SerializationFailure` or a
    /// `ReadOnlyTransaction` a rollback will be attempted.
    /// If the rollback fails, the error will be returned in a
    /// [`Error::RollbackErrorOnCommit`](diesel::result::Error::RollbackErrorOnCommit),
    /// from which you will be able to extract both the original commit error and
    /// the rollback error.
    /// In addition, the connection will be considered broken
    /// as it contains a uncommitted unabortable open transaction. Any further
    /// interaction with the transaction system will result in an returned error
    /// in this case.
    ///
    /// If the closure returns an `Err(_)` and the rollback fails the function
    /// will return that rollback error directly, and the transaction manager will
    /// be marked as broken as it contains a uncommitted unabortable open transaction.
    ///
    /// If a nested transaction fails to release the corresponding savepoint
    /// the error will be returned directly.
    ///
    /// **WARNING:** Canceling the returned future does currently **not**
    /// close an already open transaction. You may end up with a connection
    /// containing a dangling transaction.
    ///
    /// # Example
    ///
    /// ```rust
    /// # include!("../async_doctest_setup.rs");
    /// use diesel::result::Error;
    /// use scoped_futures::ScopedFutureExt;
    /// use diesel::prelude::*;
    ///
    /// # #[tokio::main(flavor = "current_thread")]
    /// # async fn main() {
    /// #     run_test().await.unwrap();
    /// # }
    /// #
    /// # async fn run_test() -> QueryResult<()> {
    /// #     use schema::users::dsl::*;
    /// #     let conn = &mut establish_connection().await;
    /// conn.transaction::<_, Error, _>(|conn| async move {
    ///     diesel::insert_into(users)
    ///         .values(name.eq("Ruby"))
    ///         .execute(conn)
    ///         .await?;
    ///
    ///     let all_names = users.select(name).load::<String>(conn).await?;
    ///     assert_eq!(vec!["Sean", "Tess", "Ruby"], all_names);
    ///
    ///     Ok(())
    /// }.scope_boxed()).await?;
    ///
    /// conn.transaction::<(), _, _>(|conn| async move {
    ///     diesel::insert_into(users)
    ///         .values(name.eq("Pascal"))
    ///         .execute(conn)
    ///         .await?;
    ///
    ///     let all_names = users.select(name).load::<String>(conn).await?;
    ///     assert_eq!(vec!["Sean", "Tess", "Ruby", "Pascal"], all_names);
    ///
    ///     // If we want to roll back the transaction, but don't have an
    ///     // actual error to return, we can return `RollbackTransaction`.
    ///     Err(Error::RollbackTransaction)
    /// }.scope_boxed()).await;
    ///
    /// let all_names = users.select(name).load::<String>(conn).await?;
    /// assert_eq!(vec!["Sean", "Tess", "Ruby"], all_names);
    /// #     Ok(())
    /// # }
    /// ```
    fn transaction<'a, 'conn, R, E, F>(
        &'conn mut self,
        callback: F,
    ) -> BoxFuture<'conn, Result<R, E>>
    // we cannot use `impl Trait` here due to bugs in rustc
    // https://github.com/rust-lang/rust/issues/100013
    //impl Future<Output = Result<R, E>> + Send + 'async_trait
    where
        F: for<'r> FnOnce(&'r mut Self) -> ScopedBoxFuture<'a, 'r, Result<R, E>> + Send + 'a,
        E: From<crate::result::Error> + Send + 'a,
        R: Send + 'a,
        'a: 'conn,
    {
        Self::TransactionManager::transaction(self, callback).boxed()
    }

    /// Creates a transaction that will never be committed. This is useful for
    /// tests. Panics if called while inside of a transaction or
    /// if called with a connection containing a broken transaction
    fn begin_test_transaction(&mut self) -> impl Future<Output = QueryResult<()>> + Send {
        use crate::connection::TransactionManagerStatus;

        async {
            match Self::TransactionManager::transaction_manager_status_mut(self) {
                TransactionManagerStatus::Valid(valid_status) => {
                    assert_eq!(None, valid_status.transaction_depth())
                }
                TransactionManagerStatus::InError => panic!("Transaction manager in error"),
            };
            Self::TransactionManager::begin_transaction(self).await?;
            // set the test transaction flag
            // to prevent that this connection gets dropped in connection pools
            // Tests commonly set the poolsize to 1 and use `begin_test_transaction`
            // to prevent modifications to the schema
            Self::TransactionManager::transaction_manager_status_mut(self)
                .set_test_transaction_flag();
            Ok(())
        }
    }

    /// Executes the given function inside a transaction, but does not commit
    /// it. Panics if the given function returns an error.
    ///
    /// # Example
    ///
    /// ```rust
    /// # include!("../async_doctest_setup.rs");
    /// use diesel::result::Error;
    /// use scoped_futures::ScopedFutureExt;
    /// use diesel::prelude::*;
    ///
    /// # #[tokio::main(flavor = "current_thread")]
    /// # async fn main() {
    /// #     run_test().await.unwrap();
    /// # }
    /// #
    /// # async fn run_test() -> QueryResult<()> {
    /// #     use schema::users::dsl::*;
    /// #     let conn = &mut establish_connection().await;
    /// conn.test_transaction::<_, Error, _>(|conn| async move {
    ///     diesel::insert_into(users)
    ///         .values(name.eq("Ruby"))
    ///         .execute(conn)
    ///         .await?;
    ///
    ///     let all_names = users.select(name).load::<String>(conn).await?;
    ///     assert_eq!(vec!["Sean", "Tess", "Ruby"], all_names);
    ///
    ///     Ok(())
    /// }.scope_boxed()).await;
    ///
    /// // Even though we returned `Ok`, the transaction wasn't committed.
    /// let all_names = users.select(name).load::<String>(conn).await?;
    /// assert_eq!(vec!["Sean", "Tess"], all_names);
    /// #     Ok(())
    /// # }
    /// ```
    fn test_transaction<'conn, 'a, R, E, F>(
        &'conn mut self,
        f: F,
    ) -> impl Future<Output = R> + Send + 'conn
    where
        F: for<'r> FnOnce(&'r mut Self) -> ScopedBoxFuture<'a, 'r, Result<R, E>> + Send + 'a,
        E: Debug + Send + 'a,
        R: Send + 'a,
        'a: 'conn,
    {
        use futures_util::TryFutureExt;
        let (user_result_tx, user_result_rx) = std::sync::mpsc::channel();
        self.transaction::<R, _, _>(move |conn| {
            f(conn)
                .map_err(|_| crate::result::Error::RollbackTransaction)
                .and_then(move |r| {
                    let _ = user_result_tx.send(r);
                    std::future::ready(Err(crate::result::Error::RollbackTransaction))
                })
                .scope_boxed()
        })
        .then(move |_r| {
            let r = user_result_rx
                .try_recv()
                .expect("Transaction did not succeed");
            std::future::ready(r)
        })
    }

    #[doc(hidden)]
    fn transaction_state(
        &mut self,
    ) -> &mut <Self::TransactionManager as AsyncTransactionManager<Self>>::TransactionStateData;

    #[doc(hidden)]
    fn instrumentation(&mut self) -> &mut dyn Instrumentation;

    /// Set a specific [`Instrumentation`] implementation for this connection
    fn set_instrumentation(&mut self, instrumentation: impl Instrumentation);

    /// Set the prepared statement cache size to [`CacheSize`] for this connection
    fn set_prepared_statement_cache_size(&mut self, size: CacheSize);
}
