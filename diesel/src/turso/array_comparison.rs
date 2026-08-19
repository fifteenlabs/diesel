//! `IN (…)` and `NOT IN (…)` on Turso: one JSON bind for the whole list,
//! instead of one placeholder per element.
//!
//! # The problem this solves
//!
//! Diesel's default rendering of `eq_any` is
//! [`AnsiSqlArrayComparison`](crate::backend::sql_dialect::array_comparison::AnsiSqlArrayComparison):
//! one bind per element, so a three-element list is `IN (?, ?, ?)` and a
//! four-element list is `IN (?, ?, ?, ?)`. Those are *different SQL texts*,
//! and Turso — like SQLite — keys its compiled-program cache by the text. A
//! call site whose list length varies therefore mints a new program on every
//! distinct length, and the cache never helps it.
//!
//! This backend's cache — `StatementCache::admit` in `turso::connection`,
//! which is private, so this is a pointer rather than a link — makes
//! that worse in a way worth spelling out, because it is the reason a
//! chunked call site is the *guaranteed* bad case rather than merely a
//! likely one. Placeholder runs are collapsed into a "family", and the
//! second distinct text under one family key is taken as proof that the call
//! site is variadic — after which it is closed for good, for the life of the
//! process. Chunking a list to stay under a parameter ceiling produces
//! exactly two lengths, a full chunk and the tail, so it trips that test on
//! the very first call and the site never caches again.
//!
//! # The mechanism
//!
//! Bind the list as a single JSON array and let the engine unpack it:
//!
//! ```sql
//! -- integer columns
//! WHERE id IN (SELECT value FROM json_each(?))
//! -- BLOB columns
//! WHERE id IN (SELECT unhex(value) FROM json_each(?))
//! -- TEXT columns
//! WHERE id IN (SELECT CAST(unhex(value) AS TEXT) FROM json_each(?))
//! ```
//!
//! One bind, one SQL text, any length — including zero, which matters more
//! than it looks: the ANSI rendering special-cases an empty list to `1=0`
//! (and `NOT IN` to `1=1`), so a call site that is sometimes empty produces
//! a *third* text today. Here every length renders the same text, so the
//! empty case costs nothing and needs no branch.
//!
//! The decoding function is deliberately on the **JSON side** of the
//! comparison. Putting `hex(col)` on the column side would read the same and
//! would silently drop every index on that column, which is the kind of
//! regression that keeps returning correct answers while getting a hundred
//! times slower. Verified with `EXPLAIN QUERY PLAN` against real databases:
//! each form above adds `LIST SUBQUERY 1 / SCAN json_each` in front of a
//! plan that is otherwise identical to the placeholder form's — same index,
//! same driving direction, no sorter introduced — including for restrictions
//! written against expression indexes over `union_extract(…)`. Those
//! assertions live in `tests/turso/query_plans.rs`.
//!
//! # Why TEXT goes through `unhex` too
//!
//! This is the part that looks redundant and is not. **Do not "simplify"
//! `CAST(unhex(value) AS TEXT)` to a bare `value`.**
//!
//! SQLite's `json_each` returns string values *unescaped* — its `value`
//! column is the decoded SQL text. Turso's does not: it hands back the raw
//! JSON source of the string, escapes and all. Measured against the pinned
//! revision, binding a JSON array of these strings and reading `value` back:
//!
//! | bound              | SQLite returns     | Turso returns          |
//! |--------------------|--------------------|------------------------|
//! | `with "quotes"`    | `with "quotes"`    | `with \"quotes\"`      |
//! | `back\slash`       | `back\slash`       | `back\\slash`          |
//! | `new\nline`        | `new` `⏎` `line`   | `new\nline` (literal)  |
//! | `\u{1}`            | `\u{1}`            | `` (six chars)   |
//!
//! `atom` and `CAST(value AS TEXT)` have the same defect, and
//! `json_extract(value, '$')` is not a way out either — it errors with
//! "malformed JSON" on both engines, because a decoded string is not itself
//! valid JSON. So a bare `value` would compare the column against a
//! *differently spelled*
//! string, and the filter would match nothing — silently, with no error and
//! no wrong-looking SQL. A filter on an email subject or a channel name
//! containing an apostrophe-free but quote-bearing string would simply
//! return zero rows.
//!
//! Hex sidesteps the whole question, and it buys a second property worth
//! having: because every textual element is hex, the JSON this module emits
//! **cannot contain a character that needs escaping**. There is no escaping
//! code here to get wrong, in either direction.
//!
//! `tests/turso/array_comparison.rs` pins all four cases above end to end,
//! so removing the `unhex` turns the suite red rather than the answers
//! wrong.
//!
//! Reported upstream as tursodatabase/turso#8420 — it is the `json_each`
//! counterpart of their #5751, which fixed the same defect for `json_extract`
//! and `->>`. If it is fixed, the `Text` arm *could* become a bare `value`,
//! but there is no reason to make that change: hex costs about fifty
//! nanoseconds per element, and it keeps this module independent of which
//! Turso revision is pinned.
//!
//! # What is not converted
//!
//! `REAL` columns fall back to the ANSI rendering. A JSON number would have
//! to survive Turso's parser as an exactly-round-tripped `f64`, and that is
//! not a property this module can assert from the outside — whereas integers
//! are verified exact to `i64::MIN`/`i64::MAX` (`json_each` reports
//! `typeof` = `integer`, not `real`, so there is no f64 in that path at
//! all). An `IN` list of floats is a nonsense query anyway; the fallback
//! costs it nothing but the cache it never had.
//!
//! Subqueries — `eq_any(some_table::table.select(…))` — never reach this
//! module. They are [`Subselect`](crate::expression::subselect::Subselect),
//! not [`Many`], and were always a single fixed text.

