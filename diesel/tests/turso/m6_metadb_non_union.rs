//! M6 acceptance: re-run the non-UNION operations from the raw-`turso`
//! exploration (`crates/turso-union-experiment/tests/metadb.rs`, since
//! deleted — see `PLAN.md`) through the diesel DSL against
//! `TursoConnection`.
//!
//! This exercises the inherited SQLite QueryFragment chain across: INSERT
//! with `on_conflict_do_nothing`, ON CONFLICT DO UPDATE, DELETE with
//! `IN (SELECT …)` subquery, multi-column predicates, ORDER BY (asc+desc),
//! aggregate `max`, SQLite's scalar `max(a, b)` / `min(a, b)`.

use anyhow::Result;
use diesel::async_dsl::RunQueryDsl;
use diesel::connection::{AsyncConnection, SimpleAsyncConnection};
use diesel::define_sql_function;
use diesel::prelude::*;
use diesel::sql_types::BigInt;
use diesel::turso::TursoConnection;
use diesel::upsert::excluded;

// SQLite accepts `max(a, b)` / `min(a, b)` as scalar functions (distinct
// from the aggregate form). Declare them with the same SQL name so the
// emitted query matches; the Rust names are different so we don't collide
// with `diesel::dsl::max`.
define_sql_function! {
    #[sql_name = "max"]
    fn scalar_max(a: BigInt, b: BigInt) -> BigInt;
}
define_sql_function! {
    #[sql_name = "min"]
    fn scalar_min(a: BigInt, b: BigInt) -> BigInt;
}

// ---- table! decls (minimal subset of meta.rs that M6 needs) ----------------

diesel::table! {
    account_socials (account_id, social_id, social_variant) {
        account_id -> Integer,
        social_id -> BigInt,
        social_variant -> Integer,
    }
}

diesel::table! {
    accounts (id) {
        id -> Integer,
        service -> Text,
        display_name -> Nullable<Text>,
        user_id -> Nullable<BigInt>,
        is_active -> Integer,
        created_at -> BigInt,
    }
}

diesel::table! {
    chat_index_watermarks (user_id, chat_id) {
        user_id -> BigInt,
        chat_id -> BigInt,
        oldest_seen -> BigInt,
        newest_seen -> BigInt,
    }
}

diesel::table! {
    local_read_watermarks (user_id, chat_id) {
        user_id -> BigInt,
        chat_id -> BigInt,
        last_read_inbox_message_id -> BigInt,
    }
}

diesel::table! {
    message_comments (id) {
        id -> Binary,
        message_chat_id -> BigInt,
        message_id -> BigInt,
        author_user_id -> BigInt,
        text -> Text,
        comment_index -> Integer,
        created_at -> BigInt,
        updated_at -> BigInt,
    }
}

diesel::table! {
    message_edits (message_id, date) {
        message_id -> Binary,
        date -> Integer,
        text -> Nullable<Text>,
    }
}

diesel::table! {
    spaces (id) {
        id -> Binary,
        user_id -> BigInt,
        name -> Text,
        created_at -> BigInt,
        updated_at -> BigInt,
    }
}

diesel::table! {
    space_columns (id) {
        id -> BigInt,
        space_id -> Binary,
        column_order -> Integer,
        chat_id -> Nullable<BigInt>,
        topic_id -> BigInt,
        created_at -> BigInt,
        updated_at -> BigInt,
    }
}

// Needed for `DELETE ... WHERE col IN (SELECT ... FROM other_table)` to
// typecheck: diesel's `ValidSubselect` impl wants `Join<F, QS, Inner>: QuerySource`,
// which only exists once the two tables are declared compatible.
diesel::allow_tables_to_appear_in_same_query!(spaces, space_columns);

// ---- setup -----------------------------------------------------------------

