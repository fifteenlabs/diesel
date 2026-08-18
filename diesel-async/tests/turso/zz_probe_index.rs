//! TEMPORARY probe: does Turso match a struct_extract expression index when
//! the column reference is table-qualified and quoted the way diesel renders
//! it? Delete after answering.

use anyhow::Result;
use diesel::prelude::*;
use diesel_async::{AsyncConnection, RunQueryDsl, SimpleAsyncConnection};
use diesel_async::turso::TursoConnection;

#[derive(QueryableByName, Debug)]
struct Plan {
    #[diesel(sql_type = diesel::sql_types::Text)]
    detail: String,
}

#[tokio::test(flavor = "current_thread")]
#[allow(clippy::disallowed_methods)]
async fn probe() -> Result<()> {
    let mut conn = TursoConnection::establish(":memory:").await?;
    conn.batch_execute(
        "CREATE TYPE telegram_mid AS STRUCT(user_id INT, chat_id INT, message_id INT, topic_id INT);
         CREATE TYPE message_id_v4 AS UNION(telegram telegram_mid, other TEXT);
         CREATE TABLE messages(mid message_id_v4 PRIMARY KEY, date INT NOT NULL) STRICT;
         CREATE INDEX messages_tg ON messages(
             struct_extract(union_extract(mid, 'telegram'), 'chat_id'),
             struct_extract(union_extract(mid, 'telegram'), 'message_id')
         );",
    )
    .await?;

    for q in [
        "EXPLAIN QUERY PLAN SELECT date FROM messages WHERE struct_extract(union_extract(mid, 'telegram'), 'chat_id') = 5",
        "EXPLAIN QUERY PLAN SELECT date FROM messages WHERE struct_extract(union_extract(\"messages\".\"mid\", 'telegram'), 'chat_id') = 5",
        "EXPLAIN QUERY PLAN SELECT date FROM messages WHERE (struct_extract(union_extract(\"messages\".\"mid\", 'telegram'), 'chat_id') = 5)",
        "EXPLAIN QUERY PLAN SELECT date FROM messages m WHERE struct_extract(union_extract(m.mid, 'telegram'), 'chat_id') = 5",
        "EXPLAIN QUERY PLAN SELECT date FROM messages WHERE (union_extract(\"messages\".\"mid\", 'telegram') IS NOT NULL) AND (struct_extract(union_extract(\"messages\".\"mid\", 'telegram'), 'chat_id') = 5)",
    ] {
        let rows: Vec<Plan> = diesel::sql_query(q).load(&mut conn).await?;
        println!("--- {q}");
        for r in rows {
            println!("    {}", r.detail);
        }
    }
    Ok(())
}
