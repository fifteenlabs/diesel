//! What the three shipped backends render, shape by shape, as a golden file.
//!
//! # Why this exists
//!
//! This fork carries a fourth backend and, since the `diesel-async` fold, the
//! whole async half of the crate as well. Both changes reach into shared
//! rendering code — `SqlDialect` gained associated types, `AstPass` gained a
//! scoped flag, `limit_clause` grew a second `QueryFragment` impl — and the
//! failure mode of getting that wrong is not a compile error. It is
//! PostgreSQL emitting one more pair of parentheses than it used to, or MySQL
//! losing a `LIMIT`, in a query that still returns the right rows on the
//! developer's machine and the wrong plan on a production table.
//!
//! So the bar for any change to the shared path is that these three backends
//! render *byte for byte* what they rendered before it. This file is how that
//! is checked: every shape below is rendered for Pg, MySQL and SQLite and
//! compared against `backend_rendering.golden`. Bind values are deliberately
//! included — `debug_query` prints them after the SQL — because a bind that
//! moves position is exactly the class of defect the subselect-`LIMIT` fix
//! was about.
//!
//! It needs no database: `debug_query` is pure rendering, so this runs with
//! `postgres_backend` and `mysql_backend` rather than `postgres` and `mysql`
//! and links no C client library.
//!
//! # When it fails
//!
//! A diff here is either a bug or a decision. If it is a decision, look at
//! every line that moved before regenerating: `UPDATE_GOLDEN=1 cargo test
//! --test backend_rendering` rewrites the file, and it is only safe to do
//! that once you can say what each change is.

#![cfg(all(
    feature = "postgres_backend",
    feature = "mysql_backend",
    feature = "sqlite"
))]

use diesel::backend::Backend;
use diesel::prelude::*;
use diesel::query_builder::{QueryFragment, QueryId};

diesel::table! {
    users (id) {
        id -> Integer,
        name -> Text,
        hair_color -> Nullable<Text>,
        score -> BigInt,
    }
}

diesel::table! {
    posts (id) {
        id -> Integer,
        user_id -> Integer,
        title -> Text,
        body -> Nullable<Text>,
    }
}

diesel::table! {
    comments (id) {
        id -> Integer,
        post_id -> Integer,
        body -> Text,
    }
}

diesel::joinable!(posts -> users (user_id));
diesel::joinable!(comments -> posts (post_id));
diesel::allow_tables_to_appear_in_same_query!(users, posts, comments);

fn render<DB, Q>(q: Q) -> String
where
    DB: Backend + Default,
    DB::QueryBuilder: Default,
    Q: QueryFragment<DB> + QueryId,
{
    diesel::debug_query::<DB, _>(&q)
        .to_string()
        .replace('\n', " ")
}

/// One shape, rendered for all three backends.
macro_rules! all3 {
    ($out:expr, $name:literal, $q:expr) => {{
        $out.push(format!(
            "{}\tPg\t{}",
            $name,
            render::<diesel::pg::Pg, _>($q)
        ));
        $out.push(format!(
            "{}\tMysql\t{}",
            $name,
            render::<diesel::mysql::Mysql, _>($q)
        ));
        $out.push(format!(
            "{}\tSqlite\t{}",
            $name,
            render::<diesel::sqlite::Sqlite, _>($q)
        ));
    }};
}

/// A shape MySQL cannot render — upserts and `RETURNING`.
macro_rules! pg_sqlite {
    ($out:expr, $name:literal, $q:expr) => {{
        $out.push(format!(
            "{}\tPg\t{}",
            $name,
            render::<diesel::pg::Pg, _>($q)
        ));
        $out.push(format!(
            "{}\tSqlite\t{}",
            $name,
            render::<diesel::sqlite::Sqlite, _>($q)
        ));
    }};
}

