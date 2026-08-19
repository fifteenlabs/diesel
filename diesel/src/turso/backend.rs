//! `Turso` — the `crate::backend::Backend` marker.
//!
//! Turso targets SQLite SQL, so the `SqlDialect` mirrors diesel's SQLite
//! backend everywhere but the join `FROM` clause, where Turso's parser is
//! the stricter of the two (see `JoinFromClauseSyntax` below). The bind
//! collector and raw value types are ours (they wrap `turso::Value`
//! directly rather than going through a byte buffer).

use crate::backend::{
    sql_dialect, Backend, DieselReserveSpecialization, SqlDialect, TrustedBackend,
};
use crate::query_builder::QueryBuilder;
use crate::sql_types::{self, HasSqlType, TypeMetadata};

use crate::turso::bind::TursoBindCollector;
use crate::turso::value::TursoValue;

/// Marker type for the Turso diesel backend.
#[derive(Debug, Clone, Copy, Hash, PartialEq, Eq, Default)]
pub struct Turso;

/// Storage-class hint passed with each bind value. Matches Turso's STRICT
/// classes (INT, REAL, TEXT, BLOB) plus NULL. Subset of SQLite's variants so
/// existing SQL parsing/codegen assumptions hold.
#[derive(Debug, Clone, Copy, Hash, PartialEq, Eq)]
pub enum TursoType {
    /// No value. Not a storage class of its own in SQLite terms, but the
    /// bind path needs a metadata answer for a `NULL` like any other.
    Null,
    /// A 64-bit signed integer. Also where `Bool` lands, as 0 or 1.
    Integer,
    /// A 64-bit float.
    Real,
    /// UTF-8 text. Dates, times and timestamps are stored here as
    /// ISO-8601, which is what keeps them STRICT-compatible.
    Text,
    /// A byte string. UUIDs and UNION values both travel as one.
    Binary,
}

impl Backend for Turso {
    type QueryBuilder = TursoQueryBuilder;
    type RawValue<'a> = TursoValue<'a>;
    type BindCollector<'a> = TursoBindCollector<'a>;
}

impl TypeMetadata for Turso {
    type TypeMetadata = TursoType;
    type MetadataLookup = ();
}

impl SqlDialect for Turso {
    // Turso parses and fills in `RETURNING` on INSERT, UPDATE and DELETE, so
    // the clause is the DSL's answer to "which rowid did that insert get?" —
    // the alternative being a second `SELECT last_insert_rowid()`, which is
    // only correct because no other statement slips in between. `tests/
    // returning_clause.rs` pins the behaviour that would make it a trap: a
    // multi-row `RETURNING` whose stream is dropped after one row still
    // applies to every row.
    type ReturningClause = sql_dialect::returning_clause::PgLikeReturningClause;
    type OnConflictClause = TursoOnConflictClause;
    type InsertWithDefaultKeyword =
        sql_dialect::default_keyword_for_insert::DoesNotSupportDefaultKeyword;
    type BatchInsertSupport = sql_dialect::batch_insert_support::SqliteLikeBatchInsertSupport;
    type ConcatClause = sql_dialect::concat_clause::ConcatWithPipesClause;
    type DefaultValueClauseForInsert = sql_dialect::default_value_clause::AnsiDefaultValueClause;
    type EmptyFromClauseSyntax = sql_dialect::from_clause_syntax::AnsiSqlFromClauseSyntax;
    // The one place the dialect can't mirror SQLite. Diesel renders the
    // sources and `ON` condition of a join as a parenthesized group —
    // `FROM ("books" INNER JOIN "authors" ON (…))` — which SQLite accepts
    // and Turso's planner rejects outright ("Parenthesized FROM clause
    // subqueries are not supported", core/translate/planner.rs). Dropping
    // the parens is equivalent for joins chained onto one query source,
    // since SQL joins associate to the left and an `ON` binds to the
    // nearest join. It is *not* equivalent for a join on the right-hand
    // side of another join (`a.left_join(b.inner_join(c))`), which Turso
    // therefore cannot express at all: the parens are load-bearing there,
    // and the SQL we emit without them fails to parse rather than
    // quietly answering the wrong question.
    type JoinFromClauseSyntax =
        sql_dialect::join_from_clause_syntax::UnparenthesizedJoinFromClauseSyntax;
    type SelectStatementSyntax = sql_dialect::select_statement_syntax::AnsiSqlSelectStatement;
    type ExistsSyntax = sql_dialect::exists_syntax::AnsiSqlExistsSyntax;
    // Not the ANSI marker, which is the one place this dialect deliberately
    // diverges from SQLite's for a reason that is not about parsing. The
    // ANSI rendering emits one placeholder per list element, so an `eq_any`
    // over a runtime-length list mints a new SQL text per length — and this
    // backend's statement cache is keyed by the text. See
    // [`crate::turso::array_comparison`] for the whole argument, and for why
    // the TEXT form's `unhex` is load-bearing rather than redundant.
    type ArrayComparison = TursoJsonArrayComparison;
    type AliasSyntax = sql_dialect::alias_syntax::AsAliasSyntax;
    type WindowFrameClauseGroupSupport =
        sql_dialect::window_frame_clause_group_support::IsoGroupWindowFrameUnit;
    type WindowFrameExclusionSupport =
        sql_dialect::window_frame_exclusion_support::FrameExclusionSupport;
    type AggregateFunctionExpressions =
        sql_dialect::aggregate_function_expressions::PostgresLikeAggregateFunctionExpressions;
    type BuiltInWindowFunctionRequireOrder =
        sql_dialect::built_in_window_function_require_order::NoOrderRequired;
    // The second place the dialect can't mirror SQLite, and the one that
    // fails at run time rather than at parse time. See
    // [`LiteralSubselectLimit`].
    type SubselectLimitSyntax = LiteralSubselectLimit;
}

