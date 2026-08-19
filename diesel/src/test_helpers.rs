#![allow(missing_docs)] // test only module
extern crate dotenvy;

// Unused when the only backend under test is Turso, whose connection has no
// sync `Connection` impl and so no `TestConnection` below.
#[allow(unused_imports)]
use crate::prelude::*;

cfg_if! {
    if #[cfg(feature = "sqlite")] {
        pub type TestConnection = SqliteConnection;

        pub fn connection() -> TestConnection {
            SqliteConnection::establish(":memory:").unwrap()
        }

        pub fn database_url() -> String {
            String::from(":memory:")
        }
    } else if #[cfg(feature = "postgres")] {
        pub type TestConnection = PgConnection;

        pub fn connection() -> TestConnection {
            pg_connection()
        }

        pub fn database_url() -> String {
            pg_database_url()
        }
    } else if #[cfg(feature = "mysql")] {
        pub type TestConnection = MysqlConnection;

        pub fn connection() -> TestConnection {
            let mut conn = connection_no_transaction();
            conn.begin_test_transaction().unwrap();
            conn
        }

        pub fn connection_no_transaction() -> TestConnection {
            MysqlConnection::establish(&database_url()).unwrap()
        }

        pub fn database_url() -> String {
            dotenvy::var("MYSQL_UNIT_TEST_DATABASE_URL")
                .or_else(|_| dotenvy::var("DATABASE_URL"))
                .expect("DATABASE_URL must be set in order to run tests")
        }
    } else if #[cfg(feature = "turso")] {
        // No `TestConnection`: `TursoConnection` is async and implements
        // `AsyncConnection`, not `Connection`, so nothing here can hand one
        // out. That is not a hole — `--features turso` alone is a supported
        // way to run `cargo test --lib`, and the unit tests it selects (the
        // statement cache's admission rules, the UNION wire codec, the DDL
        // renderer) are pure functions that never open a database. The two
        // test modules that do want a `TestConnection` and are not already
        // behind a backend feature — `connection::transaction_manager` and
        // `query_builder::sql_query` — are gated on a sync backend for the
        // same reason.
    } else {
        compile_error!(
            "At least one backend must be used to test this crate.\n \
            Pass argument `--features \"<backend>\"` with one or more of the following backends, \
            'mysql', 'postgres', 'sqlite' or 'turso'. \n\n \
            ex. cargo test --features \"mysql postgres sqlite\"\n"
        );
    }
}

#[cfg(feature = "postgres")]
pub fn pg_connection() -> PgConnection {
    let mut conn = pg_connection_no_transaction();
    conn.begin_test_transaction().unwrap();
    conn
}

#[cfg(feature = "postgres")]
pub fn pg_connection_no_transaction() -> PgConnection {
    PgConnection::establish(&pg_database_url()).unwrap()
}

#[cfg(feature = "postgres")]
pub fn pg_database_url() -> String {
    dotenvy::var("PG_DATABASE_URL")
        .or_else(|_| dotenvy::var("DATABASE_URL"))
        .expect("DATABASE_URL must be set in order to run tests")
}
