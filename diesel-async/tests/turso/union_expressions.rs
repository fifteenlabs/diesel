//! `union_extract` / `struct_extract` / `union_tag` as expression nodes,
//! against a real Turso database.
//!
//! The rendering is asserted alongside the answers on purpose. What these
//! nodes emit is not free-floating text: `crates/fifteen-db`'s
//! `messages_*` indexes are `CREATE INDEX`es over the same two calls, and
//! an expression index only serves a query whose expression matches it. So
//! a change in what `walk_ast` writes is a change in whether those indexes
//! are used, and the SQL strings below are here to make that change
//! visible. (`fifteen-db`'s `expression_index_plans` test then checks the
//! match against the real indexes with `EXPLAIN QUERY PLAN`.)

use anyhow::Result;
use diesel::prelude::*;
use diesel::query_builder::QueryId;
use diesel_async::{AsyncConnection, RunQueryDsl, SimpleAsyncConnection};
use diesel::turso::union::{CompositeExpressionMethods, TaggedUnion, UnionExpressionMethods, UnionSchema};
use diesel::UnionSchema as DeriveUnionSchema;
use diesel::turso::Turso;
use diesel_async::turso::TursoConnection;

/// Two struct variants and a scalar one — enough to tell a field of one
/// variant from the same-named field of another, and to keep the scalar
/// case (whose `union_extract` is not a composite at all) honest.
#[derive(
    Debug,
    PartialEq,
    Clone,
    diesel::deserialize::FromSqlRow,
    diesel::expression::AsExpression,
    QueryId,
    DeriveUnionSchema,
)]
#[diesel(sql_type = TaggedUnion<RowKey>)]
#[union(name = "row_key")]
pub enum RowKey {
    #[union(struct_type = "telegram_key")]
    Telegram {
        chat_id: i64,
        message_id: i64,
        topic_id: Option<i64>,
    },
    #[union(struct_type = "email_key")]
    Email { user_id: String, thread_id: String },
    /// Scalar variant: `union_extract(k, 'legacy')` is the INT itself, and
    /// `.field(…)` on it does not compile.
    Legacy(i64),
}

diesel::table! {
    use diesel::sql_types::*;
    use diesel::turso::union::TaggedUnion;
    use super::RowKey;

    rows(k) {
        k -> TaggedUnion<RowKey>,
        note -> Text,
    }
}

async fn seeded() -> Result<TursoConnection> {
    let mut conn = TursoConnection::establish(":memory:").await?;
    conn.batch_execute(&RowKey::create_type_sql()).await?;
    conn.batch_execute("CREATE TABLE rows(k row_key PRIMARY KEY, note TEXT NOT NULL) STRICT")
        .await?;
    diesel::insert_into(rows::table)
        .values(vec![
            (
                rows::k.eq(RowKey::Telegram {
                    chat_id: -100,
                    message_id: 7,
                    topic_id: Some(3),
                }),
                rows::note.eq("tg-topic"),
            ),
            (
                rows::k.eq(RowKey::Telegram {
                    chat_id: -100,
                    message_id: 8,
                    topic_id: None,
                }),
                rows::note.eq("tg-plain"),
            ),
            (
                rows::k.eq(RowKey::Email {
                    user_id: "me@example.com".into(),
                    thread_id: "t1".into(),
                }),
                rows::note.eq("email"),
            ),
            (rows::k.eq(RowKey::Legacy(42)), rows::note.eq("legacy")),
        ])
        .execute(&mut conn)
        .await?;
    Ok(conn)
}

fn sql<Q: diesel::query_builder::QueryFragment<Turso> + QueryId>(query: &Q) -> String {
    diesel::debug_query::<Turso, _>(query).to_string()
}