const SCHEMA: &str = r#"
    CREATE TABLE account_socials (
        account_id INTEGER NOT NULL,
        social_id  BIGINT  NOT NULL,
        social_variant INTEGER NOT NULL,
        PRIMARY KEY (account_id, social_id, social_variant)
    ) STRICT;
    CREATE TABLE accounts (
        id INTEGER PRIMARY KEY,
        service TEXT NOT NULL,
        display_name TEXT,
        user_id BIGINT,
        is_active INTEGER NOT NULL,
        created_at BIGINT NOT NULL
    ) STRICT;
    CREATE TABLE chat_index_watermarks (
        user_id BIGINT NOT NULL,
        chat_id BIGINT NOT NULL,
        oldest_seen BIGINT NOT NULL,
        newest_seen BIGINT NOT NULL,
        PRIMARY KEY (user_id, chat_id)
    ) STRICT;
    CREATE TABLE local_read_watermarks (
        user_id BIGINT NOT NULL,
        chat_id BIGINT NOT NULL,
        last_read_inbox_message_id BIGINT NOT NULL,
        PRIMARY KEY (user_id, chat_id)
    ) STRICT;
    CREATE TABLE message_comments (
        id BLOB PRIMARY KEY,
        message_chat_id BIGINT NOT NULL,
        message_id BIGINT NOT NULL,
        author_user_id BIGINT NOT NULL,
        text TEXT NOT NULL,
        comment_index INTEGER NOT NULL,
        created_at BIGINT NOT NULL,
        updated_at BIGINT NOT NULL
    ) STRICT;
    CREATE TABLE message_edits (
        message_id BLOB NOT NULL,
        date INTEGER NOT NULL,
        text TEXT,
        PRIMARY KEY (message_id, date)
    ) STRICT;
    CREATE TABLE spaces (
        id BLOB PRIMARY KEY,
        user_id BIGINT NOT NULL,
        name TEXT NOT NULL,
        created_at BIGINT NOT NULL,
        updated_at BIGINT NOT NULL
    ) STRICT;
    CREATE TABLE space_columns (
        id INTEGER PRIMARY KEY,
        space_id BLOB NOT NULL,
        column_order INTEGER NOT NULL,
        chat_id BIGINT,
        topic_id BIGINT NOT NULL,
        created_at BIGINT NOT NULL,
        updated_at BIGINT NOT NULL
    ) STRICT;
"#;

async fn setup() -> Result<TursoConnection> {
    let mut conn = TursoConnection::establish(":memory:").await?;
    conn.batch_execute(SCHEMA).await?;
    Ok(conn)
}

// ---- Tests mirroring the exploration's metadb.rs ---------------------------

#[tokio::test(flavor = "current_thread")]
async fn link_social_to_account_dedupes() -> Result<()> {
    let mut conn = setup().await?;
    diesel::insert_into(accounts::table)
        .values((
            accounts::id.eq(0),
            accounts::service.eq("telegram"),
            accounts::is_active.eq(1),
            accounts::created_at.eq(1i64),
        ))
        .execute(&mut conn)
        .await?;

    for _ in 0..3 {
        diesel::insert_into(account_socials::table)
            .values((
                account_socials::account_id.eq(0),
                account_socials::social_id.eq(42i64),
                account_socials::social_variant.eq(0),
            ))
            .on_conflict_do_nothing()
            .execute(&mut conn)
            .await?;
    }
    let count: i64 = account_socials::table.count().get_result(&mut conn).await?;
    assert_eq!(count, 1);
    Ok(())
}

#[tokio::test(flavor = "current_thread")]
async fn get_comments_ordered() -> Result<()> {
    let mut conn = setup().await?;
    for i in 0..3i32 {
        diesel::insert_into(message_comments::table)
            .values((
                message_comments::id.eq(uuid::Uuid::now_v7().as_bytes().to_vec()),
                message_comments::message_chat_id.eq(100i64),
                message_comments::message_id.eq(7i64),
                message_comments::author_user_id.eq(1i64),
                message_comments::text.eq("c"),
                message_comments::comment_index.eq(i),
                message_comments::created_at.eq(0i64),
                message_comments::updated_at.eq(0i64),
            ))
            .execute(&mut conn)
            .await?;
    }
    let seen: Vec<i32> = message_comments::table
        .filter(message_comments::message_chat_id.eq(100i64))
        .filter(message_comments::message_id.eq(7i64))
        .order(message_comments::comment_index.asc())
        .select(message_comments::comment_index)
        .load(&mut conn)
        .await?;
    assert_eq!(seen, vec![0, 1, 2]);
    Ok(())
}

#[tokio::test(flavor = "current_thread")]
async fn message_edits_insert_or_ignore_and_desc_order() -> Result<()> {
    let mut conn = setup().await?;
    let mid = uuid::Uuid::now_v7().as_bytes().to_vec();
    for d in [10i32, 20, 20, 30] {
        diesel::insert_into(message_edits::table)
            .values((
                message_edits::message_id.eq(&mid),
                message_edits::date.eq(d),
                message_edits::text.eq(Some("t")),
            ))
            .on_conflict_do_nothing()
            .execute(&mut conn)
            .await?;
    }
    let total: i64 = message_edits::table.count().get_result(&mut conn).await?;
    assert_eq!(total, 3);

    let dates: Vec<i32> = message_edits::table
        .filter(message_edits::message_id.eq(&mid))
        .order(message_edits::date.desc())
        .select(message_edits::date)
        .load(&mut conn)
        .await?;
    assert_eq!(dates, vec![30, 20, 10]);
    Ok(())
}