use std::marker::PhantomData;

use crate::expression::array_comparison::{In, InExpression, Many, NotIn};
use crate::query_builder::{AstPass, QueryFragment};
use crate::result::QueryResult;
use crate::serialize::{self, IsNull, Output, ToSql};
use crate::sql_types::{HasSqlType, SingleValue};
use crate::turso::backend::{Turso, TursoJsonArrayComparison, TursoType};
use crate::turso::union::encode_field;

/// The SQL that turns one `json_each` row back into the storage class the
/// column is compared against, or `None` for a type with no exact JSON
/// round trip (see the module docs on `REAL`).
///
/// Read off [`HasSqlType`] rather than declared alongside the `ToSql` impls,
/// for the same reason
/// [`ddl_type_name`](crate::turso::union::ddl_type_name) is: this way the
/// decoding named in the SQL can only ever be the one for the storage class
/// the bind path actually produces. A separate declaration could drift from
/// it, and the symptom of that drift would be zero rows rather than an
/// error.
///
/// Every impl in this module calls this one function on the same `ST`, which
/// is what keeps the SQL text and the bind in agreement. They must agree: a
/// `Many` that decided to write one JSON bind while its enclosing `In` wrote
/// `IN (?, ?, ?)` would produce a statement with the wrong number of binds.
pub(crate) fn decode_expr<ST>() -> Option<&'static str>
where
    Turso: HasSqlType<ST>,
{
    match <Turso as HasSqlType<ST>>::metadata(&mut ()) {
        TursoType::Integer => Some("value"),
        TursoType::Binary => Some("unhex(value)"),
        TursoType::Text => Some("CAST(unhex(value) AS TEXT)"),
        TursoType::Real | TursoType::Null => None,
    }
}

/// The synthetic SQL type the whole list is bound under.
///
/// The list has to be serialized from a borrow of `Many::values` that
/// outlives `walk_ast`, because [`AstPass::push_bind_param`] takes `&'b U`
/// and the JSON string is built during serialization, not during rendering.
/// So the encoding lives in a `ToSql` impl and this type selects it —
/// exactly how the Postgres dialect binds its `IN` list as `Array<ST>`.
///
/// `ST` is carried so the impl knows which storage class to expect back from
/// each element; the bind itself is always TEXT.
#[derive(Debug, Clone, Copy)]
pub struct JsonList<ST>(PhantomData<ST>);

impl<ST: 'static> HasSqlType<JsonList<ST>> for Turso {
    fn metadata(_: &mut ()) -> TursoType {
        TursoType::Text
    }
}

