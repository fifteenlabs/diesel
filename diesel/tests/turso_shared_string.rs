//! `gpui::SharedString` as a UNION field type — covers the fork's
//! fifteen-db use case of `Option<gpui::SharedString>` in variant bodies.
//!
//! Its own test target rather than a module of `tests/turso`, because it is
//! the only thing in the Turso suite that needs `gpui`, and naming `gpui` in
//! the suite's `required-features` made all 177 of the others unselectable
//! while that dependency does not resolve. See the `[[test]]` entries in
//! `Cargo.toml`.

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
