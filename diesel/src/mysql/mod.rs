//! Provides types and functions related to working with MySQL
//!
//! Much of this module is re-exported from database agnostic locations.
//! However, if you are writing code specifically to extend Diesel on
//! MySQL, you may need to work with this module directly.

pub(crate) mod backend;
#[cfg(feature = "mysql")]
mod connection;

use crate::mysql_like::query_builder::MysqlLikeQueryBuilder;

#[doc(inline)]
pub use self::backend::Mysql;
#[cfg(feature = "mysql")]
#[doc(inline)]
pub use self::connection::MysqlConnection;
#[doc(inline)]
pub use super::mysql_like::sql_types;
#[doc(inline)]
pub use super::mysql_like::{MysqlType, MysqlValue, NumericRepresentation};
#[doc(inline)]
pub use crate::mysql_like::data_types;

/// The MySQL query builder
pub type MysqlQueryBuilder = MysqlLikeQueryBuilder<Mysql>;

// The async connection for this backend, which used to be `diesel-async`'s
// own `mysql` module. It is a submodule rather than a peer
// so that everything that speaks this backend is reachable from one path.
#[cfg(feature = "async-mysql")]
pub mod async_connection;
#[cfg(feature = "async-mysql")]
#[doc(inline)]
pub use self::async_connection::{AsyncMysqlConnection, MysqlCancelToken};
