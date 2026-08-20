//! `establish_single_process_encrypted`, from the outside and from the file.
//!
//! The property worth pinning is not "the round trip works" — an unencrypted
//! database round-trips too. It is that the bytes on disk stop being the
//! bytes that went in: a credential written through this door must not be
//! findable by reading the file, and the file must not open without the key.
//! Those two are what a caller is buying, so those two are what is asserted,
//! against the raw file rather than through the API that wrote it.

use anyhow::Result;
use diesel::async_dsl::RunQueryDsl;
use diesel::connection::SimpleAsyncConnection;
use diesel::prelude::*;
use diesel::turso::{EncryptionOpts, TursoConnection};

diesel::table! {
    secrets(id) {
        id -> Integer,
        value -> Text,
    }
}

const SCHEMA: &str = "CREATE TABLE secrets(id INTEGER PRIMARY KEY, value TEXT NOT NULL) STRICT;";

// A recognisable run of bytes to look for in the file afterwards. Long
// enough that finding it by chance is not a thing that happens.
const SECRET: &str = "correct-horse-battery-staple-0123456789abcdef";

fn key(byte: u8) -> EncryptionOpts {
    EncryptionOpts {
        cipher: "aegis256".to_string(),
        // aegis256 takes a 32-byte key, which is 64 hex digits.
        hexkey: format!("{byte:02x}").repeat(32),
    }
}

async fn write_secret(conn: &mut TursoConnection) -> Result<()> {
    conn.batch_execute(SCHEMA).await?;
    diesel::insert_into(secrets::table)
        .values((secrets::id.eq(1), secrets::value.eq(SECRET)))
        .execute(conn)
        .await?;
    Ok(())
}

async fn read_secret(conn: &mut TursoConnection) -> Result<String> {
    Ok(secrets::table
        .select(secrets::value)
        .first::<String>(conn)
        .await?)
}

// Everything a Turso database writes, not just the main file: a page that
// only ever reached the WAL is a page on disk all the same.
fn every_byte_written(path: &std::path::Path) -> Result<Vec<u8>> {
    let mut bytes = Vec::new();
    let directory = path.parent().unwrap_or(std::path::Path::new("."));
    for entry in std::fs::read_dir(directory)? {
        let entry = entry?;
        if entry.file_type()?.is_file() {
            bytes.extend(std::fs::read(entry.path())?);
        }
    }
    Ok(bytes)
}

/// The point of the door: what went in is not on the disk in the clear.
#[tokio::test(flavor = "current_thread")]
async fn a_value_written_through_the_encrypted_door_is_not_in_the_file() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let path = dir.path().join("secrets.db");
    let url = path.to_string_lossy().to_string();

    {
        let mut conn = TursoConnection::establish_single_process_encrypted(&url, key(0x11)).await?;
        write_secret(&mut conn).await?;
        // Read it back through the same connection, so the assertion below
        // is about the file and not about the write having been lost.
        assert_eq!(read_secret(&mut conn).await?, SECRET);
    }

    let bytes = every_byte_written(&path)?;
    assert!(
        !bytes
            .windows(SECRET.len())
            .any(|window| window == SECRET.as_bytes()),
        "the secret is readable in the file the encrypted connection wrote"
    );
    Ok(())
}

/// And the control: without the door it *is* in the file. Without this the
/// test above would still pass if the write had silently gone nowhere.
#[tokio::test(flavor = "current_thread")]
async fn the_same_value_written_through_the_plain_door_is_in_the_file() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let path = dir.path().join("secrets.db");
    let url = path.to_string_lossy().to_string();

    {
        let mut conn = TursoConnection::establish_single_process(&url).await?;
        write_secret(&mut conn).await?;
    }

    let bytes = every_byte_written(&path)?;
    assert!(
        bytes
            .windows(SECRET.len())
            .any(|window| window == SECRET.as_bytes()),
        "an unencrypted database was expected to hold the secret in the clear"
    );
    Ok(())
}

/// The key is the whole of the access control: the right one reads, the
/// wrong one and no key at all do not.
#[tokio::test(flavor = "current_thread")]
async fn only_the_key_it_was_written_with_opens_it() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let path = dir.path().join("secrets.db");
    let url = path.to_string_lossy().to_string();

    {
        let mut conn = TursoConnection::establish_single_process_encrypted(&url, key(0x22)).await?;
        write_secret(&mut conn).await?;
    }

    // The right key.
    {
        let mut conn = TursoConnection::establish_single_process_encrypted(&url, key(0x22)).await?;
        assert_eq!(read_secret(&mut conn).await?, SECRET);
    }

    // The wrong one. Whether it is refused at open or at the first read is
    // Turso's business; that it never yields the row is not.
    let wrong = async {
        let mut conn = TursoConnection::establish_single_process_encrypted(&url, key(0x33)).await?;
        read_secret(&mut conn).await
    }
    .await;
    assert!(wrong.is_err(), "the wrong key read the database");

    // And none at all.
    let plain = async {
        let mut conn = TursoConnection::establish_single_process(&url).await?;
        read_secret(&mut conn).await
    }
    .await;
    assert!(plain.is_err(), "no key at all read the database");

    Ok(())
}

/// Foreign keys are enforced on this door too. It differs from
/// `establish_single_process` in the cipher and in nothing else, and a door
/// that quietly stopped enforcing would be a door that quietly accepts
/// orphan rows.
#[tokio::test(flavor = "current_thread")]
async fn the_encrypted_door_enforces_references() -> Result<()> {
    diesel::table! {
        owners(id) {
            id -> Integer,
        }
    }
    diesel::table! {
        pets(id) {
            id -> Integer,
            owner_id -> Integer,
        }
    }

    let dir = tempfile::tempdir()?;
    let url = dir.path().join("pets.db").to_string_lossy().to_string();
    let mut conn = TursoConnection::establish_single_process_encrypted(&url, key(0x44)).await?;
    conn.batch_execute(
        "CREATE TABLE owners(id INTEGER PRIMARY KEY) STRICT;
         CREATE TABLE pets(id INTEGER PRIMARY KEY,
                           owner_id INTEGER NOT NULL REFERENCES owners(id)) STRICT;",
    )
    .await?;

    let err = diesel::insert_into(pets::table)
        .values((pets::id.eq(1), pets::owner_id.eq(404)))
        .execute(&mut conn)
        .await
        .expect_err("owner 404 does not exist");
    assert!(
        format!("{err}").to_lowercase().contains("foreign key"),
        "expected a foreign-key violation, got: {err}"
    );
    Ok(())
}
