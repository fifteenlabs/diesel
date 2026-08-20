//! `gpui::SharedString` as a UNION field type — covers the fork's
//! fifteen-db use case of `Option<gpui::SharedString>` in variant bodies.

#![cfg(feature = "gpui")]

use anyhow::Result;
use diesel::async_dsl::RunQueryDsl;
use diesel::connection::{AsyncConnection, SimpleAsyncConnection};
use diesel::deserialize::FromSqlRow;
use diesel::expression::AsExpression;
use diesel::prelude::*;
use diesel::turso::union::{TaggedUnion, UnionSchema};
use diesel::turso::TursoConnection;
use diesel::UnionSchema as DeriveUnionSchema;
use gpui::SharedString;

#[derive(
    Debug,
    PartialEq,
    Clone,
    FromSqlRow,
    AsExpression,
    diesel::query_builder::QueryId,
    DeriveUnionSchema,
)]
#[diesel(sql_type = TaggedUnion<Person>)]
pub enum Person {
    User {
        username: SharedString,
        first_name: Option<SharedString>,
    },
    Bot(SharedString),
}

diesel::table! {
    use diesel::sql_types::*;
    use diesel::turso::union::TaggedUnion;
    use super::Person;
    people(id) {
        id -> BigInt,
        v -> TaggedUnion<Person>,
    }
}

#[tokio::test(flavor = "current_thread")]
async fn shared_string_roundtrip() -> Result<()> {
    let mut conn = TursoConnection::establish(":memory:").await?;
    conn.batch_execute(&Person::create_type_sql()).await?;
    conn.batch_execute("CREATE TABLE people(id INTEGER PRIMARY KEY, v person NOT NULL) STRICT")
        .await?;

    diesel::insert_into(people::table)
        .values((
            people::id.eq(1i64),
            people::v.eq(Person::User {
                username: "alice".into(),
                first_name: Some("Alice".into()),
            }),
        ))
        .execute(&mut conn)
        .await?;
    diesel::insert_into(people::table)
        .values((
            people::id.eq(2i64),
            people::v.eq(Person::User {
                username: "anon".into(),
                first_name: None,
            }),
        ))
        .execute(&mut conn)
        .await?;
    diesel::insert_into(people::table)
        .values((
            people::id.eq(3i64),
            people::v.eq(Person::Bot("helper-bot".into())),
        ))
        .execute(&mut conn)
        .await?;

    let rows: Vec<(i64, Person)> = people::table
        .order(people::id.asc())
        .load(&mut conn)
        .await?;
    assert_eq!(
        rows,
        vec![
            (
                1,
                Person::User {
                    username: "alice".into(),
                    first_name: Some("Alice".into()),
                }
            ),
            (
                2,
                Person::User {
                    username: "anon".into(),
                    first_name: None,
                }
            ),
            (3, Person::Bot("helper-bot".into())),
        ]
    );

    // DDL shape: struct variant with Option<SharedString> uses TEXT;
    // scalar variant's sql_type is also TEXT.
    let sql = Person::create_type_sql();
    assert!(sql.contains("CREATE TYPE user_t AS STRUCT(username TEXT, first_name TEXT);"));
    assert!(sql.contains("CREATE TYPE person AS UNION(user user_t, bot TEXT)"));
    Ok(())
}

/// A `SharedString` comes off a Turso row through Turso's own `FromSql`,
/// not through the generic one in `type_impls::primitives` — which is what
/// keeps a `String` out of the middle of every TEXT column.
///
/// This is a compile-time assertion as much as a runtime one: the generic
/// impl is bounded on `*const str: FromSql<ST, Turso>`, which Turso does not
/// satisfy, so naming this impl at all only resolves because
/// `crate::turso::types` writes one. If that impl were deleted the call
/// below would not compile.
#[test]
fn shared_string_is_decoded_by_turso_own_from_sql() {
    use diesel::deserialize::FromSql;
    use diesel::sql_types::Text;
    use diesel::turso::driver::Value;
    use diesel::turso::{Turso, TursoValue};

    // Either side of the inline/heap boundary a `SmolStr`-backed
    // `SharedString` has, so neither path is left unread.
    for text in ["short", "a value well past any small-string optimisation"] {
        let value = Value::Text(text.to_owned());
        let decoded =
            <SharedString as FromSql<Text, Turso>>::from_sql(TursoValue::new(&value)).unwrap();
        assert_eq!(decoded, SharedString::from(text.to_owned()));
    }

    // And it refuses a column that is not text, rather than stringifying it.
    let value = Value::Integer(7);
    let error = <SharedString as FromSql<Text, Turso>>::from_sql(TursoValue::new(&value))
        .expect_err("an integer is not a Text column");
    assert!(error.to_string().contains("expected Text"), "{error}");
}