/// Lowercase hex, written straight into `out`.
///
/// Hand-rolled rather than pulled from a crate because diesel does not
/// depend on one and this is the entire requirement. `unhex` accepts either
/// case; lowercase keeps the emitted JSON stable for the SQL-diff tests.
fn push_hex(out: &mut String, bytes: &[u8]) {
    const DIGITS: &[u8; 16] = b"0123456789abcdef";
    for b in bytes {
        out.push(DIGITS[(b >> 4) as usize] as char);
        out.push(DIGITS[(b & 0x0f) as usize] as char);
    }
}

impl<ST, I> ToSql<JsonList<ST>, Turso> for Vec<I>
where
    ST: 'static,
    I: ToSql<ST, Turso>,
    Turso: HasSqlType<ST>,
{
    fn to_sql(&self, out: &mut Output<'_, '_, Turso>) -> serialize::Result {
        let expected = <Turso as HasSqlType<ST>>::metadata(&mut ());
        // Two bytes of hex per byte of value, plus quotes and a comma: for
        // the UNION-encoded ids this exists for, the guess is close enough
        // to save the regrowth without over-allocating.
        let mut json = String::with_capacity(2 + self.len() * 16);
        json.push('[');
        for (i, value) in self.iter().enumerate() {
            if i > 0 {
                json.push(',');
            }
            match encode_field::<ST, I>(value)? {
                // A NULL element compares to nothing, exactly as a NULL
                // bind does in the placeholder form — and `NOT IN` over a
                // list containing one is NULL either way. No special case.
                turso::Value::Null => json.push_str("null"),
                turso::Value::Integer(n) if expected == TursoType::Integer => {
                    // Turso's JSON parser keeps integers as integers —
                    // verified at i64::MIN and i64::MAX, and at 2^53+1,
                    // where an f64 round trip would round. So this is
                    // exact, and `Display` is the exact spelling.
                    json.push_str(&n.to_string());
                }
                turso::Value::Blob(bytes) if expected == TursoType::Binary => {
                    json.push('"');
                    push_hex(&mut json, &bytes);
                    json.push('"');
                }
                turso::Value::Text(text) if expected == TursoType::Text => {
                    json.push('"');
                    push_hex(&mut json, text.as_bytes());
                    json.push('"');
                }
                // The storage class `HasSqlType` promised and the one the
                // value actually serialized to have diverged. `decode_expr`
                // has already written the SQL for the promised class, so
                // continuing would compare against a mis-decoded value and
                // quietly return no rows. Fail loudly instead.
                other => {
                    return Err(format!(
                        "IN list element serialized as {other:?}, but its SQL type reports \
                         {expected:?}; the json_each decoding written into the statement \
                         would not match it"
                    )
                    .into());
                }
            }
        }
        json.push(']');
        out.set_value(json);
        Ok(IsNull::No)
    }
}

