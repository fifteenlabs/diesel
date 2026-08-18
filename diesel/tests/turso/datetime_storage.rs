//! What does Turso physically store for each built-in type? Probe roundtrips.

use anyhow::Result;
use turso::Builder;

async fn fresh() -> Result<turso::Connection> {
    let db = Builder::new_local(":memory:")
        .experimental_strict(true)
        .experimental_custom_types(true)
        .build()
        .await?;
    Ok(db.connect()?)
}

async fn show(conn: &turso::Connection, label: &str, sql: &str) {
    println!("--- {label}");
    match conn.query(sql, ()).await {
        Ok(mut rows) => loop {
            match rows.next().await {
                Ok(Some(r)) => {
                    let mut parts = vec![];
                    for i in 0..4 {
                        match r.get_value(i) {
                            Ok(v) => parts.push(format!("{v:?}")),
                            Err(_) => break,
                        }
                    }
                    println!("  {}", parts.join(" | "));
                }
                Ok(None) => break,
                Err(e) => {
                    println!("  [err] {e}");
                    break;
                }
            }
        },
        Err(e) => println!("  [query err] {e}"),
    }
}

#[tokio::test(flavor = "current_thread")]
async fn roundtrip_builtins() -> Result<()> {
    let conn = fresh().await?;
    conn.execute(
        "CREATE TABLE t(id INT PRIMARY KEY, \
                         b BOOLEAN, \
                         d DATE, \
                         tm TIME, \
                         ts TIMESTAMP, \
                         u UUID, \
                         j JSON) STRICT",
        (),
    )
    .await?;
    conn.execute(
        "INSERT INTO t VALUES (1, \
                               TRUE, \
                               '2026-04-12', \
                               '15:30:00', \
                               '2026-04-12 15:30:00', \
                               '550e8400-e29b-41d4-a716-446655440000', \
                               '{\"a\":1}')",
        (),
    )
    .await?;
    show(
        &conn,
        "direct SELECT (observe what the driver returns for each column)",
        "SELECT b, d, tm, ts FROM t",
    )
    .await;
    show(
        &conn,
        "typeof()",
        "SELECT typeof(b), typeof(d), typeof(tm), typeof(ts) FROM t",
    )
    .await;
    show(&conn, "more typeofs", "SELECT typeof(u), typeof(j) FROM t").await;
    // Can we use datetime functions on these columns?
    show(
        &conn,
        "strftime on timestamp column",
        "SELECT strftime('%Y', ts) FROM t",
    )
    .await;
    // What about comparison with string literals?
    show(
        &conn,
        "WHERE d = ISO literal",
        "SELECT id FROM t WHERE d = '2026-04-12'",
    )
    .await;
    show(
        &conn,
        "WHERE ts between",
        "SELECT id FROM t WHERE ts BETWEEN '2026-01-01' AND '2027-01-01'",
    )
    .await;
    Ok(())
}

#[tokio::test(flavor = "current_thread")]
async fn bool_coerce() -> Result<()> {
    let conn = fresh().await?;
    conn.execute("CREATE TABLE t(x BOOLEAN) STRICT", ()).await?;
    conn.execute("INSERT INTO t VALUES (TRUE), (FALSE), (1), (0)", ())
        .await?;
    show(&conn, "bool readback", "SELECT x, typeof(x) FROM t").await;
    Ok(())
}
