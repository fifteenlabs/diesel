//! Turso-specific `QueryFragment` / `InsertValues` impls that parallel the
//! ones diesel hardcodes for SQLite (behind the `__sqlite-shared` feature).
//!
//! These specialized impls teach diesel how to emit DML for a backend that
//! doesn't support the SQL `DEFAULT` keyword and reproduces the SQLite-style
//! UPSERT adjustments. They're the missing half that
//! `DieselReserveSpecialization` alone can't cover, because diesel writes
//! them literally against `crate::sqlite::Sqlite` rather than a generic
//! "SQLite-like" trait.

use crate::backend::sql_dialect::default_keyword_for_insert::DoesNotSupportDefaultKeyword;
use crate::expression::{AppearsOnTable, Expression};
use crate::insertable::{ColumnInsertValue, DefaultableColumnInsertValue, InsertValues};
use crate::query_builder::from_clause::NoFromClause;
use crate::query_builder::limit_clause::{LimitClause, NoLimitClause};
use crate::query_builder::limit_offset_clause::{BoxedLimitOffsetClause, LimitOffsetClause};
use crate::query_builder::offset_clause::{NoOffsetClause, OffsetClause};
use crate::query_builder::select_statement::SelectStatement;
use crate::query_builder::select_statement::boxed::{BoxedQueryHelper, BoxedSelectStatement};
use crate::query_builder::upsert::into_conflict_clause::OnConflictSelectWrapper;
use crate::query_builder::where_clause::{BoxedWhereClause, WhereClause};
use crate::query_builder::insert_statement::{InsertOrIgnore, Replace};
use crate::query_builder::{AstPass, IntoBoxedClause, QueryFragment};
use crate::{Column, QueryResult};

use crate::turso::backend::Turso;

/// SQLite/Turso require a LIMIT when OFFSET is present but no LIMIT was
/// specified; `LIMIT -1` is the idiomatic "no upper bound" sentinel.
const LIMIT_NEG_ONE: &str = " LIMIT -1 ";

impl<Col, Expr> InsertValues<Turso, Col::Table>
    for DefaultableColumnInsertValue<ColumnInsertValue<Col, Expr>>
where
    Col: Column,
    Expr: Expression<SqlType = Col::SqlType> + AppearsOnTable<NoFromClause>,
    Self: QueryFragment<Turso>,
{
    fn column_names(&self, mut out: AstPass<'_, '_, Turso>) -> QueryResult<()> {
        if let Self::Expression(..) = *self {
            out.push_identifier(Col::NAME)?;
        }
        Ok(())
    }
}

impl<Col, Expr> QueryFragment<Turso, DoesNotSupportDefaultKeyword>
    for DefaultableColumnInsertValue<ColumnInsertValue<Col, Expr>>
where
    Expr: QueryFragment<Turso>,
{
    fn walk_ast<'b>(&'b self, mut out: AstPass<'_, 'b, Turso>) -> QueryResult<()> {
        if let Self::Expression(ref inner) = *self {
            inner.walk_ast(out.reborrow())?;
        }
        Ok(())
    }
}

// ----- INSERT OR REPLACE / INSERT OR IGNORE ---------------------------------
//
// diesel only implements these insert-operator markers for its Sqlite and
// Mysql backends (behind `#[cfg(feature = ...)]`), so `crate::replace_into`
// and `crate::insert_or_ignore_into` don't compile against a custom backend
// out of the box. Turso speaks SQLite SQL, so we emit the same keywords.
// Orphan-rule-legal because `Turso` (a local type) is a type parameter of the
// `QueryFragment` trait.

impl QueryFragment<Turso> for InsertOrIgnore {
    fn walk_ast<'b>(&'b self, mut out: AstPass<'_, 'b, Turso>) -> QueryResult<()> {
        out.push_sql("INSERT OR IGNORE");
        Ok(())
    }
}

impl QueryFragment<Turso> for Replace {
    fn walk_ast<'b>(&'b self, mut out: AstPass<'_, 'b, Turso>) -> QueryResult<()> {
        out.push_sql("REPLACE");
        Ok(())
    }
}

// ----- LIMIT / OFFSET -------------------------------------------------------
//
// SQLite (and therefore Turso) requires `LIMIT -1` when an OFFSET is present
// without a LIMIT, so these impls can't fall out of the generic paths.

impl QueryFragment<Turso> for LimitOffsetClause<NoLimitClause, NoOffsetClause> {
    fn walk_ast<'b>(&'b self, _out: AstPass<'_, 'b, Turso>) -> QueryResult<()> {
        Ok(())
    }
}

impl<L> QueryFragment<Turso> for LimitOffsetClause<LimitClause<L>, NoOffsetClause>
where
    LimitClause<L>: QueryFragment<Turso>,
{
    fn walk_ast<'b>(&'b self, out: AstPass<'_, 'b, Turso>) -> QueryResult<()> {
        self.limit_clause.walk_ast(out)
    }
}

impl<O> QueryFragment<Turso> for LimitOffsetClause<NoLimitClause, OffsetClause<O>>
where
    OffsetClause<O>: QueryFragment<Turso>,
{
    fn walk_ast<'b>(&'b self, mut out: AstPass<'_, 'b, Turso>) -> QueryResult<()> {
        out.push_sql(LIMIT_NEG_ONE);
        self.offset_clause.walk_ast(out)
    }
}

