//! A connection pool implementation for `diesel-async` based on [`deadpool`]
//!
//! ```rust
//! # include!("../doctest_setup.rs");
//! use diesel::result::Error;
//! use futures_util::FutureExt;
//! use diesel::pooled_connection::AsyncDieselConnectionManager;
//! use diesel::pooled_connection::deadpool::Pool;
//! use diesel::prelude::*;
//!
//! # #[tokio::main(flavor = "current_thread")]
//! # async fn main() {
//! #     run_test().await.unwrap();
//! # }
//! #
//! # #[cfg(feature = "async-postgres")]
//! # fn get_config() -> AsyncDieselConnectionManager<diesel::pg::AsyncPgConnection> {
//! #     let db_url = database_url_from_env("PG_DATABASE_URL");
//! let config = AsyncDieselConnectionManager::<diesel::pg::AsyncPgConnection>::new(db_url);
//! #     config
//! #  }
//! #
//! # #[cfg(feature = "async-mysql")]
//! # fn get_config() -> AsyncDieselConnectionManager<diesel::mysql::AsyncMysqlConnection> {
//! #     let db_url = database_url_from_env("MYSQL_DATABASE_URL");
//! #    let config = AsyncDieselConnectionManager::<diesel::mysql::AsyncMysqlConnection>::new(db_url);
//! #     config
//! #  }
//! #
//! # #[cfg(feature = "async-sqlite")]
//! # fn get_config() -> AsyncDieselConnectionManager<diesel::sync_connection_wrapper::SyncConnectionWrapper<diesel::SqliteConnection>> {
//! #     let db_url = database_url_from_env("SQLITE_DATABASE_URL");
//! #     let config = AsyncDieselConnectionManager::<diesel::sync_connection_wrapper::SyncConnectionWrapper<diesel::SqliteConnection>>::new(db_url);
//! #     config
//! # }
//! #
//! # async fn run_test() -> Result<(), Box<dyn std::error::Error + Send + Sync + 'static>> {
//! #     use schema::users::dsl::*;
//! #     let config = get_config();
//! # #[cfg(feature = "async-postgres")]
//! let pool: Pool<AsyncPgConnection> = Pool::builder(config).build()?;
//! # #[cfg(not(feature = "async-postgres"))]
//! # let pool = Pool::builder(config).build()?;
//! let mut conn = pool.get().await?;
//! # conn.begin_test_transaction();
//! # create_tables(&mut conn).await;
//! # conn.begin_test_transaction();
//! let res = users.load::<(i32, String)>(&mut conn).await?;
//! #     Ok(())
//! # }
//! ```
use super::{AsyncDieselConnectionManager, PoolableConnection};
use crate::query_builder::QueryFragment;
use deadpool::managed::Manager;

/// Type alias for using [`deadpool::managed::Pool`] with [`diesel-async`]
///
/// This is **not** equal to [`deadpool::managed::Pool`]. It already uses the correct
/// connection manager and expects only the connection type as generic argument
pub type Pool<C> = deadpool::managed::Pool<AsyncDieselConnectionManager<C>>;
/// Type alias for using [`deadpool::managed::PoolBuilder`] with [`diesel-async`]
pub type PoolBuilder<C> = deadpool::managed::PoolBuilder<AsyncDieselConnectionManager<C>>;
/// Type alias for using [`deadpool::managed::BuildError`] with [`diesel-async`]
pub type BuildError = deadpool::managed::BuildError;
/// Type alias for using [`deadpool::managed::PoolError`] with [`diesel-async`]
pub type PoolError = deadpool::managed::PoolError<super::PoolError>;
/// Type alias for using [`deadpool::managed::Object`] with [`diesel-async`]
pub type Object<C> = deadpool::managed::Object<AsyncDieselConnectionManager<C>>;
/// Type alias for using [`deadpool::managed::Hook`] with [`diesel-async`]
pub type Hook<C> = deadpool::managed::Hook<AsyncDieselConnectionManager<C>>;
/// Type alias for using [`deadpool::managed::HookError`] with [`diesel-async`]
pub type HookError = deadpool::managed::HookError<super::PoolError>;

impl<C> Manager for AsyncDieselConnectionManager<C>
where
    C: PoolableConnection + Send + 'static,
    crate::dsl::select<crate::dsl::AsExprOf<i32, crate::sql_types::Integer>>:
        crate::query_dsl::async_run_query_dsl::methods::ExecuteDsl<C>,
    crate::query_builder::SqlQuery: QueryFragment<C::Backend>,
{
    type Type = C;

    type Error = super::PoolError;

    async fn create(&self) -> Result<Self::Type, Self::Error> {
        (self.manager_config.custom_setup)(&self.connection_url)
            .await
            .map_err(super::PoolError::ConnectionError)
    }

    async fn recycle(
        &self,
        obj: &mut Self::Type,
        _: &deadpool::managed::Metrics,
    ) -> deadpool::managed::RecycleResult<Self::Error> {
        if std::thread::panicking() || obj.is_broken() {
            return Err(deadpool::managed::RecycleError::Message(
                "Broken connection".into(),
            ));
        }
        obj.ping(&self.manager_config.recycling_method)
            .await
            .map_err(super::PoolError::QueryError)?;
        Ok(())
    }
}
