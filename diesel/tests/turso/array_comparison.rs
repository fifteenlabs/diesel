//! `IN (…)` binds its list as one JSON array, and that rendering returns
//! exactly the rows the placeholder rendering would have.
//!
//! The file is organised around the one failure this design exists to
//! prevent, which is not a slow query but a *silently empty result*. Turso's
//! `json_each` hands string values back still JSON-escaped where SQLite hands
//! them back decoded, so a list of strings routed through a bare `value`
//! would be compared against a differently-spelled string and match nothing
//! — no error, no malformed SQL, just zero rows. Everything textual therefore
//! travels as hex and comes back through `CAST(unhex(value) AS TEXT)`.
//!
//! That `unhex` is the thing a reader will want to delete. So the escape
//! cases are asserted end to end, on real rows, in
//! [`text_lists_survive_every_json_escape`] — remove the `unhex` from
//! `decode_expr` and that test fails, rather than some query somewhere
//! quietly returning nothing.
//!
//! The rest of the file pins the properties the rendering change rests on:
//! one SQL text for every list length (which is the entire point), the empty
//! list, `i64` values past the range an `f64` could carry, NULL elements,
//! `ne_all`, and the `REAL` fallback that deliberately keeps the old
//! rendering.

use anyhow::Result;
use diesel::async_dsl::RunQueryDsl;
use diesel::connection::{AsyncConnection, SimpleAsyncConnection};
use diesel::prelude::*;
use diesel::turso::TursoConnection;

use crate::sql_text::rendered;

diesel::table! {
    items (id) {
        id -> BigInt,
        name -> Text,
        tag -> Binary,
        score -> Double,
    }
}

/// Four rows whose `name`s are the JSON escapes that separate Turso's
/// `json_each` from SQLite's, plus two that need no escaping at all — a
/// control, so a test that passed by escaping *everything* into oblivion
/// would still be caught.
const NAMES: [&str; 6] = [
    "plain",
    "with \"quotes\"",
    "back\\slash",
    "new\nline\ttab",
    "\u{1}\u{7f}",
    "emoji 🙂 ümlaut",
];

async fn setup() -> Result<TursoConnection> {
    let mut conn = TursoConnection::establish(":memory:").await?;
    conn.batch_execute(
        "CREATE TABLE items(
             id INTEGER PRIMARY KEY,
             name TEXT NOT NULL,
             tag BLOB NOT NULL,
             score REAL NOT NULL
         ) STRICT;",
    )
    .await?;
    for (i, name) in NAMES.iter().enumerate() {
        diesel::insert_into(items::table)
            .values((
                items::id.eq(i as i64),
                items::name.eq(*name),
                items::tag.eq(vec![0xdeu8, 0xad, i as u8]),
                items::score.eq(i as f64 + 0.5),
            ))
            .execute(&mut conn)
            .await?;
    }
    Ok(conn)
}

// ── 1. the failure this design exists to prevent ─────────────────────────

/// Every JSON escape survives the round trip, on real rows.
///
/// This is the test that goes red if the `unhex` is taken out of the `Text`
/// arm of `decode_expr`. Each name is fetched by filtering on itself, so a
/// mis-decoded comparison returns nothing and the assertion names which
/// string was lost.
#[tokio::test(flavor = "current_thread")]
async fn text_lists_survive_every_json_escape() -> Result<()> {
    let mut conn = setup().await?;

    for name in NAMES {
        let got: Vec<String> = items::table
            .filter(items::name.eq_any(vec![name.to_string()]))
            .select(items::name)
            .load(&mut conn)
            .await?;
        assert_eq!(
            got,
            vec![name.to_string()],
            "a one-element IN list on {name:?} did not find its own row — the \
             json_each decoding is not returning the string that was bound"
        );
    }

    // And all of them at once, which is the shape a real call site has.
    let all: Vec<String> = NAMES.iter().map(|s| s.to_string()).collect();
    let mut got: Vec<String> = items::table
        .filter(items::name.eq_any(all.clone()))
        .select(items::name)
        .order(items::id.asc())
        .load(&mut conn)
        .await?;
    got.sort();
    let mut want = all;
    want.sort();
    assert_eq!(got, want, "a six-element IN list lost at least one string");
    Ok(())
}