#[allow(clippy::vec_init_then_push)]
fn rendered() -> Vec<String> {
    let mut o = Vec::new();

    // ---- projection and filtering ---------------------------------------
    all3!(o, "select_all", users::table);
    all3!(o, "select_one_column", users::table.select(users::name));
    all3!(
        o,
        "select_tuple",
        users::table.select((users::id, users::name))
    );
    all3!(o, "filter_eq", users::table.filter(users::id.eq(1)));
    all3!(o, "filter_ne", users::table.filter(users::id.ne(1)));
    all3!(
        o,
        "filter_gt_lt",
        users::table.filter(users::id.gt(1).and(users::id.lt(9)))
    );
    all3!(
        o,
        "filter_or",
        users::table.filter(users::id.eq(1).or(users::id.eq(2)))
    );
    all3!(
        o,
        "filter_not",
        users::table.filter(diesel::dsl::not(users::id.eq(1)))
    );
    all3!(
        o,
        "filter_is_null",
        users::table.filter(users::hair_color.is_null())
    );
    all3!(
        o,
        "filter_is_not_null",
        users::table.filter(users::hair_color.is_not_null())
    );
    all3!(
        o,
        "filter_between",
        users::table.filter(users::id.between(1, 5))
    );
    all3!(
        o,
        "filter_not_between",
        users::table.filter(users::id.not_between(1, 5))
    );
    all3!(
        o,
        "filter_like",
        users::table.filter(users::name.like("a%"))
    );
    all3!(
        o,
        "filter_not_like",
        users::table.filter(users::name.not_like("a%"))
    );
    all3!(
        o,
        "filter_eq_any",
        users::table.filter(users::id.eq_any(vec![1, 2, 3]))
    );
    all3!(
        o,
        "filter_ne_all",
        users::table.filter(users::id.ne_all(vec![1, 2, 3]))
    );
    all3!(
        o,
        "filter_eq_any_empty",
        users::table.filter(users::id.eq_any(Vec::<i32>::new()))
    );
    all3!(
        o,
        "filter_chained",
        users::table
            .filter(users::id.eq(1))
            .filter(users::name.eq("a"))
    );
    all3!(
        o,
        "filter_nullable_eq",
        users::table.filter(users::hair_color.eq(Some("brown")))
    );

    // ---- ordering, limit, offset ----------------------------------------
    all3!(o, "order_asc", users::table.order(users::name.asc()));
    all3!(o, "order_desc", users::table.order(users::name.desc()));
    all3!(
        o,
        "order_multi",
        users::table.order((users::name.asc(), users::id.desc()))
    );
    all3!(
        o,
        "order_nullable",
        users::table.order(users::hair_color.desc())
    );
    all3!(o, "limit", users::table.limit(10));
    all3!(o, "offset_with_limit", users::table.limit(10).offset(20));
    all3!(
        o,
        "limit_offset_ordered",
        users::table.order(users::id.desc()).limit(50).offset(100)
    );
    all3!(o, "distinct", users::table.distinct());
    all3!(
        o,
        "for_update_free_order_limit",
        users::table.order(users::score.desc()).limit(1)
    );

    // ---- joins -----------------------------------------------------------
    all3!(o, "inner_join", users::table.inner_join(posts::table));
    all3!(o, "left_join", users::table.left_join(posts::table));
    all3!(
        o,
        "inner_join_select",
        users::table
            .inner_join(posts::table)
            .select((users::name, posts::title))
    );
    all3!(
        o,
        "inner_join_filter",
        users::table
            .inner_join(posts::table)
            .filter(posts::title.eq("x"))
    );
    all3!(
        o,
        "left_join_is_null",
        users::table
            .left_join(posts::table)
            .filter(posts::id.is_null())
    );
    all3!(
        o,
        "join_on",
        users::table
            .left_join(posts::table.on(posts::user_id.eq(users::id).and(posts::title.eq("x"))))
    );
    all3!(
        o,
        "nested_join",
        users::table.inner_join(posts::table.inner_join(comments::table))
    );
    all3!(
        o,
        "join_ordered_limited",
        users::table
            .inner_join(posts::table)
            .order(posts::id.desc())
            .limit(20)
    );

    // ---- aggregates, grouping -------------------------------------------
    all3!(o, "count", users::table.count());
    all3!(
        o,
        "count_star",
        users::table.select(diesel::dsl::count_star())
    );
    all3!(
        o,
        "count_column",
        users::table.select(diesel::dsl::count(users::id))
    );
    all3!(
        o,
        "count_distinct",
        users::table.select(diesel::dsl::count_distinct(users::name))
    );
    all3!(
        o,
        "sum",
        users::table.select(diesel::dsl::sum(users::score))
    );
    all3!(
        o,
        "avg",
        users::table.select(diesel::dsl::avg(users::score))
    );
    all3!(
        o,
        "max",
        users::table.select(diesel::dsl::max(users::score))
    );
    all3!(
        o,
        "min",
        users::table.select(diesel::dsl::min(users::score))
    );
    all3!(
        o,
        "group_by",
        users::table
            .group_by(users::hair_color)
            .select((users::hair_color, diesel::dsl::count_star()))
    );
    all3!(
        o,
        "group_by_having",
        users::table
            .group_by(users::hair_color)
            .having(diesel::dsl::count_star().gt(1))
            .select(users::hair_color)
    );

    // ---- subqueries ------------------------------------------------------
    all3!(
        o,
        "exists",
        users::table.filter(diesel::dsl::exists(
            posts::table.filter(posts::user_id.eq(users::id))
        ))
    );
    all3!(
        o,
        "not_exists",
        users::table.filter(diesel::dsl::not(diesel::dsl::exists(
            posts::table.filter(posts::user_id.eq(users::id))
        )))
    );
    all3!(
        o,
        "in_subselect",
        users::table.filter(users::id.eq_any(posts::table.select(posts::user_id)))
    );
    all3!(
        o,
        "scalar_subselect_eq",
        users::table.filter(
            users::id
                .nullable()
                .eq(posts::table.select(posts::user_id).single_value())
        )
    );
    all3!(
        o,
        "scalar_subselect_limited",
        users::table.filter(
            users::id.nullable().eq(posts::table
                .select(posts::user_id)
                .order(posts::id.desc())
                .limit(1)
                .single_value())
        )
    );
    all3!(
        o,
        "subselect_with_offset",
        users::table
            .filter(users::id.eq_any(posts::table.select(posts::user_id).limit(5).offset(2)))
    );

    // ---- expressions -----------------------------------------------------
    all3!(
        o,
        "sql_concat_free_case",
        diesel::dsl::case_when::<_, _, diesel::sql_types::Integer>(users::id.eq(1), 10)
            .otherwise(20)
    );
    all3!(
        o,
        "select_literal",
        users::table.select(1.into_sql::<diesel::sql_types::Integer>())
    );
    all3!(
        o,
        "nullable_column",
        users::table.select(users::name.nullable())
    );
    all3!(
        o,
        "assume_not_null",
        users::table.select(users::hair_color.assume_not_null())
    );
    all3!(o, "arithmetic", users::table.select(users::score + 1));
    all3!(
        o,
        "arithmetic_nested",
        users::table.filter((users::score - 1).gt(3))
    );

    // ---- inserts ---------------------------------------------------------
    all3!(
        o,
        "insert_single",
        diesel::insert_into(users::table).values(users::name.eq("a"))
    );
    all3!(
        o,
        "insert_tuple",
        diesel::insert_into(users::table).values((users::name.eq("a"), users::score.eq(1i64)))
    );
    // Batch insert is deliberately not `all3!`: SQLite renders it through a
    // different fragment entirely (`BatchInsertViaValuesList` is not
    // implemented for it), so there is no one type to render three ways.
    o.push(format!(
        "insert_batch\tPg\t{}",
        render::<diesel::pg::Pg, _>(diesel::insert_into(users::table).values(vec![
            (users::name.eq("a"), users::score.eq(1i64)),
            (users::name.eq("b"), users::score.eq(2i64))
        ]))
    ));
    o.push(format!(
        "insert_batch\tMysql\t{}",
        render::<diesel::mysql::Mysql, _>(diesel::insert_into(users::table).values(vec![
            (users::name.eq("a"), users::score.eq(1i64)),
            (users::name.eq("b"), users::score.eq(2i64))
        ]))
    ));
    all3!(
        o,
        "insert_default_values",
        diesel::insert_into(users::table).default_values()
    );
    all3!(
        o,
        "insert_from_select",
        users::table
            .select((users::name, users::score))
            .insert_into(users::table)
            .into_columns((users::name, users::score))
    );

    // ---- updates and deletes ---------------------------------------------
    all3!(
        o,
        "update_one",
        diesel::update(users::table).set(users::name.eq("a"))
    );
    all3!(
        o,
        "update_filtered",
        diesel::update(users::table.filter(users::id.eq(1))).set(users::name.eq("a"))
    );
    all3!(
        o,
        "update_multi_column",
        diesel::update(users::table).set((users::name.eq("a"), users::score.eq(2i64)))
    );
    all3!(
        o,
        "update_expression",
        diesel::update(users::table).set(users::score.eq(users::score + 1))
    );
    all3!(
        o,
        "update_null",
        diesel::update(users::table).set(users::hair_color.eq(None::<String>))
    );
    all3!(o, "delete_all", diesel::delete(users::table));
    all3!(
        o,
        "delete_filtered",
        diesel::delete(users::table.filter(users::id.eq(1)))
    );

    // ---- upserts and RETURNING (not MySQL) -------------------------------
    pg_sqlite!(
        o,
        "insert_returning",
        diesel::insert_into(users::table)
            .values(users::name.eq("a"))
            .returning(users::id)
    );
    pg_sqlite!(
        o,
        "update_returning",
        diesel::update(users::table)
            .set(users::name.eq("a"))
            .returning(users::id)
    );
    pg_sqlite!(
        o,
        "delete_returning",
        diesel::delete(users::table).returning(users::id)
    );
    pg_sqlite!(
        o,
        "on_conflict_do_nothing",
        diesel::insert_into(users::table)
            .values(users::name.eq("a"))
            .on_conflict_do_nothing()
    );
    pg_sqlite!(
        o,
        "on_conflict_target_do_nothing",
        diesel::insert_into(users::table)
            .values(users::name.eq("a"))
            .on_conflict(users::id)
            .do_nothing()
    );
    pg_sqlite!(
        o,
        "on_conflict_do_update",
        diesel::insert_into(users::table)
            .values(users::name.eq("a"))
            .on_conflict(users::id)
            .do_update()
            .set(users::name.eq("b"))
    );
    pg_sqlite!(
        o,
        "on_conflict_do_update_excluded",
        diesel::insert_into(users::table)
            .values(users::name.eq("a"))
            .on_conflict(users::id)
            .do_update()
            .set(users::name.eq(diesel::upsert::excluded(users::name)))
    );
    // `filter_target` is a partial-index conflict target, which only
    // PostgreSQL has.
    o.push(format!(
        "on_conflict_where\tPg\t{}",
        render::<diesel::pg::Pg, _>(
            diesel::insert_into(users::table)
                .values(users::name.eq("a"))
                .on_conflict(users::id)
                .filter_target(users::score.gt(0i64))
                .do_nothing()
        )
    ));

    // ---- combinations ----------------------------------------------------
    all3!(
        o,
        "union",
        users::table
            .select(users::id)
            .union(posts::table.select(posts::id))
    );
    all3!(
        o,
        "union_all",
        users::table
            .select(users::id)
            .union_all(posts::table.select(posts::id))
    );

    // ---- boxed -----------------------------------------------------------
    o.push(format!(
        "boxed_filtered\tPg\t{}",
        render::<diesel::pg::Pg, _>(
            users::table
                .into_boxed::<diesel::pg::Pg>()
                .filter(users::id.eq(1))
                .limit(3)
        )
    ));
    o.push(format!(
        "boxed_filtered\tMysql\t{}",
        render::<diesel::mysql::Mysql, _>(
            users::table
                .into_boxed::<diesel::mysql::Mysql>()
                .filter(users::id.eq(1))
                .limit(3)
        )
    ));
    o.push(format!(
        "boxed_filtered\tSqlite\t{}",
        render::<diesel::sqlite::Sqlite, _>(
            users::table
                .into_boxed::<diesel::sqlite::Sqlite>()
                .filter(users::id.eq(1))
                .limit(3)
        )
    ));

    o
}

#[test]
fn the_three_shipped_backends_render_what_they_always_did() {
    let golden_path = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/tests/backend_rendering.golden"
    );
    let actual = rendered().join("\n") + "\n";

    if std::env::var("UPDATE_GOLDEN").is_ok() {
        std::fs::write(golden_path, &actual).expect("write golden");
        return;
    }

    let expected = std::fs::read_to_string(golden_path).unwrap_or_default();
    if expected == actual {
        return;
    }

    let exp: Vec<&str> = expected.lines().collect();
    let act: Vec<&str> = actual.lines().collect();
    let mut diff = Vec::new();
    for i in 0..exp.len().max(act.len()) {
        let e = exp.get(i).copied().unwrap_or("<missing>");
        let a = act.get(i).copied().unwrap_or("<missing>");
        if e != a {
            diff.push(format!("  line {}:\n    was: {e}\n    now: {a}", i + 1));
        }
    }
    panic!(
        "rendering changed for {} of {} shape/backend pairs:\n{}\n\nIf every one of these is \
         intended, regenerate with UPDATE_GOLDEN=1.",
        diff.len(),
        act.len(),
        diff.join("\n")
    );
}
