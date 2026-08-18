//! The SQL expressions diesel's DSL cannot spell.
//!
//! Diesel models every expression as a type, which is why a typo in a column
//! name is a compile error — but the flip side is that an expression diesel
//! has no type for cannot be written at all, and the only way out used to be
//! `crate::sql_query` for the whole statement. That trade is a bad one: one
//! missing node (a `CASE`) costs the checking on every column, bind and table
//! name in the statement around it, and the statement stops being
//! statement-cacheable into the bargain.
//!
//! So the missing nodes live here instead. They are ordinary diesel
//! expressions: they compose with `filter`, `select`, `set` and each other,
//! their operand types are checked, their binds go through the normal bind
//! collector in the normal order, and they carry a real [`QueryId`] so a
//! statement containing one is still cached.
//!
//! What is here is what the app's queries actually needed — a `CASE`, a
//! two-argument `max`, and `coalesce`. Diesel upstream may grow its own
//! `CASE` one day, at which point this module shrinks.
//!
//! Every node here is implemented against [`Turso`] specifically rather than
//! generically over `DB: Backend`. Both the node and the backend being local
//! types is what makes these impls legal without touching diesel itself, and
//! `CASE`/`coalesce`/`max(a, b)` are SQLite-family spellings anyway.

use std::marker::PhantomData;

use crate::expression::{
    AppearsOnTable, Expression, MixedAggregates, SelectableExpression, ValidGrouping, is_aggregate,
};
use crate::query_builder::{AstPass, QueryFragment, QueryId};
use crate::sql_types::{BoolOrNullableBool, SingleValue};

use crate::turso::backend::Turso;

crate::define_sql_function! {
    /// SQLite's two-argument `max(a, b)` — the *scalar* one, not the
    /// aggregate `MAX(col)` diesel already has.
    ///
    /// Written as `max2` because `max` is taken by the aggregate; it renders
    /// as `max`, which is what SQLite dispatches on the argument count.
    /// Used for monotonic writes ("never let this column go backwards"),
    /// where the alternative is reading the row first and racing.
    #[sql_name = "max"]
    fn max2<T: SingleValue>(a: T, b: T) -> T;
}

crate::define_sql_function! {
    /// `coalesce(a, b)` — `a` unless it is NULL, in which case `b`.
    ///
    /// The two-argument form, which is the only one the app uses. Typed so
    /// that the fallback is non-nullable and the result therefore is too,
    /// which is the point: it is how a `Nullable` column reaches a place
    /// that needs a value.
    fn coalesce<T: SingleValue>(a: crate::sql_types::Nullable<T>, b: T) -> T;
}

/// Start a `CASE` expression: `CASE WHEN <pred> THEN <value> …`.
///
/// Chain further arms with [`CaseWhen::when`] and close it with
/// [`CaseWhen::otherwise`], which supplies the `ELSE`:
///
/// ```ignore
/// // CASE WHEN "state" IN (3, 4, 5) THEN 0 ELSE "state" END
/// case_when::<Integer, _, _>(state.eq_any([3, 4, 5]), 0).otherwise(state)
/// ```
///
/// The SQL type is named once, on `case_when`, and every arm and the `ELSE`
/// are checked against it. Naming it is not optional and not an accident of
/// the API: the arms are usually bare integer literals, and an SQL type
/// inferred separately per arm leaves the compiler solving a
/// `1`-could-be-anything puzzle once per arm — a five-arm ladder took rustc
/// 46 minutes before the type was pinned here instead.
///
/// There is no `ELSE`-less form. SQL would give one NULL, which every caller
/// here would then have to handle for a branch it does not believe can be
/// taken; requiring the `ELSE` keeps the result non-nullable.
pub fn case_when<ST, P, T>(pred: P, then: T) -> CaseWhen<NoArm, P, T::Expression, ST>
where
    T: crate::expression::AsExpression<ST>,
    ST: crate::sql_types::SqlType + SingleValue,
{
    CaseWhen {
        prev: NoArm,
        pred,
        then: then.as_expression(),
        sql_type: PhantomData,
    }
}

/// The empty arm list a [`case_when`] chain starts from. Renders nothing.
#[derive(Debug, Clone, Copy, QueryId)]
pub struct NoArm;

/// One `WHEN … THEN …` arm, holding the arms before it.
///
/// The arms are a linked list in the type rather than a `Vec` because each
/// arm carries its own expression types; `ST` rides along so every arm in
/// one chain is checked against the same SQL type.
#[derive(Debug, Clone, Copy)]
pub struct CaseWhen<Prev, P, T, ST> {
    prev: Prev,
    pred: P,
    then: T,
    sql_type: PhantomData<ST>,
}

