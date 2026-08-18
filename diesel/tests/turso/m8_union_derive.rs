//! M8 acceptance: `#[derive(UnionSchema)]` replaces the boilerplate
//! from M7. Same schema as `m7_union_codec.rs`, but the trait impls
//! (UnionSchema, ToSql, FromSql) are all macro-generated.

use anyhow::Result;
use diesel::async_dsl::RunQueryDsl;
use diesel::connection::{AsyncConnection, SimpleAsyncConnection};
use diesel::deserialize::FromSqlRow;
use diesel::expression::AsExpression;
use diesel::prelude::*;
use diesel::turso::union::{TaggedUnion, UnionSchema};
use diesel::turso::TursoConnection;
use diesel::UnionSchema as DeriveUnionSchema;

#[derive(
    Debug,
    PartialEq,
    Clone,
    FromSqlRow,
    AsExpression,
    diesel::query_builder::QueryId,
    DeriveUnionSchema,
)]
#[diesel(sql_type = TaggedUnion<MessageData>)]
pub enum MessageData {
    Telegram {
        chat_id: i64,
        text: String,
    },
    Slack {
        channel_id_hash: i64,
        text: Option<String>,
    },
}

diesel::table! {
    use diesel::sql_types::*;
    use diesel::turso::union::TaggedUnion;
    use super::MessageData;

    messages(id) {
        id -> BigInt,
        data -> TaggedUnion<MessageData>,
    }
}

async fn setup() -> Result<TursoConnection> {
    let mut conn = TursoConnection::establish(":memory:").await?;
    // Use the derive-emitted DDL — proves create_type_sql matches the
    // wire format the derive writes.
    conn.batch_execute(&MessageData::create_type_sql()).await?;
    conn.batch_execute(
        "CREATE TABLE messages(id INTEGER PRIMARY KEY, data message_data NOT NULL) STRICT",
    )
    .await?;
    Ok(conn)
}

#[tokio::test(flavor = "current_thread")]
async fn derive_roundtrips_all_variants() -> Result<()> {
    let mut conn = setup().await?;

    for (id, data) in [
        (
            1i64,
            MessageData::Telegram {
                chat_id: -100,
                text: "hi".into(),
            },
        ),
        (
            2,
            MessageData::Slack {
                channel_id_hash: 777,
                text: Some("yo".into()),
            },
        ),
        (
            3,
            MessageData::Slack {
                channel_id_hash: 888,
                text: None,
            },
        ),
    ] {
        diesel::insert_into(messages::table)
            .values((messages::id.eq(id), messages::data.eq(data)))
            .execute(&mut conn)
            .await?;
    }

    let rows: Vec<(i64, MessageData)> = messages::table
        .order(messages::id.asc())
        .load(&mut conn)
        .await?;
    assert_eq!(
        rows,
        vec![
            (
                1,
                MessageData::Telegram {
                    chat_id: -100,
                    text: "hi".into()
                }
            ),
            (
                2,
                MessageData::Slack {
                    channel_id_hash: 777,
                    text: Some("yo".into())
                }
            ),
            (
                3,
                MessageData::Slack {
                    channel_id_hash: 888,
                    text: None
                }
            ),
        ]
    );
    Ok(())
}

#[tokio::test(flavor = "current_thread")]
async fn derive_metadata_matches_hand_written() -> Result<()> {
    assert_eq!(MessageData::type_name(), "message_data");
    assert_eq!(MessageData::variants(), &["telegram", "slack"]);

    let sample = MessageData::Telegram {
        chat_id: 5,
        text: "x".into(),
    };
    assert_eq!(sample.tag(), "telegram");
    assert_eq!(sample.tag_index(), 0);

    let sample = MessageData::Slack {
        channel_id_hash: 5,
        text: None,
    };
    assert_eq!(sample.tag(), "slack");
    assert_eq!(sample.tag_index(), 1);

    // create_type_sql() should be an applicable DDL script.
    let sql = MessageData::create_type_sql();
    assert!(sql.contains("CREATE TYPE telegram_t AS STRUCT(chat_id INT, text TEXT);"));
    assert!(sql.contains("CREATE TYPE slack_t AS STRUCT(channel_id_hash INT, text TEXT);"));
    assert!(sql.contains("CREATE TYPE message_data AS UNION(telegram telegram_t, slack slack_t)"));
    Ok(())
}

#[tokio::test(flavor = "current_thread")]
async fn server_still_recognizes_derive_emitted_blob() -> Result<()> {
    let mut conn = setup().await?;
    diesel::insert_into(messages::table)
        .values((
            messages::id.eq(1i64),
            messages::data.eq(MessageData::Slack {
                channel_id_hash: 42,
                text: Some("hello".into()),
            }),
        ))
        .execute(&mut conn)
        .await?;

    let mut rows = conn
        .raw()
        .query(
            "SELECT CAST(union_tag(data) AS TEXT) FROM messages WHERE id = 1",
            (),
        )
        .await?;
    let r = rows.next().await?.unwrap();
    let tag = match r.get_value(0)? {
        turso::Value::Text(s) => s,
        other => anyhow::bail!("{other:?}"),
    };
    assert_eq!(tag, "slack");
    Ok(())
}

// Tag and name overrides
#[derive(
    Debug,
    PartialEq,
    Clone,
    FromSqlRow,
    AsExpression,
    diesel::query_builder::QueryId,
    DeriveUnionSchema,
)]
#[diesel(sql_type = TaggedUnion<CustomNamed>)]
#[union(name = "custom_name")]
pub enum CustomNamed {
    #[union(tag = "alpha_tag")]
    Alpha {
        x: i64,
    },
    Beta {
        y: String,
    },
}

#[tokio::test(flavor = "current_thread")]
async fn attribute_overrides() {
    assert_eq!(CustomNamed::type_name(), "custom_name");
    assert_eq!(CustomNamed::variants(), &["alpha_tag", "beta"]);
    assert_eq!(CustomNamed::Alpha { x: 1 }.tag(), "alpha_tag");
    let sql = CustomNamed::create_type_sql();
    assert!(sql.contains("CREATE TYPE alpha_tag_t AS STRUCT(x INT);"));
    assert!(sql.contains("CREATE TYPE beta_t AS STRUCT(y TEXT);"));
    assert!(sql.contains("CREATE TYPE custom_name AS UNION(alpha_tag alpha_tag_t, beta beta_t)"));
}