#[tokio::test(flavor = "current_thread")]
async fn chat_index_watermark_upsert_expands_bounds() -> Result<()> {
    let mut conn = setup().await?;

    // First insert.
    diesel::insert_into(chat_index_watermarks::table)
        .values((
            chat_index_watermarks::user_id.eq(1i64),
            chat_index_watermarks::chat_id.eq(7i64),
            chat_index_watermarks::oldest_seen.eq(100i64),
            chat_index_watermarks::newest_seen.eq(200i64),
        ))
        .execute(&mut conn)
        .await?;

    // Upsert: should shrink oldest_seen to 50, leave newest_seen at 200.
    diesel::insert_into(chat_index_watermarks::table)
        .values((
            chat_index_watermarks::user_id.eq(1i64),
            chat_index_watermarks::chat_id.eq(7i64),
            chat_index_watermarks::oldest_seen.eq(50i64),
            chat_index_watermarks::newest_seen.eq(150i64),
        ))
        .on_conflict((
            chat_index_watermarks::user_id,
            chat_index_watermarks::chat_id,
        ))
        .do_update()
        .set((
            chat_index_watermarks::oldest_seen.eq(scalar_min(
                chat_index_watermarks::oldest_seen,
                excluded(chat_index_watermarks::oldest_seen),
            )),
            chat_index_watermarks::newest_seen.eq(scalar_max(
                chat_index_watermarks::newest_seen,
                excluded(chat_index_watermarks::newest_seen),
            )),
        ))
        .execute(&mut conn)
        .await?;

    // Upsert: should grow newest_seen to 500.
    diesel::insert_into(chat_index_watermarks::table)
        .values((
            chat_index_watermarks::user_id.eq(1i64),
            chat_index_watermarks::chat_id.eq(7i64),
            chat_index_watermarks::oldest_seen.eq(120i64),
            chat_index_watermarks::newest_seen.eq(500i64),
        ))
        .on_conflict((
            chat_index_watermarks::user_id,
            chat_index_watermarks::chat_id,
        ))
        .do_update()
        .set((
            chat_index_watermarks::oldest_seen.eq(scalar_min(
                chat_index_watermarks::oldest_seen,
                excluded(chat_index_watermarks::oldest_seen),
            )),
            chat_index_watermarks::newest_seen.eq(scalar_max(
                chat_index_watermarks::newest_seen,
                excluded(chat_index_watermarks::newest_seen),
            )),
        ))
        .execute(&mut conn)
        .await?;

    let (oldest, newest): (i64, i64) = chat_index_watermarks::table
        .filter(chat_index_watermarks::user_id.eq(1i64))
        .filter(chat_index_watermarks::chat_id.eq(7i64))
        .select((
            chat_index_watermarks::oldest_seen,
            chat_index_watermarks::newest_seen,
        ))
        .first(&mut conn)
        .await?;
    assert_eq!((oldest, newest), (50, 500));
    Ok(())
}

#[tokio::test(flavor = "current_thread")]
async fn local_read_watermark_upsert_get_delete() -> Result<()> {
    let mut conn = setup().await?;
    for (chat, last) in [(10i64, 50i64), (10, 30), (11, 7)] {
        diesel::insert_into(local_read_watermarks::table)
            .values((
                local_read_watermarks::user_id.eq(1i64),
                local_read_watermarks::chat_id.eq(chat),
                local_read_watermarks::last_read_inbox_message_id.eq(last),
            ))
            .on_conflict((
                local_read_watermarks::user_id,
                local_read_watermarks::chat_id,
            ))
            .do_update()
            .set(
                local_read_watermarks::last_read_inbox_message_id.eq(scalar_max(
                    local_read_watermarks::last_read_inbox_message_id,
                    excluded(local_read_watermarks::last_read_inbox_message_id),
                )),
            )
            .execute(&mut conn)
            .await?;
    }
    let chat10: i64 = local_read_watermarks::table
        .filter(local_read_watermarks::user_id.eq(1i64))
        .filter(local_read_watermarks::chat_id.eq(10i64))
        .select(local_read_watermarks::last_read_inbox_message_id)
        .first(&mut conn)
        .await?;
    assert_eq!(chat10, 50, "upsert should advance, not retreat");

    let total: i64 = local_read_watermarks::table
        .count()
        .get_result(&mut conn)
        .await?;
    assert_eq!(total, 2);

    diesel::delete(
        local_read_watermarks::table
            .filter(local_read_watermarks::user_id.eq(1i64))
            .filter(local_read_watermarks::chat_id.eq(10i64)),
    )
    .execute(&mut conn)
    .await?;
    let total: i64 = local_read_watermarks::table
        .count()
        .get_result(&mut conn)
        .await?;
    assert_eq!(total, 1);
    Ok(())
}