/// A complete `CASE` expression, of the SQL type its chain was opened with.
#[derive(Debug, Clone, Copy)]
pub struct Case<Arms, E, ST> {
    arms: Arms,
    otherwise: E,
    sql_type: PhantomData<ST>,
}

impl<Prev, P, T, ST> CaseWhen<Prev, P, T, ST> {
    /// Add another `WHEN … THEN …`, evaluated after the ones already added.
    pub fn when<P2, T2>(self, pred: P2, then: T2) -> CaseWhen<Self, P2, T2::Expression, ST>
    where
        T2: crate::expression::AsExpression<ST>,
        ST: crate::sql_types::SqlType + SingleValue,
    {
        CaseWhen {
            prev: self,
            pred,
            then: then.as_expression(),
            sql_type: PhantomData,
        }
    }

    /// Close the `CASE` with its `ELSE`.
    pub fn otherwise<E>(self, otherwise: E) -> Case<Self, E::Expression, ST>
    where
        E: crate::expression::AsExpression<ST>,
        ST: crate::sql_types::SqlType + SingleValue,
    {
        Case {
            arms: self,
            otherwise: otherwise.as_expression(),
            sql_type: PhantomData,
        }
    }
}

/// An arm list whose every `WHEN` is a predicate.
///
/// The `THEN` values need no check here — they were converted through
/// `AsExpression<ST>` on the way in, so an arm of the wrong type never gets
/// built. `case_when::<Integer, _, _>(x.eq(1), "text")` does not compile;
/// SQLite would have accepted it and stored a column of mixed classes.
pub trait CaseArms {}

impl CaseArms for NoArm {}

impl<Prev, P, T, ST> CaseArms for CaseWhen<Prev, P, T, ST>
where
    Prev: CaseArms,
    P: Expression,
    P::SqlType: BoolOrNullableBool,
{
}

// ── rendering ────────────────────────────────────────────────────────────

impl QueryFragment<Turso> for NoArm {
    fn walk_ast<'b>(&'b self, _out: AstPass<'_, 'b, Turso>) -> crate::QueryResult<()> {
        Ok(())
    }
}

impl<Prev, P, T, ST> QueryFragment<Turso> for CaseWhen<Prev, P, T, ST>
where
    Prev: QueryFragment<Turso>,
    P: QueryFragment<Turso>,
    T: QueryFragment<Turso>,
{
    fn walk_ast<'b>(&'b self, mut out: AstPass<'_, 'b, Turso>) -> crate::QueryResult<()> {
        // Earlier arms first: `CASE` takes the first arm that matches, so
        // the order the caller chained them in is load-bearing.
        self.prev.walk_ast(out.reborrow())?;
        out.push_sql(" WHEN ");
        self.pred.walk_ast(out.reborrow())?;
        out.push_sql(" THEN ");
        self.then.walk_ast(out.reborrow())
    }
}

impl<Arms, E, ST> QueryFragment<Turso> for Case<Arms, E, ST>
where
    Arms: QueryFragment<Turso>,
    E: QueryFragment<Turso>,
{
    fn walk_ast<'b>(&'b self, mut out: AstPass<'_, 'b, Turso>) -> crate::QueryResult<()> {
        // Parenthesised as a whole, like every other diesel operator node:
        // a bare `CASE … END` next to an `AND` or a `+` would take its
        // precedence from wherever it landed.
        out.push_sql("(CASE");
        self.arms.walk_ast(out.reborrow())?;
        out.push_sql(" ELSE ");
        self.otherwise.walk_ast(out.reborrow())?;
        out.push_sql(" END)");
        Ok(())
    }
}

// ── typing ───────────────────────────────────────────────────────────────

impl<Arms, E, ST> Expression for Case<Arms, E, ST>
where
    Arms: CaseArms,
    E: Expression<SqlType = ST>,
    ST: crate::sql_types::SingleValue,
{
    type SqlType = ST;
}

// `QS` is the query source the expression is being used against — the table
// in a `filter`/`set`, the join in a select. Propagating rather than
// blanket-implementing is what keeps a `CASE` from smuggling a column of
// some *other* table into a statement that never mentions it.
//
// The arms need their own pair of traits rather than reusing diesel's:
// `AppearsOnTable` and `SelectableExpression` both require `Self:
// Expression`, and an arm list is not an expression — it is a fragment of
// one, with no SQL type of its own until the `ELSE` gives the `CASE` one.