impl<L, O> QueryFragment<Turso> for LimitOffsetClause<LimitClause<L>, OffsetClause<O>>
where
    LimitClause<L>: QueryFragment<Turso>,
    OffsetClause<O>: QueryFragment<Turso>,
{
    fn walk_ast<'b>(&'b self, mut out: AstPass<'_, 'b, Turso>) -> QueryResult<()> {
        self.limit_clause.walk_ast(out.reborrow())?;
        self.offset_clause.walk_ast(out.reborrow())
    }
}

impl QueryFragment<Turso> for BoxedLimitOffsetClause<'_, Turso> {
    fn walk_ast<'b>(&'b self, mut out: AstPass<'_, 'b, Turso>) -> QueryResult<()> {
        match (self.limit.as_ref(), self.offset.as_ref()) {
            (Some(limit), Some(offset)) => {
                limit.walk_ast(out.reborrow())?;
                offset.walk_ast(out.reborrow())?;
            }
            (Some(limit), None) => limit.walk_ast(out.reborrow())?,
            (None, Some(offset)) => {
                out.push_sql(LIMIT_NEG_ONE);
                offset.walk_ast(out.reborrow())?;
            }
            (None, None) => {}
        }
        Ok(())
    }
}

impl<'a> IntoBoxedClause<'a, Turso> for LimitOffsetClause<NoLimitClause, NoOffsetClause> {
    type BoxedClause = BoxedLimitOffsetClause<'a, Turso>;
    fn into_boxed(self) -> Self::BoxedClause {
        BoxedLimitOffsetClause {
            limit: None,
            offset: None,
        }
    }
}

impl<'a, L> IntoBoxedClause<'a, Turso> for LimitOffsetClause<LimitClause<L>, NoOffsetClause>
where
    L: QueryFragment<Turso> + Send + 'a,
{
    type BoxedClause = BoxedLimitOffsetClause<'a, Turso>;
    fn into_boxed(self) -> Self::BoxedClause {
        BoxedLimitOffsetClause {
            limit: Some(Box::new(self.limit_clause)),
            offset: None,
        }
    }
}

impl<'a, O> IntoBoxedClause<'a, Turso> for LimitOffsetClause<NoLimitClause, OffsetClause<O>>
where
    O: QueryFragment<Turso> + Send + 'a,
{
    type BoxedClause = BoxedLimitOffsetClause<'a, Turso>;
    fn into_boxed(self) -> Self::BoxedClause {
        BoxedLimitOffsetClause {
            limit: None,
            offset: Some(Box::new(self.offset_clause)),
        }
    }
}

impl<'a, L, O> IntoBoxedClause<'a, Turso> for LimitOffsetClause<LimitClause<L>, OffsetClause<O>>
where
    L: QueryFragment<Turso> + Send + 'a,
    O: QueryFragment<Turso> + Send + 'a,
{
    type BoxedClause = BoxedLimitOffsetClause<'a, Turso>;
    fn into_boxed(self) -> Self::BoxedClause {
        BoxedLimitOffsetClause {
            limit: Some(Box::new(self.limit_clause)),
            offset: Some(Box::new(self.offset_clause)),
        }
    }
}

// ----- UPSERT: OnConflictSelectWrapper --------------------------------------
//
// SQLite's UPSERT grammar (https://www.sqlite.org/lang_UPSERT.html) has a
// parsing ambiguity around a bare `ON CONFLICT ... DO UPDATE` with no
// preceding WHERE. The boxed-select variant patches this by injecting
// `WHERE 1=1` when the inner where clause is None.
//
// The NoWhereClause case has no impl — same as SQLite — because the
// ambiguity can't be resolved there.

impl<F, S, D, W, O, LOf, G, H, LC> QueryFragment<Turso>
    for OnConflictSelectWrapper<SelectStatement<F, S, D, WhereClause<W>, O, LOf, G, H, LC>>
where
    SelectStatement<F, S, D, WhereClause<W>, O, LOf, G, H, LC>: QueryFragment<Turso>,
{
    fn walk_ast<'b>(&'b self, out: AstPass<'_, 'b, Turso>) -> QueryResult<()> {
        // Wrapper is a newtype around the inner select; field is pub(crate)
        // from diesel's side, so we can't touch it by name — but its
        // QueryFragment impl is what we actually want to run.
        QueryFragment::walk_ast(&self.0, out)
    }
}

impl<'a, ST, QS, GB> QueryFragment<Turso>
    for OnConflictSelectWrapper<BoxedSelectStatement<'a, ST, QS, Turso, GB>>
where
    BoxedSelectStatement<'a, ST, QS, Turso, GB>: QueryFragment<Turso>,
    QS: QueryFragment<Turso>,
{
    fn walk_ast<'b>(&'b self, pass: AstPass<'_, 'b, Turso>) -> QueryResult<()> {
        BoxedQueryHelper::build_query(&self.0, pass, |where_clause, mut pass| {
            match where_clause {
                BoxedWhereClause::None => pass.push_sql(" WHERE 1=1 "),
                w @ BoxedWhereClause::Where(_) => w.walk_ast(pass.reborrow())?,
            }
            Ok(())
        })
    }
}
