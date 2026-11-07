//! `TursoConnection::cancel_when`: a statement stops when its condition turns
//! true, even one that never yields, and the condition does not outlive the
//! guard that installed it.

use anyhow::Result;
use diesel::async_dsl::RunQueryDsl;
use diesel::connection::AsyncConnection;
use diesel::prelude::*;
use diesel::result::{DatabaseErrorKind, Error};
use diesel::turso::TursoConnection;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

#[derive(QueryableByName)]
struct Count {
    #[diesel(sql_type = diesel::sql_types::BigInt)]
    n: i64,
}

const RUNAWAY: &str =
    "WITH RECURSIVE c(x) AS (SELECT 1 UNION ALL SELECT x + 1 FROM c) SELECT count(*) AS n FROM c";
const BOUNDED: &str = "WITH RECURSIVE c(x) AS (SELECT 1 UNION ALL SELECT x + 1 FROM c WHERE x < 100000) SELECT count(*) AS n FROM c";

async fn count(conn: &mut TursoConnection, sql: &'static str) -> QueryResult<i64> {
    let rows: Vec<Count> = diesel::sql_query(sql).load(conn).await?;
    Ok(rows.into_iter().next().map_or(0, |row| row.n))
}

#[tokio::test(flavor = "multi_thread")]
async fn a_cancelled_condition_stops_a_statement_that_never_yields() -> Result<()> {
    let mut conn = TursoConnection::establish(":memory:").await?;
    let cancelled = Arc::new(AtomicBool::new(false));
    let token = cancelled.clone();
    let canceller = cancelled.clone();
    let cancel = std::thread::spawn(move || {
        std::thread::sleep(Duration::from_millis(50));
        canceller.store(true, Ordering::Release);
    });

    let started = Instant::now();
    let mut guarded = conn.cancel_when(move || AtomicBool::load(&token, Ordering::Acquire));
    let result = count(&mut guarded, RUNAWAY).await;
    cancel.join().expect("canceller thread");
    assert!(
        matches!(
            result,
            Err(Error::DatabaseError(DatabaseErrorKind::Interrupted, _))
        ),
        "a runaway statement stops as cancelled"
    );
    assert!(started.elapsed() < Duration::from_secs(10));
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn a_condition_that_stays_false_changes_nothing() -> Result<()> {
    let mut conn = TursoConnection::establish(":memory:").await?;
    let mut guarded = conn.cancel_when(|| false);
    assert_eq!(count(&mut guarded, BOUNDED).await?, 100000);
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn dropping_the_guard_removes_the_condition() -> Result<()> {
    let mut conn = TursoConnection::establish(":memory:").await?;
    {
        let mut guarded = conn.cancel_when(|| true);
        assert!(matches!(
            count(&mut guarded, BOUNDED).await,
            Err(Error::DatabaseError(DatabaseErrorKind::Interrupted, _))
        ));
    }
    assert_eq!(count(&mut conn, BOUNDED).await?, 100000);
    Ok(())
}