/// `union_extract(…) IS NOT NULL` is the tag test, and it selects exactly
/// the rows of that variant.
#[tokio::test(flavor = "current_thread")]
async fn a_tag_test_is_an_extract_that_is_not_null() -> Result<()> {
    use row_key::telegram;
    let mut conn = seeded().await?;

    let query = rows::table
        .filter(rows::k.extract(telegram::variant).is_not_null())
        .select(rows::note)
        .order(rows::note.asc());
    assert!(
        sql(&query).contains(r#"(union_extract("rows"."k", 'telegram') IS NOT NULL)"#),
        "{}",
        sql(&query)
    );

    let notes: Vec<String> = query.load(&mut conn).await?;
    assert_eq!(notes, vec!["tg-plain", "tg-topic"]);
    Ok(())
}

/// `union_tag` answers the same question as a value, which is what lets it
/// be compared, selected or ordered on.
#[tokio::test(flavor = "current_thread")]
async fn union_tag_names_the_variant() -> Result<()> {
    use row_key::{email, legacy};
    let mut conn = seeded().await?;

    let query = rows::table
        .filter(rows::k.union_tag().eq(email::TAG_NAME))
        .select(rows::note);
    assert!(
        sql(&query).contains(r#"(union_tag("rows"."k") = ?)"#),
        "{}",
        sql(&query)
    );
    let notes: Vec<String> = query.load(&mut conn).await?;
    assert_eq!(notes, vec!["email"]);

    // Selected rather than compared, and over the scalar variant too.
    let tags: Vec<String> = rows::table
        .filter(rows::k.union_tag().eq(legacy::TAG_NAME))
        .select(rows::k.union_tag())
        .load(&mut conn)
        .await?;
    assert_eq!(tags, vec!["legacy"]);
    Ok(())
}

/// A field comparison: the projection renders as the nested call the
/// expression indexes are built on, and the bind is the field's own type.
#[tokio::test(flavor = "current_thread")]
async fn a_field_compares_against_its_own_type() -> Result<()> {
    use row_key::telegram;
    let mut conn = seeded().await?;

    let query = rows::table
        .filter(
            rows::k
                .extract(telegram::variant)
                .field(telegram::chat_id)
                .eq(-100i64),
        )
        .filter(
            rows::k
                .extract(telegram::variant)
                .field(telegram::message_id)
                .eq(7i64),
        )
        .select(rows::note);
    assert!(
        sql(&query)
            .contains(r#"(struct_extract(union_extract("rows"."k", 'telegram'), 'chat_id') = ?)"#),
        "{}",
        sql(&query)
    );

    let notes: Vec<String> = query.load(&mut conn).await?;
    assert_eq!(notes, vec!["tg-topic"]);
    Ok(())
}

/// The projection is a value like any other: it can be selected, and a
/// field that is `Option<_>` in the enum comes back as one.
#[tokio::test(flavor = "current_thread")]
async fn a_field_can_be_selected() -> Result<()> {
    use row_key::telegram;
    let mut conn = seeded().await?;

    let rows: Vec<(i64, Option<i64>)> = rows::table
        .filter(rows::k.extract(telegram::variant).is_not_null())
        .select((
            // Non-NULL by the filter above, which is what `assume_not_null`
            // is for; `topic_id` is optional within the variant and stays
            // nullable.
            rows::k
                .extract(telegram::variant)
                .field(telegram::message_id)
                .assume_not_null(),
            rows::k.extract(telegram::variant).field(telegram::topic_id),
        ))
        .order(rows::note.asc())
        .load(&mut conn)
        .await?;
    assert_eq!(rows, vec![(8, None), (7, Some(3))]);
    Ok(())
}

/// The mismatch case, which is why every one of these is nullable: asking
/// a row for a variant it isn't gives NULL, not an error and not a wrong
/// answer — including for a field whose name two variants share.
#[tokio::test(flavor = "current_thread")]
async fn a_tag_mismatch_is_null_not_an_error() -> Result<()> {
    use row_key::{email, telegram};
    let mut conn = seeded().await?;

    let chat_ids: Vec<Option<i64>> = rows::table
        .select(rows::k.extract(telegram::variant).field(telegram::chat_id))
        .order(rows::note.asc())
        .load(&mut conn)
        .await?;
    assert_eq!(
        chat_ids,
        vec![None, None, Some(-100), Some(-100)],
        "email and legacy rows have no telegram chat_id"
    );

    // The email row is found by its own variant's field, and the telegram
    // rows are not — `thread_id` and `chat_id` are different types, so the
    // two cannot be confused in the first place.
    let notes: Vec<String> = rows::table
        .filter(
            rows::k
                .extract(email::variant)
                .field(email::thread_id)
                .eq("t1"),
        )
        .select(rows::note)
        .load(&mut conn)
        .await?;
    assert_eq!(notes, vec!["email"]);
    Ok(())
}

/// A scalar variant's `union_extract` is the scalar, so it compares
/// directly and there is nothing to project out of it.
#[tokio::test(flavor = "current_thread")]
async fn a_scalar_variant_extracts_to_its_value() -> Result<()> {
    use row_key::legacy;
    let mut conn = seeded().await?;

    let notes: Vec<String> = rows::table
        .filter(rows::k.extract(legacy::variant).eq(42i64))
        .select(rows::note)
        .load(&mut conn)
        .await?;
    assert_eq!(notes, vec!["legacy"]);
    Ok(())
}

/// The point of typing them: a statement carrying these nodes is still
/// statement-cacheable, which the `dsl::sql` fragments they replace were
/// not — diesel reports a `SqlLiteral` as unsafe to cache, and that verdict
/// covers the whole enclosing query, not just the fragment.
///
/// A const assertion rather than a test body: this is a fact about types,
/// so it should fail the build rather than a run.
mod statement_cacheable {
    use diesel::query_builder::QueryId;
    use diesel::turso::union::{Extract, GetField, UnionTag};

    use super::{row_key::telegram, rows};

    /// A whole filtered query, which is the level the property matters at.
    type FilterByField = diesel::helper_types::Filter<
        rows::table,
        diesel::dsl::Eq<GetField<Extract<rows::k, telegram::variant>, telegram::chat_id>, i64>,
    >;

    const _: () = {
        assert!(<FilterByField as QueryId>::HAS_STATIC_QUERY_ID);
        assert!(<Extract<rows::k, telegram::variant> as QueryId>::HAS_STATIC_QUERY_ID);
        assert!(<UnionTag<rows::k> as QueryId>::HAS_STATIC_QUERY_ID);
    };
}