/// The rendering names `unhex`, so a change that drops it fails here too —
/// at the SQL level, with a message that says what to look at — instead of
/// only showing up as missing rows above.
#[tokio::test(flavor = "current_thread")]
async fn text_lists_decode_through_unhex() -> Result<()> {
    let query = items::table
        .filter(items::name.eq_any(vec!["a".to_string()]))
        .select(items::id);
    let sql = rendered(&query);
    assert!(
        sql.contains("IN (SELECT CAST(unhex(value) AS TEXT) FROM json_each(?))"),
        "a TEXT list must decode through CAST(unhex(value) AS TEXT); Turso's \
         json_each does not unescape strings, so a bare `value` silently \
         matches nothing. Rendered:\n  {sql}"
    );
    Ok(())
}

// ── 2. one text per call site, whatever the length ───────────────────────

/// The point of the whole exercise: the SQL text does not move with the list
/// length. Under the ANSI rendering these would be six different texts, and
/// this backend's cache would mark the call site variadic on the second one
/// and never cache it again.
#[tokio::test(flavor = "current_thread")]
async fn every_list_length_renders_one_text() -> Result<()> {
    let mut texts = std::collections::BTreeSet::new();
    for n in [0usize, 1, 2, 3, 50, 999] {
        let values: Vec<i64> = (0..n as i64).collect();
        texts.insert(rendered(
            &items::table
                .filter(items::id.eq_any(values))
                .select(items::id),
        ));
    }
    assert_eq!(
        texts.len(),
        1,
        "an IN list should render one text at every length, got:\n  {}",
        texts.into_iter().collect::<Vec<_>>().join("\n  ")
    );
    Ok(())
}

/// The cache agrees — no family is ever marked variadic, and the statement is
/// admitted once and hit thereafter.
///
/// Asserted through `statement_cache_stats` rather than by timing, because a
/// cache that silently stopped working returns exactly the right rows.
#[tokio::test(flavor = "current_thread")]
async fn a_varying_list_length_never_marks_the_family_variadic() -> Result<()> {
    let mut conn = setup().await?;
    let before = conn.statement_cache_stats();

    for n in [1usize, 2, 3, 4, 5, 40, 1, 300] {
        let values: Vec<i64> = (0..n as i64).collect();
        let _: Vec<i64> = items::table
            .filter(items::id.eq_any(values))
            .select(items::id)
            .load(&mut conn)
            .await?;
    }

    let after = conn.statement_cache_stats();
    assert_eq!(
        after.variadic_families, before.variadic_families,
        "eight different list lengths must not produce a variadic family"
    );
    assert!(
        after.hits > before.hits,
        "the statement should have been served from the cache after its first \
         execution; stats went {before:?} -> {after:?}"
    );
    Ok(())
}

// ── 3. the value domains ─────────────────────────────────────────────────

/// `i64` past the range an `f64` can carry, which is where a JSON number
/// would round if Turso's parser went through a double. It does not — but
/// the property is the one that would silently corrupt message ids, so it is
/// asserted rather than assumed.
#[tokio::test(flavor = "current_thread")]
async fn integer_lists_keep_full_i64_precision() -> Result<()> {
    let mut conn = TursoConnection::establish(":memory:").await?;
    conn.batch_execute("CREATE TABLE items(id INTEGER PRIMARY KEY, name TEXT NOT NULL, tag BLOB NOT NULL, score REAL NOT NULL) STRICT;")
        .await?;
    let edges = [
        i64::MAX,
        i64::MIN,
        9_007_199_254_740_993,
        -9_007_199_254_740_993,
    ];
    for (i, v) in edges.iter().enumerate() {
        diesel::insert_into(items::table)
            .values((
                items::id.eq(*v),
                items::name.eq(format!("n{i}")),
                items::tag.eq(vec![0u8]),
                items::score.eq(0.0f64),
            ))
            .execute(&mut conn)
            .await?;
    }

    let mut got: Vec<i64> = items::table
        .filter(items::id.eq_any(edges.to_vec()))
        .select(items::id)
        .load(&mut conn)
        .await?;
    got.sort_unstable();
    let mut want = edges.to_vec();
    want.sort_unstable();
    assert_eq!(got, want, "an i64 IN list lost precision through JSON");
    Ok(())
}

