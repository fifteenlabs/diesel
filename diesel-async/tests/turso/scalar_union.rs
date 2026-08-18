//! Acceptance tests for scalar UNION variants and mixed scalar+struct
//! variants. Also covers `uuid::Uuid` as a composite field type.

use anyhow::Result;
use diesel::deserialize::FromSqlRow;
use diesel::expression::AsExpression;
use diesel::prelude::*;
use diesel_async::{AsyncConnection, RunQueryDsl, SimpleAsyncConnection};
use diesel::turso::union::{TaggedUnion, UnionSchema};
use diesel::UnionSchema as DeriveUnionSchema;
use diesel_async::turso::TursoConnection;
use uuid::Uuid;

// Pure-scalar UNION — the classic `UNION(i INT, f REAL, s TEXT)`.
#[derive(
    Debug,
    PartialEq,
    Clone,
    FromSqlRow,
    AsExpression,
    diesel::query_builder::QueryId,
    DeriveUnionSchema,
)]
#[diesel(sql_type = TaggedUnion<Number>)]
#[union(name = "number")]
pub enum Number {
    I(i64),
    F(f64),
    S(String),
}

diesel::table! {
    use diesel::sql_types::*;
    use diesel::turso::union::TaggedUnion;
    use super::Number;
    scalars(id) {
        id -> BigInt,
        v -> TaggedUnion<Number>,
    }
}

async fn setup_scalars() -> Result<TursoConnection> {
    let mut conn = TursoConnection::establish(":memory:").await?;
    conn.batch_execute(&Number::create_type_sql()).await?;
    conn.batch_execute("CREATE TABLE scalars(id INTEGER PRIMARY KEY, v number NOT NULL) STRICT")
        .await?;
    Ok(conn)
}

#[tokio::test(flavor = "current_thread")]
async fn scalar_variants_roundtrip() -> Result<()> {
    let mut conn = setup_scalars().await?;
    for (id, v) in [
        (1i64, Number::I(-100)),
        (2, Number::F(std::f64::consts::PI)),
        (3, Number::S("hi".into())),
    ] {
        diesel::insert_into(scalars::table)
            .values((scalars::id.eq(id), scalars::v.eq(v)))
            .execute(&mut conn)
            .await?;
    }
    let rows: Vec<(i64, Number)> = scalars::table
        .order(scalars::id.asc())
        .load(&mut conn)
        .await?;
    assert_eq!(
        rows,
        vec![
            (1, Number::I(-100)),
            (2, Number::F(std::f64::consts::PI)),
            (3, Number::S("hi".into())),
        ]
    );
    Ok(())
}

#[tokio::test(flavor = "current_thread")]
async fn scalar_variants_server_side_tag_and_extract() -> Result<()> {
    let mut conn = setup_scalars().await?;
    diesel::insert_into(scalars::table)
        .values((scalars::id.eq(1i64), scalars::v.eq(Number::I(42))))
        .execute(&mut conn)
        .await?;
    diesel::insert_into(scalars::table)
        .values((scalars::id.eq(2i64), scalars::v.eq(Number::S("x".into()))))
        .execute(&mut conn)
        .await?;

    // Turso's own SQL should recognize the blobs we emit.
    let mut rows = conn
        .raw()
        .query(
            "SELECT id, CAST(union_tag(v) AS TEXT), v.i, v.s FROM scalars ORDER BY id",
            (),
        )
        .await?;
    while let Some(r) = rows.next().await? {
        let id = match r.get_value(0)? {
            turso::Value::Integer(i) => i,
            _ => panic!("id"),
        };
        let tag = match r.get_value(1)? {
            turso::Value::Text(s) => s,
            _ => panic!("tag"),
        };
        match id {
            1 => {
                assert_eq!(tag, "i");
                assert!(matches!(r.get_value(2)?, turso::Value::Integer(42)));
                assert!(matches!(r.get_value(3)?, turso::Value::Null));
            }
            2 => {
                assert_eq!(tag, "s");
                assert!(matches!(r.get_value(2)?, turso::Value::Null));
                assert!(matches!(r.get_value(3)?, turso::Value::Text(s) if s == "x"));
            }
            _ => panic!("unexpected id {id}"),
        }
    }
    Ok(())
}

// Mixed scalar + struct variants.
#[derive(
    Debug,
    PartialEq,
    Clone,
    FromSqlRow,
    AsExpression,
    diesel::query_builder::QueryId,
    DeriveUnionSchema,
)]
#[diesel(sql_type = TaggedUnion<Mixed>)]
pub enum Mixed {
    Plain(i64),
    Complex { chat_id: i64, text: String },
}