/// `In` and `NotIn` change in exactly one respect: an **empty** list stops
/// being a special case.
///
/// Everything else is delegated to the ANSI rendering, which emits
/// `left IN (` + whatever `values` renders + `)`. That is deliberate, and it
/// is what keeps subqueries out of this code path entirely: `eq_any` over a
/// `SELECT` produces a [`Subselect`](crate::expression::subselect::Subselect),
/// not a [`Many`], and it renders itself. An earlier version of this module
/// wrapped `values` in `json_each(…)` here instead of inside `Many`, which
/// turned every subquery `IN` into the nonsense `json_each(SELECT …)`.
/// `subquery_in_lists_are_untouched` is the test that caught it.
///
/// The empty case has to move because the ANSI path renders it as the
/// constant `1=0` (and `NOT IN` as `1=1`) to avoid the syntax error that a
/// literal `IN ()` would be. `json_each('[]')` is simply an empty subquery,
/// so no constant is needed — and skipping it means a call site that is
/// sometimes empty renders one text rather than two.
///
/// `is_empty()` is a sound test for "this is a value list, and it has no
/// values": [`Subselect`](crate::expression::subselect::Subselect) answers
/// `false` unconditionally, so only a `Many` can ever report `true` here.
///
/// `!is_array()` is the same guard [`Many`]'s impl carries, and it has to be
/// on both or on neither. `Many` falls back to the ANSI rendering for an
/// array element type, which for an *empty* list renders nothing at all — so
/// an `In` that took the branch below over a list `Many` had declined would
/// emit `left IN ()`, a syntax error. Turso has no array types, so neither
/// guard fires today; they are here so that the pair cannot disagree if that
/// stops being true.
impl<T, U> QueryFragment<Turso, TursoJsonArrayComparison> for In<T, U>
where
    T: QueryFragment<Turso>,
    U: QueryFragment<Turso> + InExpression,
    Turso: HasSqlType<U::SqlType>,
{
    fn walk_ast<'b>(&'b self, mut out: AstPass<'_, 'b, Turso>) -> QueryResult<()> {
        if self.values.is_empty()
            && !self.values.is_array()
            && decode_expr::<U::SqlType>().is_some()
        {
            self.left.walk_ast(out.reborrow())?;
            out.push_sql(" IN (");
            self.values.walk_ast(out.reborrow())?;
            out.push_sql(")");
            return Ok(());
        }
        self.walk_ansi_ast(out)
    }
}

/// The `NOT IN` half of the same rule. See [`In`]'s impl above.
impl<T, U> QueryFragment<Turso, TursoJsonArrayComparison> for NotIn<T, U>
where
    T: QueryFragment<Turso>,
    U: QueryFragment<Turso> + InExpression,
    Turso: HasSqlType<U::SqlType>,
{
    fn walk_ast<'b>(&'b self, mut out: AstPass<'_, 'b, Turso>) -> QueryResult<()> {
        if self.values.is_empty()
            && !self.values.is_array()
            && decode_expr::<U::SqlType>().is_some()
        {
            self.left.walk_ast(out.reborrow())?;
            out.push_sql(" NOT IN (");
            self.values.walk_ast(out.reborrow())?;
            out.push_sql(")");
            return Ok(());
        }
        self.walk_ansi_ast(out)
    }
}

/// The value list itself: the subquery text, and the one bind under it.
///
/// This renders the *inside* of the parentheses `In` already emits, which is
/// why the fragment starts at `SELECT` and has no parens of its own.
///
/// Note the absence of `unsafe_to_cache_prepared`, which the ANSI path calls
/// unconditionally. That call is the entire reason a variadic `eq_any` was
/// never cached, and it is correct there — one bind per element really does
/// mean one text per length. Here the text is fixed, so the statement is
/// admitted on first sighting like any other.
impl<ST, I> QueryFragment<Turso, TursoJsonArrayComparison> for Many<ST, I>
where
    ST: SingleValue + 'static,
    I: ToSql<ST, Turso>,
    Turso: HasSqlType<ST>,
{
    fn walk_ast<'b>(&'b self, mut out: AstPass<'_, 'b, Turso>) -> QueryResult<()> {
        // `is_array` guards an element type that is itself an array, which
        // the ANSI path renders element-wise. Turso has no array types, so
        // this is unreachable today; it mirrors the Postgres dialect rather
        // than assuming that stays true. `In`/`NotIn` above carry the same
        // guard, and must: the two decide the same question, and an `In` that
        // took its empty-list branch over a list this impl had declined would
        // render `left IN ()`.
        match decode_expr::<ST>() {
            Some(decode) if !self.is_array() => {
                out.push_sql("SELECT ");
                out.push_sql(decode);
                out.push_sql(" FROM json_each(");
                out.push_bind_param::<JsonList<ST>, Vec<I>>(&self.values)?;
                out.push_sql(")");
                Ok(())
            }
            _ => self.walk_ansi_ast(out),
        }
    }
}

// If `Turso::ArrayComparison` were ever set back to the ANSI marker, the
// impls above would stop being reachable and every `eq_any` would silently
// go back to one bind per element — no compile error, no failing test, just
// the cache quietly not working again. This makes that a build break.
const _: fn(<Turso as crate::backend::SqlDialect>::ArrayComparison) =
    |_: TursoJsonArrayComparison| {};
