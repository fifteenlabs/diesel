//! Probe: what datetime support does Turso ship today?

use anyhow::Result;
use turso::{Builder, Value};

async fn fresh() -> Result<turso::Connection> {
    let db = Builder::new_local(":memory:")
        .experimental_strict(true)
        .experimental_custom_types(true)
        .build()
        .await?;
    Ok(db.connect()?)
}

async fn try_scalar(conn: &turso::Connection, label: &str, sql: &str) {
    match conn.query(sql, ()).await {
        Ok(mut rows) => match rows.next().await {
            Ok(Some(r)) => println!("[OK]  {label}: {:?}", r.get_value(0)),
            Ok(None) => println!("[OK empty] {label}"),
            Err(e) => println!("[ROW ERR] {label}: {e}"),
        },
        Err(e) => println!("[ERR] {label}: {e}"),
    }
}

async fn try_exec(conn: &turso::Connection, label: &str, sql: &str) {
    match conn.execute(sql, ()).await {
        Ok(_) => println!("[OK]  {label}"),
        Err(e) => println!("[ERR] {label}: {e}"),
    }
}

#[tokio::test(flavor = "current_thread")]
async fn sqlite_datetime_functions() -> Result<()> {
    let conn = fresh().await?;
    // Classic SQLite datetime functions.
    try_scalar(&conn, "date('now')", "SELECT date('now')").await;
    try_scalar(&conn, "time('now')", "SELECT time('now')").await;
    try_scalar(&conn, "datetime('now')", "SELECT datetime('now')").await;
    try_scalar(&conn, "julianday('now')", "SELECT julianday('now')").await;
    try_scalar(
        &conn,
        "strftime %Y-%m-%d",
        "SELECT strftime('%Y-%m-%d', 1700000000, 'unixepoch')",
    )
    .await;
    try_scalar(&conn, "unixepoch('now')", "SELECT unixepoch('now')").await;
    try_scalar(&conn, "CURRENT_TIMESTAMP", "SELECT CURRENT_TIMESTAMP").await;
    Ok(())
}

#[tokio::test(flavor = "current_thread")]
async fn turso_builtin_types() -> Result<()> {
    let conn = fresh().await?;
    // The create-type.mdx in the PR lists built-ins: boolean, smallint,
    // bigint, varchar, date, time, timestamp, numeric, uuid, inet, bytea,
    // json, jsonb. See if any are registered as usable types today.
    for name in [
        "boolean",
        "smallint",
        "bigint",
        "varchar",
        "date",
        "time",
        "timestamp",
        "numeric",
        "uuid",
        "inet",
        "bytea",
        "json",
        "jsonb",
    ] {
        let _ = conn.execute("DROP TABLE IF EXISTS t", ()).await;
        try_exec(
            &conn,
            &format!("CREATE TABLE t(x {name}) STRICT"),
            &format!("CREATE TABLE t(x {name}) STRICT"),
        )
        .await;
    }
    Ok(())
}

#[tokio::test(flavor = "current_thread")]
async fn text_iso_roundtrip() -> Result<()> {
    // Verify the conventional pattern: store datetime as TEXT (ISO-8601),
    // filter/compare via string comparison (works because ISO-8601 is
    // lexicographically sortable) and via strftime.
    let conn = fresh().await?;
    conn.execute(
        "CREATE TABLE events(id INT PRIMARY KEY, at TEXT) STRICT",
        (),
    )
    .await?;
    conn.execute(
        "INSERT INTO events VALUES (1, '2026-03-01T12:00:00Z'), (2, '2026-04-15T08:30:00Z'), (3, '2026-02-10T22:10:00Z')",
        (),
    )
    .await?;
    let mut rows = conn
        .query(
            "SELECT id FROM events WHERE at >= '2026-03-01' AND at < '2026-04-01' ORDER BY at",
            (),
        )
        .await?;
    let mut ids = vec![];
    while let Some(r) = rows.next().await? {
        if let Ok(Value::Integer(i)) = r.get_value(0) {
            ids.push(i);
        }
    }
    println!("march rows (ISO text range): {ids:?}");
    Ok(())
}
