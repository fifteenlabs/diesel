//! Probe: what column type declarations does Turso accept in STRICT tables?
//! Used to justify the `HasSqlType` mapping in backend.rs.

use anyhow::Result;
use turso::Builder;

async fn try_decl(decl: &str) -> Result<(), String> {
    let db = Builder::new_local(":memory:")
        .experimental_strict(true)
        .build()
        .await
        .map_err(|e| e.to_string())?;
    let conn = db.connect().map_err(|e| e.to_string())?;
    conn.execute(&format!("CREATE TABLE t(x {decl}) STRICT"), ())
        .await
        .map_err(|e| e.to_string())
        .map(|_| ())
}

#[tokio::test(flavor = "current_thread")]
async fn probe_types() -> Result<()> {
    for decl in [
        // SQLite STRICT's own list.
        "INT",
        "INTEGER",
        "REAL",
        "TEXT",
        "BLOB",
        "ANY",
        // Common diesel-originated names.
        "SMALLINT",
        "BIGINT",
        "FLOAT",
        "DOUBLE",
        // Date/time family (SQLite convention: TEXT-backed).
        "DATE",
        "TIME",
        "TIMESTAMP",
        "DATETIME",
        // Bool: not a STRICT class; diesel stores it as INTEGER.
        "BOOLEAN",
        "BOOL",
    ] {
        match try_decl(decl).await {
            Ok(_) => println!("[OK]  {decl}"),
            Err(e) => println!("[ERR] {decl}: {e}"),
        }
    }
    Ok(())
}