/// Arm lists that only mention columns reachable from `QS`.
pub trait ArmsAppearOn<QS> {}

impl<QS> ArmsAppearOn<QS> for NoArm {}

impl<QS, Prev, P, T, ST> ArmsAppearOn<QS> for CaseWhen<Prev, P, T, ST>
where
    Prev: ArmsAppearOn<QS>,
    P: AppearsOnTable<QS>,
    T: AppearsOnTable<QS>,
{
}

/// Arm lists valid in a `SELECT` against `QS` — the stricter of the two,
/// because it is what makes a left-joined column nullable.
pub trait ArmsSelectableFrom<QS> {}

impl<QS> ArmsSelectableFrom<QS> for NoArm {}

impl<QS, Prev, P, T, ST> ArmsSelectableFrom<QS> for CaseWhen<Prev, P, T, ST>
where
    Prev: ArmsSelectableFrom<QS>,
    P: SelectableExpression<QS>,
    T: SelectableExpression<QS>,
{
}

impl<QS, Arms, E, ST> AppearsOnTable<QS> for Case<Arms, E, ST>
where
    Arms: ArmsAppearOn<QS>,
    E: AppearsOnTable<QS>,
    Self: Expression,
{
}

impl<QS, Arms, E, ST> SelectableExpression<QS> for Case<Arms, E, ST>
where
    Arms: ArmsSelectableFrom<QS>,
    E: SelectableExpression<QS>,
    Self: AppearsOnTable<QS>,
{
}

// Aggregate-ness is not a property a `CASE` has of its own: it is whatever
// its parts are, combined the way diesel combines a tuple's. Computing it
// rather than demanding "not aggregate" is what lets `SUM(CASE …)` work
// (the parts are plain columns and literals, so the `CASE` comes out
// non-aggregate and the `SUM` around it makes the aggregate) while still
// refusing a `CASE` that mixes an aggregate arm with a bare column.
impl<GB> ValidGrouping<GB> for NoArm {
    type IsAggregate = is_aggregate::Never;
}

impl<GB, Prev, P, T, ST> ValidGrouping<GB> for CaseWhen<Prev, P, T, ST>
where
    Prev: ValidGrouping<GB>,
    P: ValidGrouping<GB>,
    T: ValidGrouping<GB>,
    Prev::IsAggregate: MixedAggregates<P::IsAggregate>,
    ArmMix<Prev::IsAggregate, P::IsAggregate>: MixedAggregates<T::IsAggregate>,
{
    type IsAggregate = ArmMix<ArmMix<Prev::IsAggregate, P::IsAggregate>, T::IsAggregate>;
}

impl<GB, Arms, E, ST> ValidGrouping<GB> for Case<Arms, E, ST>
where
    Arms: ValidGrouping<GB>,
    E: ValidGrouping<GB>,
    Arms::IsAggregate: MixedAggregates<E::IsAggregate>,
{
    type IsAggregate = ArmMix<Arms::IsAggregate, E::IsAggregate>;
}

/// The combined aggregate-ness of two parts of a `CASE`.
type ArmMix<A, B> = <A as MixedAggregates<B>>::Output;

// `QueryId` is what decides whether the *enclosing* statement can be
// prepared once and reused, so it is propagated rather than opted out of: a
// `CASE` built from columns and binds has a stable shape, and the claim
// query that carries one runs on a timer. Hand-written because the derive
// would demand `ST: QueryId`, and an SQL type is not a query.
impl<Prev, P, T, ST> QueryId for CaseWhen<Prev, P, T, ST>
where
    Prev: QueryId,
    P: QueryId,
    T: QueryId,
    ST: 'static,
{
    type QueryId = CaseWhen<Prev::QueryId, P::QueryId, T::QueryId, ST>;
    const HAS_STATIC_QUERY_ID: bool =
        Prev::HAS_STATIC_QUERY_ID && P::HAS_STATIC_QUERY_ID && T::HAS_STATIC_QUERY_ID;
}

impl<Arms, E, ST> QueryId for Case<Arms, E, ST>
where
    Arms: QueryId,
    E: QueryId,
    ST: 'static,
{
    type QueryId = Case<Arms::QueryId, E::QueryId, ST>;
    const HAS_STATIC_QUERY_ID: bool = Arms::HAS_STATIC_QUERY_ID && E::HAS_STATIC_QUERY_ID;
}

// `AsExpression` — which is what lets a `CASE` be the right-hand side of a
// `column.eq(…)` in an UPDATE's `SET` — comes from diesel's blanket impl for
// anything that is already an `Expression`.