diesel::table! {
    use diesel::sql_types::*;
    use diesel::turso::union::TaggedUnion;
    use super::Mixed;
    mixed_tbl(id) {
        id -> BigInt,
        v -> TaggedUnion<Mixed>,
    }
}

#[tokio::test(flavor = "current_thread")]
async fn mixed_scalar_and_struct_variants() -> Result<()> {
    let mut conn = TursoConnection::establish(":memory:").await?;
    conn.batch_execute(&Mixed::create_type_sql()).await?;
    conn.batch_execute("CREATE TABLE mixed_tbl(id INTEGER PRIMARY KEY, v mixed NOT NULL) STRICT")
        .await?;

    diesel::insert_into(mixed_tbl::table)
        .values((mixed_tbl::id.eq(1i64), mixed_tbl::v.eq(Mixed::Plain(77))))
        .execute(&mut conn)
        .await?;
    diesel::insert_into(mixed_tbl::table)
        .values((
            mixed_tbl::id.eq(2i64),
            mixed_tbl::v.eq(Mixed::Complex {
                chat_id: -9,
                text: "hey".into(),
            }),
        ))
        .execute(&mut conn)
        .await?;

    let rows: Vec<(i64, Mixed)> = mixed_tbl::table
        .order(mixed_tbl::id.asc())
        .load(&mut conn)
        .await?;
    assert_eq!(
        rows,
        vec![
            (1, Mixed::Plain(77)),
            (
                2,
                Mixed::Complex {
                    chat_id: -9,
                    text: "hey".into()
                }
            ),
        ]
    );
    Ok(())
}

#[tokio::test(flavor = "current_thread")]
async fn mixed_create_type_sql_shape() {
    let sql = Mixed::create_type_sql();
    // Scalar variant → no inline struct type.
    assert!(!sql.contains("CREATE TYPE plain_t"));
    // Struct variant → its own struct type.
    assert!(sql.contains("CREATE TYPE complex_t AS STRUCT(chat_id INT, text TEXT);"));
    // Union lists scalar variant with its sql_type directly, struct with _t.
    assert!(sql.contains("CREATE TYPE mixed AS UNION(plain INT, complex complex_t)"));
}

// Uuid in a UNION variant.
#[derive(
    Debug,
    PartialEq,
    Clone,
    FromSqlRow,
    AsExpression,
    diesel::query_builder::QueryId,
    DeriveUnionSchema,
)]
#[diesel(sql_type = TaggedUnion<KeyedValue>)]
// Named `KeyedValue` rather than `Keyed` because the derive emits a
// `snake_case(Ident)` identifier module, and a `Keyed` enum beside a
// `keyed` table would be two items called `keyed` in one scope.
#[union(name = "keyed")]
pub enum KeyedValue {
    ById(Uuid),
    ByHash {
        digest: Vec<u8>,
        note: Option<String>,
    },
}

diesel::table! {
    use diesel::sql_types::*;
    use diesel::turso::union::TaggedUnion;
    use super::KeyedValue;
    keyed(id) {
        id -> BigInt,
        v -> TaggedUnion<KeyedValue>,
    }
}

#[tokio::test(flavor = "current_thread")]
async fn uuid_field_codec_roundtrip() -> Result<()> {
    let mut conn = TursoConnection::establish(":memory:").await?;
    conn.batch_execute(&KeyedValue::create_type_sql()).await?;
    conn.batch_execute("CREATE TABLE keyed(id INTEGER PRIMARY KEY, v keyed NOT NULL) STRICT")
        .await?;

    let uid = Uuid::now_v7();
    diesel::insert_into(keyed::table)
        .values((keyed::id.eq(1i64), keyed::v.eq(KeyedValue::ById(uid))))
        .execute(&mut conn)
        .await?;
    diesel::insert_into(keyed::table)
        .values((
            keyed::id.eq(2i64),
            keyed::v.eq(KeyedValue::ByHash {
                digest: vec![1, 2, 3, 4],
                note: Some("abc".into()),
            }),
        ))
        .execute(&mut conn)
        .await?;

    let rows: Vec<(i64, KeyedValue)> = keyed::table.order(keyed::id.asc()).load(&mut conn).await?;
    assert_eq!(
        rows,
        vec![
            (1, KeyedValue::ById(uid)),
            (
                2,
                KeyedValue::ByHash {
                    digest: vec![1, 2, 3, 4],
                    note: Some("abc".into()),
                }
            ),
        ]
    );

    // And the scalar-variant's sql_type in the DDL is BLOB, matching Uuid's
    // storage class.
    assert!(KeyedValue::create_type_sql().contains("by_id BLOB"));
    Ok(())
}