#[tokio::test(flavor = "current_thread")]
async fn accounts_crud_and_set_active() -> Result<()> {
    let mut conn = setup().await?;

    for service in ["telegram", "slack", "signal"] {
        let next_id: i32 = accounts::table
            .select(diesel::dsl::max(accounts::id))
            .first::<Option<i32>>(&mut conn)
            .await?
            .map_or(0, |m| m + 1);
        diesel::insert_into(accounts::table)
            .values((
                accounts::id.eq(next_id),
                accounts::service.eq(service),
                accounts::is_active.eq(0),
                accounts::created_at.eq(0i64),
            ))
            .execute(&mut conn)
            .await?;
    }

    let ids: Vec<i32> = accounts::table
        .order(accounts::id.asc())
        .select(accounts::id)
        .load(&mut conn)
        .await?;
    assert_eq!(ids, vec![0, 1, 2]);

    // set_active_account: clear then set (as a transaction).
    use scoped_futures::ScopedFutureExt;
    conn.transaction::<_, diesel::result::Error, _>(|c| {
        async move {
            diesel::update(accounts::table)
                .set(accounts::is_active.eq(0))
                .execute(c)
                .await?;
            diesel::update(accounts::table.find(1i32))
                .set(accounts::is_active.eq(1))
                .execute(c)
                .await?;
            Ok(())
        }
        .scope_boxed()
    })
    .await?;

    let active_service: String = accounts::table
        .filter(accounts::is_active.eq(1))
        .limit(1)
        .select(accounts::service)
        .first(&mut conn)
        .await?;
    assert_eq!(active_service, "slack");

    // set_account_user_id.
    diesel::update(accounts::table.find(1i32))
        .set((
            accounts::user_id.eq(Some(42i64)),
            accounts::display_name.eq(Some("Bob")),
        ))
        .execute(&mut conn)
        .await?;
    let (uid, name): (Option<i64>, Option<String>) = accounts::table
        .find(1i32)
        .select((accounts::user_id, accounts::display_name))
        .first(&mut conn)
        .await?;
    assert_eq!((uid, name), (Some(42), Some("Bob".into())));
    Ok(())
}

#[tokio::test(flavor = "current_thread")]
async fn delete_account_cascade_via_subquery() -> Result<()> {
    let mut conn = setup().await?;
    diesel::insert_into(accounts::table)
        .values((
            accounts::id.eq(5),
            accounts::service.eq("telegram"),
            accounts::user_id.eq(Some(99i64)),
            accounts::is_active.eq(1),
            accounts::created_at.eq(0i64),
        ))
        .execute(&mut conn)
        .await?;
    // Batch-insert via `.values(&[…])` needs a Turso-side port of diesel's
    // SQLite-specific insert_with_default_for_sqlite.rs (748 lines) — see
    // PLAN.md known gap. Inserting one row at a time works.
    diesel::insert_into(account_socials::table)
        .values((
            account_socials::account_id.eq(5),
            account_socials::social_id.eq(1i64),
            account_socials::social_variant.eq(0),
        ))
        .execute(&mut conn)
        .await?;
    diesel::insert_into(account_socials::table)
        .values((
            account_socials::account_id.eq(5),
            account_socials::social_id.eq(2i64),
            account_socials::social_variant.eq(1),
        ))
        .execute(&mut conn)
        .await?;
    let sp_id = uuid::Uuid::now_v7().as_bytes().to_vec();
    diesel::insert_into(spaces::table)
        .values((
            spaces::id.eq(&sp_id),
            spaces::user_id.eq(99i64),
            spaces::name.eq("sp"),
            spaces::created_at.eq(0i64),
            spaces::updated_at.eq(0i64),
        ))
        .execute(&mut conn)
        .await?;
    diesel::insert_into(space_columns::table)
        .values((
            space_columns::id.eq(1i64),
            space_columns::space_id.eq(&sp_id),
            space_columns::column_order.eq(0),
            space_columns::chat_id.eq(Some(1i64)),
            space_columns::topic_id.eq(0i64),
            space_columns::created_at.eq(0i64),
            space_columns::updated_at.eq(0i64),
        ))
        .execute(&mut conn)
        .await?;

    // Cascade as a transaction mirroring delete_account() in meta.rs.
    use scoped_futures::ScopedFutureExt;
    conn.transaction::<_, diesel::result::Error, _>(|c| {
        async move {
            diesel::delete(account_socials::table.filter(account_socials::account_id.eq(5)))
                .execute(c)
                .await?;
            diesel::delete(
                space_columns::table.filter(
                    space_columns::space_id.eq_any(
                        spaces::table
                            .filter(spaces::user_id.eq(99i64))
                            .select(spaces::id),
                    ),
                ),
            )
            .execute(c)
            .await?;
            diesel::delete(spaces::table.filter(spaces::user_id.eq(99i64)))
                .execute(c)
                .await?;
            diesel::delete(accounts::table.find(5i32))
                .execute(c)
                .await?;
            Ok(())
        }
        .scope_boxed()
    })
    .await?;

    for (name, count) in [
        (
            "accounts",
            accounts::table.count().get_result::<i64>(&mut conn).await?,
        ),
        (
            "account_socials",
            account_socials::table
                .count()
                .get_result::<i64>(&mut conn)
                .await?,
        ),
        (
            "spaces",
            spaces::table.count().get_result::<i64>(&mut conn).await?,
        ),
        (
            "space_columns",
            space_columns::table
                .count()
                .get_result::<i64>(&mut conn)
                .await?,
        ),
    ] {
        assert_eq!(count, 0, "{name} should be empty");
    }
    Ok(())
}