/// BLOB lists — the shape the app's UNION-encoded ids take — decode through
/// `unhex` with no `CAST`, so the comparison is against the raw indexed
/// column.
#[tokio::test(flavor = "current_thread")]
async fn blob_lists_round_trip() -> Result<()> {
    let mut conn = setup().await?;
    let want = vec![vec![0xdeu8, 0xad, 1], vec![0xdeu8, 0xad, 3]];
    let mut got: Vec<Vec<u8>> = items::table
        .filter(items::tag.eq_any(want.clone()))
        .select(items::tag)
        .load(&mut conn)
        .await?;
    got.sort();
    assert_eq!(got, want);

    let sql = rendered(
        &items::table
            .filter(items::tag.eq_any(want))
            .select(items::id),
    );
    assert!(
        sql.contains("IN (SELECT unhex(value) FROM json_each(?))"),
        "a BLOB list decodes with unhex and no CAST, so the comparison stays \
         against the raw column. Rendered:\n  {sql}"
    );
    Ok(())
}

// ── 4. the edges the ANSI rendering had to special-case ──────────────────

/// An empty list matches nothing and an empty `ne_all` matches everything —
/// the same answers the ANSI `1=0` / `1=1` special cases give, but reached
/// without a special case, and therefore without a third SQL text.
#[tokio::test(flavor = "current_thread")]
async fn an_empty_list_needs_no_special_case() -> Result<()> {
    let mut conn = setup().await?;

    let none: Vec<i64> = items::table
        .filter(items::id.eq_any(Vec::<i64>::new()))
        .select(items::id)
        .load(&mut conn)
        .await?;
    assert!(none.is_empty(), "IN over an empty list matches nothing");

    let all: Vec<i64> = items::table
        .filter(items::id.ne_all(Vec::<i64>::new()))
        .select(items::id)
        .load(&mut conn)
        .await?;
    assert_eq!(
        all.len(),
        NAMES.len(),
        "NOT IN an empty list matches everything"
    );

    assert_eq!(
        rendered(
            &items::table
                .filter(items::id.eq_any(Vec::<i64>::new()))
                .select(items::id)
        ),
        rendered(
            &items::table
                .filter(items::id.eq_any(vec![1i64, 2]))
                .select(items::id)
        ),
        "the empty list must render the same text as any other length"
    );
    Ok(())
}

/// `ne_all` takes the same route, and a NULL element makes the whole
/// comparison NULL — which a `WHERE` reads as false. That is what the
/// placeholder rendering does with a NULL bind, and it is preserved here
/// rather than accidentally changed by routing through a subquery.
#[tokio::test(flavor = "current_thread")]
async fn ne_all_and_null_elements_behave_as_before() -> Result<()> {
    let mut conn = setup().await?;

    let mut got: Vec<i64> = items::table
        .filter(items::id.ne_all(vec![0i64, 1, 2]))
        .select(items::id)
        .load(&mut conn)
        .await?;
    got.sort_unstable();
    assert_eq!(got, vec![3, 4, 5]);

    let sql = rendered(
        &items::table
            .filter(items::id.ne_all(vec![1i64]))
            .select(items::id),
    );
    assert!(
        sql.contains("NOT IN (SELECT value FROM json_each(?))"),
        "ne_all takes the json_each route too. Rendered:\n  {sql}"
    );

    // A NULL in the list: `IN` can never match it, `NOT IN` is NULL.
    let with_null: Vec<Option<i64>> = vec![Some(1), None];
    let hit: Vec<i64> = items::table
        .filter(items::id.nullable().eq_any(with_null.clone()))
        .select(items::id)
        .load(&mut conn)
        .await?;
    assert_eq!(
        hit,
        vec![1],
        "a NULL element matches nothing but does not hide the others"
    );

    let miss: Vec<i64> = items::table
        .filter(items::id.nullable().ne_all(with_null))
        .select(items::id)
        .load(&mut conn)
        .await?;
    assert!(
        miss.is_empty(),
        "NOT IN a list containing NULL is NULL for every row, which WHERE reads as false"
    );
    Ok(())
}