/// Marker for [`SqlDialect::ArrayComparison`]: bind an `IN (…)` list as a
/// single JSON array and unpack it with `json_each`, rather than emitting one
/// bind per element.
///
/// Selecting this is what routes `In`/`NotIn`/`Many` to the impls in
/// [`crate::turso::array_comparison`]. Because those impls are gated on this
/// marker, no other backend can reach them: the Postgres, MySQL and SQLite
/// renderings are untouched by construction, not by convention.
#[derive(Debug, Copy, Clone)]
pub struct TursoJsonArrayComparison;

impl DieselReserveSpecialization for Turso {}
impl TrustedBackend for Turso {}

macro_rules! has_sql_type {
    ($sql_ty:ty => $turso_ty:ident) => {
        impl HasSqlType<$sql_ty> for Turso {
            fn metadata(_: &mut ()) -> TursoType {
                TursoType::$turso_ty
            }
        }
    };
}

// STRICT-class mapping for every diesel-standard scalar SQL type.
// Date/Time/Timestamp are stored as TEXT (ISO-8601) to stay STRICT-compatible.
has_sql_type!(sql_types::SmallInt  => Integer);
has_sql_type!(sql_types::Integer   => Integer);
has_sql_type!(sql_types::BigInt    => Integer);
has_sql_type!(sql_types::Float     => Real);
has_sql_type!(sql_types::Double    => Real);
has_sql_type!(sql_types::Text      => Text);
has_sql_type!(sql_types::Binary    => Binary);
has_sql_type!(sql_types::Date        => Text);
has_sql_type!(sql_types::Time        => Text);
has_sql_type!(sql_types::Timestamp   => Text);
// Turso's own `Timestamptz` (see `super::types`), not the postgres one.
has_sql_type!(crate::turso::sql_types::Timestamptz => Text);
// Bool isn't in the Backend supertrait chain, but diesel users will reach
// for it constantly. Stored as INTEGER 0/1, matching Turso's built-in
// BOOLEAN type's physical shape.
has_sql_type!(sql_types::Bool      => Integer);

/// Turso writes the value of a `LIMIT` inside a subselect into the SQL text
/// instead of binding it. See [`SqlDialect::SubselectLimitSyntax`].
///
/// Turso's planner does not merely dislike a placeholder there — it deletes
/// it. A subquery compared as a scalar (`x = (SELECT …)`, and the row-value
/// form of the same) has its `LIMIT` expression *replaced* with the literal
/// `1` unless it already parses as a literal `0` or `1`
/// (`core/translate/subquery.rs`), because at most one row can be wanted.
/// A `LIMIT ?` does not parse as a number, so it is the expression that gets
/// replaced, and the placeholder it contained is never compiled into the
/// program. Turso's parameter table is built while emitting, so that slot is
/// never registered: the statement ends up knowing about one placeholder
/// fewer than the text spells, while diesel — which counts by walking the
/// AST — sends a value for every one.
///
/// What that costs depends only on where the dropped placeholder sat. If a
/// later one exists, the bind lands in a slot that is real, the values after
/// it are shifted by nothing (indices come from the parse, not the emit), and
/// the only casualty is the limit itself, which Turso replaced with 1 anyway
/// — so `.single_value()` looked like it worked. If it was the *last*
/// placeholder in the statement, there is no slot at all and the bind fails
/// with `bind index N is out of bounds`. Same defect, and whether it shows
/// depends on the order the query happened to render its filters in.
///
/// Writing the value into the text removes the placeholder that Turso was
/// going to discard, so the two counts agree again by construction, and the
/// limit means what it says even where Turso wouldn't have rewritten it —
/// inside an `IN (SELECT … LIMIT 5)`, say, which is not a scalar subquery.
///
/// This is deliberately not what a top-level `LIMIT` does. Binding is what
/// lets one paged query serve every page from a single compiled program;
/// making every limit a literal would mint a statement per page. A subselect
/// limit is not that: it is `.single_value()`'s `1` nearly every time.
#[derive(Debug, Clone, Copy)]
pub struct LiteralSubselectLimit;

/// Turso's `ON CONFLICT` support, which is SQLite's.
#[derive(Debug, Clone, Copy)]
pub struct TursoOnConflictClause;

impl sql_dialect::on_conflict_clause::SupportsOnConflictClause for TursoOnConflictClause {}
impl sql_dialect::on_conflict_clause::SupportsOnConflictClauseWhere for TursoOnConflictClause {}
impl sql_dialect::on_conflict_clause::PgLikeOnConflictClause for TursoOnConflictClause {}

/// Simple string-accumulating query builder. Emits positional `?` placeholders
/// (SQLite-style), matching what turso expects.
#[derive(Default, Debug)]
pub struct TursoQueryBuilder {
    sql: String,
}

impl QueryBuilder<Turso> for TursoQueryBuilder {
    fn push_sql(&mut self, sql: &str) {
        self.sql.push_str(sql);
    }

    fn push_identifier(&mut self, identifier: &str) -> crate::QueryResult<()> {
        self.sql.push('"');
        // Fast path for the overwhelming majority of identifiers, which
        // don't contain embedded quotes: one bulk memcpy beats a
        // char-at-a-time loop.
        if identifier.contains('"') {
            for c in identifier.chars() {
                if c == '"' {
                    self.sql.push_str("\"\"");
                } else {
                    self.sql.push(c);
                }
            }
        } else {
            self.sql.push_str(identifier);
        }
        self.sql.push('"');
        Ok(())
    }

    fn push_bind_param(&mut self) {
        self.sql.push('?');
    }

    fn finish(self) -> String {
        self.sql
    }
}