#[tokio::test(flavor = "current_thread")]
async fn spaces_and_columns_crud() -> Result<()> {
    let mut conn = setup().await?;
    let sp = uuid::Uuid::now_v7().as_bytes().to_vec();

    diesel::insert_into(spaces::table)
        .values((
            spaces::id.eq(&sp),
            spaces::user_id.eq(1i64),
            spaces::name.eq("work"),
            spaces::created_at.eq(10i64),
            spaces::updated_at.eq(10i64),
        ))
        .execute(&mut conn)
        .await?;

    let first_name: String = spaces::table
        .filter(spaces::user_id.eq(1i64))
        .order(spaces::created_at.asc())
        .limit(1)
        .select(spaces::name)
        .first(&mut conn)
        .await?;
    assert_eq!(first_name, "work");

    // save_space_columns: delete + reinsert. One row at a time; see
    // batch-insert gap note above.
    for (id, order, chat) in [(1i64, 0, 100i64), (2i64, 1, 200i64)] {
        diesel::insert_into(space_columns::table)
            .values((
                space_columns::id.eq(id),
                space_columns::space_id.eq(&sp),
                space_columns::column_order.eq(order),
                space_columns::chat_id.eq(Some(chat)),
                space_columns::topic_id.eq(0i64),
                space_columns::created_at.eq(0i64),
                space_columns::updated_at.eq(0i64),
            ))
            .execute(&mut conn)
            .await?;
    }
    diesel::delete(space_columns::table.filter(space_columns::space_id.eq(&sp)))
        .execute(&mut conn)
        .await?;
    diesel::insert_into(space_columns::table)
        .values((
            space_columns::id.eq(10i64),
            space_columns::space_id.eq(&sp),
            space_columns::column_order.eq(0),
            space_columns::chat_id.eq(Some(300i64)),
            space_columns::topic_id.eq(0i64),
            space_columns::created_at.eq(0i64),
            space_columns::updated_at.eq(0i64),
        ))
        .execute(&mut conn)
        .await?;
    let chat_ids: Vec<Option<i64>> = space_columns::table
        .filter(space_columns::space_id.eq(&sp))
        .order(space_columns::column_order.asc())
        .select(space_columns::chat_id)
        .load(&mut conn)
        .await?;
    assert_eq!(chat_ids, vec![Some(300)]);

    // update_space_name.
    diesel::update(spaces::table.filter(spaces::id.eq(&sp)))
        .set(spaces::name.eq("weekend"))
        .execute(&mut conn)
        .await?;
    let name: String = spaces::table
        .filter(spaces::user_id.eq(1i64))
        .select(spaces::name)
        .first(&mut conn)
        .await?;
    assert_eq!(name, "weekend");

    // delete_space with child cleanup.
    diesel::delete(space_columns::table.filter(space_columns::space_id.eq(&sp)))
        .execute(&mut conn)
        .await?;
    diesel::delete(spaces::table.filter(spaces::id.eq(&sp)))
        .execute(&mut conn)
        .await?;
    let count: i64 = spaces::table.count().get_result(&mut conn).await?;
    assert_eq!(count, 0);
    Ok(())
}
