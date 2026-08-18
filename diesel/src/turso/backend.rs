//! `Turso` — the `crate::backend::Backend` marker.
//!
//! Turso targets SQLite SQL, so the `SqlDialect` mirrors diesel's SQLite
//! backend everywhere but the join `FROM` clause, where Turso's parser is
//! the stricter of the two (see `JoinFromClauseSyntax` below). The bind
//! collector and raw value types are ours (they wrap `turso::Value`
//! directly rather than going through a byte buffer).

use crate::backend::{
    Backend, DieselReserveSpecialization, SqlDialect, TrustedBackend, sql_dialect,
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
    type ArrayComparison = sql_dialect::array_comparison::AnsiSqlArrayComparison;
    type AliasSyntax = sql_dialect::alias_syntax::AsAliasSyntax;
    type WindowFrameClauseGroupSupport =
        sql_dialect::window_frame_clause_group_support::IsoGroupWindowFrameUnit;
    type WindowFrameExclusionSupport =
        sql_dialect::window_frame_exclusion_support::FrameExclusionSupport;
    type AggregateFunctionExpressions =
        sql_dialect::aggregate_function_expressions::PostgresLikeAggregateFunctionExpressions;
    type BuiltInWindowFunctionRequireOrder =
        sql_dialect::built_in_window_function_require_order::NoOrderRequired;
}

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