// ── 5. the deliberate fallback ───────────────────────────────────────────

/// `REAL` keeps the ANSI rendering, because an exact JSON round trip for
/// `f64` is not a property this backend can assert. The fallback has to
/// actually work, and it has to be the *placeholder* form — a half-converted
/// statement, JSON text with placeholder binds, would be a wrong-arity
/// statement rather than a slow one.
#[tokio::test(flavor = "current_thread")]
async fn real_columns_fall_back_to_placeholders() -> Result<()> {
    let mut conn = setup().await?;

    let sql = rendered(
        &items::table
            .filter(items::score.eq_any(vec![0.5f64, 1.5]))
            .select(items::id),
    );
    assert!(
        sql.contains("IN (?, ?)"),
        "a REAL list keeps the ANSI rendering. Rendered:\n  {sql}"
    );
    assert!(
        !sql.contains("json_each"),
        "and must not be half-converted:\n  {sql}"
    );

    let mut got: Vec<i64> = items::table
        .filter(items::score.eq_any(vec![0.5f64, 2.5]))
        .select(items::id)
        .load(&mut conn)
        .await?;
    got.sort_unstable();
    assert_eq!(
        got,
        vec![0, 2],
        "the fallback still has to return the right rows"
    );
    Ok(())
}

/// The empty list on the *fallback* path, which is a different branch from
/// the empty list in `an_empty_list_needs_no_special_case` above and the only
/// thing holding the `decode_expr(…).is_some()` clause in `In`/`NotIn`.
///
/// Without that clause an empty `REAL` list takes the `json_each` branch —
/// which for a list `Many` has declined to convert renders *nothing at all*
/// between the parens — and the statement becomes `"m"."f" IN ()`, a syntax
/// error. Every other test in this file uses an integer, text or blob column,
/// so all of them stay green while that happens.
#[tokio::test(flavor = "current_thread")]
async fn an_empty_real_list_keeps_the_ansi_constant() -> Result<()> {
    let mut conn = setup().await?;

    let sql = rendered(
        &items::table
            .filter(items::score.eq_any(Vec::<f64>::new()))
            .select(items::id),
    );
    assert!(
        sql.contains("1=0") && !sql.contains("IN ()"),
        "an empty REAL list has to keep the ANSI `1=0`; the json_each branch \
         would render `IN ()`. Rendered:\n  {sql}"
    );

    let none: Vec<i64> = items::table
        .filter(items::score.eq_any(Vec::<f64>::new()))
        .select(items::id)
        .load(&mut conn)
        .await?;
    assert!(none.is_empty(), "and it still matches nothing");

    let all: Vec<i64> = items::table
        .filter(items::score.ne_all(Vec::<f64>::new()))
        .select(items::id)
        .load(&mut conn)
        .await?;
    assert_eq!(
        all.len(),
        NAMES.len(),
        "and its NOT IN still matches everything"
    );
    Ok(())
}

/// A subquery `eq_any` is a `Subselect`, not a `Many`, so it never reaches
/// this module and its text was always fixed. Pinned so a future change to
/// the dialect impls cannot quietly start rewriting subqueries into
/// `json_each` of nothing.
#[tokio::test(flavor = "current_thread")]
async fn subquery_in_lists_are_untouched() -> Result<()> {
    let sql = rendered(
        &items::table
            .filter(items::id.eq_any(items::table.select(items::id).filter(items::id.gt(2))))
            .select(items::id),
    );
    assert!(
        !sql.contains("json_each"),
        "an IN over a subquery is not a value list. Rendered:\n  {sql}"
    );
    Ok(())
}
